//! CFX server identity bootstrap owned by the public gateway.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use baston_cfx_platform::{
    CfxPlatformError, LicenseOracle, PolicyClient, PolicySource, PublicListing, Sidecar,
    SidecarParams,
};
use baston_config::{BastonConfig, LicenseMode};
use baston_core::license::{
    boot_decision, effective_max_players, BootDecision, Entitlements, LicenseKeyToken,
    LicenseStatus,
};

#[derive(Debug, thiserror::Error)]
pub enum CfxBootstrapError {
    /// The official platform boundary rejected or failed an operation.
    #[error(transparent)]
    Platform(#[from] CfxPlatformError),

    /// Tokio could not complete the blocking broker task.
    #[error("the CFX platform broker startup task failed")]
    BrokerTask,

    /// The licence verdict or required broker configuration denied startup.
    #[error("CFX licence validation denied gateway startup: {0}")]
    LicenceDenied(String),
}

struct CancelOnDrop {
    cancelled: Arc<AtomicBool>,
    armed: bool,
}

impl CancelOnDrop {
    fn new(cancelled: Arc<AtomicBool>) -> Self {
        Self {
            cancelled,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.cancelled.store(true, Ordering::Release);
        }
    }
}

/// Live, authenticated platform identity. Owning this value keeps the official
/// FXServer broker alive for the gateway process lifetime.
pub struct CfxRuntime {
    sidecar: Arc<Sidecar>,
    token: LicenseKeyToken,
    policy_source: PolicySource,
    failure_rx: tokio::sync::oneshot::Receiver<String>,
}

impl CfxRuntime {
    /// Return the authenticated token used by the FiveM client policy contract.
    #[must_use]
    pub fn token(&self) -> &LicenseKeyToken {
        &self.token
    }

    /// Return the origin of the enforced slot decision.
    #[must_use]
    pub fn policy_source(&self) -> PolicySource {
        self.policy_source
    }

    /// Replace the private authentication broker with the official public-list
    /// broker after Baston has bound its game sockets. Entitlements were
    /// resolved once from the same authenticated licence identity before any
    /// listener opened, so the broker starts with the final enforced cap.
    ///
    /// # Errors
    ///
    /// Returns an error when public-list configuration is incomplete, either
    /// broker dies, or the public broker cannot authenticate.
    pub async fn activate_public_listing(
        &mut self,
        config: &BastonConfig,
    ) -> Result<(), CfxBootstrapError> {
        let Some(listing) = public_listing(config)? else {
            return Ok(());
        };
        let fxserver_path = config.license.fxserver_path.clone().ok_or_else(|| {
            CfxBootstrapError::LicenceDenied("FXServer path is missing".to_owned())
        })?;
        let params = SidecarParams {
            fxserver_path,
            resources_dir: sidecar_scratch_dir(&config.resources.path),
            license_key: Some(config.license.sv_license_key.trim().to_owned()),
            port: config.license.sidecar_port,
            public_listing: Some(listing),
        };
        let activation = start_and_authenticate(params);
        let (public_sidecar, public_token) = tokio::select! {
            result = activation => result?,
            reason = self.wait_for_failure() => {
                return Err(CfxBootstrapError::LicenceDenied(format!(
                    "private CFX broker stopped during public activation: {reason}"
                )));
            }
        };
        let old_sidecar = std::mem::replace(&mut self.sidecar, public_sidecar);
        self.token = public_token;
        self.failure_rx = spawn_broker_monitor(&self.sidecar);
        drop(old_sidecar);
        tracing::info!(
            target: "cfx_platform",
            max_players = config.server.max_players,
            "official FXServer broker activated public CFX registration"
        );
        Ok(())
    }

    /// Wait until the official broker exits unexpectedly.
    pub async fn wait_for_failure(&mut self) -> String {
        (&mut self.failure_rx)
            .await
            .unwrap_or_else(|_| "CFX broker health monitor stopped".to_owned())
    }
}

/// Authenticate the configured server before any public listener starts.
///
/// # Errors
///
/// Returns an error when verified mode is misconfigured, the official broker
/// cannot start, the licence verdict denies boot, or policy client setup fails.
pub async fn bootstrap(config: &mut BastonConfig) -> Result<Option<CfxRuntime>, CfxBootstrapError> {
    match config.license.mode {
        LicenseMode::Off => {
            tracing::warn!(
                target: "cfx_platform",
                "[license] mode = \"off\" — CFX server identity is not authenticated"
            );
            return Ok(None);
        }
        LicenseMode::Gate => {
            tracing::warn!(
                target: "cfx_platform",
                "[license] mode = \"gate\" only validates key shape; the server is not authenticated"
            );
            return Ok(None);
        }
        LicenseMode::Verified => {}
    }

    let fxserver_path =
        config.license.fxserver_path.clone().ok_or_else(|| {
            CfxBootstrapError::LicenceDenied("FXServer path is missing".to_owned())
        })?;
    let resources_dir = sidecar_scratch_dir(&config.resources.path);
    let private_params = SidecarParams {
        fxserver_path,
        resources_dir,
        license_key: Some(config.license.sv_license_key.trim().to_owned()),
        port: config.license.sidecar_port,
        public_listing: None,
    };

    // Authenticate privately first. Public registration starts only after the
    // slot entitlement is known, so the first advertised heartbeat cannot
    // overstate the server's authenticated capacity.
    let (sidecar, token) = start_and_authenticate(private_params).await?;
    let policy_client = PolicyClient::new()?;
    let policy = policy_client.resolve(&token).await;
    apply_slot_policy(config, &policy.entitlements);

    tracing::info!(
        target: "cfx_platform",
        max_players = config.server.max_players,
        policy_source = ?policy.source,
        public_listing = config.license.public_listing,
        "CFX server identity authenticated by the official FXServer broker"
    );

    let failure_rx = spawn_broker_monitor(&sidecar);
    Ok(Some(CfxRuntime {
        sidecar,
        token,
        policy_source: policy.source,
        failure_rx,
    }))
}

async fn start_and_authenticate(
    params: SidecarParams,
) -> Result<(Arc<Sidecar>, LicenseKeyToken), CfxBootstrapError> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut cancellation_guard = CancelOnDrop::new(Arc::clone(&cancelled));
    let worker_cancelled = Arc::clone(&cancelled);
    let result = tokio::task::spawn_blocking(move || {
        let sidecar = Sidecar::start_with_cancellation(&params, &worker_cancelled)?;
        let status = LicenseOracle::from_sidecar(Arc::clone(&sidecar)).query_with_cancellation(
            Duration::from_secs(20),
            Duration::from_secs(1),
            &worker_cancelled,
        )?;
        Ok::<_, CfxPlatformError>((sidecar, status))
    })
    .await
    .map_err(|_| CfxBootstrapError::BrokerTask)?;
    cancellation_guard.disarm();
    let (sidecar, status) = result?;
    let token = authenticated_token(&status)?;
    Ok((sidecar, token))
}

fn apply_slot_policy(config: &mut BastonConfig, entitlements: &Entitlements) {
    let configured = config.server.max_players;
    let (effective, capped) = effective_slot_cap(configured, entitlements);
    if capped {
        tracing::warn!(
            target: "cfx_platform",
            configured,
            effective,
            "max_players exceeds the authenticated CFX policy and was capped"
        );
        config.server.max_players = effective;
    }
}

fn public_listing(config: &BastonConfig) -> Result<Option<PublicListing>, CfxBootstrapError> {
    if !config.license.public_listing {
        return Ok(None);
    }
    let public_ip = config.license.listing_ip_override.ok_or_else(|| {
        CfxBootstrapError::LicenceDenied(
            "public listing requires license.listing_ip_override".to_owned(),
        )
    })?;
    Ok(Some(PublicListing {
        public_ip,
        public_port: config.server.port,
        hostname: config.server.name.clone(),
        max_clients: config.server.max_players,
        onesync: config.state_sync.onesync.is_enabled(),
    }))
}

fn spawn_broker_monitor(sidecar: &Arc<Sidecar>) -> tokio::sync::oneshot::Receiver<String> {
    let sidecar = Arc::downgrade(sidecar);
    let (failure_tx, failure_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let Some(sidecar) = sidecar.upgrade() else {
                return;
            };
            if let Some(status) = sidecar.exit_status() {
                let _ = failure_tx.send(format!("FXServer broker exited ({status})"));
                return;
            }
        }
    });
    failure_rx
}

fn sidecar_scratch_dir(resources_path: &Path) -> std::path::PathBuf {
    resources_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(".baston-sidecar-res")
}

fn authenticated_token(status: &LicenseStatus) -> Result<LicenseKeyToken, CfxBootstrapError> {
    match boot_decision(status) {
        BootDecision::Allow => status.token.clone().ok_or_else(|| {
            CfxBootstrapError::LicenceDenied(
                "the official broker returned no authenticated token".to_owned(),
            )
        }),
        BootDecision::Deny(reason) => Err(CfxBootstrapError::LicenceDenied(reason)),
    }
}

fn effective_slot_cap(configured: u32, entitlements: &Entitlements) -> (u32, bool) {
    effective_max_players(configured, entitlements)
}

#[cfg(test)]
mod tests {
    use super::*;
    use baston_core::license::{Entitlements, LicenseKeyToken, LicenseStatus};

    #[test]
    fn invalid_banned_and_missing_token_verdicts_are_rejected() {
        assert!(authenticated_token(&LicenseStatus::invalid("invalid")).is_err());

        let banned = LicenseStatus {
            valid: true,
            banned: true,
            token: LicenseKeyToken::new("revoked"),
            entitlements: Entitlements::default(),
            reason: Some("revoked".to_owned()),
        };
        assert!(authenticated_token(&banned).is_err());

        let missing_token = LicenseStatus {
            valid: true,
            banned: false,
            token: None,
            entitlements: Entitlements::default(),
            reason: None,
        };
        assert!(authenticated_token(&missing_token).is_err());
    }

    #[test]
    fn authenticated_verdict_returns_the_redacted_token_type() {
        let status =
            LicenseStatus::authenticated(LicenseKeyToken::new("authenticated-token").unwrap());
        let token = authenticated_token(&status).unwrap();
        assert_eq!(token.as_str(), "authenticated-token");
        assert!(!format!("{token:?}").contains(token.as_str()));
    }

    #[test]
    fn policy_cap_only_lowers_configured_slots() {
        assert_eq!(
            effective_slot_cap(
                128,
                &Entitlements {
                    max_slots: Some(48),
                    features: Vec::new(),
                }
            ),
            (48, true)
        );
        assert_eq!(
            effective_slot_cap(
                32,
                &Entitlements {
                    max_slots: Some(48),
                    features: Vec::new(),
                }
            ),
            (32, false)
        );
    }
}
