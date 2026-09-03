//! Validating the operator's licence key, and the identity that results.
//!
//! This is the exchange `svadhesive` performs inside FXServer, performed by
//! BASTON instead. It is an ordinary HTTPS GET carrying the operator's own key
//! in the path — no client secret, no device binding, no challenge — which is
//! why it can be done at all.
//!
//! **BASTON identifies itself as BASTON.** The `User-Agent` says so, and it is
//! never set to FXServer's. If CFX declines a self-identified third-party
//! client, that refusal is an answer and the server falls back to running
//! without a CFX identity; it is not an obstacle to route around.

use std::time::Duration;

use serde::Deserialize;

use crate::error::CfxError;
use crate::policy::{decide_slots, PolicySet, SlotDecision};
use crate::secret::Secret;

/// FXServer builds this URL as `LICENSING_EP + "v1/..."` where the endpoint
/// constant already ends in `/`, producing a double slash after the host
/// (`ServerLicensingComponent.h`). Reproduced exactly: a normalised path is a
/// different request, and this one is known to work.
const VALIDATE_EP: &str = "https://portal-api.cfx.re//v1/key/validate/";

/// Where the *client* reads the entitlements it will enforce
/// (`NetLibrary.cpp`, `POLICY_LIVE_ENDPOINT`). BASTON reads the same list so
/// its own ceiling and the client's check cannot disagree.
const POLICY_EP: &str = "https://policy-live.fivem.net/api/policy/";

/// Bounded because a hung platform call must not hold the boot open.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// A policy list is a short array of short strings; anything larger is not one.
const POLICY_MAX_BYTES: usize = 64 * 1024;

/// What BASTON tells CFX it is.
///
/// Deliberately not FXServer's string. Being identifiable is the point: it
/// lets CFX see this traffic for what it is and decide about it, and it is
/// what makes the question askable in good faith rather than assumed.
#[must_use]
pub fn user_agent() -> String {
    format!(
        "BASTON/{} (+https://github.com/lucasschimmel/Baston)",
        env!("CARGO_PKG_VERSION")
    )
}

/// The raw shape CFX returns from key validation.
///
/// Unknown fields are ignored, so a platform-side addition (`hashid` appeared
/// between the July 2026 capture and August) does not break the parse.
#[derive(Debug, Deserialize)]
struct ValidateResponse {
    #[serde(default)]
    valid: bool,
    #[serde(default)]
    token: String,
    #[serde(default)]
    nucleus_token: String,
    #[serde(default)]
    listing_token: String,
}

/// An authenticated CFX server identity, with its slot decision already made.
///
/// **The two capabilities this unlocks are inseparable by construction, and
/// that is the point.** Publishing `sv_licenseKeyToken` is what makes the
/// client fetch the policy and check the slot count; registering with the
/// server list is what makes the server discoverable. A design that could hold
/// the listing credentials while withholding the public token would be listed
/// *and* unchecked — the free tier with a paid tier's slots.
///
/// So there is one type, it carries both, and it cannot exist until
/// [`decide_slots`] has run. Anything that lists has a token to publish, and
/// anything with a token to publish has already been capped.
#[derive(Debug)]
pub struct CfxIdentity {
    token: Secret,
    nucleus_token: Secret,
    listing_token: Secret,
    policy: PolicySet,
    slots: SlotDecision,
}

impl CfxIdentity {
    /// The value that belongs in `/info.json` under `vars.sv_licenseKeyToken`.
    ///
    /// Named for the boundary it crosses. This one is *meant* to be public —
    /// the client reads it to look the server's entitlements up — but it is
    /// still a token, so it goes through the same explicit accessor as the
    /// two that are not.
    #[must_use]
    pub fn info_json_token(&self) -> &str {
        self.token.expose_at_boundary()
    }

    #[must_use]
    pub(crate) fn nucleus_token(&self) -> &Secret {
        &self.nucleus_token
    }

    #[must_use]
    pub(crate) fn listing_token(&self) -> &Secret {
        &self.listing_token
    }

    #[must_use]
    pub fn policy(&self) -> &PolicySet {
        &self.policy
    }

    /// The slot count this server may actually run, already capped.
    #[must_use]
    pub fn slots(&self) -> SlotDecision {
        self.slots
    }

    /// Build an identity without the network, for tests in this crate.
    ///
    /// Deliberately not public: outside these tests, the only way to hold a
    /// `CfxIdentity` is to have gone through [`authenticate`], which is what
    /// guarantees the slot decision was made.
    #[cfg(test)]
    pub(crate) fn for_test(token: &str, policy: PolicySet, slots: SlotDecision) -> Self {
        Self {
            token: Secret::new(token).expect("test token"),
            nucleus_token: Secret::new("nucleus").expect("test token"),
            listing_token: Secret::new("listing").expect("test token"),
            policy,
            slots,
        }
    }
}

/// Validate the key with CFX, read the entitlements it carries, and apply them.
///
/// Fails closed at every step: a refusal, an unreadable policy, or a missing
/// credential all stop the boot rather than continue with a guess.
///
/// # Errors
///
/// See [`CfxError`]. Every variant names what the operator should do.
pub async fn authenticate(
    license_key: &str,
    configured_max_players: u32,
    onesync_enabled: bool,
) -> Result<CfxIdentity, CfxError> {
    let agent = user_agent();
    let http = reqwest::Client::builder()
        .user_agent(agent.clone())
        .timeout(REQUEST_TIMEOUT)
        // The key travels in the path. A redirect would carry it to whatever
        // host the response names, so redirects are not followed.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| CfxError::ClientBuild(e.to_string()))?;

    let validated = validate_key(&http, license_key, &agent).await?;
    let policy = fetch_policy(&http, &validated.token).await?;
    let slots = decide_slots(configured_max_players, &policy, onesync_enabled);

    Ok(CfxIdentity {
        token: validated.token,
        nucleus_token: validated.nucleus_token,
        listing_token: validated.listing_token,
        policy,
        slots,
    })
}

#[derive(Debug)]
struct Credentials {
    token: Secret,
    nucleus_token: Secret,
    listing_token: Secret,
}

async fn validate_key(
    http: &reqwest::Client,
    license_key: &str,
    agent: &str,
) -> Result<Credentials, CfxError> {
    let url = format!("{VALIDATE_EP}{}", license_key.trim());
    let response = http
        .get(&url)
        .send()
        .await
        // The key is in the URL, and reqwest puts the URL in its error
        // Display. Strip it rather than log the operator's licence key.
        .map_err(|e| CfxError::ValidateRequest(without_url(&e)))?;

    let status = response.status();
    if !status.is_success() {
        return Err(CfxError::ValidateRefused {
            status: status.as_u16(),
            user_agent: agent.to_owned(),
        });
    }

    let body = response
        .text()
        .await
        .map_err(|e| CfxError::ValidateRequest(without_url(&e)))?;
    parse_validation(&body)
}

/// Split out from the request so the response contract has tests that do not
/// need the network.
fn parse_validation(body: &str) -> Result<Credentials, CfxError> {
    let parsed: ValidateResponse =
        serde_json::from_str(body).map_err(|e| CfxError::ValidateDecode(e.to_string()))?;

    if !parsed.valid {
        return Err(CfxError::KeyRejected);
    }
    Ok(Credentials {
        token: Secret::new(parsed.token).ok_or(CfxError::MissingCredential("licence token"))?,
        nucleus_token: Secret::new(parsed.nucleus_token)
            .ok_or(CfxError::MissingCredential("nucleus token"))?,
        listing_token: Secret::new(parsed.listing_token)
            .ok_or(CfxError::MissingCredential("listing token"))?,
    })
}

async fn fetch_policy(http: &reqwest::Client, token: &Secret) -> Result<PolicySet, CfxError> {
    let url = format!("{POLICY_EP}{}", token.expose_at_boundary());
    let response = http
        .get(&url)
        .send()
        .await
        .map_err(|e| CfxError::PolicyUnavailable(without_url(&e)))?;

    let status = response.status();
    if !status.is_success() {
        return Err(CfxError::PolicyUnavailable(format!("HTTP {status}")));
    }

    let body = response
        .text()
        .await
        .map_err(|e| CfxError::PolicyUnavailable(without_url(&e)))?;
    if body.len() > POLICY_MAX_BYTES {
        return Err(CfxError::PolicyTooLarge(POLICY_MAX_BYTES));
    }
    parse_policy(&body)
}

fn parse_policy(body: &str) -> Result<PolicySet, CfxError> {
    let entries: Vec<String> = serde_json::from_str(body).map_err(|_| CfxError::PolicyDecode)?;
    Ok(PolicySet::from_strings(entries))
}

/// `reqwest::Error`'s `Display` includes the request URL, and both URLs here
/// carry a credential in the path.
fn without_url(error: &reqwest::Error) -> String {
    let kind = if error.is_timeout() {
        "request timed out"
    } else if error.is_connect() {
        "could not connect"
    } else if error.is_decode() {
        "could not read the response"
    } else {
        "request failed"
    };
    match error.status() {
        Some(status) => format!("{kind} (HTTP {status})"),
        None => kind.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_user_agent_names_baston_and_never_fxserver() {
        let agent = user_agent();
        assert!(agent.starts_with("BASTON/"), "got {agent}");
        assert!(
            !agent.to_ascii_lowercase().contains("fxserver"),
            "the agent must not claim to be FXServer: {agent}"
        );
        assert!(
            agent.contains("github.com"),
            "a reader of CFX's logs should be able to find out what this is"
        );
    }

    #[test]
    fn a_full_response_yields_all_three_credentials() {
        let creds = parse_validation(
            r#"{"success":true,"valid":true,"key_user":1,"token":"t","grants_token":"g",
                "nucleus_token":"n","listing_token":"l","policy":[],"hashid":"h"}"#,
        )
        .expect("valid response");
        assert_eq!(creds.token.expose_at_boundary(), "t");
        assert_eq!(creds.nucleus_token.expose_at_boundary(), "n");
        assert_eq!(creds.listing_token.expose_at_boundary(), "l");
    }

    #[test]
    fn a_field_cfx_adds_later_does_not_break_the_parse() {
        // `hashid` appeared between two observations of this endpoint. The
        // next addition must not take a server down.
        parse_validation(
            r#"{"valid":true,"token":"t","nucleus_token":"n","listing_token":"l",
                "some_future_field":{"nested":true}}"#,
        )
        .expect("unknown fields are ignored");
    }

    #[test]
    fn a_key_cfx_rejects_stops_the_boot() {
        let err = parse_validation(r#"{"success":true,"valid":false,"token":""}"#)
            .expect_err("must reject");
        assert!(matches!(err, CfxError::KeyRejected));
    }

    #[test]
    fn a_valid_verdict_with_a_blank_credential_is_still_a_failure() {
        // Fail closed: "valid, but here is an empty listing token" must not
        // produce an identity that half-works.
        let err = parse_validation(
            r#"{"valid":true,"token":"t","nucleus_token":"n","listing_token":"  "}"#,
        )
        .expect_err("must reject");
        assert!(matches!(err, CfxError::MissingCredential("listing token")));
    }

    #[test]
    fn an_empty_policy_array_parses_as_the_free_tier() {
        let policy = parse_policy("[]").expect("empty is valid");
        assert!(policy.is_empty());
        assert_eq!(policy.slot_ceiling(), 48);
    }

    #[test]
    fn a_policy_list_parses_into_entitlements() {
        let policy = parse_policy(r#"["onesync","onesync_big"]"#).expect("valid");
        assert!(policy.grants("onesync_big"));
        assert_eq!(policy.slot_ceiling(), 2048);
    }

    #[test]
    fn a_policy_that_is_not_a_string_list_is_refused_rather_than_coerced() {
        // An HTML error page or an object here must not read as "no grants",
        // which would silently cap a paying server at 48.
        assert!(matches!(
            parse_policy("<html>502</html>"),
            Err(CfxError::PolicyDecode)
        ));
        assert!(matches!(
            parse_policy(r#"{"error":"nope"}"#),
            Err(CfxError::PolicyDecode)
        ));
    }

    #[test]
    fn the_refusal_error_tells_an_operator_the_agent_was_honest() {
        // If CFX ever declines this client, the message must make clear that
        // BASTON asked as itself — so nobody's first instinct is to forge it.
        let err = CfxError::ValidateRefused {
            status: 403,
            user_agent: user_agent(),
        };
        let text = err.to_string();
        assert!(text.contains("403"));
        assert!(text.contains("does not impersonate FXServer"));
    }
}
