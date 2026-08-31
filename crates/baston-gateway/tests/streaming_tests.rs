//! streamFiles integration: getConfiguration advertising, basename download,
//! hash validation, stream-only resources, hot-reload invalidation.
// Driven by JavaScript resources (deferral handlers, exports, dist/ layouts),
// so they run in the bundles that contain V8. The Lua runtime has its own
// tests in baston-scripting; see docs/guides/modules.md for what it covers.
#![cfg(feature = "scripting-js")]

use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use baston_config::BastonConfig;
use baston_gateway::http::DownloadPolicy;
use baston_gateway::{router, AppState, PlayerRegistry};
use baston_scripting::{DeferralRegistry, ScriptHost};
use baston_zone::resource_loader::ResourceManager;
use http_body_util::BodyExt;
use sha1::{Digest, Sha1};
use tower::ServiceExt;

/// Minimal RSC7 container: magic, version 2, virtPages 0x11, physPages 0x22.
fn rsc7_bytes() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&0x3743_5352u32.to_le_bytes());
    b.extend_from_slice(&2u32.to_le_bytes());
    b.extend_from_slice(&0x11u32.to_le_bytes());
    b.extend_from_slice(&0x22u32.to_le_bytes());
    b.extend_from_slice(b"model payload");
    b
}

/// axiom-core with client files AND a stream/ folder (nested subdir included).
fn write_streaming_resource(dir: &Path) {
    let root = dir.join("axiom-core");
    std::fs::create_dir_all(root.join("dist/client")).unwrap();
    std::fs::create_dir_all(root.join("stream/vehicles")).unwrap();
    std::fs::write(
        root.join("manifest.json"),
        serde_json::json!({
            "name": "axiom-core",
            "client_scripts": ["dist/client/index.js"],
            "files": ["dist/client/index.js"],
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(root.join("dist/client/index.js"), "console.log('boot')").unwrap();
    std::fs::write(root.join("stream/vehicles/adder2.yft"), rsc7_bytes()).unwrap();
    std::fs::write(root.join("stream/notes.txt"), b"raw non-rsc asset").unwrap();
}

/// A car pack: no scripts, no client files — only streamed assets.
fn write_stream_only_resource(dir: &Path) {
    let root = dir.join("carpack");
    std::fs::create_dir_all(root.join("stream")).unwrap();
    std::fs::write(
        root.join("manifest.json"),
        serde_json::json!({ "name": "carpack" }).to_string(),
    )
    .unwrap();
    std::fs::write(root.join("stream/sultan3.yft"), rsc7_bytes()).unwrap();
}

async fn app_with_downloads(dir: &Path) -> (axum::Router, std::sync::Arc<tokio::sync::Semaphore>) {
    let mut config: BastonConfig =
        toml::from_str("[server]\nport = 30120\n[dev]\nauth_bypass = true\n").unwrap();
    config.resources.path = dir.to_owned();

    let deferrals = Arc::new(DeferralRegistry::new());
    let players = Arc::new(PlayerRegistry::new());
    let script_host = ScriptHost::spawn(deferrals, Arc::clone(&players)).unwrap();
    let resource_manager = ResourceManager::new(script_host.clone(), dir.to_owned());
    resource_manager.discover().await.unwrap();
    resource_manager.start_all().await.unwrap();

    let auth = baston_gateway::AuthService::new(&config.auth).unwrap();
    let downloads = DownloadPolicy::new(&config.resources);
    let semaphore = std::sync::Arc::clone(&downloads.semaphore);
    let app = router(Arc::new(AppState {
        downloads,
        builtins: baston_gateway::http::BuiltinResources::from_config(&config),
        config,
        cfx: None,
        icon: None,
        resource_manager,
        players,
        script_host,
        auth,
        packfiles: baston_gateway::http::PackfileCache::new(),
        streams: baston_gateway::http::StreamCache::new(),
        mesh: None,
    }));
    (app, semaphore)
}

async fn app(dir: &Path) -> axum::Router {
    app_with_downloads(dir).await.0
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

fn resource<'a>(config: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    config["resources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == name)
        .unwrap_or_else(|| panic!("resource {name} not in configuration"))
}

#[tokio::test]
async fn get_configuration_advertises_stream_files() {
    let dir = tempfile::tempdir().unwrap();
    write_streaming_resource(dir.path());
    let app = app(dir.path()).await;

    let config = get_configuration(&app).await;
    let streams = &resource(&config, "axiom-core")["streamFiles"];

    // RSC7 asset from a nested subdir, keyed by basename: full metadata.
    let yft = &streams["adder2.yft"];
    assert_eq!(yft["hash"].as_str().unwrap().len(), 40);
    assert_eq!(yft["size"], rsc7_bytes().len() as u64);
    assert_eq!(yft["rscFlags"], yft["size"]);
    assert_eq!(yft["rscVersion"], 2);
    assert_eq!(yft["rscPagesVirtual"], 0x11);
    assert_eq!(yft["rscPagesPhysical"], 0x22);
    assert_eq!(yft["e"], false);

    // Raw (non-RSC) asset: no page fields, rscVersion 0.
    let txt = &streams["notes.txt"];
    assert_eq!(txt["rscVersion"], 0);
    assert!(txt.get("rscPagesVirtual").is_none());
    assert!(txt.get("e").is_none());
}

#[tokio::test]
async fn stream_file_downloads_by_basename_and_hash_matches() {
    let dir = tempfile::tempdir().unwrap();
    write_streaming_resource(dir.path());
    let app = app(dir.path()).await;

    let config = get_configuration(&app).await;
    let advertised = resource(&config, "axiom-core")["streamFiles"]["adder2.yft"]["hash"]
        .as_str()
        .unwrap()
        .to_owned();

    // The client requests the basename, not stream/vehicles/adder2.yft.
    let response = app
        .clone()
        .oneshot(
            Request::get("/files/axiom-core/adder2.yft")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(hex::encode(Sha1::digest(&bytes)), advertised);

    // Unknown basename still 404s.
    let missing = app
        .oneshot(
            Request::get("/files/axiom-core/nope.yft")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn stream_only_resource_is_sent_with_manifest_only_rpf() {
    let dir = tempfile::tempdir().unwrap();
    write_streaming_resource(dir.path());
    write_stream_only_resource(dir.path());
    let app = app(dir.path()).await;

    let config = get_configuration(&app).await;
    let carpack = resource(&config, "carpack");
    assert_eq!(carpack["files"]["resource.rpf"].as_str().unwrap().len(), 40);
    assert!(carpack["streamFiles"]["sultan3.yft"].is_object());

    // Its RPF (fxmanifest.lua only) downloads fine.
    let rpf = app
        .oneshot(
            Request::get("/files/carpack/resource.rpf")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(rpf.status(), StatusCode::OK);
    let bytes = rpf.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&bytes[..4], &0x3246_5052u32.to_le_bytes(), "RPF2 magic");
}

#[tokio::test]
async fn stream_hash_tracks_content_changes() {
    let dir = tempfile::tempdir().unwrap();
    write_streaming_resource(dir.path());
    let app = app(dir.path()).await;

    let before = get_configuration(&app).await;
    let hash1 = resource(&before, "axiom-core")["streamFiles"]["adder2.yft"]["hash"]
        .as_str()
        .unwrap()
        .to_owned();

    let mut changed = rsc7_bytes();
    changed.extend_from_slice(b" v2");
    std::fs::write(
        dir.path().join("axiom-core/stream/vehicles/adder2.yft"),
        changed,
    )
    .unwrap();

    let after = get_configuration(&app).await;
    let hash2 = resource(&after, "axiom-core")["streamFiles"]["adder2.yft"]["hash"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(hash1, hash2, "fingerprint invalidation on file change");
}

#[tokio::test]
async fn client_file_supports_head_and_single_byte_ranges() {
    let dir = tempfile::tempdir().unwrap();
    write_streaming_resource(dir.path());
    let app = app(dir.path()).await;
    let uri = "/files/axiom-core/dist/client/index.js";
    let complete = b"console.log('boot')";

    let head = app
        .clone()
        .oneshot(Request::head(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(head.status(), StatusCode::OK);
    assert_eq!(head.headers()[header::ACCEPT_RANGES], "bytes");
    let complete_len = complete.len().to_string();
    assert_eq!(
        head.headers()[header::CONTENT_LENGTH].to_str().unwrap(),
        complete_len
    );
    assert!(head
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .is_empty());

    for (range, expected) in [
        ("bytes=0-6", &complete[..7]),
        ("bytes=8-", &complete[8..]),
        ("bytes=-5", &complete[complete.len() - 5..]),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::get(uri)
                    .header(header::RANGE, range)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert!(response.headers().contains_key(header::CONTENT_RANGE));
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            expected
        );
    }

    let invalid = app
        .oneshot(
            Request::get(uri)
                .header(header::RANGE, "bytes=999-")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(invalid.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    let expected_content_range = format!("bytes */{}", complete.len());
    assert_eq!(
        invalid.headers()[header::CONTENT_RANGE].to_str().unwrap(),
        expected_content_range
    );
}

#[tokio::test]
async fn private_and_unlisted_files_are_not_downloadable() {
    let dir = tempfile::tempdir().unwrap();
    write_streaming_resource(dir.path());
    let root = dir.path().join("axiom-core");
    std::fs::create_dir_all(root.join("dist/server")).unwrap();
    std::fs::write(root.join("dist/server/private.js"), "server secret").unwrap();
    std::fs::write(root.join("not-in-manifest.txt"), "private").unwrap();
    let app = app(dir.path()).await;

    for uri in [
        "/files/axiom-core/dist/server/private.js",
        "/files/axiom-core/not-in-manifest.txt",
        "/files/axiom-core/manifest.json",
    ] {
        let response = app
            .clone()
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
    }
}

#[tokio::test]
async fn large_file_is_chunked_and_releases_its_permit() {
    let dir = tempfile::tempdir().unwrap();
    write_streaming_resource(dir.path());
    let root = dir.path().join("axiom-core");
    let large = vec![0x5a; 1024 * 1024 + 17];
    std::fs::write(root.join("dist/client/index.js"), &large).unwrap();
    let (app, semaphore) = app_with_downloads(dir.path()).await;
    let available = semaphore.available_permits();

    let response = app
        .oneshot(
            Request::get("/files/axiom-core/dist/client/index.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(semaphore.available_permits(), available - 1);

    let mut body = response.into_body();
    let mut frames = 0;
    let mut bytes = 0;
    while let Some(frame) = body.frame().await {
        let frame = frame.unwrap();
        if let Ok(data) = frame.into_data() {
            frames += 1;
            bytes += data.len();
        }
    }
    assert_eq!(bytes, large.len());
    assert!(frames > 1, "large files must be emitted in several chunks");
    assert_eq!(semaphore.available_permits(), available);
}

#[cfg(unix)]
#[tokio::test]
async fn manifest_symlink_cannot_escape_resource_root() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    write_streaming_resource(dir.path());
    let root = dir.path().join("axiom-core");
    let outside = dir.path().join("outside.js");
    std::fs::write(&outside, "secret").unwrap();
    std::fs::write(
        root.join("manifest.json"),
        serde_json::json!({
            "name": "axiom-core",
            "client_scripts": ["escape.js"],
            "files": ["escape.js"],
        })
        .to_string(),
    )
    .unwrap();
    symlink(&outside, root.join("escape.js")).unwrap();
    let app = app(dir.path()).await;

    let response = app
        .oneshot(
            Request::get("/files/axiom-core/escape.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[cfg(windows)]
#[tokio::test]
async fn manifest_symlink_cannot_escape_resource_root() {
    use std::os::windows::fs::symlink_file;

    let dir = tempfile::tempdir().unwrap();
    write_streaming_resource(dir.path());
    let root = dir.path().join("axiom-core");
    let outside = dir.path().join("outside.js");
    std::fs::write(&outside, "secret").unwrap();
    std::fs::write(
        root.join("manifest.json"),
        serde_json::json!({
            "name": "axiom-core",
            "client_scripts": ["escape.js"],
            "files": ["escape.js"],
        })
        .to_string(),
    )
    .unwrap();
    // Creating symlinks may require Developer Mode on Windows CI. When the OS
    // denies fixture creation, the canonical-escape test is exercised on Unix
    // CI and the runtime guard remains platform-independent.
    if symlink_file(&outside, root.join("escape.js")).is_err() {
        return;
    }
    let app = app(dir.path()).await;

    let response = app
        .oneshot(
            Request::get("/files/axiom-core/escape.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
