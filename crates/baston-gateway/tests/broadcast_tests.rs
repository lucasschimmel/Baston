//! `TriggerClientEvent(name, -1, …)` — one packet, every connected client.
//!
//! FiveM spells "everyone" as a source of `-1`, and BASTON had no path for it:
//! every outbound event went to a single peer, so a broadcast was silently a
//! message to nobody. These drive two real ENet clients through the handshake
//! and check that one send reaches both.

use std::net::UdpSocket;
use std::sync::Arc;
use std::time::{Duration, Instant};

use baston_protocol::udp::MSG_CONNECT;
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

    /// Pump until a data packet arrives, ignoring the connect event.
    fn wait_data(&mut self, timeout: Duration) -> Option<Vec<u8>> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match self.host.service().unwrap() {
                Some(enet::Event::Receive { packet, .. }) => return Some(packet.data().to_vec()),
                Some(enet::Event::Disconnect { .. }) => return None,
                _ => std::thread::sleep(Duration::from_millis(2)),
            }
        }
        None
    }

    /// Pump until a packet mentioning `needle` arrives.
    ///
    /// A client receives other traffic right after the handshake, so "the next
    /// packet" is not "the packet under test" — asserting on the former made
    /// this suite pass with the broadcast deliberately disabled.
    fn wait_for(&mut self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.wait_data(remaining) {
                Some(data) => {
                    if data.windows(needle.len()).any(|w| w == needle.as_bytes()) {
                        return true;
                    }
                }
                None => return false,
            }
        }
        false
    }

    fn wait_connected(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(enet::Event::Connect { .. }) = self.host.service().unwrap() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        false
    }

    fn send(&mut self, data: &[u8]) {
        self.host
            .peer_mut(enet::PeerID(0))
            .send(0, &enet::Packet::reliable(data))
            .unwrap();
    }
}

fn handshake_packet(token: &str) -> Vec<u8> {
    let mut data = MSG_CONNECT.to_le_bytes().to_vec();
    data.extend_from_slice(format!("token={token}&guid=42").as_bytes());
    data
}

/// A server with `count` players already through the HTTP phase.
async fn start_server(count: usize) -> (u16, Vec<String>, baston_gateway::udp::UdpHandle) {
    let players = Arc::new(PlayerDirectory::new());
    let script_host = ScriptHost::spawn(Arc::new(DeferralRegistry::new()), Arc::clone(&players))
        .expect("script host");

    let tokens: Vec<String> = (0..count)
        .map(|i| {
            let source = players.allocate_source();
            players.insert(PlayerInfo {
                source,
                name: format!("Broadcaster{i}"),
                identifiers: vec![format!("license:dev-{source}")],
            });
            let token = format!("broadcast-token-{i}");
            players.bind_token(token.clone(), source);
            token
        })
        .collect();

    let port = {
        let probe = UdpSocket::bind("127.0.0.1:0").unwrap();
        probe.local_addr().unwrap().port()
    };
    let handle = baston_gateway::udp::spawn(port, 2, 8, players, script_host).expect("udp server");
    (port, tokens, handle)
}

/// Connect a client, complete the handshake, and hand back a thread that is
/// waiting for the next packet the server sends it.
fn connected_client(
    port: u16,
    token: String,
    needle: String,
) -> (std::sync::mpsc::Receiver<bool>, std::sync::mpsc::Sender<()>) {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let (data_tx, data_rx) = std::sync::mpsc::channel();

    std::thread::spawn(move || {
        let mut client = TestClient::connect(port);
        assert!(
            client.wait_connected(Duration::from_secs(5)),
            "ENet connect"
        );
        client.send(&handshake_packet(&token));
        // connectOK
        assert!(
            client.wait_data(Duration::from_secs(5)).is_some(),
            "no connectOK"
        );
        ready_tx.send(()).unwrap();
        // Wait for the broadcast to be sent, then read what arrives.
        go_rx.recv().ok();
        data_tx
            .send(client.wait_for(&needle, Duration::from_secs(5)))
            .unwrap();
    });

    ready_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("ready");
    (data_rx, go_tx)
}

#[tokio::test(flavor = "multi_thread")]
async fn one_broadcast_reaches_every_connected_client() {
    let (port, tokens, handle) = start_server(2).await;

    const EVENT: &str = "chat:addMessage";
    let (first, first_go) = connected_client(port, tokens[0].clone(), EVENT.to_owned());
    let (second, second_go) = connected_client(port, tokens[1].clone(), EVENT.to_owned());

    let packet = baston_protocol::events::build_net_event(EVENT, &rmp_serde::to_vec(&()).unwrap());
    handle.control().broadcast(0, packet);
    first_go.send(()).unwrap();
    second_go.send(()).unwrap();

    let received = tokio::task::spawn_blocking(move || {
        (
            first.recv_timeout(Duration::from_secs(10)).unwrap(),
            second.recv_timeout(Duration::from_secs(10)).unwrap(),
        )
    })
    .await
    .unwrap();

    assert!(received.0, "the first client never saw the broadcast");
    assert!(
        received.1,
        "the second client never saw it — a broadcast that reaches one peer is \
         the bug this exists to prevent"
    );
}

/// A broadcast with nobody connected is a no-op, not a failure: a resource
/// announcing something at startup must not care whether anyone is listening.
#[tokio::test(flavor = "multi_thread")]
async fn broadcasting_to_an_empty_server_is_harmless() {
    let (_port, _tokens, handle) = start_server(0).await;
    let packet = baston_protocol::events::build_net_event("startup", &[]);
    handle.control().broadcast(0, packet);
    // Still alive and accepting commands afterwards.
    tokio::time::sleep(Duration::from_millis(100)).await;
    handle.control().broadcast(0, vec![1, 2, 3]);
}
