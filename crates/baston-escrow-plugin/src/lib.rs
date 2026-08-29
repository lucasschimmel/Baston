//! `baston-escrow-plugin` — optional integration with the genuine, unmodified
//! CFX server component (`svadhesive`) for BASTON.
//!
//! The BASTON core stays free of any `svadhesive.dll`/FXServer dependency. This
//! crate drives an FXServer **sidecar** (the component's native host) and exposes
//! two capabilities over it:
//!
//! - **Licence verdict** — [`LicenseOracle`] reads the CFX server-licence status
//!   the component exposes locally (`sv_licenseKeyToken`), for a fail-closed boot
//!   gate and restrictive entitlement enforcement.
//! - **Escrow decryption** — [`SidecarDecryptor`] implements
//!   [`baston_core::script_decryptor::ScriptDecryptor`]; install it into the
//!   `ResourceManager` with `set_script_decryptor`.
//!
//! ## One process for both
//!
//! [`SidecarHandle`] starts a single FXServer sidecar (via [`SidecarParams`]) and
//! hands out both a decryptor and an oracle backed by it — we never boot a second
//! FXServer.
//!
//! ## Compliance boundary
//!
//! BASTON never talks to a CFX service and never reimplements or spoofs the
//! licence protocol. The sidecar is the genuine FXServer, running unmodified,
//! doing exactly what it normally does with the operator's own licence; BASTON
//! only reads the local result and enforces it restrictively. See
//! `docs/licensing.md`.
//!
//! ## Backend
//!
//! `svadhesive.dll` exposes only an opaque CitizenFX component (single
//! `CreateComponent` export) — there is no flat symbol to call over FFI. The
//! **sidecar** backend is therefore the only supported one; the `direct` (FFI)
//! backend is rejected with an actionable error at the composition root.
//!
//! Platform operations now return [`CfxPlatformError`] directly; escrow
//! operations retain [`EscrowPluginError`]. This crate is pre-1.0, so the split
//! intentionally replaces the former mixed error enum with domain-specific
//! errors.

mod decryptor;
mod error;

use std::sync::Arc;

pub use baston_cfx_platform::{
    CfxPlatformError, LicenseOracle, Sidecar, SidecarParams, SHIM_RESOURCE,
};
pub use decryptor::SidecarDecryptor;
pub use error::EscrowPluginError;

use baston_core::script_decryptor::ScriptDecryptor;

/// A single genuine-FXServer sidecar, shared by licence and escrow.
///
/// Start it once with [`SidecarParams`], then obtain a [`LicenseOracle`] and/or a
/// [`SidecarDecryptor`] from it; both hold a reference to the same process, so it
/// stays alive as long as either is in use and is killed when the last reference
/// drops.
pub struct SidecarHandle {
    sidecar: Arc<Sidecar>,
}

impl SidecarHandle {
    /// Start the shared FXServer sidecar.
    pub fn start(params: &SidecarParams) -> Result<Self, EscrowPluginError> {
        Ok(Self {
            sidecar: Sidecar::start(params)?,
        })
    }

    /// A licence oracle backed by this sidecar.
    pub fn oracle(&self) -> LicenseOracle {
        LicenseOracle::from_sidecar(Arc::clone(&self.sidecar))
    }

    /// A decryptor backed by this sidecar, ready for `set_script_decryptor`.
    pub fn decryptor(&self) -> Arc<dyn ScriptDecryptor> {
        Arc::new(SidecarDecryptor::new(Arc::clone(&self.sidecar)))
    }
}
