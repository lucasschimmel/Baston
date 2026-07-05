# Monitoring & control API (`/api/v1`)

BASTON exposes an HTTP API on the admin port (`meshing.admin_port`, default
8080) for external tooling: dashboards, Discord/Telegram bots, a future
TXAdmin-style panel. It is served next to the legacy `/admin/*` routes.

## Keys and permissions

Access is per-key. Each key is a named bearer token declared in `baston.toml`
with an explicit permission list:

```toml
[api]
# Where control actions are audited (JSONL, append-only).
audit_log = "baston-audit.jsonl"

[[api.keys]]
name = "discord-bot"                                  # shows up in the audit log
token = "d0c48c8c62f5c3f6a1e9b2..."                   # openssl rand -hex 32
permissions = ["monitor.read"]

[[api.keys]]
name = "panel"
token = "e7a91b0f3d5c8e2a4f6b1d..."
permissions = ["monitor.read", "resource.control", "player.kick", "zone.drain"]
```

| Permission | Grants |
|---|---|
| `monitor.read` | All `GET /api/v1/*` routes |
| `resource.control` | `POST /api/v1/resources/{name}/{start\|stop\|restart}` |
| `player.kick` | `POST /api/v1/players/{source}/kick` |
| `zone.drain` | `POST /api/v1/zones/{id}/drain` |

Rules enforced at boot (`ApiConfig::validate`): unique names, unique tokens,
tokens ≥ 32 chars without whitespace, at least one permission per key.

The legacy `meshing.admin_token` (env `BASTON_ADMIN_TOKEN`) keeps working as an
implicit full-permission key named `admin`. With no keys and no admin token,
the listener does not start (fail-closed). Token comparison is constant-time.

## Monitoring routes (read-only, `monitor.read`)

```
GET /api/v1/status               → { name, version, uptime_secs, players, max_players, zones }
GET /api/v1/players              → [ { source, name, identifiers, zone } ]
GET /api/v1/zones                → [ { id, bounds, players, entities, max_players, heartbeat_age_ms, status } ]
GET /api/v1/zones/{id}           → zone detail (404 when unknown / meshing off)
GET /api/v1/resources            → [ { name, state, zone } ]   # zone = "gateway" or zone id
```

`zone` fields are `null`/empty in single-process mode. Prometheus metrics stay
on their own port (`metrics.port`, default 9090) — Grafana provisioning is in
`monitoring/`.

```bash
curl -s -H "Authorization: Bearer $TOKEN" http://localhost:8080/api/v1/status | jq
```

## Control routes (audited)

```
POST /api/v1/players/{source}/kick          # body optional: {"reason": "..."}   perm player.kick
POST /api/v1/resources/{name}/start         # perm resource.control
POST /api/v1/resources/{name}/stop
POST /api/v1/resources/{name}/restart
POST /api/v1/zones/{id}/drain               # perm zone.drain
```

- **Kick** drops the ENet peer; the normal disconnect path fires
  `playerDropped` and purges the player directory. The reason is recorded in
  the audit log (the stock client shows a generic disconnect).
- **Resource control** applies to the gateway's ResourceManager and, when
  meshing is enabled, is relayed via gRPC (`ZoneService.ControlResource`) to
  every registered zone. The response carries per-zone outcomes:
  `{ "resource": "...", "ok": true, "zones": { "zone-a": "ok" } }`.
- Denied attempts (401 unknown token / 403 missing permission) on control
  routes are audited too.

## Audit log

Append-only JSONL at `api.audit_log`, one record per control action:

```json
{"ts_ms":1751742000000,"key":"panel","action":"player.kick","target":"source:7 reason:spam","outcome":"ok"}
```

`outcome` is `ok`, `denied`, `not_found`, or an error summary. Writes happen
on a dedicated task — the request path never blocks on disk. The counter
`baston_api_audit_total{action,outcome}` tracks the same events in Prometheus.

## Legacy routes

`/admin/zones`, `/admin/zones/{id}`, `/admin/players`,
`/admin/zones/{id}/drain` (meshing only) and the game-port
`/admin/player/{source}/drop` keep working with the admin token, unchanged.
