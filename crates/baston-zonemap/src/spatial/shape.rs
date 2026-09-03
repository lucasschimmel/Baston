//! The shapes a zone map is drawn with, and the territory one zone ends up
//! owning once higher-priority regions are subtracted from it.

use serde::Serialize;

use super::{Aabb, Polygon, PolygonError};

/// Why a shape could not be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShapeError {
    /// `x_min >= x_max` or `y_min >= y_max`: no area.
    DegenerateRect,
    /// Radius zero or negative, or a non-finite centre.
    BadCircle,
    Polygon(PolygonError),
}

impl std::fmt::Display for ShapeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DegenerateRect => {
                write!(
                    f,
                    "rect bounds must satisfy x_min < x_max and y_min < y_max"
                )
            }
            Self::BadCircle => write!(f, "circle needs a finite centre and a radius above zero"),
            Self::Polygon(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ShapeError {}

impl From<PolygonError> for ShapeError {
    fn from(e: PolygonError) -> Self {
        Self::Polygon(e)
    }
}

/// One claimed area on the map plane.
///
/// Deliberately 2D. A `z` range would turn every shape into a prism and force
/// the whole routing surface — which speaks in `(x, y)` from the connection
/// router down to the handoff's predicted coordinates — to carry an altitude
/// it has nowhere to get.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "shape", rename_all = "lowercase")]
pub enum ZoneShape {
    Rect(Aabb),
    Circle {
        center: (f32, f32),
        radius: f32,
    },
    Poly(Polygon),
    /// Matches every point. Only legal as the last region of a map, where it
    /// is what makes a hole impossible rather than merely unlikely.
    Everywhere,
}

impl ZoneShape {
    pub fn rect(bounds: Aabb) -> Result<Self, ShapeError> {
        if bounds.x_min >= bounds.x_max || bounds.y_min >= bounds.y_max {
            return Err(ShapeError::DegenerateRect);
        }
        Ok(Self::Rect(bounds))
    }

    pub fn circle(center: (f32, f32), radius: f32) -> Result<Self, ShapeError> {
        // `radius.is_finite()` first, so a NaN radius is refused rather than
        // slipping through a comparison that is false either way.
        if !center.0.is_finite() || !center.1.is_finite() || !radius.is_finite() || radius <= 0.0 {
            return Err(ShapeError::BadCircle);
        }
        Ok(Self::Circle { center, radius })
    }

    pub fn poly(points: Vec<(f32, f32)>) -> Result<Self, ShapeError> {
        Ok(Self::Poly(Polygon::new(points)?))
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        match self {
            Self::Rect(bounds) => bounds.contains(x, y),
            Self::Circle { center, radius } => (x - center.0).hypot(y - center.1) <= *radius,
            Self::Poly(poly) => poly.contains(x, y),
            Self::Everywhere => true,
        }
    }

    /// Distance to the outline: positive inside, negative outside.
    pub fn signed_distance_to_edge(&self, x: f32, y: f32) -> f32 {
        match self {
            Self::Rect(bounds) => bounds.distance_to_edge(x, y),
            Self::Circle { center, radius } => radius - (x - center.0).hypot(y - center.1),
            Self::Poly(poly) => poly.signed_distance_to_edge(x, y),
            Self::Everywhere => f32::INFINITY,
        }
    }

    /// Axis-aligned bounds, or `None` for [`ZoneShape::Everywhere`], which has
    /// none to give. Used to index the shape spatially.
    pub fn bbox(&self) -> Option<Aabb> {
        match self {
            Self::Rect(bounds) => Some(*bounds),
            Self::Circle { center, radius } => Some(Aabb::new(
                center.0 - radius,
                center.1 - radius,
                center.0 + radius,
                center.1 + radius,
            )),
            Self::Poly(poly) => Some(poly.bbox()),
            Self::Everywhere => None,
        }
    }

    /// The shape's outline as a closed ring, for shapes that have one.
    /// `None` for a circle, which is not polygonal, and for `everywhere`.
    fn ring(&self) -> Option<Vec<(f32, f32)>> {
        match self {
            Self::Rect(b) => Some(vec![
                (b.x_min, b.y_min),
                (b.x_max, b.y_min),
                (b.x_max, b.y_max),
                (b.x_min, b.y_max),
            ]),
            Self::Poly(poly) => Some(poly.points().to_vec()),
            Self::Circle { .. } | Self::Everywhere => None,
        }
    }

    /// Whether every point of `other` is also in `self`.
    ///
    /// Conservative: a `true` is always right, a `false` may be a miss. Used
    /// only to warn about a region a higher-priority one has made unreachable,
    /// where a missed warning costs nothing and a wrong one costs trust.
    pub fn covers(&self, other: &ZoneShape) -> bool {
        match (self, other) {
            (Self::Everywhere, _) => true,
            // Nothing finite swallows the whole plane.
            (_, Self::Everywhere) => false,

            // Convex container: it holds a shape once it holds every extreme
            // point of it, and a bounding box has no extreme point outside.
            (Self::Rect(bounds), _) => other.bbox().is_some_and(|b| {
                b.x_min >= bounds.x_min
                    && b.x_max <= bounds.x_max
                    && b.y_min >= bounds.y_min
                    && b.y_max <= bounds.y_max
            }),
            (
                Self::Circle { center, radius },
                Self::Circle {
                    center: c,
                    radius: r,
                },
            ) => (c.0 - center.0).hypot(c.1 - center.1) + r <= *radius,
            (Self::Circle { center, radius }, _) => other.ring().is_some_and(|ring| {
                ring.iter()
                    .all(|&(x, y)| (x - center.0).hypot(y - center.1) <= *radius)
            }),

            // A polygon is not convex, so corners are not enough. For a circle
            // the signed distance answers it exactly: the whole disc is inside
            // when the centre is inside and no edge comes closer than r.
            (Self::Poly(poly), Self::Circle { center, radius }) => {
                poly.signed_distance_to_edge(center.0, center.1) >= *radius
            }
            (Self::Poly(poly), _) => other.ring().is_some_and(|ring| {
                ring.iter().all(|&(x, y)| poly.contains(x, y)) && !poly.crosses_ring(&ring)
            }),
        }
    }
}

/// What one zone actually owns: its own regions, minus the regions of every
/// higher-priority zone that overlaps them.
///
/// A zone needs the second list as much as the first. Without it a player
/// walking from Los Santos *into* an arena carved out of Los Santos never
/// leaves the Los Santos outline, so nothing ever fires and the player stays
/// on a zone that no longer owns the ground under them.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ZoneCoverage {
    shapes: Vec<ZoneShape>,
    overlays: Vec<ZoneShape>,
}

impl ZoneCoverage {
    pub fn new(shapes: Vec<ZoneShape>, overlays: Vec<ZoneShape>) -> Self {
        Self { shapes, overlays }
    }

    /// Single-rectangle coverage — the shape of a map-less deployment, where
    /// each zone still declares its own bounds.
    pub fn from_bounds(bounds: Aabb) -> Self {
        Self::new(vec![ZoneShape::Rect(bounds)], Vec::new())
    }

    pub fn shapes(&self) -> &[ZoneShape] {
        &self.shapes
    }

    pub fn overlays(&self) -> &[ZoneShape] {
        &self.overlays
    }

    pub fn is_empty(&self) -> bool {
        self.shapes.is_empty()
    }

    /// Does this zone own the ground at `(x, y)`?
    ///
    /// This is the authority on ownership; [`Self::signed_distance_to_edge`]
    /// is a margin heuristic and the two can disagree by an epsilon exactly on
    /// a boundary.
    pub fn contains(&self, x: f32, y: f32) -> bool {
        self.shapes.iter().any(|s| s.contains(x, y))
            && !self.overlays.iter().any(|s| s.contains(x, y))
    }

    /// Distance to the edge of this zone's territory: positive inside,
    /// negative outside.
    ///
    /// Leaving the union of our own shapes and entering an overlay are the
    /// same event — both mean the ground stops being ours — so both are
    /// measured and the nearer wins.
    ///
    /// Where two of a zone's own shapes touch, the union's real edge is
    /// further away than this reports. The error is one-sided: it prepares a
    /// handoff that resolves back to this same zone and is dropped, which is
    /// cheap. Reporting too *much* room would miss a real crossing.
    pub fn signed_distance_to_edge(&self, x: f32, y: f32) -> f32 {
        let leaving_ours = self
            .shapes
            .iter()
            .map(|s| s.signed_distance_to_edge(x, y))
            .fold(f32::NEG_INFINITY, f32::max);
        let entering_theirs = self
            .overlays
            .iter()
            .map(|s| -s.signed_distance_to_edge(x, y))
            .fold(f32::INFINITY, f32::min);
        leaving_ours.min(entering_theirs)
    }

    /// Bounds enclosing every own shape, or `None` when one of them is
    /// [`ZoneShape::Everywhere`] and there is nothing finite to enclose.
    pub fn bbox(&self) -> Option<Aabb> {
        let mut acc: Option<Aabb> = None;
        for shape in &self.shapes {
            let b = shape.bbox()?;
            acc = Some(match acc {
                None => b,
                Some(a) => Aabb::new(
                    a.x_min.min(b.x_min),
                    a.y_min.min(b.y_min),
                    a.x_max.max(b.x_max),
                    a.y_max.max(b.y_max),
                ),
            });
        }
        acc
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arena() -> ZoneShape {
        ZoneShape::circle((0.0, 0.0), 100.0).unwrap()
    }

    fn city() -> ZoneShape {
        ZoneShape::rect(Aabb::new(-1000.0, -1000.0, 1000.0, 1000.0)).unwrap()
    }

    #[test]
    fn degenerate_shapes_are_refused() {
        assert_eq!(
            ZoneShape::rect(Aabb::new(0.0, 0.0, 0.0, 10.0)),
            Err(ShapeError::DegenerateRect)
        );
        assert_eq!(
            ZoneShape::circle((0.0, 0.0), 0.0),
            Err(ShapeError::BadCircle)
        );
        assert_eq!(
            ZoneShape::circle((f32::NAN, 0.0), 10.0),
            Err(ShapeError::BadCircle)
        );
    }

    #[test]
    fn circle_containment_and_distance() {
        let c = arena();
        assert!(c.contains(0.0, 0.0));
        assert!(c.contains(99.0, 0.0));
        assert!(!c.contains(101.0, 0.0));
        assert_eq!(c.signed_distance_to_edge(0.0, 0.0), 100.0);
        assert_eq!(c.signed_distance_to_edge(150.0, 0.0), -50.0);
    }

    #[test]
    fn everywhere_swallows_the_plane() {
        let e = ZoneShape::Everywhere;
        assert!(e.contains(0.0, 0.0));
        assert!(e.contains(1e9, -1e9));
        assert_eq!(e.bbox(), None);
    }

    /// The map author's likeliest mistake: the arena listed after the city
    /// that already covers it. The region is dead and must be reported.
    #[test]
    fn a_region_swallowed_by_an_earlier_one_is_detectable() {
        assert!(city().covers(&arena()));
        assert!(!arena().covers(&city()));
        assert!(ZoneShape::Everywhere.covers(&city()));
        assert!(!city().covers(&ZoneShape::Everywhere));
    }

    /// And the same mistake when the city is traced rather than boxed, which
    /// is the shape anyone drawing a real city actually uses.
    #[test]
    fn a_traced_city_still_reveals_an_arena_hidden_behind_it() {
        let traced = ZoneShape::poly(vec![
            (-1000.0, -1000.0),
            (1000.0, -1000.0),
            (1000.0, 1000.0),
            (-1000.0, 1000.0),
        ])
        .unwrap();
        assert!(traced.covers(&arena()));
        assert!(traced.covers(&ZoneShape::rect(Aabb::new(-50.0, -50.0, 50.0, 50.0)).unwrap()));

        // A circle poking out of the outline is not covered by it.
        let straddling = ZoneShape::circle((950.0, 0.0), 100.0).unwrap();
        assert!(!traced.covers(&straddling));
    }

    /// A concave outline must not be credited with covering what sits in its
    /// notch, even though every corner of that thing is "inside".
    #[test]
    fn a_notch_is_not_covered_by_the_outline_around_it() {
        let l_shape = ZoneShape::poly(vec![
            (0.0, 0.0),
            (100.0, 0.0),
            (100.0, 40.0),
            (40.0, 40.0),
            (40.0, 100.0),
            (0.0, 100.0),
        ])
        .unwrap();
        assert!(l_shape.covers(&ZoneShape::rect(Aabb::new(5.0, 5.0, 35.0, 35.0)).unwrap()));
        // Spanning the notch: corners land in the arms, the middle does not.
        assert!(!l_shape.covers(&ZoneShape::rect(Aabb::new(10.0, 10.0, 90.0, 90.0)).unwrap()));
        assert!(!l_shape.covers(&ZoneShape::circle((80.0, 80.0), 10.0).unwrap()));
    }

    /// The derive exists so a territory can be reported to an operator. An
    /// internally-tagged enum over a newtype variant is exactly the shape
    /// serde refuses at runtime rather than at compile time, so it is checked.
    #[test]
    fn a_territory_can_be_reported_as_json() {
        let coverage = ZoneCoverage::new(
            vec![
                city(),
                ZoneShape::poly(vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0)]).unwrap(),
            ],
            vec![arena()],
        );
        let json = serde_json::to_string(&coverage).expect("a territory must be reportable");
        assert!(json.contains("\"shape\":\"rect\""), "{json}");
        assert!(json.contains("\"shape\":\"circle\""), "{json}");
        assert!(json.contains("\"shape\":\"poly\""), "{json}");
        assert!(json.contains("overlays"), "{json}");
    }

    #[test]
    fn a_circle_can_swallow_the_shapes_inside_it() {
        let big = ZoneShape::circle((0.0, 0.0), 500.0).unwrap();
        assert!(big.covers(&ZoneShape::circle((100.0, 0.0), 300.0).unwrap()));
        assert!(!big.covers(&ZoneShape::circle((100.0, 0.0), 450.0).unwrap()));
        assert!(big.covers(&ZoneShape::rect(Aabb::new(-100.0, -100.0, 100.0, 100.0)).unwrap()));
        assert!(!big.covers(&ZoneShape::rect(Aabb::new(-600.0, -100.0, 100.0, 100.0)).unwrap()));
    }

    #[test]
    fn coverage_subtracts_a_higher_priority_overlay() {
        let ls = ZoneCoverage::new(vec![city()], vec![arena()]);
        // Well inside Los Santos, nowhere near the arena.
        assert!(ls.contains(500.0, 500.0));
        // Inside Los Santos geometrically, but the arena owns this ground.
        assert!(city().contains(0.0, 0.0));
        assert!(!ls.contains(0.0, 0.0));
    }

    /// The bug this whole structure exists to prevent: without the overlay in
    /// the distance calculation, a player driving into the arena is 500m from
    /// any Los Santos edge and no handoff is ever prepared.
    #[test]
    fn approaching_an_overlay_reads_as_approaching_an_edge() {
        let ls = ZoneCoverage::new(vec![city()], vec![arena()]);
        // 200m from the arena rim, 800m from the nearest city edge.
        let distance = ls.signed_distance_to_edge(-300.0, 0.0);
        assert_eq!(distance, 200.0, "the arena rim is the nearest edge");

        let without_overlay = ZoneCoverage::new(vec![city()], Vec::new());
        assert_eq!(without_overlay.signed_distance_to_edge(-300.0, 0.0), 700.0);
    }

    #[test]
    fn inside_an_overlay_reads_as_outside_our_territory() {
        let ls = ZoneCoverage::new(vec![city()], vec![arena()]);
        assert!(ls.signed_distance_to_edge(0.0, 0.0) < 0.0);
    }

    #[test]
    fn a_zone_can_own_several_disjoint_pieces() {
        let downtown = ZoneShape::rect(Aabb::new(0.0, 0.0, 100.0, 100.0)).unwrap();
        let docks = ZoneShape::rect(Aabb::new(500.0, 500.0, 600.0, 600.0)).unwrap();
        let city = ZoneCoverage::new(vec![downtown, docks], Vec::new());
        assert!(city.contains(50.0, 50.0));
        assert!(city.contains(550.0, 550.0));
        assert!(!city.contains(300.0, 300.0));
        assert_eq!(city.bbox(), Some(Aabb::new(0.0, 0.0, 600.0, 600.0)));
    }

    #[test]
    fn from_bounds_matches_the_rectangle_it_replaces() {
        let bounds = Aabb::new(-4000.0, -4000.0, 0.0, 4000.0);
        let coverage = ZoneCoverage::from_bounds(bounds);
        assert!(coverage.contains(-500.0, 200.0));
        assert!(!coverage.contains(500.0, 200.0));
        assert_eq!(coverage.signed_distance_to_edge(-500.0, 0.0), 500.0);
    }
}
