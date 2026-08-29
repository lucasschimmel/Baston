//! Registry of live zone servers: bounds, gRPC callbacks, heartbeat liveness.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use baston_protocol::mesh::zone_service_client::ZoneServiceClient;
use baston_protocol::Aabb;
use tokio::sync::RwLock;
use tonic::transport::Channel;

use crate::quadtree::QuadTree;

/// One registered zone. The gRPC client is a persistent lazy channel — tonic
/// reconnects on demand, so a zone restart doesn't need re-registration logic
/// here (the zone re-registers itself anyway).
pub struct ZoneEntry {
    pub zone_id: String,
    pub bounds: Aabb,
    pub grpc_addr: String,
    pub grpc_client: ZoneServiceClient<Channel>,
    pub max_players: u32,
    pub player_count: Arc<AtomicU32>,
    pub entity_count: Arc<AtomicU32>,
    pub last_heartbeat: Instant,
    pub registered_at: Instant,
}

pub struct ZoneRegistry {
    zones: Arc<RwLock<HashMap<String, ZoneEntry>>>,
    quadtree: Arc<RwLock<QuadTree>>,
    /// Missed-heartbeat window before eviction (3 × 5s heartbeats).
    zone_timeout: Duration,
}

/// Snapshot of a zone for admin/introspection (no live handles).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ZoneStats {
    pub zone_id: String,
    pub bounds: Aabb,
    pub grpc_addr: String,
    pub max_players: u32,
    pub player_count: u32,
    pub entity_count: u32,
    pub heartbeat_age_ms: u64,
    pub status: &'static str,
}

impl ZoneRegistry {
    pub fn new(zone_timeout: Duration) -> Self {
        Self {
            zones: Arc::new(RwLock::new(HashMap::new())),
            quadtree: Arc::new(RwLock::new(QuadTree::gta_v())),
            zone_timeout,
        }
    }

    /// Register (or re-register) a zone: connect a lazy gRPC channel, add it
    /// to the registry, and index its bounds in the quadtree.
    pub async fn register_zone(
        &self,
        zone_id: &str,
        bounds: Aabb,
        grpc_addr: &str,
        max_players: u32,
    ) -> Result<(), String> {
        let endpoint = Channel::from_shared(normalize_grpc_uri(grpc_addr))
            .map_err(|e| format!("invalid zone gRPC addr {grpc_addr:?}: {e}"))?
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(5));
        // connect_lazy: the zone may register before its own server socket is
        // reachable from us; tonic dials on first call.
        let client = ZoneServiceClient::new(endpoint.connect_lazy());

        let now = Instant::now();
        let entry = ZoneEntry {
            zone_id: zone_id.to_owned(),
            bounds,
            grpc_addr: grpc_addr.to_owned(),
            grpc_client: client,
            max_players,
            player_count: Arc::new(AtomicU32::new(0)),
            entity_count: Arc::new(AtomicU32::new(0)),
            last_heartbeat: now,
            registered_at: now,
        };

        {
            let mut zones = self.zones.write().await;
            zones.insert(zone_id.to_owned(), entry);
        }
        {
            let mut qt = self.quadtree.write().await;
            qt.insert(zone_id, bounds);
        }
        tracing::info!(target: "gateway",
            "Zone {zone_id} registered: bounds=({},{},{},{}) grpc={grpc_addr}",
            bounds.x_min, bounds.y_min, bounds.x_max, bounds.y_max);
        Ok(())
    }

    /// Record a heartbeat. Returns false if the zone is unknown (evicted) —
    /// the zone must then re-register.
    pub async fn heartbeat(&self, zone_id: &str, player_count: u32, entity_count: u32) -> bool {
        let mut zones = self.zones.write().await;
        match zones.get_mut(zone_id) {
            Some(entry) => {
                entry.last_heartbeat = Instant::now();
                entry.player_count.store(player_count, Ordering::Relaxed);
                entry.entity_count.store(entity_count, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    /// Remove zones silent for longer than the timeout. Returns the evicted
    /// zone ids so the caller can trigger recovery (jalon D6).
    pub async fn evict_silent_zones(&self) -> Vec<String> {
        let now = Instant::now();
        let evicted: Vec<String> = {
            let mut zones = self.zones.write().await;
            let dead: Vec<String> = zones
                .iter()
                .filter(|(_, z)| now.duration_since(z.last_heartbeat) > self.zone_timeout)
                .map(|(id, _)| id.clone())
                .collect();
            for id in &dead {
                zones.remove(id);
            }
            dead
        };
        if !evicted.is_empty() {
            let mut qt = self.quadtree.write().await;
            for id in &evicted {
                qt.remove(id);
                tracing::warn!(target: "gateway", zone = %id,
                    "zone failed ({}s without heartbeat) — removed from registry",
                    self.zone_timeout.as_secs());
                metrics::counter!("zone_failures_total", "zone" => id.clone()).increment(1);
            }
        }
        evicted
    }

    /// Explicitly remove a zone (drain complete, admin action).
    pub async fn remove_zone(&self, zone_id: &str) -> bool {
        let removed = self.zones.write().await.remove(zone_id).is_some();
        if removed {
            self.quadtree.write().await.remove(zone_id);
        }
        removed
    }

    /// O(log n) quadtree lookup: which zone covers these coordinates?
    pub async fn find_zone_for_coords(&self, x: f32, y: f32) -> Option<String> {
        self.quadtree.read().await.query_point(x, y)
    }

    /// All zones intersecting a rectangle (cross-zone AoI, D5).
    pub async fn find_zones_in_aabb(&self, area: &Aabb) -> Vec<String> {
        self.quadtree.read().await.query_aabb(area)
    }

    /// Zone with the fewest active players (fallback routing / recovery).
    pub async fn find_least_loaded_zone(&self) -> Option<String> {
        self.find_least_loaded_zone_excluding(None).await
    }

    /// Least-loaded zone, optionally excluding one (drain / failure recovery).
    pub async fn find_least_loaded_zone_excluding(&self, exclude: Option<&str>) -> Option<String> {
        let zones = self.zones.read().await;
        zones
            .values()
            .filter(|z| Some(z.zone_id.as_str()) != exclude)
            .min_by_key(|z| z.player_count.load(Ordering::Relaxed))
            .map(|z| z.zone_id.clone())
    }

    /// Surviving zones and their capacity, for a one-shot rebalance.
    ///
    /// Recovery must not call [`Self::find_least_loaded_zone_excluding`] once
    /// per player: `player_count` only refreshes on the 5-second heartbeat, so
    /// every player in the burst would pick the same "least loaded" zone and
    /// pile onto it. Callers take this list once and balance against their own
    /// live tally instead.
    pub async fn survivors(&self, exclude: Option<&str>) -> Vec<(String, u32)> {
        let zones = self.zones.read().await;
        let mut survivors: Vec<(String, u32)> = zones
            .values()
            .filter(|zone| Some(zone.zone_id.as_str()) != exclude)
            .map(|zone| (zone.zone_id.clone(), zone.max_players))
            .collect();
        // Deterministic order so a rebalance is reproducible across runs.
        survivors.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        survivors
    }

    /// Clone of a zone's gRPC client (cheap: channels are ref-counted).
    pub async fn zone_client(&self, zone_id: &str) -> Option<ZoneServiceClient<Channel>> {
        self.zones
            .read()
            .await
            .get(zone_id)
            .map(|z| z.grpc_client.clone())
    }

    pub async fn zone_grpc_addr(&self, zone_id: &str) -> Option<String> {
        self.zones
            .read()
            .await
            .get(zone_id)
            .map(|z| z.grpc_addr.clone())
    }

    pub async fn zone_bounds(&self, zone_id: &str) -> Option<Aabb> {
        self.zones.read().await.get(zone_id).map(|z| z.bounds)
    }

    pub async fn contains(&self, zone_id: &str) -> bool {
        self.zones.read().await.contains_key(zone_id)
    }

    pub async fn stats(&self) -> Vec<ZoneStats> {
        let zones = self.zones.read().await;
        let now = Instant::now();
        zones
            .values()
            .map(|z| ZoneStats {
                zone_id: z.zone_id.clone(),
                bounds: z.bounds,
                grpc_addr: z.grpc_addr.clone(),
                max_players: z.max_players,
                player_count: z.player_count.load(Ordering::Relaxed),
                entity_count: z.entity_count.load(Ordering::Relaxed),
                heartbeat_age_ms: now.duration_since(z.last_heartbeat).as_millis() as u64,
                status: "active",
            })
            .collect()
    }

    /// Background task: scan for silent zones every 5s and invoke `on_failure`
    /// for each evicted zone (recovery is wired in jalon D6).
    pub fn spawn_liveness_monitor(
        self: &Arc<Self>,
        on_failure: Arc<dyn Fn(String) + Send + Sync>,
    ) -> tokio::task::JoinHandle<()> {
        let registry = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                for zone_id in registry.evict_silent_zones().await {
                    on_failure(zone_id);
                }
            }
        })
    }
}

/// tonic requires a scheme; zones register with `host:port`.
fn normalize_grpc_uri(addr: &str) -> String {
    if addr.starts_with("http://") || addr.starts_with("https://") {
        addr.to_owned()
    } else {
        format!("http://{addr}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> Arc<ZoneRegistry> {
        Arc::new(ZoneRegistry::new(Duration::from_millis(100)))
    }

    #[tokio::test]
    async fn register_appears_in_registry_and_routes() {
        let reg = registry();
        reg.register_zone(
            "zone-a",
            Aabb::new(-4000.0, -4000.0, 0.0, 4000.0),
            "127.0.0.1:50051",
            1500,
        )
        .await
        .unwrap();
        reg.register_zone(
            "zone-b",
            Aabb::new(0.0, -4000.0, 4000.0, 4000.0),
            "127.0.0.1:50052",
            1500,
        )
        .await
        .unwrap();
        assert!(reg.contains("zone-a").await);
        assert_eq!(
            reg.find_zone_for_coords(-500.0, 200.0).await.as_deref(),
            Some("zone-a")
        );
        assert_eq!(
            reg.find_zone_for_coords(1500.0, -300.0).await.as_deref(),
            Some("zone-b")
        );
    }

    #[tokio::test]
    async fn silent_zone_is_evicted() {
        let reg = registry();
        reg.register_zone(
            "zone-a",
            Aabb::new(-4000.0, -4000.0, 0.0, 4000.0),
            "127.0.0.1:50051",
            1500,
        )
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        let evicted = reg.evict_silent_zones().await;
        assert_eq!(evicted, vec!["zone-a".to_string()]);
        assert!(!reg.contains("zone-a").await);
        assert_eq!(reg.find_zone_for_coords(-500.0, 200.0).await, None);
        // Heartbeat from an evicted zone is refused → zone must re-register.
        assert!(!reg.heartbeat("zone-a", 0, 0).await);
    }

    #[tokio::test]
    async fn heartbeat_keeps_zone_alive_and_updates_load() {
        let reg = registry();
        reg.register_zone(
            "zone-a",
            Aabb::new(-4000.0, -4000.0, 0.0, 4000.0),
            "127.0.0.1:50051",
            1500,
        )
        .await
        .unwrap();
        reg.register_zone(
            "zone-b",
            Aabb::new(0.0, -4000.0, 4000.0, 4000.0),
            "127.0.0.1:50052",
            1500,
        )
        .await
        .unwrap();
        assert!(reg.heartbeat("zone-a", 40, 100).await);
        assert!(reg.heartbeat("zone-b", 10, 30).await);
        assert_eq!(
            reg.find_least_loaded_zone().await.as_deref(),
            Some("zone-b")
        );
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(reg.heartbeat("zone-a", 40, 100).await);
        tokio::time::sleep(Duration::from_millis(60)).await;
        // zone-a heartbeated 60ms ago (alive), zone-b 120ms ago (dead).
        let evicted = reg.evict_silent_zones().await;
        assert_eq!(evicted, vec!["zone-b".to_string()]);
    }
}
