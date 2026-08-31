//! Shared types exchanged between BASTON components (gateway, zone, scripting).

pub mod connection;
pub mod debug_info;
pub mod entity;
pub mod events;
pub mod native;
pub mod player_snapshot;
pub mod players;
pub mod rage;
pub mod spatial;
pub mod udp;
pub mod zone_map;

/// gRPC types + client/server stubs generated from `proto/baston.proto`
/// by `tonic-build` (never hand-written).
pub mod mesh {
    // tonic returns every RPC as `Result<Response<T>, tonic::Status>`, and
    // `Status` is 176 bytes — which `result_large_err` flags from Rust 1.98 on.
    // The shape belongs to tonic, not to us, and the file is regenerated on
    // every build, so the only alternatives are boxing tonic's own error type
    // or pinning the toolchain. Scoped to this module so the lint stays live
    // for code we actually write.
    #![allow(clippy::result_large_err)]

    tonic::include_proto!("baston");
}

pub use player_snapshot::PlayerStateSnapshot;
pub use players::PlayerDirectory;
pub use spatial::{Aabb, ZoneCoverage, ZoneShape};
pub use zone_map::{MapError, Region, ZoneMap};

impl From<Aabb> for mesh::BoundingBox {
    fn from(a: Aabb) -> Self {
        Self {
            x_min: a.x_min,
            x_max: a.x_max,
            y_min: a.y_min,
            y_max: a.y_max,
        }
    }
}

impl From<mesh::BoundingBox> for Aabb {
    fn from(b: mesh::BoundingBox) -> Self {
        Aabb::new(b.x_min, b.y_min, b.x_max, b.y_max)
    }
}

impl From<&ZoneShape> for mesh::Shape {
    fn from(shape: &ZoneShape) -> Self {
        use mesh::shape::Kind;
        let mut wire = mesh::Shape::default();
        match shape {
            ZoneShape::Rect(bounds) => {
                wire.kind = Kind::Rect as i32;
                wire.rect = Some((*bounds).into());
            }
            ZoneShape::Circle { center, radius } => {
                wire.kind = Kind::Circle as i32;
                wire.center = Some(mesh::Point2 {
                    x: center.0,
                    y: center.1,
                });
                wire.radius = *radius;
            }
            ZoneShape::Poly(poly) => {
                wire.kind = Kind::Poly as i32;
                wire.points = poly
                    .points()
                    .iter()
                    .map(|&(x, y)| mesh::Point2 { x, y })
                    .collect();
            }
            ZoneShape::Everywhere => wire.kind = Kind::Everywhere as i32,
        }
        wire
    }
}

impl TryFrom<mesh::Shape> for ZoneShape {
    type Error = String;

    fn try_from(wire: mesh::Shape) -> Result<Self, Self::Error> {
        use mesh::shape::Kind;
        let kind =
            Kind::try_from(wire.kind).map_err(|_| format!("unknown shape kind {}", wire.kind))?;
        let shape = match kind {
            Kind::Rect => ZoneShape::rect(wire.rect.ok_or("rect shape without bounds")?.into()),
            Kind::Circle => {
                let c = wire.center.ok_or("circle shape without a centre")?;
                ZoneShape::circle((c.x, c.y), wire.radius)
            }
            Kind::Poly => ZoneShape::poly(wire.points.into_iter().map(|p| (p.x, p.y)).collect()),
            Kind::Everywhere => Ok(ZoneShape::Everywhere),
            Kind::Unspecified => return Err("shape kind is unset".to_owned()),
        };
        shape.map_err(|e| e.to_string())
    }
}

impl From<&ZoneCoverage> for mesh::Coverage {
    fn from(coverage: &ZoneCoverage) -> Self {
        Self {
            shapes: coverage.shapes().iter().map(Into::into).collect(),
            overlays: coverage.overlays().iter().map(Into::into).collect(),
        }
    }
}

impl TryFrom<mesh::Coverage> for ZoneCoverage {
    type Error = String;

    fn try_from(wire: mesh::Coverage) -> Result<Self, Self::Error> {
        let convert = |shapes: Vec<mesh::Shape>| {
            shapes
                .into_iter()
                .map(ZoneShape::try_from)
                .collect::<Result<Vec<_>, _>>()
        };
        Ok(ZoneCoverage::new(
            convert(wire.shapes)?,
            convert(wire.overlays)?,
        ))
    }
}

use serde::{Deserialize, Serialize};

/// Strongly-typed resource name (avoids passing raw `String`s around).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ResourceName(pub String);

impl ResourceName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ResourceName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ResourceName {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

/// A connected (or connecting) player, as tracked by the gateway.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerInfo {
    /// FiveM-style numeric source id, unique per session.
    pub source: u32,
    pub name: String,
    /// FiveM identifier strings, e.g. `ip:127.0.0.1`. Phase A only has `ip:`.
    pub identifiers: Vec<String>,
}

/// A resource manifest (`manifest.json`), BASTON's JSON replacement for `fxmanifest.lua`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceManifest {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub server_scripts: Vec<String>,
    #[serde(default)]
    pub client_scripts: Vec<String>,
    #[serde(default)]
    pub files: Vec<String>,
}
