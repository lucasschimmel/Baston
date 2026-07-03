//! B3 exit-criterion tests: a real ENet client performs the FiveM game
//! handshake against the BASTON UDP server on loopback.

use std::net::UdpSocket;
use std::sync::Arc;
use std::time::{Duration, Instant};

use baston_protocol::udp::{handshake, read_message_type, time_sync, MSG_CONNECT, MSG_TIME_SYNC};
use baston_protocol::{PlayerDirectory, PlayerInfo};
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
    assert_eq!(data, handshake::build_connect_ok(1));
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
        let (channel, data, _) = client
            .wait_event(Duration::from_secs(5))
            .expect("no time sync response");
        tx.send((channel, data)).unwrap();
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
