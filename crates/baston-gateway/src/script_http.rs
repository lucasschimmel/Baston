//! Outbound HTTP worker for `PerformHttpRequest`.
//!
//! Drains the queue [`baston_scripting::HttpBridge`] fills, performs each call
//! with reqwest, and dispatches the result back into the calling resource as
//! the `__cfx_internal:httpResponse` event its bootstrap listens for.
//!
//! Lives in the gateway rather than in baston-scripting so the scripting crate
//! keeps no opinion about which HTTP client the process uses — and so an
//! isolate can never block on a socket.

use std::sync::Arc;
use std::time::Duration;

use baston_scripting::{HttpRequest, ScriptHost, HTTP_RESPONSE_EVENT};
use tokio::sync::mpsc;

/// Limits applied to every outbound request.
#[derive(Clone, Copy)]
pub struct OutboundHttpPolicy {
    pub timeout: Duration,
    pub concurrency: usize,
    pub max_response_bytes: usize,
}

impl OutboundHttpPolicy {
    #[must_use]
    pub fn new(config: &baston_config::ResourcesConfig) -> Self {
        Self {
            timeout: Duration::from_secs(config.http_request_timeout_secs),
            concurrency: config.http_concurrency,
            max_response_bytes: config.http_response_max_bytes,
        }
    }
}

/// Spawn the worker. It ends when the last [`HttpBridge`] handle is dropped,
/// i.e. when the script host is gone.
///
/// [`HttpBridge`]: baston_scripting::HttpBridge
pub fn spawn_worker(
    mut requests: mpsc::Receiver<HttpRequest>,
    script_host: ScriptHost,
    policy: OutboundHttpPolicy,
) {
    let client = match reqwest::Client::builder()
        // Total deadline, not per-read: a trickling endpoint still releases
        // its slot.
        .timeout(policy.timeout)
        .user_agent(concat!("BASTON/", env!("CARGO_PKG_VERSION")))
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            tracing::error!(
                target: "http",
                error = %e,
                "outbound HTTP is disabled: the client could not be built"
            );
            return;
        }
    };

    // Bounds concurrent sockets, not the queue: a resource looping on a slow
    // endpoint waits here instead of opening a connection per iteration.
    let permits = Arc::new(tokio::sync::Semaphore::new(policy.concurrency));

    tokio::spawn(async move {
        while let Some(request) = requests.recv().await {
            let Ok(permit) = Arc::clone(&permits).acquire_owned().await else {
                break;
            };
            let client = client.clone();
            let script_host = script_host.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let reply = perform(&client, &request, policy.max_response_bytes).await;
                dispatch_reply(&script_host, &request, reply).await;
            });
        }
        tracing::debug!(target: "http", "outbound HTTP worker stopped");
    });
}

/// The outcome of one request, in the shape the script callback expects.
struct Outcome {
    status: u16,
    body: String,
    headers: serde_json::Value,
    error: Option<String>,
}

impl Outcome {
    /// A request that never produced a response. Status 0 is what FXServer
    /// reports for a transport failure, so resources already branch on it.
    fn failed(error: String) -> Self {
        Self {
            status: 0,
            body: String::new(),
            headers: serde_json::json!({}),
            error: Some(error),
        }
    }
}

async fn perform(client: &reqwest::Client, request: &HttpRequest, max_bytes: usize) -> Outcome {
    // Only http(s). Without this, `file://` and any scheme reqwest grows later
    // become a way for a resource to read the host through a native that is
    // supposed to reach the network.
    let url = match reqwest::Url::parse(&request.url) {
        Ok(url) if matches!(url.scheme(), "http" | "https") => url,
        Ok(url) => return Outcome::failed(format!("unsupported URL scheme '{}'", url.scheme())),
        Err(e) => return Outcome::failed(format!("invalid URL: {e}")),
    };

    let method = match reqwest::Method::from_bytes(request.method.as_bytes()) {
        Ok(method) => method,
        Err(_) => return Outcome::failed(format!("invalid HTTP method '{}'", request.method)),
    };

    let mut builder = client.request(method, url);
    for (name, value) in &request.headers {
        builder = builder.header(name, value);
    }
    if !request.body.is_empty() {
        builder = builder.body(request.body.clone());
    }

    let response = match builder.send().await {
        Ok(response) => response,
        Err(e) => return Outcome::failed(e.to_string()),
    };

    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            // A non-UTF-8 header value is dropped rather than lossily
            // converted: a script comparing it would get a silent mismatch.
            value
                .to_str()
                .ok()
                .map(|v| (name.as_str().to_owned(), serde_json::json!(v)))
        })
        .collect::<serde_json::Map<_, _>>();

    // Refuse oversized bodies before buffering them when the endpoint
    // announces the size; otherwise stop once the cap is crossed.
    if let Some(len) = response.content_length() {
        if len > max_bytes as u64 {
            return Outcome::failed(format!(
                "response of {len} bytes exceeds the {max_bytes} byte limit"
            ));
        }
    }

    let body = match read_capped(response, max_bytes).await {
        Ok(body) => body,
        Err(e) => return Outcome::failed(e),
    };

    Outcome {
        status,
        body,
        headers: serde_json::Value::Object(headers),
        error: None,
    }
}

/// Buffer the body, stopping at `max_bytes` rather than trusting
/// `Content-Length` (absent on chunked responses).
async fn read_capped(response: reqwest::Response, max_bytes: usize) -> Result<String, String> {
    let mut response = response;
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if buffer.len() + chunk.len() > max_bytes {
                    return Err(format!("response exceeds the {max_bytes} byte limit"));
                }
                buffer.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(e) => return Err(e.to_string()),
        }
    }
    // Lossy on purpose: the native's contract is a string, and a body that is
    // almost-UTF-8 is more useful to a script than an error.
    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

async fn dispatch_reply(script_host: &ScriptHost, request: &HttpRequest, outcome: Outcome) {
    if let Some(error) = &outcome.error {
        tracing::warn!(
            target: "http",
            resource = %request.resource,
            url = %request.url,
            %error,
            "outbound HTTP request failed"
        );
        metrics::counter!("script_http_requests_failed_total").increment(1);
    } else {
        metrics::counter!("script_http_requests_total").increment(1);
    }

    let args = [
        serde_json::json!(request.token),
        serde_json::json!(outcome.status),
        serde_json::json!(outcome.body),
        outcome.headers,
        match outcome.error {
            Some(error) => serde_json::json!(error),
            None => serde_json::Value::Null,
        },
    ];
    if let Err(e) = script_host
        .trigger_event_on(&request.resource, HTTP_RESPONSE_EVENT, &args)
        .await
    {
        tracing::warn!(
            target: "http",
            resource = %request.resource,
            error = %e,
            "could not deliver an HTTP response to its resource"
        );
    }
}
