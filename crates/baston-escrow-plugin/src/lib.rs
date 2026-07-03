//! `baston-escrow-plugin` — optional CFX Asset Escrow support for BASTON.
//!
//! The BASTON core stays free of any `svadhesive.dll` dependency. This crate
//! implements [`baston_core::script_decryptor::ScriptDecryptor`]; the caller
//! obtains one via [`build_decryptor`] and installs it into its
//! `ResourceManager` with `set_script_decryptor`. The plugin depends only on
//! `baston-core` (never on `baston-zone`), so the composition-root binary can
//! depend on the plugin without a dependency cycle.
//!
//! ## Backend
//!
//! Preliminary research (see the phase D-bis impl notes) found that
//! `svadhesive.dll` exposes only an opaque CitizenFX component (single
//! `CreateComponent` export) — there is no flat decrypt symbol to call over
//! FFI. The **sidecar** backend is therefore the supported one: a minimal
//! FXServer subprocess decrypts escrow resources through svadhesive's own VFS
//! hook and streams the plaintext back. The `direct` (FFI) backend is rejected
//! with an actionable error.

mod error;
mod sidecar;

use std::path::PathBuf;
use std::sync::Arc;

pub use error::EscrowPluginError;
pub use sidecar::SidecarDecryptor;

use baston_core::script_decryptor::ScriptDecryptor;

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

/// Build the escrow decryptor for `config`.
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
            let decryptor = SidecarDecryptor::start(fxserver_path, resources_dir)?;
            tracing::info!(backend = ?config.backend, "baston-escrow decryptor built");
            Ok(Arc::new(decryptor))
        }
    }
}
