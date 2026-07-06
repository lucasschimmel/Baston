//! Runtime performance collection for ResMon and bounded Chrome Trace export.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use serde::{Deserialize, Serialize};

const DEFAULT_SAMPLE_CAP: usize = 512;
const DEFAULT_TRACE_CAP: usize = 4096;
const MAX_PROM_EVENT_LABELS: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DispatchKind {
    LoadScript,
    Event,
    NetEvent,
    PlayerConnecting,
    ZoneTransferState,
    NativeRoundtrip,
    Command,
}

impl DispatchKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LoadScript => "LoadScript",
            Self::Event => "Event",
            Self::NetEvent => "NetEvent",
            Self::PlayerConnecting => "PlayerConnecting",
            Self::ZoneTransferState => "ZoneTransferState",
            Self::NativeRoundtrip => "NativeRoundtrip",
            Self::Command => "Command",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourcePerfStats {
    pub resource: String,
    pub dispatch_count: u64,
    pub dispatch_cpu_total_us: u64,
    pub dispatch_cpu_p50_us: u64,
    pub dispatch_cpu_p95_us: u64,
    pub dispatch_cpu_p99_us: u64,
    pub last_dispatch_us: u64,
    pub max_dispatch_us: u64,
    pub watchdog_terminations: u64,
    pub native_calls: u64,
    pub native_timeout_count: u64,
    pub native_p95_us: u64,
    pub memory_used_bytes: Option<u64>,
    pub memory_total_bytes: Option<u64>,
    pub memory_external_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandlerPerfStats {
    pub resource: String,
    pub kind: DispatchKind,
    pub name: String,
    pub count: u64,
    pub total_us: u64,
    pub p95_us: u64,
    pub p99_us: u64,
    pub errors: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResMonSnapshot {
    pub uptime_secs: u64,
    pub scope: String,
    pub resources: Vec<ResourcePerfStats>,
    pub handlers: Vec<HandlerPerfStats>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfilerStatus {
    pub active: bool,
    pub scope: String,
    pub recorded_events: usize,
    pub limit_events: usize,
    pub started_ms: Option<u64>,
    pub stops_at_ms: Option<u64>,
    pub latest_events: usize,
}

#[derive(Debug, Clone)]
pub struct ProfilerRecordOptions {
    pub frames: Option<usize>,
    pub seconds: Option<u64>,
    pub scope: String,
    pub include_native_calls: bool,
}

#[derive(Clone, Debug)]
pub struct DispatchMeasurement {
    pub resource: String,
    pub kind: DispatchKind,
    pub name: String,
    pub execute_us: u64,
    pub event_loop_us: u64,
    pub total_us: u64,
    pub errored: bool,
    pub watchdog_fired: bool,
    pub memory: Option<V8MemoryStats>,
    pub source: Option<u32>,
    pub zone: Option<String>,
}

#[derive(Clone, Copy, Debug)]
pub struct V8MemoryStats {
    pub used_bytes: u64,
    pub total_bytes: u64,
    pub external_bytes: u64,
}

#[derive(Default)]
struct StatBucket {
    count: u64,
    total_us: u64,
    last_us: u64,
    max_us: u64,
    errors: u64,
    samples: VecDeque<u64>,
}

impl StatBucket {
    fn record(&mut self, duration_us: u64, errored: bool, sample_cap: usize) {
        self.count += 1;
        self.total_us = self.total_us.saturating_add(duration_us);
        self.last_us = duration_us;
        self.max_us = self.max_us.max(duration_us);
        if errored {
            self.errors += 1;
        }
        if self.samples.len() == sample_cap {
            self.samples.pop_front();
        }
        self.samples.push_back(duration_us);
    }
}

#[derive(Default)]
struct ResourceStats {
    dispatch: StatBucket,
    handlers: HashMap<(DispatchKind, String), StatBucket>,
    native: StatBucket,
    native_timeouts: u64,
    watchdog_terminations: u64,
    memory: Option<V8MemoryStats>,
}

#[derive(Clone, Serialize)]
struct TraceEvent {
    name: String,
    cat: String,
    ph: &'static str,
    ts: u64,
    dur: u64,
    pid: u32,
    tid: u64,
    args: serde_json::Value,
}

#[derive(Default)]
struct ProfilerState {
    active: Option<ActiveRecording>,
    latest: Vec<TraceEvent>,
}

struct ActiveRecording {
    events: VecDeque<TraceEvent>,
    limit_events: usize,
    started: Instant,
    started_ms: u64,
    duration: Option<Duration>,
    scope: String,
    include_native_calls: bool,
}

pub struct Observability {
    started_at: Instant,
    sample_cap: usize,
    trace_cap: usize,
    resources: DashMap<String, Mutex<ResourceStats>>,
    profiler: Mutex<ProfilerState>,
    prom_events: Mutex<Vec<String>>,
    resmon_enabled: AtomicBool,
}

impl Default for Observability {
    fn default() -> Self {
        Self::new()
    }
}

impl Observability {
    pub fn new() -> Self {
        Self {
            started_at: Instant::now(),
            sample_cap: DEFAULT_SAMPLE_CAP,
            trace_cap: DEFAULT_TRACE_CAP,
            resources: DashMap::new(),
            profiler: Mutex::new(ProfilerState::default()),
            prom_events: Mutex::new(Vec::new()),
            resmon_enabled: AtomicBool::new(false),
        }
    }

    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    pub fn record_dispatch(&self, measurement: DispatchMeasurement) {
        let status = if measurement.errored { "error" } else { "ok" };
        let prom_event = self.prom_event_label(&measurement.name);
        metrics::histogram!(
            "baston_script_dispatch_duration_seconds",
            "resource" => measurement.resource.clone(),
            "kind" => measurement.kind.as_str(),
            "event" => prom_event.clone(),
        )
        .record(measurement.total_us as f64 / 1_000_000.0);
        metrics::counter!(
            "baston_script_dispatch_total",
            "resource" => measurement.resource.clone(),
            "kind" => measurement.kind.as_str(),
            "event" => prom_event,
            "status" => status,
        )
        .increment(1);
        if measurement.watchdog_fired {
            metrics::counter!(
                "baston_script_watchdog_terminations_total",
                "resource" => measurement.resource.clone(),
            )
            .increment(1);
        }

        let entry = self
            .resources
            .entry(measurement.resource.clone())
            .or_insert_with(|| Mutex::new(ResourceStats::default()));
        {
            let mut stats = entry.lock().unwrap_or_else(|e| e.into_inner());
            stats
                .dispatch
                .record(measurement.total_us, measurement.errored, self.sample_cap);
            let key = (measurement.kind, measurement.name.clone());
            stats.handlers.entry(key).or_default().record(
                measurement.total_us,
                measurement.errored,
                self.sample_cap,
            );
            if measurement.watchdog_fired {
                stats.watchdog_terminations += 1;
            }
            if let Some(memory) = measurement.memory {
                stats.memory = Some(memory);
            }
        }

        self.push_trace(TraceEvent {
            name: measurement.name,
            cat: "script".to_owned(),
            ph: "X",
            ts: now_us(),
            dur: measurement.total_us,
            pid: 1,
            tid: thread_id(),
            args: serde_json::json!({
                "resource": measurement.resource,
                "kind": measurement.kind.as_str(),
                "execute_us": measurement.execute_us,
                "event_loop_us": measurement.event_loop_us,
                "source": measurement.source,
                "zone": measurement.zone,
                "status": status,
            }),
        });
    }

    pub fn record_native_roundtrip(
        &self,
        resource: &str,
        hash: u64,
        source: u32,
        duration_us: u64,
        timed_out: bool,
        errored: bool,
    ) {
        let hash_label = format!("0x{hash:016X}");
        let status = if timed_out {
            "timeout"
        } else if errored {
            "error"
        } else {
            "ok"
        };
        metrics::histogram!(
            "baston_native_roundtrip_duration_seconds",
            "resource" => resource.to_owned(),
            "hash" => hash_label.clone(),
            "status" => status,
        )
        .record(duration_us as f64 / 1_000_000.0);
        if timed_out {
            metrics::counter!(
                "baston_native_roundtrip_timeouts_total",
                "resource" => resource.to_owned(),
                "hash" => hash_label.clone(),
            )
            .increment(1);
        }

        let entry = self
            .resources
            .entry(resource.to_owned())
            .or_insert_with(|| Mutex::new(ResourceStats::default()));
        {
            let mut stats = entry.lock().unwrap_or_else(|e| e.into_inner());
            stats
                .native
                .record(duration_us, errored || timed_out, self.sample_cap);
            if timed_out {
                stats.native_timeouts += 1;
            }
            let key = (DispatchKind::NativeRoundtrip, hash_label.clone());
            stats.handlers.entry(key).or_default().record(
                duration_us,
                errored || timed_out,
                self.sample_cap,
            );
        }

        self.push_trace(TraceEvent {
            name: hash_label,
            cat: "native".to_owned(),
            ph: "X",
            ts: now_us(),
            dur: duration_us,
            pid: 1,
            tid: thread_id(),
            args: serde_json::json!({
                "resource": resource,
                "kind": DispatchKind::NativeRoundtrip.as_str(),
                "source": source,
                "status": status,
            }),
        });
    }

    pub fn snapshot(&self) -> ResMonSnapshot {
        let mut resources = Vec::new();
        let mut handlers = Vec::new();
        for entry in self.resources.iter() {
            let resource = entry.key().clone();
            let stats = entry.value().lock().unwrap_or_else(|e| e.into_inner());
            resources.push(ResourcePerfStats {
                resource: resource.clone(),
                dispatch_count: stats.dispatch.count,
                dispatch_cpu_total_us: stats.dispatch.total_us,
                dispatch_cpu_p50_us: percentile(&stats.dispatch.samples, 50.0),
                dispatch_cpu_p95_us: percentile(&stats.dispatch.samples, 95.0),
                dispatch_cpu_p99_us: percentile(&stats.dispatch.samples, 99.0),
                last_dispatch_us: stats.dispatch.last_us,
                max_dispatch_us: stats.dispatch.max_us,
                watchdog_terminations: stats.watchdog_terminations,
                native_calls: stats.native.count,
                native_timeout_count: stats.native_timeouts,
                native_p95_us: percentile(&stats.native.samples, 95.0),
                memory_used_bytes: stats.memory.map(|m| m.used_bytes),
                memory_total_bytes: stats.memory.map(|m| m.total_bytes),
                memory_external_bytes: stats.memory.map(|m| m.external_bytes),
            });
            for ((kind, name), bucket) in &stats.handlers {
                handlers.push(HandlerPerfStats {
                    resource: resource.clone(),
                    kind: *kind,
                    name: name.clone(),
                    count: bucket.count,
                    total_us: bucket.total_us,
                    p95_us: percentile(&bucket.samples, 95.0),
                    p99_us: percentile(&bucket.samples, 99.0),
                    errors: bucket.errors,
                });
            }
        }
        resources.sort_by(|a, b| a.resource.cmp(&b.resource));
        handlers.sort_by(|a, b| {
            a.resource
                .cmp(&b.resource)
                .then_with(|| a.kind.as_str().cmp(b.kind.as_str()))
                .then_with(|| a.name.cmp(&b.name))
        });
        ResMonSnapshot {
            uptime_secs: self.started_at.elapsed().as_secs(),
            scope: "gateway".to_owned(),
            resources,
            handlers,
        }
    }

    pub fn set_resmon_enabled(&self, enabled: bool) {
        self.resmon_enabled.store(enabled, Ordering::Release);
    }

    pub fn resmon_enabled(&self) -> bool {
        self.resmon_enabled.load(Ordering::Acquire)
    }

    pub fn resource_snapshot(&self, name: &str) -> Option<ResourcePerfStats> {
        self.snapshot()
            .resources
            .into_iter()
            .find(|resource| resource.resource == name)
    }

    pub fn start_profiler(&self, options: ProfilerRecordOptions) -> ProfilerStatus {
        let limit_events = options
            .frames
            .unwrap_or(self.trace_cap)
            .max(1)
            .min(self.trace_cap);
        let duration = options.seconds.map(Duration::from_secs);
        let started_ms = now_ms();
        let mut profiler = self.profiler.lock().unwrap_or_else(|e| e.into_inner());
        profiler.active = Some(ActiveRecording {
            events: VecDeque::with_capacity(limit_events),
            limit_events,
            started: Instant::now(),
            started_ms,
            duration,
            scope: options.scope,
            include_native_calls: options.include_native_calls,
        });
        metrics::counter!("baston_profiler_recordings_total", "status" => "started").increment(1);
        metrics::gauge!("baston_profiler_active").set(1.0);
        self.status_locked(&profiler)
    }

    pub fn stop_profiler(&self) -> ProfilerStatus {
        let mut profiler = self.profiler.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(active) = profiler.active.take() {
            profiler.latest = active.events.into_iter().collect();
            metrics::counter!("baston_profiler_recordings_total", "status" => "stopped")
                .increment(1);
        }
        metrics::gauge!("baston_profiler_active").set(0.0);
        self.status_locked(&profiler)
    }

    pub fn profiler_status(&self) -> ProfilerStatus {
        let mut profiler = self.profiler.lock().unwrap_or_else(|e| e.into_inner());
        self.expire_recording(&mut profiler);
        self.status_locked(&profiler)
    }

    pub fn latest_trace_json(&self) -> serde_json::Value {
        let profiler = self.profiler.lock().unwrap_or_else(|e| e.into_inner());
        let mut events = profiler.latest.clone();
        events.sort_by_key(|event| event.ts);
        serde_json::json!({ "traceEvents": events })
    }

    fn push_trace(&self, event: TraceEvent) {
        let mut profiler = self.profiler.lock().unwrap_or_else(|e| e.into_inner());
        self.expire_recording(&mut profiler);
        let Some(active) = profiler.active.as_mut() else {
            return;
        };
        if event.cat == "native" && !active.include_native_calls {
            return;
        }
        if active.events.len() == active.limit_events {
            active.events.pop_front();
        }
        active.events.push_back(event);
        if active.events.len() >= active.limit_events {
            self.finish_recording_locked(&mut profiler);
        }
    }

    fn expire_recording(&self, profiler: &mut ProfilerState) {
        let expired = profiler
            .active
            .as_ref()
            .and_then(|active| {
                active
                    .duration
                    .map(|duration| active.started.elapsed() >= duration)
            })
            .unwrap_or(false);
        if expired {
            self.finish_recording_locked(profiler);
        }
    }

    fn finish_recording_locked(&self, profiler: &mut ProfilerState) {
        if let Some(active) = profiler.active.take() {
            profiler.latest = active.events.into_iter().collect();
            metrics::counter!("baston_profiler_recordings_total", "status" => "completed")
                .increment(1);
            metrics::gauge!("baston_profiler_active").set(0.0);
        }
    }

    fn status_locked(&self, profiler: &ProfilerState) -> ProfilerStatus {
        let (active, scope, recorded_events, limit_events, started_ms, stops_at_ms) =
            match &profiler.active {
                Some(active) => (
                    true,
                    active.scope.clone(),
                    active.events.len(),
                    active.limit_events,
                    Some(active.started_ms),
                    active
                        .duration
                        .map(|duration| active.started_ms + duration.as_millis() as u64),
                ),
                None => (false, "gateway".to_owned(), 0, self.trace_cap, None, None),
            };
        ProfilerStatus {
            active,
            scope,
            recorded_events,
            limit_events,
            started_ms,
            stops_at_ms,
            latest_events: profiler.latest.len(),
        }
    }

    fn prom_event_label(&self, event: &str) -> String {
        if event.len() > 80 {
            return "__other".to_owned();
        }
        let mut labels = self.prom_events.lock().unwrap_or_else(|e| e.into_inner());
        if labels.iter().any(|known| known == event) {
            return event.to_owned();
        }
        if labels.len() >= MAX_PROM_EVENT_LABELS {
            return "__other".to_owned();
        }
        labels.push(event.to_owned());
        event.to_owned()
    }
}

fn percentile(samples: &VecDeque<u64>, p: f64) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    let mut sorted: Vec<u64> = samples.iter().copied().collect();
    sorted.sort_unstable();
    let rank = ((p / 100.0) * (sorted.len().saturating_sub(1) as f64)).ceil() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn thread_id() -> u64 {
    let text = format!("{:?}", std::thread::current().id());
    text.trim_start_matches("ThreadId(")
        .trim_end_matches(')')
        .parse()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_aggregator_calculates_totals_and_percentiles() {
        let obs = Observability::new();
        for duration in [10, 20, 30, 40, 50] {
            obs.record_dispatch(DispatchMeasurement {
                resource: "axiom-core".into(),
                kind: DispatchKind::Event,
                name: "tick".into(),
                execute_us: duration,
                event_loop_us: 0,
                total_us: duration,
                errored: duration == 50,
                watchdog_fired: false,
                memory: None,
                source: None,
                zone: None,
            });
        }

        let snapshot = obs.snapshot();
        let resource = &snapshot.resources[0];
        assert_eq!(resource.dispatch_count, 5);
        assert_eq!(resource.dispatch_cpu_total_us, 150);
        assert_eq!(resource.dispatch_cpu_p50_us, 30);
        assert_eq!(resource.dispatch_cpu_p95_us, 50);
        assert_eq!(snapshot.handlers[0].errors, 1);
    }

    #[test]
    fn profiler_ring_buffer_is_bounded_and_exports_valid_trace() {
        let obs = Observability::new();
        obs.start_profiler(ProfilerRecordOptions {
            frames: Some(2),
            seconds: None,
            scope: "server".into(),
            include_native_calls: true,
        });
        for name in ["a", "b", "c"] {
            obs.record_dispatch(DispatchMeasurement {
                resource: "r".into(),
                kind: DispatchKind::Event,
                name: name.into(),
                execute_us: 1,
                event_loop_us: 0,
                total_us: 1,
                errored: false,
                watchdog_fired: false,
                memory: None,
                source: None,
                zone: None,
            });
        }
        obs.stop_profiler();

        let trace = obs.latest_trace_json();
        let events = trace["traceEvents"].as_array().expect("trace events");
        assert_eq!(events.len(), 2);
        serde_json::to_string(&trace).expect("trace is valid JSON");
    }

    #[test]
    fn trace_does_not_include_payloads_or_sensitive_identifiers() {
        let obs = Observability::new();
        obs.start_profiler(ProfilerRecordOptions {
            frames: Some(8),
            seconds: None,
            scope: "server".into(),
            include_native_calls: true,
        });
        obs.record_dispatch(DispatchMeasurement {
            resource: "auth".into(),
            kind: DispatchKind::NetEvent,
            name: "login".into(),
            execute_us: 1,
            event_loop_us: 0,
            total_us: 1,
            errored: false,
            watchdog_fired: false,
            memory: None,
            source: Some(7),
            zone: None,
        });
        obs.stop_profiler();

        let serialized = serde_json::to_string(&obs.latest_trace_json()).unwrap();
        assert!(!serialized.contains("Bearer"));
        assert!(!serialized.contains("license:"));
        assert!(!serialized.contains("127.0.0.1"));
    }
}
