//! End-to-end `displayinfo`: a real ENet client subscribes over the game
//! transport and receives server-assembled snapshots.
//!
//! The feed is answered by the ENet task itself, so these tests also pin the
//! property that matters most about it: it needs no resource, no script
//! runtime, and no scripting event handler to work.

use std::net::UdpSocket;
use std::sync::Arc;
use std::time::{Duration, Instant};

use baston_config::{DebugConfig, DisplayInfoAccess};
use baston_gateway::debug_info::DebugFeedSetup;
use baston_protocol::debug_info::{DEBUG_INFO_DENIED_EVENT, DEBUG_INFO_EVENT, DEBUG_INFO_VERSION};
use baston_protocol::rage::sync_parse::GameBuild;
use baston_protocol::udp::{read_message_type, MSG_CONNECT, MSG_NET_EVENT, MSG_SERVER_EVENT};
use baston_protocol::{events, PlayerDirectory, PlayerInfo};
use baston_scripting::{DeferralRegistry, ScriptHost};
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

    fn wait_connect(&mut self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match self.host.service().unwrap() {
                Some(enet::Event::Connect { .. }) => return,
                Some(_) => {}
                None => std::thread::sleep(Duration::from_millis(2)),
            }
        }
        panic!("client never connected");
    }

    fn send(&mut self, channel: u8, data: &[u8]) {
        self.host
            .peer_mut(enet::PeerID(0))
            .send(channel, &enet::Packet::reliable(data))
            .unwrap();
    }

    /// Pump until a named `msgNetEvent` arrives, or the timeout elapses.
    /// Other traffic (convar replication, `onPlayerJoining`) is skipped.
    fn wait_named_event(&mut self, name: &str, timeout: Duration) -> Option<serde_json::Value> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match self.host.service().unwrap() {
                Some(enet::Event::Receive { packet, .. }) => {
                    if let Some((event, args)) = parse_net_event(packet.data()) {
                        if event == name {
                            return Some(args);
                        }
                    }
                }
                Some(enet::Event::Disconnect { .. }) => return None,
                Some(_) => {}
                None => std::thread::sleep(Duration::from_millis(2)),
            }
        }
        None
    }
}

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

fn handshake_packet(token: &str) -> Vec<u8> {
    let mut data = MSG_CONNECT.to_le_bytes().to_vec();
    data.extend_from_slice(format!("token={token}&guid=42").as_bytes());
    data
}

fn build_server_event(name: &str, args: &serde_json::Value) -> Vec<u8> {
    let mut out = MSG_SERVER_EVENT.to_le_bytes().to_vec();
    out.extend_from_slice(&((name.len() + 1) as u16).to_le_bytes());
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    out.extend_from_slice(&rmp_serde::to_vec(args).unwrap());
    out
}

fn toggle(on: bool) -> Vec<u8> {
    build_server_event(
        baston_protocol::debug_info::DEBUG_INFO_TOGGLE_EVENT,
        &serde_json::json!([on]),
    )
}

/// A server with the overlay configured as given, and one player that has
/// finished the HTTP phase. No resources are loaded on purpose.
async fn start_server(access: DisplayInfoAccess, allow: Vec<String>) -> (u16, String, u32) {
    let players = Arc::new(PlayerDirectory::new());
    let script_host = ScriptHost::spawn(Arc::new(DeferralRegistry::new()), Arc::clone(&players))
        .expect("script host");

    let source = players.allocate_source();
    players.insert(PlayerInfo {
        source,
        name: "OverlayTester".into(),
        identifiers: vec!["license:tester".to_owned()],
    });
    let token = format!("displayinfo-token-{source}");
    players.bind_token(token.clone(), source);

    let port = {
        let probe = UdpSocket::bind("127.0.0.1:0").unwrap();
        probe.local_addr().unwrap().port()
    };
    baston_gateway::udp::spawn_with_mesh_on(
        std::net::IpAddr::from([127, 0, 0, 1]),
        port,
        2,
        8,
        players,
        script_host,
        None,
        None,
        None,
        Default::default(),
        GameBuild(3258),
        DebugFeedSetup {
            config: DebugConfig {
                display_info: access,
                allow,
                // Fast enough that a test does not wait a second per snapshot.
                refresh_hz: 20,
            },
            server_name: "OverlayServer".to_owned(),
            mesh: None,
        },
    )
    .expect("udp server");
    (port, token, source)
}

#[tokio::test(flavor = "multi_thread")]
async fn subscribing_yields_server_assembled_snapshots() {
    let (port, token, source) = start_server(DisplayInfoAccess::Everyone, Vec::new()).await;

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut client = TestClient::connect(port);
        client.wait_connect(Duration::from_secs(5));
        client.send(0, &handshake_packet(&token));
        client.send(0, &toggle(true));
        let first = client.wait_named_event(DEBUG_INFO_EVENT, Duration::from_secs(5));

        // Unsubscribing must actually stop the feed, not just hide it.
        client.send(0, &toggle(false));
        // Drain whatever was already in flight before the toggle landed.
        client.wait_named_event(DEBUG_INFO_EVENT, Duration::from_millis(300));
        let after = client.wait_named_event(DEBUG_INFO_EVENT, Duration::from_millis(800));
        let _ = tx.send((first, after));
    });

    let (first, after) = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("client thread");
    let args = first.expect("a snapshot arrives after subscribing");
    let snapshot = &args[0];

    assert_eq!(snapshot["v"], DEBUG_INFO_VERSION);
    assert_eq!(snapshot["server"]["name"], "OverlayServer");
    assert_eq!(snapshot["server"]["build"], 3258);
    assert_eq!(snapshot["server"]["max_players"], 8);
    assert_eq!(snapshot["player"]["source"], source);
    assert_eq!(snapshot["player"]["name"], "OverlayTester");
    // The link is measured by the server, so an established peer has an MTU.
    assert!(snapshot["net"]["mtu"].as_u64().unwrap() > 0);
    // Neither subsystem is running here, and both must say so by being absent
    // rather than by reporting zeroes.
    assert!(snapshot.get("onesync").is_none(), "{snapshot}");
    assert!(snapshot.get("mesh").is_none(), "{snapshot}");

    assert!(
        after.is_none(),
        "the feed must stop when the client unsubscribes"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unlisted_player_is_refused_with_a_reason() {
    let (port, token, _) = start_server(
        DisplayInfoAccess::Allowlist,
        vec!["license:somebody-else".to_owned()],
    )
    .await;

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut client = TestClient::connect(port);
        client.wait_connect(Duration::from_secs(5));
        client.send(0, &handshake_packet(&token));
        client.send(0, &toggle(true));
        let denied = client.wait_named_event(DEBUG_INFO_DENIED_EVENT, Duration::from_secs(5));
        let snapshot = client.wait_named_event(DEBUG_INFO_EVENT, Duration::from_millis(500));
        let _ = tx.send((denied, snapshot));
    });

    let (denied, snapshot) = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("client thread");
    let args = denied.expect("a refusal is reported to the client");
    assert!(
        args[0].as_str().unwrap_or_default().contains("allowlist"),
        "the refusal must say why: {args}"
    );
    assert!(
        snapshot.is_none(),
        "a refused player must receive no snapshots at all"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_disabled_overlay_answers_no_one() {
    let (port, token, _) = start_server(DisplayInfoAccess::Off, Vec::new()).await;

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut client = TestClient::connect(port);
        client.wait_connect(Duration::from_secs(5));
        client.send(0, &handshake_packet(&token));
        client.send(0, &toggle(true));
        let denied = client.wait_named_event(DEBUG_INFO_DENIED_EVENT, Duration::from_secs(3));
        let snapshot = client.wait_named_event(DEBUG_INFO_EVENT, Duration::from_millis(500));
        let _ = tx.send((denied, snapshot));
    });

    let (denied, snapshot) = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("client thread");
    assert!(denied.is_some(), "the client is told the feed is off");
    assert!(snapshot.is_none());
}
