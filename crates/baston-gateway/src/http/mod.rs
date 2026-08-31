//! axum HTTP gateway: `/info.json`, resource file serving, and the Phase A
//! `POST /client` connection endpoint.

mod builtin;
mod client;
mod configuration;
mod files;
mod icon;
// `pub` because the CFX server-list heartbeat advertises the same document
// `/info.json` serves — one source, so the two cannot diverge.
pub mod info;
mod packfile_cache;
mod resource_endpoint;
mod stream_cache;

pub use builtin::{BuiltinResources, DISPLAYINFO};
pub use icon::{load as load_icon, IconError};
pub use packfile_cache::PackfileCache;
pub use stream_cache::StreamCache;

use std::sync::Arc;
use std::time::Duration;

use axum::http::Method;
use axum::routing::{any, get, post};
use axum::Router;
use baston_config::BastonConfig;
use baston_scripting::ScriptHost;
use baston_zone::resource_loader::ResourceManager;
use tower_http::cors::{Any, CorsLayer};
use tower_http::timeout::ResponseBodyTimeoutLayer;
use tower_http::trace::TraceLayer;

use crate::auth::AuthService;
use crate::players::PlayerRegistry;

/// Runtime limits for resource delivery. Keeping the semaphore in the shared
/// HTTP state makes the cap global across resources and clients.
#[derive(Clone)]
pub struct DownloadPolicy {
    pub timeout: Duration,
    pub chunk_size: usize,
    pub semaphore: Arc<tokio::sync::Semaphore>,
}

impl DownloadPolicy {
    pub fn new(config: &baston_config::ResourcesConfig) -> Self {
        Self {
            timeout: Duration::from_secs(config.file_download_timeout_secs),
            chunk_size: config.file_download_chunk_bytes,
            semaphore: Arc::new(tokio::sync::Semaphore::new(
                config.file_download_concurrency,
            )),
        }
    }
}

/// Shared state for all HTTP handlers.
pub struct AppState {
    pub config: BastonConfig,
    /// The authenticated CFX identity, when `[license] mode = "cfx"`.
    ///
    /// Owning one means the slot cap it carries has already been applied to
    /// `config.server.max_players`; `baston_cfx::authenticate` is the only way
    /// to build it. Read it through [`AppState::license_token`] rather than
    /// reaching for the field, so every publication site is one grep away.
    pub cfx: Option<Arc<baston_cfx::CfxIdentity>>,
    pub resource_manager: Arc<ResourceManager>,
    /// Shared with the script host so player natives see real data.
    pub players: Arc<PlayerRegistry>,
    pub script_host: ScriptHost,
    pub auth: AuthService,
    pub packfiles: PackfileCache,
    /// Streaming assets scanned from each resource's `stream/` folder.
    pub streams: StreamCache,
    /// Bounded, timed resource delivery shared by every `/files` request.
    pub downloads: DownloadPolicy,
    /// Phase D zone federation (None when `[meshing]` is disabled).
    pub mesh: Option<Arc<crate::mesh::GatewayMesh>>,
    /// Resources served from inside the binary rather than from disk.
    pub builtins: BuiltinResources,
    /// The base64 server icon, read once at boot from `[server] icon`.
    ///
    /// Held decoded-and-re-encoded rather than re-read per request: it is a
    /// few kilobytes, `/info.json` is fetched by every connecting client, and
    /// a disk read there would be on the connect path.
    pub icon: Option<String>,
}

impl AppState {
    /// The replicated server variables — `[server.vars]` plus whatever the
    /// running scripts have set. See [`info::payload`] for the precedence.
    #[must_use]
    pub fn server_vars(&self) -> std::sync::Arc<dashmap::DashMap<String, String>> {
        self.script_host.server_vars()
    }

    /// The token `/info.json` publishes, if this server has an identity.
    ///
    /// `None` is the honest answer for a server with no CFX identity, and it
    /// is what keeps the client from looking up a policy that does not exist.
    #[must_use]
    pub fn license_token(&self) -> Option<&str> {
        self.cfx
            .as_deref()
            .map(baston_cfx::CfxIdentity::info_json_token)
    }
}

/// Build the gateway router.
pub fn router(state: Arc<AppState>) -> Router {
    let download_timeout = state.downloads.timeout;
    Router::new()
        .route("/info.json", get(info::info_json))
        // The server list queries both of these back after a heartbeat.
        .route("/dynamic.json", get(info::dynamic_json))
        .route("/players.json", get(info::players_json))
        .route(
            "/files/{resource}/{*path}",
            get(files::serve_resource_file)
                .head(files::serve_resource_file)
                // Unlike the disk-read timeout, this deadline also protects a
                // semaphore permit when the remote peer stops consuming body
                // frames. It resets after each successfully emitted frame.
                .layer(ResponseBodyTimeoutLayer::new(download_timeout)),
        )
        .route("/client", post(client::client_connect))
        .route(
            "/admin/player/{source}/drop",
            post(client::admin_drop_player),
        )
        // Resource-owned endpoints (SetHttpHandler). Last, and matched only
        // when no static gateway route claims the path.
        .route("/{resource}", any(resource_endpoint::serve_resource_root))
        .route(
            "/{resource}/{*path}",
            any(resource_endpoint::serve_resource_path),
        )
        // CORS for the FiveM CEF browser and cross-origin info.json reads.
        // Any origin, but only the methods we actually serve and no arbitrary
        // request headers — so a browser can't be tricked into a credentialed
        // cross-origin call to the authenticated drop route.
        .layer(CorsLayer::new().allow_origin(Any).allow_methods([
            Method::GET,
            Method::HEAD,
            Method::POST,
        ]))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
