//! Jalons D3/D4 — end-to-end handoff over real gRPC (no mocks):
//! gateway mesh + two zone meshes in-process, a simulated player walking
//! across the boundary, ghost prepare, atomic confirm, activate, release.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use baston_gateway::{ConnectionRouter, GatewayMesh, ZoneRegistry};
use baston_protocol::entity::EntityType;
use baston_protocol::udp::state::ClientStateUpdate;
use baston_protocol::{Aabb, PlayerDirectory, PlayerStateSnapshot};
use baston_zone::boundary_detector::BoundaryDetector;
use baston_zone::boundary_loop::BoundaryLoop;
use baston_zone::handoff_manager::{HandoffManager, HandoffState};
use baston_zone::mesh::{ZoneMesh, ZoneMeshHooks};
use baston_zone::{EntityManager, StateIngest};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Cluster {
    mesh: Arc<GatewayMesh>,
    gateway_addr: String,
}

async fn start_gateway() -> Cluster {
    let registry = Arc::new(ZoneRegistry::new(Duration::from_secs(15)));
    let router = Arc::new(ConnectionRouter::new());
    let mesh = GatewayMesh::new(registry, router);
    let port = free_port();
    mesh.spawn_grpc_server(format!("127.0.0.1:{port}").parse().unwrap());
    tokio::time::sleep(Duration::from_millis(150)).await;
    Cluster {
        mesh,
        gateway_addr: format!("127.0.0.1:{port}"),
    }
}

struct ZoneHarness {
    mesh: Arc<ZoneMesh>,
    activated: Arc<AtomicU32>,
    released: Arc<AtomicU32>,
    last_script_state: Arc<std::sync::Mutex<HashMap<String, String>>>,
}

async fn start_zone(cluster: &Cluster, zone_id: &str, bounds: Aabb) -> ZoneHarness {
    let activated = Arc::new(AtomicU32::new(0));
    let released = Arc::new(AtomicU32::new(0));
    let last_script_state = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let hooks = ZoneMeshHooks {
        player_count: Arc::new(|| 0),
        entity_count: Arc::new(|| 0),
        on_activate_player: {
            let activated = Arc::clone(&activated);
            let store = Arc::clone(&last_script_state);
            Arc::new(move |_source, snapshot: &PlayerStateSnapshot| {
                activated.fetch_add(1, Ordering::SeqCst);
                *store.lock().unwrap() = snapshot.script_state.clone();
            })
        },
        on_release_player: {
            let released = Arc::clone(&released);
            Arc::new(move |_source, _reason| {
                released.fetch_add(1, Ordering::SeqCst);
            })
        },
    };
    let port = free_port();
    let mesh = ZoneMesh::connect(
        zone_id.to_string(),
        bounds,
        &cluster.gateway_addr,
        format!("127.0.0.1:{port}"),
        1500,
        hooks,
    )
    .await
    .unwrap();
    mesh.spawn_grpc_server(format!("127.0.0.1:{port}").parse().unwrap());
    tokio::time::sleep(Duration::from_millis(100)).await;
    mesh.register_with_gateway().await.unwrap();
    ZoneHarness {
        mesh,
        activated,
        released,
        last_script_state,
    }
}

fn update_at(coords: [f32; 3], velocity: [f32; 3]) -> ClientStateUpdate {
    ClientStateUpdate {
        entity_id: None,
        entity_type: EntityType::Player,
        model_hash: 0x705E61F2,
        coords,
        heading: 90.0,
        velocity,
        health: 200.0,
        armour: 0.0,
        extra: None,
    }
}

/// Zone-A-side boundary loop harness around a real StateIngest.
fn boundary_loop(
    zone: &ZoneHarness,
    manager: &Arc<HandoffManager>,
    ingest: &Arc<StateIngest>,
    players: &Arc<PlayerDirectory>,
    released_local: Arc<AtomicU32>,
) -> BoundaryLoop {
    BoundaryLoop {
        detector: BoundaryDetector::new(300.0),
        manager: Arc::clone(manager),
        mesh: Arc::clone(&zone.mesh),
        ingest: Arc::clone(ingest),
        players: Arc::clone(players),
        scan_interval: Duration::from_millis(50),
        collect_script_state: Arc::new(|_source| {
            Box::pin(async {
                let mut m = HashMap::new();
                m.insert(
                    "axiom-core".to_string(),
                    r#"{"characterId":42}"#.to_string(),
                );
                m
            })
        }),
        post_handoff_cleanup: Arc::new(move |_source| {
            released_local.fetch_add(1, Ordering::SeqCst);
        }),
        nats: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn full_handoff_across_boundary() {
    let cluster = start_gateway().await;
    let zone_a = start_zone(&cluster, "zone-a", Aabb::new(-4000.0, -4000.0, 0.0, 4000.0)).await;
    let zone_b = start_zone(&cluster, "zone-b", Aabb::new(0.0, -4000.0, 4000.0, 4000.0)).await;

    // Player 1 lives in zone-a, moving east at 50 m/s, 200m from the edge.
    let em = Arc::new(EntityManager::new());
    let ingest = Arc::new(StateIngest::new(Arc::clone(&em), 10_000.0));
    let players = Arc::new(PlayerDirectory::new());
    players.insert(baston_protocol::PlayerInfo {
        source: 1,
        name: "Lucas".into(),
        identifiers: vec!["license:test".into()],
    });
    cluster.mesh.router.assign(1, "zone-a");
    ingest
        .apply(1, update_at([-200.0, 50.0, 30.0], [50.0, 0.0, 0.0]))
        .unwrap();

    let manager = HandoffManager::new("zone-a".into(), zone_a.mesh.gateway_client(), 5);
    let released_local = Arc::new(AtomicU32::new(0));
    let lp = boundary_loop(
        &zone_a,
        &manager,
        &ingest,
        &players,
        Arc::clone(&released_local),
    );

    // Scan 1: player approaches → prepare → ghost pending in zone-b.
    lp.scan_once().await;
    assert!(matches!(
        manager.state_of(1),
        Some(HandoffState::ReadyToTransfer { .. })
    ));
    assert_eq!(zone_b.mesh.ghost_state(1), Some("pending"));

    // Player crosses the border (anti-cheat: 50 m/s * ~5s within limits —
    // send intermediate updates).
    ingest
        .apply(1, update_at([-50.0, 50.0, 30.0], [50.0, 0.0, 0.0]))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(60)).await;
    ingest
        .apply(1, update_at([10.0, 50.0, 30.0], [50.0, 0.0, 0.0]))
        .unwrap();

    // Scan 2: crossing detected → ConfirmHandoff → ActivatePlayer → release.
    lp.scan_once().await;

    assert_eq!(cluster.mesh.router.zone_of(1).as_deref(), Some("zone-b"));
    assert_eq!(
        zone_b.activated.load(Ordering::SeqCst),
        1,
        "ghost must be activated in zone-b"
    );
    assert_eq!(
        released_local.load(Ordering::SeqCst),
        1,
        "zone-a must clean up locally"
    );
    assert_eq!(zone_b.mesh.ghost_state(1), Some("active"));
    assert_eq!(
        zone_b.released.load(Ordering::SeqCst),
        0,
        "zone-b must not release the player"
    );
    // script_state made it across.
    let state = zone_b.last_script_state.lock().unwrap().clone();
    assert_eq!(
        state.get("axiom-core").map(String::as_str),
        Some(r#"{"characterId":42}"#)
    );
    // Cooldown: no immediate new pending handoff.
    assert!(manager.state_of(1).is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn turnaround_cancels_preparation() {
    let cluster = start_gateway().await;
    let zone_a = start_zone(&cluster, "zone-a", Aabb::new(-4000.0, -4000.0, 0.0, 4000.0)).await;
    let _zone_b = start_zone(&cluster, "zone-b", Aabb::new(0.0, -4000.0, 4000.0, 4000.0)).await;

    let em = Arc::new(EntityManager::new());
    let ingest = Arc::new(StateIngest::new(Arc::clone(&em), 10_000.0));
    let players = Arc::new(PlayerDirectory::new());
    cluster.mesh.router.assign(1, "zone-a");
    ingest
        .apply(1, update_at([-250.0, 0.0, 30.0], [12.5, 0.0, 0.0]))
        .unwrap();

    let manager = HandoffManager::new("zone-a".into(), zone_a.mesh.gateway_client(), 5);
    let lp = boundary_loop(
        &zone_a,
        &manager,
        &ingest,
        &players,
        Arc::new(AtomicU32::new(0)),
    );

    lp.scan_once().await;
    assert!(matches!(
        manager.state_of(1),
        Some(HandoffState::ReadyToTransfer { .. })
    ));

    // Player turns around (velocity away from the edge, still in margin).
    tokio::time::sleep(Duration::from_millis(60)).await;
    ingest
        .apply(1, update_at([-251.0, 0.0, 30.0], [-12.5, 0.0, 0.0]))
        .unwrap();
    lp.scan_once().await;
    assert!(
        manager.state_of(1).is_none(),
        "preparation must be cancelled"
    );
    assert_eq!(cluster.mesh.router.zone_of(1).as_deref(), Some("zone-a"));

    // Anti-oscillation: approaching again immediately is ignored (cooldown 5s).
    tokio::time::sleep(Duration::from_millis(60)).await;
    ingest
        .apply(1, update_at([-250.0, 0.0, 30.0], [12.5, 0.0, 0.0]))
        .unwrap();
    lp.scan_once().await;
    assert!(
        manager.state_of(1).is_none(),
        "cooldown must block re-preparation"
    );
}
