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

/// How long updates are buffered around a handoff commit.
const HANDOFF_HOLD: Duration = Duration::from_millis(50);

pub fn ingest_subject(zone_id: &str) -> String {
    format!("baston.zone.{zone_id}.ingest")
}

pub fn outbound_subject_wildcard() -> &'static str {
    "baston.zone.*.outbound"
}

enum ForwardMsg {
    Update { source: u32, update: ClientStateUpdate },
    BeginHold { source: u32 },
    FlushHold { source: u32 },
}

/// Cloneable handle used by the UDP server (sync context).
#[derive(Clone)]
pub struct MeshForwarder {
    tx: mpsc::UnboundedSender<ForwardMsg>,
}

impl MeshForwarder {
    pub fn spawn(nats: async_nats::Client, router: Arc<ConnectionRouter>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(run(nats, router, rx, tx.clone()));
        Self { tx }
    }

    /// Forward a client state update toward the player's current zone.
    pub fn forward(&self, source: u32, update: ClientStateUpdate) {
        let _ = self.tx.send(ForwardMsg::Update { source, update });
    }

    /// Called from the handoff-committed hook: buffer this player's updates
    /// for 50ms, then flush them to the (new) current zone.
    pub fn begin_handoff_hold(&self, source: u32) {
        let _ = self.tx.send(ForwardMsg::BeginHold { source });
    }
}

async fn run(
    nats: async_nats::Client,
    router: Arc<ConnectionRouter>,
    mut rx: mpsc::UnboundedReceiver<ForwardMsg>,
    tx: mpsc::UnboundedSender<ForwardMsg>,
) {
    // source → (hold started, buffered updates)
    let mut holds: HashMap<u32, (Instant, Vec<ClientStateUpdate>)> = HashMap::new();

    while let Some(msg) = rx.recv().await {
        match msg {
            ForwardMsg::Update { source, update } => {
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
                    let _ = tx.send(ForwardMsg::FlushHold { source });
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
        }
    }
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
    let payload = match bincode::serde::encode_to_vec(
        &(source, update),
        bincode::config::standard(),
    ) {
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
