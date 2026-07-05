//! Network-ownership assignment (jalon C5).
//!
//! Every non-player entity is owned by the nearest active player: that
//! client's simulation is authoritative and its state reports are accepted.
//! Reassignment happens on a fixed cadence (`ownership_interval_secs`,
//! default 5s) which doubles as flip-flop damping — two players leapfrogging
//! around an entity can't steal it from each other more than once per scan.

use std::sync::Arc;
use std::time::Duration;

use baston_protocol::entity::{distance3d, EntityId, EntityState, EntityType};

use crate::entity_manager::EntityManager;
use crate::state_ingest::StateIngest;

/// Notified whenever an entity's owner changes; wired to
/// `TriggerEvent('onEntityOwnerChanged', entityId, newOwnerId)` in the
/// server binary.
pub type OwnerChangedCallback = Arc<dyn Fn(EntityId, Option<u32>) + Send + Sync>;

/// Pick the owner for an entity: the nearest player among `players`
/// (source, coords) pairs. `None` when no player is connected.
pub fn assign_network_owner(entity: &EntityState, players: &[(u32, [f32; 3])]) -> Option<u32> {
    players
        .iter()
        .min_by(|(_, a), (_, b)| {
            let da = distance3d(*a, entity.coords);
            let db = distance3d(*b, entity.coords);
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(source, _)| *source)
}

pub struct OwnershipMonitor {
    entity_manager: Arc<EntityManager>,
    ingest: Arc<StateIngest>,
    on_owner_changed: Option<OwnerChangedCallback>,
    interval: Duration,
}

impl OwnershipMonitor {
    pub fn new(
        entity_manager: Arc<EntityManager>,
        ingest: Arc<StateIngest>,
        interval_secs: u64,
        on_owner_changed: Option<OwnerChangedCallback>,
    ) -> Self {
        Self {
            entity_manager,
            ingest,
            on_owner_changed,
            interval: Duration::from_secs(interval_secs.max(1)),
        }
    }

    /// Periodic task: reassign ownership at most once per interval.
    pub async fn run(self) {
        let mut interval = tokio::time::interval(self.interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            self.scan();
        }
    }

    /// One reassignment pass. Public for tests and for immediate invocation
    /// on player disconnect.
    pub fn scan(&self) {
        let players = self.ingest.player_positions();
        for entity in self.entity_manager.snapshot() {
            // Player peds are always owned by their own client.
            if entity.entity_type == EntityType::Player {
                continue;
            }
            let owner_connected = entity
                .network_owner
                .is_some_and(|o| players.iter().any(|(s, _)| *s == o));
            let best = assign_network_owner(&entity, &players);
            // Reassign when the owner vanished, or when a closer player
            // exists (cadence-limited by the scan interval).
            if !owner_connected || best != entity.network_owner {
                if best == entity.network_owner {
                    continue;
                }
                if self
                    .entity_manager
                    .set_network_owner(entity.entity_id, best)
                {
                    tracing::info!(
                        target: "zone",
                        entity_id = %entity.entity_id,
                        old = ?entity.network_owner,
                        new = ?best,
                        "network owner reassigned"
                    );
                    if let Some(cb) = &self.on_owner_changed {
                        cb(entity.entity_id, best);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baston_protocol::entity::{new_entity_id, EntityExtra, EntityState};
    use baston_protocol::udp::state::ClientStateUpdate;

    fn vehicle_at(coords: [f32; 3], owner: Option<u32>) -> EntityState {
        EntityState {
            entity_id: new_entity_id(),
            entity_type: EntityType::Vehicle,
            network_owner: owner,
            model_hash: 0xA144_53E9, // adder
            coords,
            heading: 0.0,
            velocity: [0.0; 3],
            health: 1000.0,
            armour: 0.0,
            extra: EntityExtra::Vehicle {
                speed: 0.0,
                engine_health: 1000.0,
                doors_open: 0,
            },
        }
    }

    fn player_update(coords: [f32; 3]) -> ClientStateUpdate {
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

    #[test]
    fn nearest_player_wins() {
        let vehicle = vehicle_at([100.0, 0.0, 0.0], None);
        let players = vec![(1, [0.0, 0.0, 0.0]), (2, [90.0, 0.0, 0.0])];
        assert_eq!(assign_network_owner(&vehicle, &players), Some(2));
        assert_eq!(assign_network_owner(&vehicle, &[]), None);
    }

    #[test]
    fn disconnected_owner_is_replaced_by_nearest() {
        let mgr = Arc::new(EntityManager::new());
        let ingest = Arc::new(StateIngest::new(Arc::clone(&mgr), 200.0));
        // Players 1 (far) and 2 (near the vehicle).
        ingest.apply(1, player_update([1000.0, 0.0, 0.0])).unwrap();
        ingest.apply(2, player_update([10.0, 0.0, 0.0])).unwrap();
        // Vehicle owned by (now-gone) player 3.
        let vehicle = vehicle_at([0.0, 0.0, 0.0], Some(3));
        let vid = vehicle.entity_id;
        mgr.spawn_entity(vehicle);

        type ChangeLog = Arc<std::sync::Mutex<Vec<(EntityId, Option<u32>)>>>;
        let changes: ChangeLog = Default::default();
        let sink = Arc::clone(&changes);
        let monitor = OwnershipMonitor::new(
            Arc::clone(&mgr),
            ingest,
            5,
            Some(Arc::new(move |id, owner| {
                sink.lock().unwrap().push((id, owner))
            })),
        );
        monitor.scan();

        assert_eq!(mgr.get(vid).unwrap().network_owner, Some(2));
        assert_eq!(*changes.lock().unwrap(), vec![(vid, Some(2))]);

        // Second scan with unchanged world: no churn.
        monitor.scan();
        assert_eq!(changes.lock().unwrap().len(), 1);
    }

    #[test]
    fn player_entities_are_never_reassigned() {
        let mgr = Arc::new(EntityManager::new());
        let ingest = Arc::new(StateIngest::new(Arc::clone(&mgr), 200.0));
        ingest.apply(1, player_update([0.0, 0.0, 0.0])).unwrap();
        ingest.apply(2, player_update([1.0, 0.0, 0.0])).unwrap();
        let p1 = ingest.player_entity(1).unwrap();

        let monitor = OwnershipMonitor::new(Arc::clone(&mgr), ingest, 5, None);
        monitor.scan();
        assert_eq!(mgr.get(p1).unwrap().network_owner, Some(1));
    }
}
