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

/// Replicated variables BASTON owns, which an operator must not be able to
/// overwrite from `[server.vars]` or a script.
///
/// Three of these describe the server's *capacity and identity*, and the
/// client acts on them: `sv_licenseKeyToken` decides whether it looks up an
/// entitlement policy at all, and `sv_maxClients` with the `onesync` pair
/// decide which entitlement that lookup has to find. Letting a config file
/// win here would reopen the hole ADR-004 closes from the other side — a
/// server could publish a token it does not hold, or hide the one it does.
///
/// `sv_enforceGameBuild` is here for a duller reason: it comes from
/// `[server] enforce_game_build`, and two spellings of one setting is how an
/// operator ends up debugging the one that lost.
const RESERVED_VARS: &[&str] = &[
    "sv_licenseKeyToken",
    "sv_maxClients",
    "sv_enforceGameBuild",
    "onesync",
    "onesync_enabled",
];

/// Well-known variables that also drive a top-level field.
///
/// `/info.json` carries both `vars.sv_hostname`-style entries *and* a handful
/// of promoted fields the browser reads directly. FXServer keeps them in sync
/// because both come from the same convar; so does this.
fn promoted(vars: &serde_json::Map<String, serde_json::Value>, name: &str) -> Option<String> {
    vars.get(name)
        .and_then(serde_json::Value::as_str)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

/// Build the `/info.json` document.
///
/// # Precedence
///
/// Operator `[server.vars]` and script `SetConvarServerInfo` share one store,
/// last write wins — which is FXServer's behaviour for `sets` and the native.
/// BASTON's own variables are written **last** and are not overridable; see
/// [`RESERVED_VARS`].
///
/// # `sv_licenseKeyToken`
///
/// This is what arms the client's entitlement check. The client reads it here,
/// fetches the policy it names, and refuses to connect when the slot count
/// exceeds what that policy grants. Publishing it is therefore not optional
/// for a server that also advertises itself: the heartbeat refuses to send a
/// snapshot that omits it.
pub fn payload(state: &AppState, resources: Vec<String>) -> serde_json::Value {
    let onesync_enabled = state.config.state_sync.onesync.is_enabled();
    let mut vars = serde_json::Map::new();

    // 1. Whatever the operator and the running scripts have set. BASTON does
    //    not know or validate these names: `sv_projectName`, `sv_projectDesc`,
    //    `tags`, `locale`, `banner_detail` and the rest are read by the server
    //    browser, not by the server, exactly as in FXServer.
    for entry in state.server_vars().iter() {
        if RESERVED_VARS.contains(&entry.key().as_str()) {
            continue;
        }
        vars.insert(entry.key().clone(), json!(entry.value()));
    }

    // 2. BASTON's own, written last so nothing above can spoof them.
    //
    // NetLibrary.cpp reads `vars.sv_enforceGameBuild` pre-connect and
    // build-switches the client; without it every client keeps its local
    // build and mixed-build non-OneSync sessions can't join each other.
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

    // Promoted fields. `sv_hostname` / `sv_gametype` / `sv_mapname` are the
    // convars FXServer promotes, so a resource calling SetGameType now
    // actually changes what the browser shows.
    let name = promoted(&vars, "sv_hostname").unwrap_or_else(|| state.config.server.name.clone());
    let game_type = promoted(&vars, "sv_gametype").unwrap_or_else(|| "Roleplay".to_owned());
    let map_name = promoted(&vars, "sv_mapname").unwrap_or_else(|| "Los Santos".to_owned());

    let mut info = json!({
        "name": name,
        "players": state.players.count(),
        "maxPlayers": state.config.server.max_players,
        "gameType": game_type,
        "mapName": map_name,
        "enhancedHostSupport": !onesync_enabled,
        "onesync": { "enabled": onesync_enabled },
        "vars": vars,
        "version": 1,
        "resources": resources,
        "server": format!("BASTON/{} (Rust)", env!("CARGO_PKG_VERSION")),
    });

    if let Some(icon) = &state.icon {
        info["icon"] = json!(icon);
    }
    info
}

/// The `dynamic.json` half of a server-list heartbeat (`GameServer.cpp` builds
/// info, dynamic and players together).
///
/// It rides in the heartbeat's `fallbackData` *and* is served over HTTP,
/// because the server list does both: it takes the fallback and then queries
/// the server back. A live run showed the ingress reporting
/// `server request failed for endpoint .../dynamic.json` — it really does ask.
pub fn dynamic_payload(state: &AppState) -> serde_json::Value {
    let vars = state.server_vars();
    let read = |name: &str, fallback: &str| {
        vars.get(name)
            .map(|v| v.value().clone())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| fallback.to_owned())
    };
    json!({
        "clients": state.players.count(),
        "gametype": read("sv_gametype", "Roleplay"),
        "hostname": read("sv_hostname", &state.config.server.name),
        "mapname": read("sv_mapname", "Los Santos"),
        "sv_maxclients": state.config.server.max_players.to_string(),
    })
}

pub async fn info_json(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let resources = state.resource_manager.started_names().await;
    Json(payload(&state, resources))
}

/// `GET /dynamic.json` — the changing half: player count and the current
/// hostname, gametype and map.
pub async fn dynamic_json(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(dynamic_payload(&state))
}

/// `GET /players.json` — one entry per connected player, as `GameServer.cpp`
/// builds it. The endpoint FXServer serves and every server browser reads.
pub async fn players_json(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(players_payload(&state))
}

/// The `players.json` half of the heartbeat.
pub fn players_payload(state: &AppState) -> serde_json::Value {
    let players: Vec<serde_json::Value> = state
        .players
        .sources()
        .into_iter()
        .filter_map(|source| {
            let player = state.players.get(source)?;
            Some(json!({
                "id": source,
                "name": player.name,
                "identifiers": player.identifiers,
                // Ping is not tracked per player here yet; 0 reads as unknown
                // rather than inventing a latency.
                "ping": 0,
                "endpoint": state.players.endpoint(source).unwrap_or_default(),
            }))
        })
        .collect();
    serde_json::Value::Array(players)
}
