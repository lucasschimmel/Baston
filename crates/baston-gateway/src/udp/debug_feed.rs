//! Building and dispatching `displayinfo` snapshots.
//!
//! Split out of [`super::server`] because it is a self-contained producer: it
//! only reads the ENet task's state and pushes one client event per
//! subscriber. Nothing here mutates the game state, so a bug in the overlay
//! cannot corrupt a live server — only misreport it.

use std::collections::HashMap;

use baston_protocol::debug_info::{
    DebugInfoSnapshot, NetInfo, OneSyncInfo, PlayerDebugInfo, ServerInfo, DEBUG_INFO_DENIED_EVENT,
    DEBUG_INFO_EVENT, DEBUG_INFO_VERSION,
};
use baston_protocol::events;
use baston_protocol::rage::clone::NetObjEntityType;
use rusty_enet as enet;

use crate::debug_info::{loss_pct, unix_now, ToggleOutcome};

use super::handle::UdpCommand;
use super::server::UdpServer;

/// What the ENet peer reports about one link, read in a single borrow so the
/// rest of the snapshot can be assembled without holding `&mut self.host`.
struct PeerSample {
    rtt_ms: f32,
    rtt_variance_ms: f32,
    loss_pct: f32,
    packets_sent: u32,
    packets_lost: u32,
    in_total: u32,
    out_total: u32,
    mtu: u16,
}

/// Per-source entity facts, gathered in one pass over the world rather than
/// one pass per subscriber.
#[derive(Default)]
struct EntityFacts {
    owned: u32,
    /// The player's own ped: position, velocity, sector, health, armour.
    ped: Option<PedFacts>,
}

struct PedFacts {
    object_id: u16,
    position: [f32; 3],
    velocity: [f32; 3],
    sector: [i32; 3],
    heading: Option<f32>,
    health: Option<f32>,
    armour: Option<f32>,
}

impl UdpServer {
    /// Handle a `baston:displayInfo:toggle` request from a client.
    ///
    /// The decision is the server's alone: the client asks, and is told what
    /// happened. A refusal carries its reason so the overlay can print it —
    /// an operator who mistyped an identifier should not have to read server
    /// logs to find that out.
    pub(super) fn on_debug_info_toggle(&mut self, source: u32, on: bool) {
        let identifiers = self
            .players
            .get(source)
            .map(|p| p.identifiers)
            .unwrap_or_default();
        let outcome = self.debug.toggle(source, on, &identifiers);
        match outcome {
            ToggleOutcome::Subscribed => {
                tracing::info!(target: "baston", source, "displayinfo subscribed");
            }
            ToggleOutcome::Unsubscribed => {}
            ToggleOutcome::Disabled | ToggleOutcome::NotAllowed => {
                tracing::warn!(
                    target: "baston",
                    source,
                    reason = outcome.reason(),
                    "displayinfo request refused"
                );
                self.send_debug_denied(source, outcome.reason());
            }
        }
    }

    fn send_debug_denied(&mut self, source: u32, reason: &str) {
        let args = serde_json::json!([reason]);
        let Ok(msgpack) = events::json_args_to_msgpack(&args.to_string()) else {
            return;
        };
        let packet = events::build_net_event(DEBUG_INFO_DENIED_EVENT, &msgpack);
        self.handle_command(UdpCommand::SendToSource {
            source,
            channel: 0,
            data: packet,
            reliable: true,
        });
    }

    /// Build and send one snapshot to every subscriber.
    ///
    /// Zone topology is read once and shared: it is identical for every
    /// subscriber, and re-reading the registry per player would take its lock
    /// once per overlay rather than once per tick.
    pub(super) async fn debug_tick(&mut self) {
        let subscribers = self.debug.subscribers();
        if subscribers.is_empty() {
            return;
        }

        let topology = match &self.mesh_view {
            Some(view) => view.topology().await,
            None => Vec::new(),
        };
        let server = self.debug_server_info();
        let (world_entities, server_owned, mut facts) = self.debug_entity_facts(&subscribers);

        let mut packets = Vec::with_capacity(subscribers.len());
        for source in subscribers {
            let Some(peer_id) = self.source_peers.get(&source).copied() else {
                // Subscribed, but no longer connected: the disconnect path
                // clears this, so reaching it means the peer vanished between
                // ticks. Drop the subscription rather than skipping forever.
                self.debug.remove(source);
                continue;
            };
            let sample = peer_sample(self.host.peer_mut(peer_id));
            let Some((bw_in_kbps, bw_out_kbps)) =
                self.debug
                    .sample_rates(source, sample.in_total, sample.out_total)
            else {
                continue;
            };

            let facts = facts.remove(&source).unwrap_or_default();
            let position = facts
                .ped
                .as_ref()
                .map(|ped| ped.position)
                .or_else(|| self.focus_positions.get(&source).copied());

            let snapshot = DebugInfoSnapshot {
                v: DEBUG_INFO_VERSION,
                server: server.clone(),
                net: NetInfo {
                    rtt_ms: sample.rtt_ms,
                    rtt_variance_ms: sample.rtt_variance_ms,
                    loss_pct: sample.loss_pct,
                    packets_sent: sample.packets_sent,
                    packets_lost: sample.packets_lost,
                    bw_in_kbps,
                    bw_out_kbps,
                    mtu: sample.mtu,
                },
                player: PlayerDebugInfo {
                    source,
                    name: self
                        .players
                        .get(source)
                        .map(|p| p.name)
                        .unwrap_or_else(|| format!("#{source}")),
                    position: position.unwrap_or([0.0; 3]),
                    velocity: facts.ped.as_ref().map(|p| p.velocity).unwrap_or([0.0; 3]),
                    sector: facts.ped.as_ref().map(|p| p.sector).unwrap_or([0; 3]),
                    heading: facts.ped.as_ref().and_then(|p| p.heading),
                    net_id: facts.ped.as_ref().map(|p| u32::from(p.object_id)),
                    health: facts.ped.as_ref().and_then(|p| p.health),
                    armour: facts.ped.as_ref().and_then(|p| p.armour),
                },
                onesync: self.debug_onesync_info(source, world_entities, server_owned, &facts),
                mesh: self
                    .mesh_view
                    .as_ref()
                    .map(|view| view.player_mesh(source, position, &topology)),
            };

            match serde_json::to_string(&serde_json::json!([snapshot])) {
                Ok(args) => match events::json_args_to_msgpack(&args) {
                    Ok(msgpack) => {
                        packets.push((source, events::build_net_event(DEBUG_INFO_EVENT, &msgpack)))
                    }
                    Err(e) => {
                        tracing::warn!(target: "udp", source, error = %e, "displayinfo encode failed")
                    }
                },
                Err(e) => {
                    tracing::warn!(target: "udp", source, error = %e, "displayinfo serialize failed")
                }
            }
        }

        for (source, data) in packets {
            self.handle_command(UdpCommand::SendToSource {
                source,
                channel: 0,
                data,
                // Reliable: a dropped snapshot leaves the overlay showing the
                // previous tick's numbers with no way to tell they are stale.
                reliable: true,
            });
        }
    }

    fn debug_server_info(&self) -> ServerInfo {
        let (tick_hz, tick_utilization) = match self.onesync.is_some() {
            true => (
                self.sync_controller.current_hz(),
                self.sync_controller.utilization().unwrap_or(0.0) as f32,
            ),
            // No OneSync means no outbound sync tick at all; reporting the
            // controller's configured rate would claim work that never runs.
            false => (0, 0.0),
        };
        ServerInfo {
            name: self.server_name.clone(),
            build: self.game_build.0,
            uptime_secs: self.started_at.elapsed().as_secs(),
            players: self.players.count() as u32,
            max_players: self.max_players,
            tick_hz,
            tick_utilization,
            tick_ms: self.last_sync_tick.as_secs_f32() * 1000.0,
            unix_time: unix_now(),
        }
    }

    /// One pass over the world: totals, plus each subscriber's owned count and
    /// own ped. The alternative — a scan per subscriber per field — is three
    /// full traversals of a world that can hold thousands of entities.
    fn debug_entity_facts(&self, subscribers: &[u32]) -> (u32, u32, HashMap<u32, EntityFacts>) {
        let mut facts: HashMap<u32, EntityFacts> = subscribers
            .iter()
            .map(|source| (*source, EntityFacts::default()))
            .collect();
        let Some(gs) = self.onesync.as_ref() else {
            return (0, 0, facts);
        };

        let mut total = 0;
        let mut server_owned = 0;
        for entity in gs.entities() {
            total += 1;
            if entity.server_owned {
                server_owned += 1;
            }
            let Some(entry) = facts.get_mut(&entity.owner) else {
                continue;
            };
            entry.owned += 1;
            if entity.entity_type == NetObjEntityType::Player && entity.position_known {
                entry.ped = Some(PedFacts {
                    object_id: entity.object_id,
                    position: entity.position,
                    velocity: entity.velocity,
                    sector: entity.sector,
                    heading: entity.heading,
                    health: entity.health,
                    armour: entity.armour,
                });
            }
        }
        (total, server_owned, facts)
    }

    fn debug_onesync_info(
        &self,
        source: u32,
        entities: u32,
        server_owned: u32,
        facts: &EntityFacts,
    ) -> Option<OneSyncInfo> {
        let gs = self.onesync.as_ref()?;
        let buckets = gs.routing_buckets();
        let routing_bucket = buckets.player_bucket(source);
        let policy = buckets.policy(routing_bucket);
        let usage = gs.object_id_usage();
        Some(OneSyncInfo {
            entities,
            in_scope: gs.client_scope_len(source).unwrap_or(0) as u32,
            owned: facts.owned,
            server_owned,
            frame_index: gs.frame_index(),
            client_frame_index: gs.client_frame_index(source).unwrap_or(0),
            routing_bucket,
            bucket_lockdown: format!("{:?}", policy.lockdown).to_lowercase(),
            bucket_population: policy.population_enabled,
            object_ids: usage,
        })
    }
}

fn peer_sample(peer: &mut enet::Peer<super::oob::OobSocket>) -> PeerSample {
    PeerSample {
        rtt_ms: peer.round_trip_time().as_secs_f32() * 1000.0,
        rtt_variance_ms: peer.round_trip_time_variance().as_secs_f32() * 1000.0,
        loss_pct: loss_pct(peer.packet_loss()),
        packets_sent: peer.packets_sent(),
        packets_lost: peer.packets_lost(),
        in_total: peer.incoming_data_total(),
        out_total: peer.outgoing_data_total(),
        mtu: peer.mtu(),
    }
}
