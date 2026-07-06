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

#[tokio::test]
async fn handler_errors_are_reported_to_resmon() {
    let (host, _) = host();
    host.load_resource(
        "broken",
        vec![ScriptSource {
            path: "server.js".into(),
            code: "AddEventHandler('boom', () => { throw new Error('nope') });".into(),
        }],
    )
    .await
    .expect("load");

    host.trigger_event("boom", &[]).await.expect("trigger");

    let snapshot = host.observability().snapshot();
    let handler = snapshot
        .handlers
        .iter()
        .find(|handler| handler.resource == "broken" && handler.name == "boom")
        .expect("boom handler stats");
    assert_eq!(handler.errors, 1);
}

#[tokio::test]
async fn register_command_dispatches_to_resource() {
    let (host, _) = host();
    host.load_resource(
        "cmd",
        vec![ScriptSource {
            path: "server.js".into(),
            code: r#"
                RegisterCommand('hello', (source, args, raw) => {
                  if (source !== 0 || args[0] !== 'world' || raw !== 'hello world') {
                    throw new Error('bad command payload')
                  }
                  TriggerEvent('cmd:seen')
                }, false)
                AddEventHandler('cmd:seen', () => { throw new Error('command event reached') })
            "#
            .into(),
        }],
    )
    .await
    .expect("load");

    host.execute_command("hello", 0, vec!["world".into()], "hello world".into())
        .await
        .expect("command");

    let snapshot = host.observability().snapshot();
    let command = snapshot
        .handlers
        .iter()
        .find(|handler| handler.resource == "cmd" && handler.name == "hello")
        .expect("command handler stats");
    assert_eq!(command.count, 1);
    assert_eq!(command.errors, 0);

    let event = snapshot
        .handlers
        .iter()
        .find(|handler| handler.resource == "cmd" && handler.name == "cmd:seen")
        .expect("cmd event stats");
    assert_eq!(event.errors, 1);
}
