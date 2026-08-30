//! Milestone A2 exit-criterion tests: FiveM-style JS runs in a deno_core
//! isolate through the ScriptHost, including the playerConnecting flow.
// These load JavaScript resources, so they only exist in a bundle that
// contains the JS runtime. The Lua path has its own tests in src/lua.rs.
#![cfg(feature = "js")]


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

#[derive(Default)]
struct FakeVoice {
    channels: std::sync::Mutex<std::collections::HashSet<u32>>,
    muted: std::sync::Mutex<std::collections::HashSet<u32>>,
    overrides: std::sync::Mutex<std::collections::HashMap<u32, [f32; 3]>>,
}

impl baston_scripting::VoiceControl for FakeVoice {
    fn create_channel(&self, id: u32) {
        self.channels.lock().unwrap().insert(id);
    }
    fn channel_exists(&self, id: u32) -> bool {
        self.channels.lock().unwrap().contains(&id)
    }
    fn set_player_muted(&self, netid: u32, muted: bool) {
        let mut m = self.muted.lock().unwrap();
        if muted {
            m.insert(netid);
        } else {
            m.remove(&netid);
        }
    }
    fn is_player_muted(&self, netid: u32) -> bool {
        self.muted.lock().unwrap().contains(&netid)
    }
    fn set_proximity_override(&self, netid: u32, position: Option<[f32; 3]>) {
        let mut o = self.overrides.lock().unwrap();
        match position {
            Some(p) => {
                o.insert(netid, p);
            }
            None => {
                o.remove(&netid);
            }
        }
    }
    fn proximity_override(&self, netid: u32) -> [f32; 3] {
        self.overrides
            .lock()
            .unwrap()
            .get(&netid)
            .copied()
            .unwrap_or([0.0; 3])
    }
}

#[tokio::test]
async fn mumble_natives_drive_the_voice_control_surface() {
    let (host, _) = host();
    let voice = Arc::new(FakeVoice::default());
    host.set_voice_control(voice.clone());

    host.load_resource(
        "voice-test",
        vec![ScriptSource {
            path: "server.js".into(),
            code: r#"
                MumbleCreateChannel(64);
                MumbleSetPlayerMuted(7, true);
                if (MumbleIsPlayerMuted(7) !== true) throw new Error('mute readback');
                MumbleSetPlayerMuted(7, false);
                if (MumbleIsPlayerMuted(7) !== false) throw new Error('unmute readback');
                const v = NetworkGetVoiceProximityOverrideForPlayer(7);
                if (v[0] !== 0 || v[1] !== 0 || v[2] !== 0) throw new Error('default override');
            "#
            .into(),
        }],
    )
    .await
    .expect("load");

    assert!(voice.channel_exists_public(64));
}

impl FakeVoice {
    fn channel_exists_public(&self, id: u32) -> bool {
        self.channels.lock().unwrap().contains(&id)
    }
}

/// Audit ROB-2 regression: a handler stalled on an unanswered client-native
/// await (1 s timeout) must not block the resource's other events. Before the
/// concurrent-dispatch host loop, the second event's side effect would only be
/// observable after the full native timeout.
#[tokio::test]
async fn stalled_client_native_does_not_block_other_events() {
    let deferrals = Arc::new(DeferralRegistry::new());
    let players = Arc::new(baston_protocol::PlayerDirectory::new());
    let (net, mut net_rx) = baston_scripting::NetBridge::new();
    let host = ScriptHost::spawn_with_net(Arc::clone(&deferrals), Arc::clone(&players), net)
        .expect("host spawn");

    host.load_resource(
        "stall-test",
        vec![ScriptSource {
            path: "server.js".into(),
            code: r#"
                AddEventHandler('slow', async () => {
                  // TASK_PLAY_ANIM-style native; nobody ever answers.
                  await InvokeNativeOnClient(1, "0x43A66C31C68491C0", [-1], true);
                });
                AddEventHandler('fast', () => {
                  TriggerClientEvent('pong', 1);
                });
            "#
            .into(),
        }],
    )
    .await
    .expect("load");

    // Fire the stalling event; its reply only settles at the 1 s native
    // timeout, so run it in the background.
    let slow_host = host.clone();
    let slow = tokio::spawn(async move { slow_host.trigger_event("slow", &[]).await });

    // Drain the outbound native-invoke event so only 'pong' remains, then
    // fire the second event and expect its side effect well under the 1 s
    // native timeout.
    let first = tokio::time::timeout(Duration::from_secs(5), net_rx.recv())
        .await
        .expect("native invoke sent")
        .expect("net bridge open");
    let baston_scripting::NetOutbound::ClientEvent { event, .. } = first else {
        panic!("a native dispatch emits a JSON-args client event, not a raw one")
    };
    assert_eq!(event, "__baston:invokeNative");

    let started = std::time::Instant::now();
    host.trigger_event("fast", &[]).await.expect("fast event");
    let pong = tokio::time::timeout(Duration::from_millis(500), net_rx.recv())
        .await
        .expect("'fast' must not wait for the stalled native")
        .expect("net bridge open");
    let baston_scripting::NetOutbound::ClientEvent { event, .. } = pong else {
        panic!("TriggerClientEvent emits a JSON-args client event, not a raw one")
    };
    assert_eq!(event, "pong");
    assert!(
        started.elapsed() < Duration::from_millis(900),
        "second event stalled behind the native await: {:?}",
        started.elapsed()
    );

    let _ = slow.await.expect("slow join");
}

// --- context-routed (RPC) natives ---

/// Build a host whose outbound traffic the test can inspect.
fn host_with_net() -> (
    ScriptHost,
    tokio::sync::mpsc::Receiver<baston_scripting::NetOutbound>,
) {
    let deferrals = Arc::new(DeferralRegistry::new());
    let players = Arc::new(baston_protocol::PlayerDirectory::new());
    let (net, net_rx) = baston_scripting::NetBridge::new();
    let host = ScriptHost::spawn_with_net(deferrals, players, net).expect("host spawn");
    (host, net_rx)
}

/// One entity the mirror reports as simulated by `owner`.
fn owned_entity(network_id: u32, owner: u32) -> baston_scripting::EntitySummary {
    baston_scripting::EntitySummary {
        network_id,
        owner,
        entity_type: baston_scripting::ScriptEntityType::Ped,
        net_type: 6, // NetObjEntityType::Ped
        first_owner: owner,
        position: [0.0; 3],
        velocity: [0.0; 3],
        routing_bucket: 0,
        health: None,
        max_health: None,
        armour: None,
        model: None,
        heading: None,
        desired_heading: None,
        sync: Default::default(),
    }
}

/// Decompose an `__baston:invokeNative` message into `(target, hash, args)`.
fn invoke_native_call(outbound: baston_scripting::NetOutbound) -> (u32, String, serde_json::Value) {
    let baston_scripting::NetOutbound::ClientEvent {
        source,
        event,
        args_json,
    } = outbound
    else {
        panic!("a native dispatch emits a JSON-args client event, not a raw one")
    };
    assert_eq!(event, "__baston:invokeNative");
    let payload: serde_json::Value = serde_json::from_str(&args_json).expect("payload is JSON");
    let call = payload[0].clone();
    let hash = call["hash"].as_str().expect("hash").to_owned();
    (source, hash, call["args"].clone())
}

/// A context-routed native whose entity has no owner must stay on the server:
/// with no sync state published, nobody can execute it, and a server in that
/// state must not spray undeliverable calls at clients.
#[tokio::test]
async fn entity_ctx_native_without_a_known_owner_is_not_dispatched() {
    let (host, mut net_rx) = host_with_net();
    host.load_resource(
        "rpc-test",
        vec![ScriptSource {
            path: "server.js".into(),
            // SET_PED_ARMOUR is a pure client native: ctx = owner of args[0].
            code: "AddEventHandler('armour', () => { SetPedArmour(4242, 50) })".into(),
        }],
    )
    .await
    .expect("load");

    host.trigger_event("armour", &[]).await.expect("event");
    assert!(
        tokio::time::timeout(Duration::from_millis(200), net_rx.recv())
            .await
            .is_err(),
        "an unowned entity must not produce a client dispatch"
    );

    // Same call, same handle — but now a client owns it.
    host.entity_world().publish([owned_entity(4242, 9)]);
    host.trigger_event("armour", &[]).await.expect("event");

    let outbound = tokio::time::timeout(Duration::from_secs(5), net_rx.recv())
        .await
        .expect("the owning client must receive the native")
        .expect("net bridge open");
    let (target, hash, args) = invoke_native_call(outbound);
    assert_eq!(target, 9, "routed to the owner, not to the caller");
    assert_eq!(hash, "0xCEA04D83135264CC");
    // A script handle IS the network id, so the entity argument travels
    // verbatim — there is no translation table on either side.
    assert_eq!(args, serde_json::json!([4242, 50]));
}

/// `ctx.type == "Player"` natives address the player argument directly: the
/// net id in `args[ctx.idx]` is already the target, no owner lookup involved.
#[tokio::test]
async fn player_ctx_native_is_dispatched_to_that_player() {
    let (host, mut net_rx) = host_with_net();
    host.load_resource(
        "rpc-test",
        vec![ScriptSource {
            path: "server.js".into(),
            code: "AddEventHandler('wanted', () => { SetPlayerWantedLevel(7, 3, false) })".into(),
        }],
    )
    .await
    .expect("load");

    host.trigger_event("wanted", &[]).await.expect("event");

    let outbound = tokio::time::timeout(Duration::from_secs(5), net_rx.recv())
        .await
        .expect("player natives dispatch without any entity state")
        .expect("net bridge open");
    let (target, hash, args) = invoke_native_call(outbound);
    assert_eq!(target, 7);
    assert_eq!(hash, "0x39FF19C64EF7DA5B");
    assert_eq!(args, serde_json::json!([7, 3, false]));
}

/// `NETWORK_GET_ENTITY_OWNER` is how a script predicts where a context native
/// will land, so the JS global must read the real mirror instead of the
/// pre-mirror stub that always answered 0.
#[tokio::test]
async fn network_get_entity_owner_reads_the_entity_mirror() {
    let (host, mut net_rx) = host_with_net();
    host.entity_world().publish([owned_entity(77, 3)]);
    host.load_resource(
        "owner-test",
        vec![ScriptSource {
            path: "server.js".into(),
            code: r#"
                AddEventHandler('check', () => {
                  if (NetworkGetEntityOwner(77) !== 3) throw new Error('owner not read')
                  if (NetworkGetEntityOwner(78) !== 0) throw new Error('unknown handle must be 0')
                  SetPlayerWantedLevel(NetworkGetEntityOwner(77), 1, false)
                })
            "#
            .into(),
        }],
    )
    .await
    .expect("load");

    host.trigger_event("check", &[]).await.expect("event");

    let outbound = tokio::time::timeout(Duration::from_secs(5), net_rx.recv())
        .await
        .expect("the handler must have run to completion")
        .expect("net bridge open");
    let (target, _, _) = invoke_native_call(outbound);
    assert_eq!(target, 3);
}

/// `SET_ENTITY_COORDS` has real server-side behaviour (the synthetic store) and
/// is a context native. Both must happen: the store keeps answering for
/// script-created handles, and a networked entity's owner is told to move it.
#[tokio::test]
async fn set_entity_coords_updates_server_state_and_propagates_to_the_owner() {
    let (host, mut net_rx) = host_with_net();
    host.entity_world().publish([owned_entity(4243, 5)]);
    host.load_resource(
        "coords-test",
        vec![ScriptSource {
            path: "server.js".into(),
            code: r#"
                AddEventHandler('move', () => {
                  // A script-created handle: server-side store only, no owner.
                  const obj = CreateObject(1, 1.0, 2.0, 3.0, false, false, false)
                  SetEntityCoords(obj, 9.0, 8.0, 7.0, false, false, false, false)
                  const [x] = GetEntityCoords(obj)
                  if (x !== 9.0) throw new Error('server state was not updated')
                  // A networked handle: must reach its owner.
                  SetEntityCoords(4243, 1.0, 2.0, 3.0, false, false, false, false)
                })
            "#
            .into(),
        }],
    )
    .await
    .expect("load");

    host.trigger_event("move", &[]).await.expect("event");

    let outbound = tokio::time::timeout(Duration::from_secs(5), net_rx.recv())
        .await
        .expect("the owner must be told to move the entity")
        .expect("net bridge open");
    let (target, hash, args) = invoke_native_call(outbound);
    assert_eq!(target, 5, "only the networked handle has an owner");
    assert_eq!(hash, "0x06843DA7060A026B");
    assert_eq!(args[0], serde_json::json!(4243));
}

// --- vehicle and ped state natives ---

/// A vehicle carrying every node the readers depend on.
fn synced_vehicle(network_id: u32, owner: u32) -> baston_scripting::EntitySummary {
    use baston_protocol::rage::sync_parse::{VehicleAppearance, VehicleGameState, VehicleHealth};

    let mut appearance = VehicleAppearance {
        primary_colour: 12,
        secondary_colour: 34,
        window_tint_index: 255,
        plate: *b"BASTON01",
        has_neon_lights: true,
        neon_colour: [255, 0, 128],
        neon_sides: [true, false, true, false],
        // Extras are inverted on the wire: a set bit turns the extra off. Bit 2
        // is extra 1, so extra 1 reads as off and extra 2 as on.
        extras: 0b100,
        ..VehicleAppearance::default()
    };
    appearance.livery_index = 5;

    baston_scripting::EntitySummary {
        network_id,
        owner,
        entity_type: baston_scripting::ScriptEntityType::Vehicle,
        net_type: 0, // NetObjEntityType::Automobile
        first_owner: owner,
        position: [0.0; 3],
        velocity: [3.0, 4.0, 0.0],
        routing_bucket: 0,
        health: None,
        max_health: None,
        armour: None,
        model: None,
        heading: Some(90.0),
        desired_heading: None,
        sync: baston_protocol::rage::sync_parse::EntityNodeState {
            vehicle_game_state: Some(VehicleGameState {
                radio_station: 21,
                engine_on: true,
                siren_on: true,
                lock_status: 2,
                doors_open: 0b000_0001,
                door_positions: [4, 0, 0, 0, 0, 0, 0],
                lights_on: true,
                has_lock: true,
                locked_players: -1,
                ..VehicleGameState::default()
            }),
            vehicle_health: Some(VehicleHealth {
                engine_health: 650,
                body_health: 910,
                tyres_fine: false,
                tyre_status: {
                    let mut status = [0; 16];
                    status[0] = 1;
                    status[1] = 2;
                    status
                },
                ..VehicleHealth::default()
            }),
            vehicle_appearance: Some(appearance),
            ..Default::default()
        },
    }
}

/// A ped sitting in `vehicle`'s driver seat (raw seat 1 = script seat -1).
fn seated_ped(network_id: u32, owner: u32, vehicle: i32) -> baston_scripting::EntitySummary {
    use baston_protocol::rage::sync_parse::PedGameState;

    let mut ped = owned_entity(network_id, owner);
    ped.sync.ped_game_state = Some(PedGameState {
        cur_vehicle: vehicle,
        cur_vehicle_seat: 1,
        cur_weapon: 0x1B06_D7B1,
        is_handcuffed: true,
        ..PedGameState::default()
    });
    ped
}

#[tokio::test]
async fn vehicle_natives_read_the_decoded_sync_tree() {
    let (host, _) = host();
    host.entity_world().publish([synced_vehicle(4243, 5)]);
    host.load_resource(
        "vehicle-test",
        vec![ScriptSource {
            path: "server.js".into(),
            code: r#"
                const v = 4243
                const eq = (label, got, want) => {
                  if (got !== want) {
                    throw new Error(`${label}: got ${got}, wanted ${want}`)
                  }
                }
                eq('engine', GetIsVehicleEngineRunning(v), true)
                eq('siren', IsVehicleSirenOn(v), true)
                eq('lock', GetVehicleDoorLockStatus(v), 2)
                eq('lockedForPlayer', GetVehicleDoorsLockedForPlayer(v), true)
                eq('door0', GetVehicleDoorStatus(v, 0), 4)
                eq('door1', GetVehicleDoorStatus(v, 1), 0)
                eq('radio', GetVehicleRadioStationIndex(v), 21)
                eq('engineHealth', GetVehicleEngineHealth(v), 650)
                eq('bodyHealth', GetVehicleBodyHealth(v), 910)
                eq('tyre0Burst', IsVehicleTyreBurst(v, 0, false), true)
                eq('tyre0Completely', IsVehicleTyreBurst(v, 0, true), false)
                eq('tyre1Completely', IsVehicleTyreBurst(v, 1, true), true)
                eq('tyre2', IsVehicleTyreBurst(v, 2, false), false)
                eq('plate', GetVehicleNumberPlateText(v), 'BASTON01')
                eq('livery', GetVehicleLivery(v), 5)
                eq('tint', GetVehicleWindowTint(v), -1)
                eq('extra1', IsVehicleExtraTurnedOn(v, 1), false)
                eq('extra2', IsVehicleExtraTurnedOn(v, 2), true)
                eq('neonLeft', GetVehicleNeonEnabled(v, 0), true)
                eq('neonRight', GetVehicleNeonEnabled(v, 1), false)

                const colours = GetVehicleColours(v)
                eq('primary', colours[0], 12)
                eq('secondary', colours[1], 34)
                const neon = GetVehicleNeonColour(v)
                eq('neonR', neon[0], 255)
                eq('neonB', neon[2], 128)
                const lights = GetVehicleLightsState(v)
                eq('lightsOn', lights[1], true)
                eq('highbeams', lights[2], false)

                eq('speed', GetEntitySpeed(v), 5)
                eq('rotationZ', GetEntityRotation(v)[2], 90)

                // An entity the mirror does not know answers neutrally rather
                // than inventing a reading.
                eq('unknownEngine', GetIsVehicleEngineRunning(9999), false)
                eq('unknownPlate', GetVehicleNumberPlateText(9999), '')
            "#
            .into(),
        }],
    )
    .await
    .expect("load");
}

#[tokio::test]
async fn ped_occupancy_natives_resolve_both_directions() {
    let (host, _) = host();
    host.entity_world()
        .publish([synced_vehicle(4243, 5), seated_ped(77, 5, 4243)]);
    host.load_resource(
        "occupancy-test",
        vec![ScriptSource {
            path: "server.js".into(),
            code: r#"
                const eq = (label, got, want) => {
                  if (got !== want) {
                    throw new Error(`${label}: got ${got}, wanted ${want}`)
                  }
                }
                eq('vehiclePedIsIn', GetVehiclePedIsIn(77, false), 4243)
                eq('inAnyVehicle', IsPedInAnyVehicle(77), true)
                eq('inThatVehicle', IsPedInVehicle(77, 4243, false), true)
                eq('inOtherVehicle', IsPedInVehicle(77, 1, false), false)
                eq('seat', GetSeatPedIsUsing(77), -1)
                eq('driver', GetPedInVehicleSeat(4243, -1), 77)
                eq('emptySeat', GetPedInVehicleSeat(4243, 0), 0)
                eq('lastDriver', GetLastPedInVehicleSeat(4243, -1), 77)
                eq('weapon', GetSelectedPedWeapon(77), 0x1B06D7B1)
                eq('handcuffed', IsPedHandcuffed(77), true)
                eq('stealth', GetPedStealthMovement(77), false)
            "#
            .into(),
        }],
    )
    .await
    .expect("load");
}

/// Leaving a vehicle must keep the seat's last occupant, or a script cannot
/// tell who just got out.
#[tokio::test]
async fn a_seat_keeps_its_last_occupant_after_the_ped_leaves() {
    let (host, _) = host();
    let world = host.entity_world();
    world.publish([synced_vehicle(4243, 5), seated_ped(77, 5, 4243)]);

    let mut left = seated_ped(77, 5, -1);
    left.sync.ped_game_state = left.sync.ped_game_state.map(|mut state| {
        state.cur_vehicle = -1;
        state.cur_vehicle_seat = -1;
        state
    });
    left.sync.last_vehicle = Some(4243);
    left.sync.last_vehicle_seat = Some(1);
    world.publish([synced_vehicle(4243, 5), left]);

    assert_eq!(world.occupant(4243, -1), None, "the seat is empty now");
    assert_eq!(world.last_occupant(4243, -1), Some(77));
}

/// The death and movement family, which anti-cheat and RP death handlers
/// depend on and which had no server-side answer at all before the ped nodes
/// were decoded.
#[tokio::test]
async fn ped_state_natives_read_the_decoded_nodes() {
    use baston_protocol::rage::sync_parse::{PedMovement, PedTasks};

    let (host, _) = host();
    let mut killer = owned_entity(88, 6);
    killer.health = Some(200.0);

    let mut victim = owned_entity(77, 5);
    victim.health = Some(0.0);
    victim.desired_heading = Some(120.0);
    victim.sync.cause_of_death = Some(0xA284_C825);
    victim.sync.source_of_damage = Some(88);
    victim.sync.relationship_group = Some(0x1234_5678);
    victim.sync.is_visible = Some(true);
    victim.sync.attached_to = Some(0);
    victim.sync.ped_movement = Some(PedMovement {
        is_stealthy: false,
        is_strafing: true,
        is_ragdolling: true,
    });
    victim.sync.ped_tasks = Some(PedTasks {
        script_command: 0xDEAD_BEEF,
        script_task_stage: 1,
        task_types: [151, 531, 47, 531, 531, 531, 531, 531],
    });

    host.entity_world().publish([victim, killer]);
    host.load_resource(
        "ped-state-test",
        vec![ScriptSource {
            path: "server.js".into(),
            code: r#"
                const eq = (label, got, want) => {
                  if (got !== want) {
                    throw new Error(`${label}: got ${got}, wanted ${want}`)
                  }
                }
                eq('causeOfDeath', GetPedCauseOfDeath(77), 0xA284C825)
                eq('sourceOfDamage', GetPedSourceOfDamage(77), 88)
                eq('sourceOfDeath', GetPedSourceOfDeath(77), 88)
                eq('ragdoll', IsPedRagdoll(77), true)
                eq('strafing', IsPedStrafing(77), true)
                eq('relationship', GetPedRelationshipGroupHash(77), 0x12345678)
                eq('desiredHeading', GetPedDesiredHeading(77), 120)
                eq('taskCommand', GetPedScriptTaskCommand(77), 0xDEADBEEF)
                eq('taskStage', GetPedScriptTaskStage(77), 1)
                eq('task0', GetPedSpecificTaskType(77, 0), 151)
                eq('task2', GetPedSpecificTaskType(77, 2), 47)
                eq('taskOutOfRange', GetPedSpecificTaskType(77, 99), 151)
                eq('visible', IsEntityVisible(77), true)
                eq('attachedTo', GetEntityAttachedTo(77), 0)

                // A living ped has no source of death, only a source of damage.
                eq('killerDeath', GetPedSourceOfDeath(88), 0)
            "#
            .into(),
        }],
    )
    .await
    .expect("load");
}

/// `GET_ENTITY_SCRIPT` resolves the joaat hash on the wire back to the
/// resource name a script can actually use.
#[tokio::test]
async fn entity_script_resolves_the_owning_resource_name() {
    let (host, _) = host();
    let mut entity = owned_entity(77, 5);
    entity.sync.script_hash = Some(baston_protocol::udp::hash_rage_string("script-owner"));
    host.entity_world().publish([entity]);

    // The reverse lookup scans loaded resources, so the registry has to know
    // about this one — the ResourceManager does that in production.
    host.resources().upsert_resource(
        "script-owner".into(),
        std::env::temp_dir(),
        baston_protocol::ResourceManifest {
            name: "script-owner".into(),
            version: None,
            dependencies: Vec::new(),
            server_scripts: Vec::new(),
            client_scripts: Vec::new(),
            files: Vec::new(),
        },
        ScriptResourceState::Started,
    );

    host.load_resource(
        "script-owner",
        vec![ScriptSource {
            path: "server.js".into(),
            code: r#"
                if (GetEntityScript(77) !== 'script-owner') {
                  throw new Error(`got ${GetEntityScript(77)}`)
                }
                // A hash nothing loaded produced resolves to nothing.
                if (GetEntityScript(9999) !== '') throw new Error('unknown entity')
            "#
            .into(),
        }],
    )
    .await
    .expect("load");
}

// --- server control natives ---

#[tokio::test]
async fn resource_lifecycle_natives_reach_the_manager() {
    let (host, _) = host();
    let (control, mut commands) = baston_scripting::QueuedResourceControl::new();
    host.set_resource_control(Arc::new(control));

    host.load_resource(
        "admin",
        vec![ScriptSource {
            path: "server.js".into(),
            code: r#"
                if (StartResource('chat') !== true) throw new Error('start refused')
                if (StopResource('chat') !== true) throw new Error('stop refused')
            "#
            .into(),
        }],
    )
    .await
    .expect("load");

    assert_eq!(
        commands.recv().await,
        Some(baston_scripting::ResourceCommand::Start("chat".into()))
    );
    assert_eq!(
        commands.recv().await,
        Some(baston_scripting::ResourceCommand::Stop("chat".into()))
    );
}

/// With no manager wired the natives must report failure rather than look like
/// they worked — a script can then fall back instead of waiting on a resource
/// that will never start.
#[tokio::test]
async fn resource_lifecycle_natives_refuse_without_a_manager() {
    let (host, _) = host();
    host.load_resource(
        "admin",
        vec![ScriptSource {
            path: "server.js".into(),
            code: "if (StartResource('chat') !== false) throw new Error('claimed success')".into(),
        }],
    )
    .await
    .expect("load");
}

#[tokio::test]
async fn server_metadata_natives_write_the_convars_info_json_reads() {
    let (host, _) = host();
    host.load_resource(
        "meta",
        vec![ScriptSource {
            path: "server.js".into(),
            code: r#"
                SetGameType('Roleplay')
                SetMapName('Los Santos')
                if (GetConvar('sv_gametype', '') !== 'Roleplay') throw new Error('game type')
                if (GetConvar('sv_mapname', '') !== 'Los Santos') throw new Error('map name')
            "#
            .into(),
        }],
    )
    .await
    .expect("load");
}

/// bcrypt round trip: the hash a script stores must verify, and a wrong
/// password must not.
#[tokio::test]
async fn password_hashing_round_trips() {
    let (host, _) = host();
    host.load_resource(
        "auth",
        vec![ScriptSource {
            path: "server.js".into(),
            code: r#"
                const hash = GetPasswordHash('correct horse battery staple')
                if (!hash.startsWith('$2')) throw new Error(`not a bcrypt hash: ${hash}`)
                if (VerifyPasswordHash('correct horse battery staple', hash) !== true) {
                  throw new Error('the right password did not verify')
                }
                if (VerifyPasswordHash('wrong', hash) !== false) {
                  throw new Error('the wrong password verified')
                }
                if (VerifyPasswordHash('anything', 'not-a-hash') !== false) {
                  throw new Error('a malformed hash must verify as false')
                }
            "#
            .into(),
        }],
    )
    .await
    .expect("load");
}

/// The internal event native carries msgpack the caller packed itself, so the
/// bytes must reach the transport unchanged.
#[tokio::test]
async fn trigger_client_event_internal_forwards_its_payload_verbatim() {
    let (host, mut net_rx) = host_with_net();
    host.load_resource(
        "raw-events",
        vec![ScriptSource {
            path: "server.js".into(),
            code: r#"
                AddEventHandler('send', () => {
                  // msgpack for [1, 255] — bytes that are not valid UTF-8 text.
                  const payload = String.fromCharCode(0x92, 0x01, 0xCC, 0xFF)
                  TriggerClientEventInternal('custom:ping', 7, payload, payload.length)
                })
            "#
            .into(),
        }],
    )
    .await
    .expect("load");

    host.trigger_event("send", &[]).await.expect("event");

    let outbound = tokio::time::timeout(Duration::from_secs(5), net_rx.recv())
        .await
        .expect("the raw event reached the bridge")
        .expect("net bridge open");
    let baston_scripting::NetOutbound::ClientEventRaw {
        source,
        event,
        payload,
    } = outbound
    else {
        panic!("the internal native must emit a raw event, not a JSON-args one")
    };
    assert_eq!(source, 7);
    assert_eq!(event, "custom:ping");
    assert_eq!(payload, vec![0x92, 0x01, 0xCC, 0xFF]);
}

#[tokio::test]
async fn the_console_buffer_retains_what_resources_printed() {
    let (host, _) = host();
    host.load_resource(
        "chatty",
        vec![ScriptSource {
            path: "server.js".into(),
            code: r#"
                console.log('a distinctive line')
                if (!GetConsoleBuffer().includes('a distinctive line')) {
                  throw new Error('the console buffer did not retain the line')
                }
            "#
            .into(),
        }],
    )
    .await
    .expect("load");
}

// --- HTTP natives ---

/// `SetHttpHandler` end to end: the JS registers, Rust dispatches a request
/// through the internal event, and the handler's `response.send()` resolves the
/// parked waiter with the status, headers and body it wrote.
#[tokio::test]
async fn set_http_handler_answers_a_dispatched_request() {
    let (host, _) = host();
    host.load_resource(
        "panel",
        vec![ScriptSource {
            path: "server.js".into(),
            code: r#"
                SetHttpHandler((req, res) => {
                    req.setDataHandler((body) => {
                        res.writeHead(201, { 'Content-Type': 'application/json' })
                        res.send(JSON.stringify({
                            method: req.method,
                            path: req.path,
                            address: req.address,
                            body,
                        }))
                    })
                })
            "#
            .into(),
        }],
    )
    .await
    .expect("load");

    let registry = host.http_handlers();
    assert!(registry.has_handler("panel"), "the resource registered");

    let (id, reply) = registry.begin();
    let request = serde_json::json!({
        "id": id,
        "method": "POST",
        "path": "/callback",
        "address": "203.0.113.7:52000",
        "headers": { "x-test": "1" },
        "body": "payload",
    });
    host.trigger_event_on("panel", baston_scripting::HTTP_REQUEST_EVENT, &[request])
        .await
        .expect("dispatch");

    let response = tokio::time::timeout(Duration::from_secs(5), reply)
        .await
        .expect("the handler answered in time")
        .expect("the waiter was not dropped");
    assert_eq!(response.status, 201);
    assert_eq!(
        response.headers,
        vec![("Content-Type".to_owned(), "application/json".to_owned())]
    );
    let body: serde_json::Value = serde_json::from_str(&response.body).expect("json body");
    assert_eq!(body["method"], "POST");
    assert_eq!(body["path"], "/callback");
    assert_eq!(body["address"], "203.0.113.7:52000");
    assert_eq!(body["body"], "payload", "setDataHandler received the body");
}

/// A stopped resource must stop answering, or the next request parks a waiter
/// nobody can resolve.
#[tokio::test]
async fn unloading_a_resource_drops_its_http_handler() {
    let (host, _) = host();
    host.load_resource(
        "panel",
        vec![ScriptSource {
            path: "server.js".into(),
            code: "SetHttpHandler(() => {})".into(),
        }],
    )
    .await
    .expect("load");
    assert!(host.http_handlers().has_handler("panel"));

    host.unload_resource("panel").await.expect("unload");
    assert!(!host.http_handlers().has_handler("panel"));
}

/// The full `PerformHttpRequest` round trip minus the socket: the JS queues a
/// request onto the bridge, and the reply event resolves its callback.
#[tokio::test]
async fn perform_http_request_queues_and_resolves_its_callback() {
    let (host, _) = host();
    let (bridge, mut requests) = baston_scripting::HttpBridge::new();
    host.set_http_bridge(bridge);

    host.load_resource(
        "caller",
        vec![ScriptSource {
            path: "server.js".into(),
            code: r#"
                const token = PerformHttpRequest(
                    'https://api.test/v1/thing',
                    (status, body, headers, err) => {
                        SetResourceKvp('reply', `${status}|${body}|${headers['x-kind']}|${err}`)
                    },
                    'POST',
                    { hello: 'world' },
                    { 'Content-Type': 'application/json' }
                )
                SetResourceKvp('token', String(token))
            "#
            .into(),
        }],
    )
    .await
    .expect("load");

    let request = tokio::time::timeout(Duration::from_secs(5), requests.recv())
        .await
        .expect("the request reached the worker queue")
        .expect("bridge open");
    assert_eq!(request.resource, "caller");
    assert_eq!(request.url, "https://api.test/v1/thing");
    assert_eq!(request.method, "POST");
    assert_eq!(request.body, r#"{"hello":"world"}"#);
    assert_eq!(
        request.headers,
        vec![("Content-Type".to_owned(), "application/json".to_owned())]
    );

    assert_eq!(
        host.kvp().get("caller", "token"),
        Some(request.token.to_string()),
        "the script holds the same token the worker sees"
    );

    host.trigger_event_on(
        "caller",
        baston_scripting::HTTP_RESPONSE_EVENT,
        &[
            serde_json::json!(request.token),
            serde_json::json!(200),
            serde_json::json!("pong"),
            serde_json::json!({ "x-kind": "test" }),
            serde_json::Value::Null,
        ],
    )
    .await
    .expect("dispatch the reply");

    assert_eq!(
        host.kvp().get("caller", "reply"),
        Some("200|pong|test|null".to_owned()),
        "the callback ran with the worker's result"
    );
}

/// Without a worker the native must refuse rather than hand back a token that
/// never resolves — a script waiting on that callback would hang forever.
#[tokio::test]
async fn perform_http_request_without_a_worker_refuses() {
    let (host, _) = host();
    host.load_resource(
        "caller",
        vec![ScriptSource {
            path: "server.js".into(),
            code: r#"
                const token = PerformHttpRequest('https://api.test/', () => {})
                SetResourceKvp('token', String(token))
            "#
            .into(),
        }],
    )
    .await
    .expect("load");

    assert_eq!(host.kvp().get("caller", "token"), Some("0".to_owned()));
}
