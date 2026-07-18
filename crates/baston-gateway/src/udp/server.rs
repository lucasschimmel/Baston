//! ENet host task: spawn entry points, event pump, outbound path, and
//! session-host arbitration state.

use std::collections::HashMap;
use std::net::UdpSocket;
use std::sync::Arc;
use std::time::Instant;

use baston_config::OneSyncMode;
use baston_protocol::events;
use baston_protocol::udp::host;
use baston_protocol::PlayerDirectory;
use baston_scripting::{NetOutbound, ScriptHost};
use baston_zone::onesync::ServerGameState;
use baston_zone::StateIngest;
use rusty_enet as enet;
use tokio::sync::mpsc;

use super::handle::{UdpCommand, UdpError, UdpHandle, CMD_CAPACITY};
use super::oob::{OobInfo, OobSocket};

/// Channel count used by the FiveM client (`enet_host_create(..., 2, 0, 0)`).
const CHANNEL_COUNT: usize = 2;

pub(super) struct UdpServer {
    pub(super) host: enet::Host<OobSocket>,
    pub(super) players: Arc<PlayerDirectory>,
    pub(super) script_host: ScriptHost,
    pub(super) started_at: Instant,
    /// ENet peer → authenticated source.
    pub(super) peer_sources: HashMap<enet::PeerID, u32>,
    /// Authenticated source → ENet peer.
    pub(super) source_peers: HashMap<u32, enet::PeerID>,
    /// Non-OneSync session host: (netId, baseNum). The first client to send
    /// `msgIHost` becomes host (`IHostPacketHandler.h`).
    pub(super) session_host: Option<(u32, u32)>,
    /// Enhanced-host-support arbitration (sessionmanager parity): the client
    /// currently cleared to host, with its grant deadline. A client that
    /// fails a P2P join sends `hostingSession` and waits for a
    /// `sessionHostResult` client event (NetHook.cpp HS_START_HOSTING).
    pub(super) current_hosting: Option<(u32, Instant)>,
    /// Clients answered `wait` — notified `free` when the grant releases.
    pub(super) host_release_waiters: Vec<u32>,
    /// Phase C: validated client-state ingestion (None before wiring).
    pub(super) state_ingest: Option<Arc<StateIngest>>,
    /// Embedded Mumble voice server: torn down per player on disconnect.
    pub(super) voice: Option<baston_voice::server::VoiceHandle>,
    /// Voice endpoint replicated to clients (`voice_externalAddress`/`Port`).
    pub(super) voice_advertise: Option<(String, u16)>,
    /// Phase D: forward client state to the player's current zone process.
    pub(super) mesh_forward: Option<crate::mesh_forward::MeshForwarder>,
    /// OneSync-NG: server-authoritative entity game state. `Some` when
    /// `state_sync.onesync = "on"`; `None` keeps the msgRoute P2P relay.
    pub(super) onesync: Option<ServerGameState>,
    /// Interest-management tuning for the outbound tick.
    pub(super) onesync_cfg: baston_zone::interest_ng::InterestConfig,
    /// Best-effort per-client focus positions (from the state-report path),
    /// driving OneSync-NG interest management until sync-node position parsing
    /// lands.
    pub(super) focus_positions: HashMap<u32, [f32; 3]>,
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
        voice: None,
        voice_advertise: None,
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
pub(super) const HOSTING_GRANT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

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

    pub(super) fn handle_command(&mut self, cmd: UdpCommand) {
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
            UdpCommand::SetVoice { handle, advertise } => {
                self.voice = Some(handle);
                self.voice_advertise = advertise;
            }
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

    /// Send `onPlayerJoining`/`onPlayerDropped` (aboutNetId, name, slotId)
    /// to one client.
    pub(super) fn send_player_event(&mut self, event: &str, to: u32, about: u32, name: &str) {
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

    pub(super) fn expire_hosting_grant(&mut self) {
        if self
            .current_hosting
            .is_some_and(|(_, deadline)| Instant::now() >= deadline)
        {
            tracing::info!(target: "baston", "hostingSession grant expired");
            self.release_hosting_grant();
        }
    }

    pub(super) fn release_hosting_grant(&mut self) {
        self.current_hosting = None;
        let waiters = std::mem::take(&mut self.host_release_waiters);
        for waiter in waiters {
            self.send_session_host_result(waiter, "free");
        }
    }

    /// `sessionHostResult` client event — single msgpack string argument
    /// (NetHook.cpp hsInitFunction).
    pub(super) fn send_session_host_result(&mut self, to: u32, result: &str) {
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

    pub(super) fn broadcast_host(&mut self, net_id: u16, base_num: u32) {
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

    pub(super) async fn on_disconnect(&mut self, peer_id: enet::PeerID) {
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
        if let Some(voice) = &self.voice {
            voice.on_player_dropped(source);
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
