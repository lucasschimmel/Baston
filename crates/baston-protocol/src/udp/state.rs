//! Client → Zone state updates and Gateway → client entity snapshots
//! (Phase C, jalons C2/C3).
//!
//! These are BASTON-internal messages (`msgBaston*` rage-hash types) used by
//! the loadtest client and, on the real-client path, produced by the
//! axiom-core client shim which forwards its ped state via a net event that
//! the gateway re-encodes into `ClientStateUpdate`.

use serde::{Deserialize, Serialize};

use super::hash_rage_string;
use crate::entity::{DirtyEntity, EntityId, EntityType};

/// Client → server per-frame authoritative state of an owned entity.
pub const MSG_BASTON_STATE: u32 = hash_rage_string("msgBastonState");
/// Server → client batch of entity ops (jalon C3 internal snapshot format).
pub const MSG_BASTON_SNAPSHOT: u32 = hash_rage_string("msgBastonSnapshot");

/// State the network owner reports for one of its entities.
///
/// `entity_id: None` means "my own player ped" — the zone resolves/creates
/// the player entity bound to the sending source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientStateUpdate {
    pub entity_id: Option<EntityId>,
    pub entity_type: EntityType,
    pub model_hash: u32,
    pub coords: [f32; 3],
    pub heading: f32,
    pub velocity: [f32; 3],
    pub health: f32,
    pub armour: f32,
    pub extra: Option<crate::entity::EntityExtra>,
}

/// Net event the real-client shim uses to report its ped state
/// (client scripts can only `emitNet`, not send raw ENet packets).
pub const STATE_UPDATE_EVENT: &str = "__baston:stateUpdate";

/// Decode the shim's net-event args into a `ClientStateUpdate`.
/// Expected shape: `[{ model, coords: [x,y,z], heading, velocity: [x,y,z],
/// health, armour }]`.
pub fn client_state_update_from_json(args: &serde_json::Value) -> Option<ClientStateUpdate> {
    let obj = args.get(0)?;
    let vec3 = |v: &serde_json::Value| -> Option<[f32; 3]> {
        Some([
            v.get(0)?.as_f64()? as f32,
            v.get(1)?.as_f64()? as f32,
            v.get(2)?.as_f64()? as f32,
        ])
    };
    Some(ClientStateUpdate {
        entity_id: None,
        entity_type: EntityType::Player,
        model_hash: obj.get("model")?.as_u64()? as u32,
        coords: vec3(obj.get("coords")?)?,
        heading: obj.get("heading")?.as_f64()? as f32,
        velocity: obj.get("velocity").and_then(&vec3).unwrap_or([0.0; 3]),
        health: obj.get("health").and_then(|v| v.as_f64()).unwrap_or(100.0) as f32,
        armour: obj.get("armour").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32,
        extra: None,
    })
}

/// One entity operation inside a snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EntityOp {
    /// Create-or-update. Carries the dirty flags so the client applies only
    /// the changed fields (delta) once it already knows the entity.
    Upsert(DirtyEntity),
    /// Entity left the client's AoI or despawned.
    Delete(EntityId),
}

/// Gateway → client batch, pushed at 20fps per client (jalon C3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntitySnapshot {
    /// Gateway push-loop tick, for ordering on the client.
    pub tick: u64,
    pub ops: Vec<EntityOp>,
}

fn encode<T: Serialize>(msg_type: u32, value: &T) -> Vec<u8> {
    let mut out = msg_type.to_le_bytes().to_vec();
    // Serialization of our own types into a Vec cannot fail.
    let payload = bincode::serde::encode_to_vec(value, bincode::config::standard())
        .expect("bincode encode of protocol type");
    out.extend_from_slice(&payload);
    out
}

fn decode<T: for<'de> Deserialize<'de>>(payload: &[u8]) -> Option<T> {
    bincode::serde::decode_from_slice(payload, bincode::config::standard())
        .ok()
        .map(|(value, _)| value)
}

/// Full packet (`u32 type | bincode payload`) for a state update.
pub fn build_state_update(update: &ClientStateUpdate) -> Vec<u8> {
    encode(MSG_BASTON_STATE, update)
}

/// Parse a `msgBastonState` payload (message type already stripped).
pub fn parse_state_update(payload: &[u8]) -> Option<ClientStateUpdate> {
    decode(payload)
}

/// Full packet for an entity snapshot.
pub fn build_snapshot(snapshot: &EntitySnapshot) -> Vec<u8> {
    encode(MSG_BASTON_SNAPSHOT, snapshot)
}

/// Parse a `msgBastonSnapshot` payload (message type already stripped).
pub fn parse_snapshot(payload: &[u8]) -> Option<EntitySnapshot> {
    decode(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{new_entity_id, DirtyFlags, EntityExtra, EntityState};
    use crate::udp::read_message_type;

    fn sample_update() -> ClientStateUpdate {
        ClientStateUpdate {
            entity_id: None,
            entity_type: EntityType::Player,
            model_hash: 0x705E61F2,
            coords: [-1037.0, -2738.0, 20.0],
            heading: 180.0,
            velocity: [1.0, 0.0, 0.0],
            health: 100.0,
            armour: 25.0,
            extra: None,
        }
    }

    #[test]
    fn state_update_roundtrip() {
        let update = sample_update();
        let packet = build_state_update(&update);
        let (ty, payload) = read_message_type(&packet).unwrap();
        assert_eq!(ty, MSG_BASTON_STATE);
        assert_eq!(parse_state_update(payload).unwrap(), update);
    }

    #[test]
    fn snapshot_roundtrip() {
        let id = new_entity_id();
        let snapshot = EntitySnapshot {
            tick: 42,
            ops: vec![
                EntityOp::Upsert(DirtyEntity {
                    entity_id: id,
                    dirty_fields: DirtyFlags::COORDS | DirtyFlags::CREATED,
                    state: EntityState {
                        entity_id: id,
                        entity_type: EntityType::Player,
                        network_owner: Some(7),
                        model_hash: 1,
                        coords: [1.0, 2.0, 3.0],
                        heading: 0.0,
                        velocity: [0.0; 3],
                        health: 100.0,
                        armour: 0.0,
                        extra: EntityExtra::Player {
                            is_in_vehicle: false,
                            vehicle_id: None,
                        },
                    },
                }),
                EntityOp::Delete(new_entity_id()),
            ],
        };
        let packet = build_snapshot(&snapshot);
        let (ty, payload) = read_message_type(&packet).unwrap();
        assert_eq!(ty, MSG_BASTON_SNAPSHOT);
        assert_eq!(parse_snapshot(payload).unwrap(), snapshot);
    }

    #[test]
    fn parse_garbage_returns_none() {
        assert!(parse_state_update(&[0xFF, 0x01]).is_none());
        assert!(parse_snapshot(b"garbage").is_none());
    }
}
