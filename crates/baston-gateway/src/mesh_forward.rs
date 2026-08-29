//! Gateway → Zone state forwarding (jalon D4).
//!
//! In mesh mode the zones run in their own processes; the Gateway terminates
//! the client UDP and forwards each validated `ClientStateUpdate` to the
//! player's *current* zone over NATS (`baston.zone.{zone_id}.ingest`). During
//! a handoff commit, updates for the player are held in a 50ms buffer and
//! flushed to the new zone — no packet is lost or applied out of order.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use baston_protocol::udp::state::ClientStateUpdate;
use tokio::sync::mpsc;

use crate::connection_router::ConnectionRouter;
use crate::zone_registry::ZoneRegistry;

/// How long updates are buffered around a handoff commit.
const HANDOFF_HOLD: Duration = Duration::from_millis(50);

/// Budget for the zone-side release RPC. Bounded so a wedged zone cannot stall
/// the forwarder task for every other player.
const RELEASE_TIMEOUT: Duration = Duration::from_secs(2);

/// Ceiling on forwarder shards. Beyond a handful the bottleneck is NATS, not
/// the number of publishing tasks.
const MAX_FORWARD_SHARDS: usize = 8;

/// Bounded so a slow NATS consumer can't let the forward queue grow without
/// limit. State updates are unreliable (superseded ~50ms later), so overflow
/// drops them rather than risking OOM.
const FORWARD_CAPACITY: usize = 8192;

pub fn ingest_subject(zone_id: &str) -> String {
    format!("baston.zone.{zone_id}.ingest")
}

pub fn outbound_subject_wildcard() -> &'static str {
    "baston.zone.*.outbound"
}

enum ForwardMsg {
    Update {
        source: u32,
        update: ClientStateUpdate,
    },
    BeginHold {
        source: u32,
    },
    FlushHold {
        source: u32,
    },
    /// The client disconnected: release it in its zone and forget it here.
    Dropped {
        source: u32,
    },
}

/// Cloneable handle used by the UDP server (sync context).
///
/// Work is sharded across several tasks keyed by player. Sharding on the
/// player is what makes it safe: every message about a given player — its
/// updates, its handoff hold, its release — lands on the same shard, so the
/// ordering the handoff hold depends on is preserved exactly, while unrelated
/// players stop queueing behind each other's NATS round trips.
#[derive(Clone)]
pub struct MeshForwarder {
    shards: Arc<Vec<mpsc::Sender<ForwardMsg>>>,
}

impl MeshForwarder {
    pub fn spawn(
        nats: async_nats::Client,
        router: Arc<ConnectionRouter>,
        registry: Arc<ZoneRegistry>,
    ) -> Self {
        let shard_count = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .clamp(1, MAX_FORWARD_SHARDS);
        let mut shards = Vec::with_capacity(shard_count);
        for _ in 0..shard_count {
            // Capacity is per shard, so total buffering scales with the shard
            // count rather than being split thinner as shards are added.
            let (tx, rx) = mpsc::channel(FORWARD_CAPACITY);
            tokio::spawn(run(
                nats.clone(),
                Arc::clone(&router),
                Arc::clone(&registry),
                rx,
                tx.clone(),
            ));
            shards.push(tx);
        }
        tracing::info!(target: "gateway", shards = shard_count, "mesh forwarder started");
        Self {
            shards: Arc::new(shards),
        }
    }

    /// The shard owning every message about `source`.
    fn shard(&self, source: u32) -> &mpsc::Sender<ForwardMsg> {
        &self.shards[source as usize % self.shards.len()]
    }

    /// Forward a client state update toward the player's current zone.
    /// Called from the sync UDP path; on overflow the update is dropped (it's
    /// unreliable and superseded shortly after).
    pub fn forward(&self, source: u32, update: ClientStateUpdate) {
        if self
            .shard(source)
            .try_send(ForwardMsg::Update { source, update })
            .is_err()
        {
            metrics::counter!("mesh_forward_dropped_total").increment(1);
        }
    }

    /// Called from the handoff-committed hook: buffer this player's updates
    /// for 50ms, then flush them to the (new) current zone.
    pub fn begin_handoff_hold(&self, source: u32) {
        self.send_control(source, ForwardMsg::BeginHold { source }, "handoff hold");
    }

    /// The client disconnected: tell its zone to release it, and drop the
    /// per-player bookkeeping this forwarder holds.
    ///
    /// Without this nothing ever tells a zone that a player is gone — the
    /// zone keeps its ped, its entities and its directory entry forever, and
    /// the leak compounds with every reconnect.
    pub fn player_dropped(&self, source: u32) {
        self.send_control(source, ForwardMsg::Dropped { source }, "player release");
    }

    /// Control messages change bookkeeping, so losing one corrupts state that
    /// nothing later repairs. Take the lock-free path when there is room, and
    /// fall back to an awaited send rather than dropping when the queue is
    /// full — late is recoverable, lost is not.
    fn send_control(&self, source: u32, msg: ForwardMsg, what: &'static str) {
        let shard = self.shard(source);
        let Err(mpsc::error::TrySendError::Full(msg)) = shard.try_send(msg) else {
            return;
        };
        metrics::counter!("mesh_forward_control_deferred_total").increment(1);
        tracing::warn!(target: "gateway", %what, "forward queue full: control message deferred");
        let shard = shard.clone();
        tokio::spawn(async move {
            let _ = shard.send(msg).await;
        });
    }
}

async fn run(
    nats: async_nats::Client,
    router: Arc<ConnectionRouter>,
    registry: Arc<ZoneRegistry>,
    mut rx: mpsc::Receiver<ForwardMsg>,
    tx: mpsc::Sender<ForwardMsg>,
) {
    // source → (hold started, buffered updates)
    let mut holds: HashMap<u32, (Instant, Vec<ClientStateUpdate>)> = HashMap::new();
    // Players whose first update was already seen (spawn re-homing done).
    let mut homed: std::collections::HashSet<u32> = std::collections::HashSet::new();

    while let Some(msg) = rx.recv().await {
        match msg {
            ForwardMsg::Update { source, update } => {
                // Connection-time routing has no coordinates (least-loaded
                // fallback). The FIRST positional update carries the spawn
                // point: re-home the player to the covering zone before any
                // zone ingests it — later boundary crossings go through the
                // real handoff protocol instead.
                if homed.insert(source) {
                    let (x, y) = (update.coords[0], update.coords[1]);
                    let current = router.zone_of(source);
                    if let Some(correct) = registry.find_zone_for_coords(x, y).await {
                        if current.as_deref() != Some(correct.as_str()) {
                            router.assign(source, &correct);
                            tracing::info!(target: "gateway",
                                "player={source} spawn=({x:.0}, {y:.0}) → re-homed to {correct}");
                        }
                    }
                }
                if let Some((started, buf)) = holds.get_mut(&source) {
                    if started.elapsed() < HANDOFF_HOLD {
                        buf.push(update);
                        continue;
                    }
                    // Hold expired but flush not processed yet: fall through
                    // after flushing in order.
                    let buffered = std::mem::take(buf);
                    holds.remove(&source);
                    for u in buffered {
                        publish(&nats, &router, source, u).await;
                    }
                }
                publish(&nats, &router, source, update).await;
            }
            ForwardMsg::BeginHold { source } => {
                holds.insert(source, (Instant::now(), Vec::new()));
                let tx = tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(HANDOFF_HOLD + Duration::from_millis(5)).await;
                    // Async context: await backpressure rather than drop, so a
                    // buffered player's held updates are never stranded.
                    let _ = tx.send(ForwardMsg::FlushHold { source }).await;
                });
            }
            ForwardMsg::FlushHold { source } => {
                if let Some((_, buffered)) = holds.remove(&source) {
                    metrics::histogram!("handoff_hold_buffered_updates")
                        .record(buffered.len() as f64);
                    for u in buffered {
                        publish(&nats, &router, source, u).await;
                    }
                }
            }
            ForwardMsg::Dropped { source } => {
                holds.remove(&source);
                homed.remove(&source);
                release_in_zone(&router, &registry, source).await;
            }
        }
    }
}

/// Tell the player's zone to release it, then forget the route.
///
/// Ordering matters: the route is what tells us which zone to call, so it is
/// removed only after the RPC has been addressed.
async fn release_in_zone(router: &ConnectionRouter, registry: &ZoneRegistry, source: u32) {
    let Some(zone) = router.zone_of(source) else {
        return; // never routed (or already released)
    };
    if let Some(mut client) = registry.zone_client(&zone).await {
        let request = baston_protocol::mesh::ReleasePlayerRequest {
            player_id: source,
            reason: "client disconnected".to_owned(),
        };
        match tokio::time::timeout(RELEASE_TIMEOUT, client.release_player(request)).await {
            Ok(Ok(_)) => {}
            Ok(Err(status)) => {
                tracing::warn!(target: "gateway", source, zone = %zone, error = %status,
                    "ReleasePlayer failed — the zone may retain stale player state");
                metrics::counter!("mesh_release_failures_total").increment(1);
            }
            Err(_) => {
                tracing::warn!(target: "gateway", source, zone = %zone,
                    "ReleasePlayer timed out — the zone may retain stale player state");
                metrics::counter!("mesh_release_failures_total").increment(1);
            }
        }
    }
    router.remove(source);
}

async fn publish(
    nats: &async_nats::Client,
    router: &ConnectionRouter,
    source: u32,
    update: ClientStateUpdate,
) {
    let Some(zone) = router.zone_of(source) else {
        tracing::debug!(target: "gateway", source, "state update dropped: player has no zone");
        return;
    };
    let payload =
        match bincode::serde::encode_to_vec(&(source, update), bincode::config::standard()) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(target: "gateway", error = %e, "state update encode failed");
                return;
            }
        };
    if let Err(e) = nats.publish(ingest_subject(&zone), payload.into()).await {
        tracing::error!(target: "gateway", error = %e, zone = %zone,
            "state forward to zone failed");
        metrics::counter!("mesh_forward_failures_total", "zone" => zone).increment(1);
    }
}
