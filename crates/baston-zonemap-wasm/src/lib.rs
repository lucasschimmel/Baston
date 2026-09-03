//! WebAssembly ABI over [`baston_zonemap`], for the map editor.
//!
//! The editor has to refuse exactly what the Gateway refuses at boot. Two
//! implementations of "is this map valid" would drift, and a validator that
//! drifts is worse than none: it tells you a map is fine and the server tells
//! you otherwise after you have drawn the whole thing. So the editor runs this
//! crate's compilation of the real one.
//!
//! Deliberately no `wasm-bindgen`: it would pin a `wasm-bindgen-cli` version to
//! install and keep in step, for an interface of five functions. A raw C ABI
//! over linear memory needs `cargo build --target wasm32-unknown-unknown` and
//! nothing else.
//!
//! # Protocol
//!
//! Strings cross as UTF-8 bytes in linear memory. The caller allocates with
//! [`bz_alloc`], writes, calls, and frees with [`bz_free`]. Results are held
//! in a module-level buffer that stays valid until the next call that writes
//! one, which is safe because WebAssembly here is single-threaded.

use std::cell::RefCell;

use baston_zonemap::ZoneMap;
use serde::Serialize;

thread_local! {
    /// The parsed map, so hit-testing does not re-parse the document on every
    /// mouse move.
    static MAP: RefCell<Option<ZoneMap>> = const { RefCell::new(None) };
    /// The JSON answer of the last call that produced one.
    static RESULT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// What [`bz_load`] answers.
#[derive(Serialize)]
struct LoadReport {
    /// Whether the map is one the Gateway would accept.
    ok: bool,
    /// Why not, in the same words the Gateway uses. `None` when `ok`.
    error: Option<String>,
    /// Valid, but probably not meant — a region an earlier one has hidden.
    warnings: Vec<String>,
    /// The regions, in priority order.
    regions: Vec<RegionReport>,
}

/// A region as the editor needs it: what it is called, and enough geometry to
/// draw it.
///
/// Carrying the geometry back is what keeps a TOML parser out of the editor
/// entirely. It writes documents and reads them back through here, so the only
/// thing that ever interprets a map file is the code the server uses.
#[derive(Serialize)]
struct RegionReport {
    name: String,
    zone: String,
    shape: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    bounds: Option<[f32; 4]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    center: Option<[f32; 2]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    radius: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    points: Option<Vec<[f32; 2]>>,
}

/// The layout every [`bz_alloc`] buffer is made with, so [`bz_free`] gives the
/// allocator back exactly what it was handed.
///
/// A `Vec<u8>` would be the obvious vehicle and is the wrong one:
/// `with_capacity(n)` is allowed to reserve more than `n`, and freeing it as
/// capacity `n` would hand the allocator a size it never issued.
fn layout(len: usize) -> Option<std::alloc::Layout> {
    (len > 0)
        .then(|| std::alloc::Layout::from_size_align(len, 1).ok())
        .flatten()
}

/// Reserve `len` bytes for the caller to write a UTF-8 string into. Null if
/// the request cannot be served.
///
/// # Safety
/// The returned pointer must be handed back to [`bz_free`] with the same
/// `len`.
#[no_mangle]
pub extern "C" fn bz_alloc(len: usize) -> *mut u8 {
    match layout(len) {
        // SAFETY: the layout is non-zero-sized, which is `alloc`'s only
        // requirement. A null return is the documented failure and the caller
        // checks it.
        Some(layout) => unsafe { std::alloc::alloc(layout) },
        None => std::ptr::null_mut(),
    }
}

/// Release a buffer obtained from [`bz_alloc`].
///
/// # Safety
/// `ptr` must come from [`bz_alloc`] with the same `len`, and must not have
/// been freed already.
#[no_mangle]
pub unsafe extern "C" fn bz_free(ptr: *mut u8, len: usize) {
    if let (false, Some(layout)) = (ptr.is_null(), layout(len)) {
        std::alloc::dealloc(ptr, layout);
    }
}

/// Parse and validate a map document, and keep it for [`bz_region_at`].
///
/// Returns the byte length of the JSON [`LoadReport`], which the caller then
/// reads from [`bz_result_ptr`]. An invalid document still produces a report —
/// the error is the answer, not a failure to answer.
///
/// # Safety
/// `ptr`/`len` must describe a readable buffer of UTF-8 bytes.
#[no_mangle]
pub unsafe extern "C" fn bz_load(ptr: *const u8, len: usize) -> usize {
    let text = match std::str::from_utf8(std::slice::from_raw_parts(ptr, len)) {
        Ok(text) => text,
        Err(e) => return store(&LoadReport::failed(format!("not valid UTF-8: {e}"))),
    };

    match ZoneMap::parse(text) {
        Ok((map, warnings)) => {
            let report = LoadReport {
                ok: true,
                error: None,
                warnings,
                regions: map.regions().iter().map(RegionReport::from).collect(),
            };
            MAP.with(|slot| *slot.borrow_mut() = Some(map));
            store(&report)
        }
        Err(e) => {
            // A map that will not load must not leave the previous one behind
            // to answer hit tests about a document that is no longer on screen.
            MAP.with(|slot| *slot.borrow_mut() = None);
            store(&LoadReport::failed(e.to_string()))
        }
    }
}

/// Pointer to the JSON written by the last [`bz_load`].
#[no_mangle]
pub extern "C" fn bz_result_ptr() -> *const u8 {
    RESULT.with(|buf| buf.borrow().as_ptr())
}

/// Index of the region owning `(x, y)`, or `-1` if none does.
///
/// This is the whole reason the editor speaks to WebAssembly at all: the
/// "first region that contains it" rule is answered by the code the server
/// runs, not by a second implementation in JavaScript.
#[no_mangle]
pub extern "C" fn bz_region_at(x: f32, y: f32) -> i32 {
    MAP.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(|map| map.region_at(x, y))
            .map_or(-1, |index| index as i32)
    })
}

impl LoadReport {
    fn failed(error: String) -> Self {
        Self {
            ok: false,
            error: Some(error),
            warnings: Vec::new(),
            regions: Vec::new(),
        }
    }
}

impl From<&baston_zonemap::Region> for RegionReport {
    fn from(region: &baston_zonemap::Region) -> Self {
        use baston_zonemap::ZoneShape;
        let mut report = Self {
            name: region.name.clone(),
            zone: region.zone.clone(),
            shape: "everywhere",
            bounds: None,
            center: None,
            radius: None,
            points: None,
        };
        match &region.shape {
            ZoneShape::Rect(b) => {
                report.shape = "rect";
                report.bounds = Some([b.x_min, b.y_min, b.x_max, b.y_max]);
            }
            ZoneShape::Circle { center, radius } => {
                report.shape = "circle";
                report.center = Some([center.0, center.1]);
                report.radius = Some(*radius);
            }
            ZoneShape::Poly(poly) => {
                report.shape = "poly";
                report.points = Some(poly.points().iter().map(|&(x, y)| [x, y]).collect());
            }
            ZoneShape::Everywhere => {}
        }
        report
    }
}

/// Serialize into the result buffer and return its length.
fn store(report: &LoadReport) -> usize {
    // Serializing a report cannot fail; if it somehow did, an empty object is
    // still parseable by the caller and reports nothing rather than hanging it.
    let json = serde_json::to_vec(report).unwrap_or_else(|_| b"{}".to_vec());
    let len = json.len();
    RESULT.with(|buf| *buf.borrow_mut() = json);
    len
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the ABI the way the editor does: allocate, write, call, read.
    fn load(text: &str) -> serde_json::Value {
        let bytes = text.as_bytes();
        let ptr = bz_alloc(bytes.len().max(1));
        assert!(!ptr.is_null(), "allocation refused");
        let len = unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
            bz_load(ptr, bytes.len())
        };
        let json = unsafe { std::slice::from_raw_parts(bz_result_ptr(), len) };
        let value = serde_json::from_slice(json).expect("the report must be JSON");
        unsafe { bz_free(ptr, bytes.len().max(1)) };
        value
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

    #[test]
    fn a_valid_map_reports_its_regions_in_priority_order() {
        let report = load(ARENA_MAP);
        assert_eq!(report["ok"], true);
        assert!(report["error"].is_null());
        assert_eq!(report["warnings"].as_array().unwrap().len(), 0);
        let regions = report["regions"].as_array().unwrap();
        assert_eq!(regions.len(), 3);
        assert_eq!(regions[0]["name"], "arena");
        assert_eq!(regions[0]["shape"], "circle");
        assert_eq!(regions[2]["shape"], "everywhere");
    }

    /// The editor draws from this and never parses TOML itself, so every shape
    /// has to come back with the geometry needed to render and edit it.
    #[test]
    fn every_shape_comes_back_with_its_geometry() {
        let report = load(
            r#"
[[region]]
zone = "z"
shape = "circle"
center = [1.0, 2.0]
radius = 30.0

[[region]]
zone = "z"
shape = "rect"
bounds = [-10.0, -20.0, 30.0, 40.0]

[[region]]
zone = "z"
shape = "poly"
points = [[0.0, 0.0], [10.0, 0.0], [10.0, 10.0]]

[[region]]
zone = "rest"
shape = "everywhere"
"#,
        );
        let r = report["regions"].as_array().unwrap();
        assert_eq!(r[0]["center"], serde_json::json!([1.0, 2.0]));
        assert_eq!(r[0]["radius"], 30.0);
        assert_eq!(
            r[1]["bounds"],
            serde_json::json!([-10.0, -20.0, 30.0, 40.0])
        );
        assert_eq!(
            r[2]["points"],
            serde_json::json!([[0.0, 0.0], [10.0, 0.0], [10.0, 10.0]])
        );
        // The catch-all has no geometry, and says so by omission.
        assert!(r[3].get("bounds").is_none());
        assert!(r[3].get("points").is_none());
    }

    #[test]
    fn hit_testing_answers_with_the_region_that_wins() {
        load(ARENA_MAP);
        assert_eq!(bz_region_at(0.0, 0.0), 0, "the arena outranks the city");
        assert_eq!(bz_region_at(500.0, 500.0), 1);
        assert_eq!(bz_region_at(9999.0, 9999.0), 2, "the catch-all");
    }

    /// The editor must refuse what the Gateway refuses, in the same words.
    #[test]
    fn an_invalid_map_reports_the_gateways_own_error() {
        let report = load(
            r#"
[[region]]
zone = "zone-a"
shape = "rect"
bounds = [0.0, 0.0, 100.0, 100.0]
"#,
        );
        assert_eq!(report["ok"], false);
        let error = report["error"].as_str().unwrap();
        assert!(error.contains("belongs to no zone"), "{error}");
    }

    #[test]
    fn a_shadowed_region_comes_back_as_a_warning_on_a_valid_map() {
        let report = load(
            r#"
[[region]]
name = "los-santos"
zone = "zone-city"
shape = "rect"
bounds = [-1000.0, -1000.0, 1000.0, 1000.0]

[[region]]
name = "arena"
zone = "zone-arena"
shape = "circle"
center = [0.0, 0.0]
radius = 100.0

[[region]]
zone = "zone-country"
shape = "everywhere"
"#,
        );
        assert_eq!(report["ok"], true, "a hidden region is legal, just useless");
        let warnings = report["warnings"].as_array().unwrap();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].as_str().unwrap().contains("arena"));
    }

    /// A document that fails to parse must not leave the previous map behind
    /// to answer questions about something no longer on screen.
    #[test]
    fn a_failed_load_forgets_the_map_it_replaced() {
        load(ARENA_MAP);
        assert_eq!(bz_region_at(0.0, 0.0), 0);
        load("[[region]]\nshape = \"hexagon\"\nzone = \"z\"\n");
        assert_eq!(bz_region_at(0.0, 0.0), -1);
    }

    #[test]
    fn a_point_outside_a_map_without_a_catch_all_belongs_to_nobody() {
        // Such a map never loads, so there is nothing to hit-test against.
        load("");
        assert_eq!(bz_region_at(0.0, 0.0), -1);
    }
}
