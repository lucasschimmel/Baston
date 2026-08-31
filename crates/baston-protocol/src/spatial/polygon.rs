//! Simple polygons on the map plane, for zone regions that follow a coastline
//! or a district outline rather than a rectangle.
//!
//! Closed implicitly: the last vertex connects back to the first. Winding
//! order is irrelevant — containment is a crossing-number test, which does not
//! care. Self-intersecting rings are rejected at construction, because "inside"
//! has no single meaning for them and a zone map has to be unambiguous.

use serde::Serialize;

use super::Aabb;

/// Why a vertex ring could not become a polygon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolygonError {
    /// Fewer than three distinct vertices — no interior to speak of.
    TooFewPoints(usize),
    /// Two non-adjacent edges cross, so "inside" is ambiguous.
    SelfIntersecting { first: usize, second: usize },
}

impl std::fmt::Display for PolygonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooFewPoints(n) => {
                write!(f, "a polygon needs at least 3 points, got {n}")
            }
            Self::SelfIntersecting { first, second } => write!(
                f,
                "edges {first} and {second} cross: the outline overlaps itself, \
                 so which side is inside is undefined"
            ),
        }
    }
}

impl std::error::Error for PolygonError {}

/// A simple (non-self-intersecting) closed polygon.
///
/// The bounding box is precomputed: it rejects the overwhelming majority of
/// containment tests in four comparisons, so the crossing-number walk only
/// runs for points that are plausibly inside.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Polygon {
    points: Vec<(f32, f32)>,
    #[serde(skip)]
    bbox: Aabb,
}

impl Polygon {
    /// Build a polygon from a vertex ring.
    ///
    /// A repeated closing vertex is dropped rather than refused: most tracing
    /// tools emit one, and rejecting it would be pedantry over a ring that is
    /// perfectly well defined.
    pub fn new(mut points: Vec<(f32, f32)>) -> Result<Self, PolygonError> {
        if points.len() >= 2 && points[0] == points[points.len() - 1] {
            points.pop();
        }
        if points.len() < 3 {
            return Err(PolygonError::TooFewPoints(points.len()));
        }
        if let Some((first, second)) = find_crossing_edges(&points) {
            return Err(PolygonError::SelfIntersecting { first, second });
        }

        let mut bbox = Aabb::new(f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for &(x, y) in &points {
            bbox.x_min = bbox.x_min.min(x);
            bbox.y_min = bbox.y_min.min(y);
            bbox.x_max = bbox.x_max.max(x);
            bbox.y_max = bbox.y_max.max(y);
        }
        Ok(Self { points, bbox })
    }

    pub fn points(&self) -> &[(f32, f32)] {
        &self.points
    }

    /// Axis-aligned bounds. Used to index the polygon in the spatial tree and
    /// to reject far-away points cheaply.
    pub fn bbox(&self) -> Aabb {
        self.bbox
    }

    /// Crossing-number containment.
    ///
    /// The bbox test here is inclusive on both edges, unlike [`Aabb::contains`]:
    /// it is a rejection filter, not an ownership rule, and excluding the max
    /// edge would drop points the ray cast would have accepted.
    pub fn contains(&self, x: f32, y: f32) -> bool {
        if x < self.bbox.x_min || x > self.bbox.x_max || y < self.bbox.y_min || y > self.bbox.y_max
        {
            return false;
        }

        let mut inside = false;
        let n = self.points.len();
        let mut j = n - 1;
        for i in 0..n {
            let (xi, yi) = self.points[i];
            let (xj, yj) = self.points[j];
            if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
                inside = !inside;
            }
            j = i;
        }
        inside
    }

    /// Whether any edge of this polygon crosses any edge of a closed ring.
    ///
    /// With every vertex of the ring already known to be inside, "no edge
    /// crosses" is what upgrades that to "the whole ring is inside": it rules
    /// out a concave notch reaching in between two of the ring's vertices.
    pub fn crosses_ring(&self, ring: &[(f32, f32)]) -> bool {
        let mine = self.points.len();
        for i in 0..mine {
            let a = self.points[i];
            let b = self.points[(i + 1) % mine];
            for j in 0..ring.len() {
                let c = ring[j];
                let d = ring[(j + 1) % ring.len()];
                if segments_intersect(a, b, c, d) {
                    return true;
                }
            }
        }
        false
    }

    /// Distance to the outline, positive inside and negative outside.
    pub fn signed_distance_to_edge(&self, x: f32, y: f32) -> f32 {
        let mut nearest = f32::MAX;
        let n = self.points.len();
        let mut j = n - 1;
        for i in 0..n {
            let (xi, yi) = self.points[i];
            let (xj, yj) = self.points[j];
            nearest = nearest.min(point_segment_distance(x, y, xj, yj, xi, yi));
            j = i;
        }
        if self.contains(x, y) {
            nearest
        } else {
            -nearest
        }
    }
}

/// Shortest distance from a point to a line segment.
fn point_segment_distance(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let (dx, dy) = (bx - ax, by - ay);
    let len_sq = dx * dx + dy * dy;
    if len_sq <= f32::EPSILON {
        return (px - ax).hypot(py - ay);
    }
    let t = (((px - ax) * dx + (py - ay) * dy) / len_sq).clamp(0.0, 1.0);
    (px - (ax + t * dx)).hypot(py - (ay + t * dy))
}

/// Cross product of AB × AC. Sign gives the turn direction.
fn orientation(ax: f32, ay: f32, bx: f32, by: f32, cx: f32, cy: f32) -> f32 {
    (bx - ax) * (cy - ay) - (by - ay) * (cx - ax)
}

/// Whether C lies on segment AB, knowing the three are collinear.
fn on_segment(ax: f32, ay: f32, bx: f32, by: f32, cx: f32, cy: f32) -> bool {
    cx >= ax.min(bx) && cx <= ax.max(bx) && cy >= ay.min(by) && cy <= ay.max(by)
}

fn segments_intersect(
    (ax, ay): (f32, f32),
    (bx, by): (f32, f32),
    (cx, cy): (f32, f32),
    (dx, dy): (f32, f32),
) -> bool {
    let d1 = orientation(ax, ay, bx, by, cx, cy);
    let d2 = orientation(ax, ay, bx, by, dx, dy);
    let d3 = orientation(cx, cy, dx, dy, ax, ay);
    let d4 = orientation(cx, cy, dx, dy, bx, by);

    if ((d1 > 0.0) != (d2 > 0.0)) && ((d3 > 0.0) != (d4 > 0.0)) {
        return true;
    }
    // Collinear overlap: a vertex sitting on a non-adjacent edge still makes
    // the outline ambiguous.
    (d1 == 0.0 && on_segment(ax, ay, bx, by, cx, cy))
        || (d2 == 0.0 && on_segment(ax, ay, bx, by, dx, dy))
        || (d3 == 0.0 && on_segment(cx, cy, dx, dy, ax, ay))
        || (d4 == 0.0 && on_segment(cx, cy, dx, dy, bx, by))
}

/// First pair of non-adjacent edges that cross, if any.
///
/// O(n²) over a ring authored by hand — tens of vertices, checked once at
/// boot. A sweep line would be faster and much easier to get subtly wrong.
fn find_crossing_edges(points: &[(f32, f32)]) -> Option<(usize, usize)> {
    let n = points.len();
    for i in 0..n {
        let a = points[i];
        let b = points[(i + 1) % n];
        for j in (i + 1)..n {
            // Adjacent edges legitimately share a vertex.
            if j == i + 1 || (i == 0 && j == n - 1) {
                continue;
            }
            let c = points[j];
            let d = points[(j + 1) % n];
            if segments_intersect(a, b, c, d) {
                return Some((i, j));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 200×200 square centred on the origin.
    fn square() -> Polygon {
        Polygon::new(vec![
            (-100.0, -100.0),
            (100.0, -100.0),
            (100.0, 100.0),
            (-100.0, 100.0),
        ])
        .unwrap()
    }

    #[test]
    fn contains_interior_and_rejects_exterior() {
        let p = square();
        assert!(p.contains(0.0, 0.0));
        assert!(p.contains(-99.0, 99.0));
        assert!(!p.contains(101.0, 0.0));
        assert!(!p.contains(0.0, -200.0));
    }

    /// An L shape: the point in the missing quadrant is inside the bounding
    /// box but outside the polygon. This is the whole reason for using one.
    #[test]
    fn a_concave_outline_excludes_its_notch() {
        let l = Polygon::new(vec![
            (0.0, 0.0),
            (100.0, 0.0),
            (100.0, 40.0),
            (40.0, 40.0),
            (40.0, 100.0),
            (0.0, 100.0),
        ])
        .unwrap();
        assert!(l.contains(20.0, 20.0));
        assert!(l.contains(80.0, 20.0));
        assert!(l.contains(20.0, 80.0));
        // Inside the bbox, outside the shape.
        assert!(l.bbox().contains(80.0, 80.0));
        assert!(!l.contains(80.0, 80.0));
    }

    #[test]
    fn winding_order_does_not_matter() {
        let clockwise = Polygon::new(vec![
            (-100.0, -100.0),
            (-100.0, 100.0),
            (100.0, 100.0),
            (100.0, -100.0),
        ])
        .unwrap();
        assert!(clockwise.contains(0.0, 0.0));
        assert!(!clockwise.contains(500.0, 0.0));
    }

    #[test]
    fn a_repeated_closing_vertex_is_dropped() {
        let p = Polygon::new(vec![
            (0.0, 0.0),
            (100.0, 0.0),
            (100.0, 100.0),
            (0.0, 0.0), // closes the ring explicitly
        ])
        .unwrap();
        assert_eq!(p.points().len(), 3);
        assert!(p.contains(50.0, 25.0));
    }

    #[test]
    fn too_few_points_is_refused() {
        assert_eq!(
            Polygon::new(vec![(0.0, 0.0), (1.0, 1.0)]),
            Err(PolygonError::TooFewPoints(2))
        );
        // Three points that collapse to two once the ring is closed.
        assert_eq!(
            Polygon::new(vec![(0.0, 0.0), (1.0, 1.0), (0.0, 0.0)]),
            Err(PolygonError::TooFewPoints(2))
        );
    }

    /// A bow tie. Rejected because "inside" would depend on the fill rule.
    #[test]
    fn a_self_intersecting_outline_is_refused() {
        let bow_tie = Polygon::new(vec![(0.0, 0.0), (100.0, 100.0), (100.0, 0.0), (0.0, 100.0)]);
        assert!(matches!(
            bow_tie,
            Err(PolygonError::SelfIntersecting { .. })
        ));
    }

    #[test]
    fn a_concave_but_simple_outline_is_accepted() {
        // The L above is concave and must NOT be mistaken for self-crossing.
        assert!(Polygon::new(vec![
            (0.0, 0.0),
            (100.0, 0.0),
            (100.0, 40.0),
            (40.0, 40.0),
            (40.0, 100.0),
            (0.0, 100.0),
        ])
        .is_ok());
    }

    #[test]
    fn signed_distance_is_positive_inside_negative_outside() {
        let p = square();
        assert_eq!(p.signed_distance_to_edge(0.0, 0.0), 100.0);
        assert_eq!(p.signed_distance_to_edge(-90.0, 0.0), 10.0);
        assert_eq!(p.signed_distance_to_edge(150.0, 0.0), -50.0);
    }

    #[test]
    fn bbox_wraps_every_vertex() {
        let p = square();
        assert_eq!(p.bbox(), Aabb::new(-100.0, -100.0, 100.0, 100.0));
    }
}
