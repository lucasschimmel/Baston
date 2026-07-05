# BASTON — Operations (Phase D zone mesh)

## Topology

```
FiveM clients ──UDP/HTTP──▶ gateway (30120)
                              │ gRPC 50050 (registry, handoff control)
                              │ NATS 4222 (state sync, ingest, events)
                    ┌─────────┴─────────┐
                 zone-a               zone-b        ... zone-N
              (50051, scripts)     (50051, scripts)
```

The gateway is the only FiveM-facing process. Zones are internal: they run
the script runtimes, entity state and boundary detection, and speak gRPC
(control) + NATS (data) only.

## Adding a zone

1. Pick bounds that tile the map without overlap (see `zone-config.md`).
2. Add a service to `docker-compose.yml` (copy `zone-b`), setting:
   - `ZONE_ID`: unique id (also the Docker DNS name),
   - `ZONE_BOUNDS`: `x_min,y_min,x_max,y_max`,
   - `ZONE_PUBLIC_GRPC_ADDR`: `<service-name>:50051`,
   - `GATEWAY_GRPC`, `NATS_URL` as the others.
3. `docker compose up -d <service>` — the zone registers itself; check
   `GET /admin/zones`.

Registration is idempotent and retried (2s × 30); a zone can start before
the gateway.

## Monitoring & control API

The admin port also serves `/api/v1/*`: read-only monitoring (status,
players, zones, resources) and audited control routes (kick, resource
start/stop/restart, drain) with per-key permissions declared in
`[[api.keys]]`. See [api.md](api.md).

## Draining a zone

```bash
curl -X POST -H "Authorization: Bearer $BASTON_ADMIN_TOKEN" \
  http://localhost:8080/admin/zones/zone-a/drain
```

All players routed to `zone-a` are rerouted to the least-loaded surviving
zone. Then stop the container. The zone stays registered until stopped —
new connections stop landing there because its player count only grows on
state, but remove it promptly.

## Zone crash

Detection: 3 missed heartbeats (15s) → the gateway evicts the zone from the
registry/quadtree, increments `zone_failures_total{zone}`, and reroutes every
orphaned player to the least-loaded surviving zone (kick with "Server zone
unavailable" if none). Recovery target: < 5s after eviction.

To restore: fix the zone, restart the container — it re-registers itself.
Players are NOT moved back automatically; new connections rebalance.

## Monitoring

### Three ports — don't conflate them

| Port | What | Notes |
|---|---|---|
| `9090` | `/metrics` of a BASTON **process** | The gateway **and every zone** each expose their own Prometheus endpoint on 9090 (inside their container, or on the host in dev). This is where the numbers come from. |
| `9091` | **Prometheus** UI/API | Host port only. The Prometheus container listens on 9090 internally — the mapping is `9091:9090`. Browse `http://localhost:9091` (`/targets`, `/rules`, `/alerts`). |
| `3001` | **Grafana** | Host port (`3001:3000`). Anonymous admin is enabled for dev. |

Prometheus scrapes gateway + zones every 5s (`monitoring/prometheus.yml`).

### Dashboards (Grafana folder "BASTON", auto-provisioned)

- **BASTON — Zone Mesh** (`baston-mesh`) — the cross-zone data plane. Handoffs/s,
  routing-lock hold time, handoff latency p99, prepare failures/timeouts, zone
  failures, state-sync jitter, zone-recovery reroutes, mesh forwarding
  drops/failures, hold-buffer depth, zone-side handoff errors
  (prepare/confirm/activate), entity handoffs, heartbeat failures, state-update
  accept/reject by reason, AoI entities per client, dirty entities per tick,
  NATS publish latency/throughput. Use it to **diagnose meshing**.
- **BASTON — Server Overview** (`baston-overview`) — whole-server health at a
  glance. Players online, aggregated world-state entities, resource scripts
  loaded/errors, escrow decrypt duration p95/p99, admin-API audit (rate + totals
  by action/outcome), UDP dropped commands, snapshot bandwidth. Use it as the
  first-glance **"is the server healthy"** board.

### Alerts (`monitoring/alerts.yml`)

Loaded by Prometheus via `rule_files`; visible at `http://localhost:9091/rules`
and `/alerts`. There is **no Alertmanager** in the dev compose, so alerts do not
page anywhere yet — you watch them in the Prometheus UI. Rules (all `warning`
severity; thresholds and their rationale are commented in the file):
`ZoneDown`, `ZoneEvicted`, `ZoneHeartbeatFailing`, `ApiAuthFailureSpike`,
`HandoffPrepareFailures`, `ResourceLoadErrors`.

**Reading a triggered alert.** Each alert's `description` already names what to
check; in general:

- **Zone\* / Handoff\* alerts** → `GET /api/v1/status` and `/api/v1/zones` to see
  which zones the gateway still holds; `docker compose logs <zone>` /
  `docker compose logs gateway` (structured `tracing`); the mesh-dashboard panel
  named in the alert.
- **ApiAuthFailureSpike** → the audit log (`api.audit_log` JSONL — one record per
  attempt with key/action/outcome) to find the offending key; the "API audit"
  panels on the overview dashboard; rotate the key in `[[api.keys]]` if the
  source is unexpected. The integration tests `admin_api_tests` / `api_v1_tests`
  document the exact 401/403 behaviour if you need to reproduce.
- **ResourceLoadErrors** → `GET /api/v1/resources`; zone logs filtered on
  `target=resources`.

Watch informally too: `handoff latency p99 > 500ms`, `zone_failures_total`
climbing, `handoff_prepare_timeouts_total` climbing, `NATS > 500 MB/s`.

## Key subjects / ports

| Channel | Purpose |
|---|---|
| `baston.zone.{id}.state` | zone → gateway entity state (JetStream) |
| `baston.zone.{id}.ingest` | gateway → zone client state updates |
| `baston.zone.{id}.outbound` | zone → gateway client events |
| `baston.handoff.entity.{id}` | ownerless entity migration |
| `baston.cross-zone.event.broadcast` | cross-zone script events |
| `baston.mesh.players` | gateway → zones global player list (2s) |
| `baston.mesh.resolve_zone` | request/reply coord → zone id |
| 30120 TCP+UDP | FiveM clients (gateway only) |
| 50050 / 50051 | GatewayService / ZoneService gRPC |
| 8080 | admin API (`Authorization: Bearer <token>`) |
| 9090 | Prometheus metrics (every process) |

## Benchmark (Phase D exit criterion)

```bash
docker compose up -d nats prometheus grafana
# host processes (Windows dev): gateway + two zones
$env:BASTON_MESHING_ENABLED="true"; cargo run --release --bin baston-gateway
$env:ZONE_ID="zone-a"; $env:ZONE_BOUNDS="-4000,-4000,0,4000"; $env:BASTON_METRICS_PORT="9092"; cargo run --release --bin baston-zone
$env:ZONE_ID="zone-b"; $env:ZONE_BOUNDS="0,-4000,4000,4000"; $env:BASTON_METRICS_PORT="9093"; cargo run --release --bin baston-zone

cargo run --release --bin baston-loadtest -- --zones 2 --clients-per-zone 1000 \
  --handoffs true --duration 300s \
  --zone-metrics http://127.0.0.1:9092/metrics,http://127.0.0.1:9093/metrics
```

Targets: handoff success > 99.9%, handoff p99 < 100ms, client freeze 0ms,
gateway CPU < 50%, NATS < 100 MB/s, 0 zone failures.
