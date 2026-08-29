//! The `displayinfo` debug snapshot: what the server knows about one player,
//! assembled server-side and pushed to that player's overlay.
//!
//! Modelled on Star Citizen's `r_DisplayInfo`, which is the only shipping
//! reference for what a *meshed* server should show an operator: the zone
//! hierarchy the player currently sits in, the server's own tick health, and
//! the network link — rather than the client-side frame counters a game engine
//! usually surfaces.
//!
//! Every field here is read from an authoritative server-side structure. None
//! of it is a client-side native call reflected back, which is the whole point:
//! an overlay built from client natives can only ever show what the client
//! already believes, and the interesting failures are exactly the ones where
//! the client and the server disagree.
//!
//! The snapshot is JSON on the wire (msgpack-framed like any other client
//! event). It is versioned because the overlay ships inside the server binary
//! but is *executed* by a client that may have cached an older packfile.

use serde::{Deserialize, Serialize};

/// Server → client: one snapshot, sent at `[debug].refresh_hz`.
pub const DEBUG_INFO_EVENT: &str = "baston:displayInfo";

/// Client → server: subscribe / unsubscribe. Payload is a single boolean.
pub const DEBUG_INFO_TOGGLE_EVENT: &str = "baston:displayInfo:toggle";

/// Server → client: the subscription was refused (payload: reason string).
/// Sent so the overlay can say *why* nothing appears instead of silently
/// drawing an empty box.
pub const DEBUG_INFO_DENIED_EVENT: &str = "baston:displayInfo:denied";

/// Bumped whenever a field changes meaning. The renderer refuses to draw a
/// snapshot it does not understand rather than showing stale labels.
pub const DEBUG_INFO_VERSION: u32 = 1;

/// One player's view of the server, at one instant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DebugInfoSnapshot {
    /// Wire version — see [`DEBUG_INFO_VERSION`].
    pub v: u32,
    pub server: ServerInfo,
    pub net: NetInfo,
    pub player: PlayerDebugInfo,
    /// Absent when `state_sync.onesync` is off: there is no server-side entity
    /// state to report, and reporting zeroes would read as "no entities".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub onesync: Option<OneSyncInfo>,
    /// Absent when `[meshing]` is disabled — a single-process server has no
    /// zone topology, which is different from having an empty one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mesh: Option<MeshInfo>,
}

/// Process-wide health, identical for every subscriber.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    /// Enforced game build (`sv_enforceGameBuild`). The sync-tree layouts are
    /// gated on it, so a mismatch here explains otherwise inexplicable reads.
    pub build: u32,
    pub uptime_secs: u64,
    pub players: u32,
    pub max_players: u32,
    /// Current OneSync outbound cadence. The adaptive controller moves this
    /// under load, so a drop is the first visible sign of tick pressure.
    pub tick_hz: u16,
    /// Fraction of the scheduled period the last ticks actually consumed
    /// (EWMA). Above ~0.8 the controller starts shedding rate.
    pub tick_utilization: f32,
    /// Wall time of the most recent sync tick.
    pub tick_ms: f32,
    /// Unix seconds. Formatted by the renderer, so the server never has to
    /// carry a date-formatting dependency for a debug overlay.
    pub unix_time: u64,
}

/// The ENet link to this one player, as the server measures it.
///
/// These are the server's numbers, not the client's: an overlay that showed
/// the client's own ping would agree with itself even when the server sees
/// something different, which is precisely the case worth catching.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetInfo {
    pub rtt_ms: f32,
    pub rtt_variance_ms: f32,
    /// Smoothed loss as a percentage, from ENet's own estimator.
    pub loss_pct: f32,
    pub packets_sent: u32,
    pub packets_lost: u32,
    /// Throughput measured over the interval between two snapshots, in kbit/s.
    pub bw_in_kbps: f32,
    pub bw_out_kbps: f32,
    pub mtu: u16,
}

/// Where the server thinks this player is.
///
/// Named apart from [`crate::PlayerInfo`], which is the directory entry: this
/// is the positional reading, and the two are produced side by side.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlayerDebugInfo {
    pub source: u32,
    pub name: String,
    pub position: [f32; 3],
    pub velocity: [f32; 3],
    /// Sector indices the position was reassembled from. A player whose
    /// coordinates look right but whose sector does not is a decode bug.
    pub sector: [i32; 3],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heading: Option<f32>,
    /// Network id of the player's ped, once the clone stream has placed it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub net_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub armour: Option<f32>,
}

/// Server-authoritative entity state, from this player's point of view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OneSyncInfo {
    /// Entities the server tracks, across every bucket.
    pub entities: u32,
    /// Entities currently cloned to this player.
    pub in_scope: u32,
    /// Entities this player simulates.
    pub owned: u32,
    /// Entities the server itself authored (script-created).
    pub server_owned: u32,
    /// Server frame index — the delta baseline every client acks against.
    pub frame_index: u64,
    /// The frame index this client last reported. A widening gap against
    /// `frame_index` is a client falling behind the clone stream.
    pub client_frame_index: u64,
    pub routing_bucket: u32,
    /// `inactive` / `relaxed` / `strict`.
    pub bucket_lockdown: String,
    pub bucket_population: bool,
    pub object_ids: ObjectIdUsage,
}

/// Object-id pool pressure. Exhaustion is a hard failure — clients simply stop
/// being able to create entities — and it is invisible without this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectIdUsage {
    /// Ids backing a live entity.
    pub used: u32,
    /// Ids handed to a client that has not created on them yet.
    pub leased: u32,
    pub free: u32,
    pub max: u32,
}

/// The zone topology around this player.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeshInfo {
    pub zones_online: u32,
    /// The zone process that owns this player. `None` while routing is still
    /// pending, or after the owning zone was evicted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<ZoneInfo>,
    /// Distance to the edge of the current zone, in metres. Absent when the
    /// player has no zone, or no decoded position to measure from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distance_to_edge: Option<f32>,
    /// `[meshing].boundary_margin` — the band inside which a handoff is
    /// prepared. Neighbours nearer than this are the ones actually being
    /// warmed up.
    pub handoff_margin: f32,
    /// Every other registered zone, nearest first.
    pub neighbours: Vec<NeighbourZone>,
}

/// A registered zone, as the gateway sees it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ZoneInfo {
    pub id: String,
    /// `[x_min, y_min, x_max, y_max]`.
    pub bounds: [f32; 4],
    pub players: u32,
    pub entities: u32,
    pub max_players: u32,
    pub heartbeat_age_ms: u64,
    /// `healthy` / `stale` — as classified by the zone registry.
    pub status: String,
}

/// A zone the player is not in, and how close they are to entering it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NeighbourZone {
    #[serde(flatten)]
    pub zone: ZoneInfo,
    /// Metres from the player to this zone's bounds (0 when already inside).
    pub distance: f32,
    /// Compass bearing from the player to the zone, e.g. `NE`.
    pub direction: String,
    /// Within [`MeshInfo::handoff_margin`]: close enough that crossing would
    /// trigger handoff preparation into this zone.
    pub armed: bool,
}

/// Compass bearing from a point to the nearest part of an axis-aligned box.
///
/// Returns an empty string for a point inside the box: it has no bearing, and
/// naming one would be worse than saying nothing.
pub fn bearing_to(from: [f32; 2], bounds: [f32; 4]) -> String {
    let [x, y] = from;
    let [x_min, y_min, x_max, y_max] = bounds;
    let dx = if x < x_min {
        1
    } else if x >= x_max {
        -1
    } else {
        0
    };
    let dy = if y < y_min {
        1
    } else if y >= y_max {
        -1
    } else {
        0
    };
    // GTA V world space is Y-north, X-east.
    let ns = match dy {
        1 => "N",
        -1 => "S",
        _ => "",
    };
    let ew = match dx {
        1 => "E",
        -1 => "W",
        _ => "",
    };
    format!("{ns}{ew}")
}

/// Planar distance from a point to an axis-aligned box (0 when inside).
pub fn distance_to_bounds(from: [f32; 2], bounds: [f32; 4]) -> f32 {
    let [x, y] = from;
    let [x_min, y_min, x_max, y_max] = bounds;
    let dx = (x_min - x).max(0.0).max(x - x_max);
    let dy = (y_min - y).max(0.0).max(y - y_max);
    (dx * dx + dy * dy).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOX: [f32; 4] = [0.0, 0.0, 100.0, 100.0];

    #[test]
    fn bearing_names_every_quadrant() {
        assert_eq!(bearing_to([50.0, 150.0], BOX), "S");
        assert_eq!(bearing_to([50.0, -50.0], BOX), "N");
        assert_eq!(bearing_to([150.0, 50.0], BOX), "W");
        assert_eq!(bearing_to([-50.0, 50.0], BOX), "E");
        assert_eq!(bearing_to([-50.0, -50.0], BOX), "NE");
        assert_eq!(bearing_to([150.0, 150.0], BOX), "SW");
    }

    #[test]
    fn a_point_inside_has_no_bearing() {
        assert_eq!(bearing_to([50.0, 50.0], BOX), "");
        assert_eq!(distance_to_bounds([50.0, 50.0], BOX), 0.0);
    }

    #[test]
    fn distance_is_planar_and_corner_aware() {
        // Straight out from an edge: the edge distance.
        assert_eq!(distance_to_bounds([50.0, 130.0], BOX), 30.0);
        // Past a corner: the diagonal, not the larger of the two axes.
        let corner = distance_to_bounds([130.0, 140.0], BOX);
        assert!(
            (corner - 50.0).abs() < 1e-3,
            "expected 3-4-5 corner, got {corner}"
        );
    }

    #[test]
    fn absent_subsystems_are_omitted_not_zeroed() {
        let snapshot = DebugInfoSnapshot {
            v: DEBUG_INFO_VERSION,
            server: ServerInfo {
                name: "BASTON".into(),
                build: 3258,
                uptime_secs: 1,
                players: 1,
                max_players: 32,
                tick_hz: 20,
                tick_utilization: 0.1,
                tick_ms: 0.5,
                unix_time: 0,
            },
            net: NetInfo {
                rtt_ms: 12.0,
                rtt_variance_ms: 1.0,
                loss_pct: 0.0,
                packets_sent: 10,
                packets_lost: 0,
                bw_in_kbps: 1.0,
                bw_out_kbps: 2.0,
                mtu: 1400,
            },
            player: PlayerDebugInfo {
                source: 1,
                name: "dev".into(),
                position: [0.0; 3],
                velocity: [0.0; 3],
                sector: [0; 3],
                heading: None,
                net_id: None,
                health: None,
                armour: None,
            },
            onesync: None,
            mesh: None,
        };
        let json = serde_json::to_string(&snapshot).expect("snapshot serializes");
        assert!(
            !json.contains("onesync"),
            "onesync must be absent, not null: {json}"
        );
        assert!(
            !json.contains("mesh"),
            "mesh must be absent, not null: {json}"
        );
        assert!(
            !json.contains("heading"),
            "absent optionals stay absent: {json}"
        );
    }

    #[test]
    fn a_neighbour_flattens_its_zone_fields() {
        let neighbour = NeighbourZone {
            zone: ZoneInfo {
                id: "zone-b".into(),
                bounds: BOX,
                players: 3,
                entities: 40,
                max_players: 64,
                heartbeat_age_ms: 120,
                status: "healthy".into(),
            },
            distance: 30.0,
            direction: "N".into(),
            armed: true,
        };
        let json = serde_json::to_value(&neighbour).expect("neighbour serializes");
        // Flattened: the renderer reads `id` directly, not `zone.id`.
        assert_eq!(json["id"], "zone-b");
        assert_eq!(json["armed"], true);
    }
}
