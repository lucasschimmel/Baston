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

    let report = manager.start_all().await;
    assert!(
        report.failed.is_empty() && report.skipped.is_empty(),
        "{report:?}"
    );
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
    let report = manager.start_all().await;
    assert!(
        report.failed.is_empty() && report.skipped.is_empty(),
        "{report:?}"
    );
    let started: Vec<_> = manager.started_names().await;
    assert_eq!(started, vec!["first".to_string(), "second".to_string()]);
}

#[tokio::test]
async fn plain_resource_unaffected_by_decryptor_pipeline() {
    // Non-regression: with the default PlainDecryptor, a plain resource loads
    // and starts exactly as before D-bis.
    let dir = tempfile::tempdir().unwrap();
    write_resource(
        dir.path(),
        "axiom-core",
        &[],
        "console.log('[axiom-core] up')",
    );

    let manager = manager(dir.path());
    manager.discover().await.expect("discover");
    manager.start("axiom-core").await.expect("start");
    assert_eq!(
        manager.status().await,
        vec![("axiom-core".into(), ResourceState::Started)]
    );
}

#[tokio::test]
async fn a_cfx_encrypted_script_fails_cleanly_rather_than_reaching_the_engine() {
    // A file carrying the CFX "FXAP" magic, loaded with the default
    // PlainDecryptor, must fail cleanly (not a V8 SyntaxError) and mark Error.
    // This is the whole of BASTON's escrow story: it refuses, legibly.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("escrow-test");
    std::fs::create_dir_all(root.join("dist/server")).unwrap();
    let manifest = serde_json::json!({
        "name": "escrow-test",
        "version": "0.0.1",
        "dependencies": [],
        "server_scripts": ["dist/server/index.js"],
    });
    std::fs::write(root.join("manifest.json"), manifest.to_string()).unwrap();
    let mut encrypted = b"FXAP".to_vec();
    encrypted.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
    std::fs::write(root.join("dist/server/index.js"), &encrypted).unwrap();

    let manager = manager(dir.path());
    manager.discover().await.expect("discover");
    let err = manager.start("escrow-test").await.expect_err("must fail");
    assert!(
        matches!(err, baston_zone::ZoneError::Decrypt { .. }),
        "expected Decrypt error, got {err:?}"
    );
    assert_eq!(
        manager.status().await,
        vec![("escrow-test".into(), ResourceState::Error)]
    );
}

/// Test decryptor: strips a leading "FXAP" magic and returns the rest as-is.
struct StubDecryptor;
impl baston_core::script_decryptor::ScriptDecryptor for StubDecryptor {
    fn decrypt(
        &self,
        _resource: &str,
        _file: &str,
        bytes: &[u8],
        _entitlement: Option<&baston_core::script_decryptor::EntitlementContext>,
    ) -> Result<Vec<u8>, baston_core::script_decryptor::DecryptError> {
        if baston_core::script_decryptor::is_cfx_encrypted(bytes) {
            Ok(bytes[4..].to_vec())
        } else {
            Ok(bytes.to_vec())
        }
    }
    fn supports_encrypted(&self) -> bool {
        true
    }
}

#[tokio::test]
async fn installed_decryptor_starts_encrypted_resource() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("escrow-test");
    std::fs::create_dir_all(root.join("dist/server")).unwrap();
    let manifest = serde_json::json!({
        "name": "escrow-test",
        "version": "0.0.1",
        "dependencies": [],
        "server_scripts": ["dist/server/index.js"],
    });
    std::fs::write(root.join("manifest.json"), manifest.to_string()).unwrap();
    // "FXAP" magic + valid JS payload; the stub decryptor unwraps it.
    let mut encrypted = b"FXAP".to_vec();
    encrypted.extend_from_slice(b"console.log('[escrow-test] decrypted up')");
    std::fs::write(root.join("dist/server/index.js"), &encrypted).unwrap();

    let manager = manager(dir.path());
    manager.set_script_decryptor(Arc::new(StubDecryptor));
    manager.discover().await.expect("discover");
    manager.start("escrow-test").await.expect("start decrypted");
    assert_eq!(
        manager.status().await,
        vec![("escrow-test".into(), ResourceState::Started)]
    );
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

/// Found on a real cfx-server-data run: one resource whose script would not
/// even parse took the entire server down at boot, so a single bad resource
/// could deny the whole thing.
#[tokio::test]
async fn a_resource_that_cannot_even_parse_costs_only_itself() {
    let dir = tempfile::tempdir().unwrap();
    write_resource(dir.path(), "good", &[], "console.log('up');");
    write_resource(dir.path(), "broken", &[], "this is ( not javascript");

    let manager = manager(dir.path());
    manager.discover().await.expect("discover");
    let report = manager.start_all().await;

    assert_eq!(report.started, vec!["good".to_string()]);
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert_eq!(report.failed[0].0, "broken");

    let status: std::collections::HashMap<_, _> = manager.status().await.into_iter().collect();
    assert_eq!(status["good"], ResourceState::Started);
    assert_eq!(status["broken"], ResourceState::Error);
}

/// Starting a resource whose dependency never came up would run it against a
/// world that dependency was supposed to build.
#[tokio::test]
async fn what_depended_on_a_failure_is_not_started_either() {
    let dir = tempfile::tempdir().unwrap();
    write_resource(dir.path(), "base", &[], "syntax ( error here");
    write_resource(dir.path(), "on-top", &["base"], "console.log('up');");
    write_resource(dir.path(), "unrelated", &[], "console.log('up');");

    let manager = manager(dir.path());
    manager.discover().await.expect("discover");
    let report = manager.start_all().await;

    assert_eq!(report.started, vec!["unrelated".to_string()]);
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].0, "base");
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.skipped[0].0, "on-top");
    assert!(report.skipped[0].1.contains("base"), "{report:?}");
}

/// A manifest naming a resource nobody installed used to fail the whole
/// startup at the topological sort, before a single script had run.
#[tokio::test]
async fn a_dependency_nobody_installed_costs_only_its_dependents() {
    let dir = tempfile::tempdir().unwrap();
    write_resource(
        dir.path(),
        "orphan",
        &["never-installed"],
        "console.log('up');",
    );
    write_resource(dir.path(), "fine", &[], "console.log('up');");

    let manager = manager(dir.path());
    manager.discover().await.expect("discover");
    let report = manager.start_all().await;

    assert_eq!(report.started, vec!["fine".to_string()]);
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].0, "orphan");
    assert!(report.failed[0].1.contains("never-installed"), "{report:?}");
}

/// Caught on the VPS: money failed, money-fountain was skipped for depending on
/// it — and money-fountain-example-map started anyway, because a *skip* did not
/// carry forward the way a *failure* did. Absent is absent either way.
#[tokio::test]
async fn a_skip_carries_down_the_chain_the_way_a_failure_does() {
    let dir = tempfile::tempdir().unwrap();
    write_resource(dir.path(), "money", &[], "syntax ( error");
    write_resource(dir.path(), "fountain", &["money"], "console.log('up');");
    write_resource(
        dir.path(),
        "fountain-map",
        &["fountain"],
        "console.log('up');",
    );
    write_resource(dir.path(), "unrelated", &[], "console.log('up');");

    let manager = manager(dir.path());
    manager.discover().await.expect("discover");
    let report = manager.start_all().await;

    assert_eq!(report.started, vec!["unrelated".to_string()]);
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].0, "money");

    let mut skipped: Vec<&str> = report.skipped.iter().map(|(n, _)| n.as_str()).collect();
    skipped.sort_unstable();
    assert_eq!(
        skipped,
        vec!["fountain", "fountain-map"],
        "the grandchild must not start either: {report:?}"
    );
}
