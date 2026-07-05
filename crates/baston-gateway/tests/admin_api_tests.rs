//! Jalon D2 — admin API tests (bearer auth, zone listing, drain).

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use baston_gateway::admin::{admin_router, AdminState};
use baston_gateway::{ConnectionRouter, GatewayMesh, ZoneRegistry};
use baston_protocol::Aabb;
use http_body_util::BodyExt;
use tower::util::ServiceExt;

async fn state_with_two_zones() -> AdminState {
    let registry = Arc::new(ZoneRegistry::new(Duration::from_secs(15)));
    registry
        .register_zone(
            "zone-a",
            Aabb::new(-4000.0, -4000.0, 0.0, 4000.0),
            "127.0.0.1:50051",
            1500,
        )
        .await
        .unwrap();
    registry
        .register_zone(
            "zone-b",
            Aabb::new(0.0, -4000.0, 4000.0, 4000.0),
            "127.0.0.1:50052",
            1500,
        )
        .await
        .unwrap();
    let router = Arc::new(ConnectionRouter::new());
    router.assign(1, "zone-a");
    router.assign(2, "zone-a");
    router.assign(3, "zone-b");
    let mesh = GatewayMesh::new(registry, router);
    AdminState {
        mesh,
        players: Arc::new(baston_gateway::PlayerRegistry::new()),
        token: "secret".into(),
    }
}

fn req(path: &str, method: &str, token: Option<&str>) -> Request<Body> {
    let mut b = Request::builder().uri(path).method(method);
    if let Some(t) = token {
        b = b.header("Authorization", format!("Bearer {t}"));
    }
    b.body(Body::empty()).unwrap()
}

#[tokio::test]
async fn rejects_missing_or_bad_token() {
    let app = admin_router(state_with_two_zones().await);
    let resp = app
        .clone()
        .oneshot(req("/admin/zones", "GET", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let resp = app
        .oneshot(req("/admin/zones", "GET", Some("wrong")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn lists_zones_with_player_counts() {
    let app = admin_router(state_with_two_zones().await);
    let resp = app
        .oneshot(req("/admin/zones", "GET", Some("secret")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let zones: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(zones.as_array().unwrap().len(), 2);
    assert_eq!(zones[0]["id"], "zone-a");
    assert_eq!(zones[0]["players"], 2);
    assert_eq!(zones[1]["id"], "zone-b");
    assert_eq!(zones[1]["players"], 1);
    assert_eq!(zones[0]["status"], "active");
}

#[tokio::test]
async fn zone_detail_and_players() {
    let app = admin_router(state_with_two_zones().await);
    let resp = app
        .clone()
        .oneshot(req("/admin/zones/zone-a", "GET", Some("secret")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let detail: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let mut players: Vec<u64> = detail["players"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap())
        .collect();
    players.sort();
    assert_eq!(players, vec![1, 2]);

    let resp = app
        .oneshot(req("/admin/zones/zone-x", "GET", Some("secret")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn drain_reroutes_players() {
    let state = state_with_two_zones().await;
    let app = admin_router(state.clone());
    let resp = app
        .oneshot(req("/admin/zones/zone-a/drain", "POST", Some("secret")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let result: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(result["players_rerouted"], 2);
    assert_eq!(state.mesh.router.count_in_zone("zone-a"), 0);
    assert_eq!(state.mesh.router.count_in_zone("zone-b"), 3);
}
