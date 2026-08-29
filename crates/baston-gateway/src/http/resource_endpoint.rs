//! `ANY /{resource}/{*path}` — the endpoint a resource exposes with
//! `SetHttpHandler`.
//!
//! FXServer serves these off the same port as `/info.json` and the file
//! routes, and resources hardcode that shape (Discord OAuth callbacks, webhook
//! receivers, admin panels). The static routes are registered first and win on
//! axum's matcher, so this catch-all only sees paths no gateway route claims.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{Extensions, HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use baston_scripting::HTTP_REQUEST_EVENT;

use super::AppState;

/// Requests to `/{resource}` with no trailing path.
pub async fn serve_resource_root(
    state: State<Arc<AppState>>,
    extensions: Extensions,
    path: Path<String>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Path(resource) = path;
    serve(
        state,
        extensions,
        resource,
        "/".to_owned(),
        method,
        headers,
        body,
    )
    .await
}

/// Requests to `/{resource}/{*path}`.
pub async fn serve_resource_path(
    state: State<Arc<AppState>>,
    extensions: Extensions,
    path: Path<(String, String)>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Path((resource, rest)) = path;
    serve(
        state,
        extensions,
        resource,
        format!("/{rest}"),
        method,
        headers,
        body,
    )
    .await
}

async fn serve(
    State(state): State<Arc<AppState>>,
    extensions: Extensions,
    resource: String,
    path: String,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let registry = state.script_host.http_handlers();
    if !registry.has_handler(&resource) {
        // Indistinguishable from "no such resource" on purpose: probing this
        // endpoint must not enumerate which resources are loaded.
        return StatusCode::NOT_FOUND.into_response();
    }

    let max_bytes = state.config.resources.http_request_max_bytes;
    if body.len() > max_bytes {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("request body exceeds the {max_bytes} byte limit"),
        )
            .into_response();
    }

    let (id, reply) = registry.begin();
    let payload = serde_json::json!({
        "id": id,
        "method": method.as_str(),
        "path": path,
        // Read from the extensions rather than extracted: a router mounted
        // without connect info (tests, an embedded harness) then reports an
        // empty address instead of failing the request.
        "address": extensions
            .get::<ConnectInfo<std::net::SocketAddr>>()
            .map(|ConnectInfo(addr)| addr.to_string())
            .unwrap_or_default(),
        "headers": request_headers(&headers),
        "body": String::from_utf8_lossy(&body),
    });

    // Dispatch on a task rather than awaiting it: a handler that answers from
    // a callback resolves the request long before its own dispatch returns,
    // and one that never answers must not hold this connection past the
    // deadline below.
    let dispatch_host = state.script_host.clone();
    let dispatch_resource = resource.clone();
    tokio::spawn(async move {
        if let Err(e) = dispatch_host
            .trigger_event_on(&dispatch_resource, HTTP_REQUEST_EVENT, &[payload])
            .await
        {
            tracing::warn!(
                target: "http",
                resource = %dispatch_resource,
                error = %e,
                "could not dispatch an inbound HTTP request"
            );
        }
    });

    let deadline = Duration::from_secs(state.config.resources.http_handler_timeout_secs);
    match tokio::time::timeout(deadline, reply).await {
        Ok(Ok(response)) => {
            metrics::counter!("script_http_handler_requests_total").increment(1);
            build_response(response)
        }
        // The sender was dropped: the resource stopped mid-request.
        Ok(Err(_)) => {
            metrics::counter!("script_http_handler_failed_total").increment(1);
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
        Err(_) => {
            registry.abandon(id);
            tracing::warn!(
                target: "http",
                %resource,
                %path,
                "the resource's HTTP handler did not answer in time"
            );
            metrics::counter!("script_http_handler_timeouts_total").increment(1);
            StatusCode::GATEWAY_TIMEOUT.into_response()
        }
    }
}

/// Header map -> JSON object. Non-UTF-8 values are dropped rather than lossily
/// converted, so a handler comparing one never matches a mangled string.
fn request_headers(headers: &HeaderMap) -> serde_json::Value {
    let map = headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|v| (name.as_str().to_owned(), serde_json::json!(v)))
        })
        .collect::<serde_json::Map<_, _>>();
    serde_json::Value::Object(map)
}

fn build_response(response: baston_scripting::ScriptHttpResponse) -> Response {
    let status = StatusCode::from_u16(response.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut out = Response::new(axum::body::Body::from(response.body));
    *out.status_mut() = status;
    for (name, value) in response.headers {
        // A header a script made up can be invalid; skip it rather than
        // failing the whole response the handler already produced.
        let (Ok(name), Ok(value)) = (
            HeaderName::try_from(name.as_str()),
            HeaderValue::try_from(value.as_str()),
        ) else {
            tracing::debug!(target: "http", %name, "dropped an invalid response header");
            continue;
        };
        out.headers_mut().append(name, value);
    }
    out
}
