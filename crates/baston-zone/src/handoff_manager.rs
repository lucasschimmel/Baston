//! Handoff orchestration, zone side (jalon D3/D4).
//!
//! Drives the lifecycle: boundary candidate → PrepareHandoff (Gateway) →
//! ready → crossing → ConfirmHandoff → ActivatePlayer → local release.

use std::sync::Arc;
use std::time::{Duration, Instant};

use baston_protocol::mesh::gateway_service_client::GatewayServiceClient;
use baston_protocol::mesh::{ConfirmHandoffRequest, PrepareHandoffRequest};
use baston_protocol::PlayerStateSnapshot;
use dashmap::DashMap;
use tonic::transport::Channel;

/// Gateway must answer a preparation request within this window, otherwise
/// the handoff is cancelled and the player stays in the current zone.
const PREPARE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub enum HandoffState {
    PreparationRequested {
        target_zone: String,
        requested_at: Instant,
    },
    ReadyToTransfer {
        target_zone: String,
        target_grpc: String,
    },
    Transferring,
}

pub struct HandoffManager {
    zone_id: String,
    gateway: GatewayServiceClient<Channel>,
    pending_handoffs: DashMap<u32, HandoffState>,
    /// Anti ping-pong: last completed/cancelled handoff per player.
    cooldowns: DashMap<u32, Instant>,
    cooldown: Duration,
}

impl HandoffManager {
    pub fn new(
        zone_id: String,
        gateway: GatewayServiceClient<Channel>,
        cooldown_secs: u64,
    ) -> Arc<Self> {
        Arc::new(Self {
            zone_id,
            gateway,
            pending_handoffs: DashMap::new(),
            cooldowns: DashMap::new(),
            cooldown: Duration::from_secs(cooldown_secs),
        })
    }

    pub fn state_of(&self, player_id: u32) -> Option<HandoffState> {
        self.pending_handoffs.get(&player_id).map(|s| s.clone())
    }

    fn in_cooldown(&self, player_id: u32) -> bool {
        self.cooldowns
            .get(&player_id)
            .map(|t| t.elapsed() < self.cooldown)
            .unwrap_or(false)
    }

    /// Ask the Gateway to prepare a handoff toward the zone covering
    /// `predicted` coords. No-op if one is already pending or in cooldown.
    /// Returns the target zone when the ghost is ready.
    pub async fn request_handoff(
        &self,
        player_id: u32,
        predicted: (f32, f32),
        snapshot: PlayerStateSnapshot,
    ) -> Option<String> {
        if self.pending_handoffs.contains_key(&player_id) || self.in_cooldown(player_id) {
            return None;
        }
        self.pending_handoffs.insert(
            player_id,
            HandoffState::PreparationRequested {
                target_zone: String::new(),
                requested_at: Instant::now(),
            },
        );
        tracing::info!(target: "zone", zone = %self.zone_id,
            "HandoffManager: preparing handoff for player={player_id} (predicted crossing at {:.0},{:.0})",
            predicted.0, predicted.1);

        let req = PrepareHandoffRequest {
            player_id,
            from_zone: self.zone_id.clone(),
            target_zone: String::new(), // Gateway resolves from predicted coords
            predicted_x: predicted.0,
            predicted_y: predicted.1,
            snapshot: snapshot.encode(),
        };
        let result =
            tokio::time::timeout(PREPARE_TIMEOUT, self.gateway.clone().prepare_handoff(req)).await;

        match result {
            Ok(Ok(resp)) => {
                let resp = resp.into_inner();
                if resp.ready {
                    tracing::info!(target: "zone", zone = %self.zone_id,
                        "HandoffManager: {} ready for player={player_id}", resp.target_zone);
                    self.pending_handoffs.insert(
                        player_id,
                        HandoffState::ReadyToTransfer {
                            target_zone: resp.target_zone.clone(),
                            target_grpc: resp.target_zone_grpc.clone(),
                        },
                    );
                    Some(resp.target_zone)
                } else {
                    tracing::warn!(target: "zone", zone = %self.zone_id,
                        "HandoffManager: prepare refused for player={player_id}: {}", resp.message);
                    self.cancel(player_id);
                    None
                }
            }
            Ok(Err(status)) => {
                tracing::error!(target: "zone", error = %status,
                    "HandoffManager: PrepareHandoff gRPC failed for player={player_id}");
                metrics::counter!("handoff_prepare_errors_total").increment(1);
                self.cancel(player_id);
                None
            }
            Err(_) => {
                tracing::error!(target: "zone",
                    "HandoffManager: PrepareHandoff timed out ({PREPARE_TIMEOUT:?}) for player={player_id}");
                metrics::counter!("handoff_prepare_timeouts_total").increment(1);
                self.cancel(player_id);
                None
            }
        }
    }

    /// Undo a committed crossing by re-pointing the gateway's routing table
    /// back at this zone.
    ///
    /// `ConfirmHandoff` moves the player's route before the target zone is
    /// asked to activate. If that activation then fails, the player is routed
    /// to a zone that never woke it up — its updates land nowhere and it is
    /// invisible to every script. Committing back to ourselves restores a
    /// consistent world: the player never left, and the mesh forwarder's held
    /// packets flush here.
    ///
    /// Returns `true` when the gateway accepted the rollback.
    pub async fn rollback_crossing(&self, player_id: u32) -> bool {
        let req = ConfirmHandoffRequest {
            player_id,
            from_zone: self.zone_id.clone(),
            to_zone: self.zone_id.clone(),
        };
        let accepted = matches!(
            tokio::time::timeout(PREPARE_TIMEOUT, self.gateway.clone().confirm_handoff(req)).await,
            Ok(Ok(resp)) if resp.get_ref().ok
        );
        if accepted {
            metrics::counter!("handoff_rollbacks_total").increment(1);
        } else {
            // Nothing else can save the player from here: say so loudly rather
            // than leaving a silent inconsistency.
            tracing::error!(
                target: "zone",
                zone = %self.zone_id,
                "handoff rollback REFUSED for player={player_id} — routing is now inconsistent"
            );
            metrics::counter!("handoff_rollback_failures_total").increment(1);
        }
        accepted
    }

    /// Drop bookkeeping for a player that left the mesh entirely.
    pub fn forget(&self, player_id: u32) {
        self.pending_handoffs.remove(&player_id);
        self.cooldowns.remove(&player_id);
    }

    /// Expire stale bookkeeping.
    ///
    /// Both maps are keyed by player and only ever cleared on the happy path,
    /// so a disconnect mid-handoff leaves an entry behind forever. Sweeping on
    /// the boundary scan keeps them bounded by the live population instead of
    /// by the session's total connection count.
    pub fn prune_expired(&self) {
        self.cooldowns.retain(|_, at| at.elapsed() < self.cooldown);
        self.pending_handoffs.retain(|_, state| match state {
            HandoffState::PreparationRequested { requested_at, .. } => {
                requested_at.elapsed() < PREPARE_TIMEOUT * 2
            }
            // Ready/Transferring are resolved by the crossing path itself;
            // they are short-lived and must not be swept from under it.
            _ => true,
        });
    }

    /// Cancel a pending handoff (player turned around, or prepare failed).
    /// Starts the cooldown so the same player doesn't immediately re-trigger.
    pub fn cancel(&self, player_id: u32) {
        if self.pending_handoffs.remove(&player_id).is_some() {
            self.cooldowns.insert(player_id, Instant::now());
        }
    }

    /// The player crossed the edge: atomically commit the reroute on the
    /// Gateway. Returns `(target_zone, target_grpc_addr)` on success. The
    /// caller then runs ActivatePlayer + local release (D4 sequence).
    pub async fn confirm_crossing(&self, player_id: u32) -> Option<(String, String)> {
        let (target_zone, target_grpc) = match self.state_of(player_id) {
            Some(HandoffState::ReadyToTransfer {
                target_zone,
                target_grpc,
            }) => (target_zone, target_grpc),
            _ => return None, // not prepared (or already transferring)
        };
        self.pending_handoffs
            .insert(player_id, HandoffState::Transferring);
        tracing::info!(target: "zone", zone = %self.zone_id,
            "HandoffManager: confirm handoff player={player_id} → {target_zone}");

        let req = ConfirmHandoffRequest {
            player_id,
            from_zone: self.zone_id.clone(),
            to_zone: target_zone.clone(),
        };
        match tokio::time::timeout(PREPARE_TIMEOUT, self.gateway.clone().confirm_handoff(req)).await
        {
            Ok(Ok(resp)) if resp.get_ref().ok => {
                self.pending_handoffs.remove(&player_id);
                self.cooldowns.insert(player_id, Instant::now());
                Some((target_zone, target_grpc))
            }
            Ok(Ok(resp)) => {
                tracing::error!(target: "zone",
                    "ConfirmHandoff refused for player={player_id}: {}", resp.get_ref().message);
                self.cancel(player_id);
                None
            }
            Ok(Err(status)) => {
                tracing::error!(target: "zone", error = %status,
                    "ConfirmHandoff gRPC failed for player={player_id}");
                metrics::counter!("handoff_confirm_errors_total").increment(1);
                self.cancel(player_id);
                None
            }
            Err(_) => {
                tracing::error!(target: "zone",
                    "ConfirmHandoff timed out for player={player_id}");
                self.cancel(player_id);
                None
            }
        }
    }
}
