//! axum HTTP gateway: `/info.json`, resource file serving, and the Phase A
//! `POST /client` connection endpoint.

mod client;
mod configuration;
mod files;
mod info;
mod packfile_cache;

pub use packfile_cache::PackfileCache;

use std::sync::Arc;

use axum::http::Method;
use axum::routing::{get, post};
use axum::Router;
use baston_config::BastonConfig;
use baston_scripting::ScriptHost;
use baston_zone::resource_loader::ResourceManager;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

use crate::auth::AuthService;
use crate::players::PlayerRegistry;

/// Shared state for all HTTP handlers.
pub struct AppState {
    pub config: BastonConfig,
    pub resource_manager: Arc<ResourceManager>,
    /// Shared with the script host so player natives see real data.
    pub players: Arc<PlayerRegistry>,
    pub script_host: ScriptHost,
    pub auth: AuthService,
    pub packfiles: PackfileCache,
    /// Phase D zone federation (None when `[meshing]` is disabled).
    pub mesh: Option<Arc<crate::mesh::GatewayMesh>>,
}

/// Build the gateway router.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/info.json", get(info::info_json))
        .route("/files/{resource}/{*path}", get(files::serve_resource_file))
        .route("/client", post(client::client_connect))
        .route(
            "/admin/player/{source}/drop",
            post(client::admin_drop_player),
        )
        // CORS for the FiveM CEF browser and cross-origin info.json reads.
        // Any origin, but only the methods we actually serve and no arbitrary
        // request headers — so a browser can't be tricked into a credentialed
        // cross-origin call to the authenticated drop route.
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([Method::GET, Method::POST]),
        )
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
