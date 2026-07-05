//! Simulated clients, multiplexed: one OS thread drives a BATCH of clients
//! (own ENet host each) in a round-robin. 2000 clients as 2000 threads melts
//! the scheduler long before the server is the bottleneck; ~25 clients per
//! thread keeps the harness honest.

use std::collections::{HashMap, HashSet};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use baston_protocol::entity::{EntityId, EntityType};
use baston_protocol::udp::state::MSG_BASTON_SNAPSHOT;
use baston_protocol::udp::state::{
    build_state_update, parse_snapshot, ClientStateUpdate, EntityOp,
};
use baston_protocol::udp::{read_message_type, MSG_CONNECT};
use rusty_enet as enet;

use crate::{ClientPlan, Stats};

const REPORT_INTERVAL: Duration = Duration::from_millis(50);
const WALK_SPEED_MPS: f32 = 1.5;
/// Crossers drive across the boundary at vehicle-ish speed so each one
/// crosses roughly once per minute (10%/min fleet-wide with 10% crossers).
const CROSS_SPEED_MPS: f32 = 15.0;
/// How far past the boundary a crosser goes before turning back.
const CROSS_DEPTH: f32 = 400.0;
const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(60);

/// Cheap deterministic direction change (no rand dependency).
fn heading_for(index: usize, tick: u64) -> f32 {
    (((index as u64)
        .wrapping_mul(2654435761)
        .wrapping_add(tick / 40))
        % 360) as f32
}

/// Recover the 32-bit send-time from the health/armour stamp pair.
fn record_latency(stats: &Stats, health: f32, armour: f32) {
    let sent = (health as u32) & 0xFFFF | ((armour as u32) << 16);
    let now = stats.now_ms();
    if sent > 0 && now >= sent && now - sent < 10_000 {
        stats
            .latencies_ms
            .lock()
            .unwrap()
            .push(f64::from(now - sent));
    }
}

enum Stage {
    Connecting {
        deadline: Instant,
        sent_handshake: bool,
    },
    Active,
    Done,
}

/// One simulated client = one ENet PEER on the thread's shared host.
struct Client {
    index: usize,
    plan: ClientPlan,
    stage: Stage,
    coords: [f32; 3],
    cross_dir: f32,
    last_side_positive: bool,
    last_snapshot_at: Option<Instant>,
    /// When this crosser last crossed the boundary — gaps are attributed to
    /// the handoff only inside a window after the crossing; anything else is
    /// harness/OS scheduling noise on a shared machine.
    last_crossing_at: Option<Instant>,
    tick: u64,
    last_report: Instant,
    known: HashSet<EntityId>,
    deleted: HashSet<EntityId>,
    stamps: HashMap<EntityId, (f32, f32)>,
    handshake: Vec<u8>,
}

impl Client {
    fn new(index: usize, token: String, plan: ClientPlan) -> Self {
        let mut handshake = MSG_CONNECT.to_le_bytes().to_vec();
        handshake
            .extend_from_slice(format!("token={token}&guid={index}&bastonClient=1").as_bytes());
        Self {
            index,
            plan,
            stage: Stage::Connecting {
                deadline: Instant::now() + HANDSHAKE_DEADLINE,
                sent_handshake: false,
            },
            coords: plan.spawn,
            cross_dir: if plan.spawn[0] < 0.0 { 1.0 } else { -1.0 },
            last_side_positive: plan.spawn[0] >= 0.0,
            last_snapshot_at: None,
            last_crossing_at: None,
            tick: 0,
            last_report: Instant::now() - REPORT_INTERVAL,
            known: HashSet::new(),
            deleted: HashSet::new(),
            stamps: HashMap::new(),
            handshake,
        }
    }

    /// Send the periodic state report if due (peer must be Active).
    fn maybe_report(&mut self, peer: &mut enet::Peer<UdpSocket>, stats: &Stats) {
        if self.last_report.elapsed() >= REPORT_INTERVAL {
            self.last_report = Instant::now();
            self.tick += 1;
            let (heading, dx, dy, speed);
            if self.plan.crosser {
                if self.coords[0] * self.cross_dir > CROSS_DEPTH {
                    self.cross_dir = -self.cross_dir;
                }
                (dx, dy) = (self.cross_dir, 0.0);
                heading = if self.cross_dir > 0.0 { 90.0 } else { 270.0 };
                speed = CROSS_SPEED_MPS;
            } else {
                heading = heading_for(self.index, self.tick);
                (dx, dy) = heading.to_radians().sin_cos();
                speed = WALK_SPEED_MPS;
            }
            let step = speed * REPORT_INTERVAL.as_secs_f32();
            self.coords[0] += dx * step;
            self.coords[1] += dy * step;
            let side_positive = self.coords[0] >= 0.0;
            if self.plan.crosser && side_positive != self.last_side_positive {
                self.last_side_positive = side_positive;
                self.last_crossing_at = Some(Instant::now());
                stats.crossings.fetch_add(1, Ordering::Relaxed);
            }

            let sent = stats.now_ms();
            let update = ClientStateUpdate {
                entity_id: None,
                entity_type: EntityType::Player,
                model_hash: 0x705E61F2,
                coords: self.coords,
                heading,
                velocity: [dx * speed, dy * speed, 0.0],
                health: (sent & 0xFFFF) as f32,
                armour: (sent >> 16) as f32,
                extra: None,
            };
            let packet = build_state_update(&update);
            let _ = peer.send(1, &enet::Packet::unreliable(packet.as_slice()));
        }
    }

    fn consume_snapshot(&mut self, payload: &[u8], stats: &Stats) {
        let Some(snapshot) = parse_snapshot(payload) else {
            stats.desyncs.fetch_add(1, Ordering::Relaxed);
            return;
        };
        stats.snapshots_received.fetch_add(1, Ordering::Relaxed);
        if self.plan.crosser {
            let now = Instant::now();
            // Only stream gaps overlapping a handoff window (crossing − 1s
            // .. crossing + 2s) measure what the PLAYER would feel from the
            // zone switch; other gaps are shared-machine scheduling noise.
            let in_handoff_window = self
                .last_crossing_at
                .is_some_and(|t| now.duration_since(t) < Duration::from_secs(2))
                || self.last_snapshot_at.is_some_and(|prev| {
                    self.last_crossing_at.is_some_and(|t| t > prev && t <= now)
                });
            if stats.measure_freeze.load(Ordering::Relaxed) && in_handoff_window {
                if let Some(prev) = self.last_snapshot_at {
                    let gap = now.duration_since(prev).as_secs_f64() * 1000.0;
                    stats.crosser_gaps_ms.lock().unwrap().push(gap);
                }
            }
            self.last_snapshot_at = Some(now);
        }
        for op in snapshot.ops {
            match op {
                EntityOp::Upsert(dirty) => {
                    use baston_protocol::entity::DirtyFlags;
                    let id = dirty.entity_id;
                    let is_create = dirty.dirty_fields.contains(DirtyFlags::CREATED);
                    if is_create {
                        self.deleted.remove(&id);
                    } else if self.deleted.contains(&id) {
                        stats.desyncs.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    self.known.insert(id);
                    self.stamps
                        .insert(id, (dirty.state.health, dirty.state.armour));
                    record_latency(stats, dirty.state.health, dirty.state.armour);
                }
                EntityOp::Delta(delta) => {
                    let id = delta.entity_id;
                    if self.deleted.contains(&id) || !self.known.contains(&id) {
                        stats.desyncs.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    let entry = self.stamps.entry(id).or_insert((0.0, 0.0));
                    if let Some(h) = delta.health {
                        entry.0 = h;
                    }
                    if let Some(a) = delta.armour {
                        entry.1 = a;
                    }
                    if delta.health.is_some() || delta.armour.is_some() {
                        record_latency(stats, entry.0, entry.1);
                    }
                }
                EntityOp::Delete(id) => {
                    if !self.known.remove(&id) {
                        stats.desyncs.fetch_add(1, Ordering::Relaxed);
                    }
                    self.stamps.remove(&id);
                    self.deleted.insert(id);
                }
            }
        }
    }
}

/// Drive a batch of clients on one thread until the stop flag.
///
/// ALL clients of the batch share ONE ENet host (one UDP socket) with one
/// peer per client — 2000 clients as 2000 sockets floods the OS UDP stack
/// long before the server is the bottleneck.
pub fn run_batch(batch: Vec<(usize, String, ClientPlan)>, server: SocketAddr, stats: Arc<Stats>) {
    let Ok(socket) = UdpSocket::bind("0.0.0.0:0") else {
        stats
            .dropped_connections
            .fetch_add(batch.len() as u64, Ordering::Relaxed);
        return;
    };
    let mut host = match enet::Host::new(
        socket,
        enet::HostSettings {
            peer_limit: batch.len().max(1),
            channel_limit: 2,
            ..Default::default()
        },
    ) {
        Ok(h) => h,
        Err(_) => {
            stats
                .dropped_connections
                .fetch_add(batch.len() as u64, Ordering::Relaxed);
            return;
        }
    };

    // Connect one peer per client; map PeerID → client slot.
    let mut clients: Vec<Client> = Vec::with_capacity(batch.len());
    let mut peer_ids: Vec<enet::PeerID> = Vec::with_capacity(batch.len());
    let mut peer_slot: HashMap<enet::PeerID, usize> = HashMap::new();
    for (index, token, plan) in batch {
        match host.connect(server, 2, 0) {
            Ok(peer) => {
                let pid = peer.id();
                peer_slot.insert(pid, clients.len());
                peer_ids.push(pid);
                clients.push(Client::new(index, token, plan));
            }
            Err(_) => {
                stats.dropped_connections.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    let mut done_count = 0usize;
    let mut announced_stop = false;
    loop {
        // 1. Drain all host events, routing to the owning client.
        loop {
            match host.service() {
                Ok(Some(event)) => match event {
                    enet::Event::Connect { peer, .. } => {
                        let pid = peer.id();
                        if let Some(&slot) = peer_slot.get(&pid) {
                            let c = &mut clients[slot];
                            if let Stage::Connecting {
                                ref mut sent_handshake,
                                ..
                            } = c.stage
                            {
                                if !*sent_handshake {
                                    let payload = c.handshake.clone();
                                    let _ =
                                        peer.send(0, &enet::Packet::reliable(payload.as_slice()));
                                    *sent_handshake = true;
                                }
                            }
                        }
                    }
                    enet::Event::Receive { peer, packet, .. } => {
                        let pid = peer.id();
                        let Some(&slot) = peer_slot.get(&pid) else {
                            continue;
                        };
                        let c = &mut clients[slot];
                        let data = packet.data();
                        stats
                            .bytes_received
                            .fetch_add(data.len() as u64, Ordering::Relaxed);
                        let Some((ty, payload)) = read_message_type(data) else {
                            continue;
                        };
                        match c.stage {
                            Stage::Connecting { .. } if ty == MSG_CONNECT => {
                                stats.connected.fetch_add(1, Ordering::Relaxed);
                                c.stage = Stage::Active;
                            }
                            Stage::Active if ty == MSG_BASTON_SNAPSHOT => {
                                c.consume_snapshot(payload, &stats);
                            }
                            _ => {}
                        }
                    }
                    enet::Event::Disconnect { peer, .. } => {
                        let pid = peer.id();
                        if let Some(&slot) = peer_slot.get(&pid) {
                            let c = &mut clients[slot];
                            if !matches!(c.stage, Stage::Done) {
                                stats.dropped_connections.fetch_add(1, Ordering::Relaxed);
                                c.stage = Stage::Done;
                                done_count += 1;
                            }
                        }
                    }
                },
                Ok(None) => break,
                Err(_) => break,
            }
        }

        let stopping = stats.stop.load(Ordering::Relaxed);
        if stopping && !announced_stop {
            announced_stop = true;
            for (slot, c) in clients.iter_mut().enumerate() {
                if !matches!(c.stage, Stage::Done) {
                    host.peer_mut(peer_ids[slot]).disconnect(0);
                    c.stage = Stage::Done;
                    done_count += 1;
                }
            }
        }

        // 2. Per-client timers: handshake deadline + state reports.
        if !stopping {
            for (slot, c) in clients.iter_mut().enumerate() {
                match c.stage {
                    Stage::Connecting { deadline, .. } => {
                        if Instant::now() > deadline {
                            stats.dropped_connections.fetch_add(1, Ordering::Relaxed);
                            c.stage = Stage::Done;
                            done_count += 1;
                        }
                    }
                    Stage::Active => {
                        let peer = host.peer_mut(peer_ids[slot]);
                        c.maybe_report(peer, &stats);
                    }
                    Stage::Done => {}
                }
            }
        } else {
            // Give ENet a few rounds to flush the disconnects, then exit.
            for _ in 0..20 {
                let _ = host.service();
                std::thread::sleep(Duration::from_millis(1));
            }
            return;
        }

        if done_count >= clients.len() {
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}
