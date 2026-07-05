//! `/api/v1` — monitoring + control HTTP API with per-key permissions.
//!
//! Served on the admin port next to the legacy `/admin/*` routes. Keys and
//! their permissions come from `[[api.keys]]`; the legacy
//! `meshing.admin_token` acts as an implicit full-permission key. Monitoring
//! routes need `monitor.read`; control routes need their specific permission
//! and every attempt (including denied ones) is written to the audit log.

pub mod audit;
pub mod auth;

use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use baston_config::ApiPermission;
use baston_zone::resource_loader::ResourceState;
use baston_zone::ResourceManager;
use serde_json::json;

use crate::mesh::GatewayMesh;
use crate::players::PlayerRegistry;
use crate::udp::UdpHandle;
pub use audit::AuditLog;
pub use auth::{AuthError, KeyRing};

#[derive(Clone)]
pub struct ApiState {
    pub keyring: Arc<KeyRing>,
    pub audit: AuditLog,
    pub players: Arc<PlayerRegistry>,
    pub resource_manager: Arc<ResourceManager>,
    /// Zone federation; `None` in single-process mode.
    pub mesh: Option<Arc<GatewayMesh>>,
    /// Game transport; `None` until the UDP server is up.
    pub udp: Option<UdpHandle>,
    pub server_name: String,
    pub max_players: u32,
    pub started_at: Instant,
}

pub fn api_router(state: ApiState) -> Router {
    Router::new()
        .route("/api/v1/status", get(status))
        .route("/api/v1/players", get(list_players))
        .route("/api/v1/zones", get(list_zones))
        .route("/api/v1/zones/{id}", get(zone_detail))
        .route("/api/v1/resources", get(list_resources))
        .route("/api/v1/players/{source}/kick", post(kick_player))
        .route(
            "/api/v1/resources/{name}/{action}",
            post(control_resource),
        )
        .route("/api/v1/zones/{id}/drain", post(drain_zone))
        .with_state(state)
}

/// Spawn the combined admin listener: legacy `/admin/*` (when meshing is on)
/// merged with `/api/v1/*`. Refuses to start with no keys at all — an open
/// admin surface can kick players and stop resources.
pub fn spawn_api(
    state: ApiState,
    legacy: Option<crate::admin::AdminState>,
    port: u16,
) -> Option<tokio::task::JoinHandle<()>> {
    let legacy_enabled = legacy.as_ref().is_some_and(|l| !l.token.is_empty());
    if state.keyring.is_empty() && !legacy_enabled {
        tracing::warn!(target: "api", "no [[api.keys]] and no admin_token — admin/API listener disabled");
        return None;
    }
    let mut router = api_router(state);
    if let Some(legacy) = legacy {
        router = router.merge(crate::admin::admin_router(legacy));
    }
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    Some(tokio::spawn(async move {
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(target: "api", %addr, error = %e, "API listener bind failed");
                return;
            }
        };
        tracing::info!(target: "api", %addr, "admin/API listening");
        if let Err(e) = axum::serve(listener, router).await {
            tracing::error!(target: "api", error = %e, "API listener exited");
        }
    }))
}

/// Authorize or answer 401/403. Denied *control* attempts are audited by the
/// callers that pass `audit_action` — monitoring guards pass `None`.
fn guard(
    state: &ApiState,
    headers: &HeaderMap,
    permission: ApiPermission,
    audit_action: Option<(&str, &str)>,
) -> Result<String, Box<Response>> {
    match state.keyring.authorize(headers, permission) {
        Ok(key) => Ok(key),
        Err(AuthError::Unknown) => Err(Box::new(
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "invalid API token"})),
            )
                .into_response(),
        )),
        Err(AuthError::Forbidden { key_name }) => {
            if let Some((action, target)) = audit_action {
                state.audit.record(&key_name, action, target, "denied");
            }
            Err(Box::new(
                (
                    StatusCode::FORBIDDEN,
                    Json(json!({"error": "missing permission"})),
                )
                    .into_response(),
            ))
        }
    }
}

fn resource_state_str(state: &ResourceState) -> &'static str {
    match state {
        ResourceState::Unloaded => "unloaded",
        ResourceState::Loading => "loading",
        ResourceState::Started => "started",
        ResourceState::Stopped => "stopped",
        ResourceState::Error => "error",
    }
}

async fn status(State(state): State<ApiState>, headers: HeaderMap) -> Response {
    if let Err(resp) = guard(&state, &headers, ApiPermission::MonitorRead, None) {
        return *resp;
    }
    let zones = match &state.mesh {
        Some(mesh) => mesh.registry.stats().await.len(),
        None => 0,
    };
    Json(json!({
        "name": state.server_name,
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_secs": state.started_at.elapsed().as_secs(),
        "players": state.players.count(),
        "max_players": state.max_players,
        "zones": zones,
    }))
    .into_response()
}

async fn list_players(State(state): State<ApiState>, headers: HeaderMap) -> Response {
    if let Err(resp) = guard(&state, &headers, ApiPermission::MonitorRead, None) {
        return *resp;
    }
    let mut sources = state.players.sources();
    sources.sort_unstable();
    let players: Vec<_> = sources
        .into_iter()
        .filter_map(|source| {
            let info = state.players.get(source)?;
            let zone = state
                .mesh
                .as_ref()
                .and_then(|m| m.router.zone_of(source));
            Some(json!({
                "source": source,
                "name": info.name,
                "identifiers": info.identifiers,
                "zone": zone,
            }))
        })
        .collect();
    Json(json!(players)).into_response()
}

async fn list_zones(State(state): State<ApiState>, headers: HeaderMap) -> Response {
    if let Err(resp) = guard(&state, &headers, ApiPermission::MonitorRead, None) {
        return *resp;
    }
    let Some(mesh) = &state.mesh else {
        return Json(json!([])).into_response();
    };
    let mut stats = mesh.registry.stats().await;
    stats.sort_by(|a, b| a.zone_id.cmp(&b.zone_id));
    let zones: Vec<_> = stats
        .iter()
        .map(|z| {
            json!({
                "id": z.zone_id,
                "bounds": z.bounds,
                "players": mesh.router.count_in_zone(&z.zone_id),
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
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = guard(&state, &headers, ApiPermission::MonitorRead, None) {
        return *resp;
    }
    let Some(mesh) = &state.mesh else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "meshing disabled"})),
        )
            .into_response();
    };
    let stats = mesh.registry.stats().await;
    let Some(z) = stats.iter().find(|z| z.zone_id == id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "unknown zone"})),
        )
            .into_response();
    };
    Json(json!({
        "id": z.zone_id,
        "bounds": z.bounds,
        "grpc_addr": z.grpc_addr,
        "players": mesh.router.players_in_zone(&id),
        "entities": z.entity_count,
        "max_players": z.max_players,
        "heartbeat_age_ms": z.heartbeat_age_ms,
        "status": z.status,
    }))
    .into_response()
}

async fn list_resources(State(state): State<ApiState>, headers: HeaderMap) -> Response {
    if let Err(resp) = guard(&state, &headers, ApiPermission::MonitorRead, None) {
        return *resp;
    }
    let mut statuses = state.resource_manager.status().await;
    statuses.sort_by(|a, b| a.0.cmp(&b.0));
    let mut resources: Vec<_> = statuses
        .iter()
        .map(|(name, rs)| {
            json!({ "name": name, "state": resource_state_str(rs), "zone": "gateway" })
        })
        .collect();
    // Meshing: each zone runs its own ResourceManager — relay the listing.
    if let Some(mesh) = &state.mesh {
        for z in mesh.registry.stats().await {
            let Some(mut client) = mesh.registry.zone_client(&z.zone_id).await else {
                continue;
            };
            match client
                .list_resources(baston_protocol::mesh::ListResourcesRequest {})
                .await
            {
                Ok(resp) => {
                    for r in resp.into_inner().resources {
                        resources.push(json!({
                            "name": r.name, "state": r.state, "zone": z.zone_id,
                        }));
                    }
                }
                Err(status) => {
                    resources.push(json!({
                        "zone": z.zone_id, "error": status.message(),
                    }));
                }
            }
        }
    }
    Json(json!(resources)).into_response()
}

async fn kick_player(
    State(state): State<ApiState>,
    Path(source): Path<u32>,
    headers: HeaderMap,
    body: Option<Json<serde_json::Value>>,
) -> Response {
    let target = format!("source:{source}");
    let key = match guard(
        &state,
        &headers,
        ApiPermission::PlayerKick,
        Some(("player.kick", &target)),
    ) {
        Ok(k) => k,
        Err(resp) => return *resp,
    };
    if state.players.get(source).is_none() {
        state.audit.record(&key, "player.kick", &target, "not_found");
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "unknown player"})),
        )
            .into_response();
    }
    let Some(udp) = &state.udp else {
        state.audit.record(&key, "player.kick", &target, "no_udp");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "game transport not running"})),
        )
            .into_response();
    };
    let reason = body
        .as_ref()
        .and_then(|Json(v)| v.get("reason").and_then(|r| r.as_str()))
        .unwrap_or("Kicked by admin");
    // ENet disconnect fires the normal drop path (playerDropped + directory
    // purge); the reason is recorded server-side.
    udp.drop_source(source);
    state
        .audit
        .record(&key, "player.kick", &format!("{target} reason:{reason}"), "ok");
    (
        StatusCode::ACCEPTED,
        Json(json!({ "source": source, "kicked": true })),
    )
        .into_response()
}

async fn control_resource(
    State(state): State<ApiState>,
    Path((name, action)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let audit_action = format!("resource.{action}");
    let target = format!("resource:{name}");
    let key = match guard(
        &state,
        &headers,
        ApiPermission::ResourceControl,
        Some((&audit_action, &target)),
    ) {
        Ok(k) => k,
        Err(resp) => return *resp,
    };
    let result = match action.as_str() {
        "start" => state.resource_manager.start(&name).await,
        "stop" => state.resource_manager.stop(&name).await,
        "restart" => state.resource_manager.restart(&name).await,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "action must be start, stop or restart"})),
            )
                .into_response();
        }
    };
    // Meshing: relay the action to every zone's ResourceManager.
    let mut zone_results = serde_json::Map::new();
    if let Some(mesh) = &state.mesh {
        for z in mesh.registry.stats().await {
            let Some(mut client) = mesh.registry.zone_client(&z.zone_id).await else {
                continue;
            };
            let outcome = match client
                .control_resource(baston_protocol::mesh::ControlResourceRequest {
                    name: name.clone(),
                    action: action.clone(),
                })
                .await
            {
                Ok(resp) => {
                    let r = resp.into_inner();
                    if r.ok {
                        json!("ok")
                    } else {
                        json!({ "error": r.message })
                    }
                }
                Err(status) => json!({ "error": status.message() }),
            };
            zone_results.insert(z.zone_id, outcome);
        }
    }

    match result {
        Ok(()) => {
            state.audit.record(&key, &audit_action, &target, "ok");
            Json(json!({
                "resource": name, "action": action, "ok": true,
                "zones": zone_results,
            }))
            .into_response()
        }
        Err(e) => {
            let msg = e.to_string();
            state.audit.record(&key, &audit_action, &target, &msg);
            (
                StatusCode::CONFLICT,
                Json(json!({
                    "resource": name, "action": action, "error": msg,
                    "zones": zone_results,
                })),
            )
                .into_response()
        }
    }
}

async fn drain_zone(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let target = format!("zone:{id}");
    let key = match guard(
        &state,
        &headers,
        ApiPermission::ZoneDrain,
        Some(("zone.drain", &target)),
    ) {
        Ok(k) => k,
        Err(resp) => return *resp,
    };
    let Some(mesh) = &state.mesh else {
        state.audit.record(&key, "zone.drain", &target, "meshing_disabled");
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "meshing disabled"})),
        )
            .into_response();
    };
    if !mesh.registry.contains(&id).await {
        state.audit.record(&key, "zone.drain", &target, "not_found");
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "unknown zone"})),
        )
            .into_response();
    }
    let drained = mesh.drain_zone(&id).await;
    state.audit.record(&key, "zone.drain", &target, "ok");
    (
        StatusCode::ACCEPTED,
        Json(json!({ "zone": id, "players_rerouted": drained })),
    )
        .into_response()
}
