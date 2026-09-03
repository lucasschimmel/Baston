//! Zone side of the federation: `ZoneService` gRPC implementation, gateway
//! registration, and the 5s heartbeat loop.

use std::sync::Arc;
use std::time::{Duration, Instant};

use baston_protocol::mesh::gateway_service_client::GatewayServiceClient;
use baston_protocol::mesh::zone_service_client::ZoneServiceClient;
use baston_protocol::mesh::zone_service_server::{ZoneService, ZoneServiceServer};
use baston_protocol::mesh::{
    ActivatePlayerRequest, ActivatePlayerResponse, ControlResourceRequest, ControlResourceResponse,
    HeartbeatRequest, ListResourcesRequest, ListResourcesResponse, PlayerStateRequest,
    PrepareForPlayerResponse, RegisterZoneRequest, ReleasePlayerRequest, ReleasePlayerResponse,
    ResmonSnapshotRequest, ResmonSnapshotResponse, ResourceStatus,
};
use baston_protocol::{Aabb, PlayerStateSnapshot, ZoneCoverage};
use dashmap::DashMap;
use tonic::transport::Channel;
use tonic::{Request, Response, Status};

/// Ghost players pre-loaded ahead of a handoff: present in the registry with
/// `Pending` state, no `playerJoining` fired yet.
#[derive(Debug)]
pub enum GhostState {
    Pending {
        /// Boxed so an `Active` ghost — the common, long-lived case — does not
        /// pay the full snapshot's footprint in every map entry.
        snapshot: Box<PlayerStateSnapshot>,
        created_at: Instant,
    },
    Active,
}

/// Ghost → active transition callback: fire `playerJoining`, restore state.
type ActivatePlayerFn = Arc<dyn Fn(u32, &PlayerStateSnapshot) + Send + Sync>;
/// Release callback: internal `playerDropped`, entity release.
type ReleasePlayerFn = Arc<dyn Fn(u32, &str) + Send + Sync>;

/// Hooks the zone runtime provides to the mesh layer. Wired fully in D4;
/// D1 only needs the count providers for heartbeats.
pub struct ZoneMeshHooks {
    pub player_count: Arc<dyn Fn() -> u32 + Send + Sync>,
    pub entity_count: Arc<dyn Fn() -> u32 + Send + Sync>,
    /// Ghost → active transition: fire `playerJoining`, restore script_state.
    pub on_activate_player: ActivatePlayerFn,
    /// Cleanup on release: internal playerDropped, entity release.
    pub on_release_player: ReleasePlayerFn,
}

pub struct ZoneMesh {
    pub zone_id: String,
    /// Bounds this zone declares for itself, if any. Only its actual
    /// territory when the Gateway has no map file; otherwise the map overrules
    /// it and the answer to `RegisterZone` replaces [`Self::coverage`].
    pub bounds: Option<Aabb>,
    /// What this zone owns, as the Gateway sees it. Behind an `Arc` so the
    /// boundary scan can take it once per pass rather than clone a polygon
    /// per player.
    coverage: std::sync::RwLock<Arc<ZoneCoverage>>,
    /// Address the Gateway can call us back on (registered value).
    pub public_grpc_addr: String,
    pub max_players: u32,
    gateway: GatewayServiceClient<Channel>,
    ghosts: DashMap<u32, GhostState>,
    /// Lazy gRPC clients to sibling zones, keyed by address (ActivatePlayer
    /// after a handoff commit).
    peer_clients: DashMap<String, ZoneServiceClient<Channel>>,
    hooks: ZoneMeshHooks,
    /// Wired by the composition root so the Gateway's admin API can relay
    /// resource control to this zone. `None` until set (RPCs answer
    /// unavailable).
    resource_manager: std::sync::RwLock<Option<Arc<crate::ResourceManager>>>,
    /// Wired by the composition root (the manager is built after the mesh) so
    /// releasing a player also clears its handoff bookkeeping.
    handoff_manager: std::sync::RwLock<Option<Arc<crate::handoff_manager::HandoffManager>>>,
}

impl ZoneMesh {
    pub async fn connect(
        zone_id: String,
        bounds: Option<Aabb>,
        gateway_grpc: &str,
        public_grpc_addr: String,
        max_players: u32,
        hooks: ZoneMeshHooks,
    ) -> anyhow::Result<Arc<Self>> {
        let endpoint = Channel::from_shared(normalize_grpc_uri(gateway_grpc))?
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(5));
        let gateway = GatewayServiceClient::new(endpoint.connect_lazy());
        Ok(Arc::new(Self {
            zone_id,
            bounds,
            coverage: std::sync::RwLock::new(Arc::new(
                bounds.map(ZoneCoverage::from_bounds).unwrap_or_default(),
            )),
            public_grpc_addr,
            max_players,
            gateway,
            ghosts: DashMap::new(),
            peer_clients: DashMap::new(),
            hooks,
            resource_manager: std::sync::RwLock::new(None),
            handoff_manager: std::sync::RwLock::new(None),
        }))
    }

    /// What this zone owns right now.
    pub fn coverage(&self) -> Arc<ZoneCoverage> {
        Arc::clone(&self.coverage.read().expect("coverage lock poisoned"))
    }

    /// Give the mesh access to the zone's ResourceManager (resource-control
    /// RPCs). Call before `spawn_grpc_server`.
    /// Let a release purge handoff bookkeeping for the departing player.
    pub fn set_handoff_manager(&self, manager: Arc<crate::handoff_manager::HandoffManager>) {
        *self
            .handoff_manager
            .write()
            .expect("handoff manager lock poisoned") = Some(manager);
    }

    fn handoff_manager(&self) -> Option<Arc<crate::handoff_manager::HandoffManager>> {
        self.handoff_manager
            .read()
            .expect("handoff manager lock poisoned")
            .clone()
    }

    pub fn set_resource_manager(&self, rm: Arc<crate::ResourceManager>) {
        *self.resource_manager.write().expect("rm lock poisoned") = Some(rm);
    }

    fn resource_manager(&self) -> Option<Arc<crate::ResourceManager>> {
        self.resource_manager
            .read()
            .expect("rm lock poisoned")
            .clone()
    }

    /// Register with the Gateway, retrying until accepted (the Gateway may
    /// still be booting when the zone container starts).
    pub async fn register_with_gateway(&self) -> anyhow::Result<()> {
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            let req = RegisterZoneRequest {
                zone_id: self.zone_id.clone(),
                bounds: self.bounds.map(Into::into),
                grpc_addr: self.public_grpc_addr.clone(),
                max_players: self.max_players as i32,
            };
            match self.gateway.clone().register_zone(req).await {
                Ok(resp) if resp.get_ref().accepted => {
                    self.adopt_coverage(resp.into_inner().coverage);
                    tracing::info!(target: "zone", zone = %self.zone_id,
                        "registered with gateway (attempt {attempt})");
                    return Ok(());
                }
                Ok(resp) => {
                    anyhow::bail!("gateway refused registration: {}", resp.get_ref().message);
                }
                Err(status) => {
                    if attempt >= 30 {
                        anyhow::bail!("gateway unreachable after {attempt} attempts: {status}");
                    }
                    tracing::warn!(target: "zone", error = %status,
                        "gateway not ready — retrying registration in 2s (attempt {attempt})");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    }

    /// Take the territory the Gateway assigned us.
    ///
    /// A malformed or absent coverage keeps the declared rectangle rather than
    /// leaving the zone owning nothing: a zone that thinks it owns no ground
    /// hands every player away and accepts none back.
    fn adopt_coverage(&self, wire: Option<baston_protocol::mesh::Coverage>) {
        let Some(wire) = wire else { return };
        match ZoneCoverage::try_from(wire) {
            Ok(coverage) if !coverage.is_empty() => {
                tracing::info!(target: "zone", zone = %self.zone_id,
                    "territory from the gateway map: {} region(s), {} carved out",
                    coverage.shapes().len(), coverage.overlays().len());
                *self.coverage.write().expect("coverage lock poisoned") = Arc::new(coverage);
            }
            Ok(_) => {}
            Err(e) => {
                tracing::error!(target: "zone", zone = %self.zone_id,
                    "gateway sent an unusable territory ({e}) — keeping declared bounds");
            }
        }
    }

    /// 5s heartbeat loop. A failed heartbeat is logged, never fatal; if the
    /// Gateway no longer knows us (evicted after silence), we re-register.
    pub fn spawn_heartbeat_loop(
        self: &Arc<Self>,
        interval_secs: u64,
    ) -> tokio::task::JoinHandle<()> {
        let mesh = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                let req = HeartbeatRequest {
                    zone_id: mesh.zone_id.clone(),
                    player_count: (mesh.hooks.player_count)(),
                    entity_count: (mesh.hooks.entity_count)(),
                };
                match mesh.gateway.clone().heartbeat(req).await {
                    Ok(resp) if !resp.get_ref().known => {
                        tracing::warn!(target: "zone", zone = %mesh.zone_id,
                            "gateway evicted us — re-registering");
                        if let Err(e) = mesh.register_with_gateway().await {
                            tracing::error!(target: "zone", error = %e, "re-registration failed");
                        }
                    }
                    Ok(_) => {}
                    Err(status) => {
                        // Failure is logged, not fatal — the gateway may restart.
                        tracing::warn!(target: "zone", error = %status, "heartbeat failed");
                        metrics::counter!("zone_heartbeat_failures_total").increment(1);
                    }
                }
            }
        })
    }

    /// Serve `ZoneService` for Gateway callbacks.
    pub fn spawn_grpc_server(
        self: &Arc<Self>,
        addr: std::net::SocketAddr,
    ) -> tokio::task::JoinHandle<()> {
        let mesh = Arc::clone(self);
        tokio::spawn(async move {
            tracing::info!(target: "zone", %addr, "ZoneService gRPC listening");
            if let Err(e) = tonic::transport::Server::builder()
                .add_service(ZoneServiceServer::new(ZoneGrpc { mesh }))
                .serve(addr)
                .await
            {
                tracing::error!(target: "zone", error = %e, "ZoneService gRPC exited");
            }
        })
    }

    pub fn gateway_client(&self) -> GatewayServiceClient<Channel> {
        self.gateway.clone()
    }

    /// Lazy, cached client to a sibling zone's ZoneService.
    pub fn peer_client(&self, addr: &str) -> Result<ZoneServiceClient<Channel>, String> {
        if let Some(c) = self.peer_clients.get(addr) {
            return Ok(c.clone());
        }
        let endpoint = Channel::from_shared(normalize_grpc_uri(addr))
            .map_err(|e| format!("invalid peer zone addr {addr:?}: {e}"))?
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(2));
        let client = ZoneServiceClient::new(endpoint.connect_lazy());
        self.peer_clients.insert(addr.to_owned(), client.clone());
        Ok(client)
    }

    pub fn ghost_state(&self, player_id: u32) -> Option<&'static str> {
        self.ghosts.get(&player_id).map(|g| match *g {
            GhostState::Pending { .. } => "pending",
            GhostState::Active => "active",
        })
    }

    /// Drop ghosts that were prepared but never activated (player turned
    /// around). Called periodically by the boundary loop (D3).
    pub fn expire_stale_ghosts(&self, max_age: Duration) {
        let now = Instant::now();
        self.ghosts.retain(|player_id, g| match g {
            GhostState::Pending { created_at, .. } => {
                let keep = now.duration_since(*created_at) <= max_age;
                if !keep {
                    tracing::debug!(target: "zone", player = player_id, "expired stale ghost");
                }
                keep
            }
            GhostState::Active => true,
        });
    }

    pub fn remove_ghost(&self, player_id: u32) {
        self.ghosts.remove(&player_id);
    }
}

struct ZoneGrpc {
    mesh: Arc<ZoneMesh>,
}

#[tonic::async_trait]
impl ZoneService for ZoneGrpc {
    async fn prepare_for_player(
        &self,
        request: Request<PlayerStateRequest>,
    ) -> Result<Response<PrepareForPlayerResponse>, Status> {
        let req = request.into_inner();
        let snapshot =
            PlayerStateSnapshot::decode(&req.snapshot).map_err(Status::invalid_argument)?;
        if snapshot.source_id != req.player_id {
            return Err(Status::invalid_argument("snapshot source_id mismatch"));
        }
        self.mesh.ghosts.insert(
            req.player_id,
            GhostState::Pending {
                snapshot: Box::new(snapshot),
                created_at: Instant::now(),
            },
        );
        tracing::info!(target: "zone", zone = %self.mesh.zone_id,
            "PrepareForPlayer: ghost created for player={}", req.player_id);
        Ok(Response::new(PrepareForPlayerResponse {
            ready: true,
            message: String::new(),
        }))
    }

    async fn activate_player(
        &self,
        request: Request<ActivatePlayerRequest>,
    ) -> Result<Response<ActivatePlayerResponse>, Status> {
        let req = request.into_inner();
        let Some((_, ghost)) = self.mesh.ghosts.remove(&req.player_id) else {
            return Ok(Response::new(ActivatePlayerResponse {
                ok: false,
                message: format!("no ghost prepared for player {}", req.player_id),
            }));
        };
        let GhostState::Pending { snapshot, .. } = ghost else {
            return Ok(Response::new(ActivatePlayerResponse {
                ok: false,
                message: format!("player {} already active", req.player_id),
            }));
        };
        (self.mesh.hooks.on_activate_player)(req.player_id, &snapshot);
        self.mesh.ghosts.insert(req.player_id, GhostState::Active);
        tracing::info!(target: "zone", zone = %self.mesh.zone_id,
            "player={} activated from ghost state", req.player_id);
        Ok(Response::new(ActivatePlayerResponse {
            ok: true,
            message: String::new(),
        }))
    }

    async fn release_player(
        &self,
        request: Request<ReleasePlayerRequest>,
    ) -> Result<Response<ReleasePlayerResponse>, Status> {
        let req = request.into_inner();
        self.mesh.ghosts.remove(&req.player_id);
        if let Some(manager) = self.mesh.handoff_manager() {
            manager.forget(req.player_id);
        }
        (self.mesh.hooks.on_release_player)(req.player_id, &req.reason);
        tracing::info!(target: "zone", zone = %self.mesh.zone_id,
            "player={} released ({})", req.player_id,
            if req.reason.is_empty() { "playerDropped internal" } else { &req.reason });
        Ok(Response::new(ReleasePlayerResponse { ok: true }))
    }

    async fn list_resources(
        &self,
        _request: Request<ListResourcesRequest>,
    ) -> Result<Response<ListResourcesResponse>, Status> {
        let Some(rm) = self.mesh.resource_manager() else {
            return Err(Status::unavailable("resource manager not wired"));
        };
        let resources = rm
            .status()
            .await
            .into_iter()
            .map(|(name, state)| ResourceStatus {
                name,
                state: format!("{state:?}").to_ascii_lowercase(),
            })
            .collect();
        Ok(Response::new(ListResourcesResponse { resources }))
    }

    async fn control_resource(
        &self,
        request: Request<ControlResourceRequest>,
    ) -> Result<Response<ControlResourceResponse>, Status> {
        let Some(rm) = self.mesh.resource_manager() else {
            return Err(Status::unavailable("resource manager not wired"));
        };
        let req = request.into_inner();
        let result = match req.action.as_str() {
            "start" => rm.start(&req.name).await,
            "stop" => rm.stop(&req.name).await,
            "restart" => rm.restart(&req.name).await,
            other => {
                return Err(Status::invalid_argument(format!(
                    "unknown action {other:?} (start|stop|restart)"
                )));
            }
        };
        tracing::info!(target: "zone", zone = %self.mesh.zone_id,
            resource = %req.name, action = %req.action, ok = result.is_ok(),
            "resource control via gateway");
        Ok(Response::new(match result {
            Ok(()) => ControlResourceResponse {
                ok: true,
                message: String::new(),
            },
            Err(e) => ControlResourceResponse {
                ok: false,
                message: e.to_string(),
            },
        }))
    }

    async fn get_resmon_snapshot(
        &self,
        _request: Request<ResmonSnapshotRequest>,
    ) -> Result<Response<ResmonSnapshotResponse>, Status> {
        let Some(rm) = self.mesh.resource_manager() else {
            return Err(Status::unavailable("resource manager not wired"));
        };
        let snapshot = rm.observability().snapshot();
        let snapshot_json =
            serde_json::to_string(&snapshot).map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(ResmonSnapshotResponse {
            ok: true,
            message: String::new(),
            snapshot_json,
        }))
    }
}

fn normalize_grpc_uri(addr: &str) -> String {
    if addr.starts_with("http://") || addr.starts_with("https://") {
        addr.to_owned()
    } else {
        format!("http://{addr}")
    }
}
