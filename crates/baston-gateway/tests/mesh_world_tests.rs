//! Server-authored entities across the mesh boundary — real gRPC between a
//! zone's `ZoneWorldControl` and the gateway's `GatewayService`, no mocks.
//!
//! The thing under test is the whole chain a script sets off: lease a block of
//! network ids, mint one synchronously, ship the spawn, and have it land on the
//! same queue the gateway's own scripts feed.

use std::sync::Arc;
use std::time::Duration;

use baston_gateway::{ConnectionRouter, GatewayMesh, GatewayWorldControl, ZoneRegistry};
use baston_protocol::mesh::gateway_service_client::GatewayServiceClient;
use baston_protocol::mesh::LeaseNetworkIdsRequest;
use baston_protocol::Aabb;
use baston_scripting::{ScriptEntityType, WorldCommand, WorldControl};
use baston_zone::world_control::{WorldUnavailable, ZoneWorldControl};
use tokio::sync::mpsc::Receiver;
use tonic::transport::Channel;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Cluster {
    mesh: Arc<GatewayMesh>,
    addr: String,
    /// What the UDP task would drain on its next sync tick.
    commands: Option<Receiver<WorldCommand>>,
}

/// `with_world` false models a gateway running the msgRoute relay: no
/// authoritative world to create anything in.
async fn start_gateway(with_world: bool) -> Cluster {
    let mesh = GatewayMesh::new(
        Arc::new(ZoneRegistry::new(Duration::from_secs(15))),
        Arc::new(ConnectionRouter::new()),
    );
    let commands = with_world.then(|| {
        let (control, rx) = GatewayWorldControl::new();
        mesh.set_world_control(control);
        rx
    });
    let port = free_port();
    mesh.spawn_grpc_server(format!("127.0.0.1:{port}").parse().unwrap());
    tokio::time::sleep(Duration::from_millis(150)).await;
    Cluster {
        mesh,
        addr: format!("127.0.0.1:{port}"),
        commands,
    }
}

async fn client(cluster: &Cluster) -> GatewayServiceClient<Channel> {
    GatewayServiceClient::new(
        Channel::from_shared(format!("http://{}", cluster.addr))
            .unwrap()
            .connect_lazy(),
    )
}

async fn register(cluster: &Cluster, zone_id: &str) {
    cluster
        .mesh
        .registry
        .register_zone(
            zone_id,
            Aabb::new(-4000.0, -4000.0, 0.0, 4000.0),
            "127.0.0.1:1",
            1500,
        )
        .await
        .unwrap();
}

async fn next_command(rx: &mut Receiver<WorldCommand>) -> WorldCommand {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("no world command reached the gateway")
        .expect("command channel closed")
}

/// The whole point: a script in a zone process creates an entity, and the
/// gateway — the process that owns the world clients talk to — is the one that
/// spawns it.
#[tokio::test]
async fn a_zone_script_spawns_an_entity_in_the_gateways_world() {
    let mut cluster = start_gateway(true).await;
    register(&cluster, "zone-a").await;
    let mut commands = cluster.commands.take().unwrap();

    let control = ZoneWorldControl::connect("zone-a".to_owned(), client(&cluster).await)
        .await
        .expect("the gateway has a world, so the lease must be granted");

    // Synchronous, no I/O: this is what `CreateVehicle` returns on the spot.
    let network_id = control
        .reserve_network_id()
        .expect("a freshly leased block has ids");
    assert_ne!(network_id, 0, "0 is the invalid handle");

    control.submit(WorldCommand::Spawn {
        network_id,
        entity_type: ScriptEntityType::Vehicle,
        model: 0x0BD8_A5B5,
        position: [123.0, -456.0, 30.0],
        heading: 180.0,
        dynamic: false,
    });

    assert_eq!(
        next_command(&mut commands).await,
        WorldCommand::Spawn {
            network_id,
            entity_type: ScriptEntityType::Vehicle,
            model: 0x0BD8_A5B5,
            position: [123.0, -456.0, 30.0],
            heading: 180.0,
            dynamic: false,
        }
    );
}

/// A `Despawn` must never overtake the `Spawn` it undoes, or the world keeps an
/// entity the script deleted. One drain task sending sequentially is what
/// guarantees it.
#[tokio::test]
async fn commands_arrive_in_the_order_the_script_wrote_them() {
    let mut cluster = start_gateway(true).await;
    register(&cluster, "zone-a").await;
    let mut commands = cluster.commands.take().unwrap();

    let control = ZoneWorldControl::connect("zone-a".to_owned(), client(&cluster).await)
        .await
        .unwrap();

    let ids: Vec<u32> = (0..8)
        .map(|_| control.reserve_network_id().unwrap())
        .collect();
    for id in &ids {
        control.submit(WorldCommand::Spawn {
            network_id: *id,
            entity_type: ScriptEntityType::Object,
            model: 1,
            position: [0.0, 0.0, 0.0],
            heading: 0.0,
            dynamic: false,
        });
    }
    for id in &ids {
        control.submit(WorldCommand::Despawn { network_id: *id });
    }

    for id in &ids {
        match next_command(&mut commands).await {
            WorldCommand::Spawn { network_id, .. } => assert_eq!(network_id, *id),
            other => panic!("expected a spawn for {id}, got {other:?}"),
        }
    }
    for id in &ids {
        assert_eq!(
            next_command(&mut commands).await,
            WorldCommand::Despawn { network_id: *id }
        );
    }
}

/// Blocks come out of the gateway's own descending allocator, so no two zones
/// can mint the same id. That is what makes "spawn refused: id already in use"
/// unreachable from a zone.
#[tokio::test]
async fn two_zones_never_receive_overlapping_ids() {
    let cluster = start_gateway(true).await;
    register(&cluster, "zone-a").await;
    register(&cluster, "zone-b").await;

    let a = ZoneWorldControl::connect("zone-a".to_owned(), client(&cluster).await)
        .await
        .unwrap();
    let b = ZoneWorldControl::connect("zone-b".to_owned(), client(&cluster).await)
        .await
        .unwrap();

    let mut seen = std::collections::HashSet::new();
    for _ in 0..200 {
        assert!(seen.insert(a.reserve_network_id().unwrap()));
        assert!(seen.insert(b.reserve_network_id().unwrap()));
    }
    assert_eq!(seen.len(), 400);
    assert!(!seen.contains(&0), "0 is never handed out");
}

/// Draining a block must refill from the gateway rather than start lying. The
/// block is 256 wide with a 64 low-water mark, so this crosses both.
#[tokio::test]
async fn a_drained_block_refills_instead_of_running_out() {
    let cluster = start_gateway(true).await;
    register(&cluster, "zone-a").await;

    let control = ZoneWorldControl::connect("zone-a".to_owned(), client(&cluster).await)
        .await
        .unwrap();

    let mut seen = std::collections::HashSet::new();
    for i in 0..600 {
        let id = control
            .reserve_network_id()
            .unwrap_or_else(|| panic!("ran dry after {i} ids — the refill did not land"));
        assert!(seen.insert(id), "id {id} handed out twice");
        // The refill is a round trip; yield so its task can run.
        if i % 32 == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

/// With OneSync off there is no authoritative world. The zone must be told so
/// it can leave its script host on `NoWorldControl`, rather than mint ids for
/// spawns that would be dropped at the tick.
#[tokio::test]
async fn a_gateway_without_a_world_refuses_the_lease() {
    let cluster = start_gateway(false).await;
    register(&cluster, "zone-a").await;

    let error = ZoneWorldControl::connect("zone-a".to_owned(), client(&cluster).await)
        .await
        .expect_err("a gateway with no world must refuse");
    let WorldUnavailable::Refused(message) = error;
    assert!(
        message.contains("onesync"),
        "the refusal must say why: {message}"
    );
}

/// An evicted zone must re-register before it can mint anything — its entities
/// would otherwise outlive the gateway's knowledge of it.
#[tokio::test]
async fn an_unregistered_zone_cannot_lease_ids() {
    let cluster = start_gateway(true).await;

    let response = client(&cluster)
        .await
        .lease_network_ids(LeaseNetworkIdsRequest {
            zone_id: "ghost-zone".to_owned(),
            count: 16,
        })
        .await
        .unwrap()
        .into_inner();

    assert!(!response.ok);
    assert_eq!(response.granted, 0);
    assert!(
        response.message.contains("not registered"),
        "{}",
        response.message
    );
}
