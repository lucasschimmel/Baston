//! Milestones A4 + A5 exit-criterion tests, run against the real router with
//! a real script host and a temp resources directory.
// Driven by JavaScript resources (deferral handlers, exports, dist/ layouts),
// so they run in the bundles that contain V8. The Lua runtime has its own
// tests in baston-scripting; see docs/guides/modules.md for what it covers.
#![cfg(feature = "scripting-js")]

use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use baston_config::{BastonConfig, OneSyncMode};
use baston_gateway::{router, AppState, PlayerRegistry};
use baston_scripting::{DeferralRegistry, ScriptHost};
use baston_zone::resource_loader::ResourceManager;
use http_body_util::BodyExt;
use tower::ServiceExt;

const AXIOM_CORE_JS: &str = r#"
AddEventHandler('playerConnecting', function (name, setKickReason, deferrals) {
  deferrals.defer();
  console.log('[axiom-core] player connecting: ' + name);
  deferrals.update('Checking whitelist...');
  deferrals.done();
});
AddEventHandler('playerDropped', function (reason) {
  console.log('[axiom-core] player dropped: ' + reason);
});
"#;

fn write_axiom_core(dir: &Path, script: &str) {
    let root = dir.join("axiom-core");
    std::fs::create_dir_all(root.join("dist/server")).unwrap();
    std::fs::create_dir_all(root.join("dist/client")).unwrap();
    std::fs::write(
        root.join("manifest.json"),
        serde_json::json!({
            "name": "axiom-core",
            "server_scripts": ["dist/server/index.js"],
            "client_scripts": ["dist/client/index.js"],
            "files": ["dist/client/index.js"],
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(root.join("dist/server/index.js"), script).unwrap();
    std::fs::write(
        root.join("dist/client/index.js"),
        "console.log('client boot')",
    )
    .unwrap();
}

async fn app(dir: &Path, script: &str) -> axum::Router {
    app_with_mode(dir, script, OneSyncMode::Off).await
}

async fn app_with_mode(dir: &Path, script: &str, onesync: OneSyncMode) -> axum::Router {
    write_axiom_core(dir, script);
    // Tests run with dev.auth_bypass (no launcher ticket available in CI).
    let mut config: BastonConfig =
        toml::from_str("[server]\nport = 30120\n[dev]\nauth_bypass = true\n").unwrap();
    config.resources.path = dir.to_owned();
    config.connection.deferral_timeout_secs = 2;
    config.state_sync.onesync = onesync;

    let deferrals = Arc::new(DeferralRegistry::new());
    let players = Arc::new(PlayerRegistry::new());
    let script_host = ScriptHost::spawn(deferrals, Arc::clone(&players)).unwrap();
    let resource_manager = ResourceManager::new(script_host.clone(), dir.to_owned());
    resource_manager.discover().await.unwrap();
    resource_manager.start_all().await.unwrap();

    let auth = baston_gateway::AuthService::new(&config.auth).unwrap();
    router(Arc::new(AppState {
        downloads: baston_gateway::http::DownloadPolicy::new(&config.resources),
        builtins: baston_gateway::http::BuiltinResources::from_config(&config),
        config,
        resource_manager,
        players,
        script_host,
        auth,
        packfiles: baston_gateway::http::PackfileCache::new(),
        streams: baston_gateway::http::StreamCache::new(),
        mesh: None,
    }))
}

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn info_json_returns_server_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path(), AXIOM_CORE_JS).await;
    let response = app
        .oneshot(Request::get("/info.json").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert_eq!(json["name"], "BASTON Dev");
    assert_eq!(json["onesync"]["enabled"], false);
    // BASTON has no authenticated CFX identity, so it must not publish a
    // token it never obtained — a client reading one here would believe the
    // server is licensed (ADR-003).
    assert!(json["vars"].get("sv_licenseKeyToken").is_none());
    assert_eq!(json["vars"]["sv_maxClients"], "32");
    assert_eq!(json["resources"][0], "axiom-core");
}

#[tokio::test]
async fn files_route_serves_resource_scripts_and_404s_missing() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path(), AXIOM_CORE_JS).await;

    let ok = app
        .clone()
        .oneshot(
            Request::get("/files/axiom-core/dist/client/index.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);
    assert_eq!(ok.headers()["content-type"], "application/javascript");

    let private = app
        .clone()
        .oneshot(
            Request::get("/files/axiom-core/dist/server/index.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(private.status(), StatusCode::NOT_FOUND);

    let missing = app
        .clone()
        .oneshot(
            Request::get("/files/nope/file.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);

    let traversal = app
        .oneshot(
            Request::get("/files/axiom-core/..%2F..%2Fbaston.toml")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(traversal.status(), StatusCode::OK);
}

#[tokio::test]
async fn client_connect_runs_player_connecting_and_accepts() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path(), AXIOM_CORE_JS).await;
    let response = app
        .oneshot(
            Request::post("/client")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(
                    "method=initConnect&name=LucasTest&protocol=12&gameName=gta5&guid=1",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert_eq!(json["status"], "ok");
    // Connection token is now a per-connection UUID.
    assert!(json["token"].as_str().is_some_and(|t| t.len() == 36));
}

#[tokio::test]
async fn client_connect_rejected_with_reason() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(
        dir.path(),
        r#"
        AddEventHandler('playerConnecting', function (name, setKickReason, deferrals) {
          deferrals.defer();
          deferrals.done('Not whitelisted');
        });
        "#,
    )
    .await;
    let response = app
        .oneshot(
            Request::post("/client")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(
                    "method=initConnect&name=Intruder&protocol=12&gameName=gta5&guid=2",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let json = body_json(response).await;
    assert_eq!(json["error"], "Not whitelisted");
}

#[tokio::test]
async fn init_connect_response_has_fxserver_fields() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path(), AXIOM_CORE_JS).await;
    let response = app
        .oneshot(
            Request::post("/client")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(
                    "method=initConnect&name=Proto&protocol=12&gameName=gta5&guid=9",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let json = body_json(response).await;
    assert_eq!(json["protocol"], 5);
    assert!(json["sH"].is_boolean(), "client requires non-null sH");
    assert_eq!(json["netlibVersion"], 2);
    assert_eq!(json["maxClients"], 32);
    assert!(json["handover"].is_object());
}

#[tokio::test]
async fn onesync_mode_is_consistent_in_info_and_init_connect() {
    let dir = tempfile::tempdir().unwrap();
    let app = app_with_mode(dir.path(), AXIOM_CORE_JS, OneSyncMode::On).await;

    let info = app
        .clone()
        .oneshot(Request::get("/info.json").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let info = body_json(info).await;
    assert_eq!(info["onesync"]["enabled"], true);
    assert_eq!(info["enhancedHostSupport"], false);
    assert_eq!(info["vars"]["onesync"], "on");
    assert_eq!(info["vars"]["onesync_enabled"], "true");

    let connect = app
        .oneshot(
            Request::post("/client")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(
                    "method=initConnect&name=OneSync&protocol=12&gameName=gta5&guid=10",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let connect = body_json(connect).await;
    assert_eq!(connect["onesync"], true);
    assert_eq!(connect["onesync_big"], true);
    assert_eq!(connect["onesync_population"], true);
    assert_eq!(connect["enhancedHostSupport"], false);
}

#[tokio::test]
async fn old_protocol_client_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path(), AXIOM_CORE_JS).await;
    let response = app
        .oneshot(
            Request::post("/client")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from("method=initConnect&name=Old&protocol=5&guid=9"))
                .unwrap(),
        )
        .await
        .unwrap();
    let json = body_json(response).await;
    assert!(json["error"].as_str().unwrap().contains("version mismatch"));
}

#[tokio::test]
async fn get_configuration_lists_resource_packfile_hash() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path(), AXIOM_CORE_JS).await;
    let response = app
        .oneshot(
            Request::post("/client")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("host", "localhost:30120")
                .body(Body::from("method=getConfiguration"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    // Literal URL (no %s): keeps the FiveM client on plain HTTP — any %s
    // template is force-rewritten to https by the client.
    assert_eq!(json["fileServer"], "http://localhost:30120/files");
    assert_eq!(json["resources"][0]["name"], "axiom-core");
    let hash = json["resources"][0]["files"]["resource.rpf"]
        .as_str()
        .unwrap();
    assert_eq!(hash.len(), 40, "sha1 hex of the packfile");
}

#[tokio::test]
async fn resource_rpf_downloads_and_hash_tracks_content() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path(), AXIOM_CORE_JS).await;

    let rpf = app
        .clone()
        .oneshot(
            Request::get("/files/axiom-core/resource.rpf")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(rpf.status(), StatusCode::OK);
    let bytes = rpf.into_body().collect().await.unwrap().to_bytes();
    // RPF2 magic
    assert_eq!(&bytes[..4], &0x3246_5052u32.to_le_bytes());

    // Hash changes when a client file changes (hot-reload invalidation).
    let config1 = app
        .clone()
        .oneshot(
            Request::post("/client")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from("method=getConfiguration"))
                .unwrap(),
        )
        .await
        .unwrap();
    let hash1 = body_json(config1).await["resources"][0]["files"]["resource.rpf"]
        .as_str()
        .unwrap()
        .to_owned();

    std::fs::write(
        dir.path().join("axiom-core/dist/client/index.js"),
        "console.log('client boot v2')",
    )
    .unwrap();

    let config2 = app
        .oneshot(
            Request::post("/client")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from("method=getConfiguration"))
                .unwrap(),
        )
        .await
        .unwrap();
    let hash2 = body_json(config2).await["resources"][0]["files"]["resource.rpf"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(hash1, hash2);
}

#[tokio::test]
async fn client_connect_times_out_when_deferral_never_resolves() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(
        dir.path(),
        r#"
        AddEventHandler('playerConnecting', function (name, setKickReason, deferrals) {
          deferrals.defer(); // never calls done()
        });
        "#,
    )
    .await;
    let response = app
        .oneshot(
            Request::post("/client")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(
                    "method=initConnect&name=Ghost&protocol=12&gameName=gta5&guid=3",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
}
