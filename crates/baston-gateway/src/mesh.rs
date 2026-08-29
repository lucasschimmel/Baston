//! Gateway side of the zone federation: gRPC `GatewayService` implementation
//! plus new-player routing built on `ZoneRegistry` + `ConnectionRouter`.

use std::sync::Arc;
use std::time::Duration;

use baston_protocol::mesh::gateway_service_server::{GatewayService, GatewayServiceServer};
use baston_protocol::mesh::{
    ActivatePlayerRequest, ConfirmHandoffRequest, ConfirmHandoffResponse, HeartbeatRequest,
    HeartbeatResponse, PlayerStateRequest, PrepareHandoffRequest, PrepareHandoffResponse,
    RegisterZoneRequest, RegisterZoneResponse,
};
use baston_protocol::{Aabb, PlayerDirectory, PlayerStateSnapshot};
use futures::StreamExt;
use tonic::{Request, Response, Status};

use crate::connection_router::ConnectionRouter;
use crate::zone_registry::ZoneRegistry;

/// Timeout for the Gateway → Zone `PrepareForPlayer` call during handoff prep.
const HANDOFF_PREP_TIMEOUT: Duration = Duration::from_secs(2);

/// How many recovered players are registered in their new zone at once.
/// High enough that a large recovery finishes quickly, low enough that a
/// surviving zone is not hit with thousands of simultaneous RPCs while it is
/// already absorbing the extra load.
const RECOVERY_CONCURRENCY: usize = 32;

/// Callback invoked after a handoff commit (StateAggregator rerouting + UDP
/// buffer flush are wired here in jalon D4).
pub type HandoffCommittedHook = Arc<dyn Fn(u32, &str, &str) + Send + Sync>;

pub struct GatewayMesh {
    pub registry: Arc<ZoneRegistry>,
    pub router: Arc<ConnectionRouter>,
    on_handoff_committed: std::sync::RwLock<Option<HandoffCommittedHook>>,
    /// Wired by the composition root. Zone-failure recovery needs it to tell
    /// the surviving zone who the players it just inherited actually are.
    players: std::sync::RwLock<Option<Arc<PlayerDirectory>>>,
}

impl GatewayMesh {
    pub fn new(registry: Arc<ZoneRegistry>, router: Arc<ConnectionRouter>) -> Arc<Self> {
        Arc::new(Self {
            registry,
            router,
            on_handoff_committed: std::sync::RwLock::new(None),
            players: std::sync::RwLock::new(None),
        })
    }

    /// Give recovery access to player identities.
    pub fn set_player_directory(&self, players: Arc<PlayerDirectory>) {
        *self
            .players
            .write()
            .expect("player directory lock poisoned") = Some(players);
    }

    fn player_directory(&self) -> Option<Arc<PlayerDirectory>> {
        self.players
            .read()
            .expect("player directory lock poisoned")
            .clone()
    }

    pub fn set_handoff_committed_hook(&self, hook: HandoffCommittedHook) {
        *self
            .on_handoff_committed
            .write()
            .expect("hook lock poisoned") = Some(hook);
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
        let survivors = self.registry.survivors(Some(failed_zone)).await;

        let (assignments, unplaceable) = self.plan_rebalance(&orphaned, &survivors);
        let mut kicked = 0;
        for source in unplaceable {
            kick(source, "Server zone unavailable");
            self.router.remove(source);
            kicked += 1;
        }

        self.router.commit_batch(&assignments).await;
        let rerouted = assignments.len();
        // Routing alone leaves the player unknown to its new zone: no directory
        // entry, no `playerJoining`, invisible to every script there. Register
        // them for real. The dead zone's snapshot died with it, so this carries
        // only the identity the gateway itself holds — the ped respawns from
        // the client's next state update.
        self.activate_recovered(&assignments).await;

        metrics::counter!("zone_recovery_players_rerouted_total").increment(rerouted as u64);
        tracing::warn!(target: "gateway",
            "Rerouting {total} players from {failed_zone} — complete in {:.1}s ({rerouted} rerouted, {kicked} kicked)",
            started.elapsed().as_secs_f64());
        (rerouted, kicked)
    }

    /// Spread orphans across the survivors, respecting each zone's capacity.
    ///
    /// The tally starts from the router's own live counts and is incremented as
    /// assignments are made, so the balance reflects this burst rather than a
    /// heartbeat that is up to five seconds stale. Returns the assignments and
    /// the players no surviving zone had room for.
    fn plan_rebalance(
        &self,
        orphaned: &[u32],
        survivors: &[(String, u32)],
    ) -> (Vec<(u32, String)>, Vec<u32>) {
        if survivors.is_empty() {
            return (Vec::new(), orphaned.to_vec());
        }
        // (current load, capacity, zone), kept sorted by load as we assign.
        let mut load: Vec<(usize, usize, &str)> = survivors
            .iter()
            .map(|(zone, max_players)| {
                (
                    self.router.count_in_zone(zone),
                    *max_players as usize,
                    zone.as_str(),
                )
            })
            .collect();

        let mut assignments = Vec::with_capacity(orphaned.len());
        let mut unplaceable = Vec::new();
        for &source in orphaned {
            // Least loaded zone that still has room.
            let target = load
                .iter_mut()
                .filter(|(count, capacity, _)| count < capacity)
                .min_by_key(|(count, _, _)| *count);
            match target {
                Some((count, _, zone)) => {
                    assignments.push((source, (*zone).to_owned()));
                    *count += 1;
                }
                None => unplaceable.push(source),
            }
        }
        (assignments, unplaceable)
    }

    /// Register each recovered player in its new zone, concurrently.
    ///
    /// Bounded concurrency: a recovery can move thousands of players, and one
    /// round trip each, sequentially, would take longer than the reroute it is
    /// meant to complete.
    async fn activate_recovered(&self, assignments: &[(u32, String)]) {
        let Some(players) = self.player_directory() else {
            tracing::error!(
                target: "gateway",
                "no player directory wired: recovered players are routed but not \
                 registered in their new zone"
            );
            return;
        };
        futures::stream::iter(assignments.iter().cloned())
            .for_each_concurrent(RECOVERY_CONCURRENCY, |(source, zone)| {
                let registry = Arc::clone(&self.registry);
                let players = Arc::clone(&players);
                async move {
                    let Some(mut client) = registry.zone_client(&zone).await else {
                        return;
                    };
                    let info = players.get(source);
                    let snapshot = PlayerStateSnapshot {
                        source_id: source,
                        name: info.as_ref().map(|p| p.name.clone()).unwrap_or_default(),
                        identifiers: info.map(|p| p.identifiers).unwrap_or_default(),
                        coords: [0.0; 3],
                        heading: 0.0,
                        velocity: [0.0; 3],
                        health: 0.0,
                        armour: 0.0,
                        current_weapon: 0,
                        player_entity: None,
                        owned_entities: Vec::new(),
                        script_state: Default::default(),
                    };
                    let prepared = tokio::time::timeout(
                        HANDOFF_PREP_TIMEOUT,
                        client.prepare_for_player(PlayerStateRequest {
                            player_id: source,
                            from_zone: String::new(),
                            snapshot: snapshot.encode(),
                        }),
                    )
                    .await;
                    if !matches!(prepared, Ok(Ok(ref resp)) if resp.get_ref().ready) {
                        metrics::counter!("zone_recovery_activation_failures_total").increment(1);
                        return;
                    }
                    let activated = tokio::time::timeout(
                        HANDOFF_PREP_TIMEOUT,
                        client.activate_player(ActivatePlayerRequest { player_id: source }),
                    )
                    .await;
                    if !matches!(activated, Ok(Ok(ref resp)) if resp.get_ref().ok) {
                        tracing::warn!(
                            target: "gateway",
                            source,
                            zone = %zone,
                            "recovered player could not be activated in its new zone"
                        );
                        metrics::counter!("zone_recovery_activation_failures_total").increment(1);
                    }
                }
            })
            .await;
    }

    /// Drain a zone: reroute all its players to the least-loaded other zone.
    /// Returns how many players were rerouted. (Full state migration rides
    /// the D4 handoff path; routing moves immediately so the zone empties.)
    pub async fn drain_zone(&self, zone_id: &str) -> usize {
        let players = self.router.players_in_zone(zone_id);
        let mut moved = 0;
        for source in players {
            let Some(fallback) = self
                .registry
                .find_least_loaded_zone_excluding(Some(zone_id))
                .await
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
            .register_zone(
                &req.zone_id,
                bounds,
                &req.grpc_addr,
                req.max_players.max(0) as u32,
            )
            .await
        {
            Ok(()) => Ok(Response::new(RegisterZoneResponse {
                accepted: true,
                message: String::new(),
            })),
            Err(e) => Ok(Response::new(RegisterZoneResponse {
                accepted: false,
                message: e,
            })),
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
        let target_zone_grpc = self
            .mesh
            .registry
            .zone_grpc_addr(&target_zone)
            .await
            .unwrap_or_default();

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
        self.mesh
            .router
            .commit_handoff(req.player_id, &req.to_zone)
            .await;
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
        Ok(Response::new(ConfirmHandoffResponse {
            ok: true,
            message: String::new(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mesh_with(routed: &[(u32, &str)]) -> Arc<GatewayMesh> {
        let registry = Arc::new(ZoneRegistry::new(Duration::from_secs(15)));
        let router = Arc::new(ConnectionRouter::new());
        for (source, zone) in routed {
            router.assign(*source, zone);
        }
        GatewayMesh::new(registry, router)
    }

    fn survivors(zones: &[(&str, u32)]) -> Vec<(String, u32)> {
        zones
            .iter()
            .map(|(id, capacity)| ((*id).to_owned(), *capacity))
            .collect()
    }

    fn placed_in(assignments: &[(u32, String)], zone: &str) -> usize {
        assignments.iter().filter(|(_, z)| z == zone).count()
    }

    /// The bug this replaces: every orphan separately asked the registry for
    /// the "least loaded" zone, whose player count only refreshes on a
    /// five-second heartbeat — so the entire burst piled onto one zone.
    #[test]
    fn orphans_are_spread_instead_of_piling_onto_one_zone() {
        let mesh = mesh_with(&[]);
        let orphans: Vec<u32> = (1..=100).collect();

        let (assignments, unplaceable) =
            mesh.plan_rebalance(&orphans, &survivors(&[("a", 1000), ("b", 1000)]));

        assert!(unplaceable.is_empty());
        assert_eq!(assignments.len(), 100);
        assert_eq!(placed_in(&assignments, "a"), 50);
        assert_eq!(placed_in(&assignments, "b"), 50);
    }

    /// Balancing starts from what each zone already holds, not from zero.
    #[test]
    fn existing_load_is_taken_into_account() {
        let already: Vec<(u32, &str)> = (500..530).map(|source| (source, "a")).collect();
        let mesh = mesh_with(&already);
        let orphans: Vec<u32> = (1..=30).collect();

        let (assignments, _) =
            mesh.plan_rebalance(&orphans, &survivors(&[("a", 1000), ("b", 1000)]));

        // "a" starts 30 ahead, so every orphan goes to "b" until they level up.
        assert_eq!(placed_in(&assignments, "b"), 30);
        assert_eq!(placed_in(&assignments, "a"), 0);
    }

    /// A survivor must not be pushed past its configured capacity; whoever
    /// does not fit is reported so the caller kicks them explicitly instead of
    /// silently overfilling a zone.
    #[test]
    fn capacity_is_respected_and_the_overflow_is_reported() {
        let mesh = mesh_with(&[]);
        let orphans: Vec<u32> = (1..=10).collect();

        let (assignments, unplaceable) =
            mesh.plan_rebalance(&orphans, &survivors(&[("a", 4), ("b", 4)]));

        assert_eq!(assignments.len(), 8);
        assert_eq!(placed_in(&assignments, "a"), 4);
        assert_eq!(placed_in(&assignments, "b"), 4);
        assert_eq!(unplaceable.len(), 2, "the rest must be kicked, not dropped");
    }

    #[test]
    fn without_a_survivor_every_player_is_unplaceable() {
        let mesh = mesh_with(&[]);
        let orphans = vec![1, 2, 3];

        let (assignments, unplaceable) = mesh.plan_rebalance(&orphans, &survivors(&[]));

        assert!(assignments.is_empty());
        assert_eq!(unplaceable, orphans);
    }
}
