//! `ScriptRuntime` — one deno_core `JsRuntime` (V8 isolate) per resource.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use deno_core::{JsRuntime, PollEventLoopOptions, RuntimeOptions};

use baston_protocol::PlayerDirectory;

use crate::deferrals::DeferralRegistry;
use crate::error::ScriptError;
use crate::extensions::{all_extensions, RuntimeContext, SharedDeferrals, SharedPlayers};

const BOOTSTRAP_JS: &str = include_str!("../assets/bootstrap.js");

/// A single resource's V8 isolate with the BASTON extensions and bootstrap
/// polyfills loaded. `!Send` — lives on the script-host thread only.
pub struct ScriptRuntime {
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
    ) -> Result<Self, ScriptError> {
        let mut js = JsRuntime::new(RuntimeOptions {
            extensions: all_extensions(),
            ..Default::default()
        });

        {
            let op_state = js.op_state();
            let mut op_state = op_state.borrow_mut();
            op_state.put(RuntimeContext {
                resource_name: resource_name.to_owned(),
                host_started_at,
                queued_events: VecDeque::new(),
                handled_events: Default::default(),
                exports: Default::default(),
            });
            op_state.put(SharedDeferrals(deferrals));
            op_state.put(SharedPlayers(players));
        }

        js.execute_script("baston:bootstrap.js", BOOTSTRAP_JS)
            .map_err(|e| ScriptError::RuntimeInit {
                resource: resource_name.to_owned(),
                message: e.to_string(),
            })?;

        Ok(Self {
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
        let name = format!("baston:{}/{}", self.resource_name, script_path);
        // deno_core requires a 'static specifier; scripts load once per
        // resource start, so the small leak is bounded and acceptable.
        let name: &'static str = Box::leak(name.into_boxed_str());
        self.js
            .execute_script(name, code)
            .map_err(|e| ScriptError::Execute {
                resource: self.resource_name.clone(),
                script: script_path.to_owned(),
                message: e.to_string(),
            })?;
        self.pump().await
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
        self.js
            .execute_script("baston:dispatch", code)
            .map_err(|e| ScriptError::Execute {
                resource: self.resource_name.clone(),
                script: format!("dispatch:{event}"),
                message: e.to_string(),
            })?;
        self.pump().await
    }

    /// Flush the microtask queue / pending ops after a script ran.
    async fn pump(&mut self) -> Result<(), ScriptError> {
        self.js
            .run_event_loop(PollEventLoopOptions::default())
            .await
            .map_err(|e| ScriptError::Execute {
                resource: self.resource_name.clone(),
                script: "<event loop>".to_owned(),
                message: e.to_string(),
            })
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
