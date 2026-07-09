//! Error type for the escrow plugin.

use thiserror::Error;

/// Errors produced while installing or running the escrow/licence sidecar.
#[derive(Debug, Error)]
pub enum EscrowPluginError {
    #[error(
        "the 'direct' (FFI) escrow backend is not supported: svadhesive.dll exposes only an \
         opaque CitizenFX component (single `CreateComponent` export, no flat decrypt symbol). \
         Use backend = \"sidecar\" instead."
    )]
    DirectBackendUnsupported,

    #[error("failed to spawn FXServer sidecar: {0}")]
    SidecarSpawn(String),

    #[error("sidecar did not become ready within the startup timeout")]
    SidecarStartTimeout,

    #[error("sidecar I/O error: {0}")]
    SidecarIo(String),

    #[error("sidecar process died: {0}")]
    SidecarDied(String),

    #[error("sidecar request timed out")]
    SidecarRequestTimeout,

    #[error("sidecar protocol error: {0}")]
    SidecarProtocol(String),

    #[error("sidecar decrypt failed: {0}")]
    SidecarDecryptFailed(String),

    #[error("licence query to the CFX component failed: {0}")]
    LicenseQueryFailed(String),
}
