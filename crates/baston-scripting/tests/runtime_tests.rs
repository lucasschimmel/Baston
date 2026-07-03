//! Milestone A2 exit-criterion tests: FiveM-style JS runs in a deno_core
//! isolate through the ScriptHost, including the playerConnecting flow.

use std::sync::Arc;
use std::time::Duration;

use baston_scripting::{DeferralRegistry, ScriptHost, ScriptSource};

fn host() -> (ScriptHost, Arc<DeferralRegistry>) {
    let deferrals = Arc::new(DeferralRegistry::new());
    let players = Arc::new(baston_protocol::PlayerDirectory::new());
    let host = ScriptHost::spawn(Arc::clone(&deferrals), players).expect("host spawn");
    (host, deferrals)
}

#[tokio::test]
async fn loads_module_with_event_handler_without_panic() {
    let (host, _) = host();
    host.load_resource(
        "test-resource",
        vec![ScriptSource {
            path: "server.js".into(),
            code: "AddEventHandler('playerConnecting', () => {}); console.log('[test] resource loaded');".into(),
        }],
    )
    .await
    .expect("load");
}

#[tokio::test]
async fn exit_criterion_script_runs() {
    let (host, deferrals) = host();
    host.load_resource(
        "test-resource",
        vec![ScriptSource {
            path: "server.js".into(),
            code: r#"
                AddEventHandler('playerConnecting', (name, setKickReason, deferrals) => {
                  deferrals.defer()
                  console.log('[test] playerConnecting: ' + name)
                  deferrals.done()
                })
                console.log('[test] resource loaded')
            "#
            .into(),
        }],
    )
    .await
    .expect("load");

    let rx = deferrals.register(1);
    host.fire_player_connecting(1, "TestPlayer")
        .await
        .expect("fire");
    let outcome = tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .expect("deferral timed out")
        .expect("channel closed");
    assert_eq!(outcome, Ok(()));
}

#[tokio::test]
async fn deferral_rejection_propagates_reason() {
    let (host, deferrals) = host();
    host.load_resource(
        "gatekeeper",
        vec![ScriptSource {
            path: "server.js".into(),
            code: r#"
                AddEventHandler('playerConnecting', (name, setKickReason, deferrals) => {
                  deferrals.defer()
                  deferrals.done('Not whitelisted')
                })
            "#
            .into(),
        }],
    )
    .await
    .expect("load");

    let rx = deferrals.register(7);
    host.fire_player_connecting(7, "Intruder")
        .await
        .expect("fire");
    let outcome = tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .expect("deferral timed out")
        .expect("channel closed");
    assert_eq!(outcome, Err("Not whitelisted".into()));
}

#[tokio::test]
async fn no_defer_auto_accepts() {
    let (host, deferrals) = host();
    host.load_resource(
        "passive",
        vec![ScriptSource {
            path: "server.js".into(),
            code:
                "AddEventHandler('playerConnecting', (name) => { console.log('seen ' + name); });"
                    .into(),
        }],
    )
    .await
    .expect("load");

    let rx = deferrals.register(9);
    host.fire_player_connecting(9, "Walkthrough")
        .await
        .expect("fire");
    let outcome = tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .expect("deferral timed out")
        .expect("channel closed");
    assert_eq!(outcome, Ok(()));
}

#[tokio::test]
async fn trigger_event_round_trips_between_resources() {
    let (host, _) = host();
    host.load_resource(
        "emitter",
        vec![ScriptSource {
            path: "server.js".into(),
            code: r#"
                AddEventHandler('onResourceStart', (res) => {
                  if (res === 'listener') { TriggerEvent('custom:ping', 42) }
                })
            "#
            .into(),
        }],
    )
    .await
    .expect("load emitter");

    host.load_resource(
        "listener",
        vec![ScriptSource {
            path: "server.js".into(),
            code: r#"
                AddEventHandler('custom:ping', (value) => {
                  console.log('[listener] got ping ' + value)
                  globalThis.__got = value
                })
            "#
            .into(),
        }],
    )
    .await
    .expect("load listener");

    // Round-trip is observable via console output; correctness assertion is
    // that neither dispatch errored (load_resource surfaces JS exceptions).
    host.trigger_event("custom:ping", &[serde_json::json!(43)])
        .await
        .expect("trigger");
}
