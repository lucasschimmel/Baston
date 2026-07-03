//! Boundary scan loop (jalon D3/D4): every ~500ms, examine player kinematics,
//! prepare handoffs for players approaching an edge, cancel for players who
//! turned around, and confirm + activate + release for players who crossed.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use baston_protocol::mesh::{ActivatePlayerRequest, ReleasePlayerRequest};
use baston_protocol::{PlayerDirectory, PlayerStateSnapshot};

use crate::boundary_detector::BoundaryDetector;
use crate::handoff_manager::{HandoffManager, HandoffState};
use crate::mesh::ZoneMesh;
use crate::state_ingest::StateIngest;

/// Callback collecting zone-transferable script state for a player
/// (`RegisterZoneTransferState`, jalon D4). Returns resource → JSON text.
/// Async: the collection round-trips through every resource isolate.
pub type ScriptStateCollector = Arc<
    dyn Fn(u32) -> Pin<Box<dyn Future<Output = std::collections::HashMap<String, String>> + Send>>
        + Send
        + Sync,
>;

/// Callback running the local cleanup after a completed handoff (internal
/// playerDropped, entity release) — zone side of the D4 sequence, step 5.
pub type PostHandoffCleanup = Arc<dyn Fn(u32) + Send + Sync>;

pub struct BoundaryLoop {
    pub detector: BoundaryDetector,
    pub manager: Arc<HandoffManager>,
    pub mesh: Arc<ZoneMesh>,
    pub ingest: Arc<StateIngest>,
    pub players: Arc<PlayerDirectory>,
    pub scan_interval: Duration,
    pub collect_script_state: ScriptStateCollector,
    pub post_handoff_cleanup: PostHandoffCleanup,
}

impl BoundaryLoop {
    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(self.scan_interval);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                self.scan_once().await;
                // Ghosts prepared for players who never crossed expire after
                // a while so the neighbour zone doesn't accumulate them.
                self.mesh.expire_stale_ghosts(Duration::from_secs(30));
            }
        })
    }

    pub async fn scan_once(&self) {
        for (source, coords, velocity) in self.ingest.player_kinematics() {
            let candidate = self.detector.check_player(source, coords, velocity);
            let crossed = !self.mesh.bounds.contains(coords[0], coords[1]);
            match self.manager.state_of(source) {
                None => {
                    if let Some(c) = candidate {
                        tracing::info!(target: "zone", zone = %self.mesh.zone_id,
                            "player={source} approaching {:?} boundary ({:.0}m, ETA {:.1}s)",
                            c.target_direction, c.distance_to_edge,
                            c.estimated_crossing_ms as f64 / 1000.0);
                        let snapshot = self.build_snapshot(source, coords, velocity).await;
                        self.manager.request_handoff(source, c.predicted_coords, snapshot).await;
                    }
                }
                Some(HandoffState::ReadyToTransfer { .. }) => {
                    if crossed {
                        self.complete_handoff(source).await;
                    } else if candidate.is_none() {
                        // Turned around inside the margin: cancel preparation.
                        tracing::info!(target: "zone", zone = %self.mesh.zone_id,
                            "player={source} turned around — handoff cancelled");
                        self.manager.cancel(source);
                    }
                }
                Some(HandoffState::PreparationRequested { .. })
                | Some(HandoffState::Transferring) => {} // in flight
            }
        }
    }

    /// D4 sequence, zone-A side: ConfirmHandoff (Gateway, atomic) →
    /// ActivatePlayer (Zone B, via Gateway's registry → our gRPC client) →
    /// local cleanup (internal playerDropped, never fired to the network).
    async fn complete_handoff(&self, source: u32) {
        let started = std::time::Instant::now();
        tracing::info!(target: "zone", zone = %self.mesh.zone_id,
            "player={source} crossing boundary");
        let Some((target_zone, target_grpc)) = self.manager.confirm_crossing(source).await
        else {
            return; // confirm failed — player stays here, cooldown applies
        };

        // Activate the ghost in the target zone. The zone address came back
        // in the Gateway's prepare response; we reach Zone B directly.
        match self.mesh.peer_client(&target_grpc) {
            Ok(mut client) => {
                match tokio::time::timeout(
                    Duration::from_secs(2),
                    client.activate_player(ActivatePlayerRequest { player_id: source }),
                )
                .await
                {
                    Ok(Ok(resp)) if resp.get_ref().ok => {}
                    Ok(Ok(resp)) => {
                        tracing::error!(target: "zone",
                            "ActivatePlayer refused for player={source}: {}",
                            resp.get_ref().message);
                    }
                    Ok(Err(status)) => {
                        tracing::error!(target: "zone", error = %status, zone = %target_zone,
                            "ActivatePlayer gRPC failed for player={source}");
                        metrics::counter!("handoff_activate_errors_total").increment(1);
                    }
                    Err(_) => {
                        tracing::error!(target: "zone",
                            "ActivatePlayer timed out for player={source}");
                    }
                }
            }
            Err(e) => {
                tracing::error!(target: "zone",
                    "no gRPC client for target zone {target_zone} ({e}) — cannot activate player={source}");
            }
        }

        // Local cleanup: internal playerDropped + release owned entities.
        (self.post_handoff_cleanup)(source);
        let total = started.elapsed();
        metrics::histogram!("handoff_total_duration_ms").record(total.as_secs_f64() * 1000.0);
        tracing::info!(target: "zone", zone = %self.mesh.zone_id,
            "player={source} released (playerDropped internal) — handoff complete: {}ms",
            total.as_millis());
    }

    async fn build_snapshot(
        &self,
        source: u32,
        coords: [f32; 3],
        velocity: [f32; 3],
    ) -> PlayerStateSnapshot {
        let info = self.players.get(source);
        let entity = self
            .ingest
            .player_entity(source)
            .and_then(|id| self.ingest.entity_manager().get(id));
        let (health, armour) = entity.map(|e| (e.health, e.armour)).unwrap_or((200.0, 0.0));
        // The player's own ped entity travels via the snapshot's spatial
        // fields; owned_entities carries everything else (vehicle, ...).
        let player_entity_id = self.ingest.player_entity(source);
        let owned_entities = self
            .ingest
            .entity_manager()
            .entities_owned_by(source)
            .into_iter()
            .filter(|e| Some(e.entity_id) != player_entity_id)
            .collect();
        PlayerStateSnapshot {
            source_id: source,
            name: info.as_ref().map(|p| p.name.clone()).unwrap_or_default(),
            identifiers: info.map(|p| p.identifiers).unwrap_or_default(),
            coords,
            heading: 0.0,
            velocity,
            health,
            armour,
            current_weapon: 0,
            owned_entities,
            script_state: (self.collect_script_state)(source).await,
        }
    }
}
