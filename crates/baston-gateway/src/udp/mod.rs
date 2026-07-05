//! Game-transport server: ENet over UDP (jalon B3).
//!
//! FiveM's game channel is standard ENet 1.3 with 2 channels
//! (`GameServerNet.ENet.cpp`). The ENet reliability layer (sequencing, ACKs,
//! retransmission, fragmentation, keep-alive) is provided by `rusty_enet`;
//! BASTON implements the FiveM message layer on top
//! (`GameServer::ProcessPacket`): `u32 LE` message type + payload.
//!
//! The ENet host is `!Sync` state-machine style, so it lives in one tokio
//! task; outbound traffic goes through an mpsc command channel.

use std::collections::HashMap;
use std::net::UdpSocket;
use std::sync::Arc;
use std::time::Instant;

use baston_config::OneSyncMode;
use baston_protocol::events;
use baston_protocol::native::NATIVE_RESULT_EVENT;
use baston_protocol::rage::object_ids::MSG_REQUEST_OBJECT_IDS;
use baston_protocol::rage::reliability::{self, GAME_STATE_ACK, GAME_STATE_NACK};
use baston_protocol::udp::state::{self as state_msg, MSG_BASTON_STATE, STATE_UPDATE_EVENT};
use baston_protocol::udp::{
    handshake, host, read_message_type, time_sync, MSG_CONNECT, MSG_I_HOST, MSG_I_QUIT, MSG_ROUTE,
    MSG_SERVER_EVENT, MSG_TIME_SYNC_REQ,
};
use baston_protocol::PlayerDirectory;
use baston_scripting::{NetOutbound, ScriptHost};
use baston_zone::onesync::ServerGameState;
use baston_zone::StateIngest;
use rusty_enet as enet;
use tokio::sync::mpsc;

mod oob;

pub use oob::{OobInfo, OobSocket};

/// Channel count used by the FiveM client (`enet_host_create(..., 2, 0, 0)`).
const CHANNEL_COUNT: usize = 2;

#[derive(Debug, thiserror::Error)]
pub enum UdpError {
    #[error("failed to bind UDP socket on port {port}: {source}")]
    Bind { port: u16, source: std::io::Error },
    #[error("failed to create ENet host: {0}")]
    HostCreate(String),
}

/// Commands other subsystems (native dispatch, client events) send to the
/// UDP task.
///
/// Bounded so a stalled ENet pump can't let queued commands (mostly outbound
/// snapshots) grow without limit. Overflow drops unreliable packets silently
/// and logs reliable/drop commands.
const CMD_CAPACITY: usize = 8192;

#[derive(Debug)]
pub enum UdpCommand {
    /// Send a raw message packet to a connected player.
    SendToSource {
        source: u32,
        channel: u8,
        data: Vec<u8>,
        reliable: bool,
    },
    /// Forcefully drop a player's game connection.
    DropSource { source: u32 },
}

/// Cloneable handle to the UDP task.
#[derive(Clone)]
pub struct UdpHandle {
    cmd_tx: mpsc::Sender<UdpCommand>,
}

impl UdpHandle {
    /// Handle wired to nothing — sends are dropped. For tests that need a
    /// `StateAggregator` without an ENet host.
    pub fn disconnected() -> (Self, mpsc::Receiver<UdpCommand>) {
        let (cmd_tx, cmd_rx) = mpsc::channel(CMD_CAPACITY);
        (Self { cmd_tx }, cmd_rx)
    }

    pub fn send_to_source(&self, source: u32, channel: u8, data: Vec<u8>, reliable: bool) {
        match self.cmd_tx.try_send(UdpCommand::SendToSource {
            source,
            channel,
            data,
            reliable,
        }) {
            Ok(()) => {}
            // Unreliable packets are safe to drop under overload; a dropped
            // reliable packet is a real problem, so surface it.
            Err(mpsc::error::TrySendError::Full(_)) => {
                if reliable {
                    tracing::warn!(target: "udp", source, "reliable send dropped: command queue full");
                }
                metrics::counter!("udp_cmd_dropped_total").increment(1);
            }
            // Server task gone (shutdown) — stay silent.
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }

    pub fn drop_source(&self, source: u32) {
        if self
            .cmd_tx
            .try_send(UdpCommand::DropSource { source })
            .is_err()
        {
            tracing::warn!(target: "udp", source, "drop command not delivered: queue full or closed");
        }
    }
}

struct UdpServer {
    host: enet::Host<OobSocket>,
    players: Arc<PlayerDirectory>,
    script_host: ScriptHost,
    started_at: Instant,
    /// ENet peer → authenticated source.
    peer_sources: HashMap<enet::PeerID, u32>,
    /// Authenticated source → ENet peer.
    source_peers: HashMap<u32, enet::PeerID>,
    /// Non-OneSync session host: (netId, baseNum). The first client to send
    /// `msgIHost` becomes host (`IHostPacketHandler.h`).
    session_host: Option<(u32, u32)>,
    /// Enhanced-host-support arbitration (sessionmanager parity): the client
    /// currently cleared to host, with its grant deadline. A client that
    /// fails a P2P join sends `hostingSession` and waits for a
    /// `sessionHostResult` client event (NetHook.cpp HS_START_HOSTING).
    current_hosting: Option<(u32, Instant)>,
    /// Clients answered `wait` — notified `free` when the grant releases.
    host_release_waiters: Vec<u32>,
    /// Phase C: validated client-state ingestion (None before wiring).
    state_ingest: Option<Arc<StateIngest>>,
    /// Phase D: forward client state to the player's current zone process.
    mesh_forward: Option<crate::mesh_forward::MeshForwarder>,
    /// OneSync-NG: server-authoritative entity game state. `Some` when
    /// `state_sync.onesync = "on"`; `None` keeps the msgRoute P2P relay.
    onesync: Option<ServerGameState>,
    /// Interest-management tuning for the outbound tick.
    onesync_cfg: baston_zone::interest_ng::InterestConfig,
    /// Best-effort per-client focus positions (from the state-report path),
    /// driving OneSync-NG interest management until sync-node position parsing
    /// lands.
    focus_positions: HashMap<u32, [f32; 3]>,
}

/// Spawn the UDP/ENet server task. Returns a handle for outbound sends.
pub fn spawn(
    port: u16,
    poll_interval_ms: u64,
    max_players: u32,
    players: Arc<PlayerDirectory>,
    script_host: ScriptHost,
) -> Result<UdpHandle, UdpError> {
    spawn_with_net(
        port,
        poll_interval_ms,
        max_players,
        players,
        script_host,
        None,
        None,
    )
}

/// Server hostname advertised in OOB `infoResponse` (overridable later via
/// config if needed).
fn oob_hostname() -> String {
    "BASTON".to_owned()
}

/// Spawn with the script-runtime net bridge receiver (client events +
/// native dispatch traffic).
pub fn spawn_with_net(
    port: u16,
    poll_interval_ms: u64,
    max_players: u32,
    players: Arc<PlayerDirectory>,
    script_host: ScriptHost,
    net_rx: Option<mpsc::Receiver<NetOutbound>>,
    state_ingest: Option<Arc<StateIngest>>,
) -> Result<UdpHandle, UdpError> {
    spawn_with_mesh(
        port,
        poll_interval_ms,
        max_players,
        players,
        script_host,
        net_rx,
        state_ingest,
        None,
        OneSyncMode::Off,
    )
}

/// Full-fat spawn: net bridge + local ingest + Phase D mesh forwarder.
#[allow(clippy::too_many_arguments)]
pub fn spawn_with_mesh(
    port: u16,
    poll_interval_ms: u64,
    max_players: u32,
    players: Arc<PlayerDirectory>,
    script_host: ScriptHost,
    net_rx: Option<mpsc::Receiver<NetOutbound>>,
    state_ingest: Option<Arc<StateIngest>>,
    mesh_forward: Option<crate::mesh_forward::MeshForwarder>,
    onesync_mode: OneSyncMode,
) -> Result<UdpHandle, UdpError> {
    let socket =
        UdpSocket::bind(("0.0.0.0", port)).map_err(|source| UdpError::Bind { port, source })?;
    let socket = OobSocket::new(
        socket,
        OobInfo {
            hostname: oob_hostname(),
            max_clients: max_players,
            players: Arc::clone(&players),
        },
    );
    let host = enet::Host::new(
        socket,
        enet::HostSettings {
            peer_limit: (max_players as usize) + 32,
            channel_limit: CHANNEL_COUNT,
            ..Default::default()
        },
    )
    .map_err(|e| UdpError::HostCreate(format!("{e:?}")))?;

    tracing::info!(target: "baston", port, "UDP game transport (ENet) listening");

    let (cmd_tx, cmd_rx) = mpsc::channel(CMD_CAPACITY);
    let server = UdpServer {
        host,
        players,
        script_host,
        started_at: Instant::now(),
        peer_sources: HashMap::new(),
        source_peers: HashMap::new(),
        session_host: None,
        current_hosting: None,
        host_release_waiters: Vec::new(),
        state_ingest,
        mesh_forward,
        // Big mode is implied by OneSync-on in BASTON (Infinity-style); the
        // length hack (Beyond, 16-bit ids) stays off until validated live.
        onesync: onesync_mode
            .is_enabled()
            .then(|| ServerGameState::new(true, false)),
        onesync_cfg: baston_zone::interest_ng::InterestConfig::default(),
        focus_positions: HashMap::new(),
    };
    if onesync_mode.is_enabled() {
        tracing::info!(target: "baston", "OneSync-NG enabled: server-authoritative entity parsing");
    }
    tokio::spawn(run(server, cmd_rx, net_rx, poll_interval_ms));
    Ok(UdpHandle { cmd_tx })
}

async fn run(
    mut server: UdpServer,
    mut cmd_rx: mpsc::Receiver<UdpCommand>,
    net_rx: Option<mpsc::Receiver<NetOutbound>>,
    poll_interval_ms: u64,
) {
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(poll_interval_ms.max(1)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // OneSync-NG outbound sync cadence (~20 Hz). Idle when OneSync is off.
    let mut sync_tick = tokio::time::interval(std::time::Duration::from_millis(50));
    sync_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // A closed/absent bridge must not wedge the select loop.
    let mut net_rx = net_rx;
    loop {
        tokio::select! {
            _ = tick.tick() => {
                server.pump().await;
            }
            _ = sync_tick.tick() => {
                server.onesync_tick();
            }
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(cmd) => {
                        server.handle_command(cmd);
                        // Batch-drain: at 2000 clients the aggregator emits
                        // tens of thousands of sends per second — one select
                        // iteration per command starves the ENet pump.
                        let mut drained = 0;
                        while let Ok(cmd) = cmd_rx.try_recv() {
                            server.handle_command(cmd);
                            drained += 1;
                            if drained >= 4096 { break; }
                        }
                    }
                    // All handles dropped: keep servicing ENet regardless.
                    None => server.pump().await,
                }
            }
            outbound = recv_net(&mut net_rx) => {
                if let Some(outbound) = outbound {
                    server.handle_net_outbound(outbound);
                }
            }
        }
    }
}

/// Await the net bridge, pending forever when absent or closed.
async fn recv_net(rx: &mut Option<mpsc::Receiver<NetOutbound>>) -> Option<NetOutbound> {
    match rx {
        Some(receiver) => match receiver.recv().await {
            Some(v) => Some(v),
            None => {
                *rx = None;
                std::future::pending().await
            }
        },
        None => std::future::pending().await,
    }
}

/// How long a `hostingSession` grant stays reserved before the arbiter gives
/// up on the client and frees the slot for waiters (sessionmanager: 5s).
const HOSTING_GRANT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

impl UdpServer {
    /// Drain all pending ENet events.
    async fn pump(&mut self) {
        self.expire_hosting_grant();
        loop {
            match self.host.service() {
                Ok(Some(event)) => {
                    // Split borrow: extract what we need, then drop the event.
                    match event {
                        enet::Event::Connect { peer, .. } => {
                            tracing::debug!(target: "udp", peer = ?peer.id(), addr = ?peer.address(), "ENet peer connected");
                        }
                        enet::Event::Disconnect { peer, .. } => {
                            let peer_id = peer.id();
                            self.on_disconnect(peer_id).await;
                        }
                        enet::Event::Receive {
                            peer,
                            channel_id,
                            packet,
                        } => {
                            let peer_id = peer.id();
                            let data = packet.data().to_vec();
                            self.on_receive(peer_id, channel_id, &data).await;
                        }
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::error!(target: "udp", error = %e, "ENet service error");
                    break;
                }
            }
        }
    }

    fn handle_command(&mut self, cmd: UdpCommand) {
        match cmd {
            UdpCommand::SendToSource {
                source,
                channel,
                data,
                reliable,
            } => {
                let Some(&peer_id) = self.source_peers.get(&source) else {
                    tracing::debug!(target: "udp", source, "send dropped: no game connection");
                    return;
                };
                let packet = if reliable {
                    enet::Packet::reliable(data.as_slice())
                } else {
                    enet::Packet::unreliable(data.as_slice())
                };
                if let Err(e) = self.host.peer_mut(peer_id).send(channel, &packet) {
                    tracing::warn!(target: "udp", source, error = ?e, "packet send failed");
                }
            }
            UdpCommand::DropSource { source } => {
                if let Some(peer_id) = self.source_peers.get(&source).copied() {
                    self.host.peer_mut(peer_id).disconnect(0);
                }
            }
        }
    }

    async fn on_receive(&mut self, peer_id: enet::PeerID, channel: u8, data: &[u8]) {
        let Some((msg_type, payload)) = read_message_type(data) else {
            return;
        };
        match msg_type {
            MSG_CONNECT => self.on_handshake(peer_id, payload),
            MSG_TIME_SYNC_REQ => self.on_time_sync(peer_id, payload),
            MSG_SERVER_EVENT => self.on_server_event(peer_id, payload).await,
            MSG_BASTON_STATE => self.on_state_update(peer_id, payload),
            MSG_ROUTE => self.on_route(peer_id, payload),
            MSG_REQUEST_OBJECT_IDS => self.on_request_object_ids(peer_id),
            GAME_STATE_NACK => self.on_game_state_nack(peer_id, payload),
            GAME_STATE_ACK => self.on_game_state_ack(peer_id, payload),
            MSG_I_HOST => self.on_i_host(peer_id, payload),
            MSG_I_QUIT => {
                self.host.peer_mut(peer_id).disconnect(0);
                self.on_disconnect(peer_id).await;
            }
            other => {
                tracing::debug!(
                    target: "udp",
                    msg_type = format!("0x{other:08x}"),
                    channel,
                    len = data.len(),
                    "unhandled game message"
                );
            }
        }
    }

    /// msgType 1: authenticate the ENet peer with the `initConnect` token
    /// (`GameServer::ProcessPacket`).
    fn on_handshake(&mut self, peer_id: enet::PeerID, payload: &[u8]) {
        let Some(connect) = handshake::parse_connect(payload) else {
            return;
        };
        // Binary-protocol clients (loadtest) declare themselves up front:
        // they sync via BASTON snapshots, so the O(n²) non-OneSync
        // onPlayerJoining mutual-knowledge broadcast is skipped for them.
        let is_baston_client = payload.windows(15).any(|w| w == b"bastonClient=1&")
            || payload.ends_with(b"bastonClient=1");
        let Some(source) = self.players.source_for_token(&connect.token) else {
            tracing::warn!(target: "udp", "handshake with unknown connection token; dropping peer");
            self.host.peer_mut(peer_id).disconnect(0);
            return;
        };

        self.peer_sources.insert(peer_id, source);
        self.source_peers.insert(source, peer_id);

        let peer = self.host.peer_mut(peer_id);
        let latency_ms = peer.round_trip_time().as_millis();
        let addr = peer
            .address()
            .map(|a| a.to_string())
            .unwrap_or_else(|| "unknown".into());
        // connectOK: netId = source (BASTON is not OneSync in Phase B).
        let reply = handshake::build_connect_ok(source, self.session_host);
        if let Err(e) = peer.send(0, &enet::Packet::reliable(reply.as_slice())) {
            tracing::warn!(target: "udp", source, error = ?e, "connectOK send failed");
            return;
        }
        tracing::info!(
            target: "baston",
            "UDP connection established: source={source} addr={addr} latency={latency_ms}ms"
        );

        if is_baston_client {
            if let Some(ingest) = &self.state_ingest {
                ingest.mark_snapshot_subscriber(source);
            }
        }
        self.send_post_connect(source, is_baston_client);
    }

    /// What FXServer sends/fires once the game connection is up
    /// (`ClientRegistry::HandleConnectedClient` + `ServerConsoleReplication.cpp`):
    /// replicated convars, the `onPlayerJoining` client event, and the
    /// server-side `playerJoining` event.
    fn send_post_connect(&mut self, source: u32, is_baston_client: bool) {
        let onesync_on = self.onesync.is_some();
        // Register the connection in the game state so it can lease object ids
        // and own entities.
        if let Some(gs) = self.onesync.as_mut() {
            gs.add_client(source);
        }

        // msgConVars: msgpack map of ConVar_Replicated variables.
        let onesync_value = if onesync_on { "on" } else { "off" };
        let convars =
            std::collections::BTreeMap::from([("onesync".to_owned(), onesync_value.to_owned())]);
        if let Ok(payload) = rmp_serde::to_vec(&convars) {
            let mut packet = baston_protocol::udp::hash_rage_string("msgConVars")
                .to_le_bytes()
                .to_vec();
            packet.extend_from_slice(&payload);
            self.handle_command(UdpCommand::SendToSource {
                source,
                channel: 0,
                data: packet,
                reliable: true,
            });
        }

        // onPlayerJoining (netId, name, slot) — FXServer non-bigmode
        // broadcasts the joiner to EVERY client and sends the joiner one
        // event per existing client (`ClientRegistry::HandleConnectedClient`).
        // That mutual knowledge is what triggers GTA-side player creation.
        //
        // slotId MUST be msgpack-unsigned: the client converts it with
        // `as<uint32_t>()` (HookPlayerNameHandling.cpp) and a negative int
        // throws msgpack::type_error → client crash. Non-OneSync never
        // assigns slots (`ClientRegistry.cpp` gates on IsOneSync), so every
        // slot is the unsigned representation of -1.
        // Binary BASTON clients sync via snapshots — the mutual-knowledge
        // broadcast is pure O(n²) noise for them (2000 joins ≈ 4M reliable
        // packets, enough to sink the connect phase of a mesh benchmark).
        if !is_baston_client {
            let name = self.players.get(source).map(|p| p.name).unwrap_or_default();
            let others: Vec<(u32, bool)> = self
                .peer_sources
                .values()
                .map(|&s| {
                    let sub = self
                        .state_ingest
                        .as_ref()
                        .is_some_and(|i| i.is_snapshot_subscriber(s));
                    (s, sub)
                })
                .collect();
            for (other, other_is_subscriber) in others {
                // The joiner about everyone (itself included, as FXServer does)…
                let other_name = self.players.get(other).map(|p| p.name).unwrap_or_default();
                self.send_player_event("onPlayerJoining", source, other, &other_name);
                // …and everyone else (real clients only) about the joiner.
                if other != source && !other_is_subscriber {
                    self.send_player_event("onPlayerJoining", other, source, &name);
                }
            }
        }

        // playerJoining(oldID) in the server runtimes, source bound. Detached:
        // handlers may await native round trips serviced by this task.
        let script_host = self.script_host.clone();
        tokio::spawn(async move {
            if let Err(e) = script_host
                .trigger_net_event(
                    "playerJoining",
                    source,
                    &serde_json::json!([source.to_string()]),
                )
                .await
            {
                tracing::error!(target: "udp", error = %e, "playerJoining dispatch failed");
            }
        });
    }

    /// `msgRoute` (non-OneSync): relay opaque GTA sync data to the target
    /// client, rewriting the leading netId to the sender's
    /// (`RoutingPacketHandler.h`). This is what makes clients see each
    /// other's peds — the GTA netcode does the entity sync itself.
    fn on_route(&mut self, peer_id: enet::PeerID, payload: &[u8]) {
        let Some(&source) = self.peer_sources.get(&peer_id) else {
            return;
        };
        let Some(route) = baston_protocol::udp::route::parse_client_route(payload) else {
            return;
        };

        // OneSync-NG: the server parses the clone stream authoritatively and
        // acks it, instead of relaying opaque sync blobs P2P
        // (RoutingPacketHandler.h: `if (IsOneSync()) ParseGameStatePacket`).
        if self.onesync.is_some() {
            self.on_clone_stream(source, route.data);
            return;
        }

        // Non-OneSync relay: forward opaque GTA sync data to the target,
        // rewriting the leading netId to the sender's.
        // Source can't be target.
        if u32::from(route.target_net_id) == source {
            return;
        }
        let Some(&target_peer) = self.source_peers.get(&(u32::from(route.target_net_id))) else {
            return;
        };
        let packet = baston_protocol::udp::route::build_server_route(source as u16, route.data);
        // Channel 1, unreliable — superseded ~33ms later by newer sync data.
        let peer = self.host.peer_mut(target_peer);
        if let Err(e) = peer.send(1, &enet::Packet::unreliable(packet.as_slice())) {
            tracing::debug!(target: "udp", error = ?e, "route relay failed");
        }
    }

    /// OneSync-NG outbound sync tick: advance the frame, recompute each
    /// client's interest set, and push the resulting `msgPackedClones`. Runs at
    /// the state-sync cadence (~20 Hz). No-op when OneSync is off.
    fn onesync_tick(&mut self) {
        if self.onesync.is_none() {
            return;
        }
        // Phase 1: build every client's packets while borrowing the game state.
        let cfg = self.onesync_cfg;
        let focus_positions = &self.focus_positions;
        let mut outbound: Vec<(u32, Vec<Vec<u8>>)> = Vec::new();
        {
            let gs = self.onesync.as_mut().expect("checked above");
            gs.update_player_positions(focus_positions);
            gs.tick();
            for source in gs.client_sources() {
                let focus = focus_positions.get(&source).copied().unwrap_or([0.0; 3]);
                let packets = gs.tick_client(source, focus, &cfg);
                if !packets.is_empty() {
                    outbound.push((source, packets));
                }
            }
        }
        // Phase 2: send (borrows self.host / command path).
        for (source, packets) in outbound {
            for data in packets {
                // Clone stream goes unreliable on channel 1; NAKs recover loss.
                self.handle_command(UdpCommand::SendToSource {
                    source,
                    channel: 1,
                    data,
                    reliable: false,
                });
            }
        }
    }

    /// OneSync-NG inbound: ingest the client's clone stream into the server
    /// game state and send back the resulting `msgPackedAcks`.
    fn on_clone_stream(&mut self, source: u32, data: &[u8]) {
        let Some(gs) = self.onesync.as_mut() else {
            return;
        };
        let outcome = gs.ingest_clone_payload(source, data);
        for packet in outcome.ack_packets {
            // Acks go reliable on channel 1 (the game-state channel).
            self.handle_command(UdpCommand::SendToSource {
                source,
                channel: 1,
                data: packet,
                reliable: true,
            });
        }
    }

    /// `gameStateNAck` (OneSync NAK mode): the client is missing frames or
    /// couldn't apply some entities. Roll its delta baselines back so the next
    /// tick re-sends what it missed (NG: no snapshot backlog, no drop).
    fn on_game_state_nack(&mut self, peer_id: enet::PeerID, payload: &[u8]) {
        let Some(&source) = self.peer_sources.get(&peer_id) else {
            return;
        };
        let Some(gs) = self.onesync.as_mut() else {
            return;
        };
        if let Some(nack) = reliability::parse_nack(payload) {
            gs.apply_nack(source, &nack);
        }
    }

    /// `gameStateAck` (OneSync ARQ mode): positive frame acknowledgement.
    fn on_game_state_ack(&mut self, peer_id: enet::PeerID, payload: &[u8]) {
        let Some(&source) = self.peer_sources.get(&peer_id) else {
            return;
        };
        let Some(gs) = self.onesync.as_mut() else {
            return;
        };
        if let Some(ack) = reliability::parse_ack(payload) {
            gs.apply_ack(source, &ack);
        }
    }

    /// `msgRequestObjectIds` (OneSync): lease a block of free object ids to the
    /// requesting client and reply with `msgObjectIds`
    /// (`RequestObjectIdsPacketHandler.cpp`). No-op when OneSync is off.
    fn on_request_object_ids(&mut self, peer_id: enet::PeerID) {
        let Some(&source) = self.peer_sources.get(&peer_id) else {
            return;
        };
        let Some(gs) = self.onesync.as_mut() else {
            return;
        };
        let n = gs.ids_per_request();
        let (_ids, packet) = gs.lease_object_ids(source, n);
        self.handle_command(UdpCommand::SendToSource {
            source,
            channel: 1,
            data: packet,
            reliable: true,
        });
    }

    /// `msgBastonState` (loadtest / binary path): validated state ingestion.
    fn on_state_update(&mut self, peer_id: enet::PeerID, payload: &[u8]) {
        let Some(&source) = self.peer_sources.get(&peer_id) else {
            return;
        };
        let Some(update) = state_msg::parse_state_update(payload) else {
            tracing::debug!(target: "udp", source, "malformed msgBastonState ignored");
            return;
        };
        // OneSync-NG interest management is position-driven; capture the
        // client's reported focus (hybrid feed until sync-node parsing lands).
        if self.onesync.is_some() {
            self.focus_positions.insert(source, update.coords);
        }
        // Clone the Arc so the mutable focus capture above doesn't clash with
        // the ingest borrow.
        let Some(ingest) = self.state_ingest.clone() else {
            tracing::debug!(target: "udp", source, "state update ignored: no ingest wired");
            return;
        };
        // The binary path identifies a BASTON-native client: it consumes
        // entity snapshots instead of msgRoute P2P sync.
        ingest.mark_snapshot_subscriber(source);
        // Phase D: the authoritative copy lives in the zone process — forward
        // over NATS (routing-table lookup + 50ms handoff hold). The local
        // apply below only feeds the gateway's AoI bookkeeping.
        if let Some(fwd) = &self.mesh_forward {
            fwd.forward(source, update.clone());
        }
        // Rejections are logged/counted inside StateIngest.
        let _ = ingest.apply(source, update);
    }

    /// Send `onPlayerJoining`/`onPlayerDropped` (aboutNetId, name, slotId)
    /// to one client.
    fn send_player_event(&mut self, event: &str, to: u32, about: u32, name: &str) {
        let args = serde_json::json!([about, name, u32::MAX]);
        if let Ok(msgpack) = events::json_args_to_msgpack(&args.to_string()) {
            let packet = events::build_net_event(event, &msgpack);
            self.handle_command(UdpCommand::SendToSource {
                source: to,
                channel: 0,
                data: packet,
                reliable: true,
            });
        }
    }

    fn on_time_sync(&mut self, peer_id: enet::PeerID, payload: &[u8]) {
        let Some(request) = time_sync::parse_time_sync_request(payload) else {
            return;
        };
        let server_time_ms = self.started_at.elapsed().as_millis() as u64;
        let reply = time_sync::build_time_sync_response(request, server_time_ms);
        // FXServer replies on channel 1 (TimeSyncReqPacketHandler).
        let peer = self.host.peer_mut(peer_id);
        if peer
            .send(1, &enet::Packet::reliable(reply.as_slice()))
            .is_ok()
        {
            if let Some(&source) = self.peer_sources.get(&peer_id) {
                let offset = server_time_ms as i64 - request.request_time as i64;
                tracing::info!(target: "baston", source, "clock sync: offset={offset:+}ms");
            }
        }
    }

    /// `msgIHost`: a client announces itself as session host (non-OneSync,
    /// `IHostPacketHandler.h`). First one wins; the choice is broadcast.
    fn on_i_host(&mut self, peer_id: enet::PeerID, payload: &[u8]) {
        let Some(&source) = self.peer_sources.get(&peer_id) else {
            return;
        };
        let Some(base_num) = host::parse_i_host(payload) else {
            return;
        };
        if self.session_host.is_some() {
            return;
        }
        self.session_host = Some((source, base_num));
        tracing::info!(target: "baston", source, base_num, "session host elected");
        self.broadcast_host(source as u16, base_num);
    }

    /// `hostingSession`: a client asks to become the GTA session host after
    /// a failed P2P join (NetHook.cpp HS_START_HOSTING). Reply mirrors the
    /// FXServer sessionmanager resource: `conflict` when a live host exists,
    /// `wait`/`free` when another grant is in flight, else `go`.
    fn on_hosting_session(&mut self, source: u32) {
        self.expire_hosting_grant();
        // A live established host that isn't the asker → conflict.
        if let Some((host, _)) = self.session_host {
            if host != source && self.source_peers.contains_key(&host) {
                tracing::info!(target: "baston", source, host, "hostingSession → conflict (live host)");
                self.send_session_host_result(source, "conflict");
                return;
            }
        }
        match self.current_hosting {
            Some((holder, _)) if holder == source => {
                self.send_session_host_result(source, "go");
            }
            Some((holder, _)) => {
                tracing::info!(target: "baston", source, holder, "hostingSession → wait");
                self.host_release_waiters.push(source);
                self.send_session_host_result(source, "wait");
            }
            None => {
                tracing::info!(target: "baston", source, "hostingSession → go");
                self.current_hosting = Some((source, Instant::now() + HOSTING_GRANT_TIMEOUT));
                self.send_session_host_result(source, "go");
            }
        }
    }

    /// `hostedSession`: the granted client finished hosting — release the
    /// grant and wake the waiters (they re-enter HS_LOADED and join it).
    fn on_hosted_session(&mut self, source: u32) {
        if self
            .current_hosting
            .is_some_and(|(holder, _)| holder == source)
        {
            self.release_hosting_grant();
        }
    }

    fn expire_hosting_grant(&mut self) {
        if self
            .current_hosting
            .is_some_and(|(_, deadline)| Instant::now() >= deadline)
        {
            tracing::info!(target: "baston", "hostingSession grant expired");
            self.release_hosting_grant();
        }
    }

    fn release_hosting_grant(&mut self) {
        self.current_hosting = None;
        let waiters = std::mem::take(&mut self.host_release_waiters);
        for waiter in waiters {
            self.send_session_host_result(waiter, "free");
        }
    }

    /// `sessionHostResult` client event — single msgpack string argument
    /// (NetHook.cpp hsInitFunction).
    fn send_session_host_result(&mut self, to: u32, result: &str) {
        let args = serde_json::json!([result]);
        if let Ok(msgpack) = events::json_args_to_msgpack(&args.to_string()) {
            let packet = events::build_net_event("sessionHostResult", &msgpack);
            self.handle_command(UdpCommand::SendToSource {
                source: to,
                channel: 0,
                data: packet,
                reliable: true,
            });
        }
    }

    fn broadcast_host(&mut self, net_id: u16, base_num: u32) {
        let packet = host::build_server_i_host(net_id, base_num);
        let peers: Vec<_> = self.peer_sources.keys().copied().collect();
        for peer_id in peers {
            let peer = self.host.peer_mut(peer_id);
            if let Err(e) = peer.send(0, &enet::Packet::reliable(packet.as_slice())) {
                tracing::warn!(target: "udp", error = ?e, "host broadcast failed");
            }
        }
    }

    /// Outbound client event from a script runtime → `msgNetEvent` packet.
    fn handle_net_outbound(&mut self, outbound: NetOutbound) {
        match outbound {
            NetOutbound::ClientEvent {
                source,
                event,
                args_json,
            } => {
                let msgpack = match events::json_args_to_msgpack(&args_json) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!(target: "udp", %event, error = %e, "client event args encode failed");
                        return;
                    }
                };
                let packet = events::build_net_event(&event, &msgpack);
                self.handle_command(UdpCommand::SendToSource {
                    source,
                    channel: 0,
                    data: packet,
                    reliable: true,
                });
            }
        }
    }

    /// `msgServerEvent` (TriggerServerEvent from the client): either a native
    /// dispatch result for the shim, or a regular net event for the runtimes.
    async fn on_server_event(&mut self, peer_id: enet::PeerID, payload: &[u8]) {
        let Some(&source) = self.peer_sources.get(&peer_id) else {
            tracing::debug!(target: "udp", "server event from unauthenticated peer ignored");
            return;
        };
        let Some(event) = events::parse_server_event(payload) else {
            return;
        };
        let args = match events::msgpack_to_json_args(event.msgpack_args) {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(target: "udp", event = %event.name, error = %e, "server event args decode failed");
                return;
            }
        };

        // Enhanced-host-support arbitration (sessionmanager parity) — handled
        // by the gateway itself, not the script runtimes.
        if event.name == "hostingSession" {
            self.on_hosting_session(source);
            return;
        }
        if event.name == "hostedSession" {
            self.on_hosted_session(source);
            return;
        }

        if event.name == NATIVE_RESULT_EVENT {
            // args: [id, result]
            let (Some(id), result) = (
                args.get(0).and_then(|v| v.as_u64()),
                args.get(1).cloned().unwrap_or(serde_json::Value::Null),
            ) else {
                return;
            };
            if !self.script_host.net().pending_natives.resolve(id, result) {
                tracing::debug!(target: "udp", id, "native result for unknown call (timed out?)");
            }
            return;
        }

        // Client shim reporting its ped state: same validated path as the
        // binary packet, no script-runtime dispatch.
        if event.name == STATE_UPDATE_EVENT {
            if let (Some(ingest), Some(update)) = (
                &self.state_ingest,
                state_msg::client_state_update_from_json(&args),
            ) {
                if let Some(fwd) = &self.mesh_forward {
                    fwd.forward(source, update.clone());
                }
                let _ = ingest.apply(source, update);
            }
            return;
        }

        // Dispatch detached: a handler may itself await a native-call round
        // trip that only THIS task can resolve — blocking here would deadlock.
        let script_host = self.script_host.clone();
        let name = event.name.clone();
        tokio::spawn(async move {
            if let Err(e) = script_host.trigger_net_event(&name, source, &args).await {
                tracing::error!(target: "udp", event = %name, error = %e, "net event dispatch failed");
            }
        });
    }

    async fn on_disconnect(&mut self, peer_id: enet::PeerID) {
        let Some(source) = self.peer_sources.remove(&peer_id) else {
            return;
        };
        self.source_peers.remove(&source);
        // Host left: clear and announce "no host" (GameServer.cpp drop path).
        if self.session_host.is_some_and(|(id, _)| id == source) {
            self.session_host = None;
            self.broadcast_host(0xFFFF, 0);
        }
        // Arbitration bookkeeping: a leaving grant-holder frees the slot;
        // a leaving waiter must not receive a dangling `free`.
        self.host_release_waiters.retain(|&w| w != source);
        if self
            .current_hosting
            .is_some_and(|(holder, _)| holder == source)
        {
            self.release_hosting_grant();
        }
        if let Some(ingest) = &self.state_ingest {
            ingest.on_player_dropped(source);
        }
        // OneSync-NG: release the client's object-id leases and orphan the
        // entities it owned (migration to a survivor is task 6/7).
        if let Some(gs) = self.onesync.as_mut() {
            let orphaned = gs.remove_client(source);
            if !orphaned.is_empty() {
                tracing::debug!(target: "udp", source, count = orphaned.len(), "orphaned entities on drop");
            }
        }
        let name = self
            .players
            .remove(source)
            .map(|p| p.name)
            .unwrap_or_default();
        // Tell remaining clients so GTA despawns the leaver's ped
        // (`GameServer.cpp` drop path).
        let remaining: Vec<u32> = self.peer_sources.values().copied().collect();
        for other in remaining {
            let leaver_name = name.clone();
            self.send_player_event("onPlayerDropped", other, source, &leaver_name);
        }
        tracing::info!(target: "baston", source, %name, "player dropped (game connection closed)");
        if let Err(e) = self
            .script_host
            .trigger_event("playerDropped", &[serde_json::json!("Disconnected.")])
            .await
        {
            tracing::error!(target: "udp", error = %e, "failed to fire playerDropped");
        }
    }
}
