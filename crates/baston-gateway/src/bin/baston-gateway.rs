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
    let (net_bridge, net_rx) = baston_scripting::NetBridge::new();
    let script_host =
        ScriptHost::spawn_with_net(Arc::clone(&deferrals), Arc::clone(&players), net_bridge)?;
    let resource_manager = ResourceManager::new(script_host.clone(), config.resources.path.clone());

    resource_manager.discover().await?;
    resource_manager.start_all().await?;

    // Keep the watcher alive for the process lifetime.
    let _watcher = if config.dev.hot_reload {
        Some(spawn_hot_reload(Arc::clone(&resource_manager))?)
    } else {
        None
    };

    // Phase C: entity state pipeline (Zone side, in-process until Phase D).
    let entity_manager = Arc::new(baston_zone::EntityManager::new());
    let state_ingest = Arc::new(baston_zone::StateIngest::new(
        Arc::clone(&entity_manager),
        config.state_sync.max_speed_mps,
    ));
    let nats = match async_nats::connect(&config.nats.url).await {
        Ok(client) => {
            baston_zone::state_sync::setup_nats_stream(&client).await?;
            let emitter = baston_zone::StateSyncEmitter::new(
                config.nats.zone_id.clone(),
                client.clone(),
                Arc::clone(&entity_manager),
                config.state_sync.sync_interval_ms,
            );
            tokio::spawn(emitter.run());
            Some(client)
        }
        // The zone must boot without NATS (dev without docker) — but state
        // sync between players is then disabled.
        Err(e) => {
            tracing::error!(target: "nats", url = %config.nats.url, error = %e,
                "NATS unreachable — Phase C state sync DISABLED");
            None
        }
    };

    let port = config.server.port;
    let udp_port = config.udp.port.unwrap_or(port);
    let udp = baston_gateway::udp::spawn_with_net(
        udp_port,
        config.udp.poll_interval_ms,
        config.server.max_players,
        Arc::clone(&players),
        script_host.clone(),
        Some(net_rx),
        Some(Arc::clone(&state_ingest)),
    )?;
    let _ = (&nats, &udp); // aggregator wiring lands in jalon C3

    let auth = AuthService::new(&config.auth)?;
    let state = Arc::new(AppState {
        config,
        resource_manager,
        players,
        script_host,
        auth,
        packfiles: baston_gateway::http::PackfileCache::new(),
    });

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "HTTP gateway listening");
    axum::serve(listener, router(state)).await?;
    Ok(())
}
