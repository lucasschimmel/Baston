//! Boundary proximity detection (jalon D3).
//!
//! Scans player kinematics every ~500ms (much coarser than state sync) and
//! flags players close to a zone edge AND moving toward it.

use baston_protocol::Aabb;

/// Minimum outward speed (m/s) to consider a player "approaching" an edge —
/// filters players idling next to the border.
const MIN_APPROACH_SPEED: f32 = 0.5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundaryDirection {
    West,
    East,
    South,
    North,
}

#[derive(Debug, Clone)]
pub struct HandoffCandidate {
    pub player_id: u32,
    pub estimated_crossing_ms: u64,
    pub target_direction: BoundaryDirection,
    /// Predicted position just past the edge — the Gateway resolves the
    /// target zone from these coordinates.
    pub predicted_coords: (f32, f32),
    pub distance_to_edge: f32,
}

pub struct BoundaryDetector {
    zone_bounds: Aabb,
    boundary_margin: f32,
}

impl BoundaryDetector {
    pub fn new(zone_bounds: Aabb, boundary_margin: f32) -> Self {
        Self { zone_bounds, boundary_margin }
    }

    /// Distance from an in-zone position to the nearest edge.
    pub fn distance_to_boundary(&self, coords: [f32; 3]) -> f32 {
        self.zone_bounds.distance_to_edge(coords[0], coords[1])
    }

    /// Check one player. `None` unless the player is inside the margin band
    /// and moving toward the nearest edge.
    pub fn check_player(
        &self,
        player_id: u32,
        coords: [f32; 3],
        velocity: [f32; 3],
    ) -> Option<HandoffCandidate> {
        let (x, y) = (coords[0], coords[1]);
        if !self.zone_bounds.contains(x, y) {
            // Already outside: crossing happened — the HandoffManager treats
            // this as "confirm now" (estimated_crossing_ms = 0).
        }
        let dist = self.distance_to_boundary(coords);
        if dist >= self.boundary_margin {
            return None;
        }

        // Nearest edge + outward speed toward it.
        let b = &self.zone_bounds;
        let edges = [
            (x - b.x_min, BoundaryDirection::West, -velocity[0]),
            (b.x_max - x, BoundaryDirection::East, velocity[0]),
            (y - b.y_min, BoundaryDirection::South, -velocity[1]),
            (b.y_max - y, BoundaryDirection::North, velocity[1]),
        ];
        let (edge_dist, direction, outward_speed) = edges
            .into_iter()
            .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))?;

        if outward_speed < MIN_APPROACH_SPEED {
            return None; // moving away from (or parallel to) the edge
        }

        let eta_secs = (edge_dist.max(0.0)) / outward_speed;
        // Predict the crossing point a little past the edge so the point
        // lands inside the neighbour zone, not on the shared border.
        let overshoot = eta_secs + 1.0;
        let predicted = (x + velocity[0] * overshoot, y + velocity[1] * overshoot);

        Some(HandoffCandidate {
            player_id,
            estimated_crossing_ms: (eta_secs * 1000.0) as u64,
            target_direction: direction,
            predicted_coords: predicted,
            distance_to_edge: edge_dist,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // zone-a: west half of the map; east edge at x=0.
    fn detector() -> BoundaryDetector {
        BoundaryDetector::new(Aabb::new(-4000.0, -4000.0, 0.0, 4000.0), 300.0)
    }

    #[test]
    fn far_from_boundary_no_handoff() {
        let d = detector();
        assert!(d.check_player(1, [-350.0, 0.0, 30.0], [10.0, 0.0, 0.0]).is_none());
    }

    #[test]
    fn near_boundary_approaching_triggers() {
        let d = detector();
        let c = d.check_player(1, [-250.0, 0.0, 30.0], [12.5, 0.0, 0.0]).unwrap();
        assert_eq!(c.target_direction, BoundaryDirection::East);
        // 250m at 12.5 m/s = 20s.
        assert!((19_000..=21_000).contains(&c.estimated_crossing_ms), "{}", c.estimated_crossing_ms);
        assert!(c.predicted_coords.0 > 0.0, "prediction must land past the edge");
    }

    #[test]
    fn near_boundary_moving_away_no_handoff() {
        let d = detector();
        assert!(d.check_player(1, [-250.0, 0.0, 30.0], [-12.5, 0.0, 0.0]).is_none());
    }

    #[test]
    fn idle_near_boundary_no_handoff() {
        let d = detector();
        assert!(d.check_player(1, [-50.0, 0.0, 30.0], [0.0, 0.3, 0.0]).is_none());
    }

    #[test]
    fn nearest_edge_wins() {
        let d = detector();
        // Close to the north edge, moving north.
        let c = d.check_player(1, [-2000.0, 3900.0, 30.0], [0.0, 20.0, 0.0]).unwrap();
        assert_eq!(c.target_direction, BoundaryDirection::North);
    }
}
