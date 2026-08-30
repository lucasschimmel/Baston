//! The `displayinfo` overlay reaches the client the same way any resource
//! does — advertised by `getConfiguration`, downloaded as an RPF — while never
//! existing on disk.
// Driven by JavaScript resources (deferral handlers, exports, dist/ layouts),
// so they run in the bundles that contain V8. The Lua runtime has its own
// tests in baston-scripting; see docs/guides/modules.md for what it covers.
#![cfg(feature = "scripting-js")]

use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use baston_config::{BastonConfig, DebugConfig, DisplayInfoAccess};
use baston_gateway::http::{BuiltinResources, DownloadPolicy, PackfileCache, StreamCache};
use baston_gateway::{router, AppState, PlayerRegistry};
use baston_scripting::{DeferralRegistry, ScriptHost};
use baston_zone::resource_loader::ResourceManager;
use http_body_util::BodyExt;
use sha1::{Digest, Sha1};
use tower::ServiceExt;

const OVERLAY: &str = baston_gateway::http::DISPLAYINFO;

async fn app(dir: &Path, access: DisplayInfoAccess) -> axum::Router {
    let mut config: BastonConfig =
        toml::from_str("[server]\nport = 30120\n[dev]\nauth_bypass = true\n").unwrap();
    config.resources.path = dir.to_owned();
    config.debug = DebugConfig {
        display_info: access,
        allow: vec!["license:admin".to_owned()],
        refresh_hz: 5,
    };

    let deferrals = Arc::new(DeferralRegistry::new());
    let players = Arc::new(PlayerRegistry::new());
    let script_host = ScriptHost::spawn(deferrals, Arc::clone(&players)).unwrap();
    let resource_manager = ResourceManager::new(script_host.clone(), dir.to_owned());
    resource_manager.discover().await.unwrap();
    resource_manager.start_all().await.unwrap();
    let auth = baston_gateway::AuthService::new(&config.auth).unwrap();

    router(Arc::new(AppState {
        downloads: DownloadPolicy::new(&config.resources),
        builtins: BuiltinResources::from_config(&config),
        config,
        license_token: std::sync::RwLock::new(None),
        resource_manager,
        players,
        script_host,
        auth,
        packfiles: PackfileCache::new(),
        streams: StreamCache::new(),
        mesh: None,
    }))
}

async fn get_configuration(app: &axum::Router) -> serde_json::Value {
    let response = app
        .clone()
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
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn entry<'a>(config: &'a serde_json::Value, name: &str) -> Option<&'a serde_json::Value> {
    config["resources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == name)
}

async fn download(app: &axum::Router, path: &str) -> (StatusCode, Vec<u8>) {
    let response = app
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, bytes.to_vec())
}

#[tokio::test]
async fn a_disabled_overlay_is_neither_advertised_nor_downloadable() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path(), DisplayInfoAccess::Off).await;

    assert!(entry(&get_configuration(&app).await, OVERLAY).is_none());
    let (status, _) = download(&app, &format!("/files/{OVERLAY}/resource.rpf")).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a server with the overlay off must not ship its code at all"
    );
}

#[tokio::test]
async fn an_enabled_overlay_is_advertised_and_served_from_the_binary() {
    let dir = tempfile::tempdir().unwrap();
    // Deliberately empty: the overlay must ship on a server running no
    // resources whatsoever, which is exactly when an operator needs it.
    let app = app(dir.path(), DisplayInfoAccess::Allowlist).await;

    let config = get_configuration(&app).await;
    let advertised = entry(&config, OVERLAY).expect("overlay advertised");
    let sha1 = advertised["files"]["resource.rpf"]
        .as_str()
        .expect("packfile hash advertised");
    assert_eq!(sha1.len(), 40);

    let (status, bytes) = download(&app, &format!("/files/{OVERLAY}/resource.rpf")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&bytes[..4], b"RPF2");
    assert_eq!(
        hex::encode(Sha1::digest(&bytes)),
        sha1,
        "the advertised hash must match the bytes actually served, or the client rejects them"
    );
    // The renderer is inside, not a stub.
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("baston:displayInfo"),
        "the client script is packed"
    );
    assert!(
        text.contains("client.js"),
        "the generated fxmanifest names it"
    );
}

#[tokio::test]
async fn only_the_packfile_is_reachable_under_a_builtin() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path(), DisplayInfoAccess::Everyone).await;

    // The client mounts the RPF; nothing else is exposed, and in particular no
    // path under the builtin may fall through to the resources directory.
    for path in ["client.js", "fxmanifest.lua", "../../baston.toml"] {
        let (status, _) = download(&app, &format!("/files/{OVERLAY}/{path}")).await;
        assert!(
            status == StatusCode::NOT_FOUND || status == StatusCode::BAD_REQUEST,
            "unexpected {status} for {path}"
        );
    }
}

#[tokio::test]
async fn a_resource_on_disk_cannot_impersonate_a_builtin() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join(OVERLAY);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("manifest.json"),
        serde_json::json!({
            "name": OVERLAY,
            "client_scripts": ["client.js"],
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(root.join("client.js"), "console.log('impostor')").unwrap();

    let app = app(dir.path(), DisplayInfoAccess::Everyone).await;
    let config = get_configuration(&app).await;
    let matching = config["resources"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["name"] == OVERLAY)
        .count();
    assert_eq!(
        matching, 1,
        "the client must never be told to mount one name twice"
    );

    // The builtin still wins the download: server-owned code is what runs.
    let (status, bytes) = download(&app, &format!("/files/{OVERLAY}/resource.rpf")).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        !text.contains("impostor"),
        "a resources directory must not be able to replace server-owned client code"
    );
}
