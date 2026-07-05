//! Jalon D4 — UDP rerouting (NATS forward + 50ms handoff hold) and
//! ownerless-entity handoff. Requires a running NATS (docker compose up nats).

use std::sync::Arc;
use std::time::Duration;

use baston_gateway::mesh_forward::{ingest_subject, MeshForwarder};
use baston_gateway::{ConnectionRouter, ZoneRegistry};
use baston_protocol::entity::{new_entity_id, EntityExtra, EntityState, EntityType};
use baston_protocol::udp::state::ClientStateUpdate;
use baston_zone::boundary_loop::{entity_handoff_subject, BoundaryLoop, EntityHandoffPayload};
use baston_zone::EntityManager;
use futures::StreamExt;

async fn nats() -> Option<async_nats::Client> {
    match async_nats::connect("nats://127.0.0.1:4222").await {
        Ok(c) => Some(c),
        Err(_) => {
            eprintln!("SKIP: NATS not running on 127.0.0.1:4222");
            None
        }
    }
}

fn update(x: f32) -> ClientStateUpdate {
    ClientStateUpdate {
        entity_id: None,
        entity_type: EntityType::Player,
        model_hash: 1,
        coords: [x, 0.0, 30.0],
        heading: 0.0,
        velocity: [10.0, 0.0, 0.0],
        health: 200.0,
        armour: 0.0,
        extra: None,
    }
}

async fn collect_updates(
    sub: &mut async_nats::Subscriber,
    n: usize,
    timeout: Duration,
) -> Vec<(u32, ClientStateUpdate)> {
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;
    while out.len() < n {
        let msg = tokio::time::timeout_at(deadline, sub.next()).await;
        let Ok(Some(msg)) = msg else { break };
        let ((source, update), _) =
            bincode::serde::decode_from_slice::<(u32, ClientStateUpdate), _>(
                &msg.payload,
                bincode::config::standard(),
            )
            .unwrap();
        out.push((source, update));
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn forwarder_routes_to_current_zone_and_holds_during_handoff() {
    let Some(nats) = nats().await else { return };
    let router = Arc::new(ConnectionRouter::new());
    router.assign(1, "zone-fwd-a");
    let mut sub_a = nats.subscribe(ingest_subject("zone-fwd-a")).await.unwrap();
    let mut sub_b = nats.subscribe(ingest_subject("zone-fwd-b")).await.unwrap();

    let registry = Arc::new(ZoneRegistry::new(Duration::from_secs(15)));
    registry
        .register_zone(
            "zone-fwd-a",
            baston_protocol::Aabb::new(-4000.0, -4000.0, 0.0, 4000.0),
            "127.0.0.1:1",
            100,
        )
        .await
        .unwrap();
    registry
        .register_zone(
            "zone-fwd-b",
            baston_protocol::Aabb::new(0.0, -4000.0, 4000.0, 4000.0),
            "127.0.0.1:2",
            100,
        )
        .await
        .unwrap();
    let fwd = MeshForwarder::spawn(nats.clone(), Arc::clone(&router), registry);
    fwd.forward(1, update(-100.0));
    let got = collect_updates(&mut sub_a, 1, Duration::from_secs(2)).await;
    assert_eq!(got.len(), 1, "update must reach zone A before the handoff");

    // Handoff: hold, commit to zone B, updates sent during the hold must
    // arrive at zone B (not A), none lost.
    fwd.begin_handoff_hold(1);
    router.commit_handoff(1, "zone-fwd-b").await;
    for i in 0..5 {
        fwd.forward(1, update(i as f32));
    }
    let got_b = collect_updates(&mut sub_b, 5, Duration::from_secs(2)).await;
    assert_eq!(
        got_b.len(),
        5,
        "all 5 held updates must be flushed to zone B"
    );
    // Order preserved.
    let xs: Vec<f32> = got_b.iter().map(|(_, u)| u.coords[0]).collect();
    assert_eq!(xs, vec![0.0, 1.0, 2.0, 3.0, 4.0]);
    // Nothing extra leaked to zone A.
    let extra_a = collect_updates(&mut sub_a, 1, Duration::from_millis(300)).await;
    assert!(
        extra_a.is_empty(),
        "no update may reach the old zone after commit"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn entity_handoff_spawns_in_target_zone() {
    let Some(nats) = nats().await else { return };
    let em_b = Arc::new(EntityManager::new());
    BoundaryLoop::spawn_entity_handoff_consumer(
        nats.clone(),
        "zone-eho-b".into(),
        Arc::clone(&em_b),
    );
    tokio::time::sleep(Duration::from_millis(200)).await;

    let entity = EntityState {
        entity_id: new_entity_id(),
        entity_type: EntityType::Vehicle,
        network_owner: None,
        model_hash: 0xABCDEF,
        coords: [50.0, 0.0, 30.0],
        heading: 90.0,
        velocity: [20.0, 0.0, 0.0],
        health: 1000.0,
        armour: 0.0,
        extra: EntityExtra::Vehicle {
            speed: 20.0,
            engine_health: 1000.0,
            doors_open: 0,
        },
    };
    let payload = EntityHandoffPayload {
        entity: entity.clone(),
        from_zone: "zone-eho-a".into(),
    };
    let bytes = bincode::serde::encode_to_vec(&payload, bincode::config::standard()).unwrap();
    nats.publish(entity_handoff_subject("zone-eho-b"), bytes.into())
        .await
        .unwrap();

    // Wait for the consumer to apply it.
    for _ in 0..40 {
        if em_b.get(entity.entity_id).is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let received = em_b
        .get(entity.entity_id)
        .expect("entity must exist in zone B");
    assert_eq!(received.model_hash, 0xABCDEF);
    assert_eq!(received.entity_type, EntityType::Vehicle);
}
