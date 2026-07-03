//! B3 exit-criterion tests: a real ENet client performs the FiveM game
//! handshake against the BASTON UDP server on loopback.

use std::net::UdpSocket;
use std::sync::Arc;
use std::time::{Duration, Instant};

use baston_protocol::udp::{
    handshake, read_message_type, time_sync, MSG_CONNECT, MSG_NET_EVENT, MSG_SERVER_EVENT,
    MSG_TIME_SYNC,
};
use baston_protocol::{events, PlayerDirectory, PlayerInfo};
use baston_scripting::{DeferralRegistry, NetBridge, ScriptHost, ScriptSource};
use rusty_enet as enet;

struct TestClient {
    host: enet::Host<UdpSocket>,
}

impl TestClient {
    fn connect(server_port: u16) -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut host = enet::Host::new(
            socket,
            enet::HostSettings {
                peer_limit: 1,
                channel_limit: 2,
                ..Default::default()
            },
        )
        .unwrap();
        host.connect(
            std::net::SocketAddr::from(([127, 0, 0, 1], server_port)),
            2,
            0,
        )
        .unwrap();
        Self { host }
    }

    /// Pump until an event arrives or the timeout elapses.
    fn wait_event(&mut self, timeout: Duration) -> Option<(u8, Vec<u8>, bool)> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match self.host.service().unwrap() {
                Some(enet::Event::Connect { .. }) => return Some((0, Vec::new(), true)),
                Some(enet::Event::Receive {
                    channel_id, packet, ..
                }) => return Some((channel_id, packet.data().to_vec(), false)),
                Some(enet::Event::Disconnect { .. }) => return None,
                None => std::thread::sleep(Duration::from_millis(2)),
            }
        }
        None
    }

    fn send(&mut self, channel: u8, data: &[u8]) {
        self.host
            .peer_mut(enet::PeerID(0))
            .send(channel, &enet::Packet::reliable(data))
            .unwrap();
    }
}

async fn start_server() -> (u16, Arc<PlayerDirectory>, String) {
    let players = Arc::new(PlayerDirectory::new());
    let script_host = ScriptHost::spawn(Arc::new(DeferralRegistry::new()), Arc::clone(&players))
        .expect("script host");

    // Simulate a player that finished the HTTP phase.
    let source = players.allocate_source();
    players.insert(PlayerInfo {
        source,
        name: "UdpTester".into(),
        identifiers: vec![format!("license:dev-{source}")],
    });
    let token = "test-session-token".to_owned();
    players.bind_token(token.clone(), source);

    // Bind an ephemeral UDP port.
    let port = {
        let probe = UdpSocket::bind("127.0.0.1:0").unwrap();
        probe.local_addr().unwrap().port()
        // probe dropped → port free for the server (racy in theory, fine in tests)
    };
    baston_gateway::udp::spawn(port, 2, 8, Arc::clone(&players), script_host).expect("udp server");
    (port, players, token)
}

fn handshake_packet(token: &str) -> Vec<u8> {
    let mut data = MSG_CONNECT.to_le_bytes().to_vec();
    data.extend_from_slice(format!("token={token}&guid=42").as_bytes());
    data
}

#[tokio::test(flavor = "multi_thread")]
async fn enet_handshake_returns_connect_ok() {
    let (port, _players, token) = start_server().await;

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut client = TestClient::connect(port);
        assert!(
            matches!(
                client.wait_event(Duration::from_secs(5)),
                Some((_, _, true))
            ),
            "ENet connect failed"
        );
        client.send(0, &handshake_packet(&token));
        let (channel, data, _) = client
            .wait_event(Duration::from_secs(5))
            .expect("no connectOK received");
        tx.send((channel, data)).unwrap();
    });

    let (channel, data) = tokio::task::spawn_blocking(move || rx.recv().unwrap())
        .await
        .unwrap();
    assert_eq!(channel, 0);
    let (ty, payload) = read_message_type(&data).unwrap();
    assert_eq!(ty, MSG_CONNECT);
    // " <netId> <hostNetId> <hostBase> <slot> <time>"
    let text = std::str::from_utf8(payload).unwrap();
    assert_eq!(text, " 1 -1 -1 -1 -1");
    // Server-side format matches the builder used by the server.
    assert_eq!(data, handshake::build_connect_ok(1, None));
}

#[tokio::test(flavor = "multi_thread")]
async fn time_sync_echoes_request() {
    let (port, _players, token) = start_server().await;

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut client = TestClient::connect(port);
        client.wait_event(Duration::from_secs(5)).expect("connect");
        client.send(0, &handshake_packet(&token));
        client
            .wait_event(Duration::from_secs(5))
            .expect("connectOK");

        client.send(
            0,
            &time_sync::build_time_sync_request(time_sync::TimeSyncRequest {
                request_time: 111,
                request_sequence: 3,
            }),
        );
        // Skip unrelated post-connect traffic (msgConVars, onPlayerJoining).
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let (channel, data, _) = client
                .wait_event(Duration::from_secs(5))
                .expect("no time sync response");
            if read_message_type(&data).is_some_and(|(ty, _)| ty == MSG_TIME_SYNC) {
                tx.send((channel, data)).unwrap();
                return;
            }
        }
        panic!("time sync response never arrived");
    });

    let (channel, data) = tokio::task::spawn_blocking(move || rx.recv().unwrap())
        .await
        .unwrap();
    assert_eq!(channel, 1, "msgTimeSync is sent on channel 1");
    let (ty, payload) = read_message_type(&data).unwrap();
    assert_eq!(ty, MSG_TIME_SYNC);
    assert_eq!(u32::from_le_bytes(payload[..4].try_into().unwrap()), 111);
    assert_eq!(u32::from_le_bytes(payload[4..8].try_into().unwrap()), 3);
}

/// Parse a `msgNetEvent` packet: (event name, args as JSON).
fn parse_net_event(data: &[u8]) -> Option<(String, serde_json::Value)> {
    let (ty, payload) = read_message_type(data)?;
    if ty != MSG_NET_EVENT {
        return None;
    }
    // u16 sourceNetId | u16 nameLen | name+NUL | msgpack args
    let name_len = u16::from_le_bytes(payload[2..4].try_into().ok()?) as usize;
    let name = String::from_utf8_lossy(&payload[4..4 + name_len - 1]).into_owned();
    let args = events::msgpack_to_json_args(&payload[4 + name_len..]).ok()?;
    Some((name, args))
}

/// Build a `msgServerEvent` packet from JSON args.
fn build_server_event(name: &str, args: &serde_json::Value) -> Vec<u8> {
    let mut out = MSG_SERVER_EVENT.to_le_bytes().to_vec();
    out.extend_from_slice(&((name.len() + 1) as u16).to_le_bytes());
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    out.extend_from_slice(&rmp_serde::to_vec(args).unwrap());
    out
}

/// B4 exit criterion: a server script calls `GetPlayerPed(source)`; the
/// (simulated) client executes the native and returns the ped id.
#[tokio::test(flavor = "multi_thread")]
async fn native_dispatch_roundtrip_through_simulated_client() {
    // Server with a script that answers 'test:requestPed'.
    let players = Arc::new(PlayerDirectory::new());
    let (net_bridge, net_rx) = NetBridge::new();
    let script_host = ScriptHost::spawn_with_net(
        Arc::new(DeferralRegistry::new()),
        Arc::clone(&players),
        net_bridge,
    )
    .unwrap();
    script_host
        .load_resource(
            "axiom-core",
            vec![ScriptSource {
                path: "server.js".into(),
                code: r#"
                AddEventHandler('test:requestPed', async () => {
                  const source = globalThis.source;
                  const ped = await GetPlayerPed(source);
                  console.log('[axiom-core] player ped: ' + ped);
                  TriggerClientEvent('test:pedResult', source, ped);
                });
                "#
                .into(),
            }],
        )
        .await
        .unwrap();

    let source = players.allocate_source();
    players.insert(PlayerInfo {
        source,
        name: "NativeTester".into(),
        identifiers: vec![format!("license:dev-{source}")],
    });
    players.bind_token("native-test-token".into(), source);

    let port = {
        let probe = UdpSocket::bind("127.0.0.1:0").unwrap();
        probe.local_addr().unwrap().port()
    };
    baston_gateway::udp::spawn_with_net(port, 2, 8, players, script_host, Some(net_rx)).unwrap();

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut client = TestClient::connect(port);
        client.wait_event(Duration::from_secs(5)).expect("connect");
        client.send(0, &handshake_packet("native-test-token"));
        client
            .wait_event(Duration::from_secs(5))
            .expect("connectOK");

        // Kick off the server-side handler.
        client.send(
            0,
            &build_server_event("test:requestPed", &serde_json::json!([])),
        );

        // Act as the BASTON client shim + assert the final event.
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let Some((_, data, _)) = client.wait_event(Duration::from_secs(5)) else {
                continue;
            };
            let Some((name, args)) = parse_net_event(&data) else {
                continue;
            };
            match name.as_str() {
                "__baston:invokeNative" => {
                    let call = &args[0];
                    assert_eq!(call["hash"], "0x43A66C31C68491C0", "GET_PLAYER_PED");
                    let id = call["id"].as_u64().unwrap();
                    // "Execute" the native locally: ped id 1234.
                    client.send(
                        0,
                        &build_server_event(
                            "__baston:nativeResult",
                            &serde_json::json!([id, 1234]),
                        ),
                    );
                }
                "test:pedResult" => {
                    tx.send(args[0].clone()).unwrap();
                    return;
                }
                _ => {}
            }
        }
    });

    let ped = tokio::task::spawn_blocking(move || {
        rx.recv_timeout(std::time::Duration::from_secs(15)).unwrap()
    })
    .await
    .unwrap();
    assert_eq!(ped, serde_json::json!(1234));
}

/// B5 exit criterion: full spawn sequence — playerJoining fires on UDP
/// connect, the server pushes 'axiom:core:spawnCharacter', the (simulated)
/// client runs the spawn natives and reports 'axiom:core:onCharacterSpawned'.
#[tokio::test(flavor = "multi_thread")]
async fn spawn_sequence_end_to_end() {
    let players = Arc::new(PlayerDirectory::new());
    let (net_bridge, net_rx) = NetBridge::new();
    let script_host = ScriptHost::spawn_with_net(
        Arc::new(DeferralRegistry::new()),
        Arc::clone(&players),
        net_bridge,
    )
    .unwrap();
    // Load the real axiom-core server script from the repo.
    let server_js = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../resources/axiom-core/dist/server/index.js"),
    )
    .unwrap();
    // Observe onCharacterSpawned from Rust via a marker event.
    let observer = r#"
        AddEventHandler('axiom:core:onCharacterSpawned', () => {
          TriggerClientEvent('test:spawnObserved', globalThis.source ?? 1);
        });
    "#;
    script_host
        .load_resource(
            "axiom-core",
            vec![
                ScriptSource {
                    path: "dist/server/index.js".into(),
                    code: server_js,
                },
                ScriptSource {
                    path: "observer.js".into(),
                    code: observer.into(),
                },
            ],
        )
        .await
        .unwrap();

    let source = players.allocate_source();
    players.insert(PlayerInfo {
        source,
        name: "Spawnee".into(),
        identifiers: vec![format!("license:dev-{source}")],
    });
    players.bind_token("spawn-token".into(), source);

    let port = {
        let probe = UdpSocket::bind("127.0.0.1:0").unwrap();
        probe.local_addr().unwrap().port()
    };
    baston_gateway::udp::spawn_with_net(port, 2, 8, players, script_host, Some(net_rx)).unwrap();

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut client = TestClient::connect(port);
        client.wait_event(Duration::from_secs(5)).expect("connect");
        client.send(0, &handshake_packet("spawn-token"));

        // Simulated FiveM client: track observed events, execute natives.
        let mut got_convars = false;
        let mut got_spawn_event = false;
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            let Some((_, data, _)) = client.wait_event(Duration::from_secs(5)) else {
                continue;
            };
            if let Some((ty, _)) = read_message_type(&data) {
                if ty == baston_protocol::udp::hash_rage_string("msgConVars") {
                    got_convars = true;
                    continue;
                }
            }
            let Some((name, args)) = parse_net_event(&data) else {
                continue;
            };
            match name.as_str() {
                "axiom:core:spawnCharacter" => {
                    let opts = &args[0];
                    assert_eq!(opts["model"], "mp_m_freemode_01");
                    assert_eq!(opts["x"], -1037.0);
                    got_spawn_event = true;
                    // Client script would run the spawn natives here, then:
                    client.send(
                        0,
                        &build_server_event(
                            "axiom:core:onCharacterSpawned",
                            &serde_json::json!([]),
                        ),
                    );
                }
                "test:spawnObserved" => {
                    tx.send((got_convars, got_spawn_event)).unwrap();
                    return;
                }
                _ => {}
            }
        }
    });

    let (got_convars, got_spawn_event) = tokio::task::spawn_blocking(move || {
        rx.recv_timeout(std::time::Duration::from_secs(20)).unwrap()
    })
    .await
    .unwrap();
    assert!(got_convars, "msgConVars sent after handshake");
    assert!(
        got_spawn_event,
        "spawnCharacter event pushed on playerJoining"
    );
}

/// The pre-connect probe: raw `0xFFFFFFFF getinfo <challenge>` datagram must
/// get an `infoResponse` from the same port (NetLibrary CS_FETCHING).
#[tokio::test(flavor = "multi_thread")]
async fn oob_getinfo_gets_info_response() {
    let (port, _players, _token) = start_server().await;

    let reply = tokio::task::spawn_blocking(move || {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut probe = vec![0xFF, 0xFF, 0xFF, 0xFF];
        probe.extend_from_slice(b"getinfo xyz");
        socket
            .send_to(&probe, ("127.0.0.1", port))
            .expect("send probe");
        let mut buf = [0u8; 2048];
        let (len, _) = socket.recv_from(&mut buf).expect("no OOB reply");
        buf[..len].to_vec()
    })
    .await
    .unwrap();

    assert_eq!(&reply[..4], &[0xFF, 0xFF, 0xFF, 0xFF]);
    let text = std::str::from_utf8(&reply[4..]).unwrap();
    assert!(text.starts_with("infoResponse\n"), "got: {text}");
    assert!(text.contains("\\challenge\\xyz"));
    assert!(text.contains("\\sv_maxclients\\8"));
    assert!(text.contains("\\clients\\1"));
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_token_gets_dropped() {
    let (port, players, _token) = start_server().await;

    let done = std::thread::spawn(move || {
        let mut client = TestClient::connect(port);
        client.wait_event(Duration::from_secs(5)).expect("connect");
        client.send(0, &handshake_packet("wrong-token"));
        // Expect a disconnect (None) rather than a connectOK.
        client.wait_event(Duration::from_secs(3))
    });
    let result = tokio::task::spawn_blocking(move || done.join().unwrap())
        .await
        .unwrap();
    assert!(result.is_none(), "invalid token must not get connectOK");
    // Player from the HTTP phase is still registered (only the peer dropped).
    assert_eq!(players.count(), 1);
}
