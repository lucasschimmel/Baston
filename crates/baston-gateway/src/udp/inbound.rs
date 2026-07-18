//! Inbound game-message dispatch: `GameServer::ProcessPacket` parity.

use baston_protocol::events;
use baston_protocol::native::NATIVE_RESULT_EVENT;
use baston_protocol::rage::object_ids::MSG_REQUEST_OBJECT_IDS;
use baston_protocol::rage::reliability::{self, GAME_STATE_ACK, GAME_STATE_NACK};
use baston_protocol::udp::state::{self as state_msg, MSG_BASTON_STATE, STATE_UPDATE_EVENT};
use baston_protocol::udp::{
    handshake, host, read_message_type, time_sync, MSG_CONNECT, MSG_I_HOST, MSG_I_QUIT, MSG_ROUTE,
    MSG_SERVER_EVENT, MSG_TIME_SYNC_REQ,
};
use rusty_enet as enet;
use std::time::Instant;

use super::handle::UdpCommand;
use super::server::{UdpServer, HOSTING_GRANT_TIMEOUT};

impl UdpServer {
    pub(super) async fn on_receive(&mut self, peer_id: enet::PeerID, channel: u8, data: &[u8]) {
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
}
