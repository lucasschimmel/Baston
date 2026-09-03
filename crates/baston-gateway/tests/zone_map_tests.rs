//! A zone learning its territory from the Gateway's map — real gRPC, no mocks.
//!
//! The scenario throughout is the one a map exists for: a city that owns a
//! square, and an arena carved out of the middle of it that belongs to someone
//! else. A rectangle per zone cannot express it, and the interesting part is
//! not that the arena wins but that the *city* is told the arena exists.

use std::sync::Arc;
use std::time::Duration;

use baston_gateway::{ConnectionRouter, GatewayMesh, ZoneRegistry};
use baston_protocol::mesh::gateway_service_client::GatewayServiceClient;
use baston_protocol::mesh::RegisterZoneRequest;
use baston_protocol::{Aabb, ZoneCoverage, ZoneMap};
use tonic::transport::Channel;

/// The city claims a 2km square; the arena claims 240m in the middle of it and
/// comes first, so it wins that ground.
const ARENA_MAP: &str = r#"
[[region]]
name = "maze-bank-arena"
zone = "zone-arena"
shape = "circle"
center = [0.0, 0.0]
radius = 240.0

[[region]]
name = "los-santos"
zone = "zone-city"
shape = "rect"
bounds = [-1000.0, -1000.0, 1000.0, 1000.0]

[[region]]
name = "the-countryside"
zone = "zone-country"
shape = "everywhere"
"#;

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
}

async fn start_gateway(map: Option<&str>) -> Cluster {
    let registry = match map {
        Some(text) => {
            let (map, warnings) = ZoneMap::parse(text).expect("test map should be valid");
            assert!(warnings.is_empty(), "{warnings:?}");
            ZoneRegistry::with_map(Duration::from_secs(15), map)
        }
        None => ZoneRegistry::new(Duration::from_secs(15)),
    };
    let mesh = GatewayMesh::new(Arc::new(registry), Arc::new(ConnectionRouter::new()));
    let port = free_port();
    mesh.spawn_grpc_server(format!("127.0.0.1:{port}").parse().unwrap());
    tokio::time::sleep(Duration::from_millis(150)).await;
    Cluster {
        mesh,
        addr: format!("127.0.0.1:{port}"),
    }
}

fn client(cluster: &Cluster) -> GatewayServiceClient<Channel> {
    GatewayServiceClient::new(
        Channel::from_shared(format!("http://{}", cluster.addr))
            .unwrap()
            .connect_lazy(),
    )
}

/// Register over the wire the way a zone process does, declaring bounds that
/// have nothing to do with the map.
async fn register(
    cluster: &Cluster,
    zone_id: &str,
    declared: Aabb,
) -> Result<Option<ZoneCoverage>, String> {
    let response = client(cluster)
        .register_zone(RegisterZoneRequest {
            zone_id: zone_id.to_owned(),
            bounds: Some(declared.into()),
            grpc_addr: "127.0.0.1:1".to_owned(),
            max_players: 1500,
        })
        .await
        .expect("gRPC call should succeed")
        .into_inner();

    if !response.accepted {
        return Err(response.message);
    }
    Ok(response
        .coverage
        .map(|wire| ZoneCoverage::try_from(wire).expect("coverage should decode")))
}

const WHOLE_WEST: Aabb = Aabb {
    x_min: -4000.0,
    y_min: -4000.0,
    x_max: 0.0,
    y_max: 4000.0,
};

#[tokio::test]
async fn a_zone_is_told_its_territory_when_it_registers() {
    let cluster = start_gateway(Some(ARENA_MAP)).await;

    let arena = register(&cluster, "zone-arena", WHOLE_WEST)
        .await
        .unwrap()
        .expect("a mapped gateway answers with a territory");

    assert!(arena.contains(0.0, 0.0));
    assert!(
        !arena.contains(-500.0, 0.0),
        "the map overrules the bounds the zone declared for itself"
    );
}

/// The whole point of the overlay list. Without it the city's boundary scan
/// sees a player 1000m from any edge and never prepares a handoff.
#[tokio::test]
async fn the_city_is_told_about_the_arena_carved_out_of_it() {
    let cluster = start_gateway(Some(ARENA_MAP)).await;

    let city = register(&cluster, "zone-city", WHOLE_WEST)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(city.shapes().len(), 1);
    assert_eq!(city.overlays().len(), 1, "the arena cuts into the city");
    assert!(city.contains(900.0, 900.0));
    assert!(!city.contains(0.0, 0.0), "the arena owns the middle");

    // And the distance that drives the handoff scan measures the arena rim,
    // not the far-away city edge.
    assert_eq!(city.signed_distance_to_edge(-500.0, 0.0), 260.0);
}

#[tokio::test]
async fn a_polygon_survives_the_wire_intact() {
    let cluster = start_gateway(Some(
        r#"
[[region]]
name = "l-shaped-city"
zone = "zone-city"
shape = "poly"
points = [
  [0.0, 0.0], [100.0, 0.0], [100.0, 40.0],
  [40.0, 40.0], [40.0, 100.0], [0.0, 100.0],
]

[[region]]
zone = "zone-country"
shape = "everywhere"
"#,
    ))
    .await;

    let city = register(&cluster, "zone-city", WHOLE_WEST)
        .await
        .unwrap()
        .unwrap();

    assert!(city.contains(20.0, 20.0));
    assert!(city.contains(80.0, 20.0));
    assert!(
        !city.contains(80.0, 80.0),
        "the notch must not be filled in by the trip through protobuf"
    );
}

#[tokio::test]
async fn the_catch_all_zone_owns_everything_no_one_else_claimed() {
    let cluster = start_gateway(Some(ARENA_MAP)).await;

    let country = register(&cluster, "zone-country", WHOLE_WEST)
        .await
        .unwrap()
        .unwrap();

    assert!(country.contains(9999.0, -9999.0));
    assert_eq!(country.overlays().len(), 2, "both regions above it");
    assert!(!country.contains(0.0, 0.0));
    assert!(!country.contains(900.0, 900.0));
}

#[tokio::test]
async fn a_zone_the_map_does_not_mention_is_refused_by_name() {
    let cluster = start_gateway(Some(ARENA_MAP)).await;

    let refusal = register(&cluster, "zone-typo", WHOLE_WEST)
        .await
        .expect_err("a zone with no ground must not be allowed to run");

    assert!(refusal.contains("claims no region"), "{refusal}");
    assert!(
        refusal.contains("zone-arena") && refusal.contains("zone-city"),
        "the refusal should list what the map does know: {refusal}"
    );
}

/// Routing goes through the same ordered list the zones were built from, so
/// the gateway and the zones cannot disagree about who owns a point.
#[tokio::test]
async fn routing_and_territories_agree_on_every_region() {
    let cluster = start_gateway(Some(ARENA_MAP)).await;
    for zone in ["zone-arena", "zone-city", "zone-country"] {
        register(&cluster, zone, WHOLE_WEST).await.unwrap();
    }
    let registry = &cluster.mesh.registry;

    for (x, y, expected) in [
        (0.0, 0.0, "zone-arena"),
        (900.0, 900.0, "zone-city"),
        (3000.0, 3000.0, "zone-country"),
    ] {
        assert_eq!(
            registry.find_zone_for_coords(x, y).await.as_deref(),
            Some(expected),
            "gateway routing at ({x}, {y})"
        );
        let coverage = registry.zone_coverage(expected).await.unwrap();
        assert!(
            coverage.contains(x, y),
            "{expected} was told it owns ({x}, {y}) but its territory says otherwise"
        );
    }
}

/// No map file: zones keep declaring their own rectangles, and nothing about
/// the registration handshake changes for them.
#[tokio::test]
async fn without_a_map_a_zone_keeps_the_bounds_it_declared() {
    let cluster = start_gateway(None).await;

    let coverage = register(&cluster, "zone-a", WHOLE_WEST)
        .await
        .unwrap()
        .expect("the bounds come back as a one-region territory");

    assert!(coverage.contains(-500.0, 0.0));
    assert!(!coverage.contains(500.0, 0.0));
    assert!(coverage.overlays().is_empty());
    assert_eq!(
        cluster
            .mesh
            .registry
            .find_zone_for_coords(-500.0, 0.0)
            .await
            .as_deref(),
        Some("zone-a")
    );
}
