//! `baston-escrow-plugin` — optional integration with the genuine, unmodified
//! CFX server component (`svadhesive`) for BASTON.
//!
//! The BASTON core stays free of any `svadhesive.dll`/FXServer dependency. This
//! crate drives an FXServer **sidecar** (the component's native host) and
//! exposes two capabilities over it:
//!
//! - **Escrow decryption** — [`SidecarDecryptor`] implements
//!   [`baston_core::script_decryptor::ScriptDecryptor`]; install it into the
//!   `ResourceManager` with `set_script_decryptor`.
//! - **Licence verdict** — [`LicenseOracle`] reads the CFX server-licence status
//!   the component exposes locally (`sv_licenseKeyToken`), for a fail-closed boot
//!   gate and restrictive entitlement enforcement.
//!
//! ## One process for both
//!
//! [`SidecarHandle`] starts a single FXServer sidecar and hands out both a
//! decryptor and an oracle backed by it — we never boot a second FXServer.
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
//! **sidecar** backend is therefore the supported one; the `direct` (FFI)
//! backend is rejected with an actionable error.

mod decryptor;
mod error;
mod license;
mod sidecar;

use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use decryptor::SidecarDecryptor;
pub use error::EscrowPluginError;
pub use license::LicenseOracle;
pub use sidecar::Sidecar;

use baston_core::script_decryptor::ScriptDecryptor;

/// A single genuine-FXServer sidecar, shared by escrow and licence.
///
/// Start it once, then obtain a [`SidecarDecryptor`] and/or a [`LicenseOracle`]
/// from it; both hold a reference to the same process, so it stays alive as long
/// as either is in use and is killed when the last reference drops.
pub struct SidecarHandle {
    sidecar: Arc<Sidecar>,
}

impl SidecarHandle {
    /// Start the shared FXServer sidecar.
    pub fn start(fxserver_path: &Path, resources_dir: &Path) -> Result<Self, EscrowPluginError> {
        Ok(Self {
            sidecar: Sidecar::start(fxserver_path, resources_dir)?,
        })
    }

    /// A decryptor backed by this sidecar, ready for `set_script_decryptor`.
    pub fn decryptor(&self) -> Arc<dyn ScriptDecryptor> {
        Arc::new(SidecarDecryptor::new(Arc::clone(&self.sidecar)))
    }

    /// A licence oracle backed by this sidecar.
    pub fn oracle(&self) -> LicenseOracle {
        LicenseOracle::new(Arc::clone(&self.sidecar))
    }
}

/// Which decryption backend to use.
#[derive(Debug, Clone)]
pub enum EscrowBackend {
    /// FFI into `svadhesive.dll` — not supported (see crate docs).
    Direct { dll_path: PathBuf },
    /// FXServer subprocess sidecar (the supported backend).
    Sidecar {
        fxserver_path: PathBuf,
        resources_dir: PathBuf,
    },
}

/// Plugin configuration, translated from the `[escrow]` section of `baston.toml`.
#[derive(Debug, Clone)]
pub struct EscrowConfig {
    pub backend: EscrowBackend,
    pub server_license: String,
}

/// Build the escrow decryptor for `config` (escrow-only path — starts its own
/// sidecar). When licence verification is also enabled, the composition root
/// should instead start one [`SidecarHandle`] and share it.
///
/// The caller installs the returned `Arc` into its `ResourceManager` via
/// `set_script_decryptor`. `server_license` is accepted for API completeness;
/// the sidecar backend does not need it (the FXServer subprocess holds the
/// server licence itself).
pub fn build_decryptor(
    config: &EscrowConfig,
) -> Result<Arc<dyn ScriptDecryptor>, EscrowPluginError> {
    let _ = &config.server_license;
    match &config.backend {
        EscrowBackend::Direct { .. } => Err(EscrowPluginError::DirectBackendUnsupported),
        EscrowBackend::Sidecar {
            fxserver_path,
            resources_dir,
        } => {
            // The returned decryptor holds an `Arc<Sidecar>`; the handle can drop
            // while the process stays alive via that reference.
            let handle = SidecarHandle::start(fxserver_path, resources_dir)?;
            tracing::info!(backend = ?config.backend, "baston-escrow decryptor built");
            Ok(handle.decryptor())
        }
    }
}
