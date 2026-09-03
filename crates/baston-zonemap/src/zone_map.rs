//! The zone map: an ordered list of regions, read from its own TOML file.
//!
//! The owner of a point is the **first** region that contains it. Order is
//! priority, and it is the array's order rather than a numeric field on
//! purpose: a number invites ties, and a tie would have to be broken by
//! something — insertion order, a hash iteration — that is not stable between
//! runs. An array is a total order by construction, so there is no tie-break
//! to write and none to get wrong.
//!
//! The last region must match everything. That is what turns a hole in the map
//! from "unlikely if the operator was careful" into "impossible".

use std::collections::BTreeSet;
use std::path::Path;

use serde::Deserialize;

use crate::spatial::{Aabb, ShapeError, ZoneCoverage, ZoneShape};

/// A claimed area, and the zone claiming it. One zone may claim several.
#[derive(Debug, Clone)]
pub struct Region {
    pub name: String,
    pub zone: String,
    pub shape: ZoneShape,
}

/// Why a map file could not be loaded.
#[derive(Debug)]
pub enum MapError {
    Io {
        path: String,
        source: std::io::Error,
    },
    Syntax(String),
    /// No regions at all — an empty map owns nothing.
    Empty,
    /// The last region is not `everywhere`, so parts of the plane belong to no
    /// zone and any player reaching them loses their owner.
    NoFallback {
        last: String,
    },
    /// An `everywhere` region before the end makes every region after it dead.
    FallbackNotLast {
        name: String,
        index: usize,
    },
    Region {
        name: String,
        problem: String,
    },
}

impl std::fmt::Display for MapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "cannot read zone map {path}: {source}"),
            Self::Syntax(e) => write!(f, "zone map is not valid TOML: {e}"),
            Self::Empty => write!(f, "zone map has no [[region]] entries"),
            Self::NoFallback { last } => write!(
                f,
                "the last region ({last}) is not `shape = \"everywhere\"`, so part of the \
                 map belongs to no zone; add a catch-all region at the end"
            ),
            Self::FallbackNotLast { name, index } => write!(
                f,
                "region {index} ({name}) is `shape = \"everywhere\"` but is not last: \
                 every region after it is unreachable"
            ),
            Self::Region { name, problem } => write!(f, "region {name}: {problem}"),
        }
    }
}

impl std::error::Error for MapError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// An ordered partition of the map plane.
#[derive(Debug, Clone, Default)]
pub struct ZoneMap {
    regions: Vec<Region>,
}

impl ZoneMap {
    /// Read and validate a map file.
    ///
    /// Returns the map plus any non-fatal complaints, which the caller should
    /// log: they are things the author probably did not mean but which still
    /// describe a usable map.
    pub fn load(path: &Path) -> Result<(Self, Vec<String>), MapError> {
        let text = std::fs::read_to_string(path).map_err(|source| MapError::Io {
            path: path.display().to_string(),
            source,
        })?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<(Self, Vec<String>), MapError> {
        let file: MapFile = toml::from_str(text).map_err(|e| MapError::Syntax(e.to_string()))?;
        if file.region.is_empty() {
            return Err(MapError::Empty);
        }

        let last = file.region.len() - 1;
        let mut regions = Vec::with_capacity(file.region.len());
        for (index, spec) in file.region.into_iter().enumerate() {
            let name = spec.display_name(index);
            let shape = spec.to_shape().map_err(|problem| MapError::Region {
                name: name.clone(),
                problem,
            })?;
            let zone = spec.zone.trim().to_owned();
            if zone.is_empty() {
                return Err(MapError::Region {
                    name,
                    problem: "zone id is empty".to_owned(),
                });
            }
            if matches!(shape, ZoneShape::Everywhere) && index != last {
                return Err(MapError::FallbackNotLast { name, index });
            }
            regions.push(Region { name, zone, shape });
        }

        if !matches!(regions[last].shape, ZoneShape::Everywhere) {
            return Err(MapError::NoFallback {
                last: regions[last].name.clone(),
            });
        }

        let map = Self { regions };
        let warnings = map.warnings();
        Ok((map, warnings))
    }

    /// Non-fatal complaints about a valid map.
    fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (i, region) in self.regions.iter().enumerate() {
            if let Some(shadow) = self.regions[..i]
                .iter()
                .find(|earlier| earlier.shape.covers(&region.shape))
            {
                out.push(format!(
                    "region {} (zone {}) is entirely inside {} (zone {}), which comes first: \
                     it will never own anything. Move it above {}.",
                    region.name, region.zone, shadow.name, shadow.zone, shadow.name
                ));
            }
        }
        out
    }

    /// Build the map a deployment without a map file has: one rectangle per
    /// zone, in registration order, and no catch-all — so a point outside
    /// every zone has no owner, exactly as before maps existed.
    pub fn from_declared_bounds(zones: impl IntoIterator<Item = (String, Aabb)>) -> Self {
        Self {
            regions: zones
                .into_iter()
                .map(|(zone, bounds)| Region {
                    name: zone.clone(),
                    zone,
                    shape: ZoneShape::Rect(bounds),
                })
                .collect(),
        }
    }

    pub fn regions(&self) -> &[Region] {
        &self.regions
    }

    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }

    /// Every zone the map mentions, in first-appearance order.
    pub fn zone_ids(&self) -> Vec<String> {
        let mut seen = Vec::new();
        for region in &self.regions {
            if !seen.contains(&region.zone) {
                seen.push(region.zone.clone());
            }
        }
        seen
    }

    /// Owner of a point: the first region containing it that `is_live` accepts.
    ///
    /// The liveness filter is what makes a dead zone fall through to whatever
    /// is underneath it instead of routing players into a process that is not
    /// answering.
    pub fn zone_at(&self, x: f32, y: f32, is_live: impl Fn(&str) -> bool) -> Option<&str> {
        self.regions
            .iter()
            .find(|r| r.shape.contains(x, y) && is_live(&r.zone))
            .map(|r| r.zone.as_str())
    }

    /// Index of the first region containing the point, liveness ignored.
    ///
    /// The same rule [`Self::zone_at`] applies, answering with the region
    /// rather than the zone: the map editor shows *which claim* wins under the
    /// cursor, which is what makes an ordered list legible.
    pub fn region_at(&self, x: f32, y: f32) -> Option<usize> {
        self.regions.iter().position(|r| r.shape.contains(x, y))
    }

    /// What `zone` owns: its own regions, and the higher-priority regions that
    /// overlap them and therefore cut into them.
    pub fn coverage_for(&self, zone: &str) -> ZoneCoverage {
        let mut shapes = Vec::new();
        let mut overlays: BTreeSet<usize> = BTreeSet::new();

        for (i, region) in self.regions.iter().enumerate() {
            if region.zone != zone {
                continue;
            }
            shapes.push(region.shape.clone());
            for (j, earlier) in self.regions[..i].iter().enumerate() {
                if earlier.zone != zone && may_overlap(&earlier.shape, &region.shape) {
                    overlays.insert(j);
                }
            }
        }

        ZoneCoverage::new(
            shapes,
            overlays
                .into_iter()
                .map(|j| self.regions[j].shape.clone())
                .collect(),
        )
    }
}

/// Could these two shapes share any point? Bounding boxes only — a `true` for
/// shapes that do not actually touch costs an extra containment test, never a
/// wrong answer.
fn may_overlap(a: &ZoneShape, b: &ZoneShape) -> bool {
    match (a.bbox(), b.bbox()) {
        (Some(a), Some(b)) => a.intersects(&b),
        // One of them is `everywhere`.
        _ => true,
    }
}

// ---- TOML shapes ----

#[derive(Debug, Deserialize)]
struct MapFile {
    #[serde(default)]
    region: Vec<RegionSpec>,
}

/// One `[[region]]` table, before validation.
///
/// Hand-decoded rather than a `#[serde(flatten)]` enum so that a typo names
/// itself: `deny_unknown_fields` catches `radiuss`, and a rect carrying a
/// `radius` is reported as the confusion it is instead of being ignored.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegionSpec {
    #[serde(default)]
    name: Option<String>,
    zone: String,
    shape: String,
    #[serde(default)]
    bounds: Option<[f32; 4]>,
    #[serde(default)]
    center: Option<[f32; 2]>,
    #[serde(default)]
    radius: Option<f32>,
    #[serde(default)]
    points: Option<Vec<[f32; 2]>>,
}

impl RegionSpec {
    fn display_name(&self, index: usize) -> String {
        match &self.name {
            Some(name) if !name.trim().is_empty() => name.trim().to_owned(),
            _ => format!("#{index} ({})", self.zone),
        }
    }

    /// Keys that belong to a shape other than the one declared.
    fn strays(&self, keep: &[&str]) -> Vec<&'static str> {
        [
            ("bounds", self.bounds.is_some()),
            ("center", self.center.is_some()),
            ("radius", self.radius.is_some()),
            ("points", self.points.is_some()),
        ]
        .into_iter()
        .filter(|(key, present)| *present && !keep.contains(key))
        .map(|(key, _)| key)
        .collect()
    }

    fn to_shape(&self) -> Result<ZoneShape, String> {
        let (shape, keep): (Result<ZoneShape, ShapeError>, &[&str]) =
            match self.shape.trim().to_ascii_lowercase().as_str() {
                "rect" => {
                    let b = self
                        .bounds
                        .ok_or("shape = \"rect\" needs bounds = [x_min, y_min, x_max, y_max]")?;
                    (
                        ZoneShape::rect(Aabb::new(b[0], b[1], b[2], b[3])),
                        &["bounds"],
                    )
                }
                "circle" => {
                    let c = self
                        .center
                        .ok_or("shape = \"circle\" needs center = [x, y]")?;
                    let r = self.radius.ok_or("shape = \"circle\" needs radius")?;
                    (ZoneShape::circle((c[0], c[1]), r), &["center", "radius"])
                }
                "poly" => {
                    let p = self
                        .points
                        .as_ref()
                        .ok_or("shape = \"poly\" needs points = [[x, y], [x, y], ...]")?;
                    (
                        ZoneShape::poly(p.iter().map(|p| (p[0], p[1])).collect()),
                        &["points"],
                    )
                }
                "everywhere" => (Ok(ZoneShape::Everywhere), &[]),
                other => {
                    return Err(format!(
                        "unknown shape {other:?} — use rect, circle, poly or everywhere"
                    ))
                }
            };

        let strays = self.strays(keep);
        if !strays.is_empty() {
            return Err(format!(
                "shape = {:?} does not use {}",
                self.shape,
                strays.join(", ")
            ));
        }
        shape.map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARENA_MAP: &str = r#"
[[region]]
name = "maze-bank-arena"
zone = "zone-arena"
shape = "circle"
center = [-250.0, -2000.0]
radius = 240.0

[[region]]
name = "los-santos"
zone = "zone-city"
shape = "rect"
bounds = [-4000.0, -4000.0, 4000.0, 500.0]

[[region]]
name = "le-reste"
zone = "zone-country"
shape = "everywhere"
"#;

    fn parse(text: &str) -> (ZoneMap, Vec<String>) {
        ZoneMap::parse(text).expect("map should parse")
    }

    fn live(_: &str) -> bool {
        true
    }

    #[test]
    fn load_reads_a_file_and_names_it_when_it_cannot() {
        let dir = std::env::temp_dir().join("baston-zone-map-load-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("map.toml");
        std::fs::write(&path, ARENA_MAP).unwrap();

        let (map, warnings) = ZoneMap::load(&path).expect("a valid file should load");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(map.zone_at(-250.0, -2000.0, live), Some("zone-arena"));

        let missing = dir.join("nope.toml");
        let err = ZoneMap::load(&missing).unwrap_err();
        assert!(matches!(err, MapError::Io { .. }));
        assert!(
            err.to_string().contains("nope.toml"),
            "the error must name the file it looked for: {err}"
        );

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn an_arena_inside_a_city_wins_because_it_comes_first() {
        let (map, warnings) = parse(ARENA_MAP);
        assert!(warnings.is_empty(), "{warnings:?}");
        // Dead centre of the arena, which is also inside the city rect.
        assert_eq!(map.zone_at(-250.0, -2000.0, live), Some("zone-arena"));
        // Just outside the rim, still in the city.
        assert_eq!(map.zone_at(-250.0, -1700.0, live), Some("zone-city"));
        // North of the city rect: the catch-all.
        assert_eq!(map.zone_at(0.0, 2000.0, live), Some("zone-country"));
    }

    #[test]
    fn the_arena_zone_learns_nothing_it_does_not_need() {
        let (map, _) = parse(ARENA_MAP);
        let arena = map.coverage_for("zone-arena");
        assert_eq!(arena.shapes().len(), 1);
        // Nothing outranks the first region.
        assert!(arena.overlays().is_empty());
        assert!(arena.contains(-250.0, -2000.0));
    }

    /// The city must be told about the arena, or a player driving into it is
    /// never handed over.
    #[test]
    fn the_city_is_told_about_the_arena_carved_out_of_it() {
        let (map, _) = parse(ARENA_MAP);
        let city = map.coverage_for("zone-city");
        assert_eq!(city.shapes().len(), 1);
        assert_eq!(city.overlays().len(), 1, "the arena cuts into the city");
        assert!(city.contains(-250.0, -1700.0));
        assert!(!city.contains(-250.0, -2000.0), "the arena owns this");
    }

    #[test]
    fn a_zone_claiming_two_areas_gets_both() {
        let (map, _) = parse(
            r#"
[[region]]
zone = "zone-city"
shape = "rect"
bounds = [0.0, 0.0, 100.0, 100.0]

[[region]]
name = "docks"
zone = "zone-city"
shape = "rect"
bounds = [500.0, 500.0, 600.0, 600.0]

[[region]]
zone = "zone-country"
shape = "everywhere"
"#,
        );
        let city = map.coverage_for("zone-city");
        assert_eq!(city.shapes().len(), 2);
        assert!(city.overlays().is_empty(), "a zone does not overlay itself");
        assert_eq!(map.zone_ids(), vec!["zone-city", "zone-country"]);
    }

    #[test]
    fn a_dead_zone_falls_through_to_what_is_underneath() {
        let (map, _) = parse(ARENA_MAP);
        let arena_down = |zone: &str| zone != "zone-arena";
        assert_eq!(
            map.zone_at(-250.0, -2000.0, arena_down),
            Some("zone-city"),
            "with the arena down the city takes its ground back"
        );
    }

    #[test]
    fn a_map_without_a_catch_all_is_refused() {
        let err = ZoneMap::parse(
            r#"
[[region]]
zone = "zone-a"
shape = "rect"
bounds = [0.0, 0.0, 100.0, 100.0]
"#,
        )
        .unwrap_err();
        assert!(matches!(err, MapError::NoFallback { .. }), "{err}");
        assert!(err.to_string().contains("belongs to no zone"), "{err}");
    }

    #[test]
    fn a_catch_all_in_the_middle_is_refused() {
        let err = ZoneMap::parse(
            r#"
[[region]]
zone = "zone-country"
shape = "everywhere"

[[region]]
name = "arena"
zone = "zone-arena"
shape = "circle"
center = [0.0, 0.0]
radius = 100.0
"#,
        )
        .unwrap_err();
        assert!(matches!(err, MapError::FallbackNotLast { .. }), "{err}");
        assert!(err.to_string().contains("unreachable"), "{err}");
    }

    /// The likeliest authoring mistake: the arena listed after the city that
    /// already covers it. Valid, but certainly not what was meant.
    #[test]
    fn a_region_hidden_behind_an_earlier_one_warns() {
        let (_, warnings) = parse(
            r#"
[[region]]
name = "los-santos"
zone = "zone-city"
shape = "rect"
bounds = [-4000.0, -4000.0, 4000.0, 500.0]

[[region]]
name = "maze-bank-arena"
zone = "zone-arena"
shape = "circle"
center = [-250.0, -2000.0]
radius = 240.0

[[region]]
zone = "zone-country"
shape = "everywhere"
"#,
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("maze-bank-arena"), "{warnings:?}");
        assert!(warnings[0].contains("never own anything"), "{warnings:?}");
    }

    /// The same warning, with the city traced rather than boxed — which is the
    /// shape anyone drawing a real city uses, and the case a corners-only
    /// containment test would have missed.
    #[test]
    fn a_region_hidden_behind_a_traced_outline_warns_too() {
        let (_, warnings) = parse(
            r#"
[[region]]
name = "los-santos"
zone = "zone-city"
shape = "poly"
points = [
  [-1000.0, -1000.0], [1000.0, -1000.0],
  [1000.0, 1000.0], [-1000.0, 1000.0],
]

[[region]]
name = "maze-bank-arena"
zone = "zone-arena"
shape = "circle"
center = [0.0, 0.0]
radius = 240.0

[[region]]
zone = "zone-country"
shape = "everywhere"
"#,
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("maze-bank-arena"), "{warnings:?}");
        assert!(warnings[0].contains("los-santos"), "{warnings:?}");
    }

    #[test]
    fn a_polygon_region_follows_its_outline() {
        let (map, _) = parse(
            r#"
[[region]]
name = "l-shaped-city"
zone = "zone-city"
shape = "poly"
points = [
  [0.0, 0.0], [100.0, 0.0], [100.0, 40.0],
  [40.0, 40.0], [40.0, 100.0], [0.0, 100.0],
]

[[region]]
zone = "zone-country"
shape = "everywhere"
"#,
        );
        assert_eq!(map.zone_at(20.0, 20.0, live), Some("zone-city"));
        // In the notch: inside the bounding box, outside the outline.
        assert_eq!(map.zone_at(80.0, 80.0, live), Some("zone-country"));
    }

    #[test]
    fn a_key_belonging_to_another_shape_is_refused() {
        let err = ZoneMap::parse(
            r#"
[[region]]
name = "confused"
zone = "zone-a"
shape = "rect"
bounds = [0.0, 0.0, 100.0, 100.0]
radius = 50.0
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("does not use radius"), "{err}");
    }

    #[test]
    fn a_misspelled_key_names_itself() {
        let err = ZoneMap::parse(
            r#"
[[region]]
zone = "zone-a"
shape = "circle"
center = [0.0, 0.0]
radiuss = 50.0
"#,
        )
        .unwrap_err();
        assert!(matches!(err, MapError::Syntax(_)), "{err}");
        assert!(err.to_string().contains("radiuss"), "{err}");
    }

    #[test]
    fn a_missing_key_says_which_one() {
        let err = ZoneMap::parse(
            r#"
[[region]]
name = "no-radius"
zone = "zone-a"
shape = "circle"
center = [0.0, 0.0]
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("needs radius"), "{err}");
        assert!(err.to_string().contains("no-radius"), "{err}");
    }

    #[test]
    fn an_unknown_shape_lists_the_ones_that_exist() {
        let err = ZoneMap::parse(
            r#"
[[region]]
zone = "zone-a"
shape = "hexagon"
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("rect, circle, poly"), "{err}");
    }

    #[test]
    fn an_empty_map_is_refused() {
        assert!(matches!(ZoneMap::parse("").unwrap_err(), MapError::Empty));
    }

    #[test]
    fn declared_bounds_build_a_map_with_no_catch_all() {
        let map = ZoneMap::from_declared_bounds([
            (
                "zone-a".to_owned(),
                Aabb::new(-4000.0, -4000.0, 0.0, 4000.0),
            ),
            ("zone-b".to_owned(), Aabb::new(0.0, -4000.0, 4000.0, 4000.0)),
        ]);
        assert_eq!(map.zone_at(-500.0, 200.0, live), Some("zone-a"));
        assert_eq!(map.zone_at(1500.0, -300.0, live), Some("zone-b"));
        // No fallback: off-map is nobody's, as it has always been.
        assert_eq!(map.zone_at(5000.0, 5000.0, live), None);
    }
}
