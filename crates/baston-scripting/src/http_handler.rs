//! Inbound HTTP for server scripts (`SetHttpHandler`).
//!
//! The counterpart of [`crate::http_bridge`]: instead of a resource calling
//! out, the outside world calls in. FXServer exposes every resource that
//! registered a handler under `http://<server>:<port>/<resource>/<path>`, and
//! resources use it for Discord OAuth callbacks, webhooks, in-game admin
//! panels and NUI-adjacent tooling.
//!
//! ## Shape
//!
//! The gateway parks a [`oneshot`] receiver keyed by request id, dispatches
//! the request into the owning resource as the [`HTTP_REQUEST_EVENT`] event,
//! and waits. The JS `response.send()` resolves the sender through an op. If
//! the handler never answers, the gateway's timeout fires and the entry is
//! abandoned — a resource that forgets to call `send()` costs one slow request,
//! not a leaked task.
//!
//! ## Deliberate simplification
//!
//! The engine streams both ways (`response.write()` before `send()`,
//! `request.setDataHandler()` fed as the body arrives). Here the body is read
//! whole before dispatch and `write()` buffers until `send()`. Every published
//! resource pattern works unchanged; only a handler streaming a large body
//! chunk-by-chunk sees the difference, and it sees higher memory, not wrong
//! output.

use std::sync::atomic::{AtomicU32, Ordering};

use dashmap::DashMap;
use tokio::sync::oneshot;

/// The event an inbound request is dispatched through.
pub const HTTP_REQUEST_EVENT: &str = "__cfx_internal:httpRequest";

/// What a resource's handler produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptHttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Default for ScriptHttpResponse {
    fn default() -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body: String::new(),
        }
    }
}

/// Registry of resources exposing an HTTP handler, plus the requests currently
/// waiting on one.
#[derive(Default)]
pub struct HttpHandlerRegistry {
    /// Resource name -> registered. A `DashMap` with a unit value rather than
    /// a set, to stay on the one concurrent map dependency already in use.
    handlers: DashMap<String, ()>,
    pending: DashMap<u32, oneshot::Sender<ScriptHttpResponse>>,
    next_id: AtomicU32,
}

impl HttpHandlerRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// `SetHttpHandler(handler)`. Re-registering replaces the previous handler,
    /// which is what a resource restart does.
    pub fn register(&self, resource: &str) {
        self.handlers.insert(resource.to_owned(), ());
    }

    /// Drop a stopped resource's registration so its route stops answering.
    pub fn unregister(&self, resource: &str) {
        self.handlers.remove(resource);
    }

    #[must_use]
    pub fn has_handler(&self, resource: &str) -> bool {
        self.handlers.contains_key(resource)
    }

    /// Park a request. The caller dispatches the event, then awaits the
    /// receiver under its own deadline.
    #[must_use]
    pub fn begin(&self) -> (u32, oneshot::Receiver<ScriptHttpResponse>) {
        // Never hand out 0: it is the "no such request" value on the JS side.
        // The loop skips it once per wrap-around, not once per request.
        let id = loop {
            let candidate = self.next_id.fetch_add(1, Ordering::Relaxed);
            if candidate != 0 {
                break candidate;
            }
        };
        let (tx, rx) = oneshot::channel();
        self.pending.insert(id, tx);
        (id, rx)
    }

    /// `response.send()`. Returns whether a waiter was still there — a second
    /// `send()`, or one after the gateway timed out, is a no-op.
    pub fn complete(&self, id: u32, response: ScriptHttpResponse) -> bool {
        match self.pending.remove(&id) {
            Some((_, tx)) => tx.send(response).is_ok(),
            None => false,
        }
    }

    /// Give up on a request (timeout, or the resource went away). Dropping the
    /// sender is what closes the channel.
    pub fn abandon(&self, id: u32) {
        self.pending.remove(&id);
    }

    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_registered_resources_are_routable() {
        let registry = HttpHandlerRegistry::new();
        assert!(!registry.has_handler("panel"));
        registry.register("panel");
        assert!(registry.has_handler("panel"));
        registry.unregister("panel");
        assert!(!registry.has_handler("panel"));
    }

    #[tokio::test]
    async fn a_completed_request_reaches_its_waiter() {
        let registry = HttpHandlerRegistry::new();
        let (id, rx) = registry.begin();
        let response = ScriptHttpResponse {
            status: 201,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: "{}".into(),
        };
        assert!(registry.complete(id, response.clone()));
        assert_eq!(rx.await.expect("delivered"), response);
    }

    /// A handler calling `send()` twice must not panic or resurrect a request.
    #[tokio::test]
    async fn a_second_completion_is_a_no_op() {
        let registry = HttpHandlerRegistry::new();
        let (id, _rx) = registry.begin();
        assert!(registry.complete(id, ScriptHttpResponse::default()));
        assert!(!registry.complete(id, ScriptHttpResponse::default()));
    }

    /// After the gateway gives up, a late `send()` finds nothing and the entry
    /// is gone rather than pinned until the process exits.
    #[tokio::test]
    async fn an_abandoned_request_closes_its_channel() {
        let registry = HttpHandlerRegistry::new();
        let (id, rx) = registry.begin();
        registry.abandon(id);
        assert_eq!(registry.pending_count(), 0);
        assert!(rx.await.is_err(), "the sender was dropped");
        assert!(!registry.complete(id, ScriptHttpResponse::default()));
    }

    #[test]
    fn ids_are_unique_and_never_zero() {
        let registry = HttpHandlerRegistry::new();
        let (first, _a) = registry.begin();
        let (second, _b) = registry.begin();
        assert_ne!(first, second);
        assert!(first > 0);
    }
}
