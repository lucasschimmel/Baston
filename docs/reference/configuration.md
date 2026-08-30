---
title: "Configuration reference"
description: "Every setting in baston.toml: what it does, what it defaults to, and when you would change it."
---

Every key BASTON reads, with its default. If a setting is not listed here, it
does not exist — BASTON refuses to start on an unknown *value*, but silently
ignores an unknown *key*, so a typo is easy to miss.

## Where the file lives

BASTON looks for its configuration in this order:

1. `$BASTON_CONFIG`, if set
2. `baston.toml` next to the working directory
3. `config/baston.toml`

The first that exists wins. A deployed server usually has (2); a checkout of
this repository has (3).

```bash
BASTON_CONFIG=/etc/baston/production.toml baston-gateway
```

The file is TOML. A section you do not write gets its defaults, so the smallest
valid configuration is:

```toml
[server]
port = 30120
```

---

## `[server]` — identity and capacity

| Key | Default | What it does |
| --- | --- | --- |
| `name` | `"BASTON Dev"` | Server name shown in the client's server list and `/info.json`. |
| `port` | `30120` | The game port. FiveM multiplexes TCP and UDP on it. |
| `bind_address` | `0.0.0.0` | Interface to listen on. Must be one concrete address if you enable public listing. |
| `max_players` | `32` | Slot count. A CFX licence can *lower* this (see `[license]`); it never raises it. |
| `enforce_game_build` | `""` | GTA build all clients must run, e.g. `"3258"`. Empty means no enforcement. |

`enforce_game_build` matters more than it looks: the client switches build
before connecting, and BASTON decodes entity sync trees against that build. Mixed
builds without OneSync produce desyncs that look like random rubber-banding.

## `[auth]` — CFX ticket verification

| Key | Default | What it does |
| --- | --- | --- |
| `pubkey_url` | `https://lambda.fivem.net/api/ticket/pubkey` | Where the RSA public key for offline ticket verification is fetched. |
| `http_timeout_secs` | `5` | Timeout for that fetch. |

You should not need to change either. BASTON verifies tickets offline against
this key — it does not call CFX per connection.

## `[resources]` — where resources live, and their limits

| Key | Default | What it does |
| --- | --- | --- |
| `path` | `"resources"` | Directory scanned for resources. |
| `kvp_path` | `"baston-kvp.json"` | File backing the resource KVP store. |
| `kvp_flush_interval_secs` | `30` | How often deferred KVP writes are flushed to disk. |
| `file_download_timeout_secs` | `30` | Per-file timeout when a client downloads a resource. |
| `file_download_chunk_bytes` | `65536` | Chunk size for those downloads. |
| `file_download_concurrency` | `64` | How many downloads may run at once, server-wide. |
| `http_request_timeout_secs` | `30` | Timeout for `PerformHttpRequest` (outbound, from a script). |
| `http_concurrency` | `32` | How many outbound script HTTP requests may be in flight. |
| `http_response_max_bytes` | `5242880` | Largest outbound response a script may receive (5 MiB). |
| `http_handler_timeout_secs` | `15` | Timeout for a resource's inbound `SetHttpHandler`. |
| `http_request_max_bytes` | `1048576` | Largest inbound request body a handler may receive (1 MiB). |

The `http_*` limits are backpressure, not security: they stop one resource from
exhausting the process. A resource that hits them gets an error, not silence.

## `[connection]` — the join flow

| Key | Default | What it does |
| --- | --- | --- |
| `deferral_timeout_secs` | `10` | How long a `playerConnecting` handler may defer before the player is dropped. |

Raise this if you run a queue. A player sitting in a deferral is holding a
connection slot but not a player slot.

## `[udp]` — the game transport

| Key | Default | What it does |
| --- | --- | --- |
| `port` | *(same as `server.port`)* | ENet port. Leave unset unless you know why. |
| `poll_interval_ms` | `5` | How often the ENet host is serviced. |

## `[modules]` — what this process runs

| Key | Default | What it does |
| --- | --- | --- |
| `enable` | `[]` | Modules to switch on, by slug. |
| `disable` | `[]` | Modules to switch off, by slug. |

Deltas over the defaults, not a full list. See
[Modules and bundles](../server/modules.md). Every module also has an
environment override, `BASTON_MODULE_<SLUG>` with `-` as `_`:

```bash
BASTON_MODULE_ADMIN_API=true
BASTON_MODULE_VOICE=off
```

## `[db]` — database access for scripts

Requires the `db` module. See [Using a database](../scripting/database.md).

| Key | Default | What it does |
| --- | --- | --- |
| `url` | `""` | `sqlite:…`, `postgres://…` or `mysql://…`. Required when the module is on. |
| `pool_size` | `10` | Connections held open. Keep under your database's own limit. |
| `query_timeout_secs` | `15` | A query over this is abandoned and reported to the calling script. |

## `[voice]` — the embedded Mumble server

| Key | Default | What it does |
| --- | --- | --- |
| `enabled` | `false` | Legacy flag; still authoritative for the `voice` module. |
| `port` | `30121` | TLS control and UDP voice share this port number. Must differ from `server.port`. |
| `external_address` | `""` | Address advertised to clients. **Empty means clients are never told where voice is.** |

`external_address` is the setting people miss: voice runs, the port is open, and
nothing connects, because the client was never given an address. Use the address
players actually reach you at — `127.0.0.1` for a local test.

## `[api]` — the monitoring and control API

Requires the `admin-api` module (off by default). See
[Monitoring and control API](api.md).

| Key | Default | What it does |
| --- | --- | --- |
| `audit_log` | `"baston-audit.jsonl"` | Where control actions are appended. |
| `keys` | `[]` | Array of `[[api.keys]]` entries. |

Each key:

```toml
[[api.keys]]
name = "discord-bot"                      # identifies the key in the audit log
token = "…"                               # at least 32 chars, no whitespace, unique
permissions = ["monitor.read"]
```

Permissions: `monitor.read`, `resource.control`, `player.kick`, `zone.drain`,
`profiler.control`, `profiler.read`, `console.execute`.

The loader refuses weak, duplicated or placeholder tokens, and keys with no
permissions or no name — an unusable key that boots is worse than a refusal.

## `[license]` — CFX server identity

See [CFX licensing](../operations/licensing.md) for what each mode really does.

| Key | Default | What it does |
| --- | --- | --- |
| `mode` | `"off"` | `off` \| `gate` \| `verified`. |
| `sv_license_key` | `""` | Your key from [portal.cfx.re](https://portal.cfx.re). |
| `fxserver_path` | *(unset)* | Path to an official `FXServer.exe`, required by `verified`. |
| `sidecar_port` | `30130` | Private localhost port for the broker. Give each instance on a host its own. |
| `public_listing` | `false` | Register in the public CFX server list. |
| `listing_ip_override` | *(unset)* | Public IP advertised to CFX. Required when listing. |

`off` warns every boot and is for LAN and development only. `gate` checks the
key's *shape*, nothing more. `verified` runs the official FXServer component to
validate against CFX and enforce the verdict.

## `[escrow]` — encrypted CFX assets

Requires a Windows build with the `escrow` capability. See
[Asset escrow](../operations/escrow.md).

| Key | Default | What it does |
| --- | --- | --- |
| `enabled` | `false` | |
| `backend` | `"sidecar"` | `sidecar` is supported. `direct` is not — svadhesive exposes no callable decrypt symbol. |
| `server_license` | `""` | Your `license:…` key. Required when enabled. |
| `fxserver_path` | *(unset)* | Path to `FXServer.exe` for the sidecar. |
| `dll_path` | *(unset)* | Only for the unsupported `direct` backend. |

## `[state_sync]` — entity synchronisation

The tuning surface. Defaults are benchmarked; change them with a metric in
front of you, not on a hunch.

### OneSync and rates

| Key | Default | What it does |
| --- | --- | --- |
| `onesync` | `"off"` | `off` \| `on` \| `infinity`. |
| `sync_interval_ms` | `16` | How often the emitter samples dirty entities (~60 Hz). |
| `push_interval_ms` | `50` | How often each client receives a snapshot (~20 Hz). |
| `aoi_radius` | `450.0` | Area-of-interest radius, in metres. |
| `max_speed_mps` | `200.0` | Speed above which a client position update is rejected as implausible. |
| `ownership_interval_secs` | `5` | How often network ownership is re-evaluated. |

`max_speed_mps` is your cheapest anti-teleport check. It is a plausibility
filter, not an anticheat.

### Adaptive tick

The server lowers its own tick rate under load instead of falling behind.

| Key | Default | What it does |
| --- | --- | --- |
| `adaptive_tick_enabled` | `true` | |
| `tick_default_hz` | `60` | Starting rate. |
| `tick_min_hz` | `20` | Floor under sustained load. |
| `tick_max_hz` | `120` | Ceiling. |
| `tick_high_utilization` | `0.85` | Above this fraction of the budget, the rate drops. |
| `tick_low_utilization` | `0.50` | Below this, it climbs back. |
| `tick_recovery_window` | `180` | Ticks of calm before climbing. |
| `tick_overload_backoff` | `0.5` | Multiplier applied on overload. |

### Interest budget

What each client is *allowed* to receive per push, and how BASTON chooses what
to spend it on.

| Key | Default | What it does |
| --- | --- | --- |
| `interest_budget_bytes` | `24576` | Per-client, per-push byte budget for additions. |
| `interest_remove_budget_bytes` | `4096` | Budget for removals. |
| `interest_distance_weight` | `10.0` | How much closeness raises an entity's score. |
| `interest_closing_weight` | `0.5` | How much *approaching* raises it. |
| `interest_staleness_weight` | `1.0` | How much time-since-last-update raises it. |
| `interest_hysteresis_m` | `20.0` | Dead band that stops entities flickering in and out at the edge. |

## `[nats]` — the message bus

| Key | Default | What it does |
| --- | --- | --- |
| `url` | `nats://127.0.0.1:4222` | |
| `zone_id` | `"zone-a"` | This process's zone identity. |

The gateway boots without NATS, with state sync disabled and a loud error. A
zone process does not: it exits.

## `[meshing]` — multi-zone federation

See [Multi-zone servers](../server/multi-zone.md) and
[Zone configuration](../server/zone-config.md).

| Key | Default | What it does |
| --- | --- | --- |
| `enabled` | `false` | Turns on federation. Changes which components run where. |
| `gateway_grpc_addr` | `0.0.0.0:50050` | Where the gateway's GatewayService listens. |
| `gateway_grpc` | `127.0.0.1:50050` | Where a zone finds the gateway. |
| `zone_grpc_addr` | `0.0.0.0:50051` | Where a zone's ZoneService listens. |
| `zone_public_grpc_addr` | *(derived)* | The address a zone registers with the gateway. |
| `zone_bounds` | *(unset)* | `x_min,y_min,x_max,y_max`. Required for a zone process. |
| `heartbeat_interval_secs` | `5` | Zone → gateway heartbeat. |
| `zone_timeout_secs` | `15` | Missed heartbeats before a zone is declared dead. |
| `boundary_margin` | `300.0` | Metres before a boundary at which a handoff is prepared. |
| `boundary_scan_interval_ms` | `500` | How often boundary proximity is evaluated. |
| `handoff_cooldown_secs` | `5` | Minimum time between handoffs for one player, so a player on a border does not ping-pong. |
| `admin_port` | `8080` | Port for the admin/API listener. |
| `admin_token` | `""` | Legacy full-permission token. Prefer `[[api.keys]]`. |

## `[metrics]` — Prometheus

| Key | Default | What it does |
| --- | --- | --- |
| `enabled` | `true` | Legacy flag; still authoritative for the `metrics` module. |
| `port` | `9090` | Where `/metrics` is served. |

See the [metrics reference](metrics.md).

## `[debug]` — the in-game overlay

Requires the `debug-overlay` module. See [displayinfo](../server/displayinfo.md).

| Key | Default | What it does |
| --- | --- | --- |
| `display_info` | `"off"` | `off` \| `allowlist` \| `everyone`. |
| `allow` | `[]` | Identifiers cleared for the overlay, as `GetPlayerIdentifiers` reports them. |
| `refresh_hz` | `5` | Snapshots per second per subscriber (1–30). |

`everyone` exposes zone topology and per-player network statistics to anyone
connected. Keep it to development servers.

## `[dev]` — development conveniences

| Key | Default | What it does |
| --- | --- | --- |
| `hot_reload` | `true` | Restart a resource when its scripts change on disk. Authoritative for the `hot-reload` module. |
| `auth_bypass` | `false` | **Do not enable on a public server.** CFX tickets are not validated; anyone can claim any identity. |

`auth_bypass` warns loudly on every boot. That warning is the feature.

## `[tls]` — deliberately absent

There is no `[tls]` section, and adding one breaks the server.

The FiveM client sends some game-port requests as plain HTTP, and a TLS-only
listener answers them with `Received HTTP/0.9 when not allowed`. Proper HTTPS
needs first-byte TLS/plain multiplexing on the game port, which is not
implemented. Packfile downloads are handed out as literal `http://` URLs for the
same reason.

## Environment variables

Overrides applied after the file is parsed, so a container can change one
setting without rewriting a mounted file.

| Variable | Overrides |
| --- | --- |
| `BASTON_CONFIG` | which file to load |
| `BASTON_PORT` | `server.port` |
| `BASTON_RESOURCES_PATH` | `resources.path` |
| `BASTON_METRICS_PORT` | `metrics.port` |
| `BASTON_VOICE_ENABLED`, `BASTON_VOICE_PORT` | `voice.*` |
| `BASTON_MESHING_ENABLED` | `meshing.enabled` |
| `BASTON_ADMIN_TOKEN` | `meshing.admin_token` |
| `BASTON_GRPC_ADDR` | `meshing.gateway_grpc_addr` |
| `GATEWAY_GRPC` | `meshing.gateway_grpc` |
| `ZONE_GRPC_ADDR` | `meshing.zone_grpc_addr` |
| `ZONE_PUBLIC_GRPC_ADDR` | `meshing.zone_public_grpc_addr` |
| `ZONE_ID` | `nats.zone_id` |
| `ZONE_BOUNDS` | `meshing.zone_bounds` |
| `NATS_URL` | `nats.url` |
| `BASTON_MODULE_<SLUG>` | any module |

Booleans accept `true/false`, `1/0`, `yes/no`, `on/off`.

## When BASTON refuses to start

The loader validates before opening any port, and every error names the fix.
The common ones:

| Message | Cause |
| --- | --- |
| `[license] mode = "…" requires a licence key` | `gate`/`verified` with an empty `sv_license_key`. |
| `[escrow] enabled = true but server_license is empty` | escrow needs the CFX key to derive decryption keys. |
| `[[api.keys]] key "…" has a weak or placeholder token` | tokens must be ≥ 32 characters. Use `openssl rand -hex 32`. |
| `voice.port (…) must differ from server.port` | the game transport owns the game port. |
| `module "…" is configured in two places that disagree` | a legacy flag and `[modules]` contradict each other. |
| `module "…" is not compiled into this build` | wrong bundle — run `--modules`. |
| `[db] the db module is enabled but url is empty` | set `[db] url`, or drop `db` from `[modules] enable`. |
| `public_listing requires mode = "verified"` | you cannot list a server whose identity is unverified. |
