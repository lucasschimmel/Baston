//! Connection deferral state shared between the HTTP gateway and the JS runtimes.
//!
//! One [`DeferralEntry`] exists per in-flight `playerConnecting`. The gateway
//! registers an entry (getting a oneshot receiver back), the JS `deferrals`
//! object resolves it through the `op_deferral_*` ops.

use dashmap::DashMap;
use tokio::sync::oneshot;

/// Outcome of a `playerConnecting` deferral: `Ok(())` = allowed,
/// `Err(reason)` = rejected with a kick reason.
pub type DeferralOutcome = Result<(), String>;

struct DeferralEntry {
    tx: Option<oneshot::Sender<DeferralOutcome>>,
    deferred: bool,
    kick_reason: Option<String>,
    last_update: Option<String>,
}

/// Registry of in-flight connection deferrals, keyed by player source id.
#[derive(Default)]
pub struct DeferralRegistry {
    inner: DashMap<u32, DeferralEntry>,
}

impl DeferralRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new in-flight connection. Returns the receiver the gateway
    /// awaits (with a timeout) for the outcome.
    pub fn register(&self, source: u32) -> oneshot::Receiver<DeferralOutcome> {
        let (tx, rx) = oneshot::channel();
        self.inner.insert(
            source,
            DeferralEntry {
                tx: Some(tx),
                deferred: false,
                kick_reason: None,
                last_update: None,
            },
        );
        rx
    }

    /// `deferrals.defer()` — marks the connection as asynchronously handled.
    pub fn defer(&self, source: u32) {
        if let Some(mut entry) = self.inner.get_mut(&source) {
            entry.deferred = true;
        }
    }

    /// `deferrals.update(msg)` — progress message shown on the player's
    /// loading screen. Phase A: logged only (no UDP channel to the client yet).
    pub fn update(&self, source: u32, message: String) {
        tracing::info!(target: "deferrals", source, %message, "deferral update");
        if let Some(mut entry) = self.inner.get_mut(&source) {
            entry.last_update = Some(message);
        }
    }

    /// `deferrals.presentCard(card)` — Phase A stub, logged only.
    pub fn present_card(&self, source: u32, card_json: String) {
        tracing::info!(target: "deferrals", source, card = %card_json, "presentCard (Phase A stub)");
    }

    /// `setKickReason(reason)` — stored; used if the connection is cancelled
    /// without an explicit `done(reason)`.
    pub fn set_kick_reason(&self, source: u32, reason: String) {
        if let Some(mut entry) = self.inner.get_mut(&source) {
            entry.kick_reason = Some(reason);
        }
    }

    /// `deferrals.done()` / `deferrals.done(reason)`. An empty reason accepts
    /// the connection. Subsequent calls for the same source are no-ops.
    pub fn done(&self, source: u32, reason: Option<String>) {
        if let Some(mut entry) = self.inner.get_mut(&source) {
            if let Some(tx) = entry.tx.take() {
                let outcome = match reason.filter(|r| !r.is_empty()) {
                    Some(r) => Err(r),
                    None => Ok(()),
                };
                // Receiver dropped (gateway timed out) — nothing left to notify.
                let _ = tx.send(outcome);
            }
        }
    }

    /// Called by the script host after every `playerConnecting` handler ran:
    /// if no handler called `defer()` (and none resolved it yet), the
    /// connection proceeds — matching FXServer semantics.
    pub fn resolve_if_not_deferred(&self, source: u32) {
        let should_resolve =
            matches!(self.inner.get(&source), Some(e) if !e.deferred && e.tx.is_some());
        if should_resolve {
            self.done(source, None);
        }
    }

    /// Whether the deferral is still unresolved.
    pub fn is_pending(&self, source: u32) -> bool {
        matches!(self.inner.get(&source), Some(e) if e.tx.is_some())
    }

    /// Drop all state for a source (after the connection flow finishes).
    pub fn remove(&self, source: u32) {
        self.inner.remove(&source);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn done_without_reason_accepts() {
        let reg = DeferralRegistry::new();
        let rx = reg.register(1);
        reg.done(1, None);
        assert_eq!(rx.await.unwrap(), Ok(()));
    }

    #[tokio::test]
    async fn done_with_reason_rejects() {
        let reg = DeferralRegistry::new();
        let rx = reg.register(2);
        reg.defer(2);
        reg.done(2, Some("Not whitelisted".into()));
        assert_eq!(rx.await.unwrap(), Err("Not whitelisted".into()));
    }

    #[tokio::test]
    async fn not_deferred_auto_resolves() {
        let reg = DeferralRegistry::new();
        let rx = reg.register(3);
        reg.resolve_if_not_deferred(3);
        assert_eq!(rx.await.unwrap(), Ok(()));
    }

    #[tokio::test]
    async fn deferred_does_not_auto_resolve() {
        let reg = DeferralRegistry::new();
        let _rx = reg.register(4);
        reg.defer(4);
        reg.resolve_if_not_deferred(4);
        assert!(reg.is_pending(4));
    }
}
