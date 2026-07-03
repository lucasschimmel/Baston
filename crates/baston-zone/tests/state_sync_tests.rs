//! Jalon C2 integration tests against a real NATS server (docker compose).
//! Tests skip (with a loud message) when NATS is unreachable so the suite
//! stays green on machines without the dev stack.

use std::sync::Arc;
use std::time::{Duration, Instant};

use baston_protocol::entity::{
    new_entity_id, DirtyEntity, EntityExtra, EntityState, EntityStateUpdate, EntityType,
};
use baston_zone::state_sync::{setup_nats_stream, state_subject, StateSyncEmitter};
use baston_zone::EntityManager;
use futures::StreamExt;

const NATS_URL: &str = "nats://127.0.0.1:4222";

async fn connect_or_skip() -> Option<async_nats::Client> {
    match async_nats::connect(NATS_URL).await {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("SKIPPED: NATS not reachable at {NATS_URL}: {e}");
            None
        }
    }
}

fn spawn_entities(mgr: &EntityManager, n: usize) -> Vec<baston_protocol::entity::EntityId> {
    (0..n)
        .map(|i| {
            let id = new_entity_id();
            mgr.spawn_entity(EntityState {
                entity_id: id,
                entity_type: EntityType::Player,
                network_owner: Some(i as u32 + 1),
                model_hash: 0x705E61F2,
                coords: [i as f32, 0.0, 0.0],
                heading: 0.0,
                velocity: [0.0; 3],
                health: 100.0,
                armour: 0.0,
                extra: EntityExtra::Player {
                    is_in_vehicle: false,
                    vehicle_id: None,
                },
            });
            id
        })
        .collect()
}

#[tokio::test]
async fn hundred_dirty_entities_published_under_2ms() {
    let Some(client) = connect_or_skip().await else {
        return;
    };
    setup_nats_stream(&client).await.unwrap();

    let mgr = Arc::new(EntityManager::new());
    let ids = spawn_entities(&mgr, 100);
    mgr.flush_dirty(); // drop the CREATED batch
    for (i, id) in ids.iter().enumerate() {
        mgr.update_entity(
            *id,
            EntityStateUpdate {
                coords: Some([i as f32, 1.0, 0.0]),
                ..Default::default()
            },
        );
    }

    let zone_id = format!("test-{}", uuid::Uuid::new_v4().simple());
    let subject = state_subject(&zone_id);
    let mut sub = client.subscribe(subject.clone()).await.unwrap();

    let emitter = StateSyncEmitter::new(zone_id, client.clone(), Arc::clone(&mgr), 16);
    let start = Instant::now();
    emitter.emit_once(&subject).await;
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(2),
        "publish of 100 dirty entities took {elapsed:?} (target < 2ms)"
    );

    let msg = tokio::time::timeout(Duration::from_secs(2), sub.next())
        .await
        .expect("timed out waiting for NATS message")
        .expect("subscription closed");
    let (batch, _): (Vec<DirtyEntity>, usize) =
        bincode::serde::decode_from_slice(&msg.payload, bincode::config::standard()).unwrap();
    assert_eq!(batch.len(), 100);
}

#[tokio::test]
async fn empty_tick_publishes_nothing() {
    let Some(client) = connect_or_skip().await else {
        return;
    };
    setup_nats_stream(&client).await.unwrap();

    let mgr = Arc::new(EntityManager::new());
    let zone_id = format!("test-{}", uuid::Uuid::new_v4().simple());
    let subject = state_subject(&zone_id);
    let mut sub = client.subscribe(subject.clone()).await.unwrap();

    let emitter = StateSyncEmitter::new(zone_id, client.clone(), Arc::clone(&mgr), 16);
    emitter.emit_once(&subject).await;

    let raced = tokio::time::timeout(Duration::from_millis(200), sub.next()).await;
    assert!(raced.is_err(), "no message expected on an empty tick");
}
