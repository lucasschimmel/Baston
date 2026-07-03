//! One simulated client: ENet handshake + 50ms random-walk state reports +
//! snapshot consumption with latency/desync accounting.

use std::collections::HashSet;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use baston_protocol::entity::{EntityId, EntityType};
use baston_protocol::udp::state::{
    build_state_update, parse_snapshot, ClientStateUpdate, EntityOp,
};
use baston_protocol::udp::{read_message_type, MSG_CONNECT};
use baston_protocol::udp::state::MSG_BASTON_SNAPSHOT;
use rusty_enet as enet;

use crate::Stats;

const REPORT_INTERVAL: Duration = Duration::from_millis(50);
const WALK_SPEED_MPS: f32 = 1.5;

/// Deterministic pseudo-random spawn over a 4×4 km play area — matches the
/// roadmap's "position aléatoire sur la map"; with a 450m AoI each client
/// averages a handful of visible neighbors, like a real populated server.
fn spawn_point(index: usize) -> [f32; 3] {
    let h = (index as u64).wrapping_mul(0x9E3779B97F4A7C15);
    let x = ((h >> 8) % 4000) as f32 - 2000.0;
    let y = ((h >> 32) % 4000) as f32 - 2000.0;
    [x, y, 20.0]
}

/// Cheap deterministic direction change (no rand dependency).
fn heading_for(index: usize, tick: u64) -> f32 {
    (((index as u64).wrapping_mul(2654435761).wrapping_add(tick / 40)) % 360) as f32
}

/// Recover the 32-bit send-time from the health/armour stamp pair.
fn record_latency(stats: &Stats, health: f32, armour: f32) {
    let sent = (health as u32) & 0xFFFF | ((armour as u32) << 16);
    let now = stats.now_ms();
    if sent > 0 && now >= sent && now - sent < 10_000 {
        stats.latencies_ms.lock().unwrap().push(f64::from(now - sent));
    }
}

pub fn run_client(index: usize, token: String, server: SocketAddr, stats: Arc<Stats>) {
    let socket = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(_) => {
            stats.dropped_connections.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };
    let mut host = match enet::Host::new(
        socket,
        enet::HostSettings {
            peer_limit: 1,
            channel_limit: 2,
            ..Default::default()
        },
    ) {
        Ok(h) => h,
        Err(_) => {
            stats.dropped_connections.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };
    if host.connect(server, 2, 0).is_err() {
        stats.dropped_connections.fetch_add(1, Ordering::Relaxed);
        return;
    }

    // Handshake.
    let mut handshake = MSG_CONNECT.to_le_bytes().to_vec();
    handshake.extend_from_slice(format!("token={token}&guid={index}").as_bytes());
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut connected = false;
    let mut sent_handshake = false;
    while Instant::now() < deadline && !connected {
        match host.service() {
            Ok(Some(enet::Event::Connect { peer, .. })) if !sent_handshake => {
                let _ = peer.send(0, &enet::Packet::reliable(handshake.as_slice()));
                sent_handshake = true;
            }
            Ok(Some(enet::Event::Receive { packet, .. })) => {
                if read_message_type(packet.data()).is_some_and(|(ty, _)| ty == MSG_CONNECT) {
                    connected = true;
                }
            }
            Ok(Some(enet::Event::Disconnect { .. })) => break,
            _ => std::thread::sleep(Duration::from_millis(1)),
        }
    }
    if !connected {
        stats.dropped_connections.fetch_add(1, Ordering::Relaxed);
        return;
    }
    stats.connected.fetch_add(1, Ordering::Relaxed);

    // Main loop: walk + report + consume snapshots.
    let mut coords = spawn_point(index);
    let mut tick: u64 = 0;
    let mut last_report = Instant::now() - REPORT_INTERVAL;
    let mut known: HashSet<EntityId> = HashSet::new();
    let mut deleted: HashSet<EntityId> = HashSet::new();
    // Last seen (health, armour) per entity — the latency stamp may arrive
    // split across deltas.
    let mut stamps: std::collections::HashMap<EntityId, (f32, f32)> =
        std::collections::HashMap::new();
    let mut dropped = false;

    while !stats.stop.load(Ordering::Relaxed) {
        if last_report.elapsed() >= REPORT_INTERVAL {
            last_report = Instant::now();
            tick += 1;
            let heading = heading_for(index, tick);
            let (dx, dy) = heading.to_radians().sin_cos();
            let step = WALK_SPEED_MPS * REPORT_INTERVAL.as_secs_f32();
            coords[0] += dx * step;
            coords[1] += dy * step;

            // Stamp send-time into health/armour (16 bits each, exact f32).
            let sent = stats.now_ms();
            let update = ClientStateUpdate {
                entity_id: None,
                entity_type: EntityType::Player,
                model_hash: 0x705E61F2,
                coords,
                heading,
                velocity: [dx * WALK_SPEED_MPS, dy * WALK_SPEED_MPS, 0.0],
                health: (sent & 0xFFFF) as f32,
                armour: (sent >> 16) as f32,
                extra: None,
            };
            let packet = build_state_update(&update);
            let _ = host
                .peer_mut(enet::PeerID(0))
                .send(1, &enet::Packet::unreliable(packet.as_slice()));
        }

        match host.service() {
            Ok(Some(enet::Event::Receive { packet, .. })) => {
                let data = packet.data();
                stats
                    .bytes_received
                    .fetch_add(data.len() as u64, Ordering::Relaxed);
                let Some((ty, payload)) = read_message_type(data) else {
                    continue;
                };
                if ty != MSG_BASTON_SNAPSHOT {
                    continue;
                }
                let Some(snapshot) = parse_snapshot(payload) else {
                    stats.desyncs.fetch_add(1, Ordering::Relaxed);
                    continue;
                };
                stats.snapshots_received.fetch_add(1, Ordering::Relaxed);
                for op in snapshot.ops {
                    match op {
                        EntityOp::Upsert(dirty) => {
                            use baston_protocol::entity::DirtyFlags;
                            let id = dirty.entity_id;
                            // Upsert is create-or-update; the only ordering
                            // violation is a zombie update after a Delete
                            // that no Create ever superseded.
                            let is_create = dirty.dirty_fields.contains(DirtyFlags::CREATED);
                            if is_create {
                                deleted.remove(&id);
                            } else if deleted.contains(&id) {
                                stats.desyncs.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }
                            known.insert(id);
                            stamps.insert(id, (dirty.state.health, dirty.state.armour));
                            record_latency(&stats, dirty.state.health, dirty.state.armour);
                        }
                        EntityOp::Delta(delta) => {
                            let id = delta.entity_id;
                            if deleted.contains(&id) || !known.contains(&id) {
                                stats.desyncs.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }
                            let entry = stamps.entry(id).or_insert((0.0, 0.0));
                            if let Some(h) = delta.health {
                                entry.0 = h;
                            }
                            if let Some(a) = delta.armour {
                                entry.1 = a;
                            }
                            if delta.health.is_some() || delta.armour.is_some() {
                                record_latency(&stats, entry.0, entry.1);
                            }
                        }
                        EntityOp::Delete(id) => {
                            if !known.remove(&id) {
                                stats.desyncs.fetch_add(1, Ordering::Relaxed);
                            }
                            stamps.remove(&id);
                            deleted.insert(id);
                        }
                    }
                }
            }
            Ok(Some(enet::Event::Disconnect { .. })) => {
                dropped = true;
                break;
            }
            Ok(_) => std::thread::sleep(Duration::from_millis(1)),
            Err(_) => {
                dropped = true;
                break;
            }
        }
    }

    if dropped {
        stats.dropped_connections.fetch_add(1, Ordering::Relaxed);
    } else {
        // Clean shutdown.
        host.peer_mut(enet::PeerID(0)).disconnect(0);
        for _ in 0..20 {
            let _ = host.service();
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}
