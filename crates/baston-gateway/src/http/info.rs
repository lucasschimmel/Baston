//! `GET /info.json` — server metadata the FiveM client fetches pre-connect.

use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde_json::json;

use super::AppState;

pub async fn info_json(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let resources = state.resource_manager.started_names().await;
    Json(json!({
        "name": state.config.server.name,
        "players": state.players.count(),
        "maxPlayers": state.config.server.max_players,
        "gameType": "Roleplay",
        "mapName": "Los Santos",
        "enhancedHostSupport": true,
        "onesync": { "enabled": true },
        "vars": {},
        "version": 1,
        "resources": resources,
        "server": format!("BASTON/{} (Rust)", env!("CARGO_PKG_VERSION")),
    }))
}
