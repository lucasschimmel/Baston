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
use dashmap::DashMap;
use tokio::sync::{mpsc, oneshot, RwLock};

use crate::deferrals::DeferralRegistry;
use crate::error::ScriptError;
use crate::observability::Observability;
use crate::resource_registry::ResourceRegistry;
#[cfg(feature = "js")]
use crate::runtime::ScriptRuntime;
use crate::{InMemoryRoutingControl, RoutingControl, StateBagChange, StateBagStore};

/// A script file to load into a resource's isolate.
#[derive(Debug)]
pub struct ScriptSource {
    /// Path relative to the resource dir (for error messages).
    pub path: String,
    pub code: String,
}

/// Events queued by JS `TriggerEvent` during a dispatch, to re-broadcast.
type QueuedEvents = Vec<(String, String)>;

// The `lite` bundle compiles no engine, so nothing consumes these — the
// commands still describe the protocol every engine implements, and deleting
// them per-bundle would fragment it.
#[cfg_attr(not(any(feature = "js", feature = "lua")), allow(dead_code))]
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
    DispatchCommand {
        command: String,
        source: u32,
        args: Vec<String>,
        raw: String,
        reply: oneshot::Sender<Result<QueuedEvents, ScriptError>>,
    },
    CollectTransferState {
        // Only the JS collector reads it; Lua has no transfer-state surface.
        #[cfg_attr(not(feature = "js"), allow(dead_code))]
        source: u32,
        reply: oneshot::Sender<Result<Option<String>, ScriptError>>,
    },
    DispatchStateBagChanges {
        reply: oneshot::Sender<Result<QueuedEvents, ScriptError>>,
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
        self.begin(make)
            .await?
            .await
            .map_err(|_| ScriptError::HostGone)?
    }

    /// Enqueue a command and return the reply receiver without awaiting it, so
    /// broadcasts can start every resource's dispatch before collecting any
    /// reply (one slow resource must not delay the others' start).
    async fn begin(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<QueuedEvents, ScriptError>>) -> RuntimeCommand,
    ) -> Result<oneshot::Receiver<Result<QueuedEvents, ScriptError>>, ScriptError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(make(reply))
            .await
            .map_err(|_| ScriptError::HostGone)?;
        Ok(rx)
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
    observability: Arc<Observability>,
    convars: Arc<DashMap<String, String>>,
    resources: ResourceRegistry,
    state_bags: StateBagStore,
    routing: Arc<dyn RoutingControl>,
    entity_world: Arc<crate::EntityWorldView>,
    /// Set by the composition root once the authoritative world exists.
    /// Resources loaded before then get the inert control, which refuses
    /// entity creation rather than pretending to succeed.
    world_control: Arc<std::sync::RwLock<Arc<dyn crate::WorldControl>>>,
    /// Persistent KVP store, set by the composition root once its path is
    /// known. Resources loaded before then get an in-memory store.
    kvp: Arc<std::sync::RwLock<Arc<crate::KvpStore>>>,
    /// Outbound HTTP bridge, set by the composition root that owns the worker.
    http: Arc<std::sync::RwLock<Option<crate::HttpBridge>>>,
    /// Resource lifecycle control, set by the composition root that owns the
    /// resource manager. Until then a script cannot start or stop anything.
    resource_control: Arc<std::sync::RwLock<Arc<dyn crate::ResourceControl>>>,
    /// Inbound HTTP handlers (`SetHttpHandler`). Always present: registration
    /// is driven by the resources themselves, and the gateway reads it to
    /// decide whether a request has anywhere to go.
    http_handlers: Arc<crate::HttpHandlerRegistry>,
    started_at: Instant,
    cross_zone: Arc<std::sync::RwLock<Option<CrossZonePublisher>>>,
    voice: Arc<std::sync::RwLock<Option<Arc<dyn crate::native_state::VoiceControl>>>>,
    /// Database pool, set by the composition root when the `db` module is
    /// on. Resources loaded before then see no database and say so.
    db: Arc<std::sync::RwLock<Option<Arc<dyn crate::native_state::DbAccess>>>>,
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
        Self::spawn_with_net_and_game_state(
            deferrals,
            players,
            net,
            StateBagStore::default(),
            Arc::new(InMemoryRoutingControl::default()),
        )
    }

    /// Create a host with externally supplied authoritative state. This is
    /// the integration point for a zone-owned routing registry and for a
    /// networking layer that needs to share the exact state-bag store.
    pub fn spawn_with_net_and_game_state(
        deferrals: Arc<DeferralRegistry>,
        players: Arc<PlayerDirectory>,
        net: crate::net_bridge::NetBridge,
        state_bags: StateBagStore,
        routing: Arc<dyn RoutingControl>,
    ) -> Result<Self, ScriptError> {
        Ok(Self {
            runtimes: Arc::new(RwLock::new(HashMap::new())),
            deferrals,
            players,
            net,
            observability: Observability::shared(),
            convars: Arc::new(DashMap::new()),
            resources: ResourceRegistry::default(),
            state_bags,
            routing,
            entity_world: Arc::new(crate::EntityWorldView::new()),
            world_control: Arc::new(std::sync::RwLock::new(Arc::new(crate::NoWorldControl))),
            kvp: Arc::new(std::sync::RwLock::new(Arc::new(
                crate::KvpStore::in_memory(),
            ))),
            http: Arc::new(std::sync::RwLock::new(None)),
            http_handlers: Arc::new(crate::HttpHandlerRegistry::new()),
            resource_control: Arc::new(std::sync::RwLock::new(Arc::new(crate::NoResourceControl))),
            started_at: Instant::now(),
            cross_zone: Arc::new(std::sync::RwLock::new(None)),
            voice: Arc::new(std::sync::RwLock::new(None)),
            db: Arc::new(std::sync::RwLock::new(None)),
        })
    }

    /// Install the voice control surface (`MUMBLE_*` natives). Applies to
    /// resources loaded afterwards — call before `load_resource`.
    pub fn set_voice_control(&self, voice: Arc<dyn crate::native_state::VoiceControl>) {
        *self.voice.write().unwrap_or_else(|e| e.into_inner()) = Some(voice);
    }

    /// Install the database pool backing the `db` natives. Applies to
    /// resources loaded afterwards.
    pub fn set_db(&self, db: Arc<dyn crate::native_state::DbAccess>) {
        *self.db.write().unwrap_or_else(|e| e.into_inner()) = Some(db);
    }

    fn db(&self) -> Option<Arc<dyn crate::native_state::DbAccess>> {
        self.db.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Install the authoritative world's write side, backing entity creation
    /// and deletion. Applies to resources loaded afterwards.
    pub fn set_world_control(&self, control: Arc<dyn crate::WorldControl>) {
        *self
            .world_control
            .write()
            .unwrap_or_else(|e| e.into_inner()) = control;
    }

    fn world_control(&self) -> Arc<dyn crate::WorldControl> {
        Arc::clone(&self.world_control.read().unwrap_or_else(|e| e.into_inner()))
    }

    /// Install the persistent KVP store. Applies to resources loaded
    /// afterwards, so the composition root must call this before the first
    /// `load_resource` or a resource will write to a store nobody persists.
    pub fn set_kvp_store(&self, kvp: Arc<crate::KvpStore>) {
        *self.kvp.write().unwrap_or_else(|e| e.into_inner()) = kvp;
    }

    /// The store shared by every resource isolate.
    pub fn kvp(&self) -> Arc<crate::KvpStore> {
        Arc::clone(&self.kvp.read().unwrap_or_else(|e| e.into_inner()))
    }

    /// Install the outbound HTTP bridge. Applies to resources loaded
    /// afterwards.
    pub fn set_http_bridge(&self, bridge: crate::HttpBridge) {
        *self.http.write().unwrap_or_else(|e| e.into_inner()) = Some(bridge);
    }

    fn http(&self) -> Option<crate::HttpBridge> {
        self.http.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The inbound HTTP handler registry, shared with the gateway route that
    /// feeds it.
    pub fn http_handlers(&self) -> Arc<crate::HttpHandlerRegistry> {
        Arc::clone(&self.http_handlers)
    }

    /// Install the resource lifecycle control (`StartResource` and friends).
    /// Applies to resources loaded afterwards.
    pub fn set_resource_control(&self, control: Arc<dyn crate::ResourceControl>) {
        *self
            .resource_control
            .write()
            .unwrap_or_else(|e| e.into_inner()) = control;
    }

    fn resource_control(&self) -> Arc<dyn crate::ResourceControl> {
        Arc::clone(
            &self
                .resource_control
                .read()
                .unwrap_or_else(|e| e.into_inner()),
        )
    }

    /// Dispatch an event into one resource only.
    ///
    /// Broadcasting would work — the token in the payload is meaningless to
    /// anyone else — but an HTTP reply is addressed to exactly one resource,
    /// and waking every isolate for it would put a slow endpoint's latency on
    /// the whole server.
    pub async fn trigger_event_on(
        &self,
        resource: &str,
        event: &str,
        args: &[serde_json::Value],
    ) -> Result<(), ScriptError> {
        let args_json =
            serde_json::to_string(args).map_err(|e| ScriptError::HostStart(e.to_string()))?;
        let queued = {
            let runtimes = self.runtimes.read().await;
            let Some(handle) = runtimes.get(resource) else {
                // The resource stopped while the request was in flight. Not an
                // error: there is simply nobody left to tell.
                return Ok(());
            };
            handle
                .send(|reply| RuntimeCommand::DispatchEvent {
                    event: event.to_owned(),
                    args_json,
                    reply,
                })
                .await?
        };
        self.rebroadcast(queued).await;
        Ok(())
    }

    pub fn observability(&self) -> Arc<Observability> {
        Arc::clone(&self.observability)
    }

    pub fn resources(&self) -> ResourceRegistry {
        self.resources.clone()
    }

    /// Shared state-bag store used by every resource isolate.
    pub fn state_bags(&self) -> StateBagStore {
        self.state_bags.clone()
    }

    /// Routing-bucket control shared by scripting and future zone adapters.
    pub fn routing_control(&self) -> Arc<dyn RoutingControl> {
        Arc::clone(&self.routing)
    }

    /// The world mirror the entity natives read.
    ///
    /// The authoritative game state publishes into this once per sync tick;
    /// until it does, entity natives correctly report an empty world.
    pub fn entity_world(&self) -> Arc<crate::EntityWorldView> {
        Arc::clone(&self.entity_world)
    }

    /// Drain explicitly replicated state-bag changes for the networking layer.
    pub fn drain_replicated_state_bags(&self, limit: usize) -> Vec<StateBagChange> {
        self.state_bags.drain_replicated(limit)
    }

    /// Install the Phase D cross-zone event publisher (zone processes only).
    pub fn set_cross_zone_publisher(&self, publisher: CrossZonePublisher) {
        *self.cross_zone.write().unwrap_or_else(|e| e.into_inner()) = Some(publisher);
    }

    /// Dispatch an event that arrived from ANOTHER zone: local fan-out only,
    /// never re-published (loop prevention).
    pub async fn trigger_remote_event(
        &self,
        event: &str,
        args_json: String,
    ) -> Result<(), ScriptError> {
        self.broadcast_chain_inner(event.to_owned(), args_json, false)
            .await;
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
            let mut replies = Vec::with_capacity(runtimes.len());
            for (resource, handle) in runtimes.iter() {
                let event = event.to_owned();
                let args_json = args_json.clone();
                match handle
                    .begin(|reply| RuntimeCommand::DispatchNetEvent {
                        event,
                        source,
                        args_json,
                        reply,
                    })
                    .await
                {
                    Ok(rx) => replies.push((resource.clone(), rx)),
                    Err(e) => {
                        tracing::error!(target: "scripting", %resource, error = %e, "net event dispatch failed");
                    }
                }
            }
            for (resource, rx) in replies {
                match rx.await.map_err(|_| ScriptError::HostGone).and_then(|r| r) {
                    Ok(mut q) => queued.append(&mut q),
                    Err(e) => {
                        tracing::error!(target: "scripting", %resource, error = %e, "net event dispatch failed");
                    }
                }
            }
        }
        self.rebroadcast(queued).await;
        self.rebroadcast(self.flush_state_bag_callbacks().await)
            .await;
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
            if handle
                .tx
                .send(RuntimeCommand::CollectTransferState { source, reply })
                .await
                .is_err()
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
        self.state_bags.cleanup_resource(name);
        // A reload starts from no handler: the new isolate re-registers only
        // if it still calls SetHttpHandler.
        self.http_handlers.unregister(name);

        // A client-only or files-only resource has nothing to run here.
        // Spawning a runtime for it would cost a V8 isolate (or a Lua state)
        // per such resource and buy nothing — and a server with a large
        // streaming set has many of them.
        if scripts.is_empty() {
            tracing::info!(target: "scripting", resource = %name,
                "no server scripts — no runtime spawned");
            self.trigger_event(
                "onResourceStart",
                std::slice::from_ref(&serde_json::Value::String(name.to_owned())),
            )
            .await?;
            return Ok(());
        }

        let script_paths: Vec<String> = scripts.iter().map(|s| s.path.clone()).collect();
        let engine = crate::engine::select(name, &script_paths)?;
        tracing::info!(target: "scripting", resource = %name, %engine, "runtime selected");

        let handle = spawn_runtime_thread(RuntimeThreadParams {
            resource_name: name,
            engine,
            started_at: self.started_at,
            deferrals: Arc::clone(&self.deferrals),
            players: Arc::clone(&self.players),
            net: self.net.clone(),
            observability: Arc::clone(&self.observability),
            convars: Arc::clone(&self.convars),
            resources: self.resources.clone(),
            state_bags: self.state_bags.clone(),
            routing: Arc::clone(&self.routing),
            entity_world: Arc::clone(&self.entity_world),
            world_control: self.world_control(),
            kvp: self.kvp(),
            http: self.http(),
            http_handlers: Arc::clone(&self.http_handlers),
            resource_control: self.resource_control(),
            voice: self.voice.read().unwrap_or_else(|e| e.into_inner()).clone(),
            db: self.db(),
        })?;
        let mut queued = handle
            .send(|reply| RuntimeCommand::ExecuteScripts { scripts, reply })
            .await?;

        self.runtimes.write().await.insert(name.to_owned(), handle);
        queued.extend(self.flush_state_bag_callbacks().await);
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
        self.state_bags.cleanup_resource(name);
        // Its route must stop answering with the isolate gone, or the next
        // request parks a waiter nobody can ever resolve.
        self.http_handlers.unregister(name);
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
            let mut replies = Vec::with_capacity(runtimes.len());
            for (resource, handle) in runtimes.iter() {
                let player_name = player_name.to_owned();
                match handle
                    .begin(|reply| RuntimeCommand::DispatchPlayerConnecting {
                        source,
                        player_name,
                        reply,
                    })
                    .await
                {
                    Ok(rx) => replies.push((resource.clone(), rx)),
                    Err(e) => {
                        tracing::error!(target: "scripting", %resource, error = %e, "playerConnecting dispatch failed");
                    }
                }
            }
            for (resource, rx) in replies {
                match rx.await.map_err(|_| ScriptError::HostGone).and_then(|r| r) {
                    Ok(mut q) => queued.append(&mut q),
                    Err(e) => {
                        tracing::error!(target: "scripting", %resource, error = %e, "playerConnecting dispatch failed");
                    }
                }
            }
        }
        self.rebroadcast(queued).await;
        self.rebroadcast(self.flush_state_bag_callbacks().await)
            .await;
        self.deferrals.resolve_if_not_deferred(source);
        Ok(())
    }

    pub async fn execute_command(
        &self,
        command: &str,
        source: u32,
        args: Vec<String>,
        raw: String,
    ) -> Result<(), ScriptError> {
        let mut queued = Vec::new();
        {
            let runtimes = self.runtimes.read().await;
            let mut replies = Vec::with_capacity(runtimes.len());
            for (resource, handle) in runtimes.iter() {
                match handle
                    .begin(|reply| RuntimeCommand::DispatchCommand {
                        command: command.to_owned(),
                        source,
                        args: args.clone(),
                        raw: raw.clone(),
                        reply,
                    })
                    .await
                {
                    Ok(rx) => replies.push((resource.clone(), rx)),
                    Err(e) => {
                        tracing::error!(target: "scripting", %resource, error = %e, "command dispatch failed");
                    }
                }
            }
            for (resource, rx) in replies {
                match rx.await.map_err(|_| ScriptError::HostGone).and_then(|r| r) {
                    Ok(mut q) => queued.append(&mut q),
                    Err(e) => {
                        tracing::error!(target: "scripting", %resource, error = %e, "command dispatch failed");
                    }
                }
            }
        }
        self.rebroadcast(queued).await;
        self.rebroadcast(self.flush_state_bag_callbacks().await)
            .await;
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
                if let Some(p) = self
                    .cross_zone
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                {
                    p(&event, &args_json);
                }
            }
            if dispatched > MAX_EVENT_CHAIN {
                tracing::warn!(target: "scripting", %event, "event chain exceeded {MAX_EVENT_CHAIN} dispatches; dropping remainder");
                break;
            }
            let runtimes = self.runtimes.read().await;
            let mut replies = Vec::with_capacity(runtimes.len());
            for (resource, handle) in runtimes.iter() {
                let event = event.clone();
                let args_json = args_json.clone();
                match handle
                    .begin(|reply| RuntimeCommand::DispatchEvent {
                        event,
                        args_json,
                        reply,
                    })
                    .await
                {
                    Ok(rx) => replies.push((resource.clone(), rx)),
                    Err(e) => {
                        tracing::error!(target: "scripting", %resource, error = %e, "event dispatch failed");
                    }
                }
            }
            for (resource, rx) in replies {
                match rx.await.map_err(|_| ScriptError::HostGone).and_then(|r| r) {
                    Ok(queued) => pending.extend(queued),
                    Err(e) => {
                        tracing::error!(target: "scripting", %resource, error = %e, "event dispatch failed");
                    }
                }
            }
            drop(runtimes);
            pending.extend(self.flush_state_bag_callbacks().await);
        }
    }

    /// Poll queued state-bag callbacks at a host/event-loop boundary. A
    /// bounded number of rounds allows handlers to write another bag while
    /// preventing a recursive handler pair from monopolizing the host.
    async fn flush_state_bag_callbacks(&self) -> QueuedEvents {
        const MAX_CALLBACK_ROUNDS: usize = 64;
        let mut queued_events = Vec::new();
        for _ in 0..MAX_CALLBACK_ROUNDS {
            if self.state_bags.pending_deliveries() == 0 {
                return queued_events;
            }
            let runtimes = self.runtimes.read().await;
            let mut replies = Vec::with_capacity(runtimes.len());
            for (resource, handle) in runtimes.iter() {
                match handle
                    .begin(|reply| RuntimeCommand::DispatchStateBagChanges { reply })
                    .await
                {
                    Ok(rx) => replies.push((resource.clone(), rx)),
                    Err(error) => tracing::error!(
                        target: "scripting",
                        %resource,
                        %error,
                        "state bag callback dispatch failed"
                    ),
                }
            }
            drop(runtimes);
            for (resource, rx) in replies {
                match rx.await.map_err(|_| ScriptError::HostGone).and_then(|r| r) {
                    Ok(mut events) => queued_events.append(&mut events),
                    Err(error) => tracing::error!(
                        target: "scripting",
                        %resource,
                        %error,
                        "state bag callback dispatch failed"
                    ),
                }
            }
        }
        tracing::warn!(
            target: "scripting",
            pending = self.state_bags.pending_deliveries(),
            "state bag callback chain exceeded {MAX_CALLBACK_ROUNDS} rounds"
        );
        queued_events
    }
}

// Only an engine reads these; the `lite` bundle compiles none.
#[cfg_attr(not(any(feature = "js", feature = "lua")), allow(dead_code))]
struct RuntimeThreadParams<'a> {
    resource_name: &'a str,
    /// Chosen from the resource's script extensions before the thread starts,
    /// so an unsupported resource fails at load with a bundle hint rather than
    /// after spawning a runtime it cannot use.
    engine: crate::engine::Engine,
    started_at: Instant,
    deferrals: Arc<DeferralRegistry>,
    players: Arc<PlayerDirectory>,
    net: crate::net_bridge::NetBridge,
    observability: Arc<Observability>,
    convars: Arc<DashMap<String, String>>,
    resources: ResourceRegistry,
    state_bags: StateBagStore,
    routing: Arc<dyn RoutingControl>,
    entity_world: Arc<crate::EntityWorldView>,
    world_control: Arc<dyn crate::WorldControl>,
    kvp: Arc<crate::KvpStore>,
    http: Option<crate::HttpBridge>,
    http_handlers: Arc<crate::HttpHandlerRegistry>,
    resource_control: Arc<dyn crate::ResourceControl>,
    voice: Option<Arc<dyn crate::native_state::VoiceControl>>,
    db: Option<Arc<dyn crate::native_state::DbAccess>>,
}

/// The `lite` bundle has no scripting engine at all.
///
/// Unreachable in practice — [`crate::engine::select`] refuses every resource
/// before a thread is ever spawned — but stating it here keeps the real
/// function out of a build that could not use it, instead of leaving a body
/// full of unused bindings.
#[cfg(not(any(feature = "js", feature = "lua")))]
fn spawn_runtime_thread(
    params: RuntimeThreadParams<'_>,
) -> Result<ResourceRuntimeHandle, ScriptError> {
    Err(ScriptError::RuntimeInit {
        resource: params.resource_name.to_owned(),
        message: "this build has no scripting runtime\n  \
                  → the `lite` bundle runs no resources; use the js, lua or full bundle"
            .to_owned(),
    })
}

/// Spawn the dedicated runtime thread for one resource.
#[cfg(any(feature = "js", feature = "lua"))]
fn spawn_runtime_thread(
    params: RuntimeThreadParams<'_>,
) -> Result<ResourceRuntimeHandle, ScriptError> {
    let (tx, rx) = mpsc::channel::<RuntimeCommand>(64);
    let name = params.resource_name.to_owned();
    let RuntimeThreadParams {
        engine,
        started_at,
        deferrals,
        players,
        net,
        observability,
        convars,
        resources,
        state_bags,
        routing,
        entity_world,
        world_control,
        kvp,
        http,
        http_handlers,
        resource_control,
        voice,
        db,
        ..
    } = params;
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
            let shared_game_state = crate::native_state::SharedGameState {
                state_bags,
                routing,
                entity_world,
                world_control,
                kvp,
                http,
                http_handlers,
                resource_control,
            };

            // Each engine drives its own loop. The V8 loop is intricate
            // (tickets, settle tasks, an event-loop pump); Lua has no event
            // loop of its own and only needs a tick. Keeping them separate is
            // what stops one engine's complexity from taxing the other.
            match engine {
                #[cfg(feature = "js")]
                crate::engine::Engine::Js => {
                    let mut runtime = match ScriptRuntime::new(
                        &name,
                        started_at,
                        deferrals,
                        players,
                        net,
                        observability,
                    ) {
                        Ok(r) => r,
                        Err(e) => {
                            let _ = init_tx.send(Err(e));
                            return;
                        }
                    };
                    runtime.install_server_state(convars, resources);
                    runtime.install_shared_game_state(shared_game_state);
                    runtime.install_voice(crate::native_state::SharedVoice(voice));
                    runtime.install_db(crate::native_state::SharedDb(db));
                    let _ = init_tx.send(Ok(()));

                    let local = tokio::task::LocalSet::new();
                    local.block_on(&tokio_rt, run_isolate_loop(runtime, rx));
                }
                #[cfg(feature = "lua")]
                crate::engine::Engine::Lua => {
                    let mut runtime = match crate::lua::LuaRuntime::new(
                        &name,
                        started_at,
                        deferrals,
                        players,
                        net,
                        observability,
                    ) {
                        Ok(r) => r,
                        Err(e) => {
                            let _ = init_tx.send(Err(e));
                            return;
                        }
                    };
                    runtime.install_server_state(convars, resources);
                    runtime.install_shared_game_state(shared_game_state);
                    runtime.install_voice(crate::native_state::SharedVoice(voice));
                    runtime.install_db(crate::native_state::SharedDb(db));
                    let _ = init_tx.send(Ok(()));

                    tokio_rt.block_on(run_lua_loop(runtime, rx));
                }
                // `engine::select` already refused anything this build cannot
                // run, so the remaining arms are unreachable — but the match
                // must still compile in every bundle.
                #[allow(unreachable_patterns)]
                other => {
                    let _ = init_tx.send(Err(ScriptError::RuntimeInit {
                        resource: name.clone(),
                        message: format!("no {other} runtime in this build"),
                    }));
                }
            }
        })
        .map_err(|e| ScriptError::HostStart(e.to_string()))?;

    init_rx.recv().map_err(|_| ScriptError::HostGone)??;

    Ok(ResourceRuntimeHandle { tx })
}

/// The Lua thread's command loop.
///
/// Far simpler than its V8 counterpart, and that is the point: a Lua dispatch
/// is a synchronous call, so there is no event loop to pump and no ticket to
/// settle. The only asynchrony is cooperative — `Citizen.CreateThread`
/// coroutines resumed by `tick`, which runs between commands and while idle.
#[cfg(feature = "lua")]
async fn run_lua_loop(mut runtime: crate::lua::LuaRuntime, mut rx: mpsc::Receiver<RuntimeCommand>) {
    use crate::observability::DispatchKind;

    loop {
        // Resume coroutines, then wait for the next command for no longer than
        // the runtime asked to sleep — a thread doing `Wait(0)` must not have
        // to wait for a command to arrive before it runs again.
        let idle = runtime.tick();
        let command = match tokio::time::timeout(idle, rx.recv()).await {
            Err(_elapsed) => continue,
            Ok(None) => break,
            Ok(Some(command)) => command,
        };

        match command {
            RuntimeCommand::ExecuteScripts { scripts, reply } => {
                let mut result = Ok(());
                for script in scripts {
                    if let Err(e) = runtime.execute_script(&script.path, &script.code) {
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
                let result = runtime.dispatch_event(&event, &args_json, None, DispatchKind::Event);
                let _ = reply.send(result.map(|()| runtime.drain_queued_events()));
            }
            RuntimeCommand::DispatchNetEvent {
                event,
                source,
                args_json,
                reply,
            } => {
                let result = runtime.dispatch_event(
                    &event,
                    &args_json,
                    Some(source),
                    DispatchKind::NetEvent,
                );
                let _ = reply.send(result.map(|()| runtime.drain_queued_events()));
            }
            RuntimeCommand::DispatchPlayerConnecting {
                source,
                player_name,
                reply,
            } => {
                let result = runtime.dispatch_player_connecting(source, &player_name);
                let _ = reply.send(result.map(|()| runtime.drain_queued_events()));
            }
            RuntimeCommand::DispatchCommand {
                command,
                source,
                args,
                raw,
                reply,
            } => {
                let result = runtime
                    .dispatch_command(&command, source, &args, &raw)
                    .map(|_handled| runtime.drain_queued_events());
                let _ = reply.send(result);
            }
            RuntimeCommand::CollectTransferState { source, reply } => {
                let _ = reply.send(runtime.collect_zone_transfer_state(source));
            }
            RuntimeCommand::DispatchStateBagChanges { reply } => {
                let result = runtime.dispatch_state_bag_changes();
                let _ = reply.send(result.map(|()| runtime.drain_queued_events()));
            }
        }
    }
}

/// Shared coordination state between the event-loop pump task, the command
/// loop, and the per-dispatch settle tasks. Single-threaded (`Rc`) — all of it
/// lives on the resource's isolate thread.
#[cfg(feature = "js")]
struct PumpShared {
    /// Signaled whenever a dispatch starts, so an idle pump resumes polling.
    wake: tokio::sync::Notify,
    /// Signaled after every event-loop poll pass: settle tasks re-check their
    /// completion promise.
    progress: tokio::sync::Notify,
    /// Bumped each time the event loop drains to idle.
    idle_gen: std::cell::Cell<u64>,
}

#[cfg(feature = "js")]
type SharedRuntime = std::rc::Rc<std::cell::RefCell<ScriptRuntime>>;

/// The isolate thread's command loop (audit ROB-2). One pump task drives the
/// V8 event loop; each dispatch runs its synchronous execute phase inline (so
/// per-resource handler start-order is preserved) and then settles its reply
/// from a `spawn_local` task. A handler stalled on a client-native await no
/// longer blocks the next command. Invariant: the `RefCell` borrow is never
/// held across an await.
#[cfg(feature = "js")]
async fn run_isolate_loop(runtime: ScriptRuntime, mut rx: mpsc::Receiver<RuntimeCommand>) {
    let rt: SharedRuntime = std::rc::Rc::new(std::cell::RefCell::new(runtime));
    let shared = std::rc::Rc::new(PumpShared {
        wake: tokio::sync::Notify::new(),
        progress: tokio::sync::Notify::new(),
        idle_gen: std::cell::Cell::new(0),
    });
    let pump = tokio::task::spawn_local(drive_event_loop(
        std::rc::Rc::clone(&rt),
        std::rc::Rc::clone(&shared),
    ));

    while let Some(cmd) = rx.recv().await {
        match cmd {
            RuntimeCommand::ExecuteScripts { scripts, reply } => {
                // Load path stays serialized: each script runs to event-loop
                // idle before the next, matching the old run-to-completion
                // semantics resources rely on during startup.
                let mut result = Ok(());
                for script in scripts {
                    let ticket = rt.borrow_mut().start_script_load(&script.path, script.code);
                    let since_idle = shared.idle_gen.get();
                    shared.wake.notify_one();
                    if let Err(e) = settle_ticket(&rt, &shared, ticket, since_idle).await {
                        result = Err(e);
                        break;
                    }
                    wait_event_loop_idle(&shared, since_idle).await;
                }
                let _ = reply.send(result.map(|()| rt.borrow_mut().drain_queued_events()));
            }
            RuntimeCommand::DispatchEvent {
                event,
                args_json,
                reply,
            } => {
                let ticket = rt.borrow_mut().start_event_dispatch(&event, &args_json);
                spawn_settle(&rt, &shared, ticket, reply);
            }
            RuntimeCommand::DispatchPlayerConnecting {
                source,
                player_name,
                reply,
            } => {
                let ticket = rt
                    .borrow_mut()
                    .start_player_connecting_dispatch(source, &player_name);
                spawn_settle(&rt, &shared, ticket, reply);
            }
            RuntimeCommand::DispatchNetEvent {
                event,
                source,
                args_json,
                reply,
            } => {
                let ticket = rt
                    .borrow_mut()
                    .start_net_event_dispatch(&event, source, &args_json);
                spawn_settle(&rt, &shared, ticket, reply);
            }
            RuntimeCommand::DispatchCommand {
                command,
                source,
                args,
                raw,
                reply,
            } => {
                let ticket = rt
                    .borrow_mut()
                    .start_command_dispatch(&command, source, &args, &raw);
                match ticket {
                    Some(ticket) => spawn_settle(&rt, &shared, ticket, reply),
                    None => {
                        let _ = reply.send(Ok(Vec::new()));
                    }
                }
            }
            RuntimeCommand::CollectTransferState { source, reply } => {
                // The transfer-state collector is fully synchronous JS.
                let result = rt.borrow_mut().collect_zone_transfer_state_sync(source);
                shared.wake.notify_one();
                let _ = reply.send(result);
            }
            RuntimeCommand::DispatchStateBagChanges { reply } => {
                let ticket = rt.borrow_mut().start_state_bag_dispatch();
                spawn_settle(&rt, &shared, ticket, reply);
            }
        }
    }
    pump.abort();
}

/// The single event-loop poller. Waits for `wake` while the loop is idle,
/// re-polls whenever new dispatches start, and signals `progress` after each
/// pass so settle tasks re-check their promises.
#[cfg(feature = "js")]
async fn drive_event_loop(rt: SharedRuntime, shared: std::rc::Rc<PumpShared>) {
    loop {
        let drive = std::future::poll_fn(|cx| {
            let poll = rt.borrow_mut().poll_event_loop_pass(cx);
            shared.progress.notify_waiters();
            poll
        });
        tokio::select! {
            biased;
            _ = shared.wake.notified() => {}
            res = drive => {
                if let Err(e) = res {
                    tracing::error!(target: "scripting", error = %e, "script event loop error");
                }
                shared.idle_gen.set(shared.idle_gen.get() + 1);
                shared.progress.notify_waiters();
                shared.wake.notified().await;
            }
        }
    }
}

#[cfg(feature = "js")]
fn spawn_settle(
    rt: &SharedRuntime,
    shared: &std::rc::Rc<PumpShared>,
    ticket: crate::runtime::DispatchTicket,
    reply: oneshot::Sender<Result<QueuedEvents, ScriptError>>,
) {
    // Sampled synchronously after the execute phase and before any yield: an
    // idle_gen bump past this value proves a full event-loop drain that
    // included this dispatch's ops.
    let since_idle = shared.idle_gen.get();
    shared.wake.notify_one();
    let rt = std::rc::Rc::clone(rt);
    let shared = std::rc::Rc::clone(shared);
    tokio::task::spawn_local(async move {
        let result = settle_ticket(&rt, &shared, ticket, since_idle).await;
        let _ = reply.send(result.map(|()| rt.borrow_mut().drain_queued_events()));
    });
}

/// Wait until the event loop has drained to idle at least once since
/// `since_idle` was sampled.
#[cfg(feature = "js")]
async fn wait_event_loop_idle(shared: &std::rc::Rc<PumpShared>, since_idle: u64) {
    loop {
        let notified = shared.progress.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if shared.idle_gen.get() > since_idle {
            return;
        }
        notified.await;
    }
}

/// Wait for a dispatch's completion promise to settle. A promise still pending
/// once the event loop drains to idle is dangling (nothing left can resolve
/// it) and is treated as complete — the pre-ROB-2 host replied at event-loop
/// idle too, so this preserves the old contract for such handlers.
#[cfg(feature = "js")]
async fn settle_ticket(
    rt: &SharedRuntime,
    shared: &std::rc::Rc<PumpShared>,
    ticket: crate::runtime::DispatchTicket,
    since_idle: u64,
) -> Result<(), ScriptError> {
    use crate::runtime::PromiseOutcome;
    let crate::runtime::DispatchTicket { started, meta } = ticket;
    match started {
        Err(e) => {
            rt.borrow_mut().finish_dispatch(&meta, Some(&e.to_string()));
            Err(e)
        }
        Ok(None) => {
            rt.borrow_mut().finish_dispatch(&meta, None);
            Ok(())
        }
        Ok(Some(promise)) => loop {
            let notified = shared.progress.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let outcome = rt.borrow_mut().promise_outcome(&promise);
            match outcome {
                PromiseOutcome::Pending => {
                    if shared.idle_gen.get() > since_idle {
                        rt.borrow_mut().finish_dispatch(&meta, None);
                        return Ok(());
                    }
                    notified.await;
                }
                PromiseOutcome::Fulfilled => {
                    rt.borrow_mut().finish_dispatch(&meta, None);
                    return Ok(());
                }
                PromiseOutcome::Rejected(message) => {
                    let resource = {
                        let mut r = rt.borrow_mut();
                        r.finish_dispatch(&meta, Some(&message));
                        r.resource_name().to_owned()
                    };
                    return Err(ScriptError::Execute {
                        resource,
                        script: "<dispatch>".to_owned(),
                        message,
                    });
                }
            }
        },
    }
}
