use super::*;
use baston_protocol::rage::clone::{rec, write_end as clone_end};
use baston_protocol::rage::lz4dict;
use baston_protocol::rage::packet::NET_CLONES;
use baston_protocol::rage::MessageBuffer;

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
    assert_eq!(
        gs.ids[obj as usize],
        IdState::Free,
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
    assert_eq!(gs.ids[60], IdState::Free);
}

#[test]
fn used_id_survives_leasing_client_disconnect_after_takeover() {
    let mut gs = ServerGameState::new(true, false);
    // Client 1 leases ids, then creates an entity on the first leased id:
    // the lease is consumed (Leased → Used).
    let (leased, _pkt) = gs.lease_object_ids(1, 6);
    let obj = leased[0];
    gs.ingest_clone_payload(
        1,
        &build_clone_payload(|b| {
            write_inbound_create(b, obj, 0x4444, NetObjEntityType::Automobile, &[0x01]);
        }),
    );
    assert_eq!(gs.ids[obj as usize], IdState::Used);

    // Ownership moves to client 2, then the leasing client disconnects.
    let takeover = build_clone_payload(|b| {
        b.write_bits_single(rec::TAKEOVER, 3);
        b.write_bits_single(2, 16);
        b.write_bits_single(u32::from(obj), 13);
    });
    gs.ingest_clone_payload(1, &takeover);
    gs.remove_client(1);

    // The entity is alive under client 2; its id must not be re-leasable.
    assert!(gs.entity(obj).is_some());
    assert_eq!(gs.ids[obj as usize], IdState::Used);
    // Untouched leases from client 1 were released.
    assert!(leased[1..]
        .iter()
        .all(|&id| gs.ids[id as usize] == IdState::Free));
    let (fresh, _pkt) = gs.lease_object_ids(3, 6);
    assert!(!fresh.contains(&obj), "live id must not be re-leased");
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
