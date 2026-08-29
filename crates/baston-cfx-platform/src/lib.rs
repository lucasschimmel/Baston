//! Official CFX platform integration boundary for Baston.
//!
//! This crate hosts a genuine, user-supplied FXServer process and consumes its
//! local licence verdict. It does not load `svadhesive.dll` directly and does
//! not reimplement CFX's private authentication protocol.

mod error;
mod license;
mod policy;
mod sidecar;

pub use error::CfxPlatformError;
pub use license::LicenseOracle;
pub use policy::{PolicyClient, PolicyResolution, PolicySource};
pub use sidecar::{PublicListing, Sidecar, SidecarParams, SHIM_RESOURCE};
