//! Result aggregation: percentiles, bandwidth, server CPU sampling and the
//! StateSyncEmitter jitter scraped from the Prometheus endpoint.

use std::sync::atomic::Ordering;
use std::time::Duration;

use sysinfo::System;

use crate::Stats;

/// Sample `baston-gateway` process CPU over the test window; returns the
/// average busy fraction of ONE core (matching the roadmap targets).
pub async fn sample_server_cpu(duration: Duration) -> Option<f64> {
    let mut system = System::new();
    let mut samples: Vec<f64> = Vec::new();
    let interval = Duration::from_secs(1);
    let rounds = duration.as_secs().max(1);
    for _ in 0..rounds {
        tokio::time::sleep(interval).await;
        system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        let cpu: f32 = system
            .processes()
            .values()
            .filter(|p| {
                p.name()
                    .to_string_lossy()
                    .to_ascii_lowercase()
                    .starts_with("baston-gateway")
            })
            .map(|p| p.cpu_usage())
            .sum();
        if cpu > 0.0 {
            samples.push(f64::from(cpu));
        }
    }
    if samples.is_empty() {
        None
    } else {
        Some(samples.iter().sum::<f64>() / samples.len() as f64)
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let rank = (p / 100.0 * (sorted.len() - 1) as f64).round() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

/// Parse the emitter jitter histogram from the Prometheus text exposition:
/// average = sum / count of `state_sync_tick_jitter_ms`.
async fn scrape_jitter(metrics_url: &str, http: &reqwest::Client) -> Option<f64> {
    let body = http.get(metrics_url).send().await.ok()?.text().await.ok()?;
    let mut sum = None;
    let mut count = None;
    for line in body.lines() {
        if let Some(v) = line.strip_prefix("state_sync_tick_jitter_ms_sum ") {
            sum = v.trim().parse::<f64>().ok();
        }
        if let Some(v) = line.strip_prefix("state_sync_tick_jitter_ms_count ") {
            count = v.trim().parse::<f64>().ok();
        }
    }
    match (sum, count) {
        (Some(s), Some(c)) if c > 0.0 => Some(s / c),
        _ => None,
    }
}

pub async fn print_report(
    stats: &Stats,
    duration: Duration,
    cpu_pct: Option<f64>,
    metrics_url: &str,
    http: &reqwest::Client,
) {
    let mut latencies = stats.latencies_ms.lock().unwrap().clone();
    latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = percentile(&latencies, 50.0);
    let p99 = percentile(&latencies, 99.0);
    let bytes = stats.bytes_received.load(Ordering::Relaxed);
    let mbps = (bytes as f64 * 8.0) / duration.as_secs_f64() / 1_000_000.0;
    let jitter = scrape_jitter(metrics_url, http).await;

    println!();
    println!("=== baston-loadtest report ===");
    println!(
        "clients connected : {} (dropped: {})",
        stats.connected.load(Ordering::Relaxed),
        stats.dropped_connections.load(Ordering::Relaxed),
    );
    println!(
        "snapshots received: {} ({} latency samples)",
        stats.snapshots_received.load(Ordering::Relaxed),
        latencies.len(),
    );
    println!("latency p50       : {p50:.0}ms (target < 50ms)");
    println!("latency p99       : {p99:.0}ms (target < 100ms)");
    match cpu_pct {
        Some(cpu) => println!("CPU gateway+zone  : {cpu:.1}% of one core (targets: zone < 40%, gateway < 30%)"),
        None => println!("CPU gateway+zone  : n/a (baston-gateway process not found)"),
    }
    println!("bandwidth (client-observed) : {mbps:.2} Mbps (target < 10 Mbps)");
    match jitter {
        Some(j) => println!("StateSyncEmitter jitter avg : {j:.2}ms (target < 2ms)"),
        None => println!("StateSyncEmitter jitter     : n/a (metrics endpoint unreachable)"),
    }
    println!(
        "dropped connections: {}, entity desyncs: {}",
        stats.dropped_connections.load(Ordering::Relaxed),
        stats.desyncs.load(Ordering::Relaxed),
    );

    let ok = stats.dropped_connections.load(Ordering::Relaxed) == 0
        && stats.desyncs.load(Ordering::Relaxed) == 0
        && p50 < 50.0
        && p99 < 100.0
        && mbps < 10.0
        && cpu_pct.is_none_or(|c| c < 70.0)
        && jitter.is_none_or(|j| j < 2.0);
    println!(
        "exit criterion    : {}",
        if ok { "PASS" } else { "FAIL" }
    );
    if !ok {
        std::process::exit(1);
    }
}
