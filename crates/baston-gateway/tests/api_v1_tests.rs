//! `/api/v1` — per-key permissions, monitoring routes, control routes,
//! audit log, legacy admin-token back-compat.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use baston_config::{ApiConfig, ApiKey, ApiPermission};
use baston_gateway::api::{api_router, ApiState, AuditLog, KeyRing};
use baston_gateway::udp::UdpCommand;
use baston_gateway::{ConnectionRouter, GatewayMesh, ZoneRegistry};
use baston_protocol::{Aabb, PlayerInfo};
use baston_scripting::{DeferralRegistry, ScriptHost};
use baston_zone::resource_loader::ResourceManager;
use http_body_util::BodyExt;
use tower::util::ServiceExt;

const MONITOR_TOKEN: &str = "monitor-token-0123456789abcdef0123";
const CONTROL_TOKEN: &str = "control-token-0123456789abcdef0123";
const LEGACY_TOKEN: &str = "legacy-admin-token";

fn keyring() -> Arc<KeyRing> {
    Arc::new(KeyRing::from_config(
        &ApiConfig {
            keys: vec![
                ApiKey {
                    name: "monitor-bot".into(),
                    token: MONITOR_TOKEN.into(),
                    permissions: vec![ApiPermission::MonitorRead],
                },
                ApiKey {
                    name: "panel".into(),
                    token: CONTROL_TOKEN.into(),
                    permissions: vec![
                        ApiPermission::MonitorRead,
                        ApiPermission::ResourceControl,
                        ApiPermission::PlayerKick,
                        ApiPermission::ZoneDrain,
                        ApiPermission::ProfilerControl,
                        ApiPermission::ProfilerRead,
                    ],
                },
            ],
            ..Default::default()
        },
        LEGACY_TOKEN,
    ))
}

fn write_resource(dir: &Path) {
    let root = dir.join("axiom-core");
    std::fs::create_dir_all(root.join("dist/server")).unwrap();
    std::fs::write(
        root.join("manifest.json"),
        serde_json::json!({
            "name": "axiom-core",
            "server_scripts": ["dist/server/index.js"],
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(root.join("dist/server/index.js"), "console.log('up')").unwrap();
}

struct Fixture {
    state: ApiState,
    udp_rx: tokio::sync::mpsc::Receiver<UdpCommand>,
    _dir: tempfile::TempDir,
}

async fn fixture(audit: AuditLog, with_mesh: bool) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    write_resource(dir.path());
    let deferrals = Arc::new(DeferralRegistry::new());
    let players = Arc::new(baston_gateway::PlayerRegistry::new());
    let script_host = ScriptHost::spawn(deferrals, Arc::clone(&players)).unwrap();
    let observability = script_host.observability();
    let resource_manager = ResourceManager::new(script_host, dir.path().to_owned());
    resource_manager.discover().await.unwrap();
    resource_manager.start_all().await.unwrap();

    players.insert(PlayerInfo {
        source: 7,
        name: "Lucas".into(),
        identifiers: vec!["ip:127.0.0.1".into()],
    });

    let mesh = if with_mesh {
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
        let router = Arc::new(ConnectionRouter::new());
        router.assign(7, "zone-a");
        Some(GatewayMesh::new(registry, router))
    } else {
        None
    };

    let (udp, udp_rx) = baston_gateway::udp::UdpHandle::disconnected();
    Fixture {
        state: ApiState {
            keyring: keyring(),
            audit,
            players,
            resource_manager,
            observability,
            mesh,
            udp: Some(udp),
            server_name: "BASTON Test".into(),
            max_players: 32,
            started_at: Instant::now(),
        },
        udp_rx,
        _dir: dir,
    }
}

fn req(path: &str, method: &str, token: Option<&str>) -> Request<Body> {
    let mut b = Request::builder().uri(path).method(method);
    if let Some(t) = token {
        b = b.header("Authorization", format!("Bearer {t}"));
    }
    b.body(Body::empty()).unwrap()
}

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn monitoring_key_reads_but_cannot_control() {
    let fx = fixture(AuditLog::disabled(), true).await;
    let app = api_router(fx.state);

    // Reads pass.
    for path in [
        "/api/v1/status",
        "/api/v1/players",
        "/api/v1/zones",
        "/api/v1/resources",
    ] {
        let resp = app
            .clone()
            .oneshot(req(path, "GET", Some(MONITOR_TOKEN)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "GET {path}");
    }

    // Control routes are 403 for a monitor-only key.
    for (path, method) in [
        ("/api/v1/players/7/kick", "POST"),
        ("/api/v1/resources/axiom-core/stop", "POST"),
        ("/api/v1/zones/zone-a/drain", "POST"),
    ] {
        let resp = app
            .clone()
            .oneshot(req(path, method, Some(MONITOR_TOKEN)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{method} {path}");
    }

    // Unknown token → 401.
    let resp = app
        .oneshot(req("/api/v1/status", "GET", Some("nope")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn status_and_players_report_real_data() {
    let fx = fixture(AuditLog::disabled(), true).await;
    let app = api_router(fx.state);

    let status = body_json(
        app.clone()
            .oneshot(req("/api/v1/status", "GET", Some(MONITOR_TOKEN)))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status["name"], "BASTON Test");
    assert_eq!(status["players"], 1);
    assert_eq!(status["max_players"], 32);
    assert_eq!(status["zones"], 1);

    let players = body_json(
        app.clone()
            .oneshot(req("/api/v1/players", "GET", Some(MONITOR_TOKEN)))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(players[0]["source"], 7);
    assert_eq!(players[0]["name"], "Lucas");
    assert_eq!(players[0]["zone"], "zone-a");

    let resources = body_json(
        app.clone()
            .oneshot(req("/api/v1/resources", "GET", Some(MONITOR_TOKEN)))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(resources[0]["name"], "axiom-core");
    assert_eq!(resources[0]["state"], "started");

    let resmon = body_json(
        app.oneshot(req("/api/v1/resmon", "GET", Some(MONITOR_TOKEN)))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(resmon["scope"], "gateway");
    assert_eq!(resmon["resources"][0]["name"], "axiom-core");
}

#[tokio::test]
async fn profiler_permissions_and_trace_flow() {
    let fx = fixture(AuditLog::disabled(), false).await;
    let app = api_router(fx.state);

    let resp = app
        .clone()
        .oneshot(req(
            "/api/v1/profiler/latest/trace",
            "GET",
            Some(MONITOR_TOKEN),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let request = Request::builder()
        .uri("/api/v1/profiler/record")
        .method("POST")
        .header("Authorization", format!("Bearer {CONTROL_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"frames":4,"scope":"server","include_native_calls":true}"#,
        ))
        .unwrap();
    let resp = app.clone().oneshot(request).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(req(
            "/api/v1/resources/axiom-core/restart",
            "POST",
            Some(CONTROL_TOKEN),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(req("/api/v1/profiler/stop", "POST", Some(CONTROL_TOKEN)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let trace = body_json(
        app.oneshot(req(
            "/api/v1/profiler/latest/trace",
            "GET",
            Some(CONTROL_TOKEN),
        ))
        .await
        .unwrap(),
    )
    .await;
    assert!(trace["traceEvents"].as_array().unwrap().len() > 0);
}

#[tokio::test]
async fn kick_drops_the_udp_peer_and_audits() {
    let dir = tempfile::tempdir().unwrap();
    let audit_path = dir.path().join("audit.jsonl");
    let mut fx = fixture(AuditLog::spawn(audit_path.clone()), false).await;
    let app = api_router(fx.state);

    let resp = app
        .clone()
        .oneshot(req("/api/v1/players/7/kick", "POST", Some(CONTROL_TOKEN)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // The ENet drop command was issued for the right source.
    match fx.udp_rx.try_recv() {
        Ok(UdpCommand::DropSource { source }) => assert_eq!(source, 7),
        other => panic!("expected DropSource, got {other:?}"),
    }

    // Unknown player → 404, no drop.
    let resp = app
        .oneshot(req("/api/v1/players/999/kick", "POST", Some(CONTROL_TOKEN)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(fx.udp_rx.try_recv().is_err());

    // Audit line lands on disk with the key name.
    let mut content = String::new();
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        content = tokio::fs::read_to_string(&audit_path)
            .await
            .unwrap_or_default();
        if content.lines().count() >= 2 {
            break;
        }
    }
    let first: serde_json::Value = serde_json::from_str(content.lines().next().unwrap()).unwrap();
    assert_eq!(first["key"], "panel");
    assert_eq!(first["action"], "player.kick");
    assert_eq!(first["outcome"], "ok");
}

#[tokio::test]
async fn resource_control_stops_and_restarts() {
    let fx = fixture(AuditLog::disabled(), false).await;
    let app = api_router(fx.state);

    let resp = app
        .clone()
        .oneshot(req(
            "/api/v1/resources/axiom-core/stop",
            "POST",
            Some(CONTROL_TOKEN),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resources = body_json(
        app.clone()
            .oneshot(req("/api/v1/resources", "GET", Some(CONTROL_TOKEN)))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(resources[0]["state"], "stopped");

    let resp = app
        .clone()
        .oneshot(req(
            "/api/v1/resources/axiom-core/restart",
            "POST",
            Some(CONTROL_TOKEN),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Invalid action → 400.
    let resp = app
        .oneshot(req(
            "/api/v1/resources/axiom-core/explode",
            "POST",
            Some(CONTROL_TOKEN),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn legacy_admin_token_has_full_api_access() {
    let fx = fixture(AuditLog::disabled(), true).await;
    let app = api_router(fx.state);

    let resp = app
        .clone()
        .oneshot(req("/api/v1/status", "GET", Some(LEGACY_TOKEN)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .oneshot(req(
            "/api/v1/zones/zone-a/drain",
            "POST",
            Some(LEGACY_TOKEN),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn zones_empty_without_mesh_and_drain_404s() {
    let fx = fixture(AuditLog::disabled(), false).await;
    let app = api_router(fx.state);

    let zones = body_json(
        app.clone()
            .oneshot(req("/api/v1/zones", "GET", Some(MONITOR_TOKEN)))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(zones, serde_json::json!([]));

    let resp = app
        .oneshot(req(
            "/api/v1/zones/zone-a/drain",
            "POST",
            Some(CONTROL_TOKEN),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
