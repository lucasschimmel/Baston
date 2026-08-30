---
title: "Quickstart"
description: "From nothing to a BASTON server you can join with a FiveM client."
---

Target: a running server on your machine, joinable from FiveM, in one sitting.

## What you need

| | Why |
| --- | --- |
| Rust, stable toolchain | BASTON has no prebuilt releases yet — you build it |
| ~10 GB free disk | the first build compiles V8 |
| A FiveM client | to connect |
| Docker *(optional)* | only for multi-zone, Prometheus and Grafana |

The first build takes a while — `deno_core` compiles V8 from scratch. Later
builds are fast. If that is unacceptable, build the [`lua` bundle](modules.md),
which has no V8 at all.

## 1. Build

```bash
git clone <your-fork> baston
cd baston
cargo build --release -p baston-gateway
```

That gives you the default `js` bundle. Check what you got:

```bash
cargo run --release -p baston-gateway --bin baston-gateway -- --modules
```

```
BASTON 0.1.0-alpha · bundle: js

  off     voice          tier 1  Mumble-compatible voice server (TLS control + UDP voice)
  on      metrics        tier 1  Prometheus exporter on the metrics port
  off     admin-api      tier 1  monitoring/control HTTP API and legacy /admin routes
  …
```

Three states, and the difference matters: **on** is running, **off** is present
but switched off, **absent** needs a different bundle.

## 2. Run it

```bash
cargo run --release -p baston-gateway --bin baston-gateway
```

With no configuration at all, BASTON finds `config/baston.toml`, loads the
sample resources from `examples/resources/`, and listens on `30120`.

```
   bundle      js
   modules on  metrics, hot-reload, scripting-js

INFO baston_gateway: BASTON online — speaking the FiveM protocol, zero FXServer C++
INFO scripting: runtime selected resource=axiom-core engine=js
INFO scripting: resource started resource=axiom-core
INFO baston_gateway: HTTP gateway listening addr=0.0.0.0:30120
```

The two lines under the banner are what you paste when you ask for help: they
say which binary is running and what it has switched on.

## 3. Join

In the FiveM client's console (F8):

```
connect localhost:30120
```

You should load in. If you do not, go to
[Troubleshooting](troubleshooting.md) — the failure is almost always one of
four things, and they are all listed there.

## 4. Make it yours

Copy the shipped config rather than editing it in place, so `git pull` never
fights you:

```bash
cp config/baston.toml config/my-server.toml
BASTON_CONFIG=config/my-server.toml cargo run --release -p baston-gateway --bin baston-gateway
```

Anything ending in `.local.toml` is git-ignored, which is where a config holding
a real licence key or admin token belongs.

The minimum worth changing:

```toml
[server]
name = "Chez les potes"
port = 30120
max_players = 16
# All clients are forced onto this GTA build, and BASTON decodes entity sync
# against it. Pick one and keep it.
enforce_game_build = "3258"

[resources]
path = "resources"          # your own directory, not examples/

[dev]
hot_reload = true           # restart a resource when its files change
auth_bypass = false         # NEVER true on a server anyone else can reach
```

Then create `resources/` and put something in it —
[Installing resources](resources.md).

## 5. What to turn on next

Everything below is off by default, on purpose: each one either opens a port or
widens what a caller can do.

```toml
[modules]
enable = ["admin-api"]      # the /api/v1 monitoring and control API
```

| Module | Gives you | Read |
| --- | --- | --- |
| `admin-api` | player list, resource control, kick, profiler over HTTP | [API](../reference/api.md) |
| `db` | SQL for your resources | [Database](../scripting/database.md) |
| `voice` | proximity voice without a third-party service | [Voice](voice.md) |
| `debug-overlay` | an in-game readout of what the server thinks | [displayinfo](displayinfo.md) |
| `profiler` | per-resource performance captures | [Monitoring](monitoring.md) |

`metrics` is already on. Point Prometheus at `localhost:9090` whenever you want
— see [Monitoring](monitoring.md).

## 6. Before anyone else can reach it

Do not skip this. [Going public](going-public.md) covers it properly, but the
short version:

- `dev.auth_bypass = false`. With it on, anyone can claim any identity.
- Set up `[license]` — a public FiveM server needs a real CFX key.
- If you enabled `admin-api`, give every key a real token and the *minimum*
  permissions it needs. `console.execute` is remote code execution by design.
- Do not expose `9090` (metrics) or `8080` (admin) to the internet.

## Where things are

```
config/     your baston.toml lives here
resources/  your resources (create it; examples/resources/ is the samples)
deploy/     Docker, Prometheus, Grafana
docs/       this documentation
```

## Next

- [Installing resources](resources.md) — the thing you actually want to do next
- [Configuration reference](../reference/configuration.md) — every setting
- [Troubleshooting](troubleshooting.md) — when it does not work
