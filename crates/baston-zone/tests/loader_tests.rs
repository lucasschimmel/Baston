//! Milestone A3 exit-criterion tests: discovery, lifecycle, dependency order.

use std::sync::Arc;

use baston_scripting::{DeferralRegistry, ScriptHost};
use baston_zone::resource_loader::ResourceManager;
use baston_zone::ResourceState;

fn write_resource(dir: &std::path::Path, name: &str, deps: &[&str], script: &str) {
    let root = dir.join(name);
    std::fs::create_dir_all(root.join("dist/server")).unwrap();
    let manifest = serde_json::json!({
        "name": name,
        "version": "0.0.1",
        "dependencies": deps,
        "server_scripts": ["dist/server/index.js"],
    });
    std::fs::write(root.join("manifest.json"), manifest.to_string()).unwrap();
    std::fs::write(root.join("dist/server/index.js"), script).unwrap();
}

fn manager(dir: &std::path::Path) -> Arc<ResourceManager> {
    let host = ScriptHost::spawn(
        Arc::new(DeferralRegistry::new()),
        Arc::new(baston_protocol::PlayerDirectory::new()),
    )
    .expect("host");
    ResourceManager::new(host, dir.to_owned())
}

#[tokio::test]
async fn discovers_and_starts_resource_with_player_connecting_handler() {
    let dir = tempfile::tempdir().unwrap();
    write_resource(
        dir.path(),
        "axiom-core",
        &[],
        r#"
        AddEventHandler('playerConnecting', function (name, setKickReason, deferrals) {
          console.log('[axiom-core] player connecting: ' + name);
          deferrals.done();
        });
        "#,
    );

    let manager = manager(dir.path());
    let names = manager.discover().await.expect("discover");
    assert_eq!(names, vec!["axiom-core".to_string()]);

    manager.start_all().await.expect("start_all");
    let status = manager.status().await;
    assert_eq!(status, vec![("axiom-core".into(), ResourceState::Started)]);
}

#[tokio::test]
async fn start_stop_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    write_resource(
        dir.path(),
        "lifecycle",
        &[],
        r#"
        AddEventHandler('onResourceStart', (r) => { if (r === GetCurrentResourceName()) console.log('[lifecycle] started!') });
        AddEventHandler('onResourceStop', (r) => { console.log('[lifecycle] stopping ' + r) });
        "#,
    );

    let manager = manager(dir.path());
    manager.discover().await.expect("discover");
    manager.start("lifecycle").await.expect("start");
    manager.stop("lifecycle").await.expect("stop");
    assert_eq!(
        manager.status().await,
        vec![("lifecycle".into(), ResourceState::Stopped)]
    );
    manager.ensure("lifecycle").await.expect("ensure");
    assert_eq!(
        manager.status().await,
        vec![("lifecycle".into(), ResourceState::Started)]
    );
}

#[tokio::test]
async fn starts_in_dependency_order() {
    let dir = tempfile::tempdir().unwrap();
    // "second" depends on "first"; ordering is observable through the shared
    // start log both scripts append to via globalThis (per-isolate, so we
    // just assert both start without error — topo unit tests cover ordering).
    write_resource(
        dir.path(),
        "second",
        &["first"],
        "console.log('[second] up')",
    );
    write_resource(dir.path(), "first", &[], "console.log('[first] up')");

    let manager = manager(dir.path());
    manager.discover().await.expect("discover");
    manager.start_all().await.expect("start_all");
    let started: Vec<_> = manager.started_names().await;
    assert_eq!(started, vec!["first".to_string(), "second".to_string()]);
}

#[tokio::test]
async fn invalid_script_marks_resource_error() {
    let dir = tempfile::tempdir().unwrap();
    write_resource(dir.path(), "broken", &[], "this is not javascript {{{");

    let manager = manager(dir.path());
    manager.discover().await.expect("discover");
    assert!(manager.start("broken").await.is_err());
    assert_eq!(
        manager.status().await,
        vec![("broken".into(), ResourceState::Error)]
    );
}
