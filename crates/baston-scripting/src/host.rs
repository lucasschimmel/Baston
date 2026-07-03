//! `ScriptHost` — orchestrates one `ScriptRuntime` per resource.
//!
//! `JsRuntime` is `!Send`, and V8 149 panics when two isolates share a thread
//! (isolate-entry TLS), so every resource runtime gets its own dedicated OS
//! thread with a current-thread tokio runtime. The host holds a `Send` handle
//! per resource and orchestrates broadcasts from any async context.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use baston_protocol::PlayerDirectory;
use tokio::sync::{mpsc, oneshot, RwLock};

use crate::deferrals::DeferralRegistry;
use crate::error::ScriptError;
use crate::runtime::ScriptRuntime;

/// A script file to load into a resource's isolate.
#[derive(Debug)]
pub struct ScriptSource {
    /// Path relative to the resource dir (for error messages).
    pub path: String,
    pub code: String,
}

/// Events queued by JS `TriggerEvent` during a dispatch, to re-broadcast.
type QueuedEvents = Vec<(String, String)>;

enum RuntimeCommand {
    ExecuteScripts {
        scripts: Vec<ScriptSource>,
        reply: oneshot::Sender<Result<QueuedEvents, ScriptError>>,
    },
    DispatchEvent {
        event: String,
        args_json: String,
        reply: oneshot::Sender<Result<QueuedEvents, ScriptError>>,
    },
    DispatchPlayerConnecting {
        source: u32,
        player_name: String,
        reply: oneshot::Sender<Result<QueuedEvents, ScriptError>>,
    },
    DispatchNetEvent {
        event: String,
        source: u32,
        args_json: String,
        reply: oneshot::Sender<Result<QueuedEvents, ScriptError>>,
    },
    CollectTransferState {
        source: u32,
        reply: oneshot::Sender<Result<Option<String>, ScriptError>>,
    },
}

/// `Send` handle to one resource's isolate thread. Dropping it shuts the
/// thread down (channel closes, loop exits, isolate is destroyed).
struct ResourceRuntimeHandle {
    tx: mpsc::Sender<RuntimeCommand>,
}

impl ResourceRuntimeHandle {
    async fn send(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<QueuedEvents, ScriptError>>) -> RuntimeCommand,
    ) -> Result<QueuedEvents, ScriptError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(make(reply))
            .await
            .map_err(|_| ScriptError::HostGone)?;
        rx.await.map_err(|_| ScriptError::HostGone)?
    }
}

/// Publishes locally-triggered events to sibling zones (Phase D). Receives
/// `(event, args_json)`.
pub type CrossZonePublisher = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// Cloneable orchestrator for all resource runtimes.
#[derive(Clone)]
pub struct ScriptHost {
    runtimes: Arc<RwLock<HashMap<String, ResourceRuntimeHandle>>>,
    deferrals: Arc<DeferralRegistry>,
    players: Arc<PlayerDirectory>,
    net: crate::net_bridge::NetBridge,
    started_at: Instant,
    cross_zone: Arc<std::sync::RwLock<Option<CrossZonePublisher>>>,
}

/// Lifecycle/internal events that never leave the local zone.
fn is_zone_local_event(event: &str) -> bool {
    event.starts_with("onResource")
        || event.starts_with("player")
        || event.starts_with("__baston")
        || event == "onEntityOwnerChanged"
}

/// Cap on chained event re-broadcasts per dispatch, so a pair of handlers
/// triggering each other cannot wedge the host.
const MAX_EVENT_CHAIN: usize = 64;

impl ScriptHost {
    /// Create the host. `deferrals` and `players` are shared with the
    /// gateway. A default net bridge is created; the gateway takes its
    /// receiving end via [`ScriptHost::spawn_with_net`].
    pub fn spawn(
        deferrals: Arc<DeferralRegistry>,
        players: Arc<PlayerDirectory>,
    ) -> Result<Self, ScriptError> {
        let (net, _rx) = crate::net_bridge::NetBridge::new();
        Self::spawn_with_net(deferrals, players, net)
    }

    /// Create the host with an externally-owned net bridge.
    pub fn spawn_with_net(
        deferrals: Arc<DeferralRegistry>,
        players: Arc<PlayerDirectory>,
        net: crate::net_bridge::NetBridge,
    ) -> Result<Self, ScriptError> {
        Ok(Self {
            runtimes: Arc::new(RwLock::new(HashMap::new())),
            deferrals,
            players,
            net,
            started_at: Instant::now(),
            cross_zone: Arc::new(std::sync::RwLock::new(None)),
        })
    }

    /// Install the Phase D cross-zone event publisher (zone processes only).
    pub fn set_cross_zone_publisher(&self, publisher: CrossZonePublisher) {
        *self.cross_zone.write().expect("cross_zone lock poisoned") = Some(publisher);
    }

    /// Dispatch an event that arrived from ANOTHER zone: local fan-out only,
    /// never re-published (loop prevention).
    pub async fn trigger_remote_event(
        &self,
        event: &str,
        args_json: String,
    ) -> Result<(), ScriptError> {
        self.broadcast_chain_inner(event.to_owned(), args_json, false).await;
        Ok(())
    }

    /// The net bridge shared with the runtimes (pending native calls).
    pub fn net(&self) -> &crate::net_bridge::NetBridge {
        &self.net
    }

    /// Dispatch a client-originated net event into every runtime with
    /// `globalThis.source` bound.
    pub async fn trigger_net_event(
        &self,
        event: &str,
        source: u32,
        args: &serde_json::Value,
    ) -> Result<(), ScriptError> {
        let args_json =
            serde_json::to_string(args).map_err(|e| ScriptError::HostStart(e.to_string()))?;
        let mut queued = Vec::new();
        {
            let runtimes = self.runtimes.read().await;
            for (resource, handle) in runtimes.iter() {
                let event = event.to_owned();
                let args_json = args_json.clone();
                match handle
                    .send(|reply| RuntimeCommand::DispatchNetEvent {
                        event,
                        source,
                        args_json,
                        reply,
                    })
                    .await
                {
                    Ok(mut q) => queued.append(&mut q),
                    Err(e) => {
                        tracing::error!(target: "scripting", %resource, error = %e, "net event dispatch failed");
                    }
                }
            }
        }
        self.rebroadcast(queued).await;
        Ok(())
    }

    /// Collect zone-transferable script state for a handoff (jalon D4):
    /// resource name → merged JSON object from its
    /// `RegisterZoneTransferState` callbacks.
    pub async fn collect_zone_transfer_state(
        &self,
        source: u32,
    ) -> std::collections::HashMap<String, String> {
        let mut out = std::collections::HashMap::new();
        let runtimes = self.runtimes.read().await;
        for (resource, handle) in runtimes.iter() {
            let (reply, rx) = oneshot::channel();
            if handle.tx.send(RuntimeCommand::CollectTransferState { source, reply }).await.is_err()
            {
                continue;
            }
            match rx.await {
                Ok(Ok(Some(json))) => {
                    out.insert(resource.clone(), json);
                }
                Ok(Ok(None)) => {}
                Ok(Err(e)) => {
                    tracing::error!(target: "scripting", %resource, error = %e,
                        "zone transfer state collection failed");
                }
                Err(_) => {}
            }
        }
        out
    }

    /// The deferral registry shared with the JS runtimes.
    pub fn deferrals(&self) -> &Arc<DeferralRegistry> {
        &self.deferrals
    }

    /// Create the resource's isolate (own thread), run its server scripts in
    /// order, then broadcast `onResourceStart`.
    pub async fn load_resource(
        &self,
        name: &str,
        scripts: Vec<ScriptSource>,
    ) -> Result<(), ScriptError> {
        // Replace an existing runtime (ResourceManager stops first in the
        // normal path; this keeps load idempotent regardless).
        self.runtimes.write().await.remove(name);

        let handle = spawn_runtime_thread(
            name,
            self.started_at,
            Arc::clone(&self.deferrals),
            Arc::clone(&self.players),
            self.net.clone(),
        )?;
        let queued = handle
            .send(|reply| RuntimeCommand::ExecuteScripts { scripts, reply })
            .await?;

        self.runtimes.write().await.insert(name.to_owned(), handle);
        self.rebroadcast(queued).await;

        self.trigger_event(
            "onResourceStart",
            std::slice::from_ref(&serde_json::Value::String(name.to_owned())),
        )
        .await?;
        tracing::info!(target: "scripting", resource = %name, "resource started");
        Ok(())
    }

    /// Broadcast `onResourceStop`, then destroy the resource's isolate.
    pub async fn unload_resource(&self, name: &str) -> Result<(), ScriptError> {
        if !self.runtimes.read().await.contains_key(name) {
            return Err(ScriptError::ResourceNotLoaded(name.to_owned()));
        }
        self.trigger_event(
            "onResourceStop",
            std::slice::from_ref(&serde_json::Value::String(name.to_owned())),
        )
        .await?;
        self.runtimes.write().await.remove(name);
        tracing::info!(target: "scripting", resource = %name, "resource unloaded");
        Ok(())
    }

    /// Broadcast an event to every loaded resource runtime, following any
    /// `TriggerEvent` chains the handlers produce (bounded).
    pub async fn trigger_event(
        &self,
        event: &str,
        args: &[serde_json::Value],
    ) -> Result<(), ScriptError> {
        let args_json =
            serde_json::to_string(args).map_err(|e| ScriptError::HostStart(e.to_string()))?;
        self.broadcast_chain(event.to_owned(), args_json).await;
        Ok(())
    }

    /// Fire `playerConnecting` in every runtime for `source`. If no handler
    /// called `defer()`, the connection auto-resolves as accepted (FXServer
    /// semantics). The outcome is observed through the deferral registry.
    pub async fn fire_player_connecting(
        &self,
        source: u32,
        player_name: &str,
    ) -> Result<(), ScriptError> {
        let mut queued = Vec::new();
        {
            let runtimes = self.runtimes.read().await;
            for (resource, handle) in runtimes.iter() {
                let player_name = player_name.to_owned();
                match handle
                    .send(|reply| RuntimeCommand::DispatchPlayerConnecting {
                        source,
                        player_name,
                        reply,
                    })
                    .await
                {
                    Ok(mut q) => queued.append(&mut q),
                    Err(e) => {
                        tracing::error!(target: "scripting", %resource, error = %e, "playerConnecting dispatch failed");
                    }
                }
            }
        }
        self.rebroadcast(queued).await;
        self.deferrals.resolve_if_not_deferred(source);
        Ok(())
    }

    async fn rebroadcast(&self, queued: QueuedEvents) {
        for (event, args_json) in queued {
            self.broadcast_chain(event, args_json).await;
        }
    }

    async fn broadcast_chain(&self, event: String, args_json: String) {
        self.broadcast_chain_inner(event, args_json, true).await;
    }

    async fn broadcast_chain_inner(&self, event: String, args_json: String, publish_entry: bool) {
        let mut pending = VecDeque::new();
        pending.push_back((event, args_json));
        let mut dispatched = 0;

        while let Some((event, args_json)) = pending.pop_front() {
            dispatched += 1;
            // Cross-zone fan-out (Phase D): locally-originated, non-lifecycle
            // events mirror to sibling zones. The entry event of a REMOTE
            // chain is skipped (loop prevention) — but reactions produced by
            // local handlers are local origin and do propagate.
            let is_remote_entry = dispatched == 1 && !publish_entry;
            if !is_remote_entry && !is_zone_local_event(&event) {
                if let Some(p) =
                    self.cross_zone.read().expect("cross_zone lock poisoned").as_ref()
                {
                    p(&event, &args_json);
                }
            }
            if dispatched > MAX_EVENT_CHAIN {
                tracing::warn!(target: "scripting", %event, "event chain exceeded {MAX_EVENT_CHAIN} dispatches; dropping remainder");
                break;
            }
            let runtimes = self.runtimes.read().await;
            for (resource, handle) in runtimes.iter() {
                let event = event.clone();
                let args_json = args_json.clone();
                match handle
                    .send(|reply| RuntimeCommand::DispatchEvent {
                        event,
                        args_json,
                        reply,
                    })
                    .await
                {
                    Ok(queued) => pending.extend(queued),
                    Err(e) => {
                        tracing::error!(target: "scripting", %resource, error = %e, "event dispatch failed");
                    }
                }
            }
        }
    }
}

/// Spawn the dedicated isolate thread for one resource.
fn spawn_runtime_thread(
    resource_name: &str,
    started_at: Instant,
    deferrals: Arc<DeferralRegistry>,
    players: Arc<PlayerDirectory>,
    net: crate::net_bridge::NetBridge,
) -> Result<ResourceRuntimeHandle, ScriptError> {
    let (tx, mut rx) = mpsc::channel::<RuntimeCommand>(64);
    let name = resource_name.to_owned();
    // Runtime creation happens on the isolate thread; report init errors
    // through this channel so load_resource can surface them.
    let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<(), ScriptError>>();

    std::thread::Builder::new()
        .name(format!("baston-rt-{name}"))
        .spawn(move || {
            let tokio_rt = match tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = init_tx.send(Err(ScriptError::HostStart(e.to_string())));
                    return;
                }
            };
            let mut runtime = match ScriptRuntime::new(&name, started_at, deferrals, players, net) {
                Ok(r) => r,
                Err(e) => {
                    let _ = init_tx.send(Err(e));
                    return;
                }
            };
            let _ = init_tx.send(Ok(()));

            let local = tokio::task::LocalSet::new();
            local.block_on(&tokio_rt, async move {
                while let Some(cmd) = rx.recv().await {
                    match cmd {
                        RuntimeCommand::ExecuteScripts { scripts, reply } => {
                            let mut result = Ok(());
                            for script in scripts {
                                if let Err(e) = runtime
                                    .execute_resource_script(&script.path, script.code)
                                    .await
                                {
                                    result = Err(e);
                                    break;
                                }
                            }
                            let _ = reply.send(result.map(|()| runtime.drain_queued_events()));
                        }
                        RuntimeCommand::DispatchEvent {
                            event,
                            args_json,
                            reply,
                        } => {
                            let result = runtime.dispatch_event(&event, &args_json).await;
                            let _ = reply.send(result.map(|()| runtime.drain_queued_events()));
                        }
                        RuntimeCommand::DispatchPlayerConnecting {
                            source,
                            player_name,
                            reply,
                        } => {
                            let result = runtime
                                .dispatch_player_connecting(source, &player_name)
                                .await;
                            let _ = reply.send(result.map(|()| runtime.drain_queued_events()));
                        }
                        RuntimeCommand::DispatchNetEvent {
                            event,
                            source,
                            args_json,
                            reply,
                        } => {
                            let result =
                                runtime.dispatch_net_event(&event, source, &args_json).await;
                            let _ = reply.send(result.map(|()| runtime.drain_queued_events()));
                        }
                        RuntimeCommand::CollectTransferState { source, reply } => {
                            let result = runtime.collect_zone_transfer_state(source).await;
                            let _ = reply.send(result);
                        }
                    }
                }
            });
        })
        .map_err(|e| ScriptError::HostStart(e.to_string()))?;

    init_rx.recv().map_err(|_| ScriptError::HostGone)??;

    Ok(ResourceRuntimeHandle { tx })
}
