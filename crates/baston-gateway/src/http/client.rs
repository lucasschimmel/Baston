//! `POST /client` — Phase A connection endpoint.
//!
//! The real FiveM client posts url-encoded fields (`method=initConnect`,
//! `name=...`, tokens, ...). Phase A ignores auth entirely (`auth.bypass`)
//! and runs the `playerConnecting` deferral flow against the script runtimes.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use baston_protocol::PlayerInfo;
use serde_json::json;

use super::AppState;

const DEFAULT_PLAYER_NAME: &str = "Player";

/// Extract fields from either a url-encoded or JSON body, leniently.
fn extract_field(body: &str, field: &str) -> Option<String> {
    if body.trim_start().starts_with('{') {
        let value: serde_json::Value = serde_json::from_str(body).ok()?;
        return value.get(field).and_then(|v| v.as_str()).map(String::from);
    }
    form_urlencoded::parse(body.as_bytes())
        .find(|(k, _)| k == field)
        .map(|(_, v)| v.into_owned())
}

pub async fn client_connect(State(state): State<Arc<AppState>>, body: String) -> Response {
    let name = extract_field(&body, "name")
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| DEFAULT_PLAYER_NAME.to_owned());
    // Phase A: no real transport identity yet — the UDP/RAGE layer (Phase B)
    // will carry the peer address.
    let ip = "127.0.0.1".to_owned();

    // FiveM sends several POST /client calls (getEndpoints, initConnect, ...).
    // Only initConnect runs the connection flow; other methods get a bypass ack.
    let method = extract_field(&body, "method").unwrap_or_else(|| "initConnect".to_owned());
    if method != "initConnect" {
        return Json(json!({ "token": "dev-bypass-token", "defer": false })).into_response();
    }

    let source = state.players.allocate_source();
    tracing::info!(target: "gateway", source, %name, %ip, "player connecting");

    let deferrals = state.script_host.deferrals();
    let rx = deferrals.register(source);

    if let Err(e) = state
        .script_host
        .fire_player_connecting(source, &name)
        .await
    {
        tracing::error!(target: "gateway", error = %e, "failed to fire playerConnecting");
        deferrals.remove(source);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "internal scripting error" })),
        )
            .into_response();
    }

    let timeout = Duration::from_secs(state.config.connection.deferral_timeout_secs);
    let outcome = tokio::time::timeout(timeout, rx).await;
    deferrals.remove(source);

    match outcome {
        Ok(Ok(Ok(()))) => {
            state.players.insert(PlayerInfo {
                source,
                name: name.clone(),
                identifiers: vec![format!("ip:{ip}")],
            });
            tracing::info!(target: "gateway", source, %name, "connection accepted");
            Json(json!({
                "token": "dev-bypass-token",
                "defer": false,
                "sv_licenseKeyToken": "phase-a",
                "source": source,
                "status": "ok",
            }))
            .into_response()
        }
        Ok(Ok(Err(reason))) => {
            tracing::info!(target: "gateway", source, %name, %reason, "connection rejected");
            (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": reason, "status": "rejected" })),
            )
                .into_response()
        }
        // Sender dropped without resolving — treat like a timeout.
        Ok(Err(_)) | Err(_) => {
            tracing::warn!(target: "gateway", source, %name, "connection timed out in deferrals");
            (
                StatusCode::REQUEST_TIMEOUT,
                Json(json!({ "error": "Connection timed out", "status": "rejected" })),
            )
                .into_response()
        }
    }
}

/// Phase A stand-in for a real disconnect signal: drops the player and fires
/// `playerDropped` in the runtimes.
pub async fn admin_drop_player(
    State(state): State<Arc<AppState>>,
    AxumPath(source): AxumPath<u32>,
) -> Response {
    let Some(player) = state.players.remove(source) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    tracing::info!(target: "gateway", source, name = %player.name, "player dropped");
    if let Err(e) = state
        .script_host
        .trigger_event("playerDropped", &[json!("Disconnected.")])
        .await
    {
        tracing::error!(target: "gateway", error = %e, "failed to fire playerDropped");
    }
    StatusCode::NO_CONTENT.into_response()
}
