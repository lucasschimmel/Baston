//! ENet host task: spawn entry points, event pump, outbound path, and
//! session-host arbitration state.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::sync::Arc;
use std::time::Instant;

use crate::debug_info::{DebugFeedSetup, DebugInfoFeed, MeshView};
use baston_config::StateSyncConfig;
use baston_protocol::events;
use baston_protocol::rage::sync_parse::GameBuild;
use baston_protocol::udp::host;
use baston_protocol::PlayerDirectory;
use baston_scripting::{NetOutbound, RoutingLockdownMode, ScriptHost};
use baston_zone::adaptive_tick::{AdaptiveTickConfig, AdaptiveTickController};
use baston_zone::onesync::ServerGameState;
use baston_zone::routing_bucket::LockdownMode;
use baston_zone::StateIngest;
use rusty_enet as enet;
use tokio::sync::mpsc;

use super::handle::{UdpCommand, UdpError, UdpHandle, CONTROL_CAPACITY, SYNC_CAPACITY};
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
    /// Feedback controller governing the outbound OneSync cadence.
    pub(super) sync_controller: AdaptiveTickController,
    /// Last position each client reported out-of-band (mesh state path).
    ///
    /// OneSync-NG derives interest from decoded sync-tree positions; this map
    /// is only a bootstrap seed for a client whose first positional clone
    /// record has not arrived yet, and the mesh state pipeline's own feed.
    pub(super) focus_positions: HashMap<u32, [f32; 3]>,
    /// Last scripting routing revision mirrored into OneSync.
    pub(super) routing_revision: u64,
    /// Entity creations and deletions submitted by scripts, applied at the
    /// start of each sync tick so they land on a consistent world.
    pub(super) world_commands: Option<mpsc::Receiver<baston_scripting::WorldCommand>>,
    /// `displayinfo` subscriptions and cadence.
    pub(super) debug: DebugInfoFeed,
    /// Zone topology for the overlay's mesh section. `None` without meshing.
    pub(super) mesh_view: Option<MeshView>,
    /// Server name shown in the overlay.
    pub(super) server_name: String,
    pub(super) max_players: u32,
    /// The build every client is forced onto — reported so a sync-tree
    /// misread can be traced back to a build mismatch.
    pub(super) game_build: GameBuild,
    /// Wall time of the most recent OneSync tick, for the overlay's server
    /// health line.
    pub(super) last_sync_tick: std::time::Duration,
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
        StateSyncConfig::default(),
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
    state_sync: StateSyncConfig,
) -> Result<UdpHandle, UdpError> {
    spawn_with_mesh_on(
        IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        port,
        poll_interval_ms,
        max_players,
        players,
        script_host,
        net_rx,
        state_ingest,
        mesh_forward,
        state_sync,
        GameBuild::default(),
        DebugFeedSetup::default(),
    )
}

/// Full-fat spawn on one explicit local interface. Public CFX listing uses
/// this to leave `127.0.0.1:<port>` available to the genuine FXServer broker.
///
/// `game_build` must be the enforced `sv_enforceGameBuild`: several sync-tree
/// nodes changed width between builds, so decoding against the wrong one reads
/// real fields at the wrong offset.
#[allow(clippy::too_many_arguments)]
pub fn spawn_with_mesh_on(
    bind_address: IpAddr,
    port: u16,
    poll_interval_ms: u64,
    max_players: u32,
    players: Arc<PlayerDirectory>,
    script_host: ScriptHost,
    net_rx: Option<mpsc::Receiver<NetOutbound>>,
    state_ingest: Option<Arc<StateIngest>>,
    mesh_forward: Option<crate::mesh_forward::MeshForwarder>,
    state_sync: StateSyncConfig,
    game_build: GameBuild,
    debug: DebugFeedSetup,
) -> Result<UdpHandle, UdpError> {
    let onesync_mode = state_sync.onesync;
    let socket =
        UdpSocket::bind((bind_address, port)).map_err(|source| UdpError::Bind { port, source })?;
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

    tracing::info!(
        target: "baston",
        %bind_address,
        port,
        "UDP game transport (ENet) listening"
    );

    let (control_tx, control_rx) = mpsc::channel(CONTROL_CAPACITY);
    let (sync_tx, sync_rx) = mpsc::channel(SYNC_CAPACITY);
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
        // The decoder must use the build every client is forced onto, or the
        // build-gated node layouts are read at the wrong width.
        onesync: onesync_mode
            .is_enabled()
            .then(|| ServerGameState::with_build(true, false, game_build)),
        onesync_cfg: baston_zone::interest_ng::InterestConfig::from(&state_sync),
        sync_controller: AdaptiveTickController::new(AdaptiveTickConfig::from(&state_sync)),
        focus_positions: HashMap::new(),
        routing_revision: u64::MAX,
        world_commands: None,
        debug: DebugInfoFeed::new(debug.config),
        mesh_view: debug.mesh,
        server_name: debug.server_name,
        max_players,
        game_build,
        last_sync_tick: std::time::Duration::ZERO,
    };
    if onesync_mode.is_enabled() {
        tracing::info!(target: "baston", "OneSync-NG enabled: server-authoritative entity parsing");
    }
    tokio::spawn(run(server, control_rx, sync_rx, net_rx, poll_interval_ms));
    Ok(UdpHandle::new(control_tx, sync_tx))
}

async fn run(
    mut server: UdpServer,
    control_rx: mpsc::Receiver<UdpCommand>,
    sync_rx: mpsc::Receiver<UdpCommand>,
    net_rx: Option<mpsc::Receiver<NetOutbound>>,
    poll_interval_ms: u64,
) {
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(poll_interval_ms.max(1)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Start at the configured safe rate. Rate changes reset from "now" so a
    // downshift/upshift never produces a catch-up burst.
    let initial_period = server.sync_controller.period();
    let mut sync_tick =
        tokio::time::interval_at(tokio::time::Instant::now() + initial_period, initial_period);
    sync_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let onesync_enabled = server.onesync.is_some();
    // The overlay ticks on its own cadence: it must keep reporting when the
    // sync tick is the thing that stalled, which is exactly when an operator
    // is looking at it.
    let debug_enabled = server.debug.enabled();
    let debug_period = server.debug.period();
    let mut debug_tick =
        tokio::time::interval_at(tokio::time::Instant::now() + debug_period, debug_period);
    debug_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Closed/absent channels must not wedge the select loop.
    let mut control_rx = Some(control_rx);
    let mut sync_rx = Some(sync_rx);
    let mut net_rx = net_rx;
    loop {
        tokio::select! {
            biased;
            _ = tick.tick() => {
                server.pump().await;
            }
            cmd = recv_command(&mut control_rx) => {
                if let Some(cmd) = cmd {
                    server.handle_command(cmd);
                    // Bound each batch so ENet servicing remains prompt even
                    // under a sustained control burst.
                    if let Some(receiver) = control_rx.as_mut() {
                        for _ in 0..255 {
                            let Ok(cmd) = receiver.try_recv() else { break };
                            server.handle_command(cmd);
                        }
                    }
                }
            }
            outbound = recv_net(&mut net_rx) => {
                if let Some(outbound) = outbound {
                    server.handle_net_outbound(outbound);
                }
            }
            _ = sync_tick.tick(), if onesync_enabled => {
                let scheduled_period = server.sync_controller.period();
                let started = Instant::now();
                server.onesync_tick();
                let work = started.elapsed();
                server.last_sync_tick = work;
                let queue_pressure = sync_rx
                    .as_ref()
                    .map(|receiver| {
                        receiver.len() as f64 / receiver.max_capacity().max(1) as f64
                    })
                    .unwrap_or(0.0);
                let decision = server.sync_controller.observe(
                    work,
                    queue_pressure,
                    work >= scheduled_period,
                );
                metrics::gauge!("onesync_tick_hz").set(f64::from(decision.hz));
                metrics::gauge!("onesync_tick_utilization").set(decision.utilization);
                metrics::histogram!("onesync_tick_work_seconds").record(work.as_secs_f64());
                if work >= scheduled_period {
                    metrics::counter!("onesync_tick_overruns_total").increment(1);
                }
                if decision.changed {
                    metrics::counter!(
                        "onesync_tick_rate_transitions_total",
                        "reason" => format!("{:?}", decision.reason)
                    )
                    .increment(1);
                    tracing::info!(
                        target: "udp",
                        previous_hz = decision.previous_hz,
                        hz = decision.hz,
                        utilization = decision.utilization,
                        queue_pressure,
                        reason = ?decision.reason,
                        "adaptive OneSync tick rate changed"
                    );
                    let period = server.sync_controller.period();
                    sync_tick = tokio::time::interval_at(
                        tokio::time::Instant::now() + period,
                        period,
                    );
                    sync_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                }
            }
            _ = debug_tick.tick(), if debug_enabled => {
                server.debug_tick().await;
            }
            cmd = recv_command(&mut sync_rx) => {
                if let Some(cmd) = cmd {
                    server.handle_command(cmd);
                    // State frames are cheaper to drain in larger batches,
                    // but remain below control and the ENet pump in priority.
                    if let Some(receiver) = sync_rx.as_mut() {
                        for _ in 0..1023 {
                            let Ok(cmd) = receiver.try_recv() else { break };
                            server.handle_command(cmd);
                        }
                    }
                }
            }
        }
        metrics::gauge!("udp_plane_queue_depth", "plane" => "control")
            .set(control_rx.as_ref().map_or(0, mpsc::Receiver::len) as f64);
        metrics::gauge!("udp_plane_queue_depth", "plane" => "sync")
            .set(sync_rx.as_ref().map_or(0, mpsc::Receiver::len) as f64);
    }
}

/// Await a command queue, disabling its select branch once all producers have
/// gone away.
async fn recv_command(rx: &mut Option<mpsc::Receiver<UdpCommand>>) -> Option<UdpCommand> {
    match rx {
        Some(receiver) => match receiver.recv().await {
            Some(command) => Some(command),
            None => {
                *rx = None;
                std::future::pending().await
            }
        },
        None => std::future::pending().await,
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
    /// Mirror the scripting-native routing surface into the authoritative
    /// OneSync registry. The scripting store is process-wide and lock-free on
    /// reads; the game-state registry remains transport-independent.
    pub(super) fn refresh_routing_state(&mut self) {
        let routing = self.script_host.routing_control();
        let revision = routing.revision();
        if revision == self.routing_revision {
            return;
        }
        let Some(gs) = self.onesync.as_mut() else {
            return;
        };
        let sources = gs.client_sources();
        let entity_ids = gs.entity_ids();
        let mut buckets = std::collections::BTreeSet::from([0]);

        for source in sources {
            let bucket = routing.player_bucket(source);
            gs.set_player_routing_bucket(source, bucket);
            buckets.insert(bucket);
        }
        for object_id in entity_ids {
            let bucket = routing.entity_bucket(u32::from(object_id));
            gs.set_entity_routing_bucket(object_id, bucket);
            buckets.insert(bucket);
        }
        for bucket in buckets {
            let lockdown = match routing.lockdown_mode(bucket) {
                RoutingLockdownMode::Inactive => LockdownMode::Inactive,
                RoutingLockdownMode::Relaxed => LockdownMode::Relaxed,
                RoutingLockdownMode::Strict => LockdownMode::Strict,
            };
            gs.set_routing_bucket_lockdown(bucket, lockdown);
            gs.set_routing_bucket_population_enabled(bucket, routing.population_enabled(bucket));
        }
        self.routing_revision = revision;
    }

    /// Refresh only the sender and its policy on ingress. Performing a full
    /// entity scan per clone packet would let packet rate amplify O(world)
    /// control-plane work.
    pub(super) fn refresh_routing_source(&mut self, source: u32) {
        let Some(gs) = self.onesync.as_mut() else {
            return;
        };
        let routing = self.script_host.routing_control();
        let bucket = routing.player_bucket(source);
        gs.set_player_routing_bucket(source, bucket);
        let lockdown = match routing.lockdown_mode(bucket) {
            RoutingLockdownMode::Inactive => LockdownMode::Inactive,
            RoutingLockdownMode::Relaxed => LockdownMode::Relaxed,
            RoutingLockdownMode::Strict => LockdownMode::Strict,
        };
        gs.set_routing_bucket_lockdown(bucket, lockdown);
        gs.set_routing_bucket_population_enabled(bucket, routing.population_enabled(bucket));
    }

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

    /// Frame a client event and send it where its target says.
    ///
    /// The two outbound variants differ only in whether the payload was
    /// already msgpack; who receives it is the same question for both.
    pub(super) fn dispatch_client_event(
        &mut self,
        target: baston_scripting::EventTarget,
        event: &str,
        payload: &[u8],
    ) {
        let data = events::build_net_event(event, payload);
        let command = match target {
            baston_scripting::EventTarget::All => UdpCommand::Broadcast {
                channel: 0,
                data,
                reliable: true,
            },
            baston_scripting::EventTarget::One(source) => UdpCommand::SendToSource {
                source,
                channel: 0,
                data,
                reliable: true,
            },
        };
        self.handle_command(command);
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
            UdpCommand::Broadcast {
                channel,
                data,
                reliable,
            } => {
                let packet = if reliable {
                    enet::Packet::reliable(data.as_slice())
                } else {
                    enet::Packet::unreliable(data.as_slice())
                };
                // Collected first: `peer_mut` borrows the host mutably, and
                // the map cannot be iterated across that borrow.
                let peers: Vec<enet::PeerID> = self.source_peers.values().copied().collect();
                let mut failed = 0usize;
                for peer_id in peers {
                    if self.host.peer_mut(peer_id).send(channel, &packet).is_err() {
                        failed += 1;
                    }
                }
                if failed > 0 {
                    tracing::warn!(target: "udp", failed,
                        "broadcast did not reach every peer");
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
            UdpCommand::SetWorldCommands { rx } => {
                self.world_commands = Some(rx);
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
        self.refresh_routing_state();
        self.apply_world_commands();
        // Phase 1: build one immutable world snapshot, then plan disjoint
        // client views across Rayon workers.
        let cfg = self.onesync_cfg;
        let focus_positions = &self.focus_positions;
        let mut outbound: Vec<(u32, Vec<Vec<u8>>)> = Vec::new();
        {
            let gs = self.onesync.as_mut().expect("checked above");
            // Positions come from each entity's own sync tree. The reported
            // focus is only a seed for players the clone stream has not placed
            // yet, and never overwrites decoded state.
            gs.seed_unknown_player_positions(focus_positions);
            // Entities a script created have no simulator until a client
            // adopts them, and an unowned entity is not cloned to anyone.
            gs.reassign_ownerless_server_entities();
            gs.tick();
            for client_tick in gs.tick_clients(focus_positions, &cfg) {
                outbound.push((client_tick.source, client_tick.packets));
            }
        }
        self.publish_script_world();
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

    /// Apply the entity creations and deletions scripts asked for.
    ///
    /// They are drained at the start of the tick so a creation is authored,
    /// assigned an owner and cloned to clients within the same tick, rather
    /// than landing halfway through one.
    fn apply_world_commands(&mut self) {
        use baston_scripting::WorldCommand;

        let Some(rx) = self.world_commands.as_mut() else {
            return;
        };
        let mut pending = Vec::new();
        while let Ok(command) = rx.try_recv() {
            pending.push(command);
        }
        if pending.is_empty() {
            return;
        }
        let Some(gs) = self.onesync.as_mut() else {
            // OneSync off: the script's own synthetic store already answered.
            return;
        };
        for command in pending {
            match command {
                WorldCommand::Spawn {
                    network_id,
                    entity_type,
                    model,
                    position,
                    heading,
                    dynamic,
                } => {
                    let created = gs.spawn_server_entity_with_id(
                        network_id as u16,
                        crate::world_control::net_object_type(entity_type),
                        model,
                        position,
                        heading,
                        dynamic,
                    );
                    if !created {
                        tracing::warn!(
                            target: "onesync",
                            network_id,
                            "script entity creation refused: the id is already in use"
                        );
                        metrics::counter!("world_spawn_failures_total").increment(1);
                    }
                }
                WorldCommand::Despawn { network_id } => {
                    gs.despawn_entity(network_id as u16);
                }
            }
        }
    }

    /// Mirror the authoritative world into the script-visible view.
    ///
    /// Entity natives must not lock the game state — it lives on this task's
    /// hot path — so the world is published once per sync tick and scripts
    /// read it lock-free. One tick of staleness is the same freshness any
    /// server-side observation of a client-simulated entity would have.
    fn publish_script_world(&self) {
        use baston_protocol::rage::clone::NetObjEntityType;
        use baston_scripting::{EntitySummary, ScriptEntityType};

        let Some(gs) = self.onesync.as_ref() else {
            return;
        };
        let world = gs
            .entities()
            // An entity the clone stream has not placed yet has no position to
            // report, so it is not yet a thing scripts can reason about.
            .filter(|entity| entity.position_known)
            .map(|entity| EntitySummary {
                network_id: u32::from(entity.object_id),
                owner: entity.owner,
                entity_type: match entity.entity_type {
                    NetObjEntityType::Player | NetObjEntityType::Ped => ScriptEntityType::Ped,
                    ty if ty.is_vehicle() => ScriptEntityType::Vehicle,
                    _ => ScriptEntityType::Object,
                },
                net_type: entity.entity_type as u8,
                first_owner: entity.first_owner,
                position: entity.position,
                velocity: entity.velocity,
                routing_bucket: entity.routing_bucket,
                health: entity.health,
                max_health: entity.max_health,
                armour: entity.armour,
                model: entity.model,
                heading: entity.heading,
                desired_heading: entity.desired_heading,
                sync: entity.nodes,
            });
        self.script_host.entity_world().publish(world);
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
                target,
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
                self.dispatch_client_event(target, &event, &msgpack);
            }
            // Already msgpack: framed and sent as-is.
            NetOutbound::ClientEventRaw {
                target,
                event,
                payload,
            } => {
                self.dispatch_client_event(target, &event, &payload);
            }
        }
    }

    pub(super) async fn on_disconnect(&mut self, peer_id: enet::PeerID) {
        let Some(source) = self.peer_sources.remove(&peer_id) else {
            return;
        };
        let routing = self.script_host.routing_control();
        let source_bucket = routing.player_bucket(source);
        self.source_peers.remove(&source);
        self.focus_positions.remove(&source);
        self.debug.remove(source);
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
        // Mesh mode: the zone holding this player owns its ped, its entities
        // and its directory entry. Nothing else tells it the client is gone.
        if let Some(forwarder) = &self.mesh_forward {
            forwarder.player_dropped(source);
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
        let remaining: Vec<u32> = self
            .peer_sources
            .values()
            .copied()
            .filter(|other| routing.player_bucket(*other) == source_bucket)
            .collect();
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
        routing.set_player_bucket(source, 0);
    }
}
