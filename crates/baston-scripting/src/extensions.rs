//! The six BASTON deno_core extensions (console, events, exports, runtime,
//! players, deferrals).
//!
//! Design note: JS callbacks never cross the op boundary. `bootstrap.js` keeps
//! the callback registry (event name → functions, export name → function) on
//! the JS side; these ops only do Rust-side bookkeeping, logging, and deferral
//! resolution. Event dispatch from Rust into JS goes through
//! `globalThis.__baston.dispatch(...)` via `execute_script`, which keeps us
//! independent from `v8::Global<v8::Function>` juggling.

use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use deno_core::{op2, OpState};

use crate::deferrals::DeferralRegistry;

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
}

/// Shared deferral registry handle stored in `OpState` (one per process,
/// cloned into every runtime).
pub struct SharedDeferrals(pub Arc<DeferralRegistry>);

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
fn op_trigger_event(state: &mut OpState, #[string] event: String, #[string] args_json: String) {
    state
        .borrow_mut::<RuntimeContext>()
        .queued_events
        .push_back((event, args_json));
}

deno_core::extension!(
    baston_events,
    ops = [op_add_event_handler, op_trigger_event]
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
    ops = [op_get_game_timer, op_get_current_resource_name]
);

// --- 5. players (Phase A stubs — no game state yet) ---

#[op2(fast)]
fn op_get_num_player_indices() -> u32 {
    0
}

#[op2(fast)]
fn op_get_player_from_index(_index: u32) -> u32 {
    0
}

deno_core::extension!(
    baston_players,
    ops = [op_get_num_player_indices, op_get_player_from_index]
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
    ]
}
