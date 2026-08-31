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

/// Handoff preparations and completions run concurrently within one boundary
/// scan. Bounded so a mass crossing cannot open an unbounded number of
/// simultaneous gRPC calls against a neighbour zone.
const HANDOFF_SCAN_CONCURRENCY: usize = 16;

/// Side of the grid used to batch zone-resolution lookups, in metres.
///
/// Small enough that one answer is valid for every entity in the cell (zones
/// are kilometres wide), large enough that a cluster of departing entities
/// collapses into a single lookup.
const ZONE_RESOLVE_CELL_M: f32 = 64.0;
/// Concurrent zone-resolution round trips.
const ZONE_RESOLVE_CONCURRENCY: usize = 16;
/// Budget for one resolution round trip. A wedged gateway must slow the scan,
/// not stop it forever.
const ZONE_RESOLVE_TIMEOUT: Duration = Duration::from_secs(2);

/// Grid cell a position falls into, for batching zone lookups.
fn resolution_cell(coords: [f32; 3]) -> (i32, i32) {
    (
        (coords[0] / ZONE_RESOLVE_CELL_M).floor() as i32,
        (coords[1] / ZONE_RESOLVE_CELL_M).floor() as i32,
    )
}

/// Centre of a cell — the point actually asked about.
fn resolution_cell_centre(cell: (i32, i32)) -> (f32, f32) {
    (
        (cell.0 as f32 + 0.5) * ZONE_RESOLVE_CELL_M,
        (cell.1 as f32 + 0.5) * ZONE_RESOLVE_CELL_M,
    )
}

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
        use futures::StreamExt;

        self.manager.prune_expired();

        // Decide first — the detector and the handoff state machine are cheap
        // and synchronous — then do the slow work. Preparing a handoff costs a
        // round trip through every resource isolate plus a gRPC call with a
        // two-second timeout; doing that inline, player by player, means a
        // convoy reaching a boundary together is served one at a time and the
        // last player's handoff starts seconds after the first crossed.
        let mut to_prepare = Vec::new();
        let mut to_complete = Vec::new();
        // Taken once per pass: the territory only changes on re-registration,
        // and a polygon is not worth cloning per player.
        let coverage = self.mesh.coverage();
        for (source, coords, velocity) in self.ingest.player_kinematics() {
            let candidate = self
                .detector
                .check_player(&coverage, source, coords, velocity);
            let crossed = !coverage.contains(coords[0], coords[1]);
            match self.manager.state_of(source) {
                None => {
                    if let Some(c) = candidate {
                        tracing::info!(target: "zone", zone = %self.mesh.zone_id,
                            "player={source} leaving our ground toward ({:.0}, {:.0}) \
                             ({:.0}m, ETA {:.1}s)",
                            c.predicted_coords.0, c.predicted_coords.1, c.distance_to_edge,
                            c.estimated_crossing_ms as f64 / 1000.0);
                        to_prepare.push((source, coords, velocity, c.predicted_coords));
                    }
                }
                Some(HandoffState::ReadyToTransfer { .. }) => {
                    if crossed {
                        to_complete.push(source);
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

        // Crossings first: a player who already left our bounds is the one
        // whose latency the client actually feels.
        futures::stream::iter(to_complete)
            .for_each_concurrent(HANDOFF_SCAN_CONCURRENCY, |source| async move {
                self.complete_handoff(source).await;
            })
            .await;

        futures::stream::iter(to_prepare)
            .for_each_concurrent(
                HANDOFF_SCAN_CONCURRENCY,
                |(source, coords, velocity, predicted)| async move {
                    let snapshot = self.build_snapshot(source, coords, velocity).await;
                    self.manager
                        .request_handoff(source, predicted, snapshot)
                        .await;
                },
            )
            .await;

        self.scan_entities().await;
    }

    /// Ownerless-entity handoff (D4): a vehicle/NPC whose coords left our
    /// bounds while its network owner is NOT a player being handed off is
    /// migrated over NATS: `baston.handoff.entity.{target_zone}`.
    async fn scan_entities(&self) {
        let Some(nats) = &self.nats else { return };
        let local_players: std::collections::HashSet<u32> = self
            .ingest
            .player_kinematics()
            .iter()
            .map(|(s, _, _)| *s)
            .collect();
        // Only entities that actually left our territory are candidates, so
        // the world is filtered before it is cloned rather than after.
        let coverage = self.mesh.coverage();
        let departed = self.ingest.entity_manager().snapshot_filtered(|entity| {
            // Players ride the gRPC handoff path.
            entity.entity_type != EntityType::Player
                && !coverage.contains(entity.coords[0], entity.coords[1])
                // Entities owned by a connected local player travel inside
                // that player's snapshot instead.
                && !entity
                    .network_owner
                    .is_some_and(|owner| local_players.contains(&owner))
        });
        if departed.is_empty() {
            return;
        }

        let zone_of_cell = self.resolve_zones(nats, &departed).await;

        for entity in departed {
            let Some(target) = zone_of_cell.get(&resolution_cell(entity.coords)) else {
                continue; // unresolved or no zone covers it — keep it here
            };
            if *target == self.mesh.zone_id {
                continue;
            }
            let target = target.clone();
            let mut entity_for_b = entity.clone();
            entity_for_b.network_owner = None; // target zone reassigns
            let payload = EntityHandoffPayload {
                entity: entity_for_b,
                from_zone: self.mesh.zone_id.clone(),
            };
            match bincode::serde::encode_to_vec(&payload, bincode::config::standard()) {
                Ok(bytes) => {
                    if let Err(e) = nats
                        .publish(entity_handoff_subject(&target), bytes.into())
                        .await
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

    /// Resolve which zone covers each departed entity, one round trip per
    /// distinct grid cell rather than one per entity.
    ///
    /// Entities cross a boundary in clusters — a convoy, a car park drifting
    /// over the edge — so their positions collapse onto a handful of cells.
    /// Asking the gateway once per cell, concurrently, turns what used to be
    /// hundreds of sequential blocking request/replies every 500 ms into a few
    /// parallel ones.
    async fn resolve_zones(
        &self,
        nats: &async_nats::Client,
        departed: &[EntityState],
    ) -> std::collections::HashMap<(i32, i32), String> {
        use futures::StreamExt;

        let cells: std::collections::HashSet<(i32, i32)> = departed
            .iter()
            .map(|entity| resolution_cell(entity.coords))
            .collect();
        metrics::histogram!("entity_handoff_zone_lookups").record(cells.len() as f64);

        futures::stream::iter(cells)
            .map(|cell| async move {
                // Query the cell centre: every entity mapped to this cell is
                // within half a cell of it, and cells are far smaller than a
                // zone, so the answer holds for all of them.
                let (x, y) = resolution_cell_centre(cell);
                let reply = tokio::time::timeout(
                    ZONE_RESOLVE_TIMEOUT,
                    nats.request(RESOLVE_ZONE_SUBJECT, format!("{x},{y}").into()),
                )
                .await;
                match reply {
                    Ok(Ok(reply)) if !reply.payload.is_empty() => {
                        Some((cell, String::from_utf8_lossy(&reply.payload).into_owned()))
                    }
                    Ok(Ok(_)) => None, // no zone covers it
                    Ok(Err(e)) => {
                        tracing::warn!(target: "zone", error = %e, "zone resolution failed");
                        None
                    }
                    Err(_) => {
                        tracing::warn!(target: "zone", "zone resolution timed out");
                        None
                    }
                }
            })
            .buffer_unordered(ZONE_RESOLVE_CONCURRENCY)
            .filter_map(|resolved| async move { resolved })
            .collect()
            .await
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
        let Some((target_zone, target_grpc)) = self.manager.confirm_crossing(source).await else {
            return; // confirm failed — player stays here, cooldown applies
        };

        // Activate the ghost in the target zone. The zone address came back
        // in the Gateway's prepare response; we reach Zone B directly.
        let activated = self
            .activate_on_target(source, &target_zone, &target_grpc)
            .await;

        if !activated {
            // The route already points at a zone that never woke the player.
            // Releasing locally too would orphan it: known to nobody, updates
            // landing nowhere. Roll the route back and keep the player here.
            metrics::counter!("handoff_activate_failures_total").increment(1);
            self.manager.rollback_crossing(source).await;
            tracing::warn!(target: "zone", zone = %self.mesh.zone_id,
                "player={source} kept locally: activation on {target_zone} failed");
            return;
        }

        // Local cleanup: internal playerDropped + release owned entities.
        (self.post_handoff_cleanup)(source);
        let total = started.elapsed();
        metrics::histogram!("handoff_total_duration_ms").record(total.as_secs_f64() * 1000.0);
        tracing::info!(target: "zone", zone = %self.mesh.zone_id,
            "player={source} released (playerDropped internal) — handoff complete: {}ms",
            total.as_millis());
    }

    /// Wake the pre-loaded ghost in the target zone. Returns whether the
    /// player is now live there; every failure mode is a `false`, because the
    /// caller must not release local state on anything short of success.
    async fn activate_on_target(&self, source: u32, target_zone: &str, target_grpc: &str) -> bool {
        let mut client = match self.mesh.peer_client(target_grpc) {
            Ok(client) => client,
            Err(e) => {
                tracing::error!(target: "zone",
                    "no gRPC client for target zone {target_zone} ({e}) — cannot activate player={source}");
                return false;
            }
        };
        match tokio::time::timeout(
            Duration::from_secs(2),
            client.activate_player(ActivatePlayerRequest { player_id: source }),
        )
        .await
        {
            Ok(Ok(resp)) if resp.get_ref().ok => true,
            Ok(Ok(resp)) => {
                tracing::error!(target: "zone",
                    "ActivatePlayer refused for player={source}: {}",
                    resp.get_ref().message);
                false
            }
            Ok(Err(status)) => {
                tracing::error!(target: "zone", error = %status, zone = %target_zone,
                    "ActivatePlayer gRPC failed for player={source}");
                metrics::counter!("handoff_activate_errors_total").increment(1);
                false
            }
            Err(_) => {
                tracing::error!(target: "zone",
                    "ActivatePlayer timed out for player={source}");
                false
            }
        }
    }

    async fn build_snapshot(
        &self,
        source: u32,
        coords: [f32; 3],
        velocity: [f32; 3],
    ) -> PlayerStateSnapshot {
        let info = self.players.get(source);
        let player_entity_id = self.ingest.player_entity(source);
        let player_entity = player_entity_id.and_then(|id| self.ingest.entity_manager().get(id));
        let (health, armour) = player_entity
            .as_ref()
            .map(|e| (e.health, e.armour))
            .unwrap_or((200.0, 0.0));
        // The ped travels as a full entity; owned_entities carries everything
        // else the player is authoritative for (vehicle, ...).
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
            heading: player_entity.as_ref().map_or(0.0, |e| e.heading),
            velocity,
            health,
            armour,
            current_weapon: 0,
            player_entity,
            owned_entities,
            script_state: (self.collect_script_state)(source).await,
        }
    }
}
