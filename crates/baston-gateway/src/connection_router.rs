//! source_id → zone_id routing table.
//!
//! The handoff path (jalon D4) takes a `tokio::sync::RwLock` write lock for
//! the atomic switch; reads on the UDP hot path are lock-free via `DashMap`.
//! Design: the routing table itself is a `DashMap` (single-key atomic ops are
//! already atomic), and `handoff_lock` serializes multi-step handoff commits
//! so ConfirmHandoff is one atomic transaction (< 1ms, no I/O while held).

use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::Mutex;

pub struct ConnectionRouter {
    routing_table: Arc<DashMap<u32, String>>,
    /// Serializes handoff commits (lock → update → release, never held across I/O).
    handoff_lock: Mutex<()>,
}

impl ConnectionRouter {
    pub fn new() -> Self {
        Self { routing_table: Arc::new(DashMap::new()), handoff_lock: Mutex::new(()) }
    }

    /// Assign a newly connected player to a zone.
    pub fn assign(&self, source: u32, zone_id: &str) {
        self.routing_table.insert(source, zone_id.to_owned());
    }

    /// Current zone of a player (UDP hot path — lock-free read).
    pub fn zone_of(&self, source: u32) -> Option<String> {
        self.routing_table.get(&source).map(|z| z.clone())
    }

    /// Atomic handoff commit. Returns the previous zone. The critical section
    /// is a single map insert — no await, no I/O (< 1ms rule).
    pub async fn commit_handoff(&self, source: u32, to_zone: &str) -> Option<String> {
        let _guard = self.handoff_lock.lock().await;
        let start = std::time::Instant::now();
        let previous = self.routing_table.insert(source, to_zone.to_owned());
        let held = start.elapsed();
        metrics::histogram!("handoff_routing_lock_held_us").record(held.as_micros() as f64);
        tracing::info!(target: "gateway",
            "routing_table updated: player={source} → {to_zone} (lock held {:.1}ms)",
            held.as_secs_f64() * 1000.0);
        previous
    }

    pub fn remove(&self, source: u32) -> Option<String> {
        self.routing_table.remove(&source).map(|(_, z)| z)
    }

    /// All players currently routed to `zone_id` (zone failure recovery, D6).
    pub fn players_in_zone(&self, zone_id: &str) -> Vec<u32> {
        self.routing_table
            .iter()
            .filter(|e| e.value() == zone_id)
            .map(|e| *e.key())
            .collect()
    }

    /// (source, zone) pairs for admin introspection.
    pub fn all(&self) -> Vec<(u32, String)> {
        self.routing_table.iter().map(|e| (*e.key(), e.value().clone())).collect()
    }

    pub fn count_in_zone(&self, zone_id: &str) -> usize {
        self.routing_table.iter().filter(|e| e.value() == zone_id).count()
    }
}

impl Default for ConnectionRouter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn assign_route_and_handoff() {
        let router = ConnectionRouter::new();
        router.assign(1, "zone-a");
        assert_eq!(router.zone_of(1).as_deref(), Some("zone-a"));

        let prev = router.commit_handoff(1, "zone-b").await;
        assert_eq!(prev.as_deref(), Some("zone-a"));
        assert_eq!(router.zone_of(1).as_deref(), Some("zone-b"));
    }

    #[tokio::test]
    async fn players_in_zone_filters() {
        let router = ConnectionRouter::new();
        router.assign(1, "zone-a");
        router.assign(2, "zone-b");
        router.assign(3, "zone-a");
        let mut in_a = router.players_in_zone("zone-a");
        in_a.sort();
        assert_eq!(in_a, vec![1, 3]);
        assert_eq!(router.count_in_zone("zone-b"), 1);
    }
}
