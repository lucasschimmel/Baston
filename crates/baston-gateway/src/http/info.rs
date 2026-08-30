//! `GET /info.json` — server metadata the FiveM client fetches pre-connect.
//!
//! This document is also what the CFX server list advertises: the ingress
//! heartbeat carries it verbatim as `fallbackData.info` (`GameServer.cpp`).
//! Both callers go through [`payload`] so there is exactly one version of what
//! this server claims to be — see the note on `sv_licenseKeyToken` below.

use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde_json::json;

use super::AppState;

/// Build the `/info.json` document.
///
/// **`sv_licenseKeyToken` is what arms the client's entitlement check.** The
/// client reads it here, fetches the policy it names, and refuses to connect
/// when the slot count exceeds what that policy grants. Publishing it is
/// therefore not optional for a server that also advertises itself: the
/// heartbeat refuses to send a snapshot that omits it.
pub fn payload(state: &AppState, resources: Vec<String>) -> serde_json::Value {
    let onesync_enabled = state.config.state_sync.onesync.is_enabled();
    // NetLibrary.cpp reads `vars.sv_enforceGameBuild` pre-connect and
    // build-switches the client; without it every client keeps its local
    // build and mixed-build non-OneSync sessions can't join each other.
    let mut vars = serde_json::Map::new();
    if !state.config.server.enforce_game_build.is_empty() {
        vars.insert(
            "sv_enforceGameBuild".to_owned(),
            json!(state.config.server.enforce_game_build),
        );
    }
    if let Some(token) = state.license_token() {
        vars.insert("sv_licenseKeyToken".to_owned(), json!(token));
    }
    vars.insert(
        "sv_maxClients".to_owned(),
        json!(state.config.server.max_players.to_string()),
    );
    vars.insert(
        "onesync_enabled".to_owned(),
        json!(onesync_enabled.to_string()),
    );
    vars.insert(
        "onesync".to_owned(),
        json!(state.config.state_sync.onesync.convar_value()),
    );
    json!({
        "name": state.config.server.name,
        "players": state.players.count(),
        "maxPlayers": state.config.server.max_players,
        "gameType": "Roleplay",
        "mapName": "Los Santos",
        "enhancedHostSupport": !onesync_enabled,
        "onesync": { "enabled": onesync_enabled },
        "vars": vars,
        "version": 1,
        "resources": resources,
        "server": format!("BASTON/{} (Rust)", env!("CARGO_PKG_VERSION")),
    })
}

/// The `dynamic.json` half of a server-list heartbeat (`GameServer.cpp` builds
/// info, dynamic and players together). Nothing serves this over HTTP today;
/// it exists because the ingress contract asks for it.
pub fn dynamic_payload(state: &AppState) -> serde_json::Value {
    json!({
        "clients": state.players.count(),
        "gametype": "Roleplay",
        "hostname": state.config.server.name,
        "mapname": "Los Santos",
        "sv_maxclients": state.config.server.max_players.to_string(),
    })
}

pub async fn info_json(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let resources = state.resource_manager.started_names().await;
    Json(payload(&state, resources))
}
