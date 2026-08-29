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

/// Wire the optional escrow bridge. Global CFX identity belongs exclusively to
/// the public gateway, so zone processes never authenticate, register, or
/// heartbeat independently.
#[cfg(all(feature = "escrow", windows))]
fn wire_cfx_sidecar(
    manager: &Arc<ResourceManager>,
    config: &BastonConfig,
) -> anyhow::Result<Option<baston_escrow_plugin::SidecarHandle>> {
    if !config.escrow.enabled {
        return Ok(None);
    }
    if config.escrow.backend == baston_config::EscrowBackend::Direct {
        anyhow::bail!(
            "[escrow] backend = \"direct\" is unsupported (svadhesive exposes no FFI \
             decrypt symbol); use backend = \"sidecar\""
        );
    }

    let fxserver_path = config
        .escrow
        .fxserver_path
        .clone()
        .or_else(|| config.license.fxserver_path.clone())
        .ok_or_else(|| anyhow::anyhow!("escrow needs an fxserver_path"))?;

    let license_key = {
        let key = config.license.sv_license_key.trim();
        if key.is_empty() {
            None
        } else {
            Some(key.to_owned())
        }
    };
    if license_key.is_none() {
        tracing::warn!(target: "zone",
            "escrow is enabled but [license] sv_license_key is empty — svadhesive needs the \
             CFX server key to derive escrow decryption keys, so decryption will fail. Set \
             [license] sv_license_key to your key from https://portal.cfx.re");
    }

    let params = baston_escrow_plugin::SidecarParams {
        fxserver_path,
        resources_dir: config.resources.path.clone(),
        license_key,
        // Zone-local escrow brokers allocate distinct private endpoints and
        // never compete with the gateway's authenticated identity broker.
        port: 0,
        public_listing: None,
    };
    let handle = baston_escrow_plugin::SidecarHandle::start(&params)
        .map_err(|e| anyhow::anyhow!("failed to start the FXServer sidecar: {e}"))?;

    manager.set_script_decryptor(handle.decryptor());
    tracing::info!(target: "zone", "escrow plugin active (zone-local CFX sidecar)");

    Ok(Some(handle))
}

#[cfg(not(all(feature = "escrow", windows)))]
fn wire_cfx_sidecar(
    manager: &Arc<ResourceManager>,
    config: &BastonConfig,
) -> anyhow::Result<Option<()>> {
    let _ = manager;
    if config.escrow.enabled {
        #[cfg(not(feature = "escrow"))]
        tracing::warn!(target: "zone",
            "escrow.enabled = true but this binary was built without the `escrow` feature \
             — rebuild with `--features escrow`; continuing with plain resources");
        #[cfg(all(feature = "escrow", not(windows)))]
        tracing::warn!(target: "zone",
            "escrow.enabled = true but this is not a Windows build — svadhesive.dll is \
             Windows-only; continuing with plain resources");
    }
    Ok(None)
}

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
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
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
        anyhow::anyhow!(
            "NATS unreachable at {} — a zone cannot run without it: {e}",
            config.nats.url
        )
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
    // Optional zone-local escrow bridge. Gateway owns global CFX identity.
    // Keep the handle alive for resource decryptions.
    let _cfx_sidecar = wire_cfx_sidecar(&resource_manager, &config)?;
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
                // JSON args are relayed as text for the gateway to encode; an
                // already-packed payload crosses as a byte array, because JSON
                // has no binary type and re-encoding it would corrupt it.
                let payload = match msg {
                    baston_scripting::NetOutbound::ClientEvent {
                        source,
                        event,
                        args_json,
                    } => serde_json::json!({
                        "source": source, "event": event, "args": args_json,
                    }),
                    baston_scripting::NetOutbound::ClientEventRaw {
                        source,
                        event,
                        payload,
                    } => serde_json::json!({
                        "source": source, "event": event, "raw": payload,
                    }),
                };
                if let Err(e) = nats
                    .publish(subject.clone(), payload.to_string().into())
                    .await
                {
                    tracing::error!(target: "zone", error = %e,
                        "failed to relay client event to gateway");
                }
            }
        });
    }

    // Ingest forwarded client state updates from the Gateway
    // (baston.zone.{zone_id}.ingest — the D4 UDP rerouting path).
    {
        let nats = nats.clone();
        let ingest = Arc::clone(&state_ingest);
        let subject = format!("baston.zone.{zone_id}.ingest");
        tokio::spawn(async move {
            use futures::StreamExt;
            let mut sub = match nats.subscribe(subject.clone()).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(target: "zone", error = %e, "ingest subscription failed");
                    return;
                }
            };
            tracing::info!(target: "zone", %subject, "gateway ingest consumer running");
            while let Some(msg) = sub.next().await {
                match bincode::serde::decode_from_slice::<
                    (u32, baston_protocol::udp::state::ClientStateUpdate),
                    _,
                >(&msg.payload, bincode::config::standard())
                {
                    Ok(((source, update), _)) => {
                        // Rejections logged/counted inside StateIngest.
                        let _ = ingest.apply(source, update);
                    }
                    Err(e) => {
                        tracing::error!(target: "zone", error = %e, "malformed ingest payload");
                    }
                }
            }
        });
    }

    // ── Cross-zone script events (D5) ──
    // Outbound: locally-triggered events mirror to siblings over NATS.
    {
        let nats = nats.clone();
        let zone = zone_id.clone();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, String)>();
        script_host.set_cross_zone_publisher(Arc::new(move |event, args_json| {
            let _ = tx.send((event.to_owned(), args_json.to_owned()));
        }));
        tokio::spawn(async move {
            while let Some((event, args)) = rx.recv().await {
                let payload = serde_json::json!({
                    "from_zone": zone, "event": event, "args": args,
                });
                if let Err(e) = nats
                    .publish(
                        baston_zone::boundary_loop::CROSS_ZONE_EVENT_SUBJECT,
                        payload.to_string().into(),
                    )
                    .await
                {
                    tracing::error!(target: "zone", error = %e, "cross-zone event publish failed");
                }
            }
        });
    }
    // Inbound: dispatch sibling events locally (never re-published).
    {
        let nats = nats.clone();
        let zone = zone_id.clone();
        let host = script_host.clone();
        tokio::spawn(async move {
            use futures::StreamExt;
            let mut sub = match nats
                .subscribe(baston_zone::boundary_loop::CROSS_ZONE_EVENT_SUBJECT)
                .await
            {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(target: "zone", error = %e, "cross-zone event sub failed");
                    return;
                }
            };
            while let Some(msg) = sub.next().await {
                let Ok(v) = serde_json::from_slice::<serde_json::Value>(&msg.payload) else {
                    continue;
                };
                if v["from_zone"].as_str() == Some(zone.as_str()) {
                    continue; // our own broadcast
                }
                let (Some(event), Some(args)) = (v["event"].as_str(), v["args"].as_str()) else {
                    continue;
                };
                if let Err(e) = host.trigger_remote_event(event, args.to_owned()).await {
                    tracing::warn!(target: "zone", error = %e, "remote event dispatch failed");
                }
            }
        });
    }

    // ── Global player list (D5) ── the Gateway publishes every ~2s; mirror
    // it into the local directory so GetPlayers()/GetPlayerName() span zones.
    {
        let nats = nats.clone();
        let players = Arc::clone(&players);
        tokio::spawn(async move {
            use futures::StreamExt;
            let mut sub = match nats
                .subscribe(baston_zone::boundary_loop::GLOBAL_PLAYERS_SUBJECT)
                .await
            {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(target: "zone", error = %e, "global players sub failed");
                    return;
                }
            };
            while let Some(msg) = sub.next().await {
                let Ok(list) =
                    serde_json::from_slice::<Vec<baston_protocol::PlayerInfo>>(&msg.payload)
                else {
                    continue;
                };
                let fresh: std::collections::HashSet<u32> = list.iter().map(|p| p.source).collect();
                for p in list {
                    players.insert(p);
                }
                for source in players.sources() {
                    if !fresh.contains(&source) {
                        players.remove(source);
                    }
                }
            }
        });
    }

    // Inbound ownerless-entity handoffs from sibling zones.
    baston_zone::boundary_loop::BoundaryLoop::spawn_entity_handoff_consumer(
        nats.clone(),
        zone_id.clone(),
        Arc::clone(&entity_manager),
    );

    // ── Ownership monitor (C5) ──
    let owner_event_host = script_host.clone();
    let ownership = baston_zone::OwnershipMonitor::new(
        Arc::clone(&entity_manager),
        Arc::clone(&state_ingest),
        config.state_sync.ownership_interval_secs,
        Some(Arc::new(move |entity_id, new_owner| {
            let host = owner_event_host.clone();
            tokio::spawn(async move {
                let args = [
                    serde_json::json!(entity_id.to_string()),
                    serde_json::json!(new_owner),
                ];
                if let Err(e) = host.trigger_event("onEntityOwnerChanged", &args).await {
                    tracing::warn!(target: "zone", error = %e, "onEntityOwnerChanged dispatch failed");
                }
            });
        })),
    );
    tokio::spawn(Arc::new(ownership).run());

    // ── Federation: ZoneService gRPC + registration + heartbeats ──
    let em_for_count = Arc::clone(&entity_manager);
    // Heartbeat load = players ACTIVE in this zone (the local directory also
    // mirrors the global list for cross-zone GetPlayers, so count via ingest).
    let ingest_for_count = Arc::clone(&state_ingest);
    let hooks = ZoneMeshHooks {
        player_count: Arc::new(move || ingest_for_count.player_kinematics().len() as u32),
        entity_count: Arc::new(move || em_for_count.count() as u32),
        // Ghost → active (D4): register the player locally, then fire
        // playerJoining with the restored script_state.
        on_activate_player: {
            let players = Arc::clone(&players);
            let host = script_host.clone();
            let activate_ingest = Arc::clone(&state_ingest);
            Arc::new(move |player_id, snapshot| {
                players.insert(baston_protocol::PlayerInfo {
                    source: player_id,
                    name: snapshot.name.clone(),
                    identifiers: snapshot.identifiers.clone(),
                });
                // Resurrect the ped and everything the player owned BEFORE
                // scripts observe the join, so a `playerJoining` handler that
                // inspects the player's vehicle or health sees the real state.
                activate_ingest.restore_player_state(snapshot);
                // script_state: resource → JSON text, decoded for the event.
                let state_obj: serde_json::Map<String, serde_json::Value> = snapshot
                    .script_state
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            serde_json::from_str(v).unwrap_or(serde_json::Value::Null),
                        )
                    })
                    .collect();
                let host = host.clone();
                tokio::spawn(async move {
                    let args = [
                        serde_json::json!(player_id),
                        serde_json::Value::Object(state_obj),
                    ];
                    if let Err(e) = host.trigger_event("playerJoining", &args).await {
                        tracing::warn!(target: "zone", error = %e, "playerJoining dispatch failed");
                    }
                });
            })
        },
        on_release_player: {
            let players = Arc::clone(&players);
            let ingest = Arc::clone(&state_ingest);
            let release_em = Arc::clone(&entity_manager);
            Arc::new(move |player_id, reason| {
                tracing::info!(target: "zone", player = player_id, reason, "release hook");
                // Everything the player was authoritative for goes with it —
                // its ped via the ingest, its vehicles and props here.
                // Otherwise each disconnect leaves permanent ghost entities.
                for entity in release_em.entities_owned_by(player_id) {
                    release_em.remove_entity(entity.entity_id);
                }
                ingest.on_player_dropped(player_id);
                players.remove(player_id);
            })
        },
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
    // Resource-control RPCs (gateway admin API) need the ResourceManager.
    mesh.set_resource_manager(Arc::clone(&resource_manager));
    let grpc_task = mesh.spawn_grpc_server(grpc_addr);
    mesh.register_with_gateway().await?;
    mesh.spawn_heartbeat_loop(config.meshing.heartbeat_interval_secs);

    // ── Boundary detection + handoff orchestration (D3/D4) ──
    let handoff_manager = baston_zone::handoff_manager::HandoffManager::new(
        zone_id.clone(),
        mesh.gateway_client(),
        config.meshing.handoff_cooldown_secs,
    );
    mesh.set_handoff_manager(Arc::clone(&handoff_manager));
    let cleanup_ingest = Arc::clone(&state_ingest);
    let cleanup_em = Arc::clone(&entity_manager);
    let cleanup_players = Arc::clone(&players);
    let cleanup_host = script_host.clone();
    baston_zone::boundary_loop::BoundaryLoop {
        detector: baston_zone::boundary_detector::BoundaryDetector::new(
            bounds,
            config.meshing.boundary_margin,
        ),
        manager: Arc::clone(&handoff_manager),
        mesh: Arc::clone(&mesh),
        ingest: Arc::clone(&state_ingest),
        players: Arc::clone(&players),
        scan_interval: std::time::Duration::from_millis(config.meshing.boundary_scan_interval_ms),
        // Real collector (RegisterZoneTransferState) wired via the script host.
        collect_script_state: {
            let host = script_host.clone();
            Arc::new(move |source| {
                let host = host.clone();
                Box::pin(async move { host.collect_zone_transfer_state(source).await })
            })
        },
        post_handoff_cleanup: Arc::new(move |source| {
            // Internal playerDropped: scripts see the player leave THIS zone,
            // nothing is fired to the network (the client never disconnected).
            let host = cleanup_host.clone();
            tokio::spawn(async move {
                let args = [
                    serde_json::json!(source),
                    serde_json::json!("zone handoff (internal)"),
                ];
                if let Err(e) = host.trigger_event("playerDropped", &args).await {
                    tracing::warn!(target: "zone", error = %e, "internal playerDropped failed");
                }
            });
            // Release owned entities + the ped, forget the player locally.
            for entity in cleanup_em.entities_owned_by(source) {
                cleanup_em.remove_entity(entity.entity_id);
            }
            cleanup_ingest.on_player_dropped(source);
            cleanup_players.remove(source);
        }),
        nats: Some(nats.clone()),
    }
    .spawn();

    tracing::info!(target: "zone", zone = %zone_id, "zone online");
    grpc_task.await?;
    Ok(())
}
