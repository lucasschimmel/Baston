//! CFX server-licence status and local, restrictive entitlement enforcement.
//!
//! BASTON never validates a licence itself and never speaks to any CFX service.
//! The genuine, unmodified CFX component (`svadhesive` inside a real FXServer)
//! performs the validation as a legitimate server; this module only models the
//! *result* that component reports locally and enforces it **restrictively** —
//! it can lower a limit or keep a feature off, never raise a limit or unlock a
//! feature that was not granted.
//!
//! The status is produced by an optional plugin (`baston-escrow-plugin`) that
//! reads local signals (a Lua shim reading `sv_licenseKeyToken`) from the
//! sidecar. The core stays free of any FXServer/CFX dependency: it consumes a
//! plain [`LicenseStatus`] and applies pure decision logic.

use std::fmt;

/// Authenticated CFX server token.
///
/// The inner value is intentionally excluded from [`Debug`] output so routine
/// error context and structured traces cannot disclose it.
#[derive(Clone, PartialEq, Eq)]
pub struct LicenseKeyToken(String);

impl LicenseKeyToken {
    /// Build a token from a non-empty value, trimming accidental surrounding
    /// whitespace introduced by the local IPC boundary.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into().trim().to_owned();
        (!value.is_empty()).then_some(Self(value))
    }

    /// Expose the token only at an explicit protocol boundary.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for LicenseKeyToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LicenseKeyToken([REDACTED])")
    }
}

/// Entitlements granted by the licence, as far as they are **locally
/// determinable** from the official component.
///
/// A field left `None`/empty means "no clean local signal for this" — per the
/// project's compliance rule, BASTON must then presume **nothing** (it neither
/// raises a limit nor enables a feature on an unconfirmed grant).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Entitlements {
    /// Maximum player slots the licence grants, when locally determinable.
    /// `None` = not determinable → do not cap on this basis.
    pub max_slots: Option<u32>,
    /// Feature grants reported by the component (e.g. `"onesync"`).
    pub features: Vec<String>,
}

impl Entitlements {
    /// Case-insensitive feature-grant check.
    pub fn grants_feature(&self, feature: &str) -> bool {
        self.features
            .iter()
            .any(|f| f.eq_ignore_ascii_case(feature))
    }
}

/// The licence verdict reported by the official CFX component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LicenseStatus {
    /// The component accepted the key (locally: `sv_licenseKeyToken` present).
    pub valid: bool,
    /// The key is known-banned/revoked. Note: on the current clean local signal
    /// this cannot always be distinguished from "invalid"; treat `!valid` as a
    /// hard stop regardless. Kept distinct for future richer signals.
    pub banned: bool,
    /// Token issued by CFX after successful validation.
    pub token: Option<LicenseKeyToken>,
    /// Entitlements, to the extent locally determinable.
    pub entitlements: Entitlements,
    /// Human-readable explanation for a non-valid verdict (never a secret).
    pub reason: Option<String>,
}

impl LicenseStatus {
    /// An authenticated status with no extra entitlement signal.
    pub fn authenticated(token: LicenseKeyToken) -> Self {
        Self {
            valid: true,
            banned: false,
            token: Some(token),
            entitlements: Entitlements::default(),
            reason: None,
        }
    }

    /// An invalid status carrying an explanation.
    pub fn invalid(reason: impl Into<String>) -> Self {
        Self {
            valid: false,
            banned: false,
            token: None,
            entitlements: Entitlements::default(),
            reason: Some(reason.into()),
        }
    }

    /// Whether this verdict contains every signal required to represent an
    /// authenticated, usable CFX server identity.
    #[must_use]
    pub fn is_authenticated(&self) -> bool {
        self.valid && !self.banned && self.token.is_some()
    }
}

/// Whether BASTON may boot given a licence status. **Fail-closed**: anything
/// other than an explicitly valid, non-banned licence denies startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootDecision {
    Allow,
    Deny(String),
}

impl BootDecision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, BootDecision::Allow)
    }
}

/// Decide whether to boot from a licence status. Restrictive by construction.
pub fn boot_decision(status: &LicenseStatus) -> BootDecision {
    if status.banned {
        return BootDecision::Deny(status.reason.clone().unwrap_or_else(|| {
            "licence banned or revoked (reported by the CFX component)".to_owned()
        }));
    }
    if !status.valid {
        return BootDecision::Deny(
            status
                .reason
                .clone()
                .unwrap_or_else(|| "licence not valid (reported by the CFX component)".to_owned()),
        );
    }
    if status.token.is_none() {
        return BootDecision::Deny(
            "licence accepted without an authenticated CFX token".to_owned(),
        );
    }
    BootDecision::Allow
}

/// Restrictive slot enforcement. Returns `(effective, capped)` where `effective
/// <= configured` always. A configured value at or below the entitlement, or an
/// undetermined entitlement (`None`), is returned unchanged.
pub fn effective_max_players(configured: u32, entitlements: &Entitlements) -> (u32, bool) {
    match entitlements.max_slots {
        Some(limit) if configured > limit => (limit, true),
        _ => (configured, false),
    }
}

/// Restrictive feature gate: a feature is permitted only when it is both
/// requested by the operator **and** granted by the licence. An unconfirmed
/// grant never enables anything.
pub fn feature_permitted(requested: bool, feature: &str, entitlements: &Entitlements) -> bool {
    requested && entitlements.grants_feature(feature)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn licence_key_token_is_non_empty_and_redacted() {
        assert!(LicenseKeyToken::new("   ").is_none());

        let token = LicenseKeyToken::new("cfx-token-secret").expect("non-empty token");
        assert_eq!(token.as_str(), "cfx-token-secret");
        assert_eq!(format!("{token:?}"), "LicenseKeyToken([REDACTED])");
        assert!(!format!("{token:?}").contains(token.as_str()));
    }

    fn ent(max: Option<u32>, features: &[&str]) -> Entitlements {
        Entitlements {
            max_slots: max,
            features: features.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn valid_licence_allows_boot() {
        let token = LicenseKeyToken::new("authenticated").unwrap();
        assert_eq!(
            boot_decision(&LicenseStatus::authenticated(token)),
            BootDecision::Allow
        );
    }

    #[test]
    fn nominally_valid_licence_without_token_denies_boot() {
        let status = LicenseStatus {
            valid: true,
            banned: false,
            token: None,
            entitlements: Entitlements::default(),
            reason: None,
        };
        assert!(!boot_decision(&status).is_allowed());
        assert!(!status.is_authenticated());
    }

    #[test]
    fn invalid_licence_denies_boot_with_reason() {
        let s = LicenseStatus::invalid("token empty");
        match boot_decision(&s) {
            BootDecision::Deny(r) => assert!(r.contains("token empty")),
            BootDecision::Allow => panic!("must deny"),
        }
    }

    #[test]
    fn banned_licence_denies_even_if_marked_valid() {
        let s = LicenseStatus {
            valid: true,
            banned: true,
            token: LicenseKeyToken::new("revoked"),
            entitlements: Entitlements::default(),
            reason: Some("revoked".into()),
        };
        assert!(!boot_decision(&s).is_allowed());
    }

    #[test]
    fn slots_capped_to_entitlement() {
        assert_eq!(effective_max_players(64, &ent(Some(48), &[])), (48, true));
    }

    #[test]
    fn slots_unchanged_when_below_entitlement() {
        assert_eq!(effective_max_players(32, &ent(Some(48), &[])), (32, false));
    }

    #[test]
    fn slots_unchanged_when_entitlement_undetermined() {
        // No clean signal → never cap, never raise: return configured as-is.
        assert_eq!(effective_max_players(64, &ent(None, &[])), (64, false));
    }

    #[test]
    fn feature_permitted_only_when_requested_and_granted() {
        let e = ent(None, &["onesync"]);
        assert!(feature_permitted(true, "onesync", &e));
        assert!(feature_permitted(true, "OneSync", &e)); // case-insensitive
        assert!(!feature_permitted(false, "onesync", &e)); // not requested
        assert!(!feature_permitted(true, "streaming", &e)); // not granted
    }
}
