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
use std::time::Instant;

use mlua::{Lua, LuaSerdeExt, Value, Variadic};

use crate::error::ScriptError;
use crate::native_state::{
    NativeState, RuntimeContext, SharedConvars, SharedDeferrals, SharedEntityWorld, SharedHttp,
    SharedHttpHandlers, SharedKvp, SharedNet, SharedObservability, SharedPlayers,
    SharedResourceControl, SharedResources, SharedRouting, SharedStateBags, SharedVoice,
    SharedWorldControl,
};
use crate::natives::{console, server};
use crate::observability::Observability;

const PRELUDE_LUA: &str = include_str!("../assets/prelude.lua");

/// One resource's Lua state plus the native services behind it.
pub struct LuaRuntime {
    lua: Lua,
    /// Shared with every host function bound into Lua. `RefCell` rather than a
    /// lock: the state is thread-confined, and a native that re-enters Lua
    /// would be a bug we want to hear about as a borrow panic, not a deadlock.
    state: Rc<RefCell<NativeState>>,
    resource_name: String,
    observability: Arc<Observability>,
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

        let runtime = Self {
            lua: Lua::new(),
            state: Rc::new(RefCell::new(native_state)),
            resource_name: resource_name.to_owned(),
            observability,
        };
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

    /// Run one resource script.
    pub fn execute_script(&mut self, path: &str, code: &str) -> Result<(), ScriptError> {
        let started = Instant::now();
        let result = self
            .lua
            .load(code)
            .set_name(format!("@{path}"))
            .exec()
            .map_err(|e| ScriptError::Execute {
                resource: self.resource_name.clone(),
                script: path.to_owned(),
                message: e.to_string(),
            });
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
                // No watchdog on the Lua path yet — a runaway Lua script blocks
                // its own runtime and only its own. See docs/modules.md.
                watchdog_fired: false,
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
        let errors: u32 = dispatch
            .get::<mlua::Function>("event")
            .and_then(|f| f.call((event, args_json, source)))
            .map_err(|e| ScriptError::Execute {
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
                errored: errors > 0,
                watchdog_fired: false,
                memory: None,
                source,
                zone: None,
            });
        Ok(())
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
        dispatch
            .get::<mlua::Function>("command")
            .and_then(|f| f.call((command, source, args_json, raw)))
            .map_err(|e| ScriptError::Execute {
                resource: self.resource_name.clone(),
                script: command.to_owned(),
                message: e.to_string(),
            })
    }

    /// Resume coroutines and fire timers; returns how long the runtime would
    /// like to sleep before the next tick.
    pub fn tick(&mut self) -> std::time::Duration {
        let ms = self
            .dispatch_table()
            .and_then(|t| {
                t.get::<mlua::Function>("tick")
                    .map_err(|e| self.init_error(&e))
            })
            .and_then(|f| f.call::<i64>(()).map_err(|e| self.init_error(&e)))
            .unwrap_or(50);
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
