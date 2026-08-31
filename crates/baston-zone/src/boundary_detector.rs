//! Boundary proximity detection (jalon D3).
//!
//! Scans player kinematics every ~500ms (much coarser than state sync) and
//! flags players about to leave this zone's territory.
//!
//! The test is not "which edge is the player near, and are they moving toward
//! it" — that question only has an answer for a rectangle. It is "extrapolate
//! where they will be, and ask whether that ground is still ours", which is
//! the same question for a rectangle, a circle, a coastline outline, or a
//! territory with an arena carved out of the middle of it.

use baston_protocol::ZoneCoverage;

/// Minimum planar speed (m/s) to consider a player as going anywhere —
/// filters players idling next to a border.
const MIN_APPROACH_SPEED: f32 = 0.5;

/// Extra seconds added to the estimated crossing before probing the position.
/// Lands the probe past the boundary rather than exactly on it, where a
/// containment test could go either way.
const PROBE_OVERSHOOT_SECS: f32 = 1.0;

#[derive(Debug, Clone)]
pub struct HandoffCandidate {
    pub player_id: u32,
    pub estimated_crossing_ms: u64,
    /// Position just past the boundary — the Gateway resolves the target zone
    /// from these coordinates, so nothing upstream needs to know what shape
    /// the territory is.
    pub predicted_coords: (f32, f32),
    pub distance_to_edge: f32,
}

pub struct BoundaryDetector {
    boundary_margin: f32,
}

impl BoundaryDetector {
    pub fn new(boundary_margin: f32) -> Self {
        Self { boundary_margin }
    }

    /// Distance from a position to the edge of the zone's territory. Negative
    /// once the player is off it.
    pub fn distance_to_boundary(&self, coverage: &ZoneCoverage, coords: [f32; 3]) -> f32 {
        coverage.signed_distance_to_edge(coords[0], coords[1])
    }

    /// Check one player. `None` unless they are inside the margin band and
    /// heading off this zone's territory.
    pub fn check_player(
        &self,
        coverage: &ZoneCoverage,
        player_id: u32,
        coords: [f32; 3],
        velocity: [f32; 3],
    ) -> Option<HandoffCandidate> {
        let (x, y) = (coords[0], coords[1]);

        // Already gone — a teleport, or a crossing between two scans. Report
        // it immediately with the real position: they are standing on the
        // target zone's ground, so there is nothing to predict.
        if !coverage.contains(x, y) {
            return Some(HandoffCandidate {
                player_id,
                estimated_crossing_ms: 0,
                predicted_coords: (x, y),
                distance_to_edge: 0.0,
            });
        }

        let distance = coverage.signed_distance_to_edge(x, y);
        if distance >= self.boundary_margin {
            return None;
        }

        let speed = velocity[0].hypot(velocity[1]);
        if speed < MIN_APPROACH_SPEED {
            return None;
        }

        // `distance` is a lower bound on how far the edge is: where two of our
        // own regions meet, it reads the internal seam rather than the real
        // outline. So it sets *when* to look, and the probe below decides.
        let eta_secs = distance.max(0.0) / speed;
        let horizon = eta_secs + PROBE_OVERSHOOT_SECS;
        let probe = (x + velocity[0] * horizon, y + velocity[1] * horizon);

        // Moving along the border, or inward, or across a seam between two of
        // our own regions: still our ground, nothing to hand over.
        if coverage.contains(probe.0, probe.1) {
            return None;
        }

        Some(HandoffCandidate {
            player_id,
            estimated_crossing_ms: (eta_secs * 1000.0) as u64,
            predicted_coords: probe,
            distance_to_edge: distance,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baston_protocol::{Aabb, ZoneShape};

    /// West half of the map; east edge at x=0.
    fn west_half() -> ZoneCoverage {
        ZoneCoverage::from_bounds(Aabb::new(-4000.0, -4000.0, 0.0, 4000.0))
    }

    fn detector() -> BoundaryDetector {
        BoundaryDetector::new(300.0)
    }

    #[test]
    fn far_from_boundary_no_handoff() {
        let d = detector();
        assert!(d
            .check_player(&west_half(), 1, [-350.0, 0.0, 30.0], [10.0, 0.0, 0.0])
            .is_none());
    }

    #[test]
    fn near_boundary_approaching_triggers() {
        let d = detector();
        let c = d
            .check_player(&west_half(), 1, [-250.0, 0.0, 30.0], [12.5, 0.0, 0.0])
            .unwrap();
        // 250m at 12.5 m/s = 20s.
        assert!(
            (19_000..=21_000).contains(&c.estimated_crossing_ms),
            "{}",
            c.estimated_crossing_ms
        );
        assert!(
            c.predicted_coords.0 > 0.0,
            "prediction must land past the edge"
        );
    }

    #[test]
    fn near_boundary_moving_away_no_handoff() {
        let d = detector();
        assert!(d
            .check_player(&west_half(), 1, [-250.0, 0.0, 30.0], [-12.5, 0.0, 0.0])
            .is_none());
    }

    #[test]
    fn idle_near_boundary_no_handoff() {
        let d = detector();
        assert!(d
            .check_player(&west_half(), 1, [-50.0, 0.0, 30.0], [0.0, 0.3, 0.0])
            .is_none());
    }

    /// Driving fast along the border is not leaving it.
    #[test]
    fn moving_parallel_to_the_boundary_no_handoff() {
        let d = detector();
        assert!(d
            .check_player(&west_half(), 1, [-50.0, 0.0, 30.0], [0.0, 30.0, 0.0])
            .is_none());
    }

    #[test]
    fn a_player_already_outside_is_reported_at_once() {
        let d = detector();
        let c = d
            .check_player(&west_half(), 1, [200.0, 50.0, 30.0], [0.0, 0.0, 0.0])
            .expect("a player off our ground is always a candidate");
        assert_eq!(c.estimated_crossing_ms, 0);
        assert_eq!(c.predicted_coords, (200.0, 50.0));
    }

    /// The bug the overlay list exists for. The city owns a 2km square with a
    /// 240m arena carved out of the middle. A player driving at the arena is
    /// nowhere near a city edge, and before overlays nothing fired at all.
    mod arena_carved_out_of_a_city {
        use super::*;

        fn city() -> ZoneCoverage {
            ZoneCoverage::new(
                vec![ZoneShape::rect(Aabb::new(-1000.0, -1000.0, 1000.0, 1000.0)).unwrap()],
                vec![ZoneShape::circle((0.0, 0.0), 240.0).unwrap()],
            )
        }

        #[test]
        fn driving_into_the_arena_prepares_a_handoff() {
            let d = detector();
            // 260m short of the rim, heading straight at it at 20 m/s.
            let c = d
                .check_player(&city(), 7, [-500.0, 0.0, 30.0], [20.0, 0.0, 0.0])
                .expect("entering an overlay must be treated as leaving");
            assert!(
                (12_000..=14_000).contains(&c.estimated_crossing_ms),
                "260m at 20 m/s ≈ 13s, got {}",
                c.estimated_crossing_ms
            );
            assert!(
                c.predicted_coords.0 > -240.0,
                "the probe must land inside the arena, got {:?}",
                c.predicted_coords
            );
        }

        #[test]
        fn driving_away_from_the_arena_does_not() {
            let d = detector();
            assert!(d
                .check_player(&city(), 7, [-300.0, 0.0, 30.0], [-20.0, 0.0, 0.0])
                .is_none());
        }

        #[test]
        fn standing_in_the_arena_is_already_outside() {
            let d = detector();
            let c = d
                .check_player(&city(), 7, [0.0, 0.0, 30.0], [0.0, 0.0, 0.0])
                .expect("the arena is not the city's ground");
            assert_eq!(c.estimated_crossing_ms, 0);
        }
    }

    /// Two touching regions of the same zone. The seam reads as an edge to the
    /// distance estimate, and the probe is what stops it becoming a handoff to
    /// ourselves.
    #[test]
    fn crossing_a_seam_between_our_own_regions_is_not_a_crossing() {
        let d = detector();
        let two_halves = ZoneCoverage::new(
            vec![
                ZoneShape::rect(Aabb::new(-1000.0, -1000.0, 0.0, 1000.0)).unwrap(),
                ZoneShape::rect(Aabb::new(0.0, -1000.0, 1000.0, 1000.0)).unwrap(),
            ],
            Vec::new(),
        );
        // Right on the seam, driving across it.
        assert!(d.distance_to_boundary(&two_halves, [-10.0, 0.0, 30.0]) < 300.0);
        assert!(d
            .check_player(&two_halves, 1, [-10.0, 0.0, 30.0], [20.0, 0.0, 0.0])
            .is_none());
    }
}
