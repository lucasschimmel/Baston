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

use baston_protocol::events;
use baston_protocol::native::NATIVE_RESULT_EVENT;
use baston_protocol::udp::state::{self as state_msg, MSG_BASTON_STATE, STATE_UPDATE_EVENT};
use baston_protocol::udp::{
    handshake, host, read_message_type, time_sync, MSG_CONNECT, MSG_I_HOST, MSG_I_QUIT,
    MSG_ROUTE, MSG_SERVER_EVENT, MSG_TIME_SYNC_REQ,
};
use baston_protocol::PlayerDirectory;
use baston_scripting::{NetOutbound, ScriptHost};
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
    cmd_tx: mpsc::UnboundedSender<UdpCommand>,
}

impl UdpHandle {
    /// Handle wired to nothing — sends are dropped. For tests that need a
    /// `StateAggregator` without an ENet host.
    pub fn disconnected() -> (Self, mpsc::UnboundedReceiver<UdpCommand>) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        (Self { cmd_tx }, cmd_rx)
    }

    pub fn send_to_source(&self, source: u32, channel: u8, data: Vec<u8>, reliable: bool) {
        let _ = self.cmd_tx.send(UdpCommand::SendToSource {
            source,
            channel,
            data,
            reliable,
        });
    }

    pub fn drop_source(&self, source: u32) {
        let _ = self.cmd_tx.send(UdpCommand::DropSource { source });
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
    /// Phase C: validated client-state ingestion (None before wiring).
    state_ingest: Option<Arc<StateIngest>>,
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
    net_rx: Option<mpsc::UnboundedReceiver<NetOutbound>>,
    state_ingest: Option<Arc<StateIngest>>,
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

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let server = UdpServer {
        host,
        players,
        script_host,
        started_at: Instant::now(),
        peer_sources: HashMap::new(),
        source_peers: HashMap::new(),
        session_host: None,
        state_ingest,
    };
    tokio::spawn(run(server, cmd_rx, net_rx, poll_interval_ms));
    Ok(UdpHandle { cmd_tx })
}

async fn run(
    mut server: UdpServer,
    mut cmd_rx: mpsc::UnboundedReceiver<UdpCommand>,
    net_rx: Option<mpsc::UnboundedReceiver<NetOutbound>>,
    poll_interval_ms: u64,
) {
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(poll_interval_ms.max(1)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // A closed/absent bridge must not wedge the select loop.
    let mut net_rx = net_rx;
    loop {
        tokio::select! {
            _ = tick.tick() => {
                server.pump().await;
            }
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(cmd) => server.handle_command(cmd),
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
async fn recv_net(rx: &mut Option<mpsc::UnboundedReceiver<NetOutbound>>) -> Option<NetOutbound> {
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

impl UdpServer {
    /// Drain all pending ENet events.
    async fn pump(&mut self) {
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

        self.send_post_connect(source);
    }

    /// What FXServer sends/fires once the game connection is up
    /// (`ClientRegistry::HandleConnectedClient` + `ServerConsoleReplication.cpp`):
    /// replicated convars, the `onPlayerJoining` client event, and the
    /// server-side `playerJoining` event.
    fn send_post_connect(&mut self, source: u32) {
        // msgConVars: msgpack map of ConVar_Replicated variables.
        let convars = std::collections::BTreeMap::from([("onesync".to_owned(), "off".to_owned())]);
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
        let name = self.players.get(source).map(|p| p.name).unwrap_or_default();
        let all_sources: Vec<u32> = self.peer_sources.values().copied().collect();
        for &other in &all_sources {
            // The joiner about everyone (itself included, as FXServer does)…
            let other_name = self.players.get(other).map(|p| p.name).unwrap_or_default();
            self.send_player_event("onPlayerJoining", source, other, &other_name);
            // …and everyone else about the joiner.
            if other != source {
                self.send_player_event("onPlayerJoining", other, source, &name);
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

    /// `msgBastonState` (loadtest / binary path): validated state ingestion.
    fn on_state_update(&mut self, peer_id: enet::PeerID, payload: &[u8]) {
        let Some(&source) = self.peer_sources.get(&peer_id) else {
            return;
        };
        let Some(ingest) = &self.state_ingest else {
            tracing::debug!(target: "udp", source, "state update ignored: no ingest wired");
            return;
        };
        let Some(update) = state_msg::parse_state_update(payload) else {
            tracing::debug!(target: "udp", source, "malformed msgBastonState ignored");
            return;
        };
        // The binary path identifies a BASTON-native client: it consumes
        // entity snapshots instead of msgRoute P2P sync.
        ingest.mark_snapshot_subscriber(source);
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
        if let Some(ingest) = &self.state_ingest {
            ingest.on_player_dropped(source);
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
