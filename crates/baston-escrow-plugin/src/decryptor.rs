//! Escrow decryptor backed by the shared FXServer [`Sidecar`].
//!
//! Sends `{ "op": "decrypt", ... }` requests and interprets the `{ data }` /
//! `{ error }` reply. Plain (non-escrow) files never touch the sidecar.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use baston_core::script_decryptor::{
    is_cfx_encrypted, DecryptError, EntitlementContext, ScriptDecryptor,
};

use crate::error::EscrowPluginError;
use crate::sidecar::Sidecar;

/// A [`ScriptDecryptor`] that delegates escrow decryption to the sidecar.
pub struct SidecarDecryptor {
    sidecar: Arc<Sidecar>,
}

impl SidecarDecryptor {
    /// Wrap an already-running shared sidecar.
    pub(crate) fn new(sidecar: Arc<Sidecar>) -> Self {
        Self { sidecar }
    }

    /// Start a dedicated FXServer sidecar (escrow-only convenience path).
    pub fn start(fxserver_path: &Path, resources_dir: &Path) -> Result<Self, EscrowPluginError> {
        Ok(Self::new(Sidecar::start(fxserver_path, resources_dir)?))
    }

    /// Spawn against an arbitrary command (used by tests with the stub sidecar).
    pub fn spawn_with_command(cmd: Command) -> Result<Self, EscrowPluginError> {
        Ok(Self::new(Sidecar::spawn_with_command(cmd)?))
    }

    fn request(
        &self,
        resource: &str,
        file: &str,
        bytes: &[u8],
    ) -> Result<Vec<u8>, EscrowPluginError> {
        let request_line = serde_json::json!({
            "op": "decrypt",
            "resource": resource,
            "file": file,
            "data": B64.encode(bytes),
        })
        .to_string();

        let value = self.sidecar.request(request_line)?;
        if let Some(data) = value.get("data").and_then(|d| d.as_str()) {
            B64.decode(data)
                .map_err(|e| EscrowPluginError::SidecarProtocol(e.to_string()))
        } else {
            let msg = value
                .get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("unknown error")
                .to_string();
            Err(EscrowPluginError::SidecarDecryptFailed(msg))
        }
    }
}

impl ScriptDecryptor for SidecarDecryptor {
    fn decrypt(
        &self,
        resource_name: &str,
        file_path: &str,
        bytes: &[u8],
        _entitlement: Option<&EntitlementContext>,
    ) -> Result<Vec<u8>, DecryptError> {
        // Plain files never touch the sidecar — zero overhead.
        if !is_cfx_encrypted(bytes) {
            return Ok(bytes.to_vec());
        }
        self.request(resource_name, file_path, bytes)
            .map_err(|e| DecryptError::DecryptionFailed {
                resource: resource_name.to_string(),
                file: file_path.to_string(),
                reason: e.to_string(),
            })
    }

    fn supports_encrypted(&self) -> bool {
        true
    }
}
