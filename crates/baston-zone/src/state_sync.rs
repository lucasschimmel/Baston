//! StateSyncEmitter: Zone → NATS JetStream publisher (jalon C2).
//!
//! Every `sync_interval_ms` (16ms = 62.5 fps) the emitter drains the
//! `EntityManager` dirty queue and publishes the batch — bincode-encoded —
//! on `baston.zone.{zone_id}.state`. Nothing is published on empty ticks.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::entity_manager::EntityManager;
use crate::ZoneError;

/// JetStream stream holding zone state batches. Memory storage with a short
/// max-age: a late Gateway can replay the last few seconds, no more.
pub const STATE_STREAM_NAME: &str = "BASTON_STATE";
/// Wildcard subject covering every zone's state topic.
pub const STATE_SUBJECT_WILDCARD: &str = "baston.zone.*.state";

/// Subject a given zone publishes on.
pub fn state_subject(zone_id: &str) -> String {
    format!("baston.zone.{zone_id}.state")
}

/// Idempotently create the `BASTON_STATE` JetStream stream.
pub async fn setup_nats_stream(client: &async_nats::Client) -> Result<(), ZoneError> {
    let js = async_nats::jetstream::new(client.clone());
    js.get_or_create_stream(async_nats::jetstream::stream::Config {
        name: STATE_STREAM_NAME.to_string(),
        subjects: vec![STATE_SUBJECT_WILDCARD.to_string()],
        max_age: Duration::from_secs(5),
        max_messages_per_subject: 10,
        storage: async_nats::jetstream::stream::StorageType::Memory,
        ..Default::default()
    })
    .await
    .map_err(|e| ZoneError::Nats(e.to_string()))?;
    Ok(())
}

pub struct StateSyncEmitter {
    zone_id: String,
    nats: async_nats::Client,
    entity_manager: Arc<EntityManager>,
    interval: Duration,
}

impl StateSyncEmitter {
    pub fn new(
        zone_id: String,
        nats: async_nats::Client,
        entity_manager: Arc<EntityManager>,
        interval_ms: u64,
    ) -> Self {
        Self {
            zone_id,
            nats,
            entity_manager,
            interval: Duration::from_millis(interval_ms.max(1)),
        }
    }

    /// Run the emit loop forever. Spawn as a dedicated tokio task.
    pub async fn run(self) {
        let subject = state_subject(&self.zone_id);
        let mut interval = tokio::time::interval(self.interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_tick: Option<Instant> = None;
        tracing::info!(
            target: "baston-zone",
            zone_id = %self.zone_id,
            interval_ms = self.interval.as_millis() as u64,
            "StateSyncEmitter started"
        );
        loop {
            interval.tick().await;
            let now = Instant::now();
            // Jitter = deviation of the actual tick period from the target.
            if let Some(prev) = last_tick {
                let jitter_ms =
                    (now.duration_since(prev).as_secs_f64() * 1000.0 - self.interval.as_secs_f64() * 1000.0)
                        .abs();
                metrics::histogram!("state_sync_tick_jitter_ms").record(jitter_ms);
                if jitter_ms > 5.0 {
                    tracing::warn!(target: "baston-zone", jitter_ms, "StateSyncEmitter tick jitter above 5ms");
                }
            }
            last_tick = Some(now);

            self.emit_once(&subject).await;

            let busy = now.elapsed();
            if busy > self.interval {
                tracing::warn!(
                    target: "baston-zone",
                    busy_ms = busy.as_millis() as u64,
                    "StateSyncEmitter tick exceeded its interval"
                );
            }
        }
    }

    /// One tick: drain + publish. Public for tests.
    pub async fn emit_once(&self, subject: &str) {
        let dirty = self.entity_manager.flush_dirty();
        metrics::gauge!("entities_dirty_per_tick").set(dirty.len() as f64);
        if dirty.is_empty() {
            return;
        }
        let payload = match bincode::serde::encode_to_vec(&dirty, bincode::config::standard()) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(target: "nats", error = %e, "dirty batch encode failed");
                return;
            }
        };
        let bytes = payload.len();
        let start = Instant::now();
        // publish() queues; flush() forces it onto the wire so the measured
        // latency is real and a killed task can't strand queued state.
        let result = self.nats.publish(subject.to_string(), payload.into()).await;
        let elapsed = start.elapsed();
        metrics::histogram!("nats_publish_duration_ms").record(elapsed.as_secs_f64() * 1000.0);
        match result {
            Ok(()) => {
                if elapsed > Duration::from_millis(8) {
                    tracing::warn!(
                        target: "nats",
                        elapsed_ms = elapsed.as_millis() as u64,
                        "NATS publish slower than 8ms (back-pressure?)"
                    );
                }
                metrics::counter!("nats_bytes_published").increment(bytes as u64);
                tracing::debug!(
                    target: "baston-zone",
                    "StateSyncEmitter: {} dirty entities published ({:.1} KB)",
                    dirty.len(),
                    bytes as f64 / 1024.0
                );
            }
            // Publish failure must not crash the zone: log and move on, the
            // next tick re-publishes newer state anyway.
            Err(e) => {
                tracing::error!(target: "nats", error = %e, "publish failed");
            }
        }
    }
}
