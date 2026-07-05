//! Jalon D1 integration tests — real gRPC calls between GatewayMesh and
//! ZoneMesh (in-process servers on loopback ports, no mocks).

use std::sync::Arc;
use std::time::Duration;

use baston_gateway::{ConnectionRouter, GatewayMesh, ZoneRegistry};
use baston_protocol::mesh::gateway_service_client::GatewayServiceClient;
use baston_protocol::mesh::{ConfirmHandoffRequest, PrepareHandoffRequest, RegisterZoneRequest};
use baston_protocol::{Aabb, PlayerStateSnapshot};
use baston_zone::mesh::{ZoneMesh, ZoneMeshHooks};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn test_hooks() -> ZoneMeshHooks {
    ZoneMeshHooks {
        player_count: Arc::new(|| 3),
        entity_count: Arc::new(|| 9),
        on_activate_player: Arc::new(|_, _| {}),
        on_release_player: Arc::new(|_, _| {}),
    }
}

fn snapshot(source: u32) -> PlayerStateSnapshot {
    PlayerStateSnapshot {
        source_id: source,
        name: format!("player-{source}"),
        identifiers: vec![format!("license:test-{source}")],
        coords: [-10.0, 0.0, 30.0],
        heading: 90.0,
        velocity: [8.0, 0.0, 0.0],
        health: 200.0,
        armour: 0.0,
        current_weapon: 0,
        owned_entities: vec![],
        script_state: Default::default(),
    }
}

struct TestCluster {
    mesh: Arc<GatewayMesh>,
    gateway_addr: String,
}

async fn start_gateway() -> TestCluster {
    let registry = Arc::new(ZoneRegistry::new(Duration::from_secs(15)));
    let router = Arc::new(ConnectionRouter::new());
    let mesh = GatewayMesh::new(registry, router);
    let port = free_port();
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    mesh.spawn_grpc_server(addr);
    tokio::time::sleep(Duration::from_millis(150)).await;
    TestCluster {
        mesh,
        gateway_addr: format!("127.0.0.1:{port}"),
    }
}

async fn start_zone(cluster: &TestCluster, zone_id: &str, bounds: Aabb) -> Arc<ZoneMesh> {
    let port = free_port();
    let zone = ZoneMesh::connect(
        zone_id.to_string(),
        bounds,
        &cluster.gateway_addr,
        format!("127.0.0.1:{port}"),
        1500,
        test_hooks(),
    )
    .await
    .unwrap();
    zone.spawn_grpc_server(format!("127.0.0.1:{port}").parse().unwrap());
    tokio::time::sleep(Duration::from_millis(100)).await;
    zone.register_with_gateway().await.unwrap();
    zone
}

#[tokio::test]
async fn zone_registers_and_players_route_by_coords() {
    let cluster = start_gateway().await;
    let _zone_a = start_zone(&cluster, "zone-a", Aabb::new(-4000.0, -4000.0, 0.0, 4000.0)).await;
    let _zone_b = start_zone(&cluster, "zone-b", Aabb::new(0.0, -4000.0, 4000.0, 4000.0)).await;

    assert!(cluster.mesh.registry.contains("zone-a").await);
    assert!(cluster.mesh.registry.contains("zone-b").await);
    assert_eq!(
        cluster
            .mesh
            .registry
            .find_zone_for_coords(-500.0, 200.0)
            .await
            .as_deref(),
        Some("zone-a")
    );
    assert_eq!(
        cluster
            .mesh
            .registry
            .find_zone_for_coords(1500.0, -300.0)
            .await
            .as_deref(),
        Some("zone-b")
    );
}

#[tokio::test]
async fn heartbeat_updates_load_and_evicted_zone_is_refused() {
    let cluster = start_gateway().await;
    let zone = start_zone(&cluster, "zone-a", Aabb::new(-4000.0, -4000.0, 0.0, 4000.0)).await;

    // Heartbeat over real gRPC.
    let resp = zone
        .gateway_client()
        .heartbeat(baston_protocol::mesh::HeartbeatRequest {
            zone_id: "zone-a".into(),
            player_count: 42,
            entity_count: 100,
        })
        .await
        .unwrap();
    assert!(resp.get_ref().known);

    // Heartbeat for an unknown zone → known=false (must re-register).
    let resp = zone
        .gateway_client()
        .heartbeat(baston_protocol::mesh::HeartbeatRequest {
            zone_id: "zone-ghost".into(),
            player_count: 0,
            entity_count: 0,
        })
        .await
        .unwrap();
    assert!(!resp.get_ref().known);
}

#[tokio::test]
async fn new_player_routed_least_loaded_without_coords() {
    let cluster = start_gateway().await;
    let _zone_a = start_zone(&cluster, "zone-a", Aabb::new(-4000.0, -4000.0, 0.0, 4000.0)).await;
    cluster.mesh.registry.heartbeat("zone-a", 10, 0).await;

    let assigned = cluster.mesh.route_new_player(1, None).await;
    assert_eq!(assigned.as_deref(), Some("zone-a"));
    assert_eq!(cluster.mesh.router.zone_of(1).as_deref(), Some("zone-a"));

    // With coords covered by zone-a.
    let assigned = cluster
        .mesh
        .route_new_player(2, Some((-1200.0, 50.0)))
        .await;
    assert_eq!(assigned.as_deref(), Some("zone-a"));
}

#[tokio::test]
async fn prepare_handoff_creates_ghost_in_target_zone() {
    let cluster = start_gateway().await;
    let _zone_a = start_zone(&cluster, "zone-a", Aabb::new(-4000.0, -4000.0, 0.0, 4000.0)).await;
    let zone_b = start_zone(&cluster, "zone-b", Aabb::new(0.0, -4000.0, 4000.0, 4000.0)).await;

    let mut gw = GatewayServiceClient::connect(format!("http://{}", cluster.gateway_addr))
        .await
        .unwrap();
    let resp = gw
        .prepare_handoff(PrepareHandoffRequest {
            player_id: 1,
            from_zone: "zone-a".into(),
            target_zone: String::new(), // auto-resolve from predicted coords
            predicted_x: 50.0,
            predicted_y: 0.0,
            snapshot: snapshot(1).encode(),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(resp.ready, "prepare failed: {}", resp.message);
    assert_eq!(resp.target_zone, "zone-b");
    assert_eq!(zone_b.ghost_state(1), Some("pending"));
}

#[tokio::test]
async fn confirm_handoff_atomically_updates_routing() {
    let cluster = start_gateway().await;
    let _zone_a = start_zone(&cluster, "zone-a", Aabb::new(-4000.0, -4000.0, 0.0, 4000.0)).await;
    let _zone_b = start_zone(&cluster, "zone-b", Aabb::new(0.0, -4000.0, 4000.0, 4000.0)).await;
    cluster.mesh.router.assign(1, "zone-a");

    let mut gw = GatewayServiceClient::connect(format!("http://{}", cluster.gateway_addr))
        .await
        .unwrap();
    let resp = gw
        .confirm_handoff(ConfirmHandoffRequest {
            player_id: 1,
            from_zone: "zone-a".into(),
            to_zone: "zone-b".into(),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(resp.ok);
    assert_eq!(cluster.mesh.router.zone_of(1).as_deref(), Some("zone-b"));

    // Confirm toward an unregistered zone is refused.
    let resp = gw
        .confirm_handoff(ConfirmHandoffRequest {
            player_id: 1,
            from_zone: "zone-b".into(),
            to_zone: "zone-nope".into(),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(!resp.ok);
    assert_eq!(cluster.mesh.router.zone_of(1).as_deref(), Some("zone-b"));
}

#[tokio::test]
async fn register_zone_rejects_missing_bounds() {
    let cluster = start_gateway().await;
    let mut gw = GatewayServiceClient::connect(format!("http://{}", cluster.gateway_addr))
        .await
        .unwrap();
    let err = gw
        .register_zone(RegisterZoneRequest {
            zone_id: "zone-x".into(),
            bounds: None,
            grpc_addr: "127.0.0.1:1".into(),
            max_players: 10,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}
