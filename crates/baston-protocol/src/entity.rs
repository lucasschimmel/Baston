//! Entity model shared across the state-sync pipeline (Phase C).
//!
//! `EntityState` is the authoritative representation held by a Zone Server's
//! `EntityManager`; `DirtyEntity` batches travel Zone → NATS → Gateway
//! serialized with bincode 2 (serde mode, `bincode::serde::encode_to_vec`).

use serde::{Deserialize, Serialize};

/// Globally unique entity id. UUID v7 = time-ordered (creation order sorts).
pub type EntityId = uuid::Uuid;

/// Allocate a fresh time-ordered entity id.
pub fn new_entity_id() -> EntityId {
    uuid::Uuid::now_v7()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntityType {
    /// Player ped — state is sent by the owning client.
    Player,
    /// Car, bike, boat, plane.
    Vehicle,
    /// NPC — state managed by the Zone Server (Phase D+ for cross-client sync).
    Ped,
    /// Physical object (barricade, crate, ...).
    Object,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntityState {
    pub entity_id: EntityId,
    pub entity_type: EntityType,
    /// Source id of the network owner, if any (Phase C ownership model).
    pub network_owner: Option<u32>,
    pub model_hash: u32,
    pub coords: [f32; 3],
    pub heading: f32,
    /// Needed client-side for dead-reckoning between updates.
    pub velocity: [f32; 3],
    /// 0.0 - 200.0 (100.0 = full health, GTA convention).
    pub health: f32,
    pub armour: f32,
    pub extra: EntityExtra,
}

/// Per-type extra state, synchronized when `DirtyFlags::EXTRA` is set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EntityExtra {
    Player {
        is_in_vehicle: bool,
        vehicle_id: Option<EntityId>,
    },
    Vehicle {
        speed: f32,
        engine_health: f32,
        /// One bit per door (SET_VEHICLE_DOOR_* order).
        doors_open: u8,
    },
    Ped {
        is_fleeing: bool,
        current_weapon: u32,
    },
    Object {
        is_static: bool,
    },
}

bitflags::bitflags! {
    /// Which `EntityState` fields changed since the last NATS publish.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct DirtyFlags: u32 {
        const COORDS   = 0b0000_0001;
        const HEADING  = 0b0000_0010;
        const VELOCITY = 0b0000_0100;
        const HEALTH   = 0b0000_1000;
        const ARMOUR   = 0b0001_0000;
        const EXTRA    = 0b0010_0000;
        /// New entity: the receiver must treat this as a create.
        const CREATED  = 0b0100_0000;
        /// Entity removed from the world (despawn / owner left). The
        /// aggregator drops it from `world_state` and clients get a Delete.
        const DELETED  = 0b1000_0000;
    }
}

// Serialize as the raw u32 bits: compact in bincode and stable across
// flag additions (unknown bits are retained on decode).
impl Serialize for DirtyFlags {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u32(self.bits())
    }
}

impl<'de> Deserialize<'de> for DirtyFlags {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(DirtyFlags::from_bits_retain(u32::deserialize(
            deserializer,
        )?))
    }
}

/// A dirty entity as published on `baston.zone.{zone_id}.state`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DirtyEntity {
    pub entity_id: EntityId,
    pub dirty_fields: DirtyFlags,
    /// Full state snapshot. Field-level delta encoding of the payload itself
    /// is a Phase D optimization; the flags already drive client-side deltas.
    pub state: EntityState,
}

/// Partial update applied to an entity (from a `ClientStateUpdate` or a
/// server script). `None` fields are left untouched.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EntityStateUpdate {
    pub coords: Option<[f32; 3]>,
    pub heading: Option<f32>,
    pub velocity: Option<[f32; 3]>,
    pub health: Option<f32>,
    pub armour: Option<f32>,
    pub extra: Option<EntityExtra>,
}

/// Euclidean distance between two GTA world positions.
pub fn distance3d(a: [f32; 3], b: [f32; 3]) -> f32 {
    let dx = a[0] - b[0];
    let dy = a[1] - b[1];
    let dz = a[2] - b[2];
    (dx * dx + dy * dy + dz * dz).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_state() -> EntityState {
        EntityState {
            entity_id: new_entity_id(),
            entity_type: EntityType::Player,
            network_owner: Some(1),
            model_hash: 0x705E61F2,
            coords: [-1037.0, -2738.0, 20.0],
            heading: 90.0,
            velocity: [0.5, 0.0, 0.0],
            health: 100.0,
            armour: 0.0,
            extra: EntityExtra::Player {
                is_in_vehicle: false,
                vehicle_id: None,
            },
        }
    }

    #[test]
    fn entity_ids_are_time_ordered() {
        let a = new_entity_id();
        let b = new_entity_id();
        assert!(a < b, "uuid v7 must sort by creation time");
    }

    #[test]
    fn dirty_entity_bincode_roundtrip() {
        let batch = vec![
            DirtyEntity {
                entity_id: new_entity_id(),
                dirty_fields: DirtyFlags::COORDS | DirtyFlags::HEALTH,
                state: sample_state(),
            },
            DirtyEntity {
                entity_id: new_entity_id(),
                dirty_fields: DirtyFlags::all(),
                state: EntityState {
                    extra: EntityExtra::Vehicle {
                        speed: 12.5,
                        engine_health: 1000.0,
                        doors_open: 0b0000_0011,
                    },
                    entity_type: EntityType::Vehicle,
                    ..sample_state()
                },
            },
        ];
        let bytes = bincode::serde::encode_to_vec(&batch, bincode::config::standard()).unwrap();
        let (decoded, read): (Vec<DirtyEntity>, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
        assert_eq!(read, bytes.len());
        assert_eq!(decoded, batch);
    }

    #[test]
    fn distance3d_basics() {
        assert_eq!(distance3d([0.0; 3], [3.0, 4.0, 0.0]), 5.0);
        assert_eq!(distance3d([1.0, 1.0, 1.0], [1.0, 1.0, 1.0]), 0.0);
    }
}
