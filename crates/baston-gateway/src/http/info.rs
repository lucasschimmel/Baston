//! `GET /info.json` — server metadata the FiveM client fetches pre-connect.

use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde_json::json;

use super::AppState;

pub async fn info_json(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let resources = state.resource_manager.started_names().await;
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
    Json(json!({
        "name": state.config.server.name,
        "players": state.players.count(),
        "maxPlayers": state.config.server.max_players,
        "gameType": "Roleplay",
        "mapName": "Los Santos",
        "enhancedHostSupport": true,
        "onesync": { "enabled": false },
        "vars": vars,
        "version": 1,
        "resources": resources,
        "server": format!("BASTON/{} (Rust)", env!("CARGO_PKG_VERSION")),
    }))
}
