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

use baston_protocol::udp::{
    handshake, read_message_type, time_sync, MSG_CONNECT, MSG_I_QUIT, MSG_TIME_SYNC_REQ,
};
use baston_protocol::PlayerDirectory;
use baston_scripting::ScriptHost;
use rusty_enet as enet;
use tokio::sync::mpsc;

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
    host: enet::Host<UdpSocket>,
    players: Arc<PlayerDirectory>,
    script_host: ScriptHost,
    started_at: Instant,
    /// ENet peer → authenticated source.
    peer_sources: HashMap<enet::PeerID, u32>,
    /// Authenticated source → ENet peer.
    source_peers: HashMap<u32, enet::PeerID>,
}

/// Spawn the UDP/ENet server task. Returns a handle for outbound sends.
pub fn spawn(
    port: u16,
    poll_interval_ms: u64,
    max_players: u32,
    players: Arc<PlayerDirectory>,
    script_host: ScriptHost,
) -> Result<UdpHandle, UdpError> {
    let socket =
        UdpSocket::bind(("0.0.0.0", port)).map_err(|source| UdpError::Bind { port, source })?;
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
    };
    tokio::spawn(run(server, cmd_rx, poll_interval_ms));
    Ok(UdpHandle { cmd_tx })
}

async fn run(
    mut server: UdpServer,
    mut cmd_rx: mpsc::UnboundedReceiver<UdpCommand>,
    poll_interval_ms: u64,
) {
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(poll_interval_ms.max(1)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
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
        }
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
        let reply = handshake::build_connect_ok(source);
        if let Err(e) = peer.send(0, &enet::Packet::reliable(reply.as_slice())) {
            tracing::warn!(target: "udp", source, error = ?e, "connectOK send failed");
            return;
        }
        tracing::info!(
            target: "baston",
            "UDP connection established: source={source} addr={addr} latency={latency_ms}ms"
        );
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

    async fn on_disconnect(&mut self, peer_id: enet::PeerID) {
        let Some(source) = self.peer_sources.remove(&peer_id) else {
            return;
        };
        self.source_peers.remove(&source);
        let name = self
            .players
            .remove(source)
            .map(|p| p.name)
            .unwrap_or_default();
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
