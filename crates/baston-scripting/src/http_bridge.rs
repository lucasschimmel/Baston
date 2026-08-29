//! Outbound HTTP for server scripts (`PerformHttpRequest`).
//!
//! Half the FiveM ecosystem is built on this native: database adapters,
//! Discord webhooks, any external API a resource talks to. Until now it fell
//! into the unimplemented fallback, which returned a token that never
//! resolved — so a script waiting on the callback simply waited forever.
//!
//! ## Why a bridge and not a direct call
//!
//! An op cannot await. So the op validates the request, allocates a token and
//! hands it to a worker the composition root owns; the worker performs the
//! call and dispatches the reply back into the calling resource as the
//! `__cfx_internal:httpResponse` event the runtime already listens for. That
//! is the same shape the engine uses, and it keeps a slow endpoint off the
//! isolate entirely.

use std::sync::atomic::{AtomicU32, Ordering};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// The event a completed request is reported through.
pub const HTTP_RESPONSE_EVENT: &str = "__cfx_internal:httpResponse";

/// Queue depth for in-flight requests. A resource looping on
/// `PerformHttpRequest` without backpressure hits this rather than growing the
/// queue until the process dies.
const REQUEST_CAPACITY: usize = 1024;

/// One outbound request, as the op parsed it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpRequest {
    /// Resource that asked, and that the reply is dispatched back to.
    pub resource: String,
    /// Correlation token the script holds.
    pub token: u32,
    pub url: String,
    pub method: String,
    pub body: String,
    pub headers: Vec<(String, String)>,
}

/// What the worker learned, ready to hand back to the script.
#[derive(Debug, Clone)]
pub struct HttpReply {
    pub resource: String,
    pub token: u32,
    /// HTTP status, or `0` when the request never completed.
    pub status: u16,
    pub body: String,
    pub headers: Vec<(String, String)>,
    /// Populated only on failure; the script's callback receives it as its
    /// error argument, so a failed request is distinguishable from an empty
    /// 200 rather than silently looking like success.
    pub error: Option<String>,
}

/// Handle stored in every isolate's op state.
#[derive(Clone)]
pub struct HttpBridge {
    tx: mpsc::Sender<HttpRequest>,
    next_token: std::sync::Arc<AtomicU32>,
}

impl HttpBridge {
    /// Build the bridge and the receiver the worker drains.
    #[must_use]
    pub fn new() -> (Self, mpsc::Receiver<HttpRequest>) {
        let (tx, rx) = mpsc::channel(REQUEST_CAPACITY);
        (
            Self {
                tx,
                next_token: std::sync::Arc::new(AtomicU32::new(1)),
            },
            rx,
        )
    }

    /// Queue a request and return its token, or `None` when the queue is full
    /// or the worker is gone. A script gets `0` back and its callback never
    /// fires, which is the same outcome as a dropped request but visible in
    /// the logs.
    pub fn submit(&self, request: impl FnOnce(u32) -> HttpRequest) -> Option<u32> {
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        let request = request(token);
        match self.tx.try_send(request) {
            Ok(()) => Some(token),
            Err(_) => {
                tracing::error!(
                    target: "http",
                    "outbound HTTP request dropped: the request queue is full"
                );
                metrics::counter!("script_http_dropped_total").increment(1);
                None
            }
        }
    }
}

/// Parse the JSON request object the native carries.
///
/// The engine accepts `{ url, method, data, headers }`; anything else is
/// ignored rather than rejected, because a resource written against a newer
/// FiveM must not break on a field we do not read yet.
#[must_use]
pub fn parse_request(resource: &str, raw: &str) -> Option<HttpRequestSpec> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let url = value.get("url")?.as_str()?.to_owned();
    let method = value
        .get("method")
        .and_then(|m| m.as_str())
        .unwrap_or("GET")
        .to_ascii_uppercase();
    let body = value
        .get("data")
        .map(|d| match d {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .unwrap_or_default();
    let headers = value
        .get("headers")
        .and_then(|h| h.as_object())
        .map(|map| {
            map.iter()
                .map(|(name, value)| {
                    let value = match value {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    (name.clone(), value)
                })
                .collect()
        })
        .unwrap_or_default();
    Some(HttpRequestSpec {
        resource: resource.to_owned(),
        url,
        method,
        body,
        headers,
    })
}

/// A validated request, still missing its token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequestSpec {
    pub resource: String,
    pub url: String,
    pub method: String,
    pub body: String,
    pub headers: Vec<(String, String)>,
}

impl HttpRequestSpec {
    #[must_use]
    pub fn with_token(self, token: u32) -> HttpRequest {
        HttpRequest {
            resource: self.resource,
            token,
            url: self.url,
            method: self.method,
            body: self.body,
            headers: self.headers,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minimal_request_defaults_to_get() {
        let spec = parse_request("r", r#"{"url":"https://example.test/x"}"#).expect("parsed");
        assert_eq!(spec.method, "GET");
        assert!(spec.body.is_empty());
        assert!(spec.headers.is_empty());
    }

    #[test]
    fn method_headers_and_body_are_carried() {
        let raw = r#"{"url":"https://api.test/v1","method":"post","data":"{\"a\":1}",
                      "headers":{"Content-Type":"application/json"}}"#;
        let spec = parse_request("r", raw).expect("parsed");
        assert_eq!(spec.method, "POST", "the method is normalised");
        assert_eq!(spec.body, "{\"a\":1}");
        assert_eq!(
            spec.headers,
            vec![("Content-Type".to_owned(), "application/json".to_owned())]
        );
    }

    /// A resource written against a newer FiveM may send fields we do not read.
    /// Ignoring them beats refusing the request.
    #[test]
    fn unknown_fields_are_ignored() {
        let raw = r#"{"url":"https://example.test/","followLocation":true,"future":42}"#;
        assert!(parse_request("r", raw).is_some());
    }

    #[test]
    fn a_request_without_a_url_is_refused() {
        assert!(parse_request("r", r#"{"method":"GET"}"#).is_none());
        assert!(parse_request("r", "not json").is_none());
    }

    #[test]
    fn tokens_are_unique_and_never_zero() {
        let (bridge, _rx) = HttpBridge::new();
        let spec = parse_request("r", r#"{"url":"https://example.test/"}"#).unwrap();
        let first = bridge.submit(|t| spec.clone().with_token(t)).unwrap();
        let second = bridge.submit(|t| spec.clone().with_token(t)).unwrap();
        assert_ne!(first, second);
        assert!(first > 0, "0 is the failure token");
    }
}
