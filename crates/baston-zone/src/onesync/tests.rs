use super::*;
use baston_protocol::rage::clone::{rec, write_end as clone_end};
use baston_protocol::rage::lz4dict;
use baston_protocol::rage::packet::NET_CLONES;
use baston_protocol::rage::MessageBuffer;
use std::collections::HashMap;

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
    let entity = gs.entity(50).unwrap();
    assert_eq!(entity.create_data, vec![0x09], "create payload untouched");
    assert!(entity.data.is_empty(), "the foreign sync was not recorded");
    assert_eq!(entity.owner, 1);
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
    gs.seed_unknown_player_positions(&focus);
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

#[test]
fn world_snapshot_is_deterministically_ordered() {
    let mut gs = ServerGameState::new(true, false);
    for object_id in [300, 10, 200] {
        gs.ingest_clone_payload(
            u32::from(object_id),
            &build_clone_payload(|b| {
                write_inbound_create(b, object_id, object_id, NetObjEntityType::Object, &[1]);
            }),
        );
    }
    // The stub blobs carry no position node, so they are unplaced; place them
    // explicitly, otherwise the snapshot correctly excludes them.
    for object_id in [300, 10, 200] {
        gs.set_entity_position(object_id, [f32::from(object_id), 0.0, 0.0]);
    }
    let ids: Vec<_> = gs
        .world_snapshot()
        .into_iter()
        .map(|entity| entity.object_id)
        .collect();
    assert_eq!(ids, vec![10, 200, 300]);
}

/// An entity whose position was never decoded has no place in a spatial query:
/// including it would park it at the world origin and make it a neighbour of
/// everyone who spawns there.
#[test]
fn unplaced_entities_are_excluded_from_the_world_snapshot() {
    let mut gs = ServerGameState::new(true, false);
    gs.ingest_clone_payload(
        1,
        &build_clone_payload(|b| {
            write_inbound_create(b, 42, 42, NetObjEntityType::Object, &[1]);
        }),
    );
    assert_eq!(gs.entity_count(), 1, "the entity is tracked");
    assert!(
        gs.world_snapshot().is_empty(),
        "but it is not spatially relevant until it reports a position"
    );

    gs.set_entity_position(42, [100.0, 200.0, 30.0]);
    assert_eq!(gs.world_snapshot().len(), 1);
}

#[test]
fn batch_planning_is_bucket_isolated_and_source_ordered() {
    let cfg = crate::interest_ng::InterestConfig::default();
    let mut gs = ServerGameState::new(true, false);
    for source in [3, 1, 2] {
        gs.add_client(source);
        gs.set_player_routing_bucket(source, if source == 3 { 9 } else { 7 });
        gs.ingest_clone_payload(
            source,
            &build_clone_payload(|b| {
                write_inbound_create(
                    b,
                    source as u16 * 10,
                    source as u16,
                    NetObjEntityType::Player,
                    &[source as u8],
                );
            }),
        );
    }
    let focus = HashMap::from([
        (1, [0.0, 0.0, 0.0]),
        (2, [5.0, 0.0, 0.0]),
        (3, [5.0, 0.0, 0.0]),
    ]);
    gs.seed_unknown_player_positions(&focus);
    let ticks = gs.tick_clients(&focus, &cfg);
    let sources: Vec<_> = ticks.iter().map(|tick| tick.source).collect();
    assert_eq!(sources, vec![1, 2], "bucket 9 client has no visible peer");

    for tick in ticks {
        let decoded = baston_protocol::rage::packet::decode_downlink(&tick.packets[0]).unwrap();
        let visible: Vec<_> = decoded
            .records
            .iter()
            .filter_map(|record| match record {
                baston_protocol::rage::clone::DownlinkRecord::Clone { object_id, .. } => {
                    Some(*object_id)
                }
                _ => None,
            })
            .collect();
        assert!(
            visible.iter().all(|id| *id != 30),
            "cross-bucket entity leaked"
        );
    }
}

#[test]
fn strict_bucket_lockdown_rejects_client_create_but_still_acks_protocol() {
    use crate::routing_bucket::LockdownMode;

    let mut gs = ServerGameState::new(true, false);
    gs.set_player_routing_bucket(1, 5);
    gs.set_routing_bucket_lockdown(5, LockdownMode::Strict);
    let outcome = gs.ingest_clone_payload(
        1,
        &build_clone_payload(|b| {
            write_inbound_create(b, 77, 1, NetObjEntityType::Object, &[1]);
        }),
    );
    assert!(gs.entity(77).is_none());
    assert_eq!(outcome.rejected_mutations, 1);
    assert_eq!(outcome.ack_packets.len(), 1);
}

#[test]
fn takeover_cannot_cross_routing_buckets() {
    let mut gs = ServerGameState::new(true, false);
    gs.set_player_routing_bucket(1, 1);
    gs.set_player_routing_bucket(2, 2);
    gs.ingest_clone_payload(
        1,
        &build_clone_payload(|b| {
            write_inbound_create(b, 88, 1, NetObjEntityType::Object, &[1]);
        }),
    );
    let outcome = gs.ingest_clone_payload(
        1,
        &build_clone_payload(|b| {
            b.write_bits_single(rec::TAKEOVER, 3);
            b.write_bits_single(2, 16);
            b.write_bits_single(88, 13);
        }),
    );
    assert_eq!(gs.entity(88).unwrap().owner, 1);
    assert_eq!(outcome.rejected_mutations, 1);
}

// ── Server-created entities ──

/// Give the state a player at `position` so reassignment has a candidate.
fn with_player(gs: &mut ServerGameState, source: u32, object_id: u16, position: [f32; 3]) {
    gs.ingest_clone_payload(
        source,
        &build_clone_payload(|b| {
            write_inbound_create(b, object_id, object_id, NetObjEntityType::Player, &[1]);
        }),
    );
    gs.set_entity_position(object_id, position);
}

#[test]
fn a_server_entity_is_authored_with_its_model_and_position() {
    let mut gs = ServerGameState::new(true, false);
    let position = [120.5, -340.0, 42.0];

    let id = gs
        .spawn_server_entity(
            NetObjEntityType::Automobile,
            0xDEAD_BEEF,
            position,
            90.0,
            false,
        )
        .expect("object id available");

    let entity = gs.entity(id).expect("entity registered");
    assert!(entity.server_owned);
    assert_eq!(entity.model, Some(0xDEAD_BEEF));
    assert!(entity.position_known);
    for (axis, (got, want)) in entity.position.iter().zip(position).enumerate() {
        assert!((got - want).abs() < 0.1, "axis {axis}: {got} != {want}");
    }
    assert!(
        !entity.create_data.is_empty(),
        "a create payload was authored for the client"
    );
    assert!(entity.data.is_empty(), "there is no delta yet");
}

/// Server ids come from the top of the space, client leases from the bottom,
/// so the two allocators cannot collide until the space is genuinely full.
#[test]
fn server_ids_are_allocated_downward_away_from_client_leases() {
    let mut gs = ServerGameState::new(true, false);
    let (leased, _) = gs.lease_object_ids(1, 4);
    assert_eq!(leased, vec![1, 2, 3, 4]);

    let first = gs
        .spawn_server_entity(NetObjEntityType::Object, 1, [0.0; 3], 0.0, false)
        .unwrap();
    let second = gs
        .spawn_server_entity(NetObjEntityType::Object, 1, [0.0; 3], 0.0, false)
        .unwrap();

    assert_eq!(first, MAX_OBJECT_ID_NATIVE);
    assert_eq!(second, MAX_OBJECT_ID_NATIVE - 1);
}

/// Until a client adopts it, a server entity has no simulator — and an entity
/// nobody simulates must not be cloned to anyone.
#[test]
fn an_unowned_server_entity_is_not_broadcast_then_is_assigned() {
    let mut gs = ServerGameState::new(true, false);
    let id = gs
        .spawn_server_entity(NetObjEntityType::Object, 1, [10.0, 0.0, 0.0], 0.0, false)
        .unwrap();
    assert_eq!(gs.entity(id).unwrap().owner, 0);
    assert!(
        !gs.world_snapshot().iter().any(|e| e.object_id == id),
        "an ownerless entity is not spatially broadcast"
    );

    with_player(&mut gs, 5, 100, [12.0, 0.0, 0.0]);
    gs.reassign_ownerless_server_entities();

    assert_eq!(gs.entity(id).unwrap().owner, 5);
    assert!(gs.world_snapshot().iter().any(|e| e.object_id == id));
}

#[test]
fn reassignment_picks_the_nearest_player() {
    let mut gs = ServerGameState::new(true, false);
    with_player(&mut gs, 5, 100, [1000.0, 0.0, 0.0]);
    with_player(&mut gs, 6, 101, [10.0, 0.0, 0.0]);
    let id = gs
        .spawn_server_entity(NetObjEntityType::Object, 1, [0.0; 3], 0.0, false)
        .unwrap();

    gs.reassign_ownerless_server_entities();

    assert_eq!(gs.entity(id).unwrap().owner, 6);
}

/// The engine keeps script-created entities when their simulator leaves. A
/// scripted vehicle must not vanish because whoever was driving disconnected.
#[test]
fn a_server_entity_survives_its_owner_leaving() {
    let mut gs = ServerGameState::new(true, false);
    with_player(&mut gs, 5, 100, [0.0; 3]);
    let id = gs
        .spawn_server_entity(NetObjEntityType::Object, 1, [0.0; 3], 0.0, false)
        .unwrap();
    gs.reassign_ownerless_server_entities();
    assert_eq!(gs.entity(id).unwrap().owner, 5);

    gs.remove_client(5);

    let entity = gs.entity(id).expect("the entity outlives its simulator");
    assert_eq!(entity.owner, 0, "it is ownerless, awaiting a new simulator");

    with_player(&mut gs, 7, 101, [0.0; 3]);
    gs.reassign_ownerless_server_entities();
    assert_eq!(gs.entity(id).unwrap().owner, 7);
}

/// A client-created entity keeps the old behaviour: it dies with its owner.
#[test]
fn a_client_entity_still_dies_with_its_owner() {
    let mut gs = ServerGameState::new(true, false);
    gs.ingest_clone_payload(
        5,
        &build_clone_payload(|b| {
            write_inbound_create(b, 77, 77, NetObjEntityType::Object, &[1]);
        }),
    );
    assert!(gs.entity(77).is_some());

    gs.remove_client(5);

    assert!(gs.entity(77).is_none());
}

#[test]
fn despawn_frees_the_object_id() {
    let mut gs = ServerGameState::new(true, false);
    let id = gs
        .spawn_server_entity(NetObjEntityType::Object, 1, [0.0; 3], 0.0, false)
        .unwrap();

    assert!(gs.despawn_entity(id));
    assert!(gs.entity(id).is_none());
    assert!(!gs.despawn_entity(id), "despawning twice is not an error");

    // The id is free again, so the next server entity reuses it.
    assert_eq!(
        gs.spawn_server_entity(NetObjEntityType::Object, 1, [0.0; 3], 0.0, false),
        Some(id)
    );
}

#[test]
fn object_id_usage_separates_leased_from_used() {
    let mut gs = ServerGameState::new(true, false);
    let empty = gs.object_id_usage();
    assert_eq!(empty.max, MAX_OBJECT_ID_NATIVE as u32);
    assert_eq!(empty.used + empty.leased, 0);
    assert_eq!(empty.free, empty.max, "id 0 is not part of the pool");

    // A lease is not yet an entity: the two must not be conflated, or a
    // client hoarding ids looks the same as a full world.
    let (leased, _) = gs.lease_object_ids(5, 4);
    assert_eq!(leased.len(), 4);
    let after_lease = gs.object_id_usage();
    assert_eq!(after_lease.leased, 4);
    assert_eq!(after_lease.used, 0);
    assert_eq!(after_lease.free, after_lease.max - 4);

    // Creating on a leased id consumes the lease rather than adding to it.
    gs.ingest_clone_payload(
        5,
        &build_clone_payload(|b| {
            write_inbound_create(b, leased[0], 1, NetObjEntityType::Object, &[1]);
        }),
    );
    let after_create = gs.object_id_usage();
    assert_eq!(after_create.used, 1);
    assert_eq!(after_create.leased, 3);
    assert_eq!(
        after_create.used + after_create.leased + after_create.free,
        after_create.max
    );
}

#[test]
fn per_client_counters_answer_for_the_right_client() {
    let mut gs = ServerGameState::new(true, false);
    with_player(&mut gs, 5, 10, [0.0; 3]);
    with_player(&mut gs, 6, 20, [0.0; 3]);
    let server_id = gs
        .spawn_server_entity(NetObjEntityType::Object, 1, [0.0; 3], 0.0, false)
        .expect("server entity");

    assert_eq!(gs.owned_by(5), 1);
    assert_eq!(gs.owned_by(6), 1);
    assert_eq!(gs.server_owned_count(), 1);
    assert_eq!(gs.entity_count(), 3);
    assert!(gs.entity(server_id).is_some());

    // A source that never connected has no view, which is distinct from an
    // empty one.
    assert!(gs.client_scope_len(5).is_some());
    assert!(gs.client_scope_len(99).is_none());
    assert!(gs.client_frame_index(99).is_none());
}
