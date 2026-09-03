//! Zone geometry and the ordered region map that decides which zone owns a
//! point of the world.
//!
//! Split out of `baston-protocol` so it can be compiled to WebAssembly: the
//! map editor has to enforce exactly the rules the Gateway enforces at boot,
//! and the only way for the two to agree forever is for them to be the same
//! code. `baston-protocol` carries tonic, prost and a C lz4, none of which
//! reach `wasm32-unknown-unknown`; this crate carries serde and toml.
//!
//! Nothing here does I/O beyond reading a map file, and nothing here knows
//! about the wire. The protocol crate re-exports these types and owns the
//! conversions to and from protobuf.

pub mod spatial;
pub mod zone_map;

pub use spatial::{Aabb, Polygon, PolygonError, ShapeError, ZoneCoverage, ZoneShape};
pub use zone_map::{MapError, Region, ZoneMap};
