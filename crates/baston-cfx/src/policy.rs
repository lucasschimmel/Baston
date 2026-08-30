//! What the licence grants, and the slot ceiling that follows from it.
//!
//! The authority here is the FiveM client, not BASTON. `NetLibrary.cpp` reads
//! `sv_licenseKeyToken` out of `/info.json`, fetches
//! `policy-live.fivem.net/api/policy/<token>`, and refuses the connection when
//! the slot count implies an entitlement string the policy does not contain.
//!
//! BASTON reads the same list from the same endpoint and applies the same
//! ladder **before opening a listener**. That is deliberately stricter than
//! FXServer, which lets a misconfigured server boot and accept players who are
//! then bounced at connect time. Failing at boot costs the operator one
//! restart; failing at connect costs them every player who tries.
//!
//! The rule this module never breaks: a policy may **lower** the configured
//! slot count. It may never raise it, and an unreadable policy grants nothing.

use std::collections::BTreeSet;

/// Slot ceilings, mirroring the ladder in `NetLibrary.cpp`.
const CEILING_UNLICENSED: u32 = 48;
const CEILING_ONESYNC: u32 = 64;
const CEILING_PLUS: u32 = 128;
const CEILING_BIG: u32 = 2048;

/// The entitlement strings BASTON understands. Anything else CFX returns is
/// kept in the set (so `contains` and diagnostics stay truthful) but does not
/// participate in the ladder.
const P_ONESYNC: &str = "onesync";
const P_PLUS: &str = "onesync_plus";
const P_MEDIUM: &str = "onesync_medium";
const P_BIG: &str = "onesync_big";

/// The entitlement strings CFX returned for this server's token.
///
/// An empty set is a valid, meaningful answer — it is what a free key returns —
/// and is not distinguishable from "no entitlements" by design.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PolicySet(BTreeSet<String>);

impl PolicySet {
    pub fn from_strings<I, S>(entries: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self(entries.into_iter().map(Into::into).collect())
    }

    #[must_use]
    pub fn grants(&self, entitlement: &str) -> bool {
        self.0.contains(entitlement)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The granted strings, for `--modules`-style diagnostics. Entitlement
    /// names are not secret; the token that produced them is.
    pub fn entries(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }

    /// The highest slot count this policy permits under OneSync.
    ///
    /// Note the deliberate difference from the client at the top end.
    /// `NetLibrary.cpp` initialises `onesyncType` to `"onesync"` and only
    /// overwrites it in `<= 48 / 64 / 128 / 2048` branches, so a server
    /// declaring **more** than 2048 falls through every branch and is checked
    /// against plain `"onesync"` — a cheaper entitlement than the 2048 tier it
    /// exceeds. That is a client-side oversight, and building on it would be
    /// taking slots the licence did not grant. BASTON caps at 2048.
    #[must_use]
    pub fn slot_ceiling(&self) -> u32 {
        if self.grants(P_BIG) {
            CEILING_BIG
        } else if self.grants(P_PLUS) || self.grants(P_MEDIUM) {
            CEILING_PLUS
        } else if self.grants(P_ONESYNC) {
            CEILING_ONESYNC
        } else {
            CEILING_UNLICENSED
        }
    }
}

/// The outcome of applying a policy to a configured slot count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotDecision {
    /// What the server will actually run with.
    pub effective: u32,
    /// What the operator asked for.
    pub configured: u32,
    /// The ceiling that applied, or `None` when none did.
    pub ceiling: Option<u32>,
}

impl SlotDecision {
    #[must_use]
    pub fn was_capped(&self) -> bool {
        self.effective < self.configured
    }
}

/// Apply the policy restrictively.
///
/// `onesync_enabled` matters because the client's check is gated on it: with
/// OneSync off, no policy string is ever required, so there is no ceiling to
/// apply and BASTON does not invent one. Non-OneSync sessions are bounded by
/// the game itself, not by the licence.
#[must_use]
pub fn decide_slots(configured: u32, policy: &PolicySet, onesync_enabled: bool) -> SlotDecision {
    if !onesync_enabled {
        return SlotDecision {
            effective: configured,
            configured,
            ceiling: None,
        };
    }
    let ceiling = policy.slot_ceiling();
    SlotDecision {
        effective: configured.min(ceiling),
        configured,
        ceiling: Some(ceiling),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(entries: &[&str]) -> PolicySet {
        PolicySet::from_strings(entries.iter().copied())
    }

    #[test]
    fn an_empty_policy_is_the_free_tier_ceiling() {
        // A free (Pebble) key returns []. That is a real answer, not a failure.
        assert_eq!(policy(&[]).slot_ceiling(), 48);
    }

    #[test]
    fn each_rung_of_the_ladder_matches_the_client() {
        assert_eq!(policy(&["onesync"]).slot_ceiling(), 64);
        assert_eq!(policy(&["onesync_plus"]).slot_ceiling(), 128);
        assert_eq!(policy(&["onesync_medium"]).slot_ceiling(), 128);
        assert_eq!(policy(&["onesync_big"]).slot_ceiling(), 2048);
    }

    #[test]
    fn the_highest_granted_rung_wins_regardless_of_set_order() {
        assert_eq!(policy(&["onesync", "onesync_big"]).slot_ceiling(), 2048);
        assert_eq!(policy(&["onesync_big", "onesync"]).slot_ceiling(), 2048);
    }

    #[test]
    fn unknown_entitlements_do_not_raise_the_ceiling() {
        // CFX can add strings at any time. A name BASTON has never heard of
        // must not be read as permission for anything.
        assert_eq!(policy(&["onesync_enormous", "beyond"]).slot_ceiling(), 48);
    }

    #[test]
    fn a_policy_lowers_a_configured_count_and_says_so() {
        let d = decide_slots(3000, &policy(&["onesync_big"]), true);
        assert_eq!(d.effective, 2048);
        assert_eq!(d.configured, 3000);
        assert!(d.was_capped());
    }

    #[test]
    fn a_policy_never_raises_a_configured_count() {
        // The whole point: onesync_big grants 2048, but the operator asked for
        // 32 and gets 32.
        let d = decide_slots(32, &policy(&["onesync_big"]), true);
        assert_eq!(d.effective, 32);
        assert!(!d.was_capped());
    }

    #[test]
    fn exceeding_two_thousand_forty_eight_caps_rather_than_falling_through() {
        // NetLibrary.cpp checks a >2048 server against plain "onesync" because
        // its if/else chain has no branch for that range. Honouring that would
        // let a 64-slot entitlement carry an 8000-slot server.
        let d = decide_slots(8000, &policy(&["onesync"]), true);
        assert_eq!(d.effective, 64, "must use the granted tier, not the gap");

        let d = decide_slots(8000, &policy(&["onesync_big"]), true);
        assert_eq!(d.effective, 2048, "the top tier is still a ceiling");
    }

    #[test]
    fn with_onesync_off_there_is_no_ceiling_to_apply() {
        // The client only runs the check when OneSync is on, so applying one
        // here would be inventing a restriction rather than enforcing one.
        let d = decide_slots(300, &policy(&[]), false);
        assert_eq!(d.effective, 300);
        assert_eq!(d.ceiling, None);
        assert!(!d.was_capped());
    }

    #[test]
    fn a_boundary_count_is_permitted_by_its_own_tier() {
        // <= is the client's comparison; 64 must pass on "onesync", not fail.
        assert_eq!(decide_slots(64, &policy(&["onesync"]), true).effective, 64);
        assert_eq!(decide_slots(65, &policy(&["onesync"]), true).effective, 64);
        assert_eq!(decide_slots(48, &policy(&[]), true).effective, 48);
        assert_eq!(decide_slots(49, &policy(&[]), true).effective, 48);
    }
}
