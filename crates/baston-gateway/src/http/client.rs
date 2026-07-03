//! `POST /client` — connection endpoint.
//!
//! The FiveM client posts url-encoded fields (`method=initConnect`, `name`,
//! `guid`, `cfxTicket2`, ...). Since B1 the ticket is verified offline against
//! the CFX public key (see `crate::auth`); `dev.auth_bypass = true` skips
//! validation and assigns `license:dev-{source}` identifiers.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use baston_protocol::PlayerInfo;
use serde_json::json;

use super::AppState;

const DEFAULT_PLAYER_NAME: &str = "Player";

/// Extract fields from either a url-encoded or JSON body, leniently.
pub(super) fn extract_field(body: &str, field: &str) -> Option<String> {
    if body.trim_start().starts_with('{') {
        let value: serde_json::Value = serde_json::from_str(body).ok()?;
        return value.get(field).and_then(|v| v.as_str()).map(String::from);
    }
    form_urlencoded::parse(body.as_bytes())
        .find(|(k, _)| k == field)
        .map(|(_, v)| v.into_owned())
}

/// FXServer sends connection failures as `{ "error": reason }` on HTTP 200;
/// mirror that shape (the client only looks at the JSON body).
fn client_error(reason: impl Into<String>) -> Response {
    Json(json!({ "error": reason.into() })).into_response()
}

/// Peer IP from proxy headers (as FXServer's EndPointIdentityProvider);
/// the UDP layer (B3) carries the authoritative peer address.
fn peer_ip(headers: &HeaderMap) -> String {
    headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .unwrap_or_else(|| "127.0.0.1".to_owned())
}

/// Resolve the connecting player's identifiers: dev bypass or real CFX auth.
async fn authenticate(
    state: &AppState,
    body: &str,
    source: u32,
    ip: &str,
) -> Result<Vec<String>, Response> {
    if state.config.dev.auth_bypass {
        return Ok(vec![format!("license:dev-{source}"), format!("ip:{ip}")]);
    }

    let Some(ticket) = extract_field(body, "cfxTicket2") else {
        return Err(client_error("No authentication ticket was specified."));
    };
    let guid = extract_field(body, "guid").unwrap_or_default();

    match state.auth.validate_ticket(&guid, &ticket, ip).await {
        Ok(player) => {
            tracing::info!(
                target: "baston",
                "player authenticated: {} ({})",
                player.license,
                player.steam.as_deref().unwrap_or("no steam"),
            );
            Ok(player.identifiers)
        }
        Err(e) => {
            tracing::warn!(target: "gateway", source, error = %e, "ticket validation failed");
            Err(client_error(e.to_string()))
        }
    }
}

pub async fn client_connect(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let name = extract_field(&body, "name")
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| DEFAULT_PLAYER_NAME.to_owned());
    let ip = peer_ip(&headers);

    // FiveM multiplexes several methods over POST /client
    // (ClientHttpHandler.cpp → ClientMethodRegistry).
    let method = extract_field(&body, "method").unwrap_or_else(|| "initConnect".to_owned());
    match method.as_str() {
        "initConnect" => {}
        "getConfiguration" => {
            return super::configuration::get_configuration(&state, &headers, &body).await
        }
        // No alternate endpoints configured (sv_endpoints equivalent).
        "getEndpoints" => return Json(json!([])).into_response(),
        other => {
            tracing::debug!(target: "gateway", method = %other, "unhandled /client method");
            return Json(json!({ "error": "invalid method" })).into_response();
        }
    }

    // Client/server version gate (InitConnectMethod.cpp: protocol < 12).
    let protocol: u32 = extract_field(&body, "protocol")
        .and_then(|p| p.parse().ok())
        .unwrap_or(0);
    if protocol < baston_protocol::connection::MIN_CLIENT_PROTOCOL {
        return client_error("Client/server version mismatch. Restart your game client to update.");
    }
    let game_name = extract_field(&body, "gameName").unwrap_or_else(|| "gta5".to_owned());
    if game_name != "gta5" {
        return client_error(format!("Client/Server game mismatch: {game_name}/gta5"));
    }

    let source = state.players.allocate_source();
    tracing::info!(target: "gateway", source, %name, %ip, "player connecting");

    let identifiers = match authenticate(&state, &body, source, &ip).await {
        Ok(ids) => ids,
        Err(response) => return response,
    };

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
            let token = uuid::Uuid::new_v4().to_string();
            state.players.insert(PlayerInfo {
                source,
                name: name.clone(),
                identifiers,
            });
            state.players.bind_token(token.clone(), source);
            tracing::info!(target: "gateway", source, %name, "connection accepted");
            tracing::info!(
                target: "baston",
                "connection ticket issued: session={token} udp={}",
                state.config.server.port,
            );

            let mut response =
                serde_json::to_value(baston_protocol::connection::InitConnectResponse::new(
                    token,
                    &game_name,
                    state.config.server.max_players,
                ))
                .unwrap_or_else(|_| json!({}));
            // Non-FXServer extras (harmless to the client, used by our tests).
            response["status"] = json!("ok");
            Json(response).into_response()
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
