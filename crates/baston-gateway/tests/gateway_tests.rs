//! Milestones A4 + A5 exit-criterion tests, run against the real router with
//! a real script host and a temp resources directory.

use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use baston_config::BastonConfig;
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
    std::fs::write(
        root.join("manifest.json"),
        serde_json::json!({
            "name": "axiom-core",
            "server_scripts": ["dist/server/index.js"],
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(root.join("dist/server/index.js"), script).unwrap();
}

async fn app(dir: &Path, script: &str) -> axum::Router {
    write_axiom_core(dir, script);
    // Tests run with dev.auth_bypass (no launcher ticket available in CI).
    let mut config: BastonConfig =
        toml::from_str("[server]\nport = 30120\n[dev]\nauth_bypass = true\n").unwrap();
    config.resources.path = dir.to_owned();
    config.connection.deferral_timeout_secs = 2;

    let deferrals = Arc::new(DeferralRegistry::new());
    let players = Arc::new(PlayerRegistry::new());
    let script_host = ScriptHost::spawn(deferrals, Arc::clone(&players)).unwrap();
    let resource_manager = ResourceManager::new(script_host.clone(), dir.to_owned());
    resource_manager.discover().await.unwrap();
    resource_manager.start_all().await.unwrap();

    let auth = baston_gateway::AuthService::new(&config.auth).unwrap();
    router(Arc::new(AppState {
        config,
        resource_manager,
        players,
        script_host,
        auth,
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
    assert_eq!(json["onesync"]["enabled"], true);
    assert_eq!(json["resources"][0], "axiom-core");
}

#[tokio::test]
async fn files_route_serves_resource_scripts_and_404s_missing() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path(), AXIOM_CORE_JS).await;

    let ok = app
        .clone()
        .oneshot(
            Request::get("/files/axiom-core/dist/server/index.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);
    assert_eq!(ok.headers()["content-type"], "application/javascript");

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
                .body(Body::from("method=initConnect&name=LucasTest"))
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
                .body(Body::from("method=initConnect&name=Intruder"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let json = body_json(response).await;
    assert_eq!(json["error"], "Not whitelisted");
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
                .body(Body::from("method=initConnect&name=Ghost"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
}
