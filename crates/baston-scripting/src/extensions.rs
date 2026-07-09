//! The six BASTON deno_core extensions (console, events, exports, runtime,
//! players, deferrals).
//!
//! Design note: JS callbacks never cross the op boundary. `bootstrap.js` keeps
//! the callback registry (event name → functions, export name → function) on
//! the JS side; these ops only do Rust-side bookkeeping, logging, and deferral
//! resolution. Event dispatch from Rust into JS goes through
//! `globalThis.__baston.dispatch(...)` via `execute_script`, which keeps us
//! independent from `v8::Global<v8::Function>` juggling.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;
use deno_core::{op2, OpState};

use crate::deferrals::DeferralRegistry;
use crate::observability::Observability;

/// Per-runtime context stored in the isolate's `OpState`.
pub struct RuntimeContext {
    pub resource_name: String,
    /// Millisecond epoch for `GetGameTimer()` — the script host start instant.
    pub host_started_at: Instant,
    /// Events queued by `TriggerEvent` during JS execution; drained by the
    /// host after each dispatch and re-broadcast to every runtime.
    pub queued_events: VecDeque<(String, String)>,
    /// Event names with at least one registered handler (bookkeeping).
    pub handled_events: HashSet<String>,
    /// Export names registered by this resource (bookkeeping).
    pub exports: HashSet<String>,
    /// Server commands registered by this resource via `RegisterCommand`.
    pub commands: HashMap<String, bool>,
    /// Whether this resource registered a `RegisterZoneTransferState` callback.
    pub has_zone_transfer_state: bool,
    /// JSON collected by `op_report_zone_transfer_state` during the last
    /// `collectZoneTransferState` dispatch (jalon D4 handoff).
    pub collected_transfer_state: Option<String>,
    /// Handler exceptions caught by bootstrap.js during the current dispatch.
    pub handler_errors: u64,
}

/// Shared deferral registry handle stored in `OpState` (one per process,
/// cloned into every runtime).
pub struct SharedDeferrals(pub Arc<DeferralRegistry>);

/// Shared player directory handle stored in `OpState` (owned by the gateway,
/// read by player natives).
pub struct SharedPlayers(pub Arc<baston_protocol::PlayerDirectory>);

/// Net bridge handle stored in `OpState` (client events + native dispatch).
pub struct SharedNet(pub crate::net_bridge::NetBridge);

/// Shared runtime observability collector stored in `OpState`.
pub struct SharedObservability(pub Arc<Observability>);

/// Shared console variables (`GetConvar*` / `SetConvar*`).
pub struct SharedConvars(pub Arc<DashMap<String, String>>);

/// Shared resource snapshot (`GetResourceState`, `LoadResourceFile`, ...).
pub struct SharedResources(pub crate::resource_registry::ResourceRegistry);

/// How long a server → client native call may wait for its result.
///
/// This is also the upper bound on how long one such call can stall the
/// resource's runtime: the host command loop runs one dispatch to completion at
/// a time, so while a handler awaits a native result no other event for that
/// resource is serviced (see `op_invoke_native_on_client`). Keep it low enough
/// to bound a slow/hostile client's impact, but above realistic client RTT so
/// legitimate results aren't dropped. Removing the stall entirely needs the
/// host loop to drive dispatches concurrently on the shared isolate (tracked
/// separately — it's a redesign of the execution model, not a local change).
const NATIVE_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1000);

// --- 1. console ---

#[op2(fast)]
fn op_console_log(state: &mut OpState, #[string] msg: String) {
    let resource = &state.borrow::<RuntimeContext>().resource_name;
    tracing::info!(target: "script", resource, "{msg}");
    println!("{msg}");
}

deno_core::extension!(baston_console, ops = [op_console_log]);

// --- 2. events ---

#[op2(fast)]
fn op_add_event_handler(state: &mut OpState, #[string] event: String, _cb_id: u32) {
    let ctx = state.borrow_mut::<RuntimeContext>();
    tracing::debug!(target: "events", resource = %ctx.resource_name, %event, "handler registered");
    ctx.handled_events.insert(event);
}

#[op2(fast)]
fn op_register_command(state: &mut OpState, #[string] name: String, restricted: bool, _cb_id: u32) {
    let ctx = state.borrow_mut::<RuntimeContext>();
    tracing::debug!(
        target: "commands",
        resource = %ctx.resource_name,
        %name,
        restricted,
        "command registered"
    );
    ctx.commands.insert(name, restricted);
}

#[op2(fast)]
fn op_trigger_event(state: &mut OpState, #[string] event: String, #[string] args_json: String) {
    state
        .borrow_mut::<RuntimeContext>()
        .queued_events
        .push_back((event, args_json));
}

/// `TriggerClientEvent(name, source, ...args)` — routed to the game
/// transport as a `msgNetEvent` packet.
#[op2(fast)]
fn op_trigger_client_event(
    state: &mut OpState,
    #[string] event: String,
    source: u32,
    #[string] args_json: String,
) {
    let net = &state.borrow::<SharedNet>().0;
    if net
        .tx
        .try_send(crate::net_bridge::NetOutbound::ClientEvent {
            source,
            event: event.clone(),
            args_json,
        })
        .is_err()
    {
        tracing::warn!(target: "events", %event, source, "client event dropped: net bridge full or closed");
    }
}

#[op2(fast)]
fn op_report_handler_error(state: &mut OpState) {
    state.borrow_mut::<RuntimeContext>().handler_errors += 1;
}

/// Dispatch a GTA native to `source`'s client via the BASTON shim and await
/// the result. Returns a JSON string; errors are `{"__error": "..."}` so the
/// polyfill can throw without deno_core error plumbing.
#[op2]
#[string]
async fn op_invoke_native_on_client(
    state: std::rc::Rc<std::cell::RefCell<OpState>>,
    source: u32,
    #[string] hash_hex: String,
    #[string] args_json: String,
    expects_return: bool,
) -> String {
    use baston_protocol::native::{NativeCall, INVOKE_NATIVE_EVENT};

    fn err(message: impl std::fmt::Display) -> String {
        serde_json::json!({ "__error": message.to_string() }).to_string()
    }

    let hash = match u64::from_str_radix(hash_hex.trim_start_matches("0x"), 16) {
        Ok(h) => h,
        Err(e) => return err(format!("invalid native hash {hash_hex}: {e}")),
    };
    let args: Vec<serde_json::Value> = match serde_json::from_str(&args_json) {
        Ok(a) => a,
        Err(e) => return err(format!("invalid native args: {e}")),
    };

    static REGISTRY: std::sync::OnceLock<baston_protocol::native::NativeRegistry> =
        std::sync::OnceLock::new();
    if let Err(e) = REGISTRY
        .get_or_init(baston_protocol::native::NativeRegistry::new)
        .validate(hash, args.len())
    {
        return err(e);
    }

    let (net, observability, resource) = {
        let state_ref = state.borrow();
        (
            state_ref.borrow::<SharedNet>().0.clone(),
            state_ref.borrow::<SharedObservability>().0.clone(),
            state_ref.borrow::<RuntimeContext>().resource_name.clone(),
        )
    };
    let started = Instant::now();
    let (id, rx) = net.pending_natives.register();
    let call = NativeCall {
        id,
        hash: format!("0x{hash:016X}"),
        args,
    };
    let payload = serde_json::json!([call]);
    if net
        .tx
        .try_send(crate::net_bridge::NetOutbound::ClientEvent {
            source,
            event: INVOKE_NATIVE_EVENT.to_owned(),
            args_json: payload.to_string(),
        })
        .is_err()
    {
        net.pending_natives.cancel(id);
        observability.record_native_roundtrip(
            &resource,
            hash,
            source,
            started.elapsed().as_micros() as u64,
            false,
            true,
        );
        return err("net bridge full or closed");
    }

    if !expects_return {
        // Fire-and-forget: the shim still replies, but nobody waits.
        net.pending_natives.cancel(id);
        observability.record_native_roundtrip(
            &resource,
            hash,
            source,
            started.elapsed().as_micros() as u64,
            false,
            false,
        );
        return "null".to_owned();
    }

    match tokio::time::timeout(NATIVE_CALL_TIMEOUT, rx).await {
        Ok(Ok(value)) => {
            observability.record_native_roundtrip(
                &resource,
                hash,
                source,
                started.elapsed().as_micros() as u64,
                false,
                false,
            );
            value.to_string()
        }
        Ok(Err(_)) => {
            net.pending_natives.cancel(id);
            observability.record_native_roundtrip(
                &resource,
                hash,
                source,
                started.elapsed().as_micros() as u64,
                false,
                true,
            );
            err("native result channel closed")
        }
        Err(_) => {
            net.pending_natives.cancel(id);
            observability.record_native_roundtrip(
                &resource,
                hash,
                source,
                started.elapsed().as_micros() as u64,
                true,
                true,
            );
            err(format!("native call 0x{hash:016X} timed out"))
        }
    }
}

deno_core::extension!(
    baston_events,
    ops = [
        op_add_event_handler,
        op_register_command,
        op_trigger_event,
        op_trigger_client_event,
        op_invoke_native_on_client,
        op_report_handler_error,
    ]
);

// --- 3. exports ---

#[op2(fast)]
fn op_add_export(state: &mut OpState, #[string] name: String, _fn_id: u32) {
    let ctx = state.borrow_mut::<RuntimeContext>();
    tracing::debug!(target: "exports", resource = %ctx.resource_name, %name, "export registered");
    ctx.exports.insert(name);
}

#[op2(fast)]
fn op_get_export(state: &mut OpState, #[string] resource: String, #[string] name: String) -> u32 {
    let caller = &state.borrow::<RuntimeContext>().resource_name;
    tracing::warn!(
        target: "exports",
        %caller, %resource, %name,
        "cross-resource export lookup is not supported in Phase A"
    );
    0
}

deno_core::extension!(baston_exports, ops = [op_add_export, op_get_export]);

// --- 4. runtime ---

#[op2(fast)]
fn op_get_game_timer(state: &mut OpState) -> u32 {
    let ctx = state.borrow::<RuntimeContext>();
    ctx.host_started_at.elapsed().as_millis() as u32
}

#[op2]
#[string]
fn op_get_current_resource_name(state: &mut OpState) -> String {
    state.borrow::<RuntimeContext>().resource_name.clone()
}

deno_core::extension!(
    baston_runtime,
    ops = [
        op_get_game_timer,
        op_get_current_resource_name,
        op_get_convar,
        op_get_convar_int,
        op_get_convar_float,
        op_get_convar_bool,
        op_set_convar,
        op_get_num_resources,
        op_get_resource_by_find_index,
        op_get_resource_state,
        op_get_resource_path,
        op_get_num_resource_metadata,
        op_get_resource_metadata,
        op_load_resource_file,
        op_save_resource_file,
    ]
);

// --- 5. players (backed by the shared PlayerDirectory since B1) ---

#[op2]
#[string]
fn op_get_convar(
    state: &mut OpState,
    #[string] name: String,
    #[string] default_value: String,
) -> String {
    state
        .borrow::<SharedConvars>()
        .0
        .get(&name)
        .map(|value| value.value().clone())
        .unwrap_or(default_value)
}

#[op2(fast)]
fn op_get_convar_int(state: &mut OpState, #[string] name: String, default_value: i32) -> i32 {
    state
        .borrow::<SharedConvars>()
        .0
        .get(&name)
        .and_then(|value| value.parse::<i32>().ok())
        .unwrap_or(default_value)
}

#[op2(fast)]
fn op_get_convar_float(state: &mut OpState, #[string] name: String, default_value: f64) -> f64 {
    state
        .borrow::<SharedConvars>()
        .0
        .get(&name)
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(default_value)
}

#[op2(fast)]
fn op_get_convar_bool(state: &mut OpState, #[string] name: String, default_value: bool) -> bool {
    state
        .borrow::<SharedConvars>()
        .0
        .get(&name)
        .map(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "true" | "1" | "yes" | "on"
            )
        })
        .unwrap_or(default_value)
}

#[op2(fast)]
fn op_set_convar(state: &mut OpState, #[string] name: String, #[string] value: String) {
    state.borrow::<SharedConvars>().0.insert(name, value);
}

#[op2(fast)]
fn op_get_num_resources(state: &mut OpState) -> u32 {
    state.borrow::<SharedResources>().0.count() as u32
}

#[op2]
#[string]
fn op_get_resource_by_find_index(state: &mut OpState, index: u32) -> String {
    state.borrow::<SharedResources>().0.name_at(index as usize)
}

#[op2]
#[string]
fn op_get_resource_state(state: &mut OpState, #[string] name: String) -> String {
    state.borrow::<SharedResources>().0.state(&name).to_owned()
}

#[op2]
#[string]
fn op_get_resource_path(state: &mut OpState, #[string] name: String) -> String {
    state.borrow::<SharedResources>().0.path(&name)
}

#[op2(fast)]
fn op_get_num_resource_metadata(
    state: &mut OpState,
    #[string] resource: String,
    #[string] key: String,
) -> u32 {
    state
        .borrow::<SharedResources>()
        .0
        .metadata_count(&resource, &key)
}

#[op2]
#[string]
fn op_get_resource_metadata(
    state: &mut OpState,
    #[string] resource: String,
    #[string] key: String,
    index: u32,
) -> String {
    state
        .borrow::<SharedResources>()
        .0
        .metadata_value(&resource, &key, index as usize)
}

#[op2]
#[string]
fn op_load_resource_file(
    state: &mut OpState,
    #[string] resource: String,
    #[string] file_name: String,
) -> String {
    state
        .borrow::<SharedResources>()
        .0
        .load_file(&resource, &file_name)
        .unwrap_or_default()
}

#[op2(fast)]
fn op_save_resource_file(
    state: &mut OpState,
    #[string] resource: String,
    #[string] file_name: String,
    #[string] data: String,
    data_len: i32,
) -> bool {
    state
        .borrow::<SharedResources>()
        .0
        .save_file(&resource, &file_name, &data, data_len)
}

#[op2(fast)]
fn op_get_num_player_indices(state: &mut OpState) -> u32 {
    state.borrow::<SharedPlayers>().0.count() as u32
}

#[op2(fast)]
fn op_get_player_from_index(state: &mut OpState, index: u32) -> u32 {
    let sources = state.borrow::<SharedPlayers>().0.sources();
    sources.get(index as usize).copied().unwrap_or(0)
}

#[op2]
#[string]
fn op_get_player_name(state: &mut OpState, source: u32) -> String {
    state
        .borrow::<SharedPlayers>()
        .0
        .get(source)
        .map(|p| p.name)
        .unwrap_or_default()
}

#[op2(fast)]
fn op_does_player_exist(state: &mut OpState, source: u32) -> bool {
    state.borrow::<SharedPlayers>().0.exists(source)
}

#[op2(fast)]
fn op_get_num_player_identifiers(state: &mut OpState, source: u32) -> u32 {
    state.borrow::<SharedPlayers>().0.identifier_count(source) as u32
}

#[op2]
#[string]
fn op_get_player_identifier(state: &mut OpState, source: u32, index: u32) -> String {
    state
        .borrow::<SharedPlayers>()
        .0
        .identifier_at(source, index as usize)
        .unwrap_or_default()
}

#[op2]
#[string]
fn op_get_player_endpoint(state: &mut OpState, source: u32) -> String {
    state
        .borrow::<SharedPlayers>()
        .0
        .endpoint(source)
        .unwrap_or_default()
}

#[op2]
#[string]
fn op_get_player_guid(state: &mut OpState, source: u32) -> String {
    state
        .borrow::<SharedPlayers>()
        .0
        .guid(source)
        .unwrap_or_default()
}

#[op2(fast)]
fn op_get_player_ping(_state: &mut OpState, _source: u32) -> u32 {
    0
}

#[op2(fast)]
fn op_get_num_player_tokens(_state: &mut OpState, _source: u32) -> u32 {
    0
}

#[op2]
#[string]
fn op_get_player_token(_state: &mut OpState, _source: u32, _index: u32) -> String {
    String::new()
}

/// FXServer `GetPlayerIdentifierByType`: returns the full `type:value`
/// identifier, or an empty string when absent.
#[op2]
#[string]
fn op_get_player_identifier_by_type(
    state: &mut OpState,
    source: u32,
    #[string] id_type: String,
) -> String {
    state
        .borrow::<SharedPlayers>()
        .0
        .identifier_by_type(source, &id_type)
        .unwrap_or_default()
}

deno_core::extension!(
    baston_players,
    ops = [
        op_get_num_player_indices,
        op_get_player_from_index,
        op_get_player_name,
        op_get_player_identifier_by_type,
        op_does_player_exist,
        op_get_num_player_identifiers,
        op_get_player_identifier,
        op_get_player_endpoint,
        op_get_player_guid,
        op_get_player_ping,
        op_get_num_player_tokens,
        op_get_player_token,
    ]
);

// --- 6. deferrals ---

#[op2(fast)]
fn op_deferral_defer(state: &mut OpState, source: u32) {
    state.borrow::<SharedDeferrals>().0.defer(source);
}

#[op2(fast)]
fn op_deferral_update(state: &mut OpState, source: u32, #[string] message: String) {
    state.borrow::<SharedDeferrals>().0.update(source, message);
}

#[op2(fast)]
fn op_deferral_done(state: &mut OpState, source: u32, #[string] reason: String) {
    let reason = if reason.is_empty() {
        None
    } else {
        Some(reason)
    };
    state.borrow::<SharedDeferrals>().0.done(source, reason);
}

#[op2(fast)]
fn op_deferral_present_card(state: &mut OpState, source: u32, #[string] card_json: String) {
    state
        .borrow::<SharedDeferrals>()
        .0
        .present_card(source, card_json);
}

#[op2(fast)]
fn op_set_kick_reason(state: &mut OpState, source: u32, #[string] reason: String) {
    state
        .borrow::<SharedDeferrals>()
        .0
        .set_kick_reason(source, reason);
}

// --- 7. zone mesh (Phase D handoffs) ---

/// `RegisterZoneTransferState(cb)` — bookkeeping; the callback stays JS-side.
#[op2(fast)]
fn op_register_zone_transfer_state(state: &mut OpState) {
    let ctx = state.borrow_mut::<RuntimeContext>();
    tracing::debug!(target: "mesh", resource = %ctx.resource_name, "zone transfer state registered");
    ctx.has_zone_transfer_state = true;
}

/// Called by `__baston.collectZoneTransferState(source)` to hand the merged
/// JSON object back to Rust.
#[op2(fast)]
fn op_report_zone_transfer_state(state: &mut OpState, #[string] json: String) {
    state
        .borrow_mut::<RuntimeContext>()
        .collected_transfer_state = Some(json);
}

deno_core::extension!(
    baston_mesh,
    ops = [
        op_register_zone_transfer_state,
        op_report_zone_transfer_state
    ]
);

deno_core::extension!(
    baston_deferrals,
    ops = [
        op_deferral_defer,
        op_deferral_update,
        op_deferral_done,
        op_deferral_present_card,
        op_set_kick_reason,
    ]
);

/// All BASTON extensions, in registration order.
pub fn all_extensions() -> Vec<deno_core::Extension> {
    vec![
        baston_console::init(),
        baston_events::init(),
        baston_exports::init(),
        baston_runtime::init(),
        baston_players::init(),
        baston_deferrals::init(),
        baston_mesh::init(),
    ]
}
