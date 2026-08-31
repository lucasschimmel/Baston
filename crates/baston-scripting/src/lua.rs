//! `LuaRuntime` — one Lua state per resource (ADR-002, Tier 2).
//!
//! The counterpart of `ScriptRuntime`, and deliberately much
//! smaller: Lua has no event loop of its own, so a dispatch is a synchronous
//! call and concurrency is cooperative coroutines driven by `tick`. Everything
//! a native does is shared with the V8 path through
//! [`NativeState`](crate::native_state::NativeState) — this file is only the
//! Lua half of the bridge, and any logic that accumulates here is logic the JS
//! path will not get.
//!
//! Like a V8 isolate, a Lua state is single-threaded and never shared: the
//! host gives each resource its own OS thread and the state never leaves it.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mlua::{Lua, LuaSerdeExt, Value, Variadic};

use crate::error::ScriptError;
use crate::native_state::{
    NativeState, RuntimeContext, SharedConvars, SharedDb, SharedDeferrals, SharedEntityWorld,
    SharedHttp, SharedHttpHandlers, SharedKvp, SharedNet, SharedObservability, SharedPlayers,
    SharedResourceControl, SharedResources, SharedRouting, SharedStateBags, SharedVoice,
    SharedWorldControl,
};
use crate::natives::{console, server};
use crate::observability::Observability;

const PRELUDE_LUA: &str = include_str!("../assets/prelude.lua");

/// Wall-clock budget for a single dispatch, matching the V8 path's
/// `DISPATCH_BUDGET`. Generous on purpose: legitimate handlers finish in
/// milliseconds, so this only fires on a genuinely runaway script.
const DISPATCH_BUDGET: Duration = Duration::from_secs(10);

/// How often the watchdog hook runs, in VM instructions.
///
/// Small enough that a tight `while true do end` is caught within a few
/// milliseconds, large enough that the check is noise next to the work a real
/// handler does.
const WATCHDOG_INSTRUCTIONS: u32 = 100_000;

/// Interrupts a runaway Lua script.
///
/// Where V8 needs a separate thread holding an `IsolateHandle` — its
/// `execute_script` cannot be cancelled from inside — Lua can interrupt
/// itself: a debug hook runs every N instructions and returns an error once
/// the dispatch overruns its budget, which unwinds the script exactly like any
/// other Lua error. No thread, no handle, no cross-thread synchronisation.
#[derive(Clone)]
struct Watchdog {
    /// Millis since the host started; `0` means disarmed.
    deadline: Rc<std::cell::Cell<u64>>,
    /// Set when the hook actually terminated a dispatch.
    fired: Rc<std::cell::Cell<bool>>,
    /// How long one dispatch may run. A field rather than a constant so tests
    /// can exercise the real arming path in milliseconds instead of waiting
    /// out the production budget.
    budget: Rc<std::cell::Cell<Duration>>,
}

impl Default for Watchdog {
    fn default() -> Self {
        Self {
            deadline: Rc::new(std::cell::Cell::new(0)),
            fired: Rc::new(std::cell::Cell::new(false)),
            budget: Rc::new(std::cell::Cell::new(DISPATCH_BUDGET)),
        }
    }
}

/// Disarms the watchdog when a dispatch returns, including on an early `?`.
struct WatchdogGuard(Rc<std::cell::Cell<u64>>);

impl Drop for WatchdogGuard {
    fn drop(&mut self) {
        self.0.set(0);
    }
}

impl Watchdog {
    /// Arm for one dispatch; the returned guard disarms on drop.
    fn arm(&self, host_started_at: Instant) -> WatchdogGuard {
        self.fired.set(false);
        let deadline = (host_started_at.elapsed() + self.budget.get()).as_millis() as u64;
        self.deadline.set(deadline.max(1));
        WatchdogGuard(Rc::clone(&self.deadline))
    }

    /// Whether the watchdog terminated the dispatch that just ran.
    fn took_fired(&self) -> bool {
        self.fired.replace(false)
    }
}

/// A server → client native call this runtime is waiting on.
struct PendingClientNative {
    rx: tokio::sync::oneshot::Receiver<serde_json::Value>,
    started: Instant,
    hash: u64,
    source: u32,
}

/// One resource's Lua state plus the native services behind it.
pub struct LuaRuntime {
    lua: Lua,
    /// Shared with every host function bound into Lua. `RefCell` rather than a
    /// lock: the state is thread-confined, and a native that re-enters Lua
    /// would be a bug we want to hear about as a borrow panic, not a deadlock.
    state: Rc<RefCell<NativeState>>,
    resource_name: String,
    observability: Arc<Observability>,
    watchdog: Watchdog,
    host_started_at: Instant,
    /// Client-native calls awaiting a reply.
    ///
    /// The JS path awaits a `oneshot` inside the op. Lua cannot: a Lua
    /// function is synchronous, and blocking the thread would stop the very
    /// tick loop that delivers the reply. So the call is registered here and
    /// the script polls it from a coroutine — cooperative waiting, which is
    /// how CFX Lua models every other asynchronous surface.
    pending_client_natives: Rc<RefCell<std::collections::HashMap<u64, PendingClientNative>>>,
}

impl LuaRuntime {
    pub fn new(
        resource_name: &str,
        host_started_at: Instant,
        deferrals: Arc<crate::deferrals::DeferralRegistry>,
        players: Arc<baston_protocol::PlayerDirectory>,
        net: crate::net_bridge::NetBridge,
        observability: Arc<Observability>,
    ) -> Result<Self, ScriptError> {
        let mut native_state = NativeState::new();
        native_state.put(RuntimeContext::new(resource_name, host_started_at));
        native_state.put(SharedDeferrals(deferrals));
        native_state.put(SharedPlayers(players));
        native_state.put(SharedNet(net));
        native_state.put(SharedObservability(Arc::clone(&observability)));
        native_state.put(SharedStateBags(crate::StateBagStore::default()));
        native_state.put(SharedRouting(Arc::new(
            crate::InMemoryRoutingControl::default(),
        )));
        native_state.put(SharedEntityWorld(Arc::new(crate::EntityWorldView::new())));
        native_state.put(SharedWorldControl(Arc::new(crate::NoWorldControl)));
        native_state.put(SharedKvp(Arc::new(crate::KvpStore::in_memory())));
        native_state.put(SharedHttp(None));
        native_state.put(SharedHttpHandlers(Arc::new(
            crate::HttpHandlerRegistry::new(),
        )));
        native_state.put(SharedResourceControl(Arc::new(crate::NoResourceControl)));
        native_state.put(SharedVoice(None));
        native_state.put(SharedConvars(Arc::new(dashmap::DashMap::new())));
        native_state.put(SharedDb(None));

        let runtime = Self {
            lua: Lua::new(),
            state: Rc::new(RefCell::new(native_state)),
            resource_name: resource_name.to_owned(),
            observability,
            watchdog: Watchdog::default(),
            host_started_at,
            pending_client_natives: Rc::new(RefCell::new(std::collections::HashMap::new())),
        };
        runtime.install_watchdog();
        runtime.install_host_table(host_started_at)?;
        runtime
            .lua
            .load(PRELUDE_LUA)
            .set_name("baston:prelude.lua")
            .exec()
            .map_err(|e| ScriptError::RuntimeInit {
                resource: resource_name.to_owned(),
                message: e.to_string(),
            })?;
        Ok(runtime)
    }

    pub fn resource_name(&self) -> &str {
        &self.resource_name
    }

    /// Arm the debug hook that terminates a runaway dispatch.
    ///
    /// The hook is installed once and stays installed; the cost when disarmed
    /// is one `Cell` read every [`WATCHDOG_INSTRUCTIONS`] instructions.
    fn install_watchdog(&self) {
        let deadline = Rc::clone(&self.watchdog.deadline);
        let fired = Rc::clone(&self.watchdog.fired);
        let budget = Rc::clone(&self.watchdog.budget);
        let started = self.host_started_at;
        let resource = self.resource_name.clone();
        self.lua.set_hook(
            mlua::HookTriggers::new().every_nth_instruction(WATCHDOG_INSTRUCTIONS),
            move |_lua, _debug| {
                let deadline_ms = deadline.get();
                if deadline_ms == 0 || (started.elapsed().as_millis() as u64) < deadline_ms {
                    return Ok(mlua::VmState::Continue);
                }
                // Disarm before erroring: the unwind runs Lua code (`pcall`
                // handlers, `__close`), and re-firing there would mask the
                // original site.
                deadline.set(0);
                fired.set(true);
                tracing::error!(
                    target: "script",
                    resource = %resource,
                    budget_ms = budget.get().as_millis() as u64,
                    "terminated a Lua dispatch that overran its budget"
                );
                Err(mlua::Error::runtime(
                    "BASTON: script exceeded its dispatch budget and was terminated",
                ))
            },
        );
    }

    /// Bind `__baston`: the only surface the prelude may call into Rust with.
    fn install_host_table(&self, host_started_at: Instant) -> Result<(), ScriptError> {
        let lua = &self.lua;
        let table = lua.create_table().map_err(|e| self.init_error(&e))?;

        // --- JSON, the boundary format shared with the JS path ---
        let encode = lua
            .create_function(|lua, value: Value| {
                let json: serde_json::Value = lua.from_value(value)?;
                Ok(json.to_string())
            })
            .map_err(|e| self.init_error(&e))?;
        table
            .set("json_encode", encode)
            .map_err(|e| self.init_error(&e))?;

        let decode = lua
            .create_function(|lua, text: String| {
                let json: serde_json::Value =
                    serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
                lua.to_value(&json)
            })
            .map_err(|e| self.init_error(&e))?;
        table
            .set("json_decode", decode)
            .map_err(|e| self.init_error(&e))?;

        // --- the natives, shared verbatim with the V8 path ---
        //
        // One entry point, not two: Lua has no equivalent of the JS polyfill's
        // generated bindings, so a resource calling `GetPlayerName(1)` cannot
        // know which dispatcher owns the name. The host routes it.
        let state = Rc::clone(&self.state);
        let native = lua
            .create_function(move |_, (name, kind, args): (String, String, String)| {
                Ok(server::cfx_native(
                    &mut state.borrow_mut(),
                    name,
                    kind,
                    args,
                ))
            })
            .map_err(|e| self.init_error(&e))?;
        table
            .set("native", native)
            .map_err(|e| self.init_error(&e))?;

        // --- console ---
        let resource = self.resource_name.clone();
        let print = lua
            .create_function(move |lua, args: Variadic<Value>| {
                let mut parts = Vec::with_capacity(args.len());
                for value in args.iter() {
                    parts.push(lua_tostring(lua, value));
                }
                console::log(&resource, &parts.join("\t"));
                Ok(())
            })
            .map_err(|e| self.init_error(&e))?;
        lua.globals()
            .set("print", print)
            .map_err(|e| self.init_error(&e))?;

        // --- event bookkeeping, mirroring the JS ops of the same names ---
        let state = Rc::clone(&self.state);
        let add_event_handler = lua
            .create_function(move |_, event: String| {
                state
                    .borrow_mut()
                    .borrow_mut::<RuntimeContext>()
                    .handled_events
                    .insert(event);
                Ok(())
            })
            .map_err(|e| self.init_error(&e))?;
        table
            .set("add_event_handler", add_event_handler)
            .map_err(|e| self.init_error(&e))?;

        let state = Rc::clone(&self.state);
        let trigger_event = lua
            .create_function(move |_, (event, args): (String, String)| {
                state
                    .borrow_mut()
                    .borrow_mut::<RuntimeContext>()
                    .queued_events
                    .push_back((event, args));
                Ok(())
            })
            .map_err(|e| self.init_error(&e))?;
        table
            .set("trigger_event", trigger_event)
            .map_err(|e| self.init_error(&e))?;

        let state = Rc::clone(&self.state);
        let trigger_client_event = lua
            .create_function(move |_, (event, source, args): (String, u32, String)| {
                let state = state.borrow();
                let net = &state.borrow::<SharedNet>().0;
                if net
                    .tx
                    .try_send(crate::net_bridge::NetOutbound::ClientEvent {
                        source,
                        event: event.clone(),
                        args_json: args,
                    })
                    .is_err()
                {
                    tracing::warn!(target: "events", %event, source,
                        "client event dropped: net bridge full or closed");
                }
                Ok(())
            })
            .map_err(|e| self.init_error(&e))?;
        table
            .set("trigger_client_event", trigger_client_event)
            .map_err(|e| self.init_error(&e))?;

        let state = Rc::clone(&self.state);
        let register_command = lua
            .create_function(move |_, (name, restricted): (String, bool)| {
                state
                    .borrow_mut()
                    .borrow_mut::<RuntimeContext>()
                    .commands
                    .insert(name, restricted);
                Ok(())
            })
            .map_err(|e| self.init_error(&e))?;
        table
            .set("register_command", register_command)
            .map_err(|e| self.init_error(&e))?;

        // --- exports ---
        let state = Rc::clone(&self.state);
        let add_export = lua
            .create_function(move |_, name: String| {
                state
                    .borrow_mut()
                    .borrow_mut::<RuntimeContext>()
                    .exports
                    .insert(name);
                Ok(())
            })
            .map_err(|e| self.init_error(&e))?;
        table
            .set("add_export", add_export)
            .map_err(|e| self.init_error(&e))?;

        let resource = self.resource_name.clone();
        let resource_name = lua
            .create_function(move |_, ()| Ok(resource.clone()))
            .map_err(|e| self.init_error(&e))?;
        table
            .set("resource_name", resource_name)
            .map_err(|e| self.init_error(&e))?;

        // --- state bags ---
        let state = Rc::clone(&self.state);
        let add_state_bag_handler = lua
            .create_function(
                move |_, (key_filter, bag_filter, callback_id): (String, String, u32)| {
                    let state = state.borrow();
                    let resource = state.borrow::<RuntimeContext>().resource_name.clone();
                    Ok(state.borrow::<SharedStateBags>().0.add_handler(
                        resource,
                        Some(key_filter),
                        Some(bag_filter),
                        callback_id,
                    ))
                },
            )
            .map_err(|e| self.init_error(&e))?;
        table
            .set("add_state_bag_handler", add_state_bag_handler)
            .map_err(|e| self.init_error(&e))?;

        let state = Rc::clone(&self.state);
        let remove_state_bag_handler = lua
            .create_function(move |_, cookie: u32| {
                let state = state.borrow();
                let resource = state.borrow::<RuntimeContext>().resource_name.clone();
                Ok(state
                    .borrow::<SharedStateBags>()
                    .0
                    .remove_handler(&resource, cookie))
            })
            .map_err(|e| self.init_error(&e))?;
        table
            .set("remove_state_bag_handler", remove_state_bag_handler)
            .map_err(|e| self.init_error(&e))?;

        let state = Rc::clone(&self.state);
        let poll_state_bag_changes = lua
            .create_function(move |_, ()| {
                // Same cap as the JS op: a burst must not turn one dispatch
                // into an unbounded amount of script work.
                const MAX_DELIVERIES_PER_POLL: usize = 4096;
                let state = state.borrow();
                let resource = state.borrow::<RuntimeContext>().resource_name.clone();
                let deliveries = state
                    .borrow::<SharedStateBags>()
                    .0
                    .drain_deliveries(&resource, MAX_DELIVERIES_PER_POLL);
                Ok(serde_json::to_string(&deliveries).unwrap_or_else(|_| "[]".to_owned()))
            })
            .map_err(|e| self.init_error(&e))?;
        table
            .set("poll_state_bag_changes", poll_state_bag_changes)
            .map_err(|e| self.init_error(&e))?;

        // --- deferrals (playerConnecting) ---
        let state = Rc::clone(&self.state);
        let deferral_defer = lua
            .create_function(move |_, source: u32| {
                state.borrow().borrow::<SharedDeferrals>().0.defer(source);
                Ok(())
            })
            .map_err(|e| self.init_error(&e))?;
        table
            .set("deferral_defer", deferral_defer)
            .map_err(|e| self.init_error(&e))?;

        let state = Rc::clone(&self.state);
        let deferral_update = lua
            .create_function(move |_, (source, message): (u32, String)| {
                state
                    .borrow()
                    .borrow::<SharedDeferrals>()
                    .0
                    .update(source, message);
                Ok(())
            })
            .map_err(|e| self.init_error(&e))?;
        table
            .set("deferral_update", deferral_update)
            .map_err(|e| self.init_error(&e))?;

        let state = Rc::clone(&self.state);
        let deferral_done = lua
            .create_function(move |_, (source, reason): (u32, String)| {
                // Empty means "accepted"; a reason means "rejected with this
                // message". Same contract as the JS op.
                let reason = (!reason.is_empty()).then_some(reason);
                state
                    .borrow()
                    .borrow::<SharedDeferrals>()
                    .0
                    .done(source, reason);
                Ok(())
            })
            .map_err(|e| self.init_error(&e))?;
        table
            .set("deferral_done", deferral_done)
            .map_err(|e| self.init_error(&e))?;

        let state = Rc::clone(&self.state);
        let deferral_present_card = lua
            .create_function(move |_, (source, card_json): (u32, String)| {
                state
                    .borrow()
                    .borrow::<SharedDeferrals>()
                    .0
                    .present_card(source, card_json);
                Ok(())
            })
            .map_err(|e| self.init_error(&e))?;
        table
            .set("deferral_present_card", deferral_present_card)
            .map_err(|e| self.init_error(&e))?;

        let state = Rc::clone(&self.state);
        let set_kick_reason = lua
            .create_function(move |_, (source, reason): (u32, String)| {
                state
                    .borrow()
                    .borrow::<SharedDeferrals>()
                    .0
                    .set_kick_reason(source, reason);
                Ok(())
            })
            .map_err(|e| self.init_error(&e))?;
        table
            .set("set_kick_reason", set_kick_reason)
            .map_err(|e| self.init_error(&e))?;

        // --- zone transfer (mesh handoffs) ---
        let state = Rc::clone(&self.state);
        let register_zone_transfer_state = lua
            .create_function(move |_, ()| {
                state
                    .borrow_mut()
                    .borrow_mut::<RuntimeContext>()
                    .has_zone_transfer_state = true;
                Ok(())
            })
            .map_err(|e| self.init_error(&e))?;
        table
            .set("register_zone_transfer_state", register_zone_transfer_state)
            .map_err(|e| self.init_error(&e))?;

        // --- server → client native dispatch ---
        let state = Rc::clone(&self.state);
        let pending = Rc::clone(&self.pending_client_natives);
        let invoke_client_native = lua
            .create_function(
                move |_,
                      (source, hash_hex, args_json, expects_return): (
                    u32,
                    String,
                    String,
                    bool,
                )| {
                    let hash = match u64::from_str_radix(hash_hex.trim_start_matches("0x"), 16) {
                        Ok(hash) => hash,
                        Err(e) => {
                            return Ok((None, Some(format!("invalid native hash {hash_hex}: {e}"))))
                        }
                    };
                    let args: Vec<serde_json::Value> = match serde_json::from_str(&args_json) {
                        Ok(args) => args,
                        Err(e) => return Ok((None, Some(format!("invalid native args: {e}")))),
                    };
                    // Same validation the JS path runs: a bogus hash or arity
                    // must not reach a client as a malformed call.
                    static REGISTRY: std::sync::OnceLock<baston_protocol::native::NativeRegistry> =
                        std::sync::OnceLock::new();
                    if let Err(e) = REGISTRY
                        .get_or_init(baston_protocol::native::NativeRegistry::new)
                        .validate(hash, args.len())
                    {
                        return Ok((None, Some(e.to_string())));
                    }

                    let (net, observability, resource) = {
                        let state = state.borrow();
                        (
                            state.borrow::<SharedNet>().0.clone(),
                            Arc::clone(&state.borrow::<SharedObservability>().0),
                            state.borrow::<RuntimeContext>().resource_name.clone(),
                        )
                    };
                    let started = Instant::now();
                    let (id, rx) = net.pending_natives.register();
                    if !crate::natives::client::queue_native_call(&net, source, id, hash, args) {
                        net.pending_natives.cancel(id);
                        observability
                            .record_native_roundtrip(&resource, hash, source, 0, false, true);
                        return Ok((None, Some("net bridge full or closed".to_owned())));
                    }
                    if !expects_return {
                        // The shim still replies; nobody is waiting for it.
                        net.pending_natives.cancel(id);
                        observability.record_native_roundtrip(
                            &resource,
                            hash,
                            source,
                            started.elapsed().as_micros() as u64,
                            false,
                            false,
                        );
                        return Ok((None, None));
                    }
                    pending.borrow_mut().insert(
                        id,
                        PendingClientNative {
                            rx,
                            started,
                            hash,
                            source,
                        },
                    );
                    Ok((Some(id), None))
                },
            )
            .map_err(|e| self.init_error(&e))?;
        table
            .set("invoke_client_native", invoke_client_native)
            .map_err(|e| self.init_error(&e))?;

        let state = Rc::clone(&self.state);
        let pending = Rc::clone(&self.pending_client_natives);
        let poll_client_native = lua
            .create_function(move |_, id: u64| {
                let mut pending = pending.borrow_mut();
                let Some(call) = pending.get_mut(&id) else {
                    // Unknown id: already collected, or never registered.
                    return Ok(Some(
                        serde_json::json!({ "__error": "unknown native call" }).to_string(),
                    ));
                };
                let elapsed = call.started.elapsed();
                let (outcome, timed_out) = match call.rx.try_recv() {
                    Ok(value) => (Some(value.to_string()), false),
                    Err(tokio::sync::oneshot::error::TryRecvError::Empty)
                        if elapsed < crate::natives::client::NATIVE_CALL_TIMEOUT =>
                    {
                        return Ok(None); // still in flight; the caller yields
                    }
                    Err(tokio::sync::oneshot::error::TryRecvError::Empty) => (
                        Some(
                            serde_json::json!({ "__error": "client did not answer in time" })
                                .to_string(),
                        ),
                        true,
                    ),
                    Err(tokio::sync::oneshot::error::TryRecvError::Closed) => (
                        Some(
                            serde_json::json!({ "__error": "native result channel closed" })
                                .to_string(),
                        ),
                        true,
                    ),
                };
                let (hash, source) = (call.hash, call.source);
                pending.remove(&id);
                let state = state.borrow();
                state
                    .borrow::<SharedObservability>()
                    .0
                    .record_native_roundtrip(
                        &state.borrow::<RuntimeContext>().resource_name,
                        hash,
                        source,
                        elapsed.as_micros() as u64,
                        timed_out,
                        timed_out,
                    );
                Ok(outcome)
            })
            .map_err(|e| self.init_error(&e))?;
        table
            .set("poll_client_native", poll_client_native)
            .map_err(|e| self.init_error(&e))?;

        // --- database (the `db` module) ---
        let state = Rc::clone(&self.state);
        let db_submit = lua
            .create_function(
                move |_, (kind, sql, params_json): (String, String, String)| {
                    let state = state.borrow();
                    let Some(db) = state.borrow::<SharedDb>().0.as_ref() else {
                        // A resource that queries with the module off gets told
                        // why, not an empty result set it would read as "no rows".
                        return Ok((
                            None,
                            Some(
                                "the db module is disabled — add \"db\" to [modules] enable"
                                    .to_owned(),
                            ),
                        ));
                    };
                    let params: Vec<serde_json::Value> =
                        serde_json::from_str(&params_json).unwrap_or_default();
                    let resource = state.borrow::<RuntimeContext>().resource_name.clone();
                    match db.submit(&resource, &kind, sql, params) {
                        Ok(id) => Ok((Some(id), None)),
                        Err(e) => Ok((None, Some(e))),
                    }
                },
            )
            .map_err(|e| self.init_error(&e))?;
        table
            .set("db_submit", db_submit)
            .map_err(|e| self.init_error(&e))?;

        let state = Rc::clone(&self.state);
        let db_collect = lua
            .create_function(move |_, id: u64| {
                let state = state.borrow();
                let Some(db) = state.borrow::<SharedDb>().0.as_ref() else {
                    return Ok((
                        true,
                        serde_json::json!({ "__error": "db module disabled" }).to_string(),
                    ));
                };
                // `(ready, payload)` rather than nil-or-string: a query that
                // legitimately returns nil must not read as "still running".
                match db.collect(id) {
                    None => Ok((false, String::new())),
                    Some(Ok(value)) => Ok((true, value.to_string())),
                    Some(Err(message)) => {
                        Ok((true, serde_json::json!({ "__error": message }).to_string()))
                    }
                }
            })
            .map_err(|e| self.init_error(&e))?;
        table
            .set("db_collect", db_collect)
            .map_err(|e| self.init_error(&e))?;

        // --- clock and error reporting ---
        let game_timer = lua
            .create_function(move |_, ()| Ok(host_started_at.elapsed().as_millis() as i64))
            .map_err(|e| self.init_error(&e))?;
        table
            .set("game_timer", game_timer)
            .map_err(|e| self.init_error(&e))?;

        let state = Rc::clone(&self.state);
        let resource = self.resource_name.clone();
        let report_error = lua
            .create_function(move |_, message: String| {
                tracing::error!(target: "script", resource = %resource, "{message}");
                state
                    .borrow_mut()
                    .borrow_mut::<RuntimeContext>()
                    .handler_errors += 1;
                Ok(())
            })
            .map_err(|e| self.init_error(&e))?;
        table
            .set("report_error", report_error)
            .map_err(|e| self.init_error(&e))?;

        lua.globals()
            .set("__baston", table)
            .map_err(|e| self.init_error(&e))?;
        Ok(())
    }

    fn init_error(&self, e: &mlua::Error) -> ScriptError {
        ScriptError::RuntimeInit {
            resource: self.resource_name.clone(),
            message: e.to_string(),
        }
    }

    /// Install the process-wide services, replacing the placeholders.
    pub fn install_server_state(
        &mut self,
        convars: Arc<dashmap::DashMap<String, String>>,
        resources: crate::resource_registry::ResourceRegistry,
    ) {
        let mut state = self.state.borrow_mut();
        state.put(SharedConvars(convars));
        state.put(SharedResources(resources));
    }

    pub fn install_shared_game_state(&mut self, shared: crate::native_state::SharedGameState) {
        let mut state = self.state.borrow_mut();
        state.put(SharedStateBags(shared.state_bags));
        state.put(SharedRouting(shared.routing));
        state.put(SharedEntityWorld(shared.entity_world));
        state.put(SharedWorldControl(shared.world_control));
        state.put(SharedKvp(shared.kvp));
        state.put(SharedHttp(shared.http));
        state.put(SharedHttpHandlers(shared.http_handlers));
        state.put(SharedResourceControl(shared.resource_control));
    }

    pub fn install_voice(&mut self, voice: SharedVoice) {
        self.state.borrow_mut().put(voice);
    }

    /// Install the database pool backing the `db` natives.
    pub fn install_db(&mut self, db: crate::native_state::SharedDb) {
        self.state.borrow_mut().put(db);
    }

    /// Run one resource script.
    pub fn execute_script(&mut self, path: &str, code: &str) -> Result<(), ScriptError> {
        let started = Instant::now();
        // A runaway top-level load is covered from the first script, not only
        // once handlers start running.
        let guard = self.watchdog.arm(self.host_started_at);
        // CitizenFX's Lua accepts compound assignment; the reference Lua mlua
        // embeds does not. Translating before the load is what lets a resource
        // written for FiveM run here unchanged — see `crate::cfx_lua`. Rewrites
        // stay on their own line, so `@path:line` still points where the author
        // wrote it.
        let code = crate::cfx_lua::expand_compound_assignment(code);
        let result = self
            .lua
            .load(&code)
            .set_name(format!("@{path}"))
            .exec()
            .map_err(|e| ScriptError::Execute {
                resource: self.resource_name.clone(),
                script: path.to_owned(),
                message: e.to_string(),
            });
        drop(guard);
        let elapsed = started.elapsed().as_micros() as u64;
        self.observability
            .record_dispatch(crate::observability::DispatchMeasurement {
                resource: self.resource_name.clone(),
                kind: crate::observability::DispatchKind::LoadScript,
                name: path.to_owned(),
                execute_us: elapsed,
                // Lua has no event loop of its own: a script runs to
                // completion, so every microsecond is execute time.
                event_loop_us: 0,
                total_us: elapsed,
                errored: result.is_err(),
                watchdog_fired: self.watchdog.took_fired(),
                memory: None,
                source: None,
                zone: None,
            });
        result
    }

    /// Dispatch an event to this resource's handlers.
    ///
    /// `source` is `None` for server-side events and `Some` for client → server
    /// ones, which are only delivered when the resource called
    /// `RegisterNetEvent` — a resource must not receive traffic it never opted
    /// into.
    pub fn dispatch_event(
        &mut self,
        event: &str,
        args_json: &str,
        source: Option<u32>,
        kind: crate::observability::DispatchKind,
    ) -> Result<(), ScriptError> {
        if source.is_some() && !self.accepts_net_event(event) {
            return Ok(());
        }
        let started = Instant::now();
        let dispatch: mlua::Table = self.dispatch_table()?;
        let guard = self.watchdog.arm(self.host_started_at);
        let errors: Result<u32, mlua::Error> = dispatch
            .get::<mlua::Function>("event")
            .and_then(|f| f.call((event, args_json, source)));
        drop(guard);
        let watchdog_fired = self.watchdog.took_fired();
        let errors = errors.map_err(|e| ScriptError::Execute {
            resource: self.resource_name.clone(),
            script: event.to_owned(),
            message: e.to_string(),
        })?;
        let elapsed = started.elapsed().as_micros() as u64;
        self.observability
            .record_dispatch(crate::observability::DispatchMeasurement {
                resource: self.resource_name.clone(),
                kind,
                name: event.to_owned(),
                execute_us: elapsed,
                event_loop_us: 0,
                total_us: elapsed,
                errored: errors > 0 || watchdog_fired,
                watchdog_fired,
                memory: None,
                source,
                zone: None,
            });
        Ok(())
    }

    /// Run the `playerConnecting` handlers, deferrals and all.
    ///
    /// Separate from [`Self::dispatch_event`] because the handler signature is
    /// different: CFX passes `(name, setKickReason, deferrals)`, not the event
    /// arguments.
    pub fn dispatch_player_connecting(
        &mut self,
        source: u32,
        player_name: &str,
    ) -> Result<(), ScriptError> {
        let started = Instant::now();
        let dispatch = self.dispatch_table()?;
        let guard = self.watchdog.arm(self.host_started_at);
        let errors: Result<u32, mlua::Error> = dispatch
            .get::<mlua::Function>("player_connecting")
            .and_then(|f| f.call((source, player_name)));
        drop(guard);
        let watchdog_fired = self.watchdog.took_fired();
        let errors = errors.map_err(|e| ScriptError::Execute {
            resource: self.resource_name.clone(),
            script: "playerConnecting".to_owned(),
            message: e.to_string(),
        })?;
        let elapsed = started.elapsed().as_micros() as u64;
        self.observability
            .record_dispatch(crate::observability::DispatchMeasurement {
                resource: self.resource_name.clone(),
                kind: crate::observability::DispatchKind::PlayerConnecting,
                name: "playerConnecting".to_owned(),
                execute_us: elapsed,
                event_loop_us: 0,
                total_us: elapsed,
                errored: errors > 0 || watchdog_fired,
                watchdog_fired,
                memory: None,
                source: Some(source),
                zone: None,
            });
        Ok(())
    }

    /// Deliver queued state-bag changes to this resource's handlers.
    pub fn dispatch_state_bag_changes(&mut self) -> Result<(), ScriptError> {
        let dispatch = self.dispatch_table()?;
        let guard = self.watchdog.arm(self.host_started_at);
        let result: Result<u32, mlua::Error> = dispatch
            .get::<mlua::Function>("state_bag_changes")
            .and_then(|f| f.call(()));
        drop(guard);
        self.watchdog.took_fired();
        result.map(|_| ()).map_err(|e| ScriptError::Execute {
            resource: self.resource_name.clone(),
            script: "stateBagChange".to_owned(),
            message: e.to_string(),
        })
    }

    /// Merge this resource's zone-transfer callbacks for a handoff.
    ///
    /// `None` means the resource registered none, which is the common case and
    /// not an error.
    pub fn collect_zone_transfer_state(
        &mut self,
        source: u32,
    ) -> Result<Option<String>, ScriptError> {
        if !self
            .state
            .borrow()
            .borrow::<RuntimeContext>()
            .has_zone_transfer_state
        {
            return Ok(None);
        }
        let dispatch = self.dispatch_table()?;
        let guard = self.watchdog.arm(self.host_started_at);
        let result: Result<Option<String>, mlua::Error> = dispatch
            .get::<mlua::Function>("collect_zone_transfer_state")
            .and_then(|f| f.call(source));
        drop(guard);
        self.watchdog.took_fired();
        result.map_err(|e| ScriptError::Execute {
            resource: self.resource_name.clone(),
            script: "collectZoneTransferState".to_owned(),
            message: e.to_string(),
        })
    }

    /// Whether the resource registered this event for client → server traffic.
    fn accepts_net_event(&self, event: &str) -> bool {
        self.dispatch_table()
            .and_then(|t| {
                t.get::<mlua::Function>("accepts_net_event")
                    .map_err(|e| self.init_error(&e))
            })
            .and_then(|f| f.call::<bool>(event).map_err(|e| self.init_error(&e)))
            .unwrap_or(false)
    }

    /// Run a registered command. `false` means this resource does not own it.
    pub fn dispatch_command(
        &mut self,
        command: &str,
        source: u32,
        args: &[String],
        raw: &str,
    ) -> Result<bool, ScriptError> {
        let args_json = serde_json::to_string(args).unwrap_or_else(|_| "[]".to_owned());
        let dispatch = self.dispatch_table()?;
        let guard = self.watchdog.arm(self.host_started_at);
        let handled = dispatch
            .get::<mlua::Function>("command")
            .and_then(|f| f.call((command, source, args_json, raw)));
        drop(guard);
        self.watchdog.took_fired();
        handled.map_err(|e| ScriptError::Execute {
            resource: self.resource_name.clone(),
            script: command.to_owned(),
            message: e.to_string(),
        })
    }

    /// Resume coroutines and fire timers; returns how long the runtime would
    /// like to sleep before the next tick.
    pub fn tick(&mut self) -> std::time::Duration {
        // Coroutines are script code too: a thread that never yields is the
        // most likely way to wedge a Lua runtime, so the tick is covered.
        let guard = self.watchdog.arm(self.host_started_at);
        let ms = self
            .dispatch_table()
            .and_then(|t| {
                t.get::<mlua::Function>("tick")
                    .map_err(|e| self.init_error(&e))
            })
            .and_then(|f| f.call::<i64>(()).map_err(|e| self.init_error(&e)))
            .unwrap_or(50);
        drop(guard);
        self.watchdog.took_fired();
        std::time::Duration::from_millis(ms.clamp(1, 50) as u64)
    }

    fn dispatch_table(&self) -> Result<mlua::Table, ScriptError> {
        self.lua
            .globals()
            .get::<mlua::Table>("__baston_dispatch")
            .map_err(|e| self.init_error(&e))
    }

    /// Events queued by `TriggerEvent` during the last dispatch, for the host
    /// to re-broadcast to the other resources.
    pub fn drain_queued_events(&mut self) -> Vec<(String, String)> {
        let mut state = self.state.borrow_mut();
        let ctx = state.borrow_mut::<RuntimeContext>();
        ctx.queued_events.drain(..).collect()
    }

    /// Commands this resource registered, so the host can route them.
    pub fn registered_commands(&self) -> Vec<String> {
        self.state
            .borrow()
            .borrow::<RuntimeContext>()
            .commands
            .keys()
            .cloned()
            .collect()
    }
}

/// Lua's own `tostring` semantics, so `print` matches what a resource expects.
fn lua_tostring(lua: &Lua, value: &Value) -> String {
    match value {
        Value::String(s) => s.to_string_lossy().to_string(),
        other => lua
            .globals()
            .get::<mlua::Function>("tostring")
            .and_then(|f| f.call::<String>(other.clone()))
            .unwrap_or_else(|_| format!("{other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime(name: &str) -> LuaRuntime {
        let (net, _rx) = crate::net_bridge::NetBridge::new();
        LuaRuntime::new(
            name,
            Instant::now(),
            Arc::new(crate::deferrals::DeferralRegistry::new()),
            Arc::new(baston_protocol::PlayerDirectory::default()),
            net,
            Arc::new(Observability::new()),
        )
        .expect("runtime boots")
    }

    #[test]
    fn prelude_loads_and_exposes_the_cfx_surface() {
        let rt = runtime("test");
        for global in [
            "Citizen",
            "AddEventHandler",
            "RegisterNetEvent",
            "TriggerEvent",
        ] {
            let value: Value = rt.lua.globals().get(global).unwrap();
            assert!(!matches!(value, Value::Nil), "{global} is missing");
        }
    }

    /// The shape `cfx-server-data`'s money resource is written in. Before the
    /// translation this did not even parse, and the whole server died with it.
    #[test]
    fn a_script_written_in_cfx_lua_runs() {
        let mut rt = runtime("test");
        rt.execute_script(
            "money.lua",
            r#"
            local total = 0
            local wallet = { cash = 10 }
            local log = "start"

            local function add(amount)
                total += amount
                wallet.cash += amount
                log ..= "|" .. tostring(amount)
            end

            add(5)
            add(7)
            result = { total = total, cash = wallet.cash, log = log }
            "#,
        )
        .expect("CfxLua compound assignment must load");

        let result: mlua::Table = rt.lua.globals().get("result").unwrap();
        assert_eq!(result.get::<i64>("total").unwrap(), 12);
        assert_eq!(result.get::<i64>("cash").unwrap(), 22);
        assert_eq!(result.get::<String>("log").unwrap(), "start|5|7");
    }

    /// The translation must not reach into strings, or a resource printing the
    /// syntax would have its own output rewritten.
    #[test]
    fn compound_assignment_inside_a_string_is_left_as_text() {
        let mut rt = runtime("test");
        rt.execute_script("s.lua", r#"literal = "a += b""#).unwrap();
        assert_eq!(rt.lua.globals().get::<String>("literal").unwrap(), "a += b");
    }

    /// RegisterServerEvent is the older name for RegisterNetEvent. Without the
    /// alias it fell through to the native dispatcher and did nothing, so the
    /// resource never opted in and its client events were dropped in silence.
    #[test]
    fn register_server_event_opts_in_the_same_way_register_net_event_does() {
        let mut rt = runtime("test");
        rt.execute_script(
            "main.lua",
            r#"
            got = nil
            RegisterServerEvent("legacy:ping")
            AddEventHandler("legacy:ping", function(value) got = value end)
            "#,
        )
        .unwrap();
        rt.dispatch_event(
            "legacy:ping",
            r#"["from-client"]"#,
            Some(7),
            crate::observability::DispatchKind::Event,
        )
        .unwrap();
        assert_eq!(
            rt.lua.globals().get::<String>("got").unwrap(),
            "from-client",
            "an event registered through the legacy name must still arrive"
        );
    }

    #[test]
    fn a_script_runs_and_its_events_reach_its_handlers() {
        let mut rt = runtime("test");
        rt.execute_script(
            "main.lua",
            r#"
            seen = {}
            AddEventHandler("greet", function(who) seen[#seen + 1] = who end)
            "#,
        )
        .unwrap();
        rt.dispatch_event(
            "greet",
            r#"["world"]"#,
            None,
            crate::observability::DispatchKind::Event,
        )
        .unwrap();
        let seen: mlua::Table = rt.lua.globals().get("seen").unwrap();
        assert_eq!(seen.get::<String>(1).unwrap(), "world");
    }

    #[test]
    fn net_events_need_an_explicit_registration() {
        // A resource must not receive client → server traffic it never opted
        // into, exactly as on the JS path.
        let mut rt = runtime("test");
        rt.execute_script(
            "main.lua",
            r#"
            plain, opted = 0, 0
            AddEventHandler("plain", function() plain = plain + 1 end)
            RegisterNetEvent("opted", function() opted = opted + 1 end)
            "#,
        )
        .unwrap();
        let kind = crate::observability::DispatchKind::NetEvent;
        rt.dispatch_event("plain", "[]", Some(7), kind).unwrap();
        rt.dispatch_event("opted", "[]", Some(7), kind).unwrap();
        assert_eq!(rt.lua.globals().get::<i64>("plain").unwrap(), 0);
        assert_eq!(rt.lua.globals().get::<i64>("opted").unwrap(), 1);
    }

    #[test]
    fn source_is_visible_to_a_net_event_handler() {
        let mut rt = runtime("test");
        rt.execute_script(
            "main.lua",
            r#"
            got = -1
            RegisterNetEvent("ping", function() got = source end)
            "#,
        )
        .unwrap();
        rt.dispatch_event(
            "ping",
            "[]",
            Some(42),
            crate::observability::DispatchKind::NetEvent,
        )
        .unwrap();
        assert_eq!(rt.lua.globals().get::<i64>("got").unwrap(), 42);
    }

    #[test]
    fn trigger_event_queues_for_the_host_to_rebroadcast() {
        let mut rt = runtime("test");
        rt.execute_script("main.lua", r#"TriggerEvent("hello", 1, "two")"#)
            .unwrap();
        let queued = rt.drain_queued_events();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].0, "hello");
        assert!(queued[0].1.contains("two"), "{}", queued[0].1);
        assert!(rt.drain_queued_events().is_empty(), "draining is one-shot");
    }

    #[test]
    fn a_handler_error_is_counted_and_does_not_stop_the_others() {
        let mut rt = runtime("test");
        rt.execute_script(
            "main.lua",
            r#"
            ran = 0
            AddEventHandler("boom", function() error("nope") end)
            AddEventHandler("boom", function() ran = ran + 1 end)
            "#,
        )
        .unwrap();
        rt.dispatch_event(
            "boom",
            "[]",
            None,
            crate::observability::DispatchKind::Event,
        )
        .unwrap();
        assert_eq!(
            rt.lua.globals().get::<i64>("ran").unwrap(),
            1,
            "a throwing handler must not cancel the rest"
        );
    }

    #[test]
    fn a_syntax_error_is_reported_against_its_script() {
        let mut rt = runtime("test");
        let err = rt
            .execute_script("broken.lua", "this is not lua")
            .expect_err("must not load");
        assert!(matches!(err, ScriptError::Execute { .. }), "{err:?}");
    }

    #[test]
    fn natives_answer_through_the_shared_implementation() {
        // GET_INVOKING_RESOURCE is served by the same Rust function the JS
        // path calls — this is the whole point of the neutral native layer.
        let mut rt = runtime("my-resource");
        rt.execute_script(
            "main.lua",
            r#"name = Citizen.InvokeNative("GET_INVOKING_RESOURCE")"#,
        )
        .unwrap();
        assert_eq!(
            rt.lua.globals().get::<String>("name").unwrap(),
            "my-resource"
        );
    }

    #[test]
    fn resource_kvp_round_trips_through_the_shared_natives() {
        let mut rt = runtime("kvp-test");
        rt.execute_script(
            "main.lua",
            r#"
            Citizen.InvokeNative("SET_RESOURCE_KVP", "greeting", "bonjour")
            value = Citizen.InvokeNative("GET_RESOURCE_KVP_STRING", "greeting")
            "#,
        )
        .unwrap();
        assert_eq!(rt.lua.globals().get::<String>("value").unwrap(), "bonjour");
    }

    #[test]
    fn commands_register_and_dispatch_only_to_their_owner() {
        let mut rt = runtime("test");
        rt.execute_script(
            "main.lua",
            r#"
            last = nil
            RegisterCommand("greet", function(src, args) last = args[1] end, false)
            "#,
        )
        .unwrap();
        assert_eq!(rt.registered_commands(), vec!["greet".to_owned()]);
        assert!(rt
            .dispatch_command("greet", 1, &["there".into()], "greet there")
            .unwrap());
        assert_eq!(rt.lua.globals().get::<String>("last").unwrap(), "there");
        assert!(
            !rt.dispatch_command("unknown", 1, &[], "unknown").unwrap(),
            "an unowned command must report as unhandled"
        );
    }

    #[test]
    fn threads_yield_and_resume_across_ticks() {
        let mut rt = runtime("test");
        rt.execute_script(
            "main.lua",
            r#"
            steps = 0
            CreateThread(function()
                steps = 1
                Wait(0)
                steps = 2
            end)
            "#,
        )
        .unwrap();
        assert_eq!(
            rt.lua.globals().get::<i64>("steps").unwrap(),
            0,
            "a thread must not run before the first tick"
        );
        rt.tick();
        assert_eq!(rt.lua.globals().get::<i64>("steps").unwrap(), 1);
        rt.tick();
        assert_eq!(rt.lua.globals().get::<i64>("steps").unwrap(), 2);
    }

    #[test]
    fn player_connecting_handlers_receive_the_deferrals_table() {
        // The whitelist/queue pattern: defer, tell the player what is
        // happening, then let them in. This is the single most common reason a
        // FiveM server has server-side script at all.
        let mut rt = runtime("gate");
        // The gateway registers the in-flight connection before dispatching;
        // without it `defer`/`done` have no entry to act on.
        let outcome = rt.state.borrow().borrow::<SharedDeferrals>().0.register(7);
        rt.execute_script(
            "main.lua",
            r#"
            seen_name, steps = nil, {}
            AddEventHandler("playerConnecting", function(name, setKickReason, deferrals)
                seen_name = name
                deferrals.defer()
                steps[#steps + 1] = "defer"
                deferrals.update("checking the whitelist")
                steps[#steps + 1] = "update"
                deferrals.done()
                steps[#steps + 1] = "done"
            end)
            "#,
        )
        .unwrap();
        rt.dispatch_player_connecting(7, "Lucas").unwrap();

        assert_eq!(
            rt.lua.globals().get::<String>("seen_name").unwrap(),
            "Lucas"
        );
        let steps: mlua::Table = rt.lua.globals().get("steps").unwrap();
        assert_eq!(steps.len().unwrap(), 3);
        assert_eq!(steps.get::<String>(1).unwrap(), "defer");
        assert_eq!(steps.get::<String>(3).unwrap(), "done");
        // The registry is the authority on whether the player may proceed.
        assert!(!rt
            .state
            .borrow()
            .borrow::<SharedDeferrals>()
            .0
            .is_pending(7));
        assert!(
            matches!(outcome.blocking_recv(), Ok(Ok(()))),
            "an accepted deferral must let the connection through"
        );
    }

    #[test]
    fn a_throwing_player_connecting_handler_never_strands_the_player() {
        // A handler that defers and then throws would otherwise park the
        // connection until the timeout — for every player, forever.
        let mut rt = runtime("gate");
        let outcome = rt.state.borrow().borrow::<SharedDeferrals>().0.register(9);
        rt.execute_script(
            "main.lua",
            r#"
            AddEventHandler("playerConnecting", function(name, _, deferrals)
                deferrals.defer()
                error("boom")
            end)
            "#,
        )
        .unwrap();
        rt.dispatch_player_connecting(9, "Lucas").unwrap();
        assert!(
            !rt.state
                .borrow()
                .borrow::<SharedDeferrals>()
                .0
                .is_pending(9),
            "the connection must be released, not left parked"
        );
        assert!(
            matches!(outcome.blocking_recv(), Ok(Err(_))),
            "the player is rejected with a reason, not silently accepted"
        );
    }

    #[test]
    fn exports_register_and_call_locally() {
        let mut rt = runtime("my-lib");
        rt.execute_script(
            "main.lua",
            r#"
            exports('add', function(a, b) return a + b end)
            -- Both spellings are idiomatic in the FiveM ecosystem.
            method_style = exports['my-lib']:add(2, 3)
            field_style = exports['my-lib'].add(10, 5)
            "#,
        )
        .unwrap();
        assert_eq!(rt.lua.globals().get::<i64>("method_style").unwrap(), 5);
        assert_eq!(rt.lua.globals().get::<i64>("field_style").unwrap(), 15);
        assert!(rt
            .state
            .borrow()
            .borrow::<RuntimeContext>()
            .exports
            .contains("add"));
    }

    #[test]
    fn a_cross_resource_export_fails_loudly() {
        // Returning nil would surface hundreds of lines later as "attempt to
        // index a nil value", pointing at the wrong resource.
        let mut rt = runtime("caller");
        let err = rt
            .execute_script("main.lua", r#"exports['other']:thing()"#)
            .expect_err("must not silently return nil");
        assert!(err.to_string().contains("unavailable"), "{err}");
    }

    #[test]
    fn state_bag_handlers_receive_their_changes() {
        let mut rt = runtime("bags");
        rt.execute_script(
            "main.lua",
            r#"
            got = {}
            AddStateBagChangeHandler(nil, nil, function(bag, key, value)
                got[#got + 1] = bag .. "/" .. key .. "=" .. tostring(value)
            end)
            "#,
        )
        .unwrap();

        rt.state.borrow().borrow::<SharedStateBags>().0.set(
            "player:7".to_owned(),
            "hunger".to_owned(),
            serde_json::json!(42),
            true,
            crate::StateBagSource::resource("bags".to_owned()),
        );
        rt.dispatch_state_bag_changes().unwrap();

        let got: mlua::Table = rt.lua.globals().get("got").unwrap();
        assert_eq!(got.get::<String>(1).unwrap(), "player:7/hunger=42");
    }

    #[test]
    fn zone_transfer_callbacks_merge_into_one_object() {
        let mut rt = runtime("mesh-test");
        // A resource that registered nothing has nothing to carry, and that is
        // not an error — it is the common case.
        assert!(rt.collect_zone_transfer_state(7).unwrap().is_none());

        rt.execute_script(
            "main.lua",
            r#"
            RegisterZoneTransferState(function(src) return { hunger = 42 } end)
            RegisterZoneTransferState(function(src) return { thirst = 7, src = src } end)
            "#,
        )
        .unwrap();
        let json = rt
            .collect_zone_transfer_state(7)
            .unwrap()
            .expect("registered callbacks produce state");
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["hunger"], 42);
        assert_eq!(value["thirst"], 7);
        assert_eq!(value["src"], 7);
    }

    #[test]
    fn a_fire_and_forget_client_native_reaches_the_net_bridge() {
        let (net, mut rx) = crate::net_bridge::NetBridge::new();
        let mut rt = LuaRuntime::new(
            "client-natives",
            Instant::now(),
            Arc::new(crate::deferrals::DeferralRegistry::new()),
            Arc::new(baston_protocol::PlayerDirectory::default()),
            net,
            Arc::new(Observability::new()),
        )
        .unwrap();

        // GET_PLAYER_PED, one argument — a hash the registry knows.
        rt.execute_script(
            "main.lua",
            r#"InvokeNativeOnClient(7, "0x43A66C31C68491C0", { 1 }, false)"#,
        )
        .unwrap();

        let outbound = rx.try_recv().expect("the call reaches the bridge");
        match outbound {
            crate::net_bridge::NetOutbound::ClientEvent { source, event, .. } => {
                assert_eq!(source, 7);
                assert_eq!(event, baston_protocol::native::INVOKE_NATIVE_EVENT);
            }
            other => panic!("unexpected outbound: {other:?}"),
        }
    }

    #[test]
    fn an_unknown_native_hash_is_refused_before_it_reaches_a_client() {
        let mut rt = runtime("client-natives");
        let err = rt
            .execute_script(
                "main.lua",
                r#"InvokeNativeOnClient(7, "0xDEADBEEF", {}, false)"#,
            )
            .expect_err("a bogus hash must not go on the wire");
        assert!(err.to_string().contains("InvokeNativeOnClient"), "{err}");
    }

    #[test]
    fn an_awaited_client_native_resumes_its_thread_with_the_result() {
        // The whole cooperative round trip: the thread yields, the reply lands
        // on a later tick, and the thread resumes where it left off.
        let (net, mut rx) = crate::net_bridge::NetBridge::new();
        let pending = Arc::clone(&net.pending_natives);
        let mut rt = LuaRuntime::new(
            "client-natives",
            Instant::now(),
            Arc::new(crate::deferrals::DeferralRegistry::new()),
            Arc::new(baston_protocol::PlayerDirectory::default()),
            net,
            Arc::new(Observability::new()),
        )
        .unwrap();

        rt.execute_script(
            "main.lua",
            r#"
            ped = nil
            CreateThread(function()
                ped = InvokeNativeOnClient(7, "0x43A66C31C68491C0", { 1 }, true)
            end)
            "#,
        )
        .unwrap();

        // First tick starts the thread, which dispatches and then yields.
        rt.tick();
        let call_id = match rx.try_recv().expect("the call reaches the bridge") {
            crate::net_bridge::NetOutbound::ClientEvent { args_json, .. } => {
                let parsed: serde_json::Value = serde_json::from_str(&args_json).unwrap();
                parsed[0]["id"].as_u64().expect("the call carries an id")
            }
            other => panic!("unexpected outbound: {other:?}"),
        };
        assert!(
            rt.lua.globals().get::<Value>("ped").unwrap() == Value::Nil,
            "the thread must still be waiting"
        );

        // The client answers.
        assert!(pending.resolve(call_id, serde_json::json!(4242)));
        rt.tick();
        assert_eq!(rt.lua.globals().get::<i64>("ped").unwrap(), 4242);
    }

    #[test]
    fn an_awaited_client_native_requires_a_coroutine() {
        // Outside a thread there is nothing to yield to, and blocking would
        // stop the very tick loop that delivers the reply. Say so instead.
        let mut rt = runtime("client-natives");
        let err = rt
            .execute_script(
                "main.lua",
                r#"InvokeNativeOnClient(7, "0x43A66C31C68491C0", { 1 }, true)"#,
            )
            .expect_err("must refuse outside a coroutine");
        assert!(
            err.to_string().contains("CreateThread"),
            "the error must name the fix: {err}"
        );
    }

    /// A stand-in pool: enough to prove the script-side contract without
    /// pulling a SQL driver into this crate's tests. The real drivers are
    /// exercised in baston-db's own suite.
    struct FakeDb {
        results:
            std::sync::Mutex<std::collections::HashMap<u64, Result<serde_json::Value, String>>>,
        next: std::sync::atomic::AtomicU64,
        seen: std::sync::Mutex<Vec<(String, String, Vec<serde_json::Value>)>>,
    }

    impl crate::native_state::DbAccess for FakeDb {
        fn submit(
            &self,
            _resource: &str,
            kind: &str,
            sql: String,
            params: Vec<serde_json::Value>,
        ) -> Result<u64, String> {
            if kind == "nonsense" {
                return Err("unknown query kind \"nonsense\"".to_owned());
            }
            self.seen
                .lock()
                .unwrap()
                .push((kind.to_owned(), sql, params));
            let id = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let answer = if kind == "scalar" {
                Ok(serde_json::json!("Lucas"))
            } else {
                Err("table not found".to_owned())
            };
            self.results.lock().unwrap().insert(id, answer);
            Ok(id)
        }

        fn collect(&self, id: u64) -> Option<Result<serde_json::Value, String>> {
            self.results.lock().unwrap().remove(&id)
        }

        fn query<'a>(
            &'a self,
            _resource: &'a str,
            _kind: &'a str,
            _sql: String,
            _params: Vec<serde_json::Value>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<serde_json::Value, String>> + Send + 'a>,
        > {
            Box::pin(async { Ok(serde_json::Value::Null) })
        }
    }

    fn runtime_with_db(name: &str) -> (LuaRuntime, Arc<FakeDb>) {
        let mut rt = runtime(name);
        let db = Arc::new(FakeDb {
            results: std::sync::Mutex::new(std::collections::HashMap::new()),
            next: std::sync::atomic::AtomicU64::new(1),
            seen: std::sync::Mutex::new(Vec::new()),
        });
        rt.install_db(SharedDb(Some(
            Arc::clone(&db) as Arc<dyn crate::native_state::DbAccess>
        )));
        (rt, db)
    }

    #[test]
    fn a_query_yields_its_thread_and_resumes_with_the_rows() {
        let (mut rt, db) = runtime_with_db("db-test");
        rt.execute_script(
            "main.lua",
            r#"
            name = nil
            CreateThread(function()
                name = Db.Scalar("SELECT name FROM players WHERE id = ?", { 1 })
            end)
            "#,
        )
        .unwrap();
        rt.tick();
        assert_eq!(rt.lua.globals().get::<String>("name").unwrap(), "Lucas");

        // Parameters travel as parameters, never spliced into the SQL.
        let seen = db.seen.lock().unwrap();
        assert_eq!(seen[0].0, "scalar");
        assert!(seen[0].1.contains('?'), "{}", seen[0].1);
        assert_eq!(seen[0].2, vec![serde_json::json!(1)]);
    }

    #[test]
    fn a_query_outside_a_thread_names_the_fix() {
        let (mut rt, _db) = runtime_with_db("db-test");
        let err = rt
            .execute_script("main.lua", r#"Db.Query("SELECT 1")"#)
            .expect_err("must refuse outside a coroutine");
        assert!(err.to_string().contains("CreateThread"), "{err}");
    }

    #[test]
    fn a_failing_query_raises_in_the_calling_thread() {
        let (mut rt, _db) = runtime_with_db("db-test");
        rt.execute_script(
            "main.lua",
            r#"
            failed = nil
            CreateThread(function()
                local ok, err = pcall(function() return Db.Query("SELECT 1") end)
                failed = (not ok) and tostring(err) or nil
            end)
            "#,
        )
        .unwrap();
        rt.tick();
        let failed = rt.lua.globals().get::<String>("failed").unwrap();
        assert!(failed.contains("table not found"), "{failed}");
    }

    #[test]
    fn querying_with_the_module_off_says_so() {
        // Without this the resource sees an empty result set and reads it as
        // "no rows", which is a much worse bug to chase.
        let mut rt = runtime("db-test");
        rt.execute_script(
            "main.lua",
            r#"
            failed = nil
            CreateThread(function()
                local ok, err = pcall(function() return Db.Query("SELECT 1") end)
                failed = (not ok) and tostring(err) or nil
            end)
            "#,
        )
        .unwrap();
        rt.tick();
        let failed = rt.lua.globals().get::<String>("failed").unwrap();
        assert!(failed.contains("[modules]"), "{failed}");
    }

    #[test]
    fn the_watchdog_terminates_a_runaway_script_and_the_runtime_survives() {
        let mut rt = runtime("runaway");
        // The production budget is ten seconds. What this test needs to prove
        // is that the hook fires and the state survives, so it shortens the
        // budget rather than waiting one out.
        rt.watchdog.budget.set(Duration::from_millis(50));
        let err = rt
            .execute_script("spin.lua", "while true do end")
            .expect_err("must be terminated");
        assert!(
            err.to_string().contains("dispatch budget"),
            "the error must say why: {err}"
        );

        // The whole point of interrupting rather than aborting: this runtime
        // keeps serving.
        rt.execute_script("after.lua", "survived = 1")
            .expect("the runtime survives its own watchdog");
        assert_eq!(rt.lua.globals().get::<i64>("survived").unwrap(), 1);
    }

    #[test]
    fn the_watchdog_does_not_fire_on_a_normal_dispatch() {
        let mut rt = runtime("calm");
        rt.execute_script(
            "main.lua",
            r#"
            total = 0
            AddEventHandler("work", function()
                for i = 1, 200000 do total = total + i end
            end)
            "#,
        )
        .unwrap();
        rt.dispatch_event(
            "work",
            "[]",
            None,
            crate::observability::DispatchKind::Event,
        )
        .expect("real work must not be mistaken for a runaway script");
        assert!(rt.lua.globals().get::<i64>("total").unwrap() > 0);
    }

    #[test]
    fn a_camel_case_global_resolves_to_its_native() {
        // FiveM exposes natives as globals; the prelude resolves unknown ones
        // on first use rather than generating thousands of stubs.
        let mut rt = runtime("globals-test");
        rt.execute_script("main.lua", r#"name = GetInvokingResource()"#)
            .unwrap();
        assert_eq!(
            rt.lua.globals().get::<String>("name").unwrap(),
            "globals-test"
        );
    }
}
