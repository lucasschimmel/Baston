//! Gateway side of the zone federation: gRPC `GatewayService` implementation
//! plus new-player routing built on `ZoneRegistry` + `ConnectionRouter`.

use std::sync::Arc;
use std::time::Duration;

use baston_protocol::mesh::gateway_service_server::{GatewayService, GatewayServiceServer};
use baston_protocol::mesh::{
    ConfirmHandoffRequest, ConfirmHandoffResponse, HeartbeatRequest, HeartbeatResponse,
    PlayerStateRequest, PrepareHandoffRequest, PrepareHandoffResponse, RegisterZoneRequest,
    RegisterZoneResponse,
};
use baston_protocol::Aabb;
use tonic::{Request, Response, Status};

use crate::connection_router::ConnectionRouter;
use crate::zone_registry::ZoneRegistry;

/// Timeout for the Gateway → Zone `PrepareForPlayer` call during handoff prep.
const HANDOFF_PREP_TIMEOUT: Duration = Duration::from_secs(2);

/// Callback invoked after a handoff commit (StateAggregator rerouting + UDP
/// buffer flush are wired here in jalon D4).
pub type HandoffCommittedHook = Arc<dyn Fn(u32, &str, &str) + Send + Sync>;

pub struct GatewayMesh {
    pub registry: Arc<ZoneRegistry>,
    pub router: Arc<ConnectionRouter>,
    on_handoff_committed: std::sync::RwLock<Option<HandoffCommittedHook>>,
}

impl GatewayMesh {
    pub fn new(registry: Arc<ZoneRegistry>, router: Arc<ConnectionRouter>) -> Arc<Self> {
        Arc::new(Self { registry, router, on_handoff_committed: std::sync::RwLock::new(None) })
    }

    pub fn set_handoff_committed_hook(&self, hook: HandoffCommittedHook) {
        *self.on_handoff_committed.write().expect("hook lock poisoned") = Some(hook);
    }

    /// Route a newly connected player: quadtree lookup on spawn coords, else
    /// least-loaded fallback. Returns the assigned zone (None = no zone up).
    pub async fn route_new_player(&self, source: u32, spawn: Option<(f32, f32)>) -> Option<String> {
        let zone = match spawn {
            Some((x, y)) => match self.registry.find_zone_for_coords(x, y).await {
                Some(z) => Some(z),
                None => self.registry.find_least_loaded_zone().await,
            },
            None => self.registry.find_least_loaded_zone().await,
        }?;
        self.router.assign(source, &zone);
        match spawn {
            Some((x, y)) => tracing::info!(target: "gateway",
                "player={source} spawn=({x:.0}, {y:.0}) → routed to {zone}"),
            None => tracing::info!(target: "gateway",
                "player={source} (no spawn coords) → routed to {zone} (least loaded)"),
        }
        Some(zone)
    }

    /// Zone failure recovery (jalon D6): the zone is already evicted from the
    /// registry; reroute every orphaned player to the least-loaded surviving
    /// zone, or kick when no zone is left. Returns (rerouted, kicked).
    pub async fn handle_zone_failure(
        &self,
        failed_zone: &str,
        kick: &(dyn Fn(u32, &str) + Send + Sync),
    ) -> (usize, usize) {
        let started = std::time::Instant::now();
        tracing::error!(target: "gateway", zone = %failed_zone,
            "zone failure detected — initiating recovery");
        let orphaned = self.router.players_in_zone(failed_zone);
        let total = orphaned.len();
        let (mut rerouted, mut kicked) = (0, 0);
        for source in orphaned {
            match self.registry.find_least_loaded_zone_excluding(Some(failed_zone)).await {
                Some(fallback) => {
                    self.router.commit_handoff(source, &fallback).await;
                    rerouted += 1;
                }
                None => {
                    kick(source, "Server zone unavailable");
                    self.router.remove(source);
                    kicked += 1;
                }
            }
        }
        metrics::counter!("zone_recovery_players_rerouted_total").increment(rerouted as u64);
        tracing::warn!(target: "gateway",
            "Rerouting {total} players from {failed_zone} — complete in {:.1}s ({rerouted} rerouted, {kicked} kicked)",
            started.elapsed().as_secs_f64());
        (rerouted, kicked)
    }

    /// Drain a zone: reroute all its players to the least-loaded other zone.
    /// Returns how many players were rerouted. (Full state migration rides
    /// the D4 handoff path; routing moves immediately so the zone empties.)
    pub async fn drain_zone(&self, zone_id: &str) -> usize {
        let players = self.router.players_in_zone(zone_id);
        let mut moved = 0;
        for source in players {
            let Some(fallback) =
                self.registry.find_least_loaded_zone_excluding(Some(zone_id)).await
            else {
                tracing::error!(target: "gateway",
                    "drain {zone_id}: no fallback zone available — player={source} left in place");
                break;
            };
            self.router.commit_handoff(source, &fallback).await;
            moved += 1;
        }
        tracing::info!(target: "gateway", zone = %zone_id, moved, "zone drained");
        moved
    }

    /// Start the gRPC server for zone registration/heartbeat/handoff.
    pub fn spawn_grpc_server(
        self: &Arc<Self>,
        addr: std::net::SocketAddr,
    ) -> tokio::task::JoinHandle<()> {
        let mesh = Arc::clone(self);
        tokio::spawn(async move {
            tracing::info!(target: "gateway", %addr, "GatewayService gRPC listening");
            if let Err(e) = tonic::transport::Server::builder()
                .add_service(GatewayServiceServer::new(GatewayGrpc { mesh }))
                .serve(addr)
                .await
            {
                tracing::error!(target: "gateway", error = %e, "gRPC server exited");
            }
        })
    }
}

struct GatewayGrpc {
    mesh: Arc<GatewayMesh>,
}

#[tonic::async_trait]
impl GatewayService for GatewayGrpc {
    async fn register_zone(
        &self,
        request: Request<RegisterZoneRequest>,
    ) -> Result<Response<RegisterZoneResponse>, Status> {
        let req = request.into_inner();
        let bounds: Aabb = req
            .bounds
            .ok_or_else(|| Status::invalid_argument("bounds required"))?
            .into();
        match self
            .mesh
            .registry
            .register_zone(&req.zone_id, bounds, &req.grpc_addr, req.max_players.max(0) as u32)
            .await
        {
            Ok(()) => Ok(Response::new(RegisterZoneResponse { accepted: true, message: String::new() })),
            Err(e) => Ok(Response::new(RegisterZoneResponse { accepted: false, message: e })),
        }
    }

    async fn heartbeat(
        &self,
        request: Request<HeartbeatRequest>,
    ) -> Result<Response<HeartbeatResponse>, Status> {
        let req = request.into_inner();
        let known = self
            .mesh
            .registry
            .heartbeat(&req.zone_id, req.player_count, req.entity_count)
            .await;
        Ok(Response::new(HeartbeatResponse { known }))
    }

    async fn prepare_handoff(
        &self,
        request: Request<PrepareHandoffRequest>,
    ) -> Result<Response<PrepareHandoffResponse>, Status> {
        let req = request.into_inner();
        tracing::info!(target: "gateway",
            "PrepareHandoff received: player={} {}→{}",
            req.player_id, req.from_zone,
            if req.target_zone.is_empty() { "<auto>" } else { &req.target_zone });

        // Resolve the target zone: explicit, or by predicted crossing coords.
        let target_zone = if req.target_zone.is_empty() {
            self.mesh
                .registry
                .find_zone_for_coords(req.predicted_x, req.predicted_y)
                .await
                .filter(|z| z != &req.from_zone)
        } else if self.mesh.registry.contains(&req.target_zone).await {
            Some(req.target_zone.clone())
        } else {
            None
        };
        let Some(target_zone) = target_zone else {
            return Ok(Response::new(PrepareHandoffResponse {
                ready: false,
                target_zone: String::new(),
                message: "no live zone covers the target coordinates".into(),
                target_zone_grpc: String::new(),
            }));
        };
        let target_zone_grpc =
            self.mesh.registry.zone_grpc_addr(&target_zone).await.unwrap_or_default();

        let Some(mut client) = self.mesh.registry.zone_client(&target_zone).await else {
            return Ok(Response::new(PrepareHandoffResponse {
                ready: false,
                target_zone,
                message: "target zone has no gRPC client".into(),
                target_zone_grpc: String::new(),
            }));
        };

        // Never unwrap: the target zone can die mid-call. Timeout 2s.
        let prep = tokio::time::timeout(
            HANDOFF_PREP_TIMEOUT,
            client.prepare_for_player(PlayerStateRequest {
                player_id: req.player_id,
                from_zone: req.from_zone.clone(),
                snapshot: req.snapshot,
            }),
        )
        .await;

        match prep {
            Ok(Ok(resp)) => {
                let inner = resp.into_inner();
                Ok(Response::new(PrepareHandoffResponse {
                    ready: inner.ready,
                    target_zone,
                    message: inner.message,
                    target_zone_grpc,
                }))
            }
            Ok(Err(status)) => {
                tracing::error!(target: "gateway", zone = %target_zone, error = %status,
                    "PrepareForPlayer failed");
                metrics::counter!("handoff_prepare_failures_total", "zone" => target_zone.clone())
                    .increment(1);
                Ok(Response::new(PrepareHandoffResponse {
                    ready: false,
                    target_zone,
                    message: status.to_string(),
                    target_zone_grpc: String::new(),
                }))
            }
            Err(_) => {
                tracing::error!(target: "gateway", zone = %target_zone,
                    "PrepareForPlayer timed out ({HANDOFF_PREP_TIMEOUT:?})");
                metrics::counter!("handoff_prepare_timeouts_total", "zone" => target_zone.clone())
                    .increment(1);
                Ok(Response::new(PrepareHandoffResponse {
                    ready: false,
                    target_zone,
                    message: "prepare timeout".into(),
                    target_zone_grpc: String::new(),
                }))
            }
        }
    }

    async fn confirm_handoff(
        &self,
        request: Request<ConfirmHandoffRequest>,
    ) -> Result<Response<ConfirmHandoffResponse>, Status> {
        let req = request.into_inner();
        if !self.mesh.registry.contains(&req.to_zone).await {
            return Ok(Response::new(ConfirmHandoffResponse {
                ok: false,
                message: format!("target zone {} is not registered", req.to_zone),
            }));
        }
        // Atomic commit: lock → update routing table → release. Notify after.
        self.mesh.router.commit_handoff(req.player_id, &req.to_zone).await;
        if let Some(hook) = self
            .mesh
            .on_handoff_committed
            .read()
            .expect("hook lock poisoned")
            .as_ref()
        {
            hook(req.player_id, &req.from_zone, &req.to_zone);
        }
        metrics::counter!("handoffs_committed_total").increment(1);
        Ok(Response::new(ConfirmHandoffResponse { ok: true, message: String::new() }))
    }
}
