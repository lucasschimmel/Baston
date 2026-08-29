//! BASTON gateway binary — Phase A runs gateway + zone in one process.

use std::path::Path;
use std::sync::Arc;

use baston_config::BastonConfig;
use baston_gateway::voice::GatewayVoice;
use baston_gateway::{router, AppState, AuthService, PlayerRegistry};
use baston_scripting::{DeferralRegistry, ScriptHost};
use baston_zone::resource_loader::{spawn_hot_reload, ResourceManager};

/// Raise the Windows timer resolution to 1ms. Without this the default
/// ~15.6ms granularity turns the 16ms StateSyncEmitter tick into ~31ms
/// (jitter ~15ms vs the < 2ms Phase C target).
#[cfg(windows)]
fn raise_timer_resolution() {
    #[link(name = "winmm")]
    extern "system" {
        fn timeBeginPeriod(u_period: u32) -> u32;
    }
    // 0 = TIMERR_NOERROR; failure just means default resolution.
    let result = unsafe { timeBeginPeriod(1) };
    if result != 0 {
        tracing::warn!(target: "baston", result, "timeBeginPeriod(1) failed — expect timer jitter");
    }
}

/// Startup banner. BASTON is a from-scratch FiveM server core in Rust — not a
/// fork of the C++ FXServer — so the banner states exactly what's running.
fn print_banner() {
    const RESET: &str = "\x1b[0m";
    const C: &str = "\x1b[38;5;208m"; // orange
    const D: &str = "\x1b[38;5;245m"; // dim
    println!(
        "\n{C} ██████╗  █████╗ ███████╗████████╗ ██████╗ ███╗   ██╗{RESET}\n\
           {C} ██╔══██╗██╔══██╗██╔════╝╚══██╔══╝██╔═══██╗████╗  ██║{RESET}\n\
           {C} ██████╔╝███████║███████╗   ██║   ██║   ██║██╔██╗ ██║{RESET}\n\
           {C} ██╔══██╗██╔══██║╚════██║   ██║   ██║   ██║██║╚██╗██║{RESET}\n\
           {C} ██████╔╝██║  ██║███████║   ██║   ╚██████╔╝██║ ╚████║{RESET}\n\
           {C} ╚═════╝ ╚═╝  ╚═╝╚══════╝   ╚═╝    ╚═════╝ ╚═╝  ╚═══╝{RESET}\n\
           {D}   a from-scratch FiveM server core, written in Rust{RESET}\n\
           {D}   ── NOT a fork of the C++ FXServer ──{RESET}\n\
           \n\
           {D}   transport   {RESET}ENet 1.3 + FiveM message layer (protocol reverse-engineered)\n\
           {D}   auth        {RESET}real CFX ticket verification (offline RSA)\n\
           {D}   scripting   {RESET}deno_core / V8 — runs FiveM JS resources unmodified\n\
           {D}   state sync  {RESET}NATS JetStream · dirty-flag deltas · AoI culling\n\
           {D}   benchmarked {RESET}100 players @ p50 39ms / p99 69ms · 0.6 Mbps · 0 desyncs\n\
           {D}   version     {RESET}baston {ver} · tokio · axum · rustls\n",
        ver = env!("CARGO_PKG_VERSION"),
    );
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Both ring (reqwest) and aws-lc-rs (axum-server) are compiled in.
    // Install aws-lc-rs explicitly so rustls doesn't panic at first TLS use.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    print_banner();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tower_http=info".into()),
        )
        .init();

    #[cfg(windows)]
    raise_timer_resolution();

    let config_path = std::env::var("BASTON_CONFIG").unwrap_or_else(|_| "baston.toml".into());
    let mut config = BastonConfig::load(Path::new(&config_path))?;
    // Authenticate and apply the licensed slot cap before opening metrics,
    // voice, game, admin, or HTTP listeners.
    let mut cfx_runtime = baston_gateway::cfx::bootstrap(&mut config).await?;
    tracing::info!(name = %config.server.name, port = config.server.port,
        "BASTON online — speaking the FiveM protocol, zero FXServer C++");

    // Prometheus /metrics endpoint (jalon C6).
    if config.metrics.enabled {
        let addr = std::net::SocketAddr::from(([0, 0, 0, 0], config.metrics.port));
        match metrics_exporter_prometheus::PrometheusBuilder::new()
            .with_http_listener(addr)
            .install()
        {
            Ok(()) => tracing::info!(target: "baston", %addr, "Prometheus exporter listening"),
            Err(e) => {
                tracing::error!(target: "baston", error = %e, "metrics exporter failed to start")
            }
        }
    }

    if config.dev.auth_bypass {
        tracing::warn!(target: "baston", "dev.auth_bypass is enabled — CFX tickets are NOT validated");
    }

    // Embedded Mumble-compatible voice server (baston-voice). Spawned before
    // the script host so the MUMBLE_* natives are live from the first
    // resource load. Sessions are created when a client's Mumble side
    // authenticates; the UDP task tears them down on game disconnect.
    let voice = if config.voice.enabled {
        Some(
            baston_voice::server::spawn(baston_voice::server::VoiceServerConfig {
                bind: std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
                port: config.voice.port,
            })
            .await?,
        )
    } else {
        None
    };

    let deferrals = Arc::new(DeferralRegistry::new());
    let players = Arc::new(PlayerRegistry::new());
    let (net_bridge, net_rx) = baston_scripting::NetBridge::new();
    let script_host =
        ScriptHost::spawn_with_net(Arc::clone(&deferrals), Arc::clone(&players), net_bridge)?;
    if let Some(voice) = &voice {
        script_host.set_voice_control(Arc::new(GatewayVoice(voice.clone())));
    }
    // Resource KVP is durable storage from a script's point of view, so it has
    // to be loaded before the first resource runs and swept afterwards: a
    // deferred write must not sit in memory until a crash takes it.
    {
        let kvp = Arc::new(baston_scripting::KvpStore::open(
            config.resources.kvp_path.clone(),
        ));
        script_host.set_kvp_store(Arc::clone(&kvp));
        baston_scripting::KvpStore::spawn_periodic_flush(
            kvp,
            std::time::Duration::from_secs(config.resources.kvp_flush_interval_secs.max(1)),
        );
    }
    // Outbound HTTP (`PerformHttpRequest`). Wired before the first resource
    // starts, because a resource's onResourceStart routinely calls out.
    {
        let (bridge, requests) = baston_scripting::HttpBridge::new();
        script_host.set_http_bridge(bridge);
        baston_gateway::script_http::spawn_worker(
            requests,
            script_host.clone(),
            baston_gateway::script_http::OutboundHttpPolicy::new(&config.resources),
        );
    }
    let resource_manager = ResourceManager::new(script_host.clone(), config.resources.path.clone());

    // `StartResource` and friends. The transitions run on this task rather
    // than inside the calling isolate: the manager loads resources *through*
    // the script host, so doing it inline would re-enter the very runtime the
    // native was invoked from.
    {
        let (control, mut commands) = baston_scripting::QueuedResourceControl::new();
        script_host.set_resource_control(Arc::new(control));
        let manager = Arc::clone(&resource_manager);
        tokio::spawn(async move {
            while let Some(command) = commands.recv().await {
                use baston_scripting::ResourceCommand;
                let (what, name, result) = match command {
                    ResourceCommand::Start(name) => {
                        let result = manager.start(&name).await;
                        ("start", name, result)
                    }
                    ResourceCommand::Stop(name) => {
                        let result = manager.stop(&name).await;
                        ("stop", name, result)
                    }
                    ResourceCommand::Restart(name) => {
                        let result = manager.restart(&name).await;
                        ("restart", name, result)
                    }
                    // The manager only knows one resource root, so a scan of
                    // any other path has nothing to discover.
                    ResourceCommand::ScanRoot(root) => {
                        let result = manager.discover().await.map(|_| ());
                        ("scan", root, result)
                    }
                };
                if let Err(e) = result {
                    tracing::warn!(
                        target: "resources",
                        %name,
                        error = %e,
                        "a script's resource {what} request failed"
                    );
                }
            }
        });
    }

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
            // Mesh mode: the zone processes emit state; the gateway would
            // duplicate every entity on NATS if it also ran an emitter.
            if !config.meshing.enabled {
                let emitter = baston_zone::StateSyncEmitter::new(
                    config.nats.zone_id.clone(),
                    client.clone(),
                    Arc::clone(&entity_manager),
                    config.state_sync.sync_interval_ms,
                );
                tokio::spawn(emitter.run());
            }
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

    // Jalon C5: dynamic network ownership with scripted notification.
    // Mesh mode: ownership is a zone-process concern.
    if !config.meshing.enabled {
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
    }

    // Phase D: zone federation (gRPC registry + routing). Disabled by default;
    // Docker Compose enables it via [meshing] in baston.toml or env.
    let (mesh, mut recovery_kick_rx) = if config.meshing.enabled {
        let registry = Arc::new(baston_gateway::ZoneRegistry::new(
            std::time::Duration::from_secs(config.meshing.zone_timeout_secs),
        ));
        let router = Arc::new(baston_gateway::ConnectionRouter::new());
        let mesh = baston_gateway::GatewayMesh::new(Arc::clone(&registry), Arc::clone(&router));
        mesh.set_player_directory(Arc::clone(&players));
        let grpc_addr: std::net::SocketAddr = config.meshing.gateway_grpc_addr.parse()?;
        mesh.spawn_grpc_server(grpc_addr);
        // Zone failure recovery (D6): reroute orphans to surviving zones.
        // The UDP handle doesn't exist yet at this point — recovery kicks go
        // through a channel drained after the UDP server starts.
        let (kick_tx, kick_rx) = tokio::sync::mpsc::unbounded_channel::<(u32, String)>();
        {
            let mesh = Arc::clone(&mesh);
            registry.spawn_liveness_monitor(Arc::new(move |zone_id| {
                let mesh = Arc::clone(&mesh);
                let kick_tx = kick_tx.clone();
                tokio::spawn(async move {
                    mesh.handle_zone_failure(&zone_id, &move |source, reason| {
                        let _ = kick_tx.send((source, reason.to_owned()));
                    })
                    .await;
                });
            }));
        }
        let recovery_kick_rx: Option<tokio::sync::mpsc::UnboundedReceiver<(u32, String)>> =
            Some(kick_rx);
        // Admin/API listener spawns after the UDP server exists (the kick
        // route needs the UdpHandle) — see spawn_api below.
        // Zone-resolution request/reply for entity handoffs (D4).
        if let Some(nats) = nats.clone() {
            let registry = Arc::clone(&registry);
            tokio::spawn(async move {
                use futures::StreamExt;
                let mut sub = match nats
                    .subscribe(baston_zone::boundary_loop::RESOLVE_ZONE_SUBJECT)
                    .await
                {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!(target: "gateway", error = %e,
                            "resolve_zone subscription failed");
                        return;
                    }
                };
                while let Some(msg) = sub.next().await {
                    let Some(reply) = msg.reply else { continue };
                    let text = String::from_utf8_lossy(&msg.payload);
                    let zone = match text.split_once(',') {
                        Some((x, y)) => match (x.trim().parse::<f32>(), y.trim().parse::<f32>()) {
                            (Ok(x), Ok(y)) => registry
                                .find_zone_for_coords(x, y)
                                .await
                                .unwrap_or_default(),
                            _ => String::new(),
                        },
                        None => String::new(),
                    };
                    let _ = nats.publish(reply, zone.into()).await;
                }
            });
        }
        // Routing-table hygiene: drop entries for players that disconnected
        // (the UDP layer removes them from the registry; without this sweep
        // the router accretes stale sources across sessions).
        {
            let mesh = Arc::clone(&mesh);
            let players = Arc::clone(&players);
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    interval.tick().await;
                    for (source, _) in mesh.router.all() {
                        if players.get(source).is_none() {
                            mesh.router.remove(source);
                        }
                    }
                }
            });
        }

        // D5: publish the global player list every 2s so zones can answer
        // GetPlayers()/GetPlayerName() across zone boundaries.
        if let Some(nats) = nats.clone() {
            let players = Arc::clone(&players);
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    interval.tick().await;
                    let list: Vec<baston_protocol::PlayerInfo> = players
                        .sources()
                        .into_iter()
                        .filter_map(|s| players.get(s))
                        .collect();
                    let Ok(payload) = serde_json::to_vec(&list) else {
                        continue;
                    };
                    if let Err(e) = nats
                        .publish(
                            baston_zone::boundary_loop::GLOBAL_PLAYERS_SUBJECT,
                            payload.into(),
                        )
                        .await
                    {
                        tracing::warn!(target: "gateway", error = %e,
                            "global player list publish failed");
                    }
                }
            });
        }
        (Some(mesh), recovery_kick_rx)
    } else {
        (None, None)
    };

    // D4: forward client state updates to the player's current zone, with a
    // 50ms hold around handoff commits (no packet lost mid-switch).
    let mesh_forward = match (&mesh, nats.clone()) {
        (Some(mesh), Some(nats_client)) => {
            let fwd = baston_gateway::mesh_forward::MeshForwarder::spawn(
                nats_client,
                Arc::clone(&mesh.router),
                Arc::clone(&mesh.registry),
            );
            let hook_fwd = fwd.clone();
            mesh.set_handoff_committed_hook(Arc::new(move |source, from, to| {
                tracing::info!(target: "gateway",
                    "handoff committed: player={source} {from}→{to} — buffering UDP 50ms");
                hook_fwd.begin_handoff_hold(source);
            }));
            Some(fwd)
        }
        _ => None,
    };

    let port = config.server.port;
    let bind_address = config.server.bind_address;
    let udp_port = config.udp.port.unwrap_or(port);
    let addr = std::net::SocketAddr::new(bind_address, port);
    let tcp_listener = std::net::TcpListener::bind(addr)?;
    tcp_listener.set_nonblocking(true)?;
    let udp = baston_gateway::udp::spawn_with_mesh_on(
        bind_address,
        udp_port,
        config.udp.poll_interval_ms,
        config.server.max_players,
        Arc::clone(&players),
        script_host.clone(),
        Some(net_rx),
        Some(Arc::clone(&state_ingest)),
        mesh_forward,
        config.state_sync.clone(),
        // Clients are forced onto this build, so it is the one whose sync-tree
        // node layouts the server must decode against.
        baston_protocol::rage::sync_parse::GameBuild(
            config
                .server
                .enforce_game_build
                .parse()
                .unwrap_or_else(|_| baston_protocol::rage::sync_parse::GameBuild::default().0),
        ),
        baston_gateway::debug_info::DebugFeedSetup {
            config: config.debug.clone(),
            server_name: config.server.name.clone(),
            // The overlay's mesh section only exists where a federation does.
            mesh: mesh.as_ref().map(|mesh| baston_gateway::MeshView {
                registry: Arc::clone(&mesh.registry),
                router: Arc::clone(&mesh.router),
                handoff_margin: config.meshing.boundary_margin,
            }),
        },
    )?;
    // Entity creation from scripts: the control surface reserves network ids
    // synchronously (a script needs its handle back immediately) and queues the
    // creation for the UDP task's next sync tick, which owns the world.
    {
        let (world_control, world_commands) = baston_gateway::GatewayWorldControl::new();
        script_host.set_world_control(world_control);
        udp.set_world_commands(world_commands);
    }
    // Voice teardown follows the game connection; clients learn the voice
    // endpoint through the replicated voice_external* convars.
    if let Some(voice) = &voice {
        let advertise = (!config.voice.external_address.is_empty())
            .then(|| (config.voice.external_address.clone(), config.voice.port));
        if advertise.is_none() {
            tracing::warn!(target: "voice",
                "[voice] external_address is empty — clients will not be told where the voice server is");
        }
        udp.set_voice(voice.clone(), advertise);
    }

    // Admin + monitoring/control API on the admin port. Legacy /admin/*
    // routes need the mesh; /api/v1/* works in single-process mode too.
    {
        let keyring = Arc::new(baston_gateway::api::KeyRing::from_config(
            &config.api,
            &config.meshing.admin_token,
        ));
        let audit = if keyring.is_empty() {
            baston_gateway::api::AuditLog::disabled()
        } else {
            baston_gateway::api::AuditLog::spawn(config.api.audit_log.clone())
        };
        let legacy = mesh.as_ref().map(|mesh| baston_gateway::admin::AdminState {
            mesh: Arc::clone(mesh),
            players: Arc::clone(&players),
            token: config.meshing.admin_token.clone(),
        });
        baston_gateway::api::spawn_api(
            baston_gateway::api::ApiState {
                keyring,
                audit,
                players: Arc::clone(&players),
                resource_manager: Arc::clone(&resource_manager),
                observability: script_host.observability(),
                mesh: mesh.clone(),
                udp: Some(udp.clone()),
                server_name: config.server.name.clone(),
                max_players: config.server.max_players,
                started_at: std::time::Instant::now(),
            },
            legacy,
            config.meshing.admin_port,
        );
    }

    // D6: apply recovery kicks now that the UDP handle exists.
    if let Some(mut rx) = recovery_kick_rx.take() {
        let udp = udp.clone();
        tokio::spawn(async move {
            while let Some((source, reason)) = rx.recv().await {
                tracing::warn!(target: "gateway", source, %reason, "kicking orphaned player");
                udp.drop_source(source);
            }
        });
    }

    // D4/D5: relay zone-emitted client events (TriggerClientEvent from a zone
    // process) to the right UDP peer.
    if config.meshing.enabled {
        if let Some(nats) = nats.clone() {
            let udp = udp.clone();
            tokio::spawn(async move {
                use futures::StreamExt;
                let mut sub = match nats
                    .subscribe(baston_gateway::mesh_forward::outbound_subject_wildcard())
                    .await
                {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!(target: "gateway", error = %e,
                            "zone outbound subscription failed");
                        return;
                    }
                };
                while let Some(msg) = sub.next().await {
                    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&msg.payload) else {
                        continue;
                    };
                    let (Some(source), Some(event)) = (v["source"].as_u64(), v["event"].as_str())
                    else {
                        continue;
                    };
                    // A zone can relay either JSON args to encode here, or a
                    // payload the resource already packed itself
                    // (TriggerClientEventInternal), which crosses NATS as a
                    // byte array because JSON has no binary type.
                    let args = if let Some(raw) = v["raw"].as_array() {
                        Some(
                            raw.iter()
                                .filter_map(|b| b.as_u64().and_then(|b| u8::try_from(b).ok()))
                                .collect::<Vec<u8>>(),
                        )
                    } else {
                        match v["args"]
                            .as_str()
                            .map(baston_protocol::events::json_args_to_msgpack)
                        {
                            Some(Ok(args)) => Some(args),
                            Some(Err(e)) => {
                                tracing::error!(target: "gateway", error = %e,
                                    "zone outbound event encode failed");
                                None
                            }
                            None => None,
                        }
                    };
                    if let Some(args) = args {
                        let packet = baston_protocol::events::build_net_event(event, &args);
                        udp.control().send(source as u32, 0, packet);
                    }
                }
            });
        }
    }

    // Jalon C3: NATS → per-client AoI-filtered snapshots.
    if let Some(nats) = nats {
        baston_gateway::StateAggregator::new(
            nats,
            Arc::clone(&players),
            Arc::clone(&state_ingest),
            udp.clone(),
            config.state_sync.aoi_radius,
            config.state_sync.push_interval_ms,
        )
        .spawn();
    }

    let auth = AuthService::new(&config.auth)?;
    let state = Arc::new(AppState {
        license_token: std::sync::RwLock::new(
            cfx_runtime.as_ref().map(|runtime| runtime.token().clone()),
        ),
        downloads: baston_gateway::http::DownloadPolicy::new(&config.resources),
        builtins: baston_gateway::http::BuiltinResources::from_config(&config),
        config,
        resource_manager,
        players,
        script_host,
        auth,
        packfiles: baston_gateway::http::PackfileCache::new(),
        streams: baston_gateway::http::StreamCache::new(),
        mesh,
    });

    let http_state = Arc::clone(&state);
    let (http_ready_tx, http_ready_rx) = tokio::sync::oneshot::channel();
    let mut http_server = tokio::spawn(async move {
        if let Some(tls) = http_state.config.tls.clone() {
            let tls_config =
                axum_server::tls_rustls::RustlsConfig::from_pem_file(&tls.cert_pem, &tls.key_pem)
                    .await?;
            tracing::info!(%addr, "HTTPS gateway listening");
            let server = axum_server::from_tcp_rustls(tcp_listener, tls_config);
            let _ = http_ready_tx.send(());
            // Connect info, so a resource's HTTP handler sees the peer address
            // the way FXServer reports `request.address`.
            server
                .serve(
                    router(http_state)
                        .into_make_service_with_connect_info::<std::net::SocketAddr>(),
                )
                .await?;
        } else {
            // Plain HTTP is the Phase B-validated mode: the FiveM client sends
            // some game-port requests in plaintext, and getConfiguration hands
            // out a literal http://<host>/files URL so downloads stay plain.
            let listener = tokio::net::TcpListener::from_std(tcp_listener)?;
            tracing::info!(%addr, "HTTP gateway listening");
            let _ = http_ready_tx.send(());
            axum::serve(
                listener,
                router(http_state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await?;
        }
        Ok::<(), anyhow::Error>(())
    });
    http_ready_rx
        .await
        .map_err(|_| anyhow::anyhow!("HTTP gateway stopped before its accept loop was ready"))?;

    if let Some(runtime) = &mut cfx_runtime {
        tokio::select! {
            result = runtime.activate_public_listing(&state.config) => {
                if let Err(error) = result {
                    http_server.abort();
                    let _ = http_server.await;
                    return Err(error.into());
                }
            }
            result = &mut http_server => {
                result??;
                anyhow::bail!("HTTP gateway stopped during public CFX activation");
            }
        }
        *state
            .license_token
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(runtime.token().clone());

        tokio::select! {
            result = &mut http_server => result??,
            reason = runtime.wait_for_failure() => {
                http_server.abort();
                anyhow::bail!(
                    "authenticated CFX broker stopped; shutting down the gateway: {reason}"
                );
            }
        }
    } else {
        http_server.await??;
    }
    Ok(())
}
