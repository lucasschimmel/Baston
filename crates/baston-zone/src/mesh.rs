//! Zone side of the federation: `ZoneService` gRPC implementation, gateway
//! registration, and the 5s heartbeat loop.

use std::sync::Arc;
use std::time::{Duration, Instant};

use baston_protocol::mesh::gateway_service_client::GatewayServiceClient;
use baston_protocol::mesh::zone_service_server::{ZoneService, ZoneServiceServer};
use baston_protocol::mesh::{
    ActivatePlayerRequest, ActivatePlayerResponse, HeartbeatRequest, PlayerStateRequest,
    PrepareForPlayerResponse, RegisterZoneRequest, ReleasePlayerRequest, ReleasePlayerResponse,
};
use baston_protocol::{Aabb, PlayerStateSnapshot};
use dashmap::DashMap;
use tonic::transport::Channel;
use tonic::{Request, Response, Status};

/// Ghost players pre-loaded ahead of a handoff: present in the registry with
/// `Pending` state, no `playerJoining` fired yet.
#[derive(Debug)]
pub enum GhostState {
    Pending { snapshot: PlayerStateSnapshot, created_at: Instant },
    Active,
}

/// Hooks the zone runtime provides to the mesh layer. Wired fully in D4;
/// D1 only needs the count providers for heartbeats.
pub struct ZoneMeshHooks {
    pub player_count: Arc<dyn Fn() -> u32 + Send + Sync>,
    pub entity_count: Arc<dyn Fn() -> u32 + Send + Sync>,
    /// Ghost → active transition: fire `playerJoining`, restore script_state.
    pub on_activate_player: Arc<dyn Fn(u32, &PlayerStateSnapshot) + Send + Sync>,
    /// Cleanup on release: internal playerDropped, entity release.
    pub on_release_player: Arc<dyn Fn(u32, &str) + Send + Sync>,
}

pub struct ZoneMesh {
    pub zone_id: String,
    pub bounds: Aabb,
    /// Address the Gateway can call us back on (registered value).
    pub public_grpc_addr: String,
    pub max_players: u32,
    gateway: GatewayServiceClient<Channel>,
    ghosts: DashMap<u32, GhostState>,
    hooks: ZoneMeshHooks,
}

impl ZoneMesh {
    pub async fn connect(
        zone_id: String,
        bounds: Aabb,
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
            public_grpc_addr,
            max_players,
            gateway,
            ghosts: DashMap::new(),
            hooks,
        }))
    }

    /// Register with the Gateway, retrying until accepted (the Gateway may
    /// still be booting when the zone container starts).
    pub async fn register_with_gateway(&self) -> anyhow::Result<()> {
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            let req = RegisterZoneRequest {
                zone_id: self.zone_id.clone(),
                bounds: Some(self.bounds.into()),
                grpc_addr: self.public_grpc_addr.clone(),
                max_players: self.max_players as i32,
            };
            match self.gateway.clone().register_zone(req).await {
                Ok(resp) if resp.get_ref().accepted => {
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
        let snapshot = PlayerStateSnapshot::decode(&req.snapshot)
            .map_err(Status::invalid_argument)?;
        if snapshot.source_id != req.player_id {
            return Err(Status::invalid_argument("snapshot source_id mismatch"));
        }
        self.mesh.ghosts.insert(
            req.player_id,
            GhostState::Pending { snapshot, created_at: Instant::now() },
        );
        tracing::info!(target: "zone", zone = %self.mesh.zone_id,
            "PrepareForPlayer: ghost created for player={}", req.player_id);
        Ok(Response::new(PrepareForPlayerResponse { ready: true, message: String::new() }))
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
        Ok(Response::new(ActivatePlayerResponse { ok: true, message: String::new() }))
    }

    async fn release_player(
        &self,
        request: Request<ReleasePlayerRequest>,
    ) -> Result<Response<ReleasePlayerResponse>, Status> {
        let req = request.into_inner();
        self.mesh.ghosts.remove(&req.player_id);
        (self.mesh.hooks.on_release_player)(req.player_id, &req.reason);
        tracing::info!(target: "zone", zone = %self.mesh.zone_id,
            "player={} released ({})", req.player_id,
            if req.reason.is_empty() { "playerDropped internal" } else { &req.reason });
        Ok(Response::new(ReleasePlayerResponse { ok: true }))
    }
}

fn normalize_grpc_uri(addr: &str) -> String {
    if addr.starts_with("http://") || addr.starts_with("https://") {
        addr.to_owned()
    } else {
        format!("http://{addr}")
    }
}
