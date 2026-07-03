//! BASTON gateway binary — Phase A runs gateway + zone in one process.

use std::path::Path;
use std::sync::Arc;

use baston_config::BastonConfig;
use baston_gateway::{router, AppState, AuthService, PlayerRegistry};
use baston_scripting::{DeferralRegistry, ScriptHost};
use baston_zone::resource_loader::{spawn_hot_reload, ResourceManager};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tower_http=info".into()),
        )
        .init();

    let config_path = std::env::var("BASTON_CONFIG").unwrap_or_else(|_| "baston.toml".into());
    let config = BastonConfig::load(Path::new(&config_path))?;
    tracing::info!(name = %config.server.name, port = config.server.port, "starting BASTON");

    if config.dev.auth_bypass {
        tracing::warn!(target: "baston", "dev.auth_bypass is enabled — CFX tickets are NOT validated");
    }

    let deferrals = Arc::new(DeferralRegistry::new());
    let players = Arc::new(PlayerRegistry::new());
    let script_host = ScriptHost::spawn(Arc::clone(&deferrals), Arc::clone(&players))?;
    let resource_manager = ResourceManager::new(script_host.clone(), config.resources.path.clone());

    resource_manager.discover().await?;
    resource_manager.start_all().await?;

    // Keep the watcher alive for the process lifetime.
    let _watcher = if config.dev.hot_reload {
        Some(spawn_hot_reload(Arc::clone(&resource_manager))?)
    } else {
        None
    };

    let port = config.server.port;
    let auth = AuthService::new(&config.auth)?;
    let state = Arc::new(AppState {
        config,
        resource_manager,
        players,
        script_host,
        auth,
    });

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "HTTP gateway listening");
    axum::serve(listener, router(state)).await?;
    Ok(())
}
