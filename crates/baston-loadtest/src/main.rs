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
}

fn parse_args() -> Args {
    let mut args = Args {
        clients: 100,
        duration: Duration::from_secs(60),
        server: "127.0.0.1:30120".to_owned(),
        metrics_url: "http://127.0.0.1:9090/metrics".to_owned(),
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
            other => {
                eprintln!("unknown flag: {other}");
                std::process::exit(2);
            }
        }
    }
    args
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
        let body = format!(
            "method=initConnect&name=load-{i}&protocol=12&gameName=gta5&guid={i}"
        );
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
    let mut handles = Vec::new();
    for (i, token) in tokens.into_iter().enumerate() {
        let stats = Arc::clone(&stats);
        handles.push(std::thread::spawn(move || {
            client::run_client(i, token, server_addr, stats)
        }));
        // Stagger connects: 100 simultaneous ENet handshakes is not the
        // scenario under test.
        std::thread::sleep(Duration::from_millis(5));
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
    report::print_report(&stats, args.duration, cpu, &args.metrics_url, &http).await;
}
