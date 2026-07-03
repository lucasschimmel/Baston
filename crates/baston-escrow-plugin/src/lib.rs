//! `baston-escrow-plugin` — optional CFX Asset Escrow support for BASTON.
//!
//! The BASTON core stays free of any `svadhesive.dll` dependency. This crate
//! implements [`baston_core::script_decryptor::ScriptDecryptor`] and installs
//! itself into a running [`baston_zone::resource_loader::ResourceManager`] via
//! [`install`].
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
use baston_zone::resource_loader::ResourceManager;

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

/// Build the escrow decryptor for `config` without installing it. Useful when
/// the caller wants to own the `Arc` (e.g. to also register metrics).
pub fn build_decryptor(
    config: &EscrowConfig,
) -> Result<Arc<dyn ScriptDecryptor>, EscrowPluginError> {
    match &config.backend {
        EscrowBackend::Direct { .. } => Err(EscrowPluginError::DirectBackendUnsupported),
        EscrowBackend::Sidecar {
            fxserver_path,
            resources_dir,
        } => {
            let decryptor = SidecarDecryptor::start(fxserver_path, resources_dir)?;
            Ok(Arc::new(decryptor))
        }
    }
}

/// Install the escrow plugin into a running [`ResourceManager`].
///
/// Call after `ResourceManager::new()` and before the first resource start.
/// `server_license` is accepted for API completeness; the sidecar backend does
/// not need it (the FXServer subprocess holds the server licence itself).
pub fn install(
    manager: &Arc<ResourceManager>,
    config: EscrowConfig,
) -> Result<(), EscrowPluginError> {
    let _ = &config.server_license;
    let decryptor = build_decryptor(&config)?;
    manager.set_script_decryptor(decryptor);
    tracing::info!(backend = ?config.backend, "baston-escrow-plugin installed");
    Ok(())
}
