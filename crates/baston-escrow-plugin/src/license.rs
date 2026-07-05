//! Licence oracle backed by the shared FXServer [`Sidecar`].
//!
//! Sends `{ "op": "license_status" }` and translates the reply into a
//! [`baston_core::license::LicenseStatus`]. The genuine, unmodified CFX
//! component (inside the sidecar FXServer) is the only thing that talks to CFX;
//! this oracle merely reads the *local* verdict the component exposes (the Lua
//! shim reads `sv_licenseKeyToken` via `GetConvar`). BASTON never contacts CFX.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use baston_core::license::{Entitlements, LicenseStatus};

use crate::error::EscrowPluginError;
use crate::sidecar::Sidecar;

/// Reads the CFX server-licence verdict from the sidecar.
pub struct LicenseOracle {
    sidecar: Arc<Sidecar>,
}

impl LicenseOracle {
    /// Wrap an already-running shared sidecar.
    pub(crate) fn new(sidecar: Arc<Sidecar>) -> Self {
        Self { sidecar }
    }

    /// Start a dedicated FXServer sidecar (licence-only convenience path).
    pub fn start(fxserver_path: &Path, resources_dir: &Path) -> Result<Self, EscrowPluginError> {
        Ok(Self::new(Sidecar::start(fxserver_path, resources_dir)?))
    }

    /// Spawn against an arbitrary command (used by tests with the stub sidecar).
    pub fn spawn_with_command(cmd: Command) -> Result<Self, EscrowPluginError> {
        Ok(Self::new(Sidecar::spawn_with_command(cmd)?))
    }

    /// One-shot licence query.
    pub fn query_once(&self) -> Result<LicenseStatus, EscrowPluginError> {
        let line = serde_json::json!({ "op": "license_status" }).to_string();
        let value = self.sidecar.request(line)?;
        parse_status(&value)
    }

    /// Query with a bounded retry loop, to absorb the boot-time race where the
    /// official component has not finished validating the key yet.
    ///
    /// Returns as soon as the licence is valid; a `banned` verdict is terminal
    /// and returned immediately. If neither happens within `budget`, returns the
    /// last observed (invalid) status so the caller **fails closed** — never an
    /// optimistic "valid".
    pub fn query(
        &self,
        budget: Duration,
        interval: Duration,
    ) -> Result<LicenseStatus, EscrowPluginError> {
        let deadline = Instant::now() + budget;
        let mut last: Option<LicenseStatus> = None;
        loop {
            match self.query_once() {
                Ok(status) if status.valid => return Ok(status),
                Ok(status) if status.banned => return Ok(status),
                Ok(status) => last = Some(status),
                Err(e) => {
                    // Transient I/O: only surface the error if the budget is spent.
                    if Instant::now() >= deadline {
                        return Err(e);
                    }
                }
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            std::thread::sleep(interval.min(deadline - now));
        }
        Ok(last.unwrap_or_else(|| {
            LicenseStatus::invalid(
                "no licence verdict from the CFX component within the startup budget",
            )
        }))
    }
}

/// Translate a `license_status` reply into a [`LicenseStatus`]. An `{ error }`
/// reply becomes a hard error (transport/protocol failure); a licence *verdict*
/// with `valid=false` is a normal (non-error) result the caller acts on.
fn parse_status(value: &serde_json::Value) -> Result<LicenseStatus, EscrowPluginError> {
    if let Some(err) = value.get("error").and_then(|e| e.as_str()) {
        return Err(EscrowPluginError::LicenseQueryFailed(err.to_string()));
    }

    let valid = value
        .get("valid")
        .and_then(|b| b.as_bool())
        .unwrap_or(false);
    let banned = value
        .get("banned")
        .and_then(|b| b.as_bool())
        .unwrap_or(false);
    let reason = value
        .get("reason")
        .and_then(|r| r.as_str())
        .map(|s| s.to_string());

    let mut entitlements = Entitlements::default();
    if let Some(ent) = value.get("entitlements") {
        entitlements.max_slots = ent
            .get("max_slots")
            .and_then(|m| m.as_u64())
            .and_then(|m| u32::try_from(m).ok());
        if let Some(features) = ent.get("features").and_then(|f| f.as_array()) {
            entitlements.features = features
                .iter()
                .filter_map(|f| f.as_str().map(String::from))
                .collect();
        }
    }

    Ok(LicenseStatus {
        valid,
        banned,
        entitlements,
        reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_verdict() {
        let v = serde_json::json!({ "valid": true });
        let s = parse_status(&v).unwrap();
        assert!(s.valid && !s.banned);
    }

    #[test]
    fn parses_invalid_with_reason() {
        let v = serde_json::json!({ "valid": false, "reason": "token empty" });
        let s = parse_status(&v).unwrap();
        assert!(!s.valid);
        assert_eq!(s.reason.as_deref(), Some("token empty"));
    }

    #[test]
    fn parses_entitlements() {
        let v = serde_json::json!({
            "valid": true,
            "entitlements": { "max_slots": 48, "features": ["onesync"] }
        });
        let s = parse_status(&v).unwrap();
        assert_eq!(s.entitlements.max_slots, Some(48));
        assert!(s.entitlements.grants_feature("onesync"));
    }

    #[test]
    fn error_reply_is_transport_error_not_verdict() {
        let v = serde_json::json!({ "error": "unknown op" });
        assert!(matches!(
            parse_status(&v),
            Err(EscrowPluginError::LicenseQueryFailed(_))
        ));
    }

    #[test]
    fn missing_fields_default_to_invalid() {
        // A malformed reply must never read as valid (fail-closed).
        let v = serde_json::json!({});
        let s = parse_status(&v).unwrap();
        assert!(!s.valid);
    }
}
