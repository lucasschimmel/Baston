//! `/api/v1` — per-key permissions, monitoring routes, control routes,
//! audit log, legacy admin-token back-compat.
// These start a real resource, so they need a scripting engine. The `lite`
// bundle has none; the js and lua bundles each run them in their own
// language, which is what keeps the API proven engine-agnostic.
#![cfg(any(feature = "scripting-js", feature = "scripting-lua"))]

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

/// Every Tier 1 module the API cares about, on.
fn all_modules_on() -> baston_modules::ModuleSet {
    let mut set = baston_modules::ModuleSet::defaults();
    set.enable(baston_modules::ModuleId::AdminApi);
    set.enable(baston_modules::ModuleId::Profiler);
    set
}

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
                        ApiPermission::ConsoleExecute,
                    ],
                },
            ],
            ..Default::default()
        },
        LEGACY_TOKEN,
    ))
}

/// A trivial resource in whichever language this bundle can run.
///
/// The API is engine-agnostic and these tests prove it: they exercise the same
/// routes against a JS resource in the `js` bundle and a Lua one in the `lua`
/// bundle. Hardcoding `.js` would have made them a JS-bundle test that happens
/// to live in the gateway.
fn write_resource(dir: &Path) {
    let (script, body) = if cfg!(feature = "scripting-js") {
        ("dist/server/index.js", "console.log('up')")
    } else {
        ("dist/server/index.lua", "print('up')")
    };
    let root = dir.join("axiom-core");
    std::fs::create_dir_all(root.join("dist/server")).unwrap();
    std::fs::write(
        root.join("manifest.json"),
        serde_json::json!({
            "name": "axiom-core",
            "server_scripts": [script],
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(root.join(script), body).unwrap();
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
    resource_manager.start_all().await;

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
                Some(Aabb::new(-4000.0, -4000.0, 0.0, 4000.0)),
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
            // The fixture exercises the full route table, so it runs with the
            // optional modules on. `profiler_routes_absent_when_module_off`
            // covers the other side.
            modules: all_modules_on(),
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

fn json_req(path: &str, token: Option<&str>, body: &str) -> Request<Body> {
    let mut b = Request::builder()
        .uri(path)
        .method("POST")
        .header("content-type", "application/json");
    if let Some(t) = token {
        b = b.header("Authorization", format!("Bearer {t}"));
    }
    b.body(Body::from(body.to_owned())).unwrap()
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
        ("/api/v1/commands/execute", "POST"),
    ] {
        let request = if path == "/api/v1/commands/execute" {
            json_req(path, Some(MONITOR_TOKEN), r#"{"command":"resmon 1"}"#)
        } else {
            req(path, method, Some(MONITOR_TOKEN))
        };
        let resp = app.clone().oneshot(request).await.unwrap();
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
    assert_eq!(resmon["scope"], "mesh");
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
    assert!(!trace["traceEvents"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn command_executor_controls_resmon_and_profiler() {
    let fx = fixture(AuditLog::disabled(), false).await;
    let app = api_router(fx.state);

    let resp = app
        .clone()
        .oneshot(json_req(
            "/api/v1/commands/execute",
            Some(MONITOR_TOKEN),
            r#"{"command":"resmon 1"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let resmon = body_json(
        app.clone()
            .oneshot(json_req(
                "/api/v1/commands/execute",
                Some(CONTROL_TOKEN),
                r#"{"command":"resmon 1"}"#,
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(resmon["command"], "resmon");
    assert_eq!(resmon["active"], true);

    let record = body_json(
        app.clone()
            .oneshot(json_req(
                "/api/v1/commands/execute",
                Some(CONTROL_TOKEN),
                r#"{"command":"profiler record 4"}"#,
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(record["status"]["active"], true);

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

    let stop = body_json(
        app.clone()
            .oneshot(json_req(
                "/api/v1/commands/execute",
                Some(CONTROL_TOKEN),
                r#"{"command":"profiler stop"}"#,
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(stop["status"]["active"], false);
    assert!(stop["status"]["latest_events"].as_u64().unwrap() > 0);

    let view = body_json(
        app.oneshot(json_req(
            "/api/v1/commands/execute",
            Some(CONTROL_TOKEN),
            r#"{"command":"profiler view"}"#,
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(view["trace_url"], "/api/v1/profiler/latest/trace");
    assert!(view["trace_events"].as_u64().unwrap() > 0);
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

#[tokio::test]
async fn profiler_routes_are_absent_when_the_module_is_off() {
    // ADR-002: a disabled module's routes do not exist. A 404 says the
    // capability is not running; a 403 would wrongly suggest it is there and
    // the caller merely lacks a permission.
    let mut fx = fixture(AuditLog::disabled(), false).await;
    fx.state.modules.disable(baston_modules::ModuleId::Profiler);
    let app = api_router(fx.state);

    for (path, method) in [
        ("/api/v1/profiler/status", "GET"),
        ("/api/v1/profiler/latest", "GET"),
        ("/api/v1/profiler/record", "POST"),
        ("/api/v1/profiler/stop", "POST"),
    ] {
        let resp = app
            .clone()
            .oneshot(req(path, method, Some(CONTROL_TOKEN)))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "{path} must not be routed with the profiler module off"
        );
    }

    // Monitoring routes outside the module keep working.
    let resp = app
        .oneshot(req("/api/v1/resmon", "GET", Some(MONITOR_TOKEN)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn console_profiler_command_refuses_when_the_module_is_off() {
    // The console path reaches the profiler without its routes, so it carries
    // its own gate — otherwise disabling the module only closes the front door.
    let mut fx = fixture(AuditLog::disabled(), false).await;
    fx.state.modules.disable(baston_modules::ModuleId::Profiler);
    let app = api_router(fx.state);

    let request = Request::builder()
        .uri("/api/v1/commands/execute")
        .method("POST")
        .header("Authorization", format!("Bearer {CONTROL_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"command":"profiler record 4"}"#))
        .unwrap();
    let resp = app.oneshot(request).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = body_json(resp).await;
    assert!(
        body["hint"]
            .as_str()
            .unwrap_or_default()
            .contains("[modules]"),
        "the refusal must point at the fix: {body}"
    );
}

#[tokio::test]
async fn status_reports_the_bundle_and_module_set() {
    // An operator reading /status must be able to tell which binary answered.
    let fx = fixture(AuditLog::disabled(), false).await;
    let app = api_router(fx.state);
    let body = body_json(
        app.oneshot(req("/api/v1/status", "GET", Some(MONITOR_TOKEN)))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(
        body["bundle"].as_str(),
        Some(baston_modules::Bundle::current().label())
    );
    let modules: Vec<&str> = body["modules"]
        .as_array()
        .expect("modules array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(modules.contains(&"profiler"), "{modules:?}");
    assert!(modules.contains(&"admin-api"), "{modules:?}");
}
