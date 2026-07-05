//! `POST /client method=getConfiguration` — resource list with packfile
//! hashes (GetConfigurationMethod.cpp).

use std::collections::BTreeMap;

use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;
use baston_protocol::connection::{
    GetConfigurationResponse, ResourceConfiguration, DEFAULT_RESOURCE_SET,
};
use serde_json::json;

use super::AppState;

/// Handle `getConfiguration`. The client authenticates with the connection
/// token from `initConnect` in the `X-CitizenFX-Token` header.
pub async fn get_configuration(state: &AppState, headers: &HeaderMap, body: &str) -> Response {
    let token = headers
        .get("x-citizenfx-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();

    if state.players.source_for_token(token).is_none() && !state.config.dev.auth_bypass {
        return Json(json!({ "error": "Not a valid client." })).into_response();
    }

    // Optional `resources` filter: semicolon-separated names.
    let filter: Vec<String> = super::client::extract_field(body, "resources")
        .map(|f| f.split(';').map(str::to_owned).collect())
        .unwrap_or_default();

    let mut resources = Vec::new();
    for name in state.resource_manager.started_names().await {
        if !filter.is_empty() && !filter.contains(&name) {
            continue;
        }
        let stream_files: BTreeMap<_, _> = state
            .streams
            .get(&state.resource_manager, &name)
            .await
            .map(|set| {
                set.assets
                    .iter()
                    .map(|(basename, asset)| (basename.clone(), asset.entry.clone()))
                    .collect()
            })
            .unwrap_or_default();
        // Resources with neither client files nor stream assets are
        // server-only: not sent. Stream-only resources still need an RPF
        // (manifest-only) so the client can mount them.
        let Some(pack) = state
            .packfiles
            .get(&state.resource_manager, &name, !stream_files.is_empty())
            .await
        else {
            continue;
        };
        resources.push(ResourceConfiguration {
            name: name.clone(),
            files: BTreeMap::from([(DEFAULT_RESOURCE_SET.to_owned(), pack.sha1_hex.clone())]),
            stream_files,
        });
    }

    tracing::info!(
        target: "baston",
        count = resources.len(),
        "resource configuration served"
    );

    // Scheme note (client: citizen-legacy-net-resources/ResourceNetBindings.cpp):
    // any `%s` template — "http://%s/" OR "https://%s/" — is rewritten by the
    // client to `https://<peer>/`, and BASTON has no TLS listener. A LITERAL
    // URL without `%s` (the `fileserver_add` CDN path) is used verbatim, so we
    // build one from the request's Host header to keep downloads on plain HTTP.
    let file_server = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .map(|host| format!("http://{host}/files"))
        .unwrap_or_else(|| "https://%s/files".to_owned());

    Json(GetConfigurationResponse {
        file_server,
        resources,
    })
    .into_response()
}
