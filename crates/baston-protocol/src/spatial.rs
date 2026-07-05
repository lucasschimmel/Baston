//! 2D axis-aligned bounding boxes for zone bounds (GTA V map plane).

use serde::{Deserialize, Serialize};

/// Inclusive-min / exclusive-max 2D bounds. Exclusive max edges make adjacent
/// zones tile the map without a coordinate belonging to two zones at once.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Aabb {
    pub x_min: f32,
    pub y_min: f32,
    pub x_max: f32,
    pub y_max: f32,
}

impl Aabb {
    pub fn new(x_min: f32, y_min: f32, x_max: f32, y_max: f32) -> Self {
        Self {
            x_min,
            y_min,
            x_max,
            y_max,
        }
    }

    /// Parse the `ZONE_BOUNDS` env format: `x_min,y_min,x_max,y_max`.
    pub fn parse(s: &str) -> Result<Self, String> {
        let parts: Vec<f32> = s
            .split(',')
            .map(|p| {
                p.trim()
                    .parse::<f32>()
                    .map_err(|e| format!("bad bound {p:?}: {e}"))
            })
            .collect::<Result<_, _>>()?;
        if parts.len() != 4 {
            return Err(format!(
                "expected 4 comma-separated floats, got {}",
                parts.len()
            ));
        }
        let aabb = Self::new(parts[0], parts[1], parts[2], parts[3]);
        if aabb.x_min >= aabb.x_max || aabb.y_min >= aabb.y_max {
            return Err(format!("degenerate bounds: {aabb:?}"));
        }
        Ok(aabb)
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x_min && x < self.x_max && y >= self.y_min && y < self.y_max
    }

    pub fn intersects(&self, other: &Aabb) -> bool {
        self.x_min < other.x_max
            && other.x_min < self.x_max
            && self.y_min < other.y_max
            && other.y_min < self.y_max
    }

    /// Distance from an interior point to the nearest edge (negative if outside).
    pub fn distance_to_edge(&self, x: f32, y: f32) -> f32 {
        let dx = (x - self.x_min).min(self.x_max - x);
        let dy = (y - self.y_min).min(self.y_max - y);
        dx.min(dy)
    }

    pub fn center(&self) -> (f32, f32) {
        (
            (self.x_min + self.x_max) / 2.0,
            (self.y_min + self.y_max) / 2.0,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_zone_bounds() {
        let b = Aabb::parse("-4000,-4000,0,4000").unwrap();
        assert_eq!(b, Aabb::new(-4000.0, -4000.0, 0.0, 4000.0));
        assert!(Aabb::parse("1,2,3").is_err());
        assert!(Aabb::parse("0,0,0,10").is_err()); // degenerate x
    }

    #[test]
    fn contains_is_min_inclusive_max_exclusive() {
        let a = Aabb::new(-4000.0, -4000.0, 0.0, 4000.0);
        let b = Aabb::new(0.0, -4000.0, 4000.0, 4000.0);
        // The shared edge x=0 belongs to exactly one zone.
        assert!(!a.contains(0.0, 100.0));
        assert!(b.contains(0.0, 100.0));
    }

    #[test]
    fn distance_to_edge() {
        let a = Aabb::new(-4000.0, -4000.0, 0.0, 4000.0);
        assert_eq!(a.distance_to_edge(-500.0, 0.0), 500.0);
        assert_eq!(a.distance_to_edge(-3900.0, 0.0), 100.0);
    }
}
