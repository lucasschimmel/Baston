//! Boundary scan loop (jalon D3/D4): every ~500ms, examine player kinematics,
//! prepare handoffs for players approaching an edge, cancel for players who
//! turned around, and confirm + activate + release for players who crossed.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use baston_protocol::mesh::ActivatePlayerRequest;
use baston_protocol::{PlayerDirectory, PlayerStateSnapshot};

use baston_protocol::entity::{EntityState, EntityType};
use serde::{Deserialize, Serialize};

use crate::boundary_detector::BoundaryDetector;
use crate::handoff_manager::{HandoffManager, HandoffState};
use crate::mesh::ZoneMesh;
use crate::state_ingest::StateIngest;

/// NATS payload for ownerless entities crossing a boundary (vehicles, NPCs).
#[derive(Debug, Serialize, Deserialize)]
pub struct EntityHandoffPayload {
    pub entity: EntityState,
    pub from_zone: String,
}

pub fn entity_handoff_subject(zone_id: &str) -> String {
    format!("baston.handoff.entity.{zone_id}")
}

/// Request/reply subject the Gateway answers with the zone covering "x,y".
pub const RESOLVE_ZONE_SUBJECT: &str = "baston.mesh.resolve_zone";

/// Broadcast subject for cross-zone script events (D5). Every zone publishes
/// its locally-triggered events here and dispatches those of its siblings.
pub const CROSS_ZONE_EVENT_SUBJECT: &str = "baston.cross-zone.event.broadcast";

/// Subject on which the Gateway publishes the global player list every ~2s.
pub const GLOBAL_PLAYERS_SUBJECT: &str = "baston.mesh.players";

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
    /// NATS client for entity handoffs (None disables entity migration).
    pub nats: Option<async_nats::Client>,
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
        self.scan_entities().await;
    }

    /// Ownerless-entity handoff (D4): a vehicle/NPC whose coords left our
    /// bounds while its network owner is NOT a player being handed off is
    /// migrated over NATS: `baston.handoff.entity.{target_zone}`.
    async fn scan_entities(&self) {
        let Some(nats) = &self.nats else { return };
        let local_players: std::collections::HashSet<u32> =
            self.ingest.player_kinematics().iter().map(|(s, _, _)| *s).collect();
        for entity in self.ingest.entity_manager().snapshot() {
            if entity.entity_type == EntityType::Player {
                continue; // players ride the gRPC handoff path
            }
            let (x, y) = (entity.coords[0], entity.coords[1]);
            if self.mesh.bounds.contains(x, y) {
                continue;
            }
            // Entities owned by a connected local player travel inside that
            // player's snapshot instead.
            if entity.network_owner.is_some_and(|o| local_players.contains(&o)) {
                continue;
            }
            // Ask the Gateway which zone covers the entity's position.
            let target = match nats
                .request(RESOLVE_ZONE_SUBJECT, format!("{x},{y}").into())
                .await
            {
                Ok(reply) if !reply.payload.is_empty() => {
                    String::from_utf8_lossy(&reply.payload).to_string()
                }
                Ok(_) => continue,  // no zone covers it — keep it here
                Err(e) => {
                    tracing::warn!(target: "zone", error = %e, "zone resolution failed");
                    continue;
                }
            };
            if target == self.mesh.zone_id {
                continue;
            }
            let mut entity_for_b = entity.clone();
            entity_for_b.network_owner = None; // target zone reassigns
            let payload = EntityHandoffPayload {
                entity: entity_for_b,
                from_zone: self.mesh.zone_id.clone(),
            };
            match bincode::serde::encode_to_vec(&payload, bincode::config::standard()) {
                Ok(bytes) => {
                    if let Err(e) =
                        nats.publish(entity_handoff_subject(&target), bytes.into()).await
                    {
                        tracing::error!(target: "zone", error = %e,
                            "entity handoff publish failed — entity kept locally");
                        continue;
                    }
                    self.ingest.entity_manager().remove_entity(entity.entity_id);
                    metrics::counter!("entity_handoffs_total").increment(1);
                    tracing::info!(target: "zone", zone = %self.mesh.zone_id,
                        "entity {} handed off → {target}", entity.entity_id);
                }
                Err(e) => {
                    tracing::error!(target: "zone", error = %e, "entity handoff encode failed");
                }
            }
        }
    }

    /// Consume inbound entity handoffs: spawn the entity locally.
    pub fn spawn_entity_handoff_consumer(
        nats: async_nats::Client,
        zone_id: String,
        entity_manager: Arc<crate::EntityManager>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            use futures::StreamExt;
            let subject = entity_handoff_subject(&zone_id);
            let mut sub = match nats.subscribe(subject.clone()).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(target: "zone", error = %e,
                        "entity handoff subscription failed");
                    return;
                }
            };
            tracing::info!(target: "zone", %subject, "entity handoff consumer running");
            while let Some(msg) = sub.next().await {
                match bincode::serde::decode_from_slice::<EntityHandoffPayload, _>(
                    &msg.payload,
                    bincode::config::standard(),
                ) {
                    Ok((payload, _)) => {
                        tracing::info!(target: "zone", zone = %zone_id,
                            "entity {} received from {}", payload.entity.entity_id, payload.from_zone);
                        entity_manager.spawn_entity(payload.entity);
                    }
                    Err(e) => {
                        tracing::error!(target: "zone", error = %e,
                            "malformed entity handoff payload");
                    }
                }
            }
        })
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
