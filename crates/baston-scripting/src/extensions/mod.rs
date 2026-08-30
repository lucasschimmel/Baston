//! The six BASTON deno_core extensions (console, events, exports, runtime,
//! players, deferrals).
//!
//! Design note: JS callbacks never cross the op boundary. `bootstrap.js` keeps
//! the callback registry (event name → functions, export name → function) on
//! the JS side; these ops only do Rust-side bookkeeping, logging, and deferral
//! resolution. Event dispatch from Rust into JS goes through
//! `globalThis.__baston.dispatch(...)` via `execute_script`, which keeps us
//! independent from `v8::Global<v8::Function>` juggling.
//!
//! Module layout: the heavyweight `Citizen.invokeNative` groups, the CFX
//! context-routing table, and the console buffer live in `crate::natives`.
//! Everything else — the smaller op groups — lives here.
//!
//! The natives themselves live in [`crate::natives`] and know nothing about
//! V8: this module is the V8 half of the bridge, converting between JS values
//! and the JSON the neutral natives speak (ADR-002, Tier 2). The ops below are
//! deliberately thin — logic that grows here is logic Lua will not get.


use std::sync::Arc;

use deno_core::{op2, OpState};

use crate::natives::console;
use crate::natives::{client, server};

// The context and service types moved to `crate::native_state` when the
// natives stopped depending on V8. Re-exported so this module — and the
// crate's public surface — keep their existing paths.
pub use crate::native_state::{
    RuntimeContext, SharedConvars, SharedDeferrals,
    SharedHttpHandlers, SharedNet, SharedObservability, SharedPlayers, SharedResources, SharedStateBags, SharedVoice, VoiceControl,
};

/// The engine-neutral [`NativeState`](crate::native_state::NativeState) for
/// this isolate, parked in `OpState`.
///
/// One indirection rather than storing each service twice: `OpState` holds the
/// V8-side plumbing, this holds everything the natives read.
pub struct Natives(pub crate::native_state::NativeState);

// --- 0. native dispatch (thin wrappers over `crate::natives`) ---

#[op2]
#[string]
fn op_cfx_shared_native(
    state: &mut OpState,
    #[string] name: String,
    #[string] args_json: String,
) -> String {
    server::cfx_shared_native(&mut state.borrow_mut::<Natives>().0, name, args_json)
}

#[op2]
#[string]
fn op_cfx_server_native(
    state: &mut OpState,
    #[string] name: String,
    #[string] result_kind: String,
    #[string] args_json: String,
) -> String {
    server::cfx_server_native(
        &mut state.borrow_mut::<Natives>().0,
        name,
        result_kind,
        args_json,
    )
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
    // Cloned out before the await: the neutral dispatcher must not hold a
    // borrow of the isolate's state across a client round trip.
    let (net, observability, resource) = {
        let op_state = state.borrow();
        let natives = &op_state.borrow::<Natives>().0;
        (
            natives.borrow::<SharedNet>().0.clone(),
            Arc::clone(&natives.borrow::<SharedObservability>().0),
            natives.borrow::<RuntimeContext>().resource_name.clone(),
        )
    };
    client::invoke_native_on_client(
        net,
        observability,
        resource,
        source,
        hash_hex,
        args_json,
        expects_return,
    )
    .await
}

// --- 1. console ---

#[op2(fast)]
fn op_console_log(state: &mut OpState, #[string] msg: String) {
    let natives = &state.borrow::<Natives>().0;
    let resource = natives.borrow::<RuntimeContext>().resource_name.clone();
    console::log(&resource, &msg);
}

deno_core::extension!(baston_console, ops = [op_console_log]);

// --- 2. events ---

#[op2(fast)]
fn op_add_event_handler(state: &mut OpState, #[string] event: String, _cb_id: u32) {
    let ctx = state.borrow_mut::<Natives>().0.borrow_mut::<RuntimeContext>();
    tracing::debug!(target: "events", resource = %ctx.resource_name, %event, "handler registered");
    ctx.handled_events.insert(event);
}

#[op2(fast)]
fn op_register_command(state: &mut OpState, #[string] name: String, restricted: bool, _cb_id: u32) {
    let ctx = state.borrow_mut::<Natives>().0.borrow_mut::<RuntimeContext>();
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
    state.borrow_mut::<Natives>().0
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
    let net = &state.borrow::<Natives>().0.borrow::<SharedNet>().0;
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
    state.borrow_mut::<Natives>().0.borrow_mut::<RuntimeContext>().handler_errors += 1;
}

#[op2(fast)]
fn op_add_state_bag_change_handler(
    state: &mut OpState,
    #[string] key_filter: String,
    #[string] bag_filter: String,
    callback_id: u32,
) -> u32 {
    let resource = state.borrow::<Natives>().0.borrow::<RuntimeContext>().resource_name.clone();
    state.borrow::<Natives>().0.borrow::<SharedStateBags>().0.add_handler(
        resource,
        Some(key_filter),
        Some(bag_filter),
        callback_id,
    )
}

#[op2(fast)]
fn op_remove_state_bag_change_handler(state: &mut OpState, cookie: u32) -> bool {
    let resource = state.borrow::<Natives>().0.borrow::<RuntimeContext>().resource_name.clone();
    state.borrow::<Natives>().0
        .borrow::<SharedStateBags>()
        .0
        .remove_handler(&resource, cookie)
}

#[op2]
#[string]
fn op_poll_state_bag_changes(state: &mut OpState) -> String {
    const MAX_DELIVERIES_PER_POLL: usize = 4096;
    let resource = state.borrow::<Natives>().0.borrow::<RuntimeContext>().resource_name.clone();
    serde_json::to_string(
        &state.borrow::<Natives>().0
            .borrow::<SharedStateBags>()
            .0
            .drain_deliveries(&resource, MAX_DELIVERIES_PER_POLL),
    )
    .unwrap_or_else(|_| "[]".to_owned())
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
        op_add_state_bag_change_handler,
        op_remove_state_bag_change_handler,
        op_poll_state_bag_changes,
    ]
);

// --- 3. exports ---

#[op2(fast)]
fn op_add_export(state: &mut OpState, #[string] name: String, _fn_id: u32) {
    let ctx = state.borrow_mut::<Natives>().0.borrow_mut::<RuntimeContext>();
    tracing::debug!(target: "exports", resource = %ctx.resource_name, %name, "export registered");
    ctx.exports.insert(name);
}

#[op2(fast)]
fn op_get_export(state: &mut OpState, #[string] resource: String, #[string] name: String) -> u32 {
    let caller = &state.borrow::<Natives>().0.borrow::<RuntimeContext>().resource_name;
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
    let ctx = state.borrow::<Natives>().0.borrow::<RuntimeContext>();
    ctx.host_started_at.elapsed().as_millis() as u32
}

#[op2]
#[string]
fn op_get_current_resource_name(state: &mut OpState) -> String {
    state.borrow::<Natives>().0.borrow::<RuntimeContext>().resource_name.clone()
}

deno_core::extension!(
    baston_runtime,
    ops = [
        op_get_game_timer,
        op_get_current_resource_name,
        op_cfx_shared_native,
        op_cfx_server_native,
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

#[op2]
#[string]
fn op_get_convar(
    state: &mut OpState,
    #[string] name: String,
    #[string] default_value: String,
) -> String {
    state.borrow::<Natives>().0
        .borrow::<SharedConvars>()
        .0
        .get(&name)
        .map(|value| value.value().clone())
        .unwrap_or(default_value)
}

#[op2(fast)]
fn op_get_convar_int(state: &mut OpState, #[string] name: String, default_value: i32) -> i32 {
    state.borrow::<Natives>().0
        .borrow::<SharedConvars>()
        .0
        .get(&name)
        .and_then(|value| value.parse::<i32>().ok())
        .unwrap_or(default_value)
}

#[op2(fast)]
fn op_get_convar_float(state: &mut OpState, #[string] name: String, default_value: f64) -> f64 {
    state.borrow::<Natives>().0
        .borrow::<SharedConvars>()
        .0
        .get(&name)
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(default_value)
}

#[op2(fast)]
fn op_get_convar_bool(state: &mut OpState, #[string] name: String, default_value: bool) -> bool {
    state.borrow::<Natives>().0
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
    state.borrow::<Natives>().0.borrow::<SharedConvars>().0.insert(name, value);
}

#[op2(fast)]
fn op_get_num_resources(state: &mut OpState) -> u32 {
    state.borrow::<Natives>().0.borrow::<SharedResources>().0.count() as u32
}

#[op2]
#[string]
fn op_get_resource_by_find_index(state: &mut OpState, index: u32) -> String {
    state.borrow::<Natives>().0.borrow::<SharedResources>().0.name_at(index as usize)
}

#[op2]
#[string]
fn op_get_resource_state(state: &mut OpState, #[string] name: String) -> String {
    state.borrow::<Natives>().0.borrow::<SharedResources>().0.state(&name).to_owned()
}

#[op2]
#[string]
fn op_get_resource_path(state: &mut OpState, #[string] name: String) -> String {
    state.borrow::<Natives>().0.borrow::<SharedResources>().0.path(&name)
}

#[op2(fast)]
fn op_get_num_resource_metadata(
    state: &mut OpState,
    #[string] resource: String,
    #[string] key: String,
) -> u32 {
    state.borrow::<Natives>().0
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
    state.borrow::<Natives>().0
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
    state.borrow::<Natives>().0
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
    state.borrow::<Natives>().0
        .borrow::<SharedResources>()
        .0
        .save_file(&resource, &file_name, &data, data_len)
}

// --- 5. players (backed by the shared PlayerDirectory since B1) ---

#[op2(fast)]
fn op_get_num_player_indices(state: &mut OpState) -> u32 {
    state.borrow::<Natives>().0.borrow::<SharedPlayers>().0.count() as u32
}

#[op2(fast)]
fn op_get_player_from_index(state: &mut OpState, index: u32) -> u32 {
    let sources = state.borrow::<Natives>().0.borrow::<SharedPlayers>().0.sources();
    sources.get(index as usize).copied().unwrap_or(0)
}

#[op2]
#[string]
fn op_get_player_name(state: &mut OpState, source: u32) -> String {
    state.borrow::<Natives>().0
        .borrow::<SharedPlayers>()
        .0
        .get(source)
        .map(|p| p.name)
        .unwrap_or_default()
}

#[op2(fast)]
fn op_does_player_exist(state: &mut OpState, source: u32) -> bool {
    state.borrow::<Natives>().0.borrow::<SharedPlayers>().0.exists(source)
}

#[op2(fast)]
fn op_get_num_player_identifiers(state: &mut OpState, source: u32) -> u32 {
    state.borrow::<Natives>().0.borrow::<SharedPlayers>().0.identifier_count(source) as u32
}

#[op2]
#[string]
fn op_get_player_identifier(state: &mut OpState, source: u32, index: u32) -> String {
    state.borrow::<Natives>().0
        .borrow::<SharedPlayers>()
        .0
        .identifier_at(source, index as usize)
        .unwrap_or_default()
}

#[op2]
#[string]
fn op_get_player_endpoint(state: &mut OpState, source: u32) -> String {
    state.borrow::<Natives>().0
        .borrow::<SharedPlayers>()
        .0
        .endpoint(source)
        .unwrap_or_default()
}

#[op2]
#[string]
fn op_get_player_guid(state: &mut OpState, source: u32) -> String {
    state.borrow::<Natives>().0
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
    state.borrow::<Natives>().0
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
    state.borrow::<Natives>().0.borrow::<SharedDeferrals>().0.defer(source);
}

#[op2(fast)]
fn op_deferral_update(state: &mut OpState, source: u32, #[string] message: String) {
    state.borrow::<Natives>().0.borrow::<SharedDeferrals>().0.update(source, message);
}

#[op2(fast)]
fn op_deferral_done(state: &mut OpState, source: u32, #[string] reason: String) {
    let reason = if reason.is_empty() {
        None
    } else {
        Some(reason)
    };
    state.borrow::<Natives>().0.borrow::<SharedDeferrals>().0.done(source, reason);
}

#[op2(fast)]
fn op_deferral_present_card(state: &mut OpState, source: u32, #[string] card_json: String) {
    state.borrow::<Natives>().0
        .borrow::<SharedDeferrals>()
        .0
        .present_card(source, card_json);
}

#[op2(fast)]
fn op_set_kick_reason(state: &mut OpState, source: u32, #[string] reason: String) {
    state.borrow::<Natives>().0
        .borrow::<SharedDeferrals>()
        .0
        .set_kick_reason(source, reason);
}

// --- 7. zone mesh (Phase D handoffs) ---

/// `RegisterZoneTransferState(cb)` — bookkeeping; the callback stays JS-side.
#[op2(fast)]
fn op_register_zone_transfer_state(state: &mut OpState) {
    let ctx = state.borrow_mut::<Natives>().0.borrow_mut::<RuntimeContext>();
    tracing::debug!(target: "mesh", resource = %ctx.resource_name, "zone transfer state registered");
    ctx.has_zone_transfer_state = true;
}

/// Called by `__baston.collectZoneTransferState(source)` to hand the merged
/// JSON object back to Rust.
#[op2(fast)]
fn op_report_zone_transfer_state(state: &mut OpState, #[string] json: String) {
    state.borrow_mut::<Natives>().0
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

// --- 8. inbound HTTP (`SetHttpHandler`) ---

/// `SetHttpHandler(handler)` — the callback stays JS-side; Rust only needs to
/// know this resource answers requests, so the gateway route can 404 the
/// others instead of dispatching an event nobody handles.
#[op2(fast)]
fn op_set_http_handler(state: &mut OpState) {
    let resource = state.borrow::<Natives>().0.borrow::<RuntimeContext>().resource_name.clone();
    state.borrow::<Natives>().0.borrow::<SharedHttpHandlers>().0.register(&resource);
    tracing::debug!(target: "http", %resource, "resource registered an HTTP handler");
}

/// `response.send()` — resolve the parked request. Headers arrive as a JSON
/// object because a header map is not an op-native type; a malformed one costs
/// the headers, not the response.
#[op2(fast)]
fn op_http_response(
    state: &mut OpState,
    id: u32,
    status: u32,
    #[string] headers_json: String,
    #[string] body: String,
) {
    let headers = serde_json::from_str::<serde_json::Value>(&headers_json)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .map(|map| {
            map.into_iter()
                .map(|(name, value)| {
                    let value = match value {
                        serde_json::Value::String(s) => s,
                        // `writeHead` accepts an array for repeated headers.
                        serde_json::Value::Array(items) => items
                            .iter()
                            .map(|item| match item {
                                serde_json::Value::String(s) => s.clone(),
                                other => other.to_string(),
                            })
                            .collect::<Vec<_>>()
                            .join(", "),
                        other => other.to_string(),
                    };
                    (name, value)
                })
                .collect()
        })
        .unwrap_or_default();

    let status = u16::try_from(status).unwrap_or(500);
    let delivered = state.borrow::<Natives>().0.borrow::<SharedHttpHandlers>().0.complete(
        id,
        crate::ScriptHttpResponse {
            status,
            headers,
            body,
        },
    );
    if !delivered {
        // Either a double send() or an answer past the gateway's deadline.
        // Both are silent in FXServer; say it once here rather than leaving it
        // undiagnosable.
        let resource = &state.borrow::<Natives>().0.borrow::<RuntimeContext>().resource_name;
        tracing::debug!(
            target: "http",
            resource,
            id,
            "HTTP response discarded: the request is no longer waiting"
        );
    }
}

deno_core::extension!(baston_http, ops = [op_set_http_handler, op_http_response]);

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
        baston_http::init(),
    ]
}
