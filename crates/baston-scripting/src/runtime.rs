//! `ScriptRuntime` — one deno_core `JsRuntime` (V8 isolate) per resource.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use deno_core::{v8, JsRuntime, PollEventLoopOptions, RuntimeOptions};

use baston_protocol::PlayerDirectory;
use dashmap::DashMap;

use crate::deferrals::DeferralRegistry;
use crate::error::ScriptError;
use crate::extensions::{
    all_extensions, RuntimeContext, SharedConvars, SharedDeferrals, SharedNet, SharedObservability,
    SharedPlayers, SharedResources,
};
use crate::net_bridge::NetBridge;
use crate::observability::{DispatchKind, DispatchMeasurement, Observability, V8MemoryStats};
use crate::resource_registry::ResourceRegistry;

const BOOTSTRAP_JS: &str = include_str!("../assets/bootstrap.js");

/// deno_core's `execute_script` wants a `'static` script name. Interning names
/// process-wide means a resource reloaded N times leaks each distinct name at
/// most once (bounded by the file set) instead of leaking on every reload.
fn intern_script_name(name: String) -> &'static str {
    static INTERN: OnceLock<Mutex<HashMap<String, &'static str>>> = OnceLock::new();
    let map = INTERN.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = map.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(&interned) = map.get(&name) {
        return interned;
    }
    let leaked: &'static str = Box::leak(name.clone().into_boxed_str());
    map.insert(name, leaked);
    leaked
}

/// Wall-clock budget for a single dispatch (top-level load or event handler).
/// Generous on purpose: legitimate handlers finish in milliseconds, so this
/// only ever fires on genuinely runaway (uninterruptible) JS. It is the ceiling
/// before a stuck runtime is force-terminated, not a target.
const DISPATCH_BUDGET: Duration = Duration::from_secs(10);

/// Interrupts runaway JS. deno_core's `run_event_loop`/`execute_script` can't
/// be cancelled from the outside, so a background thread holds a V8
/// [`v8::IsolateHandle`] (thread-safe) and calls `terminate_execution` once a
/// dispatch overruns its budget. Without this, a single `while(true)` in a
/// resource would wedge that runtime — and, via the host command channel,
/// eventually every `send().await` into it.
struct Watchdog {
    /// Millis-since-host-start deadline; `0` means disarmed.
    deadline: Arc<AtomicU64>,
    /// Bumped on every arm so the watchdog can tell dispatches apart and avoid
    /// racing a disarm/rearm at the exact deadline boundary.
    generation: Arc<AtomicU64>,
    /// Set by the watchdog when it actually terminated a dispatch.
    fired: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    host_started_at: Instant,
    budget: Duration,
}

/// Disarms the watchdog when a dispatch returns (RAII, so early `?` returns are
/// covered too). Holds an owned `Arc`, not a borrow of the runtime, so it never
/// conflicts with the `&mut self.js` borrow during a dispatch.
struct WatchdogGuard {
    deadline: Arc<AtomicU64>,
}

impl Drop for WatchdogGuard {
    fn drop(&mut self) {
        self.deadline.store(0, Ordering::Release);
    }
}

impl Watchdog {
    fn new(handle: v8::IsolateHandle, host_started_at: Instant, budget: Duration) -> Self {
        let deadline = Arc::new(AtomicU64::new(0));
        let generation = Arc::new(AtomicU64::new(0));
        let fired = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (deadline, generation, fired, stop) = (
                deadline.clone(),
                generation.clone(),
                fired.clone(),
                stop.clone(),
            );
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(50));
                    let dl = deadline.load(Ordering::Acquire);
                    if dl == 0 {
                        continue;
                    }
                    let now = host_started_at.elapsed().as_millis() as u64;
                    if now < dl {
                        continue;
                    }
                    // Armed and past deadline. Re-check on the same generation
                    // after a short pause so we don't terminate a dispatch that
                    // just finished (disarmed) or a fresh one (rearmed).
                    let gen = generation.load(Ordering::Acquire);
                    std::thread::sleep(Duration::from_millis(20));
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    if deadline.load(Ordering::Acquire) != 0
                        && generation.load(Ordering::Acquire) == gen
                    {
                        handle.terminate_execution();
                        fired.store(true, Ordering::Release);
                        // Don't spin re-terminating; wait for the dispatch to
                        // unwind and disarm.
                        while deadline.load(Ordering::Acquire) != 0 && !stop.load(Ordering::Relaxed)
                        {
                            std::thread::sleep(Duration::from_millis(20));
                        }
                    }
                }
            })
        };
        Self {
            deadline,
            generation,
            fired,
            stop,
            thread: Some(thread),
            host_started_at,
            budget,
        }
    }

    /// Arm for one dispatch; the returned guard disarms on drop.
    fn arm(&self) -> WatchdogGuard {
        self.fired.store(false, Ordering::Release);
        self.generation.fetch_add(1, Ordering::AcqRel);
        let dl = (self.host_started_at.elapsed() + self.budget).as_millis() as u64;
        self.deadline.store(dl.max(1), Ordering::Release);
        WatchdogGuard {
            deadline: self.deadline.clone(),
        }
    }

    /// Whether the watchdog terminated the dispatch that just ran.
    fn took_fired(&self) -> bool {
        self.fired.swap(false, Ordering::AcqRel)
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Completion state of a dispatch's JS promise.
#[derive(Debug)]
pub enum PromiseOutcome {
    Pending,
    Fulfilled,
    Rejected(String),
}

/// Measurement context carried from `start_*` to `finish_dispatch`.
#[derive(Debug)]
pub struct DispatchMeta {
    kind: DispatchKind,
    event_name: String,
    source: Option<u32>,
    execute_us: u64,
    total_started: Instant,
    watchdog_fired: bool,
}

/// A started dispatch: the synchronous execute phase already ran; `started`
/// carries its result and, when async handlers are still running, the JS
/// completion promise to poll.
#[derive(Debug)]
pub struct DispatchTicket {
    pub started: Result<Option<v8::Global<v8::Value>>, ScriptError>,
    pub meta: DispatchMeta,
}

/// A single resource's V8 isolate with the BASTON extensions and bootstrap
/// polyfills loaded. `!Send` — lives on the script-host thread only.
pub struct ScriptRuntime {
    // Declared before `js` so its `Drop` stops the watchdog thread *before* the
    // isolate it references is freed.
    watchdog: Watchdog,
    js: JsRuntime,
    resource_name: String,
    observability: Arc<Observability>,
}

impl ScriptRuntime {
    /// Create an isolate for `resource_name`, register extensions, seed
    /// `OpState`, and run `bootstrap.js`.
    pub fn new(
        resource_name: &str,
        host_started_at: Instant,
        deferrals: Arc<DeferralRegistry>,
        players: Arc<PlayerDirectory>,
        net: NetBridge,
        observability: Arc<Observability>,
    ) -> Result<Self, ScriptError> {
        Self::new_with_budget(
            resource_name,
            host_started_at,
            deferrals,
            players,
            net,
            observability,
            DISPATCH_BUDGET,
        )
    }

    fn new_with_budget(
        resource_name: &str,
        host_started_at: Instant,
        deferrals: Arc<DeferralRegistry>,
        players: Arc<PlayerDirectory>,
        net: NetBridge,
        observability: Arc<Observability>,
        budget: Duration,
    ) -> Result<Self, ScriptError> {
        let mut js = JsRuntime::new(RuntimeOptions {
            extensions: all_extensions(),
            ..Default::default()
        });
        // Thread-safe handle for the watchdog. Grabbed before any user script
        // runs so a runaway load is already covered.
        let isolate_handle = js.v8_isolate().thread_safe_handle();

        {
            let op_state = js.op_state();
            let mut op_state = op_state.borrow_mut();
            op_state.put(RuntimeContext {
                resource_name: resource_name.to_owned(),
                host_started_at,
                queued_events: VecDeque::new(),
                handled_events: Default::default(),
                exports: Default::default(),
                commands: Default::default(),
                has_zone_transfer_state: false,
                collected_transfer_state: None,
                handler_errors: 0,
            });
            op_state.put(SharedDeferrals(deferrals));
            op_state.put(SharedPlayers(players));
            op_state.put(SharedNet(net));
            op_state.put(SharedObservability(observability.clone()));
        }

        js.execute_script("baston:bootstrap.js", BOOTSTRAP_JS)
            .map_err(|e| ScriptError::RuntimeInit {
                resource: resource_name.to_owned(),
                message: e.to_string(),
            })?;

        Ok(Self {
            watchdog: Watchdog::new(isolate_handle, host_started_at, budget),
            js,
            resource_name: resource_name.to_owned(),
            observability,
        })
    }

    pub fn resource_name(&self) -> &str {
        &self.resource_name
    }

    pub fn install_server_state(
        &mut self,
        convars: Arc<DashMap<String, String>>,
        resources: ResourceRegistry,
    ) {
        let op_state = self.js.op_state();
        let mut op_state = op_state.borrow_mut();
        op_state.put(SharedConvars(convars));
        op_state.put(SharedResources(resources));
    }

    /// Install the voice control surface backing the `MUMBLE_*` natives.
    pub fn install_voice(&mut self, voice: crate::extensions::SharedVoice) {
        let op_state = self.js.op_state();
        op_state.borrow_mut().put(voice);
    }

    /// Execute a resource script (plain script semantics, like FXServer).
    pub async fn execute_resource_script(
        &mut self,
        script_path: &str,
        code: String,
    ) -> Result<(), ScriptError> {
        // deno_core requires a 'static specifier; intern so reloading a
        // resource doesn't leak a fresh copy of the name each time.
        let name = intern_script_name(format!("baston:{}/{}", self.resource_name, script_path));
        self.run_guarded(
            name,
            code,
            script_path.to_owned(),
            DispatchKind::LoadScript,
            script_path.to_owned(),
            None,
        )
        .await
    }

    /// Dispatch an event into this isolate's JS handler registry.
    pub async fn dispatch_event(
        &mut self,
        event: &str,
        args_json: &str,
    ) -> Result<(), ScriptError> {
        let code = format!(
            "globalThis.__baston.dispatch({}, {});",
            serde_json::to_string(event).unwrap_or_else(|_| "\"\"".into()),
            serde_json::to_string(args_json).unwrap_or_else(|_| "\"[]\"".into()),
        );
        self.run_dispatch(event, code, DispatchKind::Event, None)
            .await
    }

    /// Dispatch a net event (from a client) with `globalThis.source` bound.
    pub async fn dispatch_net_event(
        &mut self,
        event: &str,
        source: u32,
        args_json: &str,
    ) -> Result<(), ScriptError> {
        let code = format!(
            "globalThis.__baston.dispatchWithSource({}, {}, {});",
            serde_json::to_string(event).unwrap_or_else(|_| "\"\"".into()),
            source,
            serde_json::to_string(args_json).unwrap_or_else(|_| "\"[]\"".into()),
        );
        self.run_dispatch(event, code, DispatchKind::NetEvent, Some(source))
            .await
    }

    /// Dispatch `playerConnecting` with the (name, setKickReason, deferrals)
    /// argument triple bound to `source`.
    pub async fn dispatch_player_connecting(
        &mut self,
        source: u32,
        player_name: &str,
    ) -> Result<(), ScriptError> {
        let code = format!(
            "globalThis.__baston.dispatchPlayerConnecting({}, {});",
            source,
            serde_json::to_string(player_name).unwrap_or_else(|_| "\"\"".into()),
        );
        self.run_dispatch(
            "playerConnecting",
            code,
            DispatchKind::PlayerConnecting,
            Some(source),
        )
        .await
    }

    /// Dispatch a server command into this isolate's `RegisterCommand` registry.
    pub async fn dispatch_command(
        &mut self,
        command: &str,
        source: u32,
        args: &[String],
        raw: &str,
    ) -> Result<(), ScriptError> {
        if !self.has_command(command) {
            return Ok(());
        }
        let args_json = serde_json::to_string(args).unwrap_or_else(|_| "[]".into());
        let code = format!(
            "globalThis.__baston.dispatchCommand({}, {}, {}, {});",
            serde_json::to_string(command).unwrap_or_else(|_| "\"\"".into()),
            source,
            serde_json::to_string(&args_json).unwrap_or_else(|_| "\"[]\"".into()),
            serde_json::to_string(raw).unwrap_or_else(|_| "\"\"".into()),
        );
        self.run_dispatch(command, code, DispatchKind::Command, Some(source))
            .await
    }

    // --- concurrent dispatch API (host driver) -----------------------------
    //
    // The host no longer awaits each dispatch to completion (audit ROB-2):
    // `start_*` synchronously executes the dispatch script — which returns a
    // Promise when async handlers are still running — and the host resolves
    // the ticket from a `spawn_local` task while a single pump task drives
    // the isolate's event loop. All methods here are synchronous so the
    // `RefCell<ScriptRuntime>` borrow is never held across an await.

    /// Start an event dispatch; complete it via [`ScriptRuntime::promise_outcome`]
    /// + [`ScriptRuntime::finish_dispatch`].
    pub fn start_event_dispatch(&mut self, event: &str, args_json: &str) -> DispatchTicket {
        let code = format!(
            "globalThis.__baston.dispatch({}, {});",
            serde_json::to_string(event).unwrap_or_else(|_| "\"\"".into()),
            serde_json::to_string(args_json).unwrap_or_else(|_| "\"[]\"".into()),
        );
        self.start_dispatch_raw(code, event, DispatchKind::Event, None)
    }

    /// Start a net-event dispatch with `globalThis.source` bound.
    pub fn start_net_event_dispatch(
        &mut self,
        event: &str,
        source: u32,
        args_json: &str,
    ) -> DispatchTicket {
        let code = format!(
            "globalThis.__baston.dispatchWithSource({}, {}, {});",
            serde_json::to_string(event).unwrap_or_else(|_| "\"\"".into()),
            source,
            serde_json::to_string(args_json).unwrap_or_else(|_| "\"[]\"".into()),
        );
        self.start_dispatch_raw(code, event, DispatchKind::NetEvent, Some(source))
    }

    /// Start a `playerConnecting` dispatch.
    pub fn start_player_connecting_dispatch(
        &mut self,
        source: u32,
        player_name: &str,
    ) -> DispatchTicket {
        let code = format!(
            "globalThis.__baston.dispatchPlayerConnecting({}, {});",
            source,
            serde_json::to_string(player_name).unwrap_or_else(|_| "\"\"".into()),
        );
        self.start_dispatch_raw(
            code,
            "playerConnecting",
            DispatchKind::PlayerConnecting,
            Some(source),
        )
    }

    /// Start a command dispatch. `None` when this runtime never registered the
    /// command (nothing to run).
    pub fn start_command_dispatch(
        &mut self,
        command: &str,
        source: u32,
        args: &[String],
        raw: &str,
    ) -> Option<DispatchTicket> {
        if !self.has_command(command) {
            return None;
        }
        let args_json = serde_json::to_string(args).unwrap_or_else(|_| "[]".into());
        let code = format!(
            "globalThis.__baston.dispatchCommand({}, {}, {}, {});",
            serde_json::to_string(command).unwrap_or_else(|_| "\"\"".into()),
            source,
            serde_json::to_string(&args_json).unwrap_or_else(|_| "\"[]\"".into()),
            serde_json::to_string(raw).unwrap_or_else(|_| "\"\"".into()),
        );
        Some(self.start_dispatch_raw(code, command, DispatchKind::Command, Some(source)))
    }

    /// Start a resource script load (plain script semantics). The host waits
    /// for event-loop idle afterwards to preserve load ordering.
    pub fn start_script_load(&mut self, script_path: &str, code: String) -> DispatchTicket {
        let name = intern_script_name(format!("baston:{}/{}", self.resource_name, script_path));
        self.start_named(
            name,
            code,
            script_path.to_owned(),
            DispatchKind::LoadScript,
            script_path.to_owned(),
            None,
        )
    }

    fn start_dispatch_raw(
        &mut self,
        code: String,
        event: &str,
        kind: DispatchKind,
        source: Option<u32>,
    ) -> DispatchTicket {
        self.start_named(
            "baston:dispatch",
            code,
            format!("dispatch:{event}"),
            kind,
            event.to_owned(),
            source,
        )
    }

    fn start_named(
        &mut self,
        name: &'static str,
        code: String,
        ctx: String,
        kind: DispatchKind,
        event_name: String,
        source: Option<u32>,
    ) -> DispatchTicket {
        let total_started = Instant::now();
        let guard = self.watchdog.arm();
        let exec = self.js.execute_script(name, code);
        drop(guard);
        let execute_us = total_started.elapsed().as_micros() as u64;
        let watchdog_fired = self.watchdog.took_fired();
        if watchdog_fired {
            tracing::warn!(
                target: "scripting",
                resource = %self.resource_name,
                ctx = %ctx,
                "watchdog force-terminated a runaway dispatch after {:?}; runtime kept alive",
                self.watchdog.budget,
            );
            self.js.v8_isolate().cancel_terminate_execution();
        }
        let started = match exec {
            Ok(global) => {
                let is_promise = {
                    deno_core::scope!(scope, self.js);
                    let local = v8::Local::new(scope, &global);
                    local.is_promise()
                };
                if is_promise {
                    Ok(Some(global))
                } else {
                    Ok(None)
                }
            }
            Err(e) => Err(ScriptError::Execute {
                resource: self.resource_name.clone(),
                script: ctx.clone(),
                message: e.to_string(),
            }),
        };
        DispatchTicket {
            started,
            meta: DispatchMeta {
                kind,
                event_name,
                source,
                execute_us,
                total_started,
                watchdog_fired,
            },
        }
    }

    /// One pass of the isolate's event loop, watchdog-protected. Exactly one
    /// task (the host pump) may call this.
    pub fn poll_event_loop_pass(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), ScriptError>> {
        let guard = self.watchdog.arm();
        let poll = self.js.poll_event_loop(cx, PollEventLoopOptions::default());
        drop(guard);
        if self.watchdog.took_fired() {
            tracing::warn!(
                target: "scripting",
                resource = %self.resource_name,
                "watchdog force-terminated a runaway event-loop pass after {:?}; runtime kept alive",
                self.watchdog.budget,
            );
            self.js.v8_isolate().cancel_terminate_execution();
        }
        poll.map(|r| {
            r.map_err(|e| ScriptError::Execute {
                resource: self.resource_name.clone(),
                script: "<event loop>".to_owned(),
                message: e.to_string(),
            })
        })
    }

    /// Current state of a dispatch's completion promise.
    pub fn promise_outcome(&mut self, promise: &v8::Global<v8::Value>) -> PromiseOutcome {
        deno_core::scope!(scope, self.js);
        let local = v8::Local::new(scope, promise);
        let Ok(promise) = v8::Local::<v8::Promise>::try_from(local) else {
            return PromiseOutcome::Fulfilled;
        };
        match promise.state() {
            v8::PromiseState::Pending => PromiseOutcome::Pending,
            v8::PromiseState::Fulfilled => PromiseOutcome::Fulfilled,
            v8::PromiseState::Rejected => {
                let msg = promise.result(scope).to_rust_string_lossy(scope);
                PromiseOutcome::Rejected(msg)
            }
        }
    }

    /// Record the dispatch measurement once its ticket settled. `error` is the
    /// dispatch-level failure (execute throw or rejected completion promise).
    pub fn finish_dispatch(&mut self, meta: &DispatchMeta, error: Option<&str>) {
        let handler_errors = {
            let op_state = self.js.op_state();
            let mut op_state = op_state.borrow_mut();
            let ctx = op_state.borrow_mut::<RuntimeContext>();
            let errors = ctx.handler_errors;
            ctx.handler_errors = 0;
            errors
        };
        let total_us = meta.total_started.elapsed().as_micros() as u64;
        let memory = Some(self.v8_memory_stats());
        self.observability.record_dispatch(DispatchMeasurement {
            resource: self.resource_name.clone(),
            kind: meta.kind,
            name: meta.event_name.clone(),
            execute_us: meta.execute_us,
            event_loop_us: total_us.saturating_sub(meta.execute_us),
            total_us,
            errored: error.is_some() || meta.watchdog_fired || handler_errors > 0,
            watchdog_fired: meta.watchdog_fired,
            memory,
            source: meta.source,
            zone: None,
        });
    }

    /// Synchronous variant of [`ScriptRuntime::collect_zone_transfer_state`]
    /// for the concurrent host driver: the JS collector is fully synchronous,
    /// so the state is readable right after the execute phase.
    pub fn collect_zone_transfer_state_sync(
        &mut self,
        source: u32,
    ) -> Result<Option<String>, ScriptError> {
        {
            let op_state = self.js.op_state();
            let has = op_state
                .borrow()
                .borrow::<RuntimeContext>()
                .has_zone_transfer_state;
            if !has {
                return Ok(None);
            }
            op_state
                .borrow_mut()
                .borrow_mut::<RuntimeContext>()
                .collected_transfer_state = None;
        }
        let code = format!("globalThis.__baston.collectZoneTransferState({source});");
        let ticket = self.start_dispatch_raw(
            code,
            "collectZoneTransferState",
            DispatchKind::ZoneTransferState,
            Some(source),
        );
        let result = match &ticket.started {
            Ok(_) => None,
            Err(e) => Some(e.to_string()),
        };
        self.finish_dispatch(&ticket.meta, result.as_deref());
        ticket.started?;
        let op_state = self.js.op_state();
        let mut op_state = op_state.borrow_mut();
        Ok(op_state
            .borrow_mut::<RuntimeContext>()
            .collected_transfer_state
            .take())
    }

    async fn run_dispatch(
        &mut self,
        event: &str,
        code: String,
        kind: DispatchKind,
        source: Option<u32>,
    ) -> Result<(), ScriptError> {
        self.run_guarded(
            "baston:dispatch",
            code,
            format!("dispatch:{event}"),
            kind,
            event.to_owned(),
            source,
        )
        .await
    }

    /// Run one script under the watchdog, then flush the event loop. Shared by
    /// resource loads and event dispatch so both are protected from runaway
    /// (uninterruptible) JS. `ctx` labels the execute-phase error's `script`.
    async fn run_guarded(
        &mut self,
        name: &'static str,
        code: String,
        ctx: String,
        kind: DispatchKind,
        event_name: String,
        source: Option<u32>,
    ) -> Result<(), ScriptError> {
        let guard = self.watchdog.arm();
        let total_started = Instant::now();
        let execute_started = Instant::now();
        let exec = self.js.execute_script(name, code);
        let execute_us = execute_started.elapsed().as_micros() as u64;
        let mut event_loop_us = 0;
        let loop_res = if exec.is_ok() {
            let loop_started = Instant::now();
            let result = self
                .js
                .run_event_loop(PollEventLoopOptions::default())
                .await;
            event_loop_us = loop_started.elapsed().as_micros() as u64;
            Some(result)
        } else {
            None
        };
        drop(guard);
        let watchdog_fired = self.watchdog.took_fired();
        if watchdog_fired {
            tracing::warn!(
                target: "scripting",
                resource = %self.resource_name,
                ctx = %ctx,
                "watchdog force-terminated a runaway dispatch after {:?}; runtime kept alive",
                self.watchdog.budget,
            );
            // Clear the terminate flag so the interrupted isolate can serve
            // future dispatches instead of throwing on every later script.
            self.js.v8_isolate().cancel_terminate_execution();
        }
        let loop_errored = loop_res.as_ref().is_some_and(Result::is_err);
        let handler_errors = {
            let op_state = self.js.op_state();
            let mut op_state = op_state.borrow_mut();
            let ctx = op_state.borrow_mut::<RuntimeContext>();
            let errors = ctx.handler_errors;
            ctx.handler_errors = 0;
            errors
        };
        let memory = Some(self.v8_memory_stats());
        self.observability.record_dispatch(DispatchMeasurement {
            resource: self.resource_name.clone(),
            kind,
            name: event_name,
            execute_us,
            event_loop_us,
            total_us: total_started.elapsed().as_micros() as u64,
            errored: exec.is_err() || loop_errored || watchdog_fired || handler_errors > 0,
            watchdog_fired,
            memory,
            source,
            zone: None,
        });
        exec.map_err(|e| ScriptError::Execute {
            resource: self.resource_name.clone(),
            script: ctx,
            message: e.to_string(),
        })?;
        if let Some(loop_res) = loop_res {
            loop_res.map_err(|e| ScriptError::Execute {
                resource: self.resource_name.clone(),
                script: "<event loop>".to_owned(),
                message: e.to_string(),
            })
        } else {
            Ok(())
        }
    }

    fn v8_memory_stats(&mut self) -> V8MemoryStats {
        let stats = self.js.v8_isolate().get_heap_statistics();
        V8MemoryStats {
            used_bytes: stats.used_heap_size() as u64,
            total_bytes: stats.total_heap_size() as u64,
            external_bytes: stats.external_memory() as u64,
        }
    }

    /// Run the resource's `RegisterZoneTransferState` callbacks and return
    /// the merged JSON object (None if the resource registered none).
    pub async fn collect_zone_transfer_state(
        &mut self,
        source: u32,
    ) -> Result<Option<String>, ScriptError> {
        {
            let op_state = self.js.op_state();
            let has = op_state
                .borrow()
                .borrow::<RuntimeContext>()
                .has_zone_transfer_state;
            if !has {
                return Ok(None);
            }
            op_state
                .borrow_mut()
                .borrow_mut::<RuntimeContext>()
                .collected_transfer_state = None;
        }
        let code = format!("globalThis.__baston.collectZoneTransferState({source});");
        self.run_dispatch(
            "collectZoneTransferState",
            code,
            DispatchKind::ZoneTransferState,
            Some(source),
        )
        .await?;
        let op_state = self.js.op_state();
        let mut op_state = op_state.borrow_mut();
        Ok(op_state
            .borrow_mut::<RuntimeContext>()
            .collected_transfer_state
            .take())
    }

    /// Drain events queued by `TriggerEvent` during the last execution.
    pub fn drain_queued_events(&mut self) -> Vec<(String, String)> {
        let op_state = self.js.op_state();
        let mut op_state = op_state.borrow_mut();
        let ctx = op_state.borrow_mut::<RuntimeContext>();
        ctx.queued_events.drain(..).collect()
    }

    /// Whether this runtime registered at least one handler for `event`.
    pub fn has_handler(&mut self, event: &str) -> bool {
        let op_state = self.js.op_state();
        let op_state = op_state.borrow();
        op_state
            .borrow::<RuntimeContext>()
            .handled_events
            .contains(event)
    }

    pub fn has_command(&mut self, command: &str) -> bool {
        let op_state = self.js.op_state();
        let op_state = op_state.borrow();
        op_state
            .borrow::<RuntimeContext>()
            .commands
            .contains_key(command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deferrals::DeferralRegistry;
    use baston_protocol::PlayerDirectory;

    fn test_runtime(budget: Duration) -> ScriptRuntime {
        let (net, _rx) = NetBridge::new();
        ScriptRuntime::new_with_budget(
            "watchdog-test",
            Instant::now(),
            Arc::new(DeferralRegistry::new()),
            Arc::new(PlayerDirectory::new()),
            net,
            Observability::shared(),
            budget,
        )
        .expect("runtime builds")
    }

    #[tokio::test]
    async fn watchdog_terminates_runaway_script_and_runtime_survives() {
        let mut rt = test_runtime(Duration::from_millis(300));

        // Without the watchdog this synchronous loop would hang forever.
        let started = Instant::now();
        let result = rt
            .execute_resource_script("runaway.js", "while (true) {}".to_owned())
            .await;
        let elapsed = started.elapsed();

        assert!(result.is_err(), "runaway script must be terminated");
        assert!(
            elapsed < Duration::from_secs(5),
            "watchdog should fire near the 300ms budget, took {elapsed:?}",
        );

        // The isolate must still serve a normal dispatch after the kill.
        rt.execute_resource_script("ok.js", "globalThis.__ok = 1 + 1;".to_owned())
            .await
            .expect("runtime must survive a watchdog kill");
    }
}
