//! Jalon C4 exit-criterion tests: two simulated clients connected to BASTON
//! see each other — via the msgRoute P2P relay (real-FiveM path) and via the
//! BASTON snapshot pipeline (binary-protocol path, requires NATS).

use std::net::UdpSocket;
use std::sync::Arc;
use std::time::{Duration, Instant};

use baston_protocol::entity::{DirtyFlags, EntityType};
use baston_protocol::udp::state::{
    build_state_update, parse_snapshot, ClientStateUpdate, EntityOp, MSG_BASTON_SNAPSHOT,
};
use baston_protocol::udp::{read_message_type, route, MSG_CONNECT, MSG_ROUTE};
use baston_protocol::{PlayerDirectory, PlayerInfo};
use baston_scripting::{DeferralRegistry, ScriptHost};
use baston_zone::state_sync::{state_subject, StateSyncEmitter};
use baston_zone::{EntityManager, StateIngest};
use rusty_enet as enet;

const NATS_URL: &str = "nats://127.0.0.1:4222";

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

    /// Drive the ENet host without waiting: sends only leave the queue on
    /// `service()`, so senders must be pumped even when expecting nothing.
    fn pump(&mut self) {
        while let Ok(Some(_)) = self.host.service() {}
    }

    fn handshake(&mut self, token: &str) {
        self.wait_event(Duration::from_secs(5)).expect("connect");
        let mut data = MSG_CONNECT.to_le_bytes().to_vec();
        data.extend_from_slice(format!("token={token}&guid=42").as_bytes());
        self.send(0, &data);
        let (_, reply, _) = self.wait_event(Duration::from_secs(5)).expect("connectOK");
        let (ty, _) = read_message_type(&reply).unwrap();
        assert_eq!(ty, MSG_CONNECT);
    }

    /// Pump events until a packet of `msg_type` arrives; return its payload.
    fn wait_for_message(&mut self, msg_type: u32, timeout: Duration) -> Option<(u8, Vec<u8>)> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let (channel, data, connected) = self.wait_event(remaining)?;
            if connected {
                continue;
            }
            if let Some((ty, payload)) = read_message_type(&data) {
                if ty == msg_type {
                    return Some((channel, payload.to_vec()));
                }
            }
        }
        None
    }
}

struct Server {
    port: u16,
    ingest: Arc<StateIngest>,
    entity_manager: Arc<EntityManager>,
    players: Arc<PlayerDirectory>,
    udp: baston_gateway::udp::UdpHandle,
}

fn start_server(player_names: &[&str]) -> (Server, Vec<String>) {
    let players = Arc::new(PlayerDirectory::new());
    let script_host = ScriptHost::spawn(Arc::new(DeferralRegistry::new()), Arc::clone(&players))
        .expect("script host");
    let entity_manager = Arc::new(EntityManager::new());
    let ingest = Arc::new(StateIngest::new(Arc::clone(&entity_manager), 200.0));

    let mut tokens = Vec::new();
    for name in player_names {
        let source = players.allocate_source();
        players.insert(PlayerInfo {
            source,
            name: (*name).to_owned(),
            identifiers: vec![format!("license:dev-{source}")],
        });
        let token = format!("token-{source}");
        players.bind_token(token.clone(), source);
        tokens.push(token);
    }

    let port = {
        let probe = UdpSocket::bind("127.0.0.1:0").unwrap();
        probe.local_addr().unwrap().port()
    };
    let udp = baston_gateway::udp::spawn_with_net(
        port,
        2,
        8,
        Arc::clone(&players),
        script_host,
        None,
        Some(Arc::clone(&ingest)),
    )
    .unwrap();
    (
        Server {
            port,
            ingest,
            entity_manager,
            players,
            udp,
        },
        tokens,
    )
}

/// Private coordinate region for this binary — concurrent test binaries
/// share the NATS stream and must not overlap AoIs.
const BASE: [f32; 3] = [50_000.0, 50_000.0, 20.0];

fn offset(local: [f32; 3]) -> [f32; 3] {
    [BASE[0] + local[0], BASE[1] + local[1], BASE[2] + local[2]]
}

fn state_update(coords: [f32; 3]) -> ClientStateUpdate {
    ClientStateUpdate {
        entity_id: None,
        entity_type: EntityType::Player,
        model_hash: 0x705E61F2,
        coords,
        heading: 0.0,
        velocity: [1.0, 0.0, 0.0],
        health: 100.0,
        armour: 0.0,
        extra: None,
    }
}

/// msgRoute relay: client A's GTA sync data reaches client B with the
/// sender netId rewritten; self-addressed routes go nowhere.
#[tokio::test(flavor = "multi_thread")]
async fn msg_route_is_relayed_between_clients() {
    let (server, tokens) = start_server(&["A", "B"]);
    let port = server.port;
    let (token_a, token_b) = (tokens[0].clone(), tokens[1].clone());

    let result = tokio::task::spawn_blocking(move || {
        let mut a = TestClient::connect(port);
        a.handshake(&token_a);
        let mut b = TestClient::connect(port);
        b.handshake(&token_b);

        // A → B (netId 2): opaque sync payload.
        let mut packet = MSG_ROUTE.to_le_bytes().to_vec();
        packet.extend_from_slice(&2u16.to_le_bytes());
        packet.extend_from_slice(&8u16.to_le_bytes());
        packet.extend_from_slice(b"gta-sync");
        // A → A (self): must be dropped by the server.
        let mut self_packet = MSG_ROUTE.to_le_bytes().to_vec();
        self_packet.extend_from_slice(&1u16.to_le_bytes());
        self_packet.extend_from_slice(&4u16.to_le_bytes());
        self_packet.extend_from_slice(b"loop");
        // Retry: the relay is unreliable and B's peer may still be settling.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            a.send(0, &packet);
            a.send(0, &self_packet);
            a.pump();
            if let Some((channel, payload)) =
                b.wait_for_message(MSG_ROUTE, Duration::from_millis(300))
            {
                return (
                    channel,
                    payload,
                    a.wait_for_message(MSG_ROUTE, Duration::from_millis(200)),
                );
            }
            assert!(Instant::now() < deadline, "route never relayed to B");
        }
    })
    .await
    .unwrap();

    let (channel, payload, self_route) = result;
    assert_eq!(channel, 1, "relayed msgRoute rides channel 1");
    let parsed = route::parse_client_route(&payload).unwrap();
    assert_eq!(parsed.target_net_id, 1, "leading netId rewritten to sender");
    assert_eq!(parsed.data, b"gta-sync");
    assert!(self_route.is_none(), "self-addressed route must be dropped");
}

/// Full binary pipeline: A reports state → NATS → aggregator → B receives
/// Create, then Update, then Delete when A teleports out of AoI (via server
/// respawn — direct teleport is anti-cheat-rejected). A never sees itself.
#[tokio::test(flavor = "multi_thread")]
async fn snapshot_pipeline_between_two_clients() {
    let Ok(nats) = async_nats::connect(NATS_URL).await else {
        eprintln!("SKIPPED: NATS not reachable at {NATS_URL}");
        return;
    };

    let (server, tokens) = start_server(&["A", "B"]);
    let port = server.port;
    let (token_a, token_b) = (tokens[0].clone(), tokens[1].clone());

    // Zone emitter + gateway aggregator wired to the live UDP handle.
    let zone_id = format!("c4-{}", uuid::Uuid::new_v4().simple());
    let emitter = StateSyncEmitter::new(
        zone_id.clone(),
        nats.clone(),
        Arc::clone(&server.entity_manager),
        16,
    );
    let subject = state_subject(&zone_id);
    tokio::spawn(async move {
        // Manual 16ms pump against the unique test subject.
        let mut interval = tokio::time::interval(Duration::from_millis(16));
        loop {
            interval.tick().await;
            emitter.emit_once(&subject).await;
        }
    });
    baston_gateway::StateAggregator::new(
        nats,
        Arc::clone(&server.players),
        Arc::clone(&server.ingest),
        server.udp.clone(),
        450.0,
        20,
    )
    .with_consumer_name(format!("test-{}", uuid::Uuid::new_v4().simple()))
    .spawn();
    // Let the durable consumer attach before clients start reporting.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let result = tokio::task::spawn_blocking(move || {
        let mut a = TestClient::connect(port);
        a.handshake(&token_a);
        let mut b = TestClient::connect(port);
        b.handshake(&token_b);

        // Both clients opt into the binary protocol by reporting state.
        // A at origin, B 100m away.
        let deadline = Instant::now() + Duration::from_secs(15);

        // Phase 1: B must receive A's entity as a CREATED upsert.
        let a_entity;
        loop {
            a.send(
                1,
                &build_state_update(&state_update(offset([0.0, 0.0, 0.0]))),
            );
            a.pump();
            b.send(
                1,
                &build_state_update(&state_update(offset([100.0, 0.0, 0.0]))),
            );
            if let Some((_, payload)) =
                b.wait_for_message(MSG_BASTON_SNAPSHOT, Duration::from_millis(300))
            {
                let snapshot = parse_snapshot(&payload).unwrap();
                // Concurrent test binaries share the NATS stream; find A's
                // entity by owner rather than assuming it is the only one.
                let found = snapshot.ops.iter().find_map(|op| match op {
                    EntityOp::Upsert(d) if d.state.network_owner == Some(1) => Some(d.clone()),
                    _ => None,
                });
                if let Some(d) = found {
                    assert!(d.dirty_fields.contains(DirtyFlags::CREATED));
                    assert_eq!(d.state.coords, offset([0.0, 0.0, 0.0]));
                    a_entity = Some(d.entity_id);
                    break;
                }
            }
            assert!(Instant::now() < deadline, "B never received A's entity");
        }
        let a_entity = a_entity.unwrap();

        // Phase 2: A moves; B sees a non-CREATED update with new coords.
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            a.send(
                1,
                &build_state_update(&state_update(offset([5.0, 0.0, 0.0]))),
            );
            a.pump();
            if let Some((_, payload)) =
                b.wait_for_message(MSG_BASTON_SNAPSHOT, Duration::from_millis(300))
            {
                let snapshot = parse_snapshot(&payload).unwrap();
                let moved = snapshot.ops.iter().any(|op| {
                    matches!(op,
                    EntityOp::Delta(d)
                        if d.entity_id == a_entity && d.coords == Some(offset([5.0, 0.0, 0.0])))
                });
                if moved {
                    break;
                }
            }
            assert!(Instant::now() < deadline, "B never saw A's movement");
        }

        // Phase 3: A leaves (disconnect) → B receives a Delete for A's entity.
        drop(a);
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            b.send(
                1,
                &build_state_update(&state_update(offset([100.0, 0.0, 0.0]))),
            );
            if let Some((_, payload)) =
                b.wait_for_message(MSG_BASTON_SNAPSHOT, Duration::from_millis(300))
            {
                let snapshot = parse_snapshot(&payload).unwrap();
                if snapshot
                    .ops
                    .iter()
                    .any(|op| matches!(op, EntityOp::Delete(id) if *id == a_entity))
                {
                    break;
                }
            }
            assert!(Instant::now() < deadline, "B never received the Delete");
        }

        // Phase 4: B keeps reporting; B must never receive its own entity.
        for _ in 0..10 {
            b.send(
                1,
                &build_state_update(&state_update(offset([100.0, 0.0, 0.0]))),
            );
            if let Some((_, payload)) =
                b.wait_for_message(MSG_BASTON_SNAPSHOT, Duration::from_millis(100))
            {
                let snapshot = parse_snapshot(&payload).unwrap();
                for op in &snapshot.ops {
                    if let EntityOp::Upsert(d) = op {
                        assert_ne!(
                            d.state.network_owner,
                            Some(2),
                            "B must not be echoed its own entity"
                        );
                    }
                    if let EntityOp::Delta(d) = op {
                        // Deltas always follow an Upsert baseline; A is gone,
                        // so nothing (least of all B itself) may delta here.
                        panic!("unexpected delta after A left: {:?}", d.entity_id);
                    }
                }
            }
        }
        true
    });

    let ok = result.await.unwrap();
    assert!(ok);
}
