//! CFX server identity, owned by the public gateway.
//!
//! Two things happen here, in this order and for a reason:
//!
//! 1. **Authenticate, and apply what the licence grants** — before any
//!    listener opens. A server whose configured slot count exceeds its
//!    entitlement is capped now, not discovered by the first player the client
//!    bounces at connect time.
//! 2. **Advertise** — only after the game and HTTP listeners are up, so the
//!    first heartbeat the world sees describes a server that can be joined.
//!
//! Zone processes never authenticate: one logical server has one identity, and
//! it belongs to the process clients actually talk to.

use std::sync::Arc;

use baston_cfx::{CfxError, CfxIdentity, Listing, PublicAddress, ServerSnapshot};
use baston_config::{BastonConfig, LicenseMode};

use crate::http::{info, AppState};

/// How many times to retry nucleus registration before giving up on it.
///
/// Registration assigns the `users.cfx.re` hostname and is not required to run
/// a server, so exhausting this is reported and survived.
const NUCLEUS_ATTEMPTS: u32 = 4;

/// Authenticate with CFX and apply the licence to `config` in place.
///
/// Returns `None` for every mode that does not authenticate, which is the
/// shipped default. `config.server.max_players` may be **lowered** by this
/// call and is never raised.
///
/// # Errors
///
/// Any failure to establish identity in `cfx` mode, which is fatal: a server
/// that asked to be authenticated must not boot unauthenticated.
pub async fn authenticate(config: &mut BastonConfig) -> Result<Option<CfxIdentity>, CfxError> {
    match config.license.mode {
        LicenseMode::Off => {
            tracing::warn!(
                target: "cfx",
                "[license] mode = \"off\" — no CFX licence key is configured and none is checked"
            );
            return Ok(None);
        }
        LicenseMode::Gate => {
            tracing::warn!(
                target: "cfx",
                "[license] mode = \"gate\" — the key's shape is valid, but BASTON does not \
                 validate it against CFX and enforces no entitlement from it"
            );
            return Ok(None);
        }
        LicenseMode::Cfx => {}
    }

    let onesync = config.state_sync.onesync.is_enabled();
    let identity = baston_cfx::authenticate(
        &config.license.sv_license_key,
        config.server.max_players,
        onesync,
    )
    .await?;

    let slots = identity.slots();
    if slots.was_capped() {
        tracing::warn!(
            target: "cfx",
            configured = slots.configured,
            effective = slots.effective,
            "max_players exceeds what this licence grants and was lowered — raise the tier \
             at https://portal.cfx.re, or run [license] mode = \"off\" without CFX identity"
        );
        config.server.max_players = slots.effective;
    }

    let grants: Vec<&str> = identity.policy().entries().collect();
    tracing::info!(
        target: "cfx",
        user_agent = baston_cfx::user_agent(),
        max_players = config.server.max_players,
        grants = ?grants,
        "CFX identity authenticated"
    );
    Ok(Some(identity))
}

/// Register with nucleus and start the server-list heartbeat.
///
/// Called **after** the listeners are up. Does nothing unless `[listing]` is
/// enabled, which the configuration layer already refuses without an
/// authenticated identity.
///
/// # Errors
///
/// Only configuration errors. A platform failure is logged and retried rather
/// than taken as a reason to stop serving players: a server that is running
/// but unlisted is still a running server.
pub fn spawn_listing(state: &Arc<AppState>) -> Result<(), CfxError> {
    if !state.config.listing.enabled {
        return Ok(());
    }
    let Some(identity) = state.cfx.clone() else {
        // Unreachable through `BastonConfig::validate`, which refuses a
        // listing without `cfx` mode. Belt and braces: silently not listing is
        // better than listing without an identity, which cannot be done here
        // anyway since the heartbeat needs one.
        tracing::error!(target: "cfx", "[listing] is enabled without a CFX identity; not listing");
        return Ok(());
    };
    // Empty unless the operator set one: CFX then reaches the server through
    // the nucleus-assigned hostname rather than querying this address itself.
    let listing = Listing::new(PublicAddress {
        ip_override: state
            .config
            .listing
            .ip_override
            .map(|ip| ip.to_string())
            .unwrap_or_default(),
        port: state.config.server.port,
    })?;

    let state = Arc::clone(state);
    tokio::spawn(async move {
        if let Some(host) = listing.register(&identity, NUCLEUS_ATTEMPTS).await {
            tracing::info!(target: "cfx", host, "CFX assigned this server a hostname");
        }

        let mut ticker = tokio::time::interval(baston_cfx::HEARTBEAT_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let resources = state.resource_manager.started_names().await;
            let snapshot = ServerSnapshot {
                info: info::payload(&state, resources),
                dynamic: info::dynamic_payload(&state),
                // The list shows a player count, not a roster; BASTON does not
                // publish who is connected.
                players: serde_json::json!([]),
            };
            if let Err(reason) = listing.heartbeat(&identity, &snapshot).await {
                tracing::warn!(target: "cfx", %reason, "server-list heartbeat failed");
            }
        }
    });
    Ok(())
}
