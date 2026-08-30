---
title: "Troubleshooting"
description: "Symptoms, what causes them, and how to tell the difference — starting with what to collect before you ask."
---

## Before anything else

Two commands answer most questions before you ask them.

```bash
baston-gateway --modules
```

Tells you which binary is running and what it has switched on. **Paste this
whenever you ask for help.** Three states, three different problems:

- **on** — running
- **off** — in this binary, switched off → fix your config
- **absent** — not in this binary → you need another bundle

```bash
curl -s localhost:9090/metrics | grep script_native_unimplemented_total
```

Tells you which natives your resources asked for and did not get. When "a
resource behaves oddly", this is usually the whole answer.

---

## The server will not start

BASTON validates configuration before opening any port, and every error names
the fix. Read the message — it is more specific than this page.

| Message | Fix |
| --- | --- |
| `[license] mode = "…" requires a licence key` | Set `sv_license_key`, or `mode = "off"` for a local server. |
| `[[api.keys]] key "…" has a weak or placeholder token` | Tokens must be ≥ 32 chars: `openssl rand -hex 32`. |
| `voice.port (…) must differ from server.port` | The game transport owns the game port. Use another. |
| `module "…" is configured in two places that disagree` | A legacy flag and `[modules]` contradict each other. Keep one. |
| `module "…" is not compiled into this build` | Wrong bundle. Run `--modules`. |
| `[db] the db module is enabled but url is empty` | Set `[db] url`, or remove `db` from `[modules] enable`. |
| `unknown variant \`verified\`` | `[license] mode = "verified"` no longer exists. Use `"gate"` or `"off"`. |
| `ZONE_BOUNDS … is required` | A zone process needs bounds. See [zone config](zone-config.md). |

### "It cannot find my config"

BASTON looks for `$BASTON_CONFIG`, then `baston.toml`, then
`config/baston.toml`, relative to the **working directory**. Running the binary
from elsewhere is the usual cause.

```bash
BASTON_CONFIG=/absolute/path/to/baston.toml baston-gateway
```

### The zone process exits immediately

A zone **cannot start without NATS** — unlike the gateway, which continues with
state sync disabled. Start NATS first.

---

## I cannot connect

In rough order of likelihood.

### 1. Wrong game build

If `enforce_game_build` does not match what your client runs, the client
switches build before connecting — and if it cannot, the join fails or the world
desyncs oddly. Pick one build and keep it.

### 2. The port is not reachable

FiveM needs **both TCP and UDP** on the game port. A firewall rule that opens
only TCP produces a connection that starts and then stalls.

```bash
# Should answer even before a client connects
curl -s localhost:30120/info.json | jq
```

If that works locally but not remotely, it is a firewall or NAT issue, not
BASTON.

### 3. Stuck on "Loading" or a deferral

A `playerConnecting` handler deferred and never called `done()`. The connection
is dropped after `connection.deferral_timeout_secs` (default 10 s).

Check your whitelist logic — particularly any path that returns early without
`done()`. A handler that *throws* after deferring is handled for you: the
connection is released with a server error rather than hanging.

Note `deferrals.update()` is **logged, not shown to the player**, so the loading
screen looks frozen even when your handler is working.

### 4. Rejected outright

The error text comes from the CFX ticket check. `dev.auth_bypass = true` skips
it entirely — useful to isolate whether authentication is the problem, and
**never acceptable on a server anyone else can reach**.

---

## Resources are not starting

| Log line | Cause |
| --- | --- |
| `skipping resource with invalid manifest` | Missing or malformed `manifest.json`. |
| `server script "…" has no runtime` | An extension BASTON does not run. |
| `server_scripts mixes js and lua` | Split it into two resources. |
| `this build has no … runtime` | Wrong bundle. |
| *(nothing at all)* | The directory is not directly under `resources.path`. |

**Discovery is one level deep.** FXServer's `[category]` folder nesting does not
work — put every resource directly under `resources/`.

Also check that `resources.path` points where you think. The shipped
`config/baston.toml` points at `examples/resources`, not your own directory.

---

## A resource starts but misbehaves

### It calls something that does not exist

```bash
curl -s localhost:9090/metrics | grep script_native_unimplemented_total
```

A missing native returns a neutral value — `0`, `false`, `""` — and never
throws. That is why the symptom is "subtly wrong" rather than "crashed".

### It relies on something that exists but does nothing

These are the silent ones. See
[Coming from FXServer](../scripting/from-fivem.md#step-3-the-silent-failures):

- State bags **never reach clients** (`replicated` is ignored)
- `CancelEvent()` does nothing
- ACE permissions **always deny**
- Cross-resource `exports` throw
- Commands are not reachable from chat
- `playerDropped` gives no `source`

### It is JavaScript and has a timer

`setInterval` **never fires**; `setTimeout` ignores its delay. A JS resource
cannot run a periodic loop — that work must be Lua. See
[choosing a language](../scripting/index.md#choosing-a-language).

### It is Lua and cannot look players up

`GetPlayerName`, `GetPlayers`, `GetConvar`, `GetResourceState`,
`PerformHttpRequest` and friends are **JavaScript-only**. In Lua they return
neutral values.

### It was terminated

```bash
curl -s localhost:9090/metrics | grep watchdog_terminations
```

A dispatch exceeding 10 seconds is force-terminated and the runtime survives.
**Any non-zero value is a bug in a resource** — usually an unbounded loop, or a
Lua thread that never yields.

### Hot reload is not reloading

It watches **`.js` only**. Lua, `.mjs`, `.cjs` and `manifest.json` changes need
a manual restart:

```bash
curl -X POST -H "Authorization: Bearer $TOKEN" \
  localhost:8080/api/v1/resources/my-resource/restart
```

---

## The server is slow or players rubber-band

Look at these four, in order:

```bash
curl -s localhost:9090/metrics | grep -E 'onesync_tick_(hz|utilization)|state_sync_tick_jitter'
```

| Metric | Meaning |
| --- | --- |
| `onesync_tick_hz` below 60 | The adaptive tick is shedding rate. You are over capacity. |
| `onesync_tick_utilization` above 0.85 | Sustained, this is why. |
| `state_sync_tick_jitter_ms` p99 above 2 ms | The loop is not being scheduled on time — CPU contention, or timer resolution on Windows. |
| `baston_script_dispatch_duration_seconds` | One resource dominating. The `resource` label names it. |

Then narrow it down per resource:

```bash
curl -H "Authorization: Bearer $TOKEN" localhost:8080/api/v1/resmon | jq
```

If one resource dominates, capture a profile:

```bash
curl -X POST -H "Authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' \
  -d '{"frames":2000,"scope":"server"}' \
  localhost:8080/api/v1/profiler/record

curl -H "Authorization: Bearer $TOKEN" \
  localhost:8080/api/v1/profiler/latest/trace > trace.json
```

Open `trace.json` in Chrome's `chrome://tracing`. Requires the `profiler`
module.

### Players report seeing nothing at a distance

Expected: entities beyond `aoi_radius` (450 m) are not sent. If it happens close
up, the per-client interest budget is saturating — raise
`interest_budget_bytes`, and watch the bandwidth it costs.

### It only happens near a zone border

**Cross-zone area of interest is not implemented.** A player near a boundary
does not receive entities from the neighbouring zone. This is a known
limitation, not a misconfiguration. See [multi-zone](multi-zone.md).

---

## Voice does not work

Almost always one thing: **`voice.external_address` is empty**, so clients are
never told where the voice server is. It must be the address players actually
reach you at, not `0.0.0.0`.

```toml
[voice]
enabled = true
port = 30121
external_address = "203.0.113.10"   # or 127.0.0.1 for a local test
```

The port must also be open for **both TCP and UDP**.

If voice connects but distance does nothing: **proximity culling is not
implemented**. Everyone in a channel hears everyone. Channels and muting work.

Voice emits no metrics — the logs are your only signal.

---

## Data is being lost

```bash
curl -s localhost:9090/metrics | grep kvp_flush_failures_total
```

**Non-zero means resources are losing KVP data.** Usually a permissions or disk
problem at `resources.kvp_path`. Failed flushes are retried, so fixing the cause
recovers pending writes.

Also worth watching:

- `state_bag_changes_dropped_total` — handlers are missing updates
- `udp_plane_dropped_total{reliable="true"}` — something a client needed was
  thrown away
- `state_batches_lost_total` — the gateway missed state from a zone; in
  multi-zone, [check the JetStream sizing trap](multi-zone.md#a-jetstream-trap-worth-knowing)

---

## Multi-zone problems

| Symptom | Look at |
| --- | --- |
| Players teleport or reset on a border | Handoff failing — `handoff_prepare_timeouts_total{zone}` |
| Handoffs never complete | `handoffs_committed_total` vs `entity_handoffs_total` |
| A player ping-pongs between zones | Raise `handoff_cooldown_secs` |
| Zones falsely declared dead | Raise `zone_timeout_secs` (keep ≈ 3 heartbeats) |
| Players lose everything after a zone restart | **Expected** — zone failure and drain preserve routing only |
| `handoff_rollback_failures_total` above zero | **Routing is inconsistent.** Investigate now. |

---

## Getting help

Include:

1. The output of `--modules`.
2. The boot banner lines (`bundle`, `modules on`, `modules off`).
3. The relevant log lines — BASTON's errors name the fix, so the message itself
   usually identifies the problem.
4. `script_native_unimplemented_total`, if a resource is involved.
5. Your `baston.toml` **with `sv_license_key` and API tokens removed**.

## Next

- [Monitoring](monitoring.md)
- [Coming from FXServer](../scripting/from-fivem.md)
- [Configuration reference](../reference/configuration.md)
