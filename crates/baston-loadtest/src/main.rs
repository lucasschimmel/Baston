//! baston-loadtest — jalon C6 benchmark harness.
//!
//! Simulates N binary-protocol clients: full HTTP initConnect (server must
//! run with `dev.auth_bypass = true`), ENet handshake, then a random walk
//! with `msgBastonState` reports every 50ms while consuming
//! `msgBastonSnapshot` pushes.
//!
//! End-to-end latency is measured by stamping each state report's
//! `health`/`armour` fields with a 32-bit send-time (ms since process
//! epoch, exact in two f32s); any client receiving the entity recovers the
//! stamp and compares against the shared clock.
//!
//! Usage: baston-loadtest --clients 100 --duration 60s [--server 127.0.0.1:30120]
//!        [--metrics http://127.0.0.1:9090/metrics]

mod client;
mod report;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Shared measurement sink across all simulated clients.
pub struct Stats {
    pub epoch: Instant,
    pub latencies_ms: Mutex<Vec<f64>>,
    pub bytes_received: AtomicU64,
    pub snapshots_received: AtomicU64,
    pub dropped_connections: AtomicU64,
    pub desyncs: AtomicU64,
    pub connected: AtomicU64,
    pub stop: AtomicBool,
    /// Phase D: boundary crossings performed by crosser clients.
    pub crossings: AtomicU64,
    /// Phase D: snapshot-stream gaps (ms) observed by crossers around their
    /// crossings — the client-visible freeze measure.
    pub crosser_gaps_ms: Mutex<Vec<f64>>,
    /// Freeze measurement starts once the connect storm is over — startup
    /// backlog is not a handoff stall.
    pub measure_freeze: AtomicBool,
    /// `handoffs_committed_total` at test start (gateway may be warm).
    pub handoffs_committed_at_start: AtomicU64,
}

/// Per-client scenario (Phase D adds zone-aware spawns and crossers).
#[derive(Clone, Copy)]
pub struct ClientPlan {
    pub spawn: [f32; 3],
    /// Crossers oscillate across the zone boundary (x = 0).
    pub crosser: bool,
}

impl Stats {
    fn new() -> Self {
        Self {
            epoch: Instant::now(),
            latencies_ms: Mutex::new(Vec::new()),
            bytes_received: AtomicU64::new(0),
            snapshots_received: AtomicU64::new(0),
            dropped_connections: AtomicU64::new(0),
            desyncs: AtomicU64::new(0),
            connected: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            crossings: AtomicU64::new(0),
            crosser_gaps_ms: Mutex::new(Vec::new()),
            measure_freeze: AtomicBool::new(false),
            handoffs_committed_at_start: AtomicU64::new(0),
        }
    }

    pub fn now_ms(&self) -> u32 {
        self.epoch.elapsed().as_millis() as u32
    }
}

struct Args {
    clients: usize,
    duration: Duration,
    server: String,
    metrics_url: String,
    /// Phase D: number of zones (2 = split at x=0).
    zones: usize,
    clients_per_zone: Option<usize>,
    handoffs: bool,
    /// Metrics URLs of the zone processes (handoff latency histograms).
    zone_metrics: Vec<String>,
}

fn parse_args() -> Args {
    let mut args = Args {
        clients: 100,
        duration: Duration::from_secs(60),
        server: "127.0.0.1:30120".to_owned(),
        metrics_url: "http://127.0.0.1:9090/metrics".to_owned(),
        zones: 1,
        clients_per_zone: None,
        handoffs: false,
        zone_metrics: Vec::new(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().unwrap_or_default();
        match flag.as_str() {
            "--clients" => args.clients = value().parse().expect("--clients N"),
            "--duration" => {
                let v = value();
                let v = v.strip_suffix('s').unwrap_or(&v);
                args.duration = Duration::from_secs(v.parse().expect("--duration Ns"));
            }
            "--server" => args.server = value(),
            "--metrics" => args.metrics_url = value(),
            "--zones" => args.zones = value().parse().expect("--zones N"),
            "--clients-per-zone" => {
                args.clients_per_zone = Some(value().parse().expect("--clients-per-zone N"))
            }
            "--handoffs" => args.handoffs = value().parse().expect("--handoffs true|false"),
            "--zone-metrics" => args.zone_metrics = value().split(',').map(str::to_owned).collect(),
            other => {
                eprintln!("unknown flag: {other}");
                std::process::exit(2);
            }
        }
    }
    if let Some(cpz) = args.clients_per_zone {
        args.clients = cpz * args.zones.max(1);
    }
    args
}

/// Zone-aware spawn: clients spread inside their zone's x-band; crossers
/// (10% when --handoffs) spawn 350m from the shared boundary at x = 0.
fn plan_for(index: usize, args: &Args) -> ClientPlan {
    let h = (index as u64).wrapping_mul(0x9E3779B97F4A7C15);
    let y = ((h >> 32) % 3800) as f32 - 1900.0;
    if args.zones <= 1 {
        let x = ((h >> 8) % 4000) as f32 - 2000.0;
        return ClientPlan {
            spawn: [x, y, 20.0],
            crosser: false,
        };
    }
    let zone = index % args.zones;
    let band = 8000.0 / args.zones as f32;
    let x_min = -4000.0 + zone as f32 * band;
    let crosser = args.handoffs && index.is_multiple_of(10) && args.zones == 2;
    if crosser {
        // 350m inside the zone next to the x = 0 boundary, clustered in a
        // ±200m y-band so crossers keep each other inside their AoI: their
        // snapshot stream is then continuous and the freeze measurement
        // (max stream gap) actually observes handoff stalls.
        // 120m spacing: each crosser keeps a handful of peers inside its AoI
        // (continuous snapshot stream for freeze measurement) without piling
        // all crossers into one hyper-dense blob.
        let y = ((index / 10 % 34) as f32) * 120.0 - 2000.0;
        let x = if zone == 0 { -350.0 } else { 350.0 };
        return ClientPlan {
            spawn: [x, y, 20.0],
            crosser: true,
        };
    }
    let x = {
        // Walkers stay ≥ 500m from any boundary: at 1.5 m/s over 300s they
        // can never cross, so `crossings` counts exactly the crossers and
        // the handoff success denominator stays honest.
        x_min + 500.0 + ((h >> 8) as f32 % (band - 1000.0))
    };
    ClientPlan {
        spawn: [x, y, 20.0],
        crosser: false,
    }
}

#[tokio::main]
async fn main() {
    let args = parse_args();
    println!(
        "baston-loadtest: {} clients for {:?} against {}",
        args.clients, args.duration, args.server
    );

    let http_base = format!("http://{}", args.server);
    let udp_port: u16 = args
        .server
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .expect("server must be host:port");
    let host = args.server.trim_end_matches(&format!(":{udp_port}"));

    // Phase 1: HTTP initConnect for every client (needs dev.auth_bypass).
    let http = reqwest::Client::new();
    let mut tokens = Vec::with_capacity(args.clients);
    for i in 0..args.clients {
        let body = format!("method=initConnect&name=load-{i}&protocol=12&gameName=gta5&guid={i}");
        let response = http
            .post(format!("{http_base}/client"))
            .body(body)
            .send()
            .await
            .expect("initConnect request failed — is the server running?");
        let value: serde_json::Value = response.json().await.expect("initConnect JSON");
        match value.get("token").and_then(|t| t.as_str()) {
            Some(token) => tokens.push(token.to_owned()),
            None => {
                eprintln!("initConnect refused for client {i}: {value}");
                eprintln!("(the server must run with dev.auth_bypass = true)");
                std::process::exit(1);
            }
        }
    }
    println!("initConnect: {} tokens issued", tokens.len());

    // Phase 2: run the simulated clients on plain threads (ENet is sync).
    let stats = Arc::new(Stats::new());
    let server_addr: std::net::SocketAddr = format!("{host}:{udp_port}")
        .parse()
        .expect("resolvable server address");
    // Multiplex ~25 clients per OS thread — thread-per-client melts the
    // harness machine's scheduler around ~1000 clients.
    const CLIENTS_PER_THREAD: usize = 25;
    let mut handles = Vec::new();
    let all: Vec<(usize, String, ClientPlan)> = tokens
        .into_iter()
        .enumerate()
        .map(|(i, token)| (i, token, plan_for(i, &args)))
        .collect();
    for batch in all.chunks(CLIENTS_PER_THREAD) {
        let stats = Arc::clone(&stats);
        let batch = batch.to_vec();
        handles.push(std::thread::spawn(move || {
            client::run_batch(batch, server_addr, stats)
        }));
        // Stagger batch starts to spread the handshake storm.
        std::thread::sleep(Duration::from_millis(20));
    }

    // Baseline the gateway's handoff counter (warm process across runs).
    if let Ok(resp) = http.get(&args.metrics_url).send().await {
        if let Ok(body) = resp.text().await {
            if let Some(v) = body.lines().find_map(|l| {
                l.strip_prefix("handoffs_committed_total ")
                    .and_then(|v| v.trim().parse::<f64>().ok())
            }) {
                stats
                    .handoffs_committed_at_start
                    .store(v as u64, Ordering::Relaxed);
            }
        }
    }

    // Freeze measurement arms after the connect storm settles.
    {
        let stats = Arc::clone(&stats);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(20)).await;
            stats.measure_freeze.store(true, Ordering::Relaxed);
        });
    }

    // Phase 3: sample the server process CPU while the test runs.
    let cpu_task = tokio::spawn(report::sample_server_cpu(args.duration));
    tokio::time::sleep(args.duration).await;
    stats.stop.store(true, Ordering::Relaxed);
    let cpu = cpu_task.await.unwrap_or(None);

    for handle in handles {
        let _ = handle.join();
    }

    // Phase 4: report.
    report::print_report(
        &stats,
        args.duration,
        cpu,
        &args.metrics_url,
        &args.zone_metrics,
        args.handoffs,
        &http,
    )
    .await;
}
