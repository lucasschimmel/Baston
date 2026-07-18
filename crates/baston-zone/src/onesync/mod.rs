//! Server-authoritative OneSync game state.
//!
//! The Rust counterpart of the parts of `ServerGameState` that make the server
//! *parse* entity state instead of relaying it: an object-id-keyed entity
//! registry fed by inbound `netClones` streams, object-id leasing, and ack
//! generation. Interest management and the outbound per-client tick live in
//! the NG scheduler (task 6); this module owns ingestion + arbitration.
//!
//! Everything here is transport-agnostic: the gateway hands raw `msgRoute`
//! payloads in and gets ack packets (and, later, per-client clone packets) out.

mod ingest;
#[cfg(test)]
mod tests;

use std::collections::HashMap;

use baston_protocol::rage::clone::NetObjEntityType;
use baston_protocol::rage::object_ids;
use baston_protocol::rage::reliability::{GameStateAck, GameStateNAck};

use crate::interest_ng::ClientView;

/// The largest usable object id without the length hack (13-bit native space).
/// Server-created entities scan downward from here; client ids are leased
/// upward from 1.
pub const MAX_OBJECT_ID_NATIVE: u16 = 8191;
/// With the length hack (OneSync Beyond) ids widen to the full 16-bit space.
pub const MAX_OBJECT_ID_BEYOND: u16 = u16::MAX - 1;

/// A server-tracked networked entity. The sync-tree blob is stored opaquely
/// (the server only semantically parses a subset of nodes elsewhere); the
/// registry's job is identity, ownership, and freshness.
#[derive(Debug, Clone)]
pub struct ServerEntity {
    pub object_id: u16,
    pub uniqifier: u16,
    /// Net id of the current owner (the source that last created/synced it).
    pub owner: u32,
    pub entity_type: NetObjEntityType,
    pub creation_token: u32,
    /// Latest raw sync-tree blob (bit-packed, as received).
    pub data: Vec<u8>,
    /// Server frame index at last update — the delta baseline.
    pub frame_index: u64,
    /// Best-effort world position driving interest management. Populated from
    /// the client state-report path today; parsed from the position sync node
    /// once semantic node decoding lands (see `sync_trees`).
    pub position: [f32; 3],
}

/// Result of ingesting one inbound clone packet: the ack packets to send back
/// (already framed as `msgPackedAcks`) plus bookkeeping counters.
#[derive(Debug, Default)]
pub struct IngestOutcome {
    pub ack_packets: Vec<Vec<u8>>,
    pub creates: u32,
    pub syncs: u32,
    pub removes: u32,
    /// Takeover requests observed (objectId → requested target net id);
    /// arbitration is applied by [`ServerGameState`] before returning.
    pub takeovers: Vec<(u16, u16)>,
}

/// Per-client object-id lease + sync bookkeeping.
struct ClientState {
    /// Object ids leased to this client (awaiting or holding a live entity).
    leased: Vec<u16>,
    /// The client's current frame index (from type-6 records).
    frame_index: u64,
    /// The client's ack timestamp (from type-5 records).
    ack_ts: u32,
    /// Outbound sparse view (interest management + delta baselines).
    view: ClientView,
}

impl ClientState {
    fn new(net_id: u16) -> Self {
        Self {
            leased: Vec::new(),
            frame_index: 0,
            ack_ts: 0,
            view: ClientView::new(net_id),
        }
    }
}

/// Allocation state of one object id. A single authoritative state machine
/// replaces the old `id_used`/`id_leased` twin bitmaps (audit ROB-4): an id is
/// either free, leased to a client via `GetFreeObjectIds`, or consumed by a
/// live entity. A create on a leased id promotes `Leased → Used` (the lease is
/// consumed), so the two flags can never diverge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum IdState {
    #[default]
    Free,
    Leased,
    Used,
}

/// Server-authoritative game state for one routing bucket / zone.
pub struct ServerGameState {
    entities: HashMap<u16, ServerEntity>,
    clients: HashMap<u32, ClientState>,
    /// Allocation state of every object id.
    ids: Vec<IdState>,
    max_object_id: u16,
    big_mode: bool,
    length_hack: bool,
    frame_index: u64,
}

impl ServerGameState {
    pub fn new(big_mode: bool, length_hack: bool) -> Self {
        let max_object_id = if length_hack {
            MAX_OBJECT_ID_BEYOND
        } else {
            MAX_OBJECT_ID_NATIVE
        };
        Self {
            entities: HashMap::new(),
            clients: HashMap::new(),
            ids: vec![IdState::Free; max_object_id as usize + 1],
            max_object_id,
            big_mode,
            length_hack,
            frame_index: 1,
        }
    }

    pub fn entity(&self, object_id: u16) -> Option<&ServerEntity> {
        self.entities.get(&object_id)
    }

    pub fn entity_count(&self) -> usize {
        self.entities.len()
    }

    /// Advance the server frame index (called once per sync tick).
    pub fn tick(&mut self) {
        self.frame_index = self.frame_index.wrapping_add(1);
    }

    pub fn frame_index(&self) -> u64 {
        self.frame_index
    }

    /// Register a connected client (idempotent).
    pub fn add_client(&mut self, source: u32) {
        self.clients
            .entry(source)
            .or_insert_with(|| ClientState::new(source as u16));
    }

    /// Drop a client and release every object id it held. Entities it owned
    /// become ownerless (arbitration/migration handled by the NG scheduler).
    /// Returns the object ids of entities the client owned.
    pub fn remove_client(&mut self, source: u32) -> Vec<u16> {
        if let Some(state) = self.clients.remove(&source) {
            for id in state.leased {
                // Only release ids still in the leased state; an id promoted to
                // Used by a create stays claimed while its entity lives (it may
                // have been taken over by another client).
                if self.ids[id as usize] == IdState::Leased {
                    self.ids[id as usize] = IdState::Free;
                }
            }
        }
        let mut orphaned = Vec::new();
        for (&id, ent) in &self.entities {
            if ent.owner == source {
                orphaned.push(id);
            }
        }
        for id in &orphaned {
            self.entities.remove(id);
            self.ids[*id as usize] = IdState::Free;
        }
        orphaned
    }

    /// Lease up to `n` free object ids to a client (low-to-high scan), matching
    /// `GetFreeObjectIds`. Returns the ids and the ready-to-send `msgObjectIds`
    /// packet.
    pub fn lease_object_ids(&mut self, source: u32, n: usize) -> (Vec<u16>, Vec<u8>) {
        let mut ids = Vec::with_capacity(n);
        let mut id: u16 = 1;
        while ids.len() < n && id < self.max_object_id {
            if self.ids[id as usize] == IdState::Free {
                self.ids[id as usize] = IdState::Leased;
                ids.push(id);
            }
            id += 1;
        }
        let state = self
            .clients
            .entry(source)
            .or_insert_with(|| ClientState::new(source as u16));
        state.leased.extend_from_slice(&ids);
        let packet = object_ids::build_object_ids(&ids);
        (ids, packet)
    }

    /// The per-request lease size for this mode (6 big / 32 otherwise).
    pub fn ids_per_request(&self) -> usize {
        object_ids::ids_per_request(self.big_mode)
    }

    /// Feed a best-effort world position for an entity (from the client
    /// state-report path). Drives interest management until sync-node position
    /// parsing lands.
    pub fn set_entity_position(&mut self, object_id: u16, position: [f32; 3]) {
        if let Some(ent) = self.entities.get_mut(&object_id) {
            ent.position = position;
        }
    }

    /// Copy each connected client's reported focus onto the Player entity it
    /// owns, so player-to-player interest management works before sync-node
    /// position parsing exists. `focus` maps net id → world position.
    pub fn update_player_positions(&mut self, focus: &HashMap<u32, [f32; 3]>) {
        for ent in self.entities.values_mut() {
            if ent.entity_type == NetObjEntityType::Player {
                if let Some(&pos) = focus.get(&ent.owner) {
                    ent.position = pos;
                }
            }
        }
    }

    /// The list of connected client net ids (for driving the outbound tick).
    pub fn client_sources(&self) -> Vec<u32> {
        self.clients.keys().copied().collect()
    }

    /// Snapshot the world for interest management.
    fn world_snapshot(&self) -> Vec<crate::interest_ng::EntitySnapshot> {
        self.entities
            .values()
            .map(|e| crate::interest_ng::EntitySnapshot {
                object_id: e.object_id,
                uniqifier: e.uniqifier,
                owner: e.owner,
                position: e.position,
                frame_index: e.frame_index,
                data_len: e.data.len(),
            })
            .collect()
    }

    /// Run the outbound sync tick for one client and return the framed
    /// `msgPackedClones` packets to send. `focus` is the client's position.
    /// Produces create/sync/remove records via the NG interest manager, cut as
    /// per-client deltas against the client's view.
    pub fn tick_client(
        &mut self,
        source: u32,
        focus: [f32; 3],
        cfg: &crate::interest_ng::InterestConfig,
    ) -> Vec<Vec<u8>> {
        use baston_protocol::rage::clone::{rec, OutboundClone};
        use baston_protocol::rage::packet::{PackedWriter, MSG_PACKED_CLONES};

        let world = self.world_snapshot();
        let frame_index = self.frame_index;
        let Some(state) = self.clients.get_mut(&source) else {
            return Vec::new();
        };
        let plan = state.view.tick(focus, &world, cfg);
        if plan.creates.is_empty() && plan.syncs.is_empty() && plan.removes.is_empty() {
            return Vec::new();
        }

        let mut writer = PackedWriter::new(MSG_PACKED_CLONES, frame_index, self.length_hack);
        let timestamp = frame_index; // server clock proxy; refined at wire time

        // `state` (borrow of self.clients) and `self.entities` are disjoint
        // fields, so both can be read here without aliasing.
        for (sync_type, ids) in [(rec::CREATE, &plan.creates), (rec::SYNC, &plan.syncs)] {
            for &object_id in ids {
                let Some(e) = self.entities.get(&object_id) else {
                    continue;
                };
                let base = state.view.base_frame(object_id).unwrap_or(0);
                let clone = OutboundClone {
                    sync_type,
                    object_id: e.object_id,
                    owner_net_id: e.owner as u16,
                    entity_type: (sync_type == rec::CREATE).then_some(e.entity_type),
                    creation_token: e.creation_token,
                    uniqifier: e.uniqifier,
                    dependent_frame_index: base,
                    timestamp: timestamp as u32,
                    first_frame_update: false,
                    data: &e.data,
                };
                writer.write_clone(timestamp, &clone);
            }
        }
        for &(object_id, uniqifier) in &plan.removes {
            writer.write_remove(false, object_id, uniqifier);
        }

        writer.finish()
    }

    /// Apply a `gameStateNAck` (NAK mode): roll the client's delta baselines
    /// back so the next tick re-sends what was missed. No frame-snapshot
    /// backlog is consulted, so the client can never be dropped for a stale
    /// NACK.
    pub fn apply_nack(&mut self, source: u32, nack: &GameStateNAck) {
        let Some(state) = self.clients.get_mut(&source) else {
            return;
        };
        if let Some((first, _last)) = nack.missing_frames {
            // Re-send everything since the first missing frame.
            state.view.rollback_all(first.saturating_sub(1));
        }
        for entry in &nack.ignore_list {
            state
                .view
                .rollback(entry.object_id, entry.last_frame, false);
        }
        for &object_id in &nack.recreate_list {
            state.view.force_recreate(object_id);
        }
    }

    /// Apply a `gameStateAck` (ARQ mode). NG relies on NAK, so this only nudges
    /// the client's confirmed frame and honors any residual ignore/recreate.
    pub fn apply_ack(&mut self, source: u32, ack: &GameStateAck) {
        let Some(state) = self.clients.get_mut(&source) else {
            return;
        };
        state.view.ack_frame(ack.frame_index);
        for entry in &ack.ignore_list {
            state
                .view
                .rollback(entry.object_id, entry.last_frame, false);
        }
        for &object_id in &ack.recreate_list {
            state.view.force_recreate(object_id);
        }
    }
}
