---
title: "Monitoring"
description: "Prometheus, Grafana, the admin API and the profiler — what to watch and how to find a slow resource."
---

BASTON is instrumented out of the box. `metrics` is on by default; the rest is
opt-in.

## Prometheus in two minutes

Metrics are already being served:

```bash
curl -s localhost:9090/metrics | head
```

The compose file brings up Prometheus and Grafana wired to it, with dashboards
loaded:

```bash
docker compose -f deploy/docker/docker-compose.yml up -d prometheus grafana
```

- Prometheus — `http://localhost:9091`
- Grafana — `http://localhost:3001` (anonymous admin)

Configuration lives in `deploy/monitoring/`: `prometheus.yml`, `alerts.yml`, and
dashboards for overview and mesh.

**Do not expose port 9090.** It reveals resource names, player counts and
internal timings.

## What to watch

The five that matter:

| Metric | Healthy | Not healthy means |
| --- | --- | --- |
| `onesync_tick_hz` | 60 | The server is shedding tick rate to keep up |
| `onesync_tick_utilization` | < 0.85 | Sustained > 1.0 is over capacity |
| `state_sync_tick_jitter_ms` | p99 < 2 ms | The loop is not scheduled on time |
| `baston_script_dispatch_duration_seconds` | p95 single-digit ms | A resource is doing too much in a handler |
| `kvp_flush_failures_total` | **0** | **Resources are losing data** |

The last one deserves a page: it is silent, and nobody notices for hours.

Full list: [metrics reference](../reference/metrics.md).

## The admin API

Off by default — it can kick players and stop resources.

```toml
[modules]
enable = ["admin-api"]

[[api.keys]]
name = "me"
token = "…"                      # openssl rand -hex 32
permissions = ["monitor.read", "resource.control", "player.kick"]
```

```bash
TOKEN=…
curl -H "Authorization: Bearer $TOKEN" localhost:8080/api/v1/status | jq
curl -H "Authorization: Bearer $TOKEN" localhost:8080/api/v1/players | jq
curl -H "Authorization: Bearer $TOKEN" localhost:8080/api/v1/resources | jq
```

Permissions are per key: `monitor.read`, `resource.control`, `player.kick`,
`zone.drain`, `profiler.control`, `profiler.read`, `console.execute`. Give each
key the minimum it needs — `console.execute` is remote code execution by design.

Every control action, **including denied attempts**, is appended to
`api.audit_log`.

The listener refuses to start with no keys at all, rather than opening an
unauthenticated control surface.

Full reference: [monitoring and control API](../reference/api.md).

## Finding a slow resource

`resmon` gives you per-resource cost without a profiler:

```bash
curl -H "Authorization: Bearer $TOKEN" localhost:8080/api/v1/resmon | jq
```

Per resource: dispatch count, CPU total, p50/p95/p99 over the last 512
dispatches, watchdog terminations, native round trips, and — **JavaScript
only** — memory. Lua resources report no memory.

Then per handler, which is usually where the answer is:

```bash
curl -H "Authorization: Bearer $TOKEN" localhost:8080/api/v1/resmon/events | jq
```

Counts, p95, p99 and error counts keyed by event name. A handler with a high p99
and a low p50 is doing something occasionally expensive — a database call, an
HTTP request, a loop over players.

## The profiler

Chrome traces, for when resmon says *which* resource but not *why*.

```toml
[modules]
enable = ["admin-api", "profiler"]
```

```bash
curl -X POST -H "Authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' \
  -d '{"frames":2000,"scope":"server","include_native_calls":true}' \
  localhost:8080/api/v1/profiler/record

# … reproduce the problem …

curl -X POST -H "Authorization: Bearer $TOKEN" localhost:8080/api/v1/profiler/stop
curl -H "Authorization: Bearer $TOKEN" \
  localhost:8080/api/v1/profiler/latest/trace > trace.json
```

Open `trace.json` at `chrome://tracing`.

Notes:

- Capped at 4096 events; the recording **auto-stops** when full.
- `seconds` expires a recording lazily.
- **Payloads are never captured** — only names, durations, resource, kind and
  source id. A trace is safe to share.
- With the `profiler` module off, its routes return **404**, not 403. The
  capability is not running.
- `ProfilerEnterScope` / `ProfilerExitScope` are **no-ops** — script-authored
  scopes never appear in a trace.

## The in-game overlay

`displayinfo` draws a server-assembled readout in game: mesh topology, OneSync
state, per-player link statistics. Nothing to install client-side.

```toml
[modules]
enable = ["debug-overlay"]

[debug]
display_info = "allowlist"
allow = ["license:abc123…"]
refresh_hz = 5
```

`"everyone"` exposes zone topology and per-player network statistics to anyone
connected — development servers only. See [displayinfo](displayinfo.md).

## Alerting

`deploy/monitoring/alerts.yml` ships rules worth paging on. If you write your
own:

- **Alert on symptoms, not causes.** `onesync_tick_hz < 40 for 5m`, not CPU.
- **Any watchdog termination is a page.** A resource wedged a runtime.
- **`kvp_flush_failures_total` increasing is a page.** Silent data loss.
- **In multi-zone**, `handoff_rollback_failures_total` above zero means routing
  is inconsistent.

## A note on cardinality

Some metrics are labelled by resource, event name or native hash. A resource
that builds event names dynamically — embedding a player id, say — produces
unbounded label cardinality and will bloat Prometheus.

That is a resource bug, and these metrics are where you see it. BASTON caps the
event label at 128 distinct values and collapses long names, but the underlying
problem is worth fixing.

## Next

- [Metrics reference](../reference/metrics.md)
- [Monitoring and control API](../reference/api.md)
- [Troubleshooting](troubleshooting.md)
