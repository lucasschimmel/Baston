//! Client → Zone state-update ingestion (jalon C2).
//!
//! Funnels both ingestion paths into one validated `EntityManager` write:
//! the binary `msgBastonState` packet (loadtest clients) and the
//! `__baston:stateUpdate` net event from the real-client shim.
//!
//! Basic Phase C anti-cheat: reject updates implying a displacement speed
//! above `state_sync.max_speed_mps` (teleport / speed hack).

use std::time::Instant;

use baston_protocol::entity::{
    distance3d, new_entity_id, EntityId, EntityState, EntityStateUpdate, EntityType,
};
use baston_protocol::udp::state::ClientStateUpdate;
use dashmap::DashMap;

use crate::entity_manager::EntityManager;
use std::sync::Arc;

/// Why an update was rejected.
#[derive(Debug, PartialEq, Eq)]
pub enum RejectReason {
    /// Displacement speed above the configured ceiling.
    Teleport,
    /// The sender does not own the target entity.
    NotOwner,
    /// Update referenced an entity that does not exist.
    UnknownEntity,
}

pub struct StateIngest {
    entity_manager: Arc<EntityManager>,
    /// Player source → its player-ped entity.
    player_entities: DashMap<u32, EntityId>,
    /// Last accepted (time, coords) per entity, for the speed check.
    last_accepted: DashMap<EntityId, (Instant, [f32; 3])>,
    /// Sources that talk the binary BASTON state protocol (loadtest /
    /// headless clients). Only they receive `msgBastonSnapshot` pushes —
    /// real FiveM clients get their entity sync via the msgRoute P2P relay.
    snapshot_subscribers: DashMap<u32, ()>,
    max_speed_mps: f32,
}

impl StateIngest {
    pub fn new(entity_manager: Arc<EntityManager>, max_speed_mps: f32) -> Self {
        Self {
            entity_manager,
            player_entities: DashMap::new(),
            last_accepted: DashMap::new(),
            snapshot_subscribers: DashMap::new(),
            max_speed_mps,
        }
    }

    pub fn entity_manager(&self) -> &Arc<EntityManager> {
        &self.entity_manager
    }

    /// The player-ped entity of a source, if it reported state already.
    pub fn player_entity(&self, source: u32) -> Option<EntityId> {
        self.player_entities.get(&source).map(|e| *e)
    }

    /// Apply a validated client state update coming from `source`.
    pub fn apply(&self, source: u32, update: ClientStateUpdate) -> Result<EntityId, RejectReason> {
        let entity_id = match update.entity_id {
            // "my player ped": resolve or lazily create the player entity.
            None => match self.player_entities.get(&source) {
                Some(id) => *id,
                None => return Ok(self.spawn_player_entity(source, &update)),
            },
            Some(id) => {
                let Some(state) = self.entity_manager.get(id) else {
                    return Err(RejectReason::UnknownEntity);
                };
                if state.network_owner != Some(source) {
                    metrics::counter!("state_updates_rejected", "reason" => "not_owner")
                        .increment(1);
                    return Err(RejectReason::NotOwner);
                }
                id
            }
        };

        if let Some((at, from)) = self.last_accepted.get(&entity_id).map(|e| *e) {
            let dt = at.elapsed().as_secs_f32().max(1e-3);
            let speed = distance3d(from, update.coords) / dt;
            if speed > self.max_speed_mps {
                tracing::warn!(
                    target: "zone",
                    source,
                    %entity_id,
                    speed_mps = speed,
                    limit = self.max_speed_mps,
                    "state update rejected: implausible displacement (teleport?)"
                );
                metrics::counter!("state_updates_rejected", "reason" => "teleport").increment(1);
                return Err(RejectReason::Teleport);
            }
        }
        self.last_accepted
            .insert(entity_id, (Instant::now(), update.coords));

        self.entity_manager.update_entity(
            entity_id,
            EntityStateUpdate {
                coords: Some(update.coords),
                heading: Some(update.heading),
                velocity: Some(update.velocity),
                health: Some(update.health),
                armour: Some(update.armour),
                extra: update.extra,
            },
        );
        metrics::counter!("state_updates_accepted").increment(1);
        Ok(entity_id)
    }

    fn spawn_player_entity(&self, source: u32, update: &ClientStateUpdate) -> EntityId {
        let id = new_entity_id();
        self.entity_manager.spawn_entity(EntityState {
            entity_id: id,
            entity_type: EntityType::Player,
            network_owner: Some(source),
            model_hash: update.model_hash,
            coords: update.coords,
            heading: update.heading,
            velocity: update.velocity,
            health: update.health,
            armour: update.armour,
            extra: update.extra.clone().unwrap_or(
                baston_protocol::entity::EntityExtra::Player {
                    is_in_vehicle: false,
                    vehicle_id: None,
                },
            ),
        });
        self.player_entities.insert(source, id);
        self.last_accepted
            .insert(id, (Instant::now(), update.coords));
        tracing::info!(target: "zone", source, entity_id = %id, "player entity spawned");
        id
    }

    /// Opt a source into `msgBastonSnapshot` pushes (binary-protocol client).
    pub fn mark_snapshot_subscriber(&self, source: u32) {
        self.snapshot_subscribers.insert(source, ());
    }

    pub fn is_snapshot_subscriber(&self, source: u32) -> bool {
        self.snapshot_subscribers.contains_key(&source)
    }

    /// Player left: despawn its ped entity and forget its history.
    pub fn on_player_dropped(&self, source: u32) {
        if let Some((_, id)) = self.player_entities.remove(&source) {
            self.entity_manager.remove_entity(id);
            self.last_accepted.remove(&id);
        }
        self.snapshot_subscribers.remove(&source);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(coords: [f32; 3]) -> ClientStateUpdate {
        ClientStateUpdate {
            entity_id: None,
            entity_type: EntityType::Player,
            model_hash: 1,
            coords,
            heading: 0.0,
            velocity: [0.0; 3],
            health: 100.0,
            armour: 0.0,
            extra: None,
        }
    }

    fn ingest() -> StateIngest {
        StateIngest::new(Arc::new(EntityManager::new()), 200.0)
    }

    #[test]
    fn first_update_spawns_player_entity() {
        let ingest = ingest();
        let id = ingest.apply(1, sample([0.0; 3])).unwrap();
        assert_eq!(ingest.player_entity(1), Some(id));
        assert_eq!(ingest.entity_manager().count(), 1);
        let dirty = ingest.entity_manager().flush_dirty();
        assert_eq!(dirty.len(), 1);
        assert!(dirty[0]
            .dirty_fields
            .contains(baston_protocol::entity::DirtyFlags::CREATED));
    }

    #[test]
    fn plausible_movement_is_accepted() {
        let ingest = ingest();
        let id = ingest.apply(1, sample([0.0; 3])).unwrap();
        // Even with dt≈0, 0.1m over the clamped 1ms floor stays under 200 m/s.
        let id2 = ingest.apply(1, sample([0.1, 0.0, 0.0])).unwrap();
        assert_eq!(id, id2);
    }

    #[test]
    fn teleport_is_rejected() {
        let ingest = ingest();
        ingest.apply(1, sample([0.0; 3])).unwrap();
        // 5km instantly — far above 200 m/s whatever the elapsed time.
        let err = ingest.apply(1, sample([5000.0, 0.0, 0.0])).unwrap_err();
        assert_eq!(err, RejectReason::Teleport);
        // State unchanged.
        let id = ingest.player_entity(1).unwrap();
        assert_eq!(ingest.entity_manager().get(id).unwrap().coords, [0.0; 3]);
    }

    #[test]
    fn cannot_update_entity_owned_by_other() {
        let ingest = ingest();
        let victim = ingest.apply(1, sample([0.0; 3])).unwrap();
        let mut update = sample([1.0, 0.0, 0.0]);
        update.entity_id = Some(victim);
        assert_eq!(ingest.apply(2, update).unwrap_err(), RejectReason::NotOwner);
    }

    #[test]
    fn drop_despawns_player_entity() {
        let ingest = ingest();
        ingest.apply(1, sample([0.0; 3])).unwrap();
        ingest.entity_manager().flush_dirty();
        ingest.on_player_dropped(1);
        assert_eq!(ingest.entity_manager().count(), 0);
        let dirty = ingest.entity_manager().flush_dirty();
        assert_eq!(dirty.len(), 1);
        assert!(dirty[0]
            .dirty_fields
            .contains(baston_protocol::entity::DirtyFlags::DELETED));
    }
}
