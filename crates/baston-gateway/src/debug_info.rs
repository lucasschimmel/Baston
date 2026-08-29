//! The `displayinfo` feed: assembling one [`DebugInfoSnapshot`] per subscriber
//! and deciding who is allowed one.
//!
//! This runs inside the ENet task, which is the only place that holds all four
//! sources at once — the peers (network measurements), the OneSync game state
//! (entities, scope, object ids), the adaptive tick controller (server health)
//! and the player directory. Anywhere else would have to snapshot and ship
//! them, and the numbers would no longer be from the same instant.
//!
//! The mesh half is different: zone topology lives behind the gateway's async
//! registry, so it is read once per feed tick and shared by every subscriber
//! rather than re-read per player.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use baston_config::DebugConfig;
use baston_protocol::debug_info::{
    bearing_to, distance_to_bounds, MeshInfo, NeighbourZone, ZoneInfo,
};
use baston_protocol::Aabb;

use crate::connection_router::ConnectionRouter;
use crate::zone_registry::ZoneRegistry;

/// ENet reports smoothed loss as a fraction of this scale.
const PACKET_LOSS_SCALE: f32 = 65536.0;

/// Read-only view of the zone federation, handed to the feed by the
/// composition root. `None` on a single-process server: it has no topology to
/// report, which the snapshot expresses as an absent `mesh` rather than an
/// empty one.
#[derive(Clone)]
pub struct MeshView {
    pub registry: Arc<ZoneRegistry>,
    pub router: Arc<ConnectionRouter>,
    /// `[meshing].boundary_margin` — the band within which crossing a zone
    /// edge triggers handoff preparation.
    pub handoff_margin: f32,
}

/// Everything the ENet task needs to serve the overlay.
///
/// Bundled rather than passed as loose parameters: the zone registry only
/// exists once the composition root has built the federation, so this travels
/// with the spawn call that already carries the rest of the mesh wiring.
pub struct DebugFeedSetup {
    pub config: DebugConfig,
    /// Name shown in the overlay's header. The ENet task has no other source
    /// for it — the out-of-band query socket carries its own hostname.
    pub server_name: String,
    /// `None` on a single-process server.
    pub mesh: Option<MeshView>,
}

impl Default for DebugFeedSetup {
    /// A disabled feed, for the spawn entry points that predate meshing.
    fn default() -> Self {
        Self {
            config: DebugConfig::default(),
            server_name: "BASTON".to_owned(),
            mesh: None,
        }
    }
}

/// Per-subscriber bookkeeping the snapshot itself cannot carry.
struct Subscriber {
    /// When the last snapshot was built, for the bandwidth window.
    sampled_at: Instant,
    /// ENet's windowed byte counters at that moment.
    last_in_total: u32,
    last_out_total: u32,
    /// Whether a baseline has been taken. The counters start accumulating at
    /// connect, not at subscribe, so the first reading would otherwise divide
    /// the whole session's traffic by one tick.
    primed: bool,
}

impl Subscriber {
    fn new() -> Self {
        Self {
            sampled_at: Instant::now(),
            last_in_total: 0,
            last_out_total: 0,
            primed: false,
        }
    }

    /// Throughput in kbit/s since the previous sample.
    fn rates(&mut self, in_total: u32, out_total: u32) -> (f32, f32) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.sampled_at).as_secs_f32();
        let (in_delta, out_delta) = (
            window_delta(in_total, self.last_in_total),
            window_delta(out_total, self.last_out_total),
        );
        let primed = self.primed;
        self.primed = true;
        self.sampled_at = now;
        self.last_in_total = in_total;
        self.last_out_total = out_total;
        if !primed || elapsed <= 0.0 {
            return (0.0, 0.0);
        }
        let kbps = |bytes: u32| (bytes as f32 * 8.0) / elapsed / 1000.0;
        (kbps(in_delta), kbps(out_delta))
    }
}

/// Why a toggle request was refused. The client is told, so the overlay can
/// say what is wrong instead of drawing nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToggleOutcome {
    Subscribed,
    Unsubscribed,
    /// The feed is off server-wide.
    Disabled,
    /// The feed is on, but this player's identifiers are not cleared for it.
    NotAllowed,
}

impl ToggleOutcome {
    /// Operator-facing reason, sent to the requesting client.
    pub fn reason(self) -> &'static str {
        match self {
            Self::Subscribed | Self::Unsubscribed => "",
            Self::Disabled => "displayinfo is disabled on this server ([debug] display_info)",
            Self::NotAllowed => "your identifiers are not on the displayinfo allowlist",
        }
    }
}

/// Subscription state and cadence for the `displayinfo` overlay.
pub struct DebugInfoFeed {
    config: DebugConfig,
    period: Duration,
    subscribers: HashMap<u32, Subscriber>,
}

impl DebugInfoFeed {
    pub fn new(config: DebugConfig) -> Self {
        // Validated to 1..=30 at config load, so the division is safe; the
        // `max(1)` keeps a hand-built config out of a zero period.
        let period = Duration::from_secs_f64(1.0 / f64::from(config.refresh_hz.max(1)));
        Self {
            config,
            period,
            subscribers: HashMap::new(),
        }
    }

    pub fn enabled(&self) -> bool {
        self.config.display_info.is_enabled()
    }

    pub fn period(&self) -> Duration {
        self.period
    }

    pub fn is_subscribed(&self, source: u32) -> bool {
        self.subscribers.contains_key(&source)
    }

    pub fn subscriber_count(&self) -> usize {
        self.subscribers.len()
    }

    /// Sources currently receiving snapshots.
    pub fn subscribers(&self) -> Vec<u32> {
        self.subscribers.keys().copied().collect()
    }

    /// Apply a toggle request from `source`, whose identifiers are
    /// `identifiers`. Unsubscribing is always honoured — a player who has lost
    /// access must still be able to turn off an overlay they already have.
    pub fn toggle(&mut self, source: u32, on: bool, identifiers: &[String]) -> ToggleOutcome {
        if !on {
            self.subscribers.remove(&source);
            return ToggleOutcome::Unsubscribed;
        }
        if !self.config.display_info.is_enabled() {
            return ToggleOutcome::Disabled;
        }
        if !self.config.allows(identifiers) {
            return ToggleOutcome::NotAllowed;
        }
        self.subscribers
            .entry(source)
            .or_insert_with(Subscriber::new);
        ToggleOutcome::Subscribed
    }

    /// Forget a disconnected player.
    pub fn remove(&mut self, source: u32) {
        self.subscribers.remove(&source);
    }

    /// Compute this subscriber's bandwidth window. Returns `(in, out)` kbit/s,
    /// or `None` if the source is not subscribed.
    pub fn sample_rates(
        &mut self,
        source: u32,
        in_total: u32,
        out_total: u32,
    ) -> Option<(f32, f32)> {
        Some(
            self.subscribers
                .get_mut(&source)?
                .rates(in_total, out_total),
        )
    }
}

/// Bytes attributable to one sampling window.
///
/// ENet zeroes its per-peer byte counters every bandwidth-throttle interval, so
/// a value *below* the previous sample means the window rolled over: the
/// current total is then the whole delta that can be attributed. That
/// under-reports the bytes lost to the reset, which is the honest direction to
/// be wrong in for a diagnostic gauge.
fn window_delta(current: u32, previous: u32) -> u32 {
    if current >= previous {
        current - previous
    } else {
        current
    }
}

/// Loss percentage from ENet's fixed-point estimator.
pub fn loss_pct(packet_loss: u32) -> f32 {
    (packet_loss as f32 / PACKET_LOSS_SCALE) * 100.0
}

/// Seconds since the Unix epoch, or 0 if the clock is before it.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl MeshView {
    /// Snapshot the whole federation once, for every subscriber of this tick.
    ///
    /// Returns the registered zones alongside the margin, so the per-player
    /// step is pure arithmetic against an already-materialised list.
    pub async fn topology(&self) -> Vec<(String, ZoneInfo)> {
        self.registry
            .stats()
            .await
            .into_iter()
            .map(|zone| {
                (
                    zone.zone_id.clone(),
                    ZoneInfo {
                        id: zone.zone_id,
                        bounds: bounds_array(&zone.bounds),
                        players: zone.player_count,
                        entities: zone.entity_count,
                        max_players: zone.max_players,
                        heartbeat_age_ms: zone.heartbeat_age_ms,
                        status: zone.status.to_owned(),
                    },
                )
            })
            .collect()
    }

    /// Build one player's mesh view from a shared topology snapshot.
    ///
    /// `position` is `None` when the server has not decoded a position for the
    /// player yet: the zone assignment is still reported, but no distance is
    /// claimed, because every distance would be measured from the origin.
    pub fn player_mesh(
        &self,
        source: u32,
        position: Option<[f32; 3]>,
        topology: &[(String, ZoneInfo)],
    ) -> MeshInfo {
        let assigned = self.router.zone_of(source);
        let current = assigned
            .as_ref()
            .and_then(|id| topology.iter().find(|(zone_id, _)| zone_id == id))
            .map(|(_, zone)| zone.clone());

        let planar = position.map(|p| [p[0], p[1]]);
        let distance_to_edge = match (&current, planar) {
            (Some(zone), Some([x, y])) => {
                let bounds = Aabb::new(
                    zone.bounds[0],
                    zone.bounds[1],
                    zone.bounds[2],
                    zone.bounds[3],
                );
                Some(bounds.distance_to_edge(x, y))
            }
            _ => None,
        };

        let mut neighbours: Vec<NeighbourZone> = topology
            .iter()
            .filter(|(zone_id, _)| Some(zone_id) != assigned.as_ref())
            .map(|(_, zone)| {
                let (distance, direction) = match planar {
                    Some(from) => (
                        distance_to_bounds(from, zone.bounds),
                        bearing_to(from, zone.bounds),
                    ),
                    // No position: the zone is real and worth listing, but its
                    // distance is not knowable. f32::NAN would poison the JSON
                    // (it serialises as null), so an unreachable sentinel is
                    // used and the renderer prints "?" for it.
                    None => (UNKNOWN_DISTANCE, String::new()),
                };
                NeighbourZone {
                    zone: zone.clone(),
                    distance,
                    direction,
                    // Only a measured distance can arm a handoff. The sentinel
                    // is negative, so comparing it against the margin would
                    // report every zone as imminent.
                    armed: planar.is_some() && distance <= self.handoff_margin,
                }
            })
            .collect();
        neighbours.sort_by(|a, b| {
            a.distance
                .partial_cmp(&b.distance)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        MeshInfo {
            zones_online: topology.len() as u32,
            current,
            distance_to_edge,
            handoff_margin: self.handoff_margin,
            neighbours,
        }
    }
}

/// Sentinel distance for a zone whose distance cannot be measured, chosen so
/// no real world coordinate can produce it and `armed` is never set by it.
pub const UNKNOWN_DISTANCE: f32 = -1.0;

fn bounds_array(bounds: &Aabb) -> [f32; 4] {
    [bounds.x_min, bounds.y_min, bounds.x_max, bounds.y_max]
}

#[cfg(test)]
mod tests {
    use super::*;
    use baston_config::DisplayInfoAccess;

    fn feed(access: DisplayInfoAccess, allow: &[&str]) -> DebugInfoFeed {
        DebugInfoFeed::new(DebugConfig {
            display_info: access,
            allow: allow.iter().map(|s| (*s).to_owned()).collect(),
            refresh_hz: 5,
        })
    }

    #[test]
    fn a_disabled_feed_refuses_every_subscription() {
        let mut feed = feed(DisplayInfoAccess::Off, &[]);
        assert_eq!(
            feed.toggle(1, true, &["license:abc".to_owned()]),
            ToggleOutcome::Disabled
        );
        assert!(!feed.is_subscribed(1));
    }

    #[test]
    fn an_unlisted_player_is_told_why() {
        let mut feed = feed(DisplayInfoAccess::Allowlist, &["license:admin"]);
        let outcome = feed.toggle(1, true, &["license:someone-else".to_owned()]);
        assert_eq!(outcome, ToggleOutcome::NotAllowed);
        assert!(!outcome.reason().is_empty());
        assert!(!feed.is_subscribed(1));

        assert_eq!(
            feed.toggle(2, true, &["license:admin".to_owned()]),
            ToggleOutcome::Subscribed
        );
        assert!(feed.is_subscribed(2));
    }

    #[test]
    fn unsubscribing_works_even_without_access() {
        let mut open = feed(DisplayInfoAccess::Everyone, &[]);
        open.toggle(1, true, &[]);
        assert!(open.is_subscribed(1));

        // Access revoked mid-session (config reload, allowlist edit): the
        // player must still be able to turn the overlay off.
        let mut revoked = feed(DisplayInfoAccess::Off, &[]);
        revoked.subscribers.insert(1, Subscriber::new());
        assert_eq!(revoked.toggle(1, false, &[]), ToggleOutcome::Unsubscribed);
        assert!(!revoked.is_subscribed(1));
    }

    #[test]
    fn disconnecting_drops_the_subscription() {
        let mut feed = feed(DisplayInfoAccess::Everyone, &[]);
        feed.toggle(7, true, &[]);
        feed.remove(7);
        assert_eq!(feed.subscriber_count(), 0);
    }

    #[test]
    fn subscribing_twice_keeps_the_original_bandwidth_window() {
        let mut feed = feed(DisplayInfoAccess::Everyone, &[]);
        feed.toggle(1, true, &[]);
        feed.sample_rates(1, 1_000, 2_000);
        feed.toggle(1, true, &[]);
        // A re-subscribe that reset the window would treat the next sample's
        // whole accumulated total as one interval's traffic.
        let subscriber = feed.subscribers.get(&1).expect("still subscribed");
        assert_eq!(subscriber.last_in_total, 1_000);
        assert!(subscriber.primed);
    }

    #[test]
    fn the_first_sample_reports_nothing_rather_than_the_whole_session() {
        let mut subscriber = Subscriber::new();
        // The peer has been connected for minutes; these totals predate the
        // subscription entirely.
        assert_eq!(subscriber.rates(4_000_000, 4_000_000), (0.0, 0.0));
    }

    #[test]
    fn a_counter_reset_is_read_as_a_window_rollover() {
        assert_eq!(window_delta(1_500, 1_000), 500);
        // ENet zeroed the totals between samples: everything since the reset
        // is the delta, rather than an underflow or a silent zero.
        assert_eq!(window_delta(500, 10_000), 500);
        assert_eq!(window_delta(0, 10_000), 0);
    }

    async fn two_zone_mesh() -> MeshView {
        let registry = Arc::new(ZoneRegistry::new(Duration::from_secs(15)));
        registry
            .register_zone(
                "west",
                Aabb::new(-1000.0, -1000.0, 0.0, 1000.0),
                "127.0.0.1:1",
                64,
            )
            .await
            .expect("west registers");
        registry
            .register_zone(
                "east",
                Aabb::new(0.0, -1000.0, 1000.0, 1000.0),
                "127.0.0.1:2",
                64,
            )
            .await
            .expect("east registers");
        let router = Arc::new(ConnectionRouter::new());
        router.assign(1, "west");
        MeshView {
            registry,
            router,
            handoff_margin: 100.0,
        }
    }

    #[tokio::test]
    async fn the_neighbour_a_player_is_walking_into_is_armed() {
        let view = two_zone_mesh().await;
        let topology = view.topology().await;
        // 40m from the shared edge at x = 0, so inside the 100m margin.
        let mesh = view.player_mesh(1, Some([-40.0, 0.0, 30.0]), &topology);

        assert_eq!(mesh.zones_online, 2);
        assert_eq!(mesh.current.as_ref().expect("routed").id, "west");
        let edge = mesh.distance_to_edge.expect("a placed player has an edge");
        assert!(
            (edge - 40.0).abs() < 0.1,
            "expected 40m to the edge, got {edge}"
        );

        assert_eq!(mesh.neighbours.len(), 1);
        let east = &mesh.neighbours[0];
        assert_eq!(east.zone.id, "east");
        assert_eq!(east.direction, "E");
        assert!(east.armed, "40m from a shared edge is inside the margin");
    }

    #[tokio::test]
    async fn a_distant_neighbour_is_listed_but_not_armed() {
        let view = two_zone_mesh().await;
        let topology = view.topology().await;
        let mesh = view.player_mesh(1, Some([-800.0, 0.0, 30.0]), &topology);
        let east = &mesh.neighbours[0];
        assert!((east.distance - 800.0).abs() < 0.1, "got {}", east.distance);
        assert!(!east.armed);
    }

    #[tokio::test]
    async fn a_player_without_a_position_gets_topology_but_no_distances() {
        let view = two_zone_mesh().await;
        let topology = view.topology().await;
        let mesh = view.player_mesh(1, None, &topology);

        assert_eq!(mesh.current.as_ref().expect("routed").id, "west");
        assert!(mesh.distance_to_edge.is_none());
        let east = &mesh.neighbours[0];
        assert_eq!(east.distance, UNKNOWN_DISTANCE);
        assert!(
            !east.armed,
            "an unmeasurable distance must never read as an imminent handoff"
        );
        assert!(east.direction.is_empty());
    }

    #[tokio::test]
    async fn an_unrouted_player_still_sees_the_federation() {
        let view = two_zone_mesh().await;
        let topology = view.topology().await;
        // Source 9 was never assigned: the overlay must say so rather than
        // silently picking a zone from the coordinates.
        let mesh = view.player_mesh(9, Some([-40.0, 0.0, 30.0]), &topology);
        assert!(mesh.current.is_none());
        assert_eq!(
            mesh.neighbours.len(),
            2,
            "with no current zone, every zone is a neighbour"
        );
    }

    #[test]
    fn loss_is_a_percentage_of_enets_scale() {
        assert_eq!(loss_pct(0), 0.0);
        assert_eq!(loss_pct(65536), 100.0);
        assert!((loss_pct(6554) - 10.0).abs() < 0.1);
    }
}
