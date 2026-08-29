//! Full player state serialized across zones during a handoff (jalon D4).
//!
//! Carried as opaque bincode bytes inside the gRPC messages so the proto
//! schema stays stable while this struct evolves.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::entity::EntityState;

/// Everything a target zone needs to resurrect a player mid-session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerStateSnapshot {
    pub source_id: u32,
    pub name: String,
    pub identifiers: Vec<String>,

    pub coords: [f32; 3],
    pub heading: f32,
    pub velocity: [f32; 3],

    pub health: f32,
    pub armour: f32,
    pub current_weapon: u32,

    /// The player's own ped entity, verbatim.
    ///
    /// The loose spatial and health fields above are the routing summary the
    /// gateway reads; this is the authoritative record the target zone
    /// resurrects, so the entity id and model survive the crossing instead of
    /// being re-derived from whatever the client reports next. `None` only
    /// when the player had not been placed in the origin zone yet.
    #[serde(default)]
    pub player_entity: Option<EntityState>,

    /// Entities network-owned by this player (occupied vehicle, etc.).
    pub owned_entities: Vec<EntityState>,

    /// Zone-transferable state registered by resources via
    /// `RegisterZoneTransferState` — keyed by resource name. Values are JSON
    /// text: bincode cannot round-trip `serde_json::Value` (no
    /// `deserialize_any`), and scripts hand us JSON strings anyway.
    pub script_state: HashMap<String, String>,
}

impl PlayerStateSnapshot {
    pub fn encode(&self) -> Vec<u8> {
        bincode::serde::encode_to_vec(self, bincode::config::standard())
            .expect("PlayerStateSnapshot is always bincode-encodable")
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        bincode::serde::decode_from_slice(bytes, bincode::config::standard())
            .map(|(v, _)| v)
            .map_err(|e| format!("invalid PlayerStateSnapshot: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_roundtrip() {
        let mut script_state = HashMap::new();
        script_state.insert(
            "axiom-core".to_string(),
            r#"{"characterId":42}"#.to_string(),
        );
        let snap = PlayerStateSnapshot {
            source_id: 7,
            name: "Lucas".into(),
            identifiers: vec!["license:abc".into()],
            coords: [-120.5, 33.0, 71.2],
            heading: 90.0,
            velocity: [1.0, 0.0, 0.0],
            health: 175.0,
            armour: 50.0,
            current_weapon: 0x1B06D571,
            player_entity: Some(crate::entity::EntityState {
                entity_id: crate::entity::new_entity_id(),
                entity_type: crate::entity::EntityType::Player,
                network_owner: Some(7),
                model_hash: 0xDEAD_BEEF,
                coords: [-120.5, 33.0, 71.2],
                heading: 90.0,
                velocity: [1.0, 0.0, 0.0],
                health: 175.0,
                armour: 50.0,
                extra: crate::entity::EntityExtra::Player {
                    is_in_vehicle: false,
                    vehicle_id: None,
                },
            }),
            owned_entities: vec![],
            script_state,
        };
        let decoded = PlayerStateSnapshot::decode(&snap.encode()).unwrap();
        assert_eq!(decoded.source_id, 7);
        let state: serde_json::Value =
            serde_json::from_str(&decoded.script_state["axiom-core"]).unwrap();
        assert_eq!(state["characterId"], 42);
        // The ped must survive the wire: it is what gives the target zone
        // authority over the player's health and identity after a crossing.
        let ped = decoded.player_entity.expect("ped survives encoding");
        assert_eq!(ped.entity_id, snap.player_entity.unwrap().entity_id);
        assert_eq!(ped.health, 175.0);
        assert_eq!(ped.model_hash, 0xDEAD_BEEF);
    }
}
