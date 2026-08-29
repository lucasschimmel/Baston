//! Error type for the CFX platform boundary.

use thiserror::Error;

/// Errors produced while installing or running the CFX platform broker.
#[derive(Debug, Error)]
pub enum CfxPlatformError {
    /// FXServer or one of its private support files could not be started.
    #[error("failed to spawn FXServer sidecar: {0}")]
    SidecarSpawn(String),

    /// The shim did not publish readiness within the bounded startup window.
    #[error("sidecar did not become ready within the startup timeout")]
    SidecarStartTimeout,

    /// Local file-drop IPC failed.
    #[error("sidecar I/O error: {0}")]
    SidecarIo(String),

    /// The official broker exited before completing an operation.
    #[error("sidecar process died: {0}")]
    SidecarDied(String),

    /// A file-drop request exceeded its response budget.
    #[error("sidecar request timed out")]
    SidecarRequestTimeout,

    /// The caller cancelled a broker startup or request.
    #[error("sidecar operation was cancelled")]
    SidecarCancelled,

    /// The shim returned a malformed or unsupported response.
    #[error("sidecar protocol error: {0}")]
    SidecarProtocol(String),

    /// The local licence-status operation failed.
    #[error("licence query to the CFX component failed: {0}")]
    LicenseQueryFailed(String),

    /// The hardened HTTP policy client could not be constructed.
    #[error("failed to initialize the CFX policy client")]
    PolicyClientBuild,

    /// The configured policy URL cannot accept a token path segment.
    #[error("the configured CFX policy endpoint is invalid")]
    PolicyEndpoint,

    /// The policy request failed without a usable HTTP response.
    #[error("the CFX policy request failed")]
    PolicyRequest,

    /// The policy endpoint returned a non-success status.
    #[error("the CFX policy endpoint returned HTTP {0}")]
    PolicyHttpStatus(u16),

    /// The policy response was not the expected string array.
    #[error("the CFX policy response was not a valid policy list")]
    PolicyDecode,

    /// The policy response exceeded the fixed memory limit.
    #[error("the CFX policy response exceeded the maximum accepted size")]
    PolicyResponseTooLarge,

    /// A launch convar contained a quote or control character.
    #[error("the CFX broker configuration contains an unsafe {0}")]
    InvalidBrokerConfig(&'static str),
}
