//! Standalone zone-server binary (Phase D).
//!
//! Owns the zone-side stack: EntityManager, StateIngest, StateSyncEmitter,
//! script runtimes + resources, and the federation layer (registration,
//! heartbeat, ZoneService gRPC). Clients never connect here — the Gateway
//! is the only FiveM-facing process; state flows over NATS, control over gRPC.

use std::path::Path;
use std::sync::Arc;

use baston_config::BastonConfig;
use baston_protocol::Aabb;
use baston_scripting::{DeferralRegistry, ScriptHost};
use baston_zone::mesh::{ZoneMesh, ZoneMeshHooks};
use baston_zone::resource_loader::{spawn_hot_reload, ResourceManager};

#[cfg(windows)]
fn raise_timer_resolution() {
    #[link(name = "winmm")]
    extern "system" {
        fn timeBeginPeriod(u_period: u32) -> u32;
    }
    let result = unsafe { timeBeginPeriod(1) };
    if result != 0 {
        tracing::warn!(target: "zone", result, "timeBeginPeriod(1) failed — expect timer jitter");
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    #[cfg(windows)]
    raise_timer_resolution();

    let config_path = std::env::var("BASTON_CONFIG").unwrap_or_else(|_| "baston.toml".into());
    let config = BastonConfig::load(Path::new(&config_path))?;
    let zone_id = config.nats.zone_id.clone();

    let bounds_str = config
        .meshing
        .zone_bounds
        .clone()
        .ok_or_else(|| anyhow::anyhow!("ZONE_BOUNDS (or [meshing].zone_bounds) is required"))?;
    let bounds = Aabb::parse(&bounds_str).map_err(|e| anyhow::anyhow!("ZONE_BOUNDS: {e}"))?;

    tracing::info!(target: "zone", zone = %zone_id,
        "baston-zone starting — bounds=({},{},{},{}) gateway={}",
        bounds.x_min, bounds.y_min, bounds.x_max, bounds.y_max, config.meshing.gateway_grpc);

    if config.metrics.enabled {
        let addr = std::net::SocketAddr::from(([0, 0, 0, 0], config.metrics.port));
        if let Err(e) = metrics_exporter_prometheus::PrometheusBuilder::new()
            .with_http_listener(addr)
            .install()
        {
            tracing::error!(target: "zone", error = %e, "metrics exporter failed to start");
        }
    }

    // ── Zone entity/state stack (Phase C components, now zone-process-owned) ──
    let entity_manager = Arc::new(baston_zone::EntityManager::new());
    let state_ingest = Arc::new(baston_zone::StateIngest::new(
        Arc::clone(&entity_manager),
        config.state_sync.max_speed_mps,
    ));

    let nats = async_nats::connect(&config.nats.url).await.map_err(|e| {
        anyhow::anyhow!("NATS unreachable at {} — a zone cannot run without it: {e}", config.nats.url)
    })?;
    baston_zone::state_sync::setup_nats_stream(&nats).await?;
    let emitter = baston_zone::StateSyncEmitter::new(
        zone_id.clone(),
        nats.clone(),
        Arc::clone(&entity_manager),
        config.state_sync.sync_interval_ms,
    );
    tokio::spawn(emitter.run());

    // ── Scripting: resources run inside the zone process in Phase D ──
    let deferrals = Arc::new(DeferralRegistry::new());
    let players = Arc::new(baston_protocol::PlayerDirectory::new());
    let (net_bridge, mut net_rx) = baston_scripting::NetBridge::new();
    let script_host =
        ScriptHost::spawn_with_net(Arc::clone(&deferrals), Arc::clone(&players), net_bridge)?;
    let resource_manager = ResourceManager::new(script_host.clone(), config.resources.path.clone());
    resource_manager.discover().await?;
    resource_manager.start_all().await?;
    let _watcher = if config.dev.hot_reload {
        Some(spawn_hot_reload(Arc::clone(&resource_manager))?)
    } else {
        None
    };

    // Outbound client events can't leave a zone directly (no UDP here):
    // relay to the Gateway over NATS, keyed by zone so the Gateway can
    // route to the right peer.
    {
        let nats = nats.clone();
        let subject = format!("baston.zone.{zone_id}.outbound");
        tokio::spawn(async move {
            while let Some(msg) = net_rx.recv().await {
                let baston_scripting::NetOutbound::ClientEvent { source, event, args_json } = msg;
                let payload = serde_json::json!({
                    "source": source, "event": event, "args": args_json,
                });
                if let Err(e) = nats.publish(subject.clone(), payload.to_string().into()).await {
                    tracing::error!(target: "zone", error = %e,
                        "failed to relay client event to gateway");
                }
            }
        });
    }

    // ── Ownership monitor (C5) ──
    let owner_event_host = script_host.clone();
    let ownership = baston_zone::OwnershipMonitor::new(
        Arc::clone(&entity_manager),
        Arc::clone(&state_ingest),
        config.state_sync.ownership_interval_secs,
        Some(Arc::new(move |entity_id, new_owner| {
            let host = owner_event_host.clone();
            tokio::spawn(async move {
                let args =
                    [serde_json::json!(entity_id.to_string()), serde_json::json!(new_owner)];
                if let Err(e) = host.trigger_event("onEntityOwnerChanged", &args).await {
                    tracing::warn!(target: "zone", error = %e, "onEntityOwnerChanged dispatch failed");
                }
            });
        })),
    );
    tokio::spawn(ownership.run());

    // ── Federation: ZoneService gRPC + registration + heartbeats ──
    let em_for_count = Arc::clone(&entity_manager);
    let players_for_count = Arc::clone(&players);
    let hooks = ZoneMeshHooks {
        player_count: Arc::new(move || players_for_count.count() as u32),
        entity_count: Arc::new(move || em_for_count.count() as u32),
        // Full activation (playerJoining + script_state restore) lands in D4.
        on_activate_player: Arc::new(|player_id, _snapshot| {
            tracing::info!(target: "zone", player = player_id, "activate hook (D4 wiring pending)");
        }),
        on_release_player: Arc::new(|player_id, reason| {
            tracing::info!(target: "zone", player = player_id, reason, "release hook");
        }),
    };
    let mesh = ZoneMesh::connect(
        zone_id.clone(),
        bounds,
        &config.meshing.gateway_grpc,
        config.zone_public_grpc_addr(),
        config.server.max_players,
        hooks,
    )
    .await?;

    let grpc_addr: std::net::SocketAddr = config.meshing.zone_grpc_addr.parse()?;
    let grpc_task = mesh.spawn_grpc_server(grpc_addr);
    mesh.register_with_gateway().await?;
    mesh.spawn_heartbeat_loop(config.meshing.heartbeat_interval_secs);

    tracing::info!(target: "zone", zone = %zone_id, "zone online");
    grpc_task.await?;
    Ok(())
}
