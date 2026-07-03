//! Jalon C3 end-to-end: zone emitter → NATS JetStream → gateway aggregator
//! → per-client snapshot on the UDP command channel. Skips when NATS is
//! unreachable.

use std::sync::Arc;
use std::time::Duration;

use baston_gateway::udp::{UdpCommand, UdpHandle};
use baston_gateway::StateAggregator;
use baston_protocol::entity::EntityType;
use baston_protocol::udp::read_message_type;
use baston_protocol::udp::state::{
    parse_snapshot, ClientStateUpdate, EntityOp, MSG_BASTON_SNAPSHOT,
};
use baston_protocol::{PlayerDirectory, PlayerInfo};
use baston_zone::state_sync::{state_subject, StateSyncEmitter};
use baston_zone::{EntityManager, StateIngest};

const NATS_URL: &str = "nats://127.0.0.1:4222";

fn player_update(coords: [f32; 3]) -> ClientStateUpdate {
    ClientStateUpdate {
        entity_id: None,
        entity_type: EntityType::Player,
        model_hash: 0x705E61F2,
        coords,
        heading: 0.0,
        velocity: [0.0; 3],
        health: 100.0,
        armour: 0.0,
        extra: None,
    }
}

#[tokio::test]
async fn zone_to_client_snapshot_end_to_end() {
    let Ok(client) = async_nats::connect(NATS_URL).await else {
        eprintln!("SKIPPED: NATS not reachable at {NATS_URL}");
        return;
    };

    // Zone side: two players report state; player 2 is 100m from player 1.
    let entity_manager = Arc::new(EntityManager::new());
    let ingest = Arc::new(StateIngest::new(Arc::clone(&entity_manager), 200.0));
    // Binary-protocol clients: they receive snapshots.
    ingest.mark_snapshot_subscriber(1);
    ingest.mark_snapshot_subscriber(2);
    ingest.apply(1, player_update([0.0, 0.0, 0.0])).unwrap();
    ingest.apply(2, player_update([100.0, 0.0, 0.0])).unwrap();

    // Gateway side.
    let players = Arc::new(PlayerDirectory::new());
    for source in [1u32, 2u32] {
        players.insert(PlayerInfo {
            source,
            name: format!("player-{source}"),
            identifiers: vec![],
        });
    }
    let (udp, mut cmd_rx) = UdpHandle::disconnected();
    let aggregator = StateAggregator::new(
        client.clone(),
        players,
        Arc::clone(&ingest),
        udp,
        450.0,
        20,
    )
    .spawn();

    // Give the durable consumer a moment to attach before publishing.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let zone_id = format!("test-e2e-{}", uuid::Uuid::new_v4().simple());
    let emitter = StateSyncEmitter::new(zone_id.clone(), client, entity_manager, 16);
    emitter.emit_once(&state_subject(&zone_id)).await;

    // Expect a snapshot for player 1 containing player 2's entity (and vice
    // versa), never the viewer's own entity.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut got: std::collections::HashMap<u32, Vec<EntityOp>> = Default::default();
    while got.len() < 2 && tokio::time::Instant::now() < deadline {
        let Ok(Some(cmd)) = tokio::time::timeout_at(deadline, cmd_rx.recv()).await else {
            break;
        };
        let UdpCommand::SendToSource { source, data, .. } = cmd else {
            continue;
        };
        let (ty, payload) = read_message_type(&data).unwrap();
        assert_eq!(ty, MSG_BASTON_SNAPSHOT);
        let snapshot = parse_snapshot(payload).unwrap();
        got.entry(source).or_insert(snapshot.ops);
    }
    assert_eq!(got.len(), 2, "both clients must receive a snapshot");

    let p1_entity = ingest.player_entity(1).unwrap();
    let p2_entity = ingest.player_entity(2).unwrap();
    let upserts = |ops: &[EntityOp]| -> Vec<baston_protocol::entity::EntityId> {
        ops.iter()
            .filter_map(|op| match op {
                EntityOp::Upsert(d) => Some(d.entity_id),
                EntityOp::Delete(_) => None,
            })
            .collect()
    };
    assert_eq!(upserts(&got[&1]), vec![p2_entity], "client 1 sees player 2 only");
    assert_eq!(upserts(&got[&2]), vec![p1_entity], "client 2 sees player 1 only");

    assert_eq!(aggregator.world_state().len(), 2);
}
