//! `ScriptRuntime` — one deno_core `JsRuntime` (V8 isolate) per resource.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use deno_core::{v8, JsRuntime, PollEventLoopOptions, RuntimeOptions};

use baston_protocol::PlayerDirectory;

use crate::deferrals::DeferralRegistry;
use crate::error::ScriptError;
use crate::extensions::{
    all_extensions, RuntimeContext, SharedDeferrals, SharedNet, SharedPlayers,
};
use crate::net_bridge::NetBridge;

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

/// A single resource's V8 isolate with the BASTON extensions and bootstrap
/// polyfills loaded. `!Send` — lives on the script-host thread only.
pub struct ScriptRuntime {
    // Declared before `js` so its `Drop` stops the watchdog thread *before* the
    // isolate it references is freed.
    watchdog: Watchdog,
    js: JsRuntime,
    resource_name: String,
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
    ) -> Result<Self, ScriptError> {
        Self::new_with_budget(
            resource_name,
            host_started_at,
            deferrals,
            players,
            net,
            DISPATCH_BUDGET,
        )
    }

    fn new_with_budget(
        resource_name: &str,
        host_started_at: Instant,
        deferrals: Arc<DeferralRegistry>,
        players: Arc<PlayerDirectory>,
        net: NetBridge,
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
                has_zone_transfer_state: false,
                collected_transfer_state: None,
            });
            op_state.put(SharedDeferrals(deferrals));
            op_state.put(SharedPlayers(players));
            op_state.put(SharedNet(net));
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
        })
    }

    pub fn resource_name(&self) -> &str {
        &self.resource_name
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
        self.run_guarded(name, code, script_path.to_owned()).await
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
        self.run_dispatch(event, code).await
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
        self.run_dispatch(event, code).await
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
        self.run_dispatch("playerConnecting", code).await
    }

    async fn run_dispatch(&mut self, event: &str, code: String) -> Result<(), ScriptError> {
        self.run_guarded("baston:dispatch", code, format!("dispatch:{event}"))
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
    ) -> Result<(), ScriptError> {
        let guard = self.watchdog.arm();
        let exec = self.js.execute_script(name, code);
        let loop_res = if exec.is_ok() {
            Some(
                self.js
                    .run_event_loop(PollEventLoopOptions::default())
                    .await,
            )
        } else {
            None
        };
        drop(guard);
        if self.watchdog.took_fired() {
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
        exec.map_err(|e| ScriptError::Execute {
            resource: self.resource_name.clone(),
            script: ctx,
            message: e.to_string(),
        })?;
        loop_res
            .expect("event loop runs whenever execute_script succeeded")
            .map_err(|e| ScriptError::Execute {
                resource: self.resource_name.clone(),
                script: "<event loop>".to_owned(),
                message: e.to_string(),
            })
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
        self.run_dispatch("collectZoneTransferState", code).await?;
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
