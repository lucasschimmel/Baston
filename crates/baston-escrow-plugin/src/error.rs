//! Escrow adapter errors.

use baston_cfx_platform::CfxPlatformError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EscrowPluginError {
    #[error(
        "the 'direct' (FFI) escrow backend is not supported: svadhesive.dll exposes only an \
         opaque CitizenFX component (single `CreateComponent` export, no flat decrypt symbol). \
         Use backend = \"sidecar\" instead."
    )]
    DirectBackendUnsupported,

    #[error(transparent)]
    Platform(#[from] CfxPlatformError),

    #[error("sidecar protocol error: {0}")]
    SidecarProtocol(String),

    #[error("sidecar decrypt failed: {0}")]
    SidecarDecryptFailed(String),
}
