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

use std::collections::HashMap;

use baston_protocol::rage::clone::{write_ack_record, write_end, InboundRecord, NetObjEntityType};
use baston_protocol::rage::object_ids;
use baston_protocol::rage::packet::{decode_incoming, pack_frame, FrameIndex, MSG_PACKED_ACKS};
use baston_protocol::rage::reliability::{GameStateAck, GameStateNAck};
use baston_protocol::rage::MessageBuffer;

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

/// Server-authoritative game state for one routing bucket / zone.
pub struct ServerGameState {
    entities: HashMap<u16, ServerEntity>,
    clients: HashMap<u32, ClientState>,
    /// Ownership of every object id: leased-out set and live set.
    id_used: Vec<bool>,
    id_leased: Vec<bool>,
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
            id_used: vec![false; max_object_id as usize + 1],
            id_leased: vec![false; max_object_id as usize + 1],
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
                self.id_leased[id as usize] = false;
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
            self.id_used[*id as usize] = false;
            self.id_leased[*id as usize] = false;
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
            if !self.id_used[id as usize] && !self.id_leased[id as usize] {
                self.id_leased[id as usize] = true;
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

    /// Ingest one inbound `msgRoute` clone payload (the `[u32 type][lz4]` tail).
    /// Updates the registry and returns ack packets. Returns an empty outcome
    /// if the payload isn't a clone/ack stream or fails to decode.
    pub fn ingest_clone_payload(&mut self, source: u32, payload: &[u8]) -> IngestOutcome {
        let Some(incoming) = decode_incoming(payload) else {
            return IngestOutcome::default();
        };
        self.add_client(source);

        let mut ack = MessageBuffer::new(4096).with_length_hack(self.length_hack);
        let mut outcome = IngestOutcome::default();

        for record in incoming.records {
            match record {
                InboundRecord::Clone {
                    is_create,
                    object_id,
                    uniqifier,
                    entity_type,
                    creation_token,
                    data,
                } => {
                    if object_id == 0xFFFF {
                        continue; // "that's not an object ID, that's a snail!"
                    }
                    if is_create {
                        // Only claim the object id if the create was actually
                        // accepted — a rejected create (owner conflict or
                        // invalid entity type, both client-forgeable) must not
                        // permanently mark the id used, or the id space leaks.
                        if self.apply_create(
                            source,
                            object_id,
                            uniqifier,
                            entity_type,
                            creation_token,
                            data,
                        ) {
                            self.id_used[object_id as usize] = true;
                        }
                        write_ack_record(&mut ack, 1, object_id, uniqifier);
                        outcome.creates += 1;
                    } else {
                        self.apply_sync(source, object_id, uniqifier, data);
                        write_ack_record(&mut ack, 2, object_id, uniqifier);
                        outcome.syncs += 1;
                    }
                }
                InboundRecord::Remove {
                    object_id,
                    uniqifier,
                } => {
                    // Ack the remove regardless of acceptance (SGS.cpp:3155).
                    write_ack_record(&mut ack, 3, object_id, uniqifier);
                    self.apply_remove(source, object_id, uniqifier);
                    outcome.removes += 1;
                }
                InboundRecord::Takeover {
                    target_client,
                    object_id,
                } => {
                    let target = if target_client == 0 {
                        source as u16
                    } else {
                        target_client
                    };
                    outcome.takeovers.push((object_id, target));
                    self.apply_takeover(source, object_id, target);
                }
                InboundRecord::Timestamp(ts) => {
                    let state = self
                        .clients
                        .entry(source)
                        .or_insert_with(|| ClientState::new(source as u16));
                    if state.ack_ts == 0 || state.ack_ts < ts {
                        state.ack_ts = ts;
                    }
                    // Echo the timestamp ack (type 5) as the engine does.
                    ack.write_bits_single(5, 3);
                    ack.write_bits_single(ts, 32);
                }
                InboundRecord::Index(idx) => {
                    self.clients
                        .entry(source)
                        .or_insert_with(|| ClientState::new(source as u16))
                        .frame_index = u64::from(idx);
                }
            }
        }

        write_end(&mut ack);
        let used = ack.data_length();
        if used > 0 {
            let frame = FrameIndex {
                last_fragment: true,
                current_fragment: 0,
                frame_index: self.clients.get(&source).map_or(0, |c| c.frame_index),
            };
            outcome
                .ack_packets
                .push(pack_frame(MSG_PACKED_ACKS, frame, &ack.buffer()[..used]));
        }
        outcome
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

    /// Returns `true` if the entity was created/refreshed, `false` if the
    /// create was rejected (so the caller must not claim the object id).
    fn apply_create(
        &mut self,
        source: u32,
        object_id: u16,
        uniqifier: u16,
        entity_type: Option<NetObjEntityType>,
        creation_token: u32,
        data: Vec<u8>,
    ) -> bool {
        // Duplicate create for a live entity with a different owner: reject
        // (SGS.cpp:3427). Same owner refreshes.
        if let Some(existing) = self.entities.get(&object_id) {
            if existing.owner != source || existing.uniqifier != uniqifier {
                return false;
            }
        }
        let Some(entity_type) = entity_type else {
            return false; // invalid type → no sync tree → drop (SGS.cpp:3417)
        };
        let frame_index = self.frame_index;
        // Preserve a known position across a same-owner re-create.
        let position = self
            .entities
            .get(&object_id)
            .map_or([0.0; 3], |e| e.position);
        self.entities.insert(
            object_id,
            ServerEntity {
                object_id,
                uniqifier,
                owner: source,
                entity_type,
                creation_token,
                data,
                frame_index,
                position,
            },
        );
        true
    }

    fn apply_sync(&mut self, source: u32, object_id: u16, uniqifier: u16, data: Vec<u8>) {
        let frame_index = self.frame_index;
        if let Some(ent) = self.entities.get_mut(&object_id) {
            // Wrong uniqifier or wrong owner → ignore (SGS.cpp:3454, 3491).
            if ent.uniqifier != uniqifier || ent.owner != source {
                return;
            }
            if !data.is_empty() {
                ent.data = data;
                ent.frame_index = frame_index;
            }
        }
    }

    fn apply_remove(&mut self, source: u32, object_id: u16, uniqifier: u16) {
        if let Some(ent) = self.entities.get(&object_id) {
            // Only the owner may remove, and the uniqifier must match.
            if ent.owner == source && ent.uniqifier == uniqifier {
                self.entities.remove(&object_id);
                self.id_used[object_id as usize] = false;
                self.id_leased[object_id as usize] = false;
            }
        }
    }

    fn apply_takeover(&mut self, sender: u32, object_id: u16, target: u16) {
        if let Some(ent) = self.entities.get_mut(&object_id) {
            // The sender must currently own the entity (SGS.cpp:3128).
            if ent.owner == sender {
                ent.owner = u32::from(target);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baston_protocol::rage::clone::{rec, write_end as clone_end};
    use baston_protocol::rage::lz4dict;
    use baston_protocol::rage::packet::NET_CLONES;

    /// Build a compressed `netClones` payload from hand-written records.
    fn build_clone_payload(build: impl FnOnce(&mut MessageBuffer)) -> Vec<u8> {
        let mut body = MessageBuffer::new(4096);
        build(&mut body);
        clone_end(&mut body);
        let used = body.data_length();
        let raw = body.into_inner();
        let compressed = lz4dict::compress_default(&raw[..used]);
        let mut payload = NET_CLONES.to_le_bytes().to_vec();
        payload.extend_from_slice(&compressed);
        payload
    }

    fn write_inbound_create(
        b: &mut MessageBuffer,
        obj: u16,
        uniq: u16,
        ty: NetObjEntityType,
        blob: &[u8],
    ) {
        b.write_bits_single(rec::CREATE, 3);
        b.write_bits_single(u32::from(uniq), 16);
        b.write_bits_single(u32::from(obj), 13);
        b.write_bits_single(0xABCD, 32); // creation token
        b.write_bits_single(ty as u32, 4);
        b.write_bits_single(blob.len() as u32, 12);
        b.write_bits(blob, blob.len() * 8);
    }

    fn write_inbound_sync(b: &mut MessageBuffer, obj: u16, uniq: u16, blob: &[u8]) {
        b.write_bits_single(rec::SYNC, 3);
        b.write_bits_single(u32::from(uniq), 16);
        b.write_bits_single(u32::from(obj), 13);
        b.write_bits_single(blob.len() as u32, 12);
        b.write_bits(blob, blob.len() * 8);
    }

    #[test]
    fn create_then_sync_updates_registry_and_acks() {
        let mut gs = ServerGameState::new(true, false);
        let payload = build_clone_payload(|b| {
            write_inbound_create(b, 100, 0xBEEF, NetObjEntityType::Player, &[0x01, 0x02]);
            write_inbound_sync(b, 100, 0xBEEF, &[0x03, 0x04, 0x05]);
        });
        let outcome = gs.ingest_clone_payload(42, &payload);

        assert_eq!(outcome.creates, 1);
        assert_eq!(outcome.syncs, 1);
        assert_eq!(gs.entity_count(), 1);
        let ent = gs.entity(100).unwrap();
        assert_eq!(ent.owner, 42);
        assert_eq!(ent.entity_type, NetObjEntityType::Player);
        assert_eq!(ent.data, vec![0x03, 0x04, 0x05]); // sync overwrote create blob
        assert_eq!(outcome.ack_packets.len(), 1);
    }

    #[test]
    fn ack_stream_decodes_to_matching_records() {
        use baston_protocol::rage::clone::read_ack_record;
        use baston_protocol::rage::lz4dict::decompress_plain;

        let mut gs = ServerGameState::new(true, false);
        let payload = build_clone_payload(|b| {
            write_inbound_create(b, 7, 1, NetObjEntityType::Automobile, &[0xAA]);
        });
        let outcome = gs.ingest_clone_payload(1, &payload);
        let ack = &outcome.ack_packets[0];
        // Strip [u32 type][u64 frame], decompress, read the first ack record.
        let body = decompress_plain(&ack[12..], 4096).unwrap();
        let mut buf = MessageBuffer::from_bytes(body);
        let ty = buf.read_bits_single(3).unwrap();
        let record = read_ack_record(ty, &mut buf).unwrap();
        assert_eq!(record.ty, 1); // create ack
        assert_eq!(record.object_id, 7);
        assert_eq!(record.uniqifier, 1);
    }

    #[test]
    fn foreign_owner_cannot_sync_or_remove() {
        let mut gs = ServerGameState::new(true, false);
        let create = build_clone_payload(|b| {
            write_inbound_create(b, 50, 0x1111, NetObjEntityType::Ped, &[0x09]);
        });
        gs.ingest_clone_payload(1, &create); // owner = source 1

        // Source 2 tries to sync entity 50 → ignored.
        let steal = build_clone_payload(|b| {
            write_inbound_sync(b, 50, 0x1111, &[0xFF]);
        });
        gs.ingest_clone_payload(2, &steal);
        assert_eq!(gs.entity(50).unwrap().data, vec![0x09]); // unchanged
        assert_eq!(gs.entity(50).unwrap().owner, 1);
    }

    #[test]
    fn rejected_create_does_not_leak_object_id() {
        let mut gs = ServerGameState::new(true, false);
        let obj = 123u16;
        // A create carrying an invalid (unmapped) 4-bit entity type is rejected
        // by apply_create — a trivially client-forgeable value.
        let payload = build_clone_payload(|b| {
            b.write_bits_single(rec::CREATE, 3);
            b.write_bits_single(0x9999, 16); // uniqifier
            b.write_bits_single(u32::from(obj), 13); // object id
            b.write_bits_single(0xABCD, 32); // creation token
            b.write_bits_single(0xF, 4); // entity type 15 → None (invalid)
            b.write_bits_single(1, 12); // blob length
            b.write_bits(&[0x01], 8); // blob
        });
        gs.ingest_clone_payload(7, &payload);

        // Nothing was created, and crucially the object id was NOT claimed, so
        // the id space doesn't leak (regression test for the unconditional
        // `id_used` marking).
        assert_eq!(gs.entity_count(), 0);
        assert!(
            !gs.id_used[obj as usize],
            "rejected create must not mark the object id as used"
        );
    }

    #[test]
    fn object_id_leasing_is_unique_and_marked() {
        let mut gs = ServerGameState::new(true, false);
        let (a, _pkt_a) = gs.lease_object_ids(1, 6);
        let (b, _pkt_b) = gs.lease_object_ids(2, 6);
        assert_eq!(a.len(), 6);
        assert_eq!(b.len(), 6);
        // No overlap between the two leases.
        assert!(a.iter().all(|id| !b.contains(id)));
        // Round-trip the wire packet.
        let ids = object_ids::parse_object_ids(&_pkt_a[4..]).unwrap();
        assert_eq!(ids, a);
    }

    #[test]
    fn removing_client_orphans_its_entities_and_frees_ids() {
        let mut gs = ServerGameState::new(true, false);
        let create = build_clone_payload(|b| {
            write_inbound_create(b, 60, 0x2222, NetObjEntityType::Object, &[0x01]);
        });
        gs.ingest_clone_payload(5, &create);
        assert_eq!(gs.entity_count(), 1);
        let orphaned = gs.remove_client(5);
        assert_eq!(orphaned, vec![60]);
        assert_eq!(gs.entity_count(), 0);
    }

    #[test]
    fn takeover_transfers_ownership_only_from_real_owner() {
        let mut gs = ServerGameState::new(true, false);
        let create = build_clone_payload(|b| {
            write_inbound_create(b, 70, 0x3333, NetObjEntityType::Automobile, &[0x01]);
        });
        gs.ingest_clone_payload(1, &create);
        // Source 1 (owner) hands entity 70 to net id 9.
        let takeover = build_clone_payload(|b| {
            b.write_bits_single(rec::TAKEOVER, 3);
            b.write_bits_single(9, 16); // target client
            b.write_bits_single(70, 13);
        });
        let outcome = gs.ingest_clone_payload(1, &takeover);
        assert_eq!(outcome.takeovers, vec![(70, 9)]);
        assert_eq!(gs.entity(70).unwrap().owner, 9);
    }

    #[test]
    fn non_clone_payload_is_ignored() {
        let mut gs = ServerGameState::new(true, false);
        let outcome = gs.ingest_clone_payload(1, &[0xDE, 0xAD, 0xBE, 0xEF]);
        assert!(outcome.ack_packets.is_empty());
        assert_eq!(gs.entity_count(), 0);
    }

    #[test]
    fn two_clients_see_each_other_via_server_parsed_onesync() {
        use crate::interest_ng::InterestConfig;
        use baston_protocol::rage::clone::DownlinkRecord;
        use baston_protocol::rage::packet::decode_downlink;
        use std::collections::HashMap;

        let cfg = InterestConfig::default();
        let mut gs = ServerGameState::new(true, false);
        gs.add_client(1);
        gs.add_client(2);

        // Client 1 creates its player ped (object 100); client 2 its own (200).
        gs.ingest_clone_payload(
            1,
            &build_clone_payload(|b| {
                write_inbound_create(b, 100, 0xAAAA, NetObjEntityType::Player, &[0x01, 0x02]);
            }),
        );
        gs.ingest_clone_payload(
            2,
            &build_clone_payload(|b| {
                write_inbound_create(b, 200, 0xBBBB, NetObjEntityType::Player, &[0x03, 0x04]);
            }),
        );
        assert_eq!(gs.entity_count(), 2);

        // Both players stand near each other (within AoI).
        let focus: HashMap<u32, [f32; 3]> =
            HashMap::from([(1, [0.0, 0.0, 0.0]), (2, [20.0, 0.0, 0.0])]);
        gs.update_player_positions(&focus);
        gs.tick();

        // Client 1's outbound tick must include a create for player 200
        // (owned by client 2), and NOT for its own player 100 (owner echo).
        let packets = gs.tick_client(1, focus[&1], &cfg);
        assert!(!packets.is_empty(), "client 1 should receive clone data");

        let decoded = decode_downlink(&packets[0]).unwrap();
        let created: Vec<u16> = decoded
            .records
            .iter()
            .filter_map(|r| match r {
                DownlinkRecord::Clone {
                    is_create: true,
                    object_id,
                    ..
                } => Some(*object_id),
                _ => None,
            })
            .collect();
        assert!(
            created.contains(&200),
            "client 1 must be told about player 200"
        );
        assert!(
            !created.contains(&100),
            "client 1 must not receive its own ped"
        );

        // The create record carries the owner and the parsed entity type.
        let rec_200 = decoded
            .records
            .iter()
            .find_map(|r| match r {
                DownlinkRecord::Clone {
                    object_id: 200,
                    owner_net_id,
                    entity_type,
                    ..
                } => Some((*owner_net_id, *entity_type)),
                _ => None,
            })
            .expect("player 200 create present");
        assert_eq!(rec_200.0, 2); // owner net id
        assert_eq!(rec_200.1, Some(NetObjEntityType::Player));
    }
}
