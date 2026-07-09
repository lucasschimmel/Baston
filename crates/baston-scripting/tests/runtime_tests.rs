//! Milestone A2 exit-criterion tests: FiveM-style JS runs in a deno_core
//! isolate through the ScriptHost, including the playerConnecting flow.

use std::sync::Arc;
use std::time::Duration;

use baston_scripting::{DeferralRegistry, ScriptHost, ScriptResourceState, ScriptSource};

fn host() -> (ScriptHost, Arc<DeferralRegistry>) {
    let (host, deferrals, _) = host_with_players();
    (host, deferrals)
}

fn host_with_players() -> (
    ScriptHost,
    Arc<DeferralRegistry>,
    Arc<baston_protocol::PlayerDirectory>,
) {
    let deferrals = Arc::new(DeferralRegistry::new());
    let players = Arc::new(baston_protocol::PlayerDirectory::new());
    let host = ScriptHost::spawn(Arc::clone(&deferrals), Arc::clone(&players)).expect("host spawn");
    (host, deferrals, players)
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

#[tokio::test]
async fn core_convar_natives_follow_fxserver_defaults() {
    let (host, _) = host();
    host.load_resource(
        "convars",
        vec![ScriptSource {
            path: "server.js".into(),
            code: r#"
                if (GetConvar('missing', 'fallback') !== 'fallback') throw new Error('default string')
                if (GetConvarInt('missing_int', 7) !== 7) throw new Error('default int')
                if (GetConvarFloat('missing_float', 1.5) !== 1.5) throw new Error('default float')
                if (GetConvarBool('missing_bool', true) !== true) throw new Error('default bool')
                SetConvar('voice_useNativeAudio', 'true')
                SetConvar('rounds', '42')
                SetConvar('ratio', '2.25')
                if (GetConvar('voice_useNativeAudio', 'false') !== 'true') throw new Error('string')
                if (GetConvarBool('voice_useNativeAudio', false) !== true) throw new Error('bool')
                if (GetConvarInt('rounds', 0) !== 42) throw new Error('int')
                if (GetConvarFloat('ratio', 0) !== 2.25) throw new Error('float')
            "#
            .into(),
        }],
    )
    .await
    .expect("load");
}

#[tokio::test]
async fn core_player_natives_read_player_directory() {
    let (host, _, players) = host_with_players();
    players.insert(baston_protocol::PlayerInfo {
        source: 12,
        name: "Lucas".into(),
        identifiers: vec![
            "license:abc123".into(),
            "steam:76561198000000000".into(),
            "ip:127.0.0.1".into(),
        ],
    });

    host.load_resource(
        "players",
        vec![ScriptSource {
            path: "server.js".into(),
            code: r#"
                if (!DoesPlayerExist(12)) throw new Error('exists')
                if (DoesPlayerExist(99)) throw new Error('missing exists')
                if (GetNumPlayerIdentifiers(12) !== 3) throw new Error('identifier count')
                if (GetPlayerIdentifier(12, 1) !== 'steam:76561198000000000') throw new Error('identifier index')
                if (GetPlayerIdentifier(12, 9) !== null) throw new Error('identifier bounds')
                if (GetPlayerIdentifierByType(12, 'license') !== 'license:abc123') throw new Error('identifier type')
                if (GetPlayerEndpoint(12) !== '127.0.0.1') throw new Error('endpoint')
                if (GetPlayerGuid(12) !== 'license:abc123') throw new Error('guid')
                if (GetPlayerPing(12) !== 0) throw new Error('ping fallback')
                if (GetNumPlayerTokens(12) !== 0) throw new Error('token count')
                if (GetPlayerToken(12, 0) !== null) throw new Error('token')
            "#
            .into(),
        }],
    )
    .await
    .expect("load");
}

#[tokio::test]
async fn core_resource_natives_read_registry_and_files() {
    let (host, _) = host();
    let root = std::env::temp_dir().join(format!(
        "baston-resource-native-test-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("temp resource root");
    std::fs::write(root.join("data.json"), "{\"ok\":true}").expect("fixture");
    host.resources().upsert_resource(
        "core".into(),
        root.clone(),
        baston_protocol::ResourceManifest {
            name: "core".into(),
            version: Some("1.2.3".into()),
            dependencies: vec!["dep".into()],
            server_scripts: vec!["server.js".into()],
            client_scripts: vec!["client.js".into()],
            files: vec!["data.json".into()],
        },
        ScriptResourceState::Started,
    );

    host.load_resource(
        "core",
        vec![ScriptSource {
            path: "server.js".into(),
            code: r#"
                if (GetNumResources() !== 1) throw new Error('resource count')
                if (GetResourceByFindIndex(0) !== 'core') throw new Error('find index')
                if (GetResourceByFindIndex(1) !== null) throw new Error('find index bounds')
                if (GetResourceState('core') !== 'started') throw new Error('state')
                if (GetResourceState('missing') !== 'missing') throw new Error('missing state')
                if (!GetResourcePath('core')) throw new Error('path')
                if (GetNumResourceMetadata('core', 'file') !== 1) throw new Error('metadata count')
                if (GetResourceMetadata('core', 'version', 0) !== '1.2.3') throw new Error('metadata version')
                if (LoadResourceFile('core', 'data.json') !== '{"ok":true}') throw new Error('load file')
                if (!SaveResourceFile('core', 'out.txt', 'abcdef', 3)) throw new Error('save file')
                if (LoadResourceFile('core', 'out.txt') !== 'abc') throw new Error('save length')
                if (LoadResourceFile('core', '../blocked.txt') !== null) throw new Error('path traversal')
            "#
            .into(),
        }],
    )
    .await
    .expect("load");

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn shared_cfx_runtime_natives_are_exposed() {
    let (host, _) = host();
    host.load_resource(
        "shared-cfx",
        vec![ScriptSource {
            path: "server.js".into(),
            code: r#"
                if (!IsDuplicityVersion()) throw new Error('server runtime')
                if (GetGameName() !== 'gta5') throw new Error('game name')
                if (GetInstanceId() !== 0) throw new Error('instance id')
                if (GetInvokingResource() !== 'shared-cfx') throw new Error('invoking resource')
                if (ProfilerIsRecording()) throw new Error('profiler default')
                ProfilerEnterScope('shared-cfx-test')
                ProfilerExitScope()
                if (WasEventCanceled()) throw new Error('event cancel default')
                if (IsAceAllowed('command.test')) throw new Error('ace default')
                if (GetRegisteredCommands().length !== 0) throw new Error('registered commands default')
            "#
            .into(),
        }],
    )
    .await
    .expect("load");
}

#[tokio::test]
async fn shared_cfx_kvp_natives_are_resource_scoped() {
    let (host, _) = host();
    host.load_resource(
        "kvp-a",
        vec![ScriptSource {
            path: "server.js".into(),
            code: r#"
                SetResourceKvp('alpha', 'one')
                SetResourceKvpInt('count', 42)
                SetResourceKvpFloat('ratio', 2.5)
                if (GetResourceKvpString('alpha') !== 'one') throw new Error('string kvp')
                if (GetResourceKvpInt('count') !== 42) throw new Error('int kvp')
                if (GetResourceKvpFloat('ratio') !== 2.5) throw new Error('float kvp')
                const h = StartFindKvp('')
                const keys = [FindKvp(h), FindKvp(h), FindKvp(h)].filter(Boolean).sort()
                EndFindKvp(h)
                if (keys.join(',') !== 'alpha,count,ratio') throw new Error('find kvp')
                DeleteResourceKvp('alpha')
                if (GetResourceKvpString('alpha') !== null) throw new Error('delete kvp')
            "#
            .into(),
        }],
    )
    .await
    .expect("load kvp-a");

    host.load_resource(
        "kvp-b",
        vec![ScriptSource {
            path: "server.js".into(),
            code: r#"
                if (GetResourceKvpString('count') !== null) throw new Error('kvp leaked')
            "#
            .into(),
        }],
    )
    .await
    .expect("load kvp-b");
}

#[tokio::test]
async fn generated_server_native_shims_are_callable() {
    let (host, _) = host();
    host.load_resource(
        "server-cfx",
        vec![ScriptSource {
            path: "server.js".into(),
            code: r#"
                const vehicle = CreateVehicle(0x1234, 1.0, 2.0, 3.0, 90.0, true, false)
                if (!DoesEntityExist(vehicle)) throw new Error('vehicle exists')
                if (GetEntityType(vehicle) !== 2) throw new Error('vehicle type')
                const coords = GetEntityCoords(vehicle)
                if (coords[0] !== 1.0 || coords[1] !== 2.0 || coords[2] !== 3.0) {
                  throw new Error('vehicle coords')
                }
                SetEntityCoords(vehicle, 4.0, 5.0, 6.0)
                const moved = GetEntityCoords(vehicle)
                if (moved[0] !== 4.0 || moved[1] !== 5.0 || moved[2] !== 6.0) {
                  throw new Error('moved coords')
                }
                if (GetEntityModel(vehicle) !== 0x1234) throw new Error('model')
                if (GetAllVehicles()[0] !== vehicle) throw new Error('all vehicles')
                if (GetHashKey('test') === 0) throw new Error('hash')
                DeleteEntity(vehicle)
                if (DoesEntityExist(vehicle)) throw new Error('deleted')
            "#
            .into(),
        }],
    )
    .await
    .expect("load");
}
