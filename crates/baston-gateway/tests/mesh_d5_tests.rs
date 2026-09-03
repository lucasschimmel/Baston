//! Jalon D5 — cross-zone script events: locally-triggered events are
//! published via the host hook; remote events are dispatched locally without
//! re-publication (loop prevention), and handler reactions ARE published.
// Driven by JavaScript resources (deferral handlers, exports, dist/ layouts),
// so they run in the bundles that contain V8. The Lua runtime has its own
// tests in baston-scripting; see docs/guides/modules.md for what it covers.
#![cfg(feature = "scripting-js")]

use std::sync::Arc;

use baston_protocol::PlayerDirectory;
use baston_scripting::{DeferralRegistry, ScriptHost, ScriptSource};
use tokio::sync::mpsc;

fn host() -> ScriptHost {
    ScriptHost::spawn(
        Arc::new(DeferralRegistry::new()),
        Arc::new(PlayerDirectory::new()),
    )
    .expect("script host")
}

#[tokio::test(flavor = "multi_thread")]
async fn cross_zone_publish_and_loop_prevention() {
    let host = host();
    let (tx, mut rx) = mpsc::unbounded_channel::<(String, String)>();
    host.set_cross_zone_publisher(Arc::new(move |event, args| {
        let _ = tx.send((event.to_owned(), args.to_owned()));
    }));

    // Resource replying to axiom:ping with axiom:pong.
    host.load_resource(
        "test-res",
        vec![ScriptSource {
            path: "server.js".into(),
            code: r#"
                AddEventHandler('axiom:ping', (v) => {
                    TriggerEvent('axiom:pong', v + 1);
                });
            "#
            .into(),
        }],
    )
    .await
    .unwrap();

    // Locally-triggered events are mirrored cross-zone.
    host.trigger_event(
        "axiom:economy:transaction",
        &[serde_json::json!({"amount": 500})],
    )
    .await
    .unwrap();
    let (event, args) = rx.recv().await.unwrap();
    assert_eq!(event, "axiom:economy:transaction");
    assert!(args.contains("500"));
    // ... the handler chain also ran locally: nothing else pending from it.

    // Lifecycle events stay local.
    host.trigger_event("playerDropped", &[serde_json::json!(1)])
        .await
        .unwrap();

    // A REMOTE event is dispatched locally but NOT re-published; the local
    // handler's reaction (axiom:pong) IS published (it's local origin).
    host.trigger_remote_event("axiom:ping", "[41]".into())
        .await
        .unwrap();
    let (event, args) = rx.recv().await.unwrap();
    assert_eq!(
        event, "axiom:pong",
        "only the handler's reaction is published"
    );
    assert!(args.contains("42"));
    assert!(
        rx.try_recv().is_err(),
        "axiom:ping itself must not be re-published"
    );
}
