//! StateAggregator: NATS → per-client UDP push (jalon C3).
//!
//! Two independent tasks:
//! - **subscriber**: durable JetStream pull consumer on
//!   `baston.zone.*.state`; merges `DirtyEntity` batches into `world_state`
//!   (last-write-wins per `entity_id`), applies DELETED markers, acks.
//! - **push loop**: every `push_interval_ms` (50ms = 20fps) builds a
//!   per-client `EntitySnapshot` limited to the client's area of interest
//!   and sends it over the game channel.
//!
//! The AoI filter is a brute-force O(n) scan — fine up to ~500 entities per
//! zone. Phase D+ replaces it with a quadtree spatial index.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use baston_protocol::entity::{distance3d, DirtyEntity, DirtyFlags, EntityId, EntityState};
use baston_protocol::udp::state::{build_snapshot, EntityOp, EntitySnapshot};
use baston_protocol::PlayerDirectory;
use baston_zone::state_sync::{setup_nats_stream, STATE_STREAM_NAME, STATE_SUBJECT_WILDCARD};
use baston_zone::StateIngest;
use dashmap::DashMap;
use futures::StreamExt;

use crate::udp::UdpHandle;

/// Update cadence per entity distance (jalon C5 variable rates).
pub fn update_rate_for_distance(dist: f32) -> Duration {
    match dist as u32 {
        0..=99 => Duration::from_millis(50),    // 20fps
        100..=299 => Duration::from_millis(100), // 10fps
        _ => Duration::from_millis(500),         // 2fps
    }
}

/// What the push loop knows about one connected client.
#[derive(Default)]
pub struct ClientEntityTracker {
    /// Entities the client has been sent a create for.
    known_entities: std::collections::HashSet<EntityId>,
    /// Last time an update for the entity was pushed (variable-rate skip).
    last_sent_at: HashMap<EntityId, Instant>,
}

pub struct StateAggregator {
    nats: async_nats::Client,
    world_state: Arc<DashMap<EntityId, EntityState>>,
    players: Arc<PlayerDirectory>,
    state_ingest: Arc<StateIngest>,
    udp: UdpHandle,
    aoi_radius: f32,
    push_interval: Duration,
    trackers: DashMap<u32, ClientEntityTracker>,
}

impl StateAggregator {
    pub fn new(
        nats: async_nats::Client,
        players: Arc<PlayerDirectory>,
        state_ingest: Arc<StateIngest>,
        udp: UdpHandle,
        aoi_radius: f32,
        push_interval_ms: u64,
    ) -> Self {
        Self {
            nats,
            world_state: Arc::new(DashMap::new()),
            players,
            state_ingest,
            udp,
            aoi_radius,
            push_interval: Duration::from_millis(push_interval_ms.max(1)),
            trackers: DashMap::new(),
        }
    }

    pub fn world_state(&self) -> Arc<DashMap<EntityId, EntityState>> {
        Arc::clone(&self.world_state)
    }

    /// Spawn subscriber + push loop. Consumes self into an `Arc` shared by
    /// both tasks.
    pub fn spawn(self) -> Arc<Self> {
        let this = Arc::new(self);
        tokio::spawn(Arc::clone(&this).run_subscriber());
        tokio::spawn(Arc::clone(&this).run_push_loop());
        this
    }

    async fn run_subscriber(self: Arc<Self>) {
        loop {
            if let Err(e) = self.subscribe_once().await {
                tracing::error!(target: "nats", error = %e, "StateAggregator subscription lost; retrying in 1s");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }

    async fn subscribe_once(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // The stream may not exist yet if the gateway won the startup race.
        setup_nats_stream(&self.nats).await?;
        let js = async_nats::jetstream::new(self.nats.clone());
        let stream = js.get_stream(STATE_STREAM_NAME).await?;
        let consumer = stream
            .create_consumer(async_nats::jetstream::consumer::pull::Config {
                durable_name: Some("baston-gateway".to_string()),
                filter_subject: STATE_SUBJECT_WILDCARD.to_string(),
                ..Default::default()
            })
            .await?;
        tracing::info!(
            target: "baston-gateway",
            "StateAggregator: subscribed to {STATE_SUBJECT_WILDCARD}"
        );
        let mut messages = consumer.messages().await?;
        while let Some(message) = messages.next().await {
            let message = message?;
            match bincode::serde::decode_from_slice::<Vec<DirtyEntity>, _>(
                &message.payload,
                bincode::config::standard(),
            ) {
                Ok((batch, _)) => self.merge_batch(batch),
                Err(e) => {
                    tracing::warn!(target: "nats", error = %e, "undecodable state batch dropped")
                }
            }
            // Ack regardless: a poison message must not redeliver forever.
            if let Err(e) = message.ack().await {
                tracing::warn!(target: "nats", error = %e, "state batch ack failed");
            }
        }
        Err("JetStream message stream ended".into())
    }

    /// Merge one zone batch into the aggregated world (last-write-wins).
    fn merge_batch(&self, batch: Vec<DirtyEntity>) {
        for dirty in batch {
            if dirty.dirty_fields.contains(DirtyFlags::DELETED) {
                self.world_state.remove(&dirty.entity_id);
            } else {
                self.world_state.insert(dirty.entity_id, dirty.state);
            }
        }
        metrics::gauge!("world_state_entities").set(self.world_state.len() as f64);
    }

    async fn run_push_loop(self: Arc<Self>) {
        let mut interval = tokio::time::interval(self.push_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut tick: u64 = 0;
        loop {
            interval.tick().await;
            tick += 1;
            for source in self.players.sources() {
                self.push_to_client(source, tick);
            }
            // Drop trackers of players that left.
            self.trackers
                .retain(|source, _| self.players.get(*source).is_some());
        }
    }

    /// Build and send one client's snapshot for this tick.
    fn push_to_client(&self, source: u32, tick: u64) {
        // The client's viewpoint is its own player entity; before its first
        // state report there is nothing to cull against, so skip.
        let Some(player_entity) = self.state_ingest.player_entity(source) else {
            return;
        };
        let Some(center) = self.world_state.get(&player_entity).map(|e| e.coords) else {
            return;
        };

        let mut tracker = self.trackers.entry(source).or_default();
        let ops = compute_client_ops(
            &self.world_state,
            &mut tracker,
            source,
            center,
            self.aoi_radius,
        );
        if ops.is_empty() {
            return;
        }
        // Creates/deletes must survive packet loss; pure updates are
        // superseded 50ms later anyway.
        let reliable = ops
            .iter()
            .any(|op| !matches!(op, EntityOp::Upsert(d) if !d.dirty_fields.contains(DirtyFlags::CREATED)));
        let entity_count = ops.len();
        let packet = build_snapshot(&EntitySnapshot { tick, ops });
        metrics::gauge!("entities_per_client", "source" => source.to_string())
            .set(entity_count as f64);
        metrics::counter!("snapshot_bytes_sent").increment(packet.len() as u64);
        tracing::trace!(
            target: "baston-gateway",
            source,
            entities_visible = entity_count,
            bytes = packet.len(),
            "push loop"
        );
        self.udp.send_to_source(source, 1, packet, reliable);
    }
}

/// Pure per-client op computation: AoI filter + enter/leave tracking +
/// per-distance rate limiting. Separated from the aggregator for testability.
pub fn compute_client_ops(
    world: &DashMap<EntityId, EntityState>,
    tracker: &mut ClientEntityTracker,
    viewer_source: u32,
    center: [f32; 3],
    radius: f32,
) -> Vec<EntityOp> {
    let now = Instant::now();
    let mut ops = Vec::new();
    let mut visible = std::collections::HashSet::new();

    for entry in world.iter() {
        let state = entry.value();
        // The network owner is the authority on its own entities — echoing
        // its state back would fight the local simulation.
        if state.network_owner == Some(viewer_source) {
            continue;
        }
        let dist = distance3d(center, state.coords);
        if dist > radius {
            continue;
        }
        visible.insert(state.entity_id);

        if tracker.known_entities.contains(&state.entity_id) {
            // Variable update rate by distance (jalon C5): skip until due.
            let due = tracker
                .last_sent_at
                .get(&state.entity_id)
                .is_none_or(|last| now.duration_since(*last) >= update_rate_for_distance(dist));
            if !due {
                continue;
            }
            tracker.last_sent_at.insert(state.entity_id, now);
            ops.push(EntityOp::Upsert(DirtyEntity {
                entity_id: state.entity_id,
                // Per-client field-level deltas are Phase D; a known entity
                // gets the full field set minus CREATED.
                dirty_fields: DirtyFlags::all() - DirtyFlags::CREATED - DirtyFlags::DELETED,
                state: state.clone(),
            }));
        } else {
            tracker.known_entities.insert(state.entity_id);
            tracker.last_sent_at.insert(state.entity_id, now);
            ops.push(EntityOp::Upsert(DirtyEntity {
                entity_id: state.entity_id,
                dirty_fields: DirtyFlags::all() - DirtyFlags::DELETED,
                state: state.clone(),
            }));
        }
    }

    // Entities the client knew that are gone (despawn) or out of AoI:
    // exactly one Delete, then forgotten.
    tracker.known_entities.retain(|id| {
        if visible.contains(id) {
            true
        } else {
            ops.push(EntityOp::Delete(*id));
            false
        }
    });
    tracker.last_sent_at.retain(|id, _| visible.contains(id));

    ops
}

#[cfg(test)]
mod tests {
    use super::*;
    use baston_protocol::entity::{new_entity_id, EntityExtra, EntityType};

    fn entity_at(coords: [f32; 3], owner: Option<u32>) -> EntityState {
        EntityState {
            entity_id: new_entity_id(),
            entity_type: EntityType::Player,
            network_owner: owner,
            model_hash: 1,
            coords,
            heading: 0.0,
            velocity: [0.0; 3],
            health: 100.0,
            armour: 0.0,
            extra: EntityExtra::Player {
                is_in_vehicle: false,
                vehicle_id: None,
            },
        }
    }

    fn world_with(entities: Vec<EntityState>) -> DashMap<EntityId, EntityState> {
        let world = DashMap::new();
        for e in entities {
            world.insert(e.entity_id, e);
        }
        world
    }

    const CENTER: [f32; 3] = [0.0, 0.0, 0.0];

    #[test]
    fn entity_inside_aoi_is_included() {
        let world = world_with(vec![entity_at([100.0, 0.0, 0.0], Some(2))]);
        let mut tracker = ClientEntityTracker::default();
        let ops = compute_client_ops(&world, &mut tracker, 1, CENTER, 450.0);
        assert_eq!(ops.len(), 1);
        assert!(matches!(&ops[0], EntityOp::Upsert(d) if d.dirty_fields.contains(DirtyFlags::CREATED)));
    }

    #[test]
    fn entity_outside_aoi_is_excluded() {
        let world = world_with(vec![entity_at([500.0, 0.0, 0.0], Some(2))]);
        let mut tracker = ClientEntityTracker::default();
        assert!(compute_client_ops(&world, &mut tracker, 1, CENTER, 450.0).is_empty());
    }

    #[test]
    fn own_entities_are_not_echoed() {
        let world = world_with(vec![entity_at([10.0, 0.0, 0.0], Some(1))]);
        let mut tracker = ClientEntityTracker::default();
        assert!(compute_client_ops(&world, &mut tracker, 1, CENTER, 450.0).is_empty());
    }

    #[test]
    fn entering_aoi_sends_create_then_updates() {
        let mut entity = entity_at([500.0, 0.0, 0.0], Some(2));
        let id = entity.entity_id;
        let world = world_with(vec![entity.clone()]);
        let mut tracker = ClientEntityTracker::default();
        assert!(compute_client_ops(&world, &mut tracker, 1, CENTER, 450.0).is_empty());

        // Entity moves into range → CREATED upsert.
        entity.coords = [100.0, 0.0, 0.0];
        world.insert(id, entity.clone());
        let ops = compute_client_ops(&world, &mut tracker, 1, CENTER, 450.0);
        assert_eq!(ops.len(), 1);
        assert!(matches!(&ops[0], EntityOp::Upsert(d) if d.dirty_fields.contains(DirtyFlags::CREATED)));

        // Still in range after the rate-limit window → plain update.
        std::thread::sleep(update_rate_for_distance(100.0));
        let ops = compute_client_ops(&world, &mut tracker, 1, CENTER, 450.0);
        assert_eq!(ops.len(), 1);
        assert!(
            matches!(&ops[0], EntityOp::Upsert(d) if !d.dirty_fields.contains(DirtyFlags::CREATED))
        );
    }

    #[test]
    fn leaving_aoi_sends_delete_exactly_once() {
        let mut entity = entity_at([100.0, 0.0, 0.0], Some(2));
        let id = entity.entity_id;
        let world = world_with(vec![entity.clone()]);
        let mut tracker = ClientEntityTracker::default();
        assert_eq!(compute_client_ops(&world, &mut tracker, 1, CENTER, 450.0).len(), 1);

        entity.coords = [1000.0, 0.0, 0.0];
        world.insert(id, entity);
        let ops = compute_client_ops(&world, &mut tracker, 1, CENTER, 450.0);
        assert_eq!(ops.len(), 1);
        assert!(matches!(ops[0], EntityOp::Delete(gone) if gone == id));

        // Second tick: nothing (delete only once).
        assert!(compute_client_ops(&world, &mut tracker, 1, CENTER, 450.0).is_empty());
    }

    #[test]
    fn rate_limit_by_distance() {
        // 350m away → 500ms cadence: an immediate second tick sends nothing.
        let world = world_with(vec![entity_at([350.0, 0.0, 0.0], Some(2))]);
        let mut tracker = ClientEntityTracker::default();
        assert_eq!(compute_client_ops(&world, &mut tracker, 1, CENTER, 450.0).len(), 1);
        assert!(compute_client_ops(&world, &mut tracker, 1, CENTER, 450.0).is_empty());

        // 50m away → 50ms cadence.
        assert_eq!(update_rate_for_distance(50.0), Duration::from_millis(50));
        assert_eq!(update_rate_for_distance(350.0), Duration::from_millis(500));
    }
}
