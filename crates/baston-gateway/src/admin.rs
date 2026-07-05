//! Admin HTTP API (jalon D2) — operational introspection + zone drain.
//!
//! Served on its own port (default 8080) so it never mixes with the FiveM
//! game port. Every route requires `Authorization: Bearer <admin_token>`.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;
use subtle::ConstantTimeEq;

use crate::mesh::GatewayMesh;
use crate::players::PlayerRegistry;

#[derive(Clone)]
pub struct AdminState {
    pub mesh: Arc<GatewayMesh>,
    pub players: Arc<PlayerRegistry>,
    pub token: String,
}

pub fn admin_router(state: AdminState) -> Router {
    Router::new()
        .route("/admin/zones", get(list_zones))
        .route("/admin/zones/{id}", get(zone_detail))
        .route("/admin/players", get(list_players))
        .route("/admin/zones/{id}/drain", post(drain_zone))
        .with_state(state)
}

/// Spawn the admin listener. Refuses to start without a token — an open
/// admin API can reroute players.
pub fn spawn_admin_api(state: AdminState, port: u16) -> Option<tokio::task::JoinHandle<()>> {
    if state.token.is_empty() {
        tracing::warn!(target: "gateway", "admin_token empty — admin API disabled");
        return None;
    }
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    Some(tokio::spawn(async move {
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(target: "gateway", %addr, error = %e, "admin API bind failed");
                return;
            }
        };
        tracing::info!(target: "gateway", %addr, "admin API listening");
        if let Err(e) = axum::serve(listener, admin_router(state)).await {
            tracing::error!(target: "gateway", error = %e, "admin API exited");
        }
    }))
}

/// Constant-time bearer-token check shared by the admin API and the game-port
/// drop route. An empty configured token means "disabled" and always denies
/// (fail-closed) — never treat a missing token as an open door.
pub(crate) fn bearer_matches(token: &str, headers: &HeaderMap) -> bool {
    if token.is_empty() {
        return false;
    }
    let provided = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match provided {
        // Constant-time compare so the token can't be recovered byte-by-byte
        // via response timing. Length is not secret, so an early length
        // mismatch returning false is fine.
        Some(p) => bool::from(p.as_bytes().ct_eq(token.as_bytes())),
        None => false,
    }
}

// Boxed error: an axum `Response` is large, and returning it unboxed in a
// `Result` trips clippy::result_large_err.
fn check_auth(state: &AdminState, headers: &HeaderMap) -> Result<(), Box<Response>> {
    if bearer_matches(&state.token, headers) {
        Ok(())
    } else {
        Err(Box::new(
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "invalid admin token"})),
            )
                .into_response(),
        ))
    }
}

async fn list_zones(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    if let Err(resp) = check_auth(&state, &headers) {
        return *resp;
    }
    let mut stats = state.mesh.registry.stats().await;
    stats.sort_by(|a, b| a.zone_id.cmp(&b.zone_id));
    let zones: Vec<_> = stats
        .iter()
        .map(|z| {
            json!({
                "id": z.zone_id,
                "bounds": z.bounds,
                "grpc_addr": z.grpc_addr,
                "players": state.mesh.router.count_in_zone(&z.zone_id),
                "players_reported": z.player_count,
                "entities": z.entity_count,
                "max_players": z.max_players,
                "heartbeat_age_ms": z.heartbeat_age_ms,
                "status": z.status,
            })
        })
        .collect();
    Json(json!(zones)).into_response()
}

async fn zone_detail(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = check_auth(&state, &headers) {
        return *resp;
    }
    let stats = state.mesh.registry.stats().await;
    let Some(z) = stats.iter().find(|z| z.zone_id == id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "unknown zone"})),
        )
            .into_response();
    };
    let players: Vec<u32> = state.mesh.router.players_in_zone(&id);
    Json(json!({
        "id": z.zone_id,
        "bounds": z.bounds,
        "grpc_addr": z.grpc_addr,
        "players": players,
        "players_reported": z.player_count,
        "entities": z.entity_count,
        "max_players": z.max_players,
        "heartbeat_age_ms": z.heartbeat_age_ms,
        "status": z.status,
    }))
    .into_response()
}

async fn list_players(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    if let Err(resp) = check_auth(&state, &headers) {
        return *resp;
    }
    let mut routed = state.mesh.router.all();
    routed.sort_by_key(|(source, _)| *source);
    let players: Vec<_> = routed
        .into_iter()
        .map(|(source, zone)| {
            let name = state.players.get(source).map(|p| p.name.clone());
            json!({ "source": source, "zone": zone, "name": name })
        })
        .collect();
    Json(json!(players)).into_response()
}

/// Progressive drain: reroute every player of the zone to the least-loaded
/// other zone. Jalon D2 moves the routing; the per-player state migration
/// rides the D4 handoff path once it exists.
async fn drain_zone(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = check_auth(&state, &headers) {
        return *resp;
    }
    if !state.mesh.registry.contains(&id).await {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "unknown zone"})),
        )
            .into_response();
    }
    let drained = state.mesh.drain_zone(&id).await;
    (
        StatusCode::ACCEPTED,
        Json(json!({ "zone": id, "players_rerouted": drained })),
    )
        .into_response()
}
