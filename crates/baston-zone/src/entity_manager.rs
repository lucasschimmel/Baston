//! Concurrent entity store for a Zone Server (jalon C1).
//!
//! `entities` is a `DashMap` (sharded, no global lock) so many client-update
//! handlers can write simultaneously. The dirty queue is a single small
//! mutex-guarded map drained every 16ms by the `StateSyncEmitter`; entries
//! are coalesced per entity (flags OR-ed, latest state wins) so a client
//! updating faster than the tick costs one queue slot, not N.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use baston_protocol::entity::{
    DirtyEntity, DirtyFlags, EntityId, EntityState, EntityStateUpdate,
};
use dashmap::DashMap;

#[derive(Default)]
pub struct EntityManager {
    entities: DashMap<EntityId, EntityState>,
    dirty_queue: Arc<Mutex<HashMap<EntityId, DirtyEntity>>>,
}

impl EntityManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new entity; marked fully dirty (CREATED) for the next flush.
    pub fn spawn_entity(&self, state: EntityState) {
        let id = state.entity_id;
        self.entities.insert(id, state.clone());
        self.mark_dirty(id, DirtyFlags::all() - DirtyFlags::DELETED, state);
    }

    /// Remove an entity; a DELETED marker propagates through the pipeline.
    pub fn remove_entity(&self, id: EntityId) -> Option<EntityState> {
        let removed = self.entities.remove(&id).map(|(_, state)| state);
        if let Some(state) = &removed {
            let mut queue = self.dirty_queue.lock().expect("dirty_queue poisoned");
            // A delete supersedes any pending update for this entity.
            queue.insert(
                id,
                DirtyEntity {
                    entity_id: id,
                    dirty_fields: DirtyFlags::DELETED,
                    state: state.clone(),
                },
            );
        }
        removed
    }

    /// Apply a partial update; dirty flags are derived by diffing against the
    /// current state, so a no-op update publishes nothing.
    pub fn update_entity(&self, id: EntityId, update: EntityStateUpdate) -> DirtyFlags {
        let Some(mut entry) = self.entities.get_mut(&id) else {
            tracing::debug!(target: "zone", %id, "update for unknown entity ignored");
            return DirtyFlags::empty();
        };
        let state = entry.value_mut();
        let mut flags = DirtyFlags::empty();

        if let Some(coords) = update.coords {
            if state.coords != coords {
                state.coords = coords;
                flags |= DirtyFlags::COORDS;
            }
        }
        if let Some(heading) = update.heading {
            if state.heading != heading {
                state.heading = heading;
                flags |= DirtyFlags::HEADING;
            }
        }
        if let Some(velocity) = update.velocity {
            if state.velocity != velocity {
                state.velocity = velocity;
                flags |= DirtyFlags::VELOCITY;
            }
        }
        if let Some(health) = update.health {
            if state.health != health {
                state.health = health;
                flags |= DirtyFlags::HEALTH;
            }
        }
        if let Some(armour) = update.armour {
            if state.armour != armour {
                state.armour = armour;
                flags |= DirtyFlags::ARMOUR;
            }
        }
        if let Some(extra) = update.extra {
            if state.extra != extra {
                state.extra = extra;
                flags |= DirtyFlags::EXTRA;
            }
        }

        if !flags.is_empty() {
            let snapshot = state.clone();
            // Release the shard lock before touching the queue mutex to keep
            // the lock ordering one-way (entities → queue, never both held
            // against each other from another path).
            drop(entry);
            self.mark_dirty(id, flags, snapshot);
        }
        flags
    }

    /// Assign (or clear) the network owner. Ownership changes ride the EXTRA
    /// channel so clients within AoI learn the new authority (jalon C5).
    pub fn set_network_owner(&self, id: EntityId, owner: Option<u32>) -> bool {
        let Some(mut entry) = self.entities.get_mut(&id) else {
            return false;
        };
        if entry.network_owner == owner {
            return true;
        }
        entry.network_owner = owner;
        let snapshot = entry.value().clone();
        drop(entry);
        self.mark_dirty(id, DirtyFlags::EXTRA, snapshot);
        true
    }

    fn mark_dirty(&self, id: EntityId, flags: DirtyFlags, state: EntityState) {
        let mut queue = self.dirty_queue.lock().expect("dirty_queue poisoned");
        match queue.entry(id) {
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                let pending = slot.get_mut();
                // Never resurrect an entity already scheduled for deletion.
                if pending.dirty_fields.contains(DirtyFlags::DELETED) {
                    return;
                }
                pending.dirty_fields |= flags;
                pending.state = state;
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(DirtyEntity {
                    entity_id: id,
                    dirty_fields: flags,
                    state,
                });
            }
        }
    }

    /// Drain the dirty queue. Idempotent: a second call without intervening
    /// updates returns an empty vec. Called every 16ms — the lock is held
    /// only for a `mem::take` (< 1ms even with thousands of entries).
    pub fn flush_dirty(&self) -> Vec<DirtyEntity> {
        let drained = {
            let mut queue = self.dirty_queue.lock().expect("dirty_queue poisoned");
            std::mem::take(&mut *queue)
        };
        drained.into_values().collect()
    }

    pub fn get(&self, id: EntityId) -> Option<EntityState> {
        self.entities.get(&id).map(|e| e.value().clone())
    }

    /// Snapshot of all live entities (used by ownership reassignment scans).
    pub fn snapshot(&self) -> Vec<EntityState> {
        self.entities.iter().map(|e| e.value().clone()).collect()
    }

    /// Entities network-owned by a player (occupied vehicle, etc.) — used to
    /// build the handoff snapshot (D4).
    pub fn entities_owned_by(&self, source: u32) -> Vec<EntityState> {
        self.entities
            .iter()
            .filter(|e| e.value().network_owner == Some(source))
            .map(|e| e.value().clone())
            .collect()
    }

    pub fn count(&self) -> usize {
        self.entities.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baston_protocol::entity::{new_entity_id, EntityExtra, EntityType};

    fn spawn_player(mgr: &EntityManager) -> EntityId {
        let id = new_entity_id();
        mgr.spawn_entity(EntityState {
            entity_id: id,
            entity_type: EntityType::Player,
            network_owner: Some(1),
            model_hash: 0x705E61F2,
            coords: [0.0, 0.0, 0.0],
            heading: 0.0,
            velocity: [0.0, 0.0, 0.0],
            health: 100.0,
            armour: 0.0,
            extra: EntityExtra::Player {
                is_in_vehicle: false,
                vehicle_id: None,
            },
        });
        mgr.flush_dirty(); // clear the CREATED entry
        id
    }

    #[test]
    fn update_coords_marks_only_coords_dirty() {
        let mgr = EntityManager::new();
        let id = spawn_player(&mgr);
        let flags = mgr.update_entity(
            id,
            EntityStateUpdate {
                coords: Some([10.0, 0.0, 0.0]),
                ..Default::default()
            },
        );
        assert_eq!(flags, DirtyFlags::COORDS);
        let dirty = mgr.flush_dirty();
        assert_eq!(dirty.len(), 1);
        assert_eq!(dirty[0].dirty_fields, DirtyFlags::COORDS);
        assert_eq!(dirty[0].state.coords, [10.0, 0.0, 0.0]);
    }

    #[test]
    fn update_coords_and_health_sets_both_flags() {
        let mgr = EntityManager::new();
        let id = spawn_player(&mgr);
        let flags = mgr.update_entity(
            id,
            EntityStateUpdate {
                coords: Some([1.0, 2.0, 3.0]),
                health: Some(50.0),
                ..Default::default()
            },
        );
        assert_eq!(flags, DirtyFlags::COORDS | DirtyFlags::HEALTH);
    }

    #[test]
    fn noop_update_publishes_nothing() {
        let mgr = EntityManager::new();
        let id = spawn_player(&mgr);
        let flags = mgr.update_entity(
            id,
            EntityStateUpdate {
                coords: Some([0.0, 0.0, 0.0]), // unchanged
                ..Default::default()
            },
        );
        assert!(flags.is_empty());
        assert!(mgr.flush_dirty().is_empty());
    }

    #[test]
    fn flush_dirty_is_idempotent() {
        let mgr = EntityManager::new();
        let id = spawn_player(&mgr);
        mgr.update_entity(
            id,
            EntityStateUpdate {
                heading: Some(90.0),
                ..Default::default()
            },
        );
        assert_eq!(mgr.flush_dirty().len(), 1);
        assert!(mgr.flush_dirty().is_empty());
    }

    #[test]
    fn same_tick_updates_coalesce() {
        let mgr = EntityManager::new();
        let id = spawn_player(&mgr);
        mgr.update_entity(
            id,
            EntityStateUpdate {
                coords: Some([1.0, 0.0, 0.0]),
                ..Default::default()
            },
        );
        mgr.update_entity(
            id,
            EntityStateUpdate {
                health: Some(80.0),
                ..Default::default()
            },
        );
        let dirty = mgr.flush_dirty();
        assert_eq!(dirty.len(), 1);
        assert_eq!(dirty[0].dirty_fields, DirtyFlags::COORDS | DirtyFlags::HEALTH);
        assert_eq!(dirty[0].state.health, 80.0);
    }

    #[test]
    fn remove_entity_emits_deleted_marker_that_wins() {
        let mgr = EntityManager::new();
        let id = spawn_player(&mgr);
        mgr.update_entity(
            id,
            EntityStateUpdate {
                coords: Some([5.0, 0.0, 0.0]),
                ..Default::default()
            },
        );
        assert!(mgr.remove_entity(id).is_some());
        // Post-delete update must not resurrect the queue entry.
        mgr.update_entity(
            id,
            EntityStateUpdate {
                health: Some(1.0),
                ..Default::default()
            },
        );
        let dirty = mgr.flush_dirty();
        assert_eq!(dirty.len(), 1);
        assert_eq!(dirty[0].dirty_fields, DirtyFlags::DELETED);
        assert_eq!(mgr.count(), 0);
    }

    #[test]
    fn concurrent_updates_are_safe() {
        let mgr = Arc::new(EntityManager::new());
        let ids: Vec<EntityId> = (0..10).map(|_| spawn_player(&mgr)).collect();

        let handles: Vec<_> = (0..10)
            .map(|t| {
                let mgr = Arc::clone(&mgr);
                let ids = ids.clone();
                std::thread::spawn(move || {
                    for i in 0..1000u32 {
                        let id = ids[(t + i as usize) % ids.len()];
                        mgr.update_entity(
                            id,
                            EntityStateUpdate {
                                coords: Some([i as f32, t as f32, 0.0]),
                                health: Some((i % 200) as f32),
                                ..Default::default()
                            },
                        );
                        if i % 100 == 0 {
                            let _ = mgr.flush_dirty();
                        }
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("worker thread panicked");
        }
        assert_eq!(mgr.count(), 10);
        // Every queued entry must reference a live entity.
        for d in mgr.flush_dirty() {
            assert!(mgr.get(d.entity_id).is_some());
        }
    }

    #[test]
    fn throughput_1000_updates_per_second_without_contention() {
        // Benchmark-as-test: 10 threads × 10k updates must complete far
        // faster than the 100s that 1000/s would allow.
        let mgr = Arc::new(EntityManager::new());
        let ids: Vec<EntityId> = (0..100).map(|_| spawn_player(&mgr)).collect();
        let start = std::time::Instant::now();
        let handles: Vec<_> = (0..10)
            .map(|t| {
                let mgr = Arc::clone(&mgr);
                let ids = ids.clone();
                std::thread::spawn(move || {
                    for i in 0..10_000u32 {
                        mgr.update_entity(
                            ids[(i as usize + t) % ids.len()],
                            EntityStateUpdate {
                                coords: Some([i as f32, 0.0, 0.0]),
                                ..Default::default()
                            },
                        );
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let elapsed = start.elapsed();
        let rate = 100_000.0 / elapsed.as_secs_f64();
        assert!(
            rate > 1000.0,
            "expected > 1000 updates/s, measured {rate:.0}/s"
        );
    }
}
