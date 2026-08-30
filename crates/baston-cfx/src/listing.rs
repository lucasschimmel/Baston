//! Nucleus registration and server-list ingress.
//!
//! Both exchanges are transcribed from the public tree —
//! `ServerNucleusMock.cpp` for the register call, `GameServer.cpp` for the
//! heartbeat — including the cadence and the backoff. Only the credentials
//! come from somewhere else, and only the `User-Agent` differs.
//!
//! **Unverified against the live service.** Registering a server is an
//! outward-facing act with a public result, so this code has been written
//! against the sources rather than tested against `servers-frontend`. The
//! first operator to enable it is the first real test; the response body is
//! logged for exactly that reason.

use std::time::Duration;

use crate::error::CfxError;
use crate::identity::{user_agent, CfxIdentity};

/// `ServerNucleusMock.cpp` — assigns the `users.cfx.re` hostname.
const NUCLEUS_EP: &str = "https://cfx.re/api/register/?v=2";

/// `GameServer.cpp`, `kDefaultServerList`.
const INGRESS_EP: &str = "https://servers-frontend.fivem.net/api/serverlist/ingress";

/// `GameServer.cpp`: `m_nextHeartbeatTime = msec() + 3min`.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(180);

/// `ServerNucleusMock.cpp`: 15s, doubling, capped at 15min.
const REGISTER_BACKOFF_START: Duration = Duration::from_secs(15);
const REGISTER_BACKOFF_MAX: Duration = Duration::from_secs(900);

const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// The three documents the server list wants, which are also the three
/// documents `/info.json` and `/players.json` are built from.
///
/// The gateway must hand over **the same `info` value it serves**, not a
/// second one built for this purpose. See [`Listing::heartbeat`].
pub struct ServerSnapshot {
    pub info: serde_json::Value,
    pub dynamic: serde_json::Value,
    pub players: serde_json::Value,
}

/// Public address configuration. Both fields are required by the ingress
/// contract, and BASTON refuses to guess either.
#[derive(Debug, Clone)]
pub struct PublicAddress {
    pub ip_override: String,
    pub port: u16,
}

/// Registration and heartbeat against the CFX server list.
#[derive(Debug)]
pub struct Listing {
    http: reqwest::Client,
    address: PublicAddress,
}

impl Listing {
    /// # Errors
    ///
    /// Fails when the HTTP client cannot be built, or the public address is
    /// missing.
    pub fn new(address: PublicAddress) -> Result<Self, CfxError> {
        if address.ip_override.trim().is_empty() {
            return Err(CfxError::ListingAddressMissing);
        }
        let http = reqwest::Client::builder()
            .user_agent(user_agent())
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| CfxError::ClientBuild(e.to_string()))?;
        Ok(Self { http, address })
    }

    /// Register with nucleus, returning the assigned `users.cfx.re` hostname.
    ///
    /// Retries with the same backoff FXServer uses, because the platform
    /// answers slowly on a cold key. Returns `Ok(None)` when `attempts` is
    /// exhausted — registration is not required to run a server, so a failure
    /// here is reported and survived rather than fatal.
    pub async fn register(&self, identity: &CfxIdentity, attempts: u32) -> Option<String> {
        let body = serde_json::json!({
            "token": identity.nucleus_token().expose_at_boundary(),
            "port": self.address.port.to_string(),
            "ipOverride": self.address.ip_override,
        });

        let mut delay = REGISTER_BACKOFF_START;
        for attempt in 1..=attempts {
            match self.post_json(NUCLEUS_EP, &body).await {
                Ok(text) => {
                    if let Some(host) = serde_json::from_str::<serde_json::Value>(&text)
                        .ok()
                        .and_then(|v| v.get("host")?.as_str().map(str::to_owned))
                        .filter(|h| !h.is_empty())
                    {
                        tracing::info!(target: "cfx", host, "registered with CFX nucleus");
                        return Some(host);
                    }
                    tracing::warn!(
                        target: "cfx", attempt,
                        "nucleus registration returned no host; retrying"
                    );
                }
                Err(reason) => {
                    tracing::warn!(target: "cfx", attempt, %reason, "nucleus registration failed");
                }
            }
            if attempt < attempts {
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(REGISTER_BACKOFF_MAX);
            }
        }
        tracing::error!(
            target: "cfx",
            "nucleus registration did not succeed; the server runs but has no cfx.re hostname"
        );
        None
    }

    /// Send one server-list heartbeat.
    ///
    /// # The check at the top is the point of this function
    ///
    /// Being listed and being slot-checked are two halves of one bargain. The
    /// client only fetches the entitlement policy when it finds
    /// `sv_licenseKeyToken` in `/info.json`; a server that heartbeats with a
    /// valid listing token while serving an `/info.json` without that token
    /// would be discoverable *and* unchecked — a free key with a paid key's
    /// slots.
    ///
    /// In FXServer the two cannot come apart, because both read one convar
    /// flagged `ConVar_ServerInfo`. Here they are separate code paths, so the
    /// invariant has to be asserted rather than inherited. If the snapshot
    /// being advertised does not carry this identity's token, no heartbeat is
    /// sent.
    pub async fn heartbeat(
        &self,
        identity: &CfxIdentity,
        snapshot: &ServerSnapshot,
    ) -> Result<(), String> {
        let advertised = snapshot
            .info
            .get("vars")
            .and_then(|v| v.get("sv_licenseKeyToken"))
            .and_then(serde_json::Value::as_str);

        if advertised != Some(identity.info_json_token()) {
            return Err(
                "refusing to advertise a server whose /info.json does not publish this \
                 identity's sv_licenseKeyToken — being listed and being slot-checked are \
                 the same bargain (see docs/adr/004-cfx-identity-without-fxserver.md)"
                    .to_owned(),
            );
        }

        let body = serde_json::json!({
            "port": self.address.port,
            "listingToken": identity.listing_token().expose_at_boundary(),
            "ipOverride": self.address.ip_override,
            "private": false,
            "fallbackData": {
                "players": snapshot.players,
                "info": snapshot.info,
                "dynamic": snapshot.dynamic,
            }
        });

        let text = self.post_json(INGRESS_EP, &body).await?;
        // The ingress reports query problems in the body of a 200, so a
        // successful POST is not a successful listing.
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(error) = value.get("lastError").and_then(serde_json::Value::as_str) {
                tracing::warn!(
                    target: "cfx",
                    error = error.lines().next().unwrap_or(error),
                    "the server list accepted the heartbeat but reported a query error"
                );
            }
        }
        Ok(())
    }

    async fn post_json(&self, url: &str, body: &serde_json::Value) -> Result<String, String> {
        let response = self
            .http
            .post(url)
            .header("Content-Type", "application/json; charset=utf-8")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| {
                // These URLs carry no credential in the path, but the body
                // does, so keep the shape of the message consistent with
                // identity.rs and say nothing about the request.
                if e.is_timeout() {
                    "request timed out".to_owned()
                } else if e.is_connect() {
                    "could not connect".to_owned()
                } else {
                    "request failed".to_owned()
                }
            })?;

        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(format!(
                "HTTP {status}: {}",
                text.lines().next().unwrap_or("")
            ));
        }
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_listing_without_a_public_address_is_refused_at_construction() {
        let err = Listing::new(PublicAddress {
            ip_override: "   ".to_owned(),
            port: 30120,
        })
        .expect_err("blank address must be refused");
        assert!(matches!(err, CfxError::ListingAddressMissing));
    }

    #[test]
    fn the_heartbeat_cadence_matches_fxserver() {
        assert_eq!(HEARTBEAT_INTERVAL, Duration::from_secs(3 * 60));
    }

    fn identity(token: &str) -> CfxIdentity {
        let policy = crate::policy::PolicySet::from_strings(["onesync_big"]);
        let slots = crate::policy::decide_slots(2048, &policy, true);
        CfxIdentity::for_test(token, policy, slots)
    }

    fn snapshot(vars: serde_json::Value) -> ServerSnapshot {
        ServerSnapshot {
            info: serde_json::json!({ "name": "BASTON", "vars": vars }),
            dynamic: serde_json::json!({}),
            players: serde_json::json!([]),
        }
    }

    fn listing() -> Listing {
        Listing::new(PublicAddress {
            ip_override: "203.0.113.10".to_owned(),
            port: 30120,
        })
        .expect("valid address")
    }

    #[tokio::test]
    async fn a_snapshot_without_the_token_is_never_advertised() {
        // The accident this guards: /info.json and the heartbeat are separate
        // code paths here, so a server could be listed while publishing no
        // token — discoverable, and never slot-checked by any client.
        let err = listing()
            .heartbeat(
                &identity("tok"),
                &snapshot(serde_json::json!({ "sv_maxClients": "2048" })),
            )
            .await
            .expect_err("must refuse to advertise");
        assert!(err.contains("sv_licenseKeyToken"), "got: {err}");
    }

    #[tokio::test]
    async fn a_snapshot_advertising_a_different_token_is_refused_too() {
        // Not just presence: the advertised token must be *this* identity's,
        // or the client looks up a policy that does not describe this server.
        let err = listing()
            .heartbeat(
                &identity("tok"),
                &snapshot(serde_json::json!({ "sv_licenseKeyToken": "some-other-token" })),
            )
            .await
            .expect_err("must refuse a mismatched token");
        assert!(err.contains("sv_licenseKeyToken"), "got: {err}");
    }

    #[tokio::test]
    async fn a_matching_snapshot_passes_the_check_and_reaches_the_network() {
        // Proves the guard is not simply always-on. The request itself is
        // expected to fail with a transport error (the token is fake), which
        // is a different failure than the refusal above.
        let err = listing()
            .heartbeat(
                &identity("tok"),
                &snapshot(serde_json::json!({ "sv_licenseKeyToken": "tok" })),
            )
            .await
            .err();
        if let Some(err) = err {
            assert!(
                !err.contains("refusing to advertise"),
                "the guard must have passed, got: {err}"
            );
        }
    }
}
