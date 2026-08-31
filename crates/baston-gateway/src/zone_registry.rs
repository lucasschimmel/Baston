//! Registry of live zone servers: territory, gRPC callbacks, heartbeat liveness.
//!
//! Ownership is resolved through a [`ZoneMap`] — an ordered list of regions
//! where the first one containing a point wins. Two sources are possible:
//!
//! - a **configured** map, read from `meshing.map_file`, which the Gateway
//!   holds and hands to each zone at registration;
//! - a **declared** map, rebuilt from the bounds each zone announces about
//!   itself, which is what a deployment without a map file has always had.
//!
//! There is deliberately no spatial tree. Ordered regions have to be walked in
//! order, which is exactly what a tree cannot do without collecting every
//! candidate and re-sorting them — and at map sizes anyone writes by hand, a
//! bounding-box-filtered scan is the faster of the two anyway.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use baston_protocol::mesh::zone_service_client::ZoneServiceClient;
use baston_protocol::{Aabb, ZoneCoverage, ZoneMap};
use tokio::sync::RwLock;
use tonic::transport::Channel;

/// Bounds reported for a zone whose territory has no finite box — one that
/// owns the map's catch-all region. Display only: ownership always goes
/// through the coverage, never through this.
const WORLD_PLANE: Aabb = Aabb {
    x_min: -4000.0,
    y_min: -4000.0,
    x_max: 4000.0,
    y_max: 4000.0,
};

/// One registered zone. The gRPC client is a persistent lazy channel — tonic
/// reconnects on demand, so a zone restart doesn't need re-registration logic
/// here (the zone re-registers itself anyway).
pub struct ZoneEntry {
    pub zone_id: String,
    /// What this zone owns, and what has been carved out of it.
    pub coverage: ZoneCoverage,
    /// Bounds the zone declared for itself, if it declared any. Only
    /// meaningful without a configured map, where it is the whole territory.
    pub declared_bounds: Option<Aabb>,
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
    /// Static map from `meshing.map_file`. Never changes once loaded.
    configured_map: Option<Arc<ZoneMap>>,
    /// Rebuilt from declared bounds whenever the zone set changes. Unused
    /// when `configured_map` is set.
    declared_map: Arc<RwLock<ZoneMap>>,
    /// Missed-heartbeat window before eviction (3 × 5s heartbeats).
    zone_timeout: Duration,
}

/// Snapshot of a zone for admin/introspection (no live handles).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ZoneStats {
    pub zone_id: String,
    /// Box enclosing the zone's territory. A zone owning the catch-all region
    /// reports the world plane, since its territory has no finite box.
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
            configured_map: None,
            declared_map: Arc::new(RwLock::new(ZoneMap::default())),
            zone_timeout,
        }
    }

    /// Registry backed by a map file: zones are told what they own instead of
    /// announcing it, and a zone the map does not mention is refused.
    pub fn with_map(zone_timeout: Duration, map: ZoneMap) -> Self {
        Self {
            configured_map: Some(Arc::new(map)),
            ..Self::new(zone_timeout)
        }
    }

    pub fn configured_map(&self) -> Option<&ZoneMap> {
        self.configured_map.as_deref()
    }

    /// Register (or re-register) a zone: connect a lazy gRPC channel, work out
    /// what the zone owns, and index it.
    ///
    /// Returns the zone's coverage, which the caller sends back so the zone
    /// knows its own territory — including the higher-priority regions carved
    /// out of it, which it cannot derive on its own.
    pub async fn register_zone(
        &self,
        zone_id: &str,
        declared_bounds: Option<Aabb>,
        grpc_addr: &str,
        max_players: u32,
    ) -> Result<ZoneCoverage, String> {
        let coverage = match (&self.configured_map, declared_bounds) {
            (Some(map), _) => {
                let coverage = map.coverage_for(zone_id);
                if coverage.is_empty() {
                    return Err(format!(
                        "zone {zone_id:?} claims no region in the configured map; \
                         it lists: {}",
                        map.zone_ids().join(", ")
                    ));
                }
                coverage
            }
            (None, Some(bounds)) => ZoneCoverage::from_bounds(bounds),
            // Neither side knows what this zone owns. Saying so beats letting
            // it run owning nothing, which looks like a working server that
            // hands every player away.
            (None, None) => {
                return Err(format!(
                    "zone {zone_id:?} declared no bounds and this gateway has no \
                     map_file — set one or the other"
                ))
            }
        };

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
            coverage: coverage.clone(),
            declared_bounds,
            grpc_addr: grpc_addr.to_owned(),
            grpc_client: client,
            max_players,
            player_count: Arc::new(AtomicU32::new(0)),
            entity_count: Arc::new(AtomicU32::new(0)),
            last_heartbeat: now,
            registered_at: now,
        };

        self.zones.write().await.insert(zone_id.to_owned(), entry);
        self.rebuild_declared_map().await;

        match &self.configured_map {
            Some(_) => tracing::info!(target: "gateway",
                "Zone {zone_id} registered: {} region(s) from the map, {} overlay(s) \
                 carved out, grpc={grpc_addr}",
                coverage.shapes().len(), coverage.overlays().len()),
            None => tracing::info!(target: "gateway",
                "Zone {zone_id} registered: declared bounds, grpc={grpc_addr}"),
        }
        Ok(coverage)
    }

    /// Rebuild the declared-bounds index. No-op when a map is configured,
    /// where the map is the authority and declared bounds are ignored.
    ///
    /// Regions are ordered by zone id rather than by registration: with a
    /// correctly tiled map the order cannot matter, and if two zones do
    /// overlap, an order that survives a restart beats one that does not.
    async fn rebuild_declared_map(&self) {
        if self.configured_map.is_some() {
            return;
        }
        let mut declared: Vec<(String, Aabb)> = self
            .zones
            .read()
            .await
            .values()
            .filter_map(|z| z.declared_bounds.map(|b| (z.zone_id.clone(), b)))
            .collect();
        declared.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        *self.declared_map.write().await = ZoneMap::from_declared_bounds(declared);
    }

    /// Run `f` against whichever map is in force.
    async fn read_map<R>(&self, f: impl FnOnce(&ZoneMap) -> R) -> R {
        match &self.configured_map {
            Some(map) => f(map),
            None => f(&*self.declared_map.read().await),
        }
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
            for id in &evicted {
                tracing::warn!(target: "gateway", zone = %id,
                    "zone failed ({}s without heartbeat) — removed from registry",
                    self.zone_timeout.as_secs());
                metrics::counter!("zone_failures_total", "zone" => id.clone()).increment(1);
            }
            self.rebuild_declared_map().await;
        }
        evicted
    }

    /// Explicitly remove a zone (drain complete, admin action).
    pub async fn remove_zone(&self, zone_id: &str) -> bool {
        let removed = self.zones.write().await.remove(zone_id).is_some();
        if removed {
            self.rebuild_declared_map().await;
        }
        removed
    }

    /// Which zone owns these coordinates?
    ///
    /// A region whose zone is not currently registered is skipped, so ground
    /// belonging to a dead zone falls through to whatever is underneath it
    /// rather than routing players into a process that is not answering.
    pub async fn find_zone_for_coords(&self, x: f32, y: f32) -> Option<String> {
        let zones = self.zones.read().await;
        self.read_map(|map| {
            map.zone_at(x, y, |zone| zones.contains_key(zone))
                .map(str::to_owned)
        })
        .await
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

    /// Box enclosing a zone's territory — see [`ZoneStats::bounds`].
    pub async fn zone_bounds(&self, zone_id: &str) -> Option<Aabb> {
        self.zones
            .read()
            .await
            .get(zone_id)
            .map(|z| z.coverage.bbox().unwrap_or(WORLD_PLANE))
    }

    pub async fn zone_coverage(&self, zone_id: &str) -> Option<ZoneCoverage> {
        self.zones
            .read()
            .await
            .get(zone_id)
            .map(|z| z.coverage.clone())
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
                bounds: z.coverage.bbox().unwrap_or(WORLD_PLANE),
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

    const WEST: Aabb = Aabb {
        x_min: -4000.0,
        y_min: -4000.0,
        x_max: 0.0,
        y_max: 4000.0,
    };
    const EAST: Aabb = Aabb {
        x_min: 0.0,
        y_min: -4000.0,
        x_max: 4000.0,
        y_max: 4000.0,
    };

    fn registry() -> Arc<ZoneRegistry> {
        Arc::new(ZoneRegistry::new(Duration::from_millis(100)))
    }

    async fn register(reg: &ZoneRegistry, id: &str, bounds: Aabb, port: u16) -> ZoneCoverage {
        reg.register_zone(id, Some(bounds), &format!("127.0.0.1:{port}"), 1500)
            .await
            .unwrap()
    }

    const ARENA_MAP: &str = r#"
[[region]]
name = "arena"
zone = "zone-arena"
shape = "circle"
center = [0.0, 0.0]
radius = 100.0

[[region]]
name = "los-santos"
zone = "zone-city"
shape = "rect"
bounds = [-1000.0, -1000.0, 1000.0, 1000.0]

[[region]]
name = "everything-else"
zone = "zone-country"
shape = "everywhere"
"#;

    fn mapped_registry() -> Arc<ZoneRegistry> {
        let (map, warnings) = ZoneMap::parse(ARENA_MAP).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        Arc::new(ZoneRegistry::with_map(Duration::from_millis(100), map))
    }

    #[tokio::test]
    async fn register_appears_in_registry_and_routes() {
        let reg = registry();
        register(&reg, "zone-a", WEST, 50051).await;
        register(&reg, "zone-b", EAST, 50052).await;
        assert!(reg.contains("zone-a").await);
        assert_eq!(
            reg.find_zone_for_coords(-500.0, 200.0).await.as_deref(),
            Some("zone-a")
        );
        assert_eq!(
            reg.find_zone_for_coords(1500.0, -300.0).await.as_deref(),
            Some("zone-b")
        );
        // Off-map belongs to nobody when zones declare their own bounds.
        assert_eq!(reg.find_zone_for_coords(5000.0, 5000.0).await, None);
    }

    #[tokio::test]
    async fn silent_zone_is_evicted() {
        let reg = registry();
        register(&reg, "zone-a", WEST, 50051).await;
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
        register(&reg, "zone-a", WEST, 50051).await;
        register(&reg, "zone-b", EAST, 50052).await;
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

    #[tokio::test]
    async fn a_configured_map_overrules_the_bounds_a_zone_declares() {
        let reg = mapped_registry();
        // The zone claims the whole west half; the map gives it a 100m circle.
        let coverage = register(&reg, "zone-arena", WEST, 50051).await;
        register(&reg, "zone-city", WEST, 50052).await;
        register(&reg, "zone-country", WEST, 50053).await;

        assert!(coverage.contains(0.0, 0.0));
        assert!(!coverage.contains(-500.0, 0.0), "the declared bounds lost");
        assert_eq!(
            reg.find_zone_for_coords(0.0, 0.0).await.as_deref(),
            Some("zone-arena")
        );
        assert_eq!(
            reg.find_zone_for_coords(500.0, 500.0).await.as_deref(),
            Some("zone-city")
        );
        assert_eq!(
            reg.find_zone_for_coords(3000.0, 3000.0).await.as_deref(),
            Some("zone-country")
        );
    }

    /// The city has to be told what was carved out of it, or a player walking
    /// into the arena is never handed over.
    #[tokio::test]
    async fn a_zone_is_told_which_regions_outrank_it() {
        let reg = mapped_registry();
        let city = register(&reg, "zone-city", WEST, 50052).await;
        assert_eq!(city.overlays().len(), 1);
        assert!(city.contains(500.0, 500.0));
        assert!(!city.contains(0.0, 0.0), "the arena owns the middle");
    }

    /// Without a map, a zone that declares nothing is a zone nobody can place.
    /// Letting it run would look like a working server that hands every player
    /// straight back out.
    #[tokio::test]
    async fn no_bounds_and_no_map_is_refused_with_both_ways_out() {
        let reg = registry();
        let err = reg
            .register_zone("zone-a", None, "127.0.0.1:50051", 1500)
            .await
            .unwrap_err();
        assert!(err.contains("declared no bounds"), "{err}");
        assert!(err.contains("map_file"), "{err}");
        assert!(!reg.contains("zone-a").await);
    }

    /// With a map, declaring nothing is the normal case: the map decides.
    #[tokio::test]
    async fn a_mapped_gateway_needs_no_declared_bounds_at_all() {
        let reg = mapped_registry();
        let coverage = reg
            .register_zone("zone-arena", None, "127.0.0.1:50051", 1500)
            .await
            .expect("the map places it");
        assert!(coverage.contains(0.0, 0.0));
        assert_eq!(
            reg.find_zone_for_coords(0.0, 0.0).await.as_deref(),
            Some("zone-arena")
        );
    }

    #[tokio::test]
    async fn a_zone_the_map_does_not_mention_is_refused() {
        let reg = mapped_registry();
        let err = reg
            .register_zone("zone-nowhere", Some(WEST), "127.0.0.1:50051", 1500)
            .await
            .unwrap_err();
        assert!(err.contains("claims no region"), "{err}");
        assert!(err.contains("zone-arena"), "{err}");
        assert!(!reg.contains("zone-nowhere").await);
    }

    /// With the arena process down, the ground under it goes back to the city
    /// rather than becoming unroutable.
    #[tokio::test]
    async fn ground_under_a_dead_zone_falls_through_to_the_next_region() {
        let reg = mapped_registry();
        register(&reg, "zone-arena", WEST, 50051).await;
        register(&reg, "zone-city", WEST, 50052).await;
        assert_eq!(
            reg.find_zone_for_coords(0.0, 0.0).await.as_deref(),
            Some("zone-arena")
        );

        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(reg.evict_silent_zones().await.len(), 2);
        register(&reg, "zone-city", WEST, 50052).await;

        assert_eq!(
            reg.find_zone_for_coords(0.0, 0.0).await.as_deref(),
            Some("zone-city")
        );
    }

    #[tokio::test]
    async fn a_catch_all_zone_reports_the_world_plane_as_its_box() {
        let reg = mapped_registry();
        register(&reg, "zone-country", WEST, 50053).await;
        assert_eq!(reg.zone_bounds("zone-country").await, Some(WORLD_PLANE));
    }
}
