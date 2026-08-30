# Modules, bundles and addons

BASTON ships only what you ask it to run. This page is the operator-facing
reference; the reasoning behind it is in
[ADR-002](adr/002-module-tiers.md).

Three words, three different things:

| Term | What it is | How you change it |
| --- | --- | --- |
| **module** | a capability compiled into every binary | `[modules]` in `baston.toml`, at runtime |
| **capability** | a capability whose code is selected at build time | pick a different **bundle** |
| **addon** | something outside the binary entirely | deploy it yourself |

## What is in this binary?

```bash
baston-gateway --modules
```

```
BASTON 0.1.0-alpha · bundle: js

  off     voice          tier 1  Mumble-compatible voice server (TLS control + UDP voice)
  on      metrics        tier 1  Prometheus exporter on the metrics port
  off     admin-api      tier 1  monitoring/control HTTP API and legacy /admin routes
  off     debug-overlay  tier 1  in-game displayinfo overlay
  off     profiler       tier 1  script profiler capture and its API routes
  on      hot-reload     tier 1  restart resources when their scripts change on disk
  on      scripting-js   tier 2  JavaScript resources (deno_core / V8)
  absent  scripting-lua  tier 2  Lua resources (mlua / Lua 5.4)
  absent  escrow         tier 2  CFX Asset Escrow decryption (Windows)

  absent capabilities need a different bundle:
    scripting-lua  → bundle lua (or full)
    escrow         → bundle full, on Windows
```

Three states, and the difference matters:

- **on** — running.
- **off** — in this binary, switched off. Fix it in `baston.toml`.
- **absent** — not in this binary at all. Fix it by using another bundle.

The same summary prints on every boot and is served at `/api/v1/status`. When
you report a problem, paste it: it answers the first three questions anyone
would ask.

## Modules (tier 1)

Compiled into every bundle, toggled at runtime. A module that is off costs
nothing: no thread, no listener, no allocation, no generated certificate.

| Module | Default | Section | What it does |
| --- | --- | --- | --- |
| `voice` | **on** | `[voice]` | Mumble-compatible voice server |
| `metrics` | **on** | `[metrics]` | Prometheus exporter |
| `hot-reload` | **on** | `[dev]` | restart a resource when its scripts change |
| `admin-api` | off | `[api]` | monitoring/control HTTP API, legacy `/admin/*` |
| `debug-overlay` | off | `[debug]` | in-game `displayinfo` overlay |
| `profiler` | off | — | script profiler capture and its API routes |

`admin-api`, `debug-overlay` and `profiler` default to off because each widens
what a caller can do to a running server — kick players, stop resources, read
zone topology. They open where you ask, not by default.

### Turning modules on and off

```toml
[modules]
enable = ["admin-api", "profiler"]
disable = ["voice"]
```

Deltas, not a full list: a module added in a later version does not force you
to rewrite this section.

Environment overrides win last, so a container can flip one without editing the
file it mounted:

```bash
BASTON_MODULE_ADMIN_API=true
BASTON_MODULE_VOICE=off
```

Accepted: `true/false`, `1/0`, `yes/no`, `on/off`.

### The older per-section flags still work

`[voice] enabled`, `[metrics] enabled`, `[debug] display_info`,
`[escrow] enabled` and `[dev] hot_reload` remain authoritative for their own
module. Existing configuration files keep their exact meaning.

If a flag and `[modules]` disagree, BASTON refuses to boot and names both
sites, rather than silently picking one:

```
module "voice" is configured in two places that disagree
  → [voice] enabled says true, but [modules] disable says the opposite
  → keep one of the two; [voice] enabled is the older spelling and still works
```

And if you configure a section whose module is off, it says so at boot instead
of leaving you to wonder why nothing happened:

```
WARN [voice] is configured but module "voice" is disabled — those settings are inert
```

## Bundles (tier 2)

Some capabilities change what the binary is built from, so they are chosen at
build time. You get them as prebuilt bundles — you never need a Rust toolchain.

| Bundle | Contains | For |
| --- | --- | --- |
| `lite` | no scripting runtime | zone worker, relay, benchmarking |
| `js` | JavaScript (V8) | **the default** |
| `lua` | Lua 5.4 | Lua-only servers |
| `full` | JavaScript + Lua + escrow | migrations and mixed estates |

Building from source:

```bash
cargo build --release -p baston-gateway                                              # js
cargo build --release -p baston-gateway --no-default-features                        # lite
cargo build --release -p baston-gateway --no-default-features --features scripting-lua  # lua
cargo build --release -p baston-gateway --features scripting-lua,escrow               # full
```

Other feature combinations build, but only these four are covered by CI. A
binary built outside the list reports `bundle: custom` and says so.

Asking for a capability your bundle does not contain is an error at startup,
not a surprise later:

```
module "scripting-lua" is not compiled into this build
  → it ships in bundle lua (or full)
  → run `baston-gateway --modules` to see what this binary contains
```

## Scripting runtimes

A resource declares its language through the extension of its server scripts.
There is no new manifest key, and existing resources work unchanged.

| Extension | Runtime | Bundle |
| --- | --- | --- |
| `.js`, `.mjs`, `.cjs` | deno_core / V8 | `js`, `full` |
| `.lua` | mlua / Lua 5.4 | `lua`, `full` |

A resource runs on one engine. Mixing `.js` and `.lua` in one `server_scripts`
is refused — split it into two resources that talk over events.

Both runtimes call **the same** native implementations, so a native behaves
identically whichever language invoked it. The engine layer only converts
values to and from JSON.

A resource with no server scripts (client-only, or streaming assets) does not
get a runtime at all.

### What Lua supports today

Working: `AddEventHandler`, `RegisterNetEvent`, `TriggerEvent`,
`TriggerClientEvent`, `RegisterCommand`, `Citizen.CreateThread` / `Wait` /
`SetTimeout`, `print`, `Citizen.InvokeNative`, and the natives as globals
(`GetPlayerName(1)` resolves on first use). Net events reach a resource only
where it called `RegisterNetEvent`, as on the JS side.

Not yet, and worth knowing before you migrate a server:

- **No watchdog.** The V8 path force-terminates a runaway script; the Lua path
  does not yet. A Lua script that never yields blocks its own runtime — and
  only its own, since every resource has its own thread.
- **No client-native dispatch.** Calling a *client* native from a Lua server
  resource is not wired yet. JS resources can.
- **No zone transfer state.** `RegisterZoneTransferState` is JS-only, so a Lua
  resource carries no state across a zone handoff.
- **Lua 5.4, not LuaJIT.** CFX supports both (`lua54`). The interpreter is
  swappable later without touching resource code.

## Addons (tier 3)

Everything else lives outside the binary and needs no switch. BASTON already
provides the two surfaces an addon needs:

- **the monitoring/control API** — `/api/v1/*` with per-key permissions
  (`monitor.read`, `resource.control`, `player.kick`, `zone.drain`,
  `profiler.*`, `console.execute`) and an audit log. See [api.md](api.md).
- **the NATS bus** carrying gateway/zone traffic.

An administration dashboard is an addon, not a module: it consumes the API with
a scoped key, deploys on its own cadence, and cannot take the server down when
it fails. The Prometheus/Grafana setup under `monitoring/` is the reference
example.

If something can be built on the public natives in JS or Lua, it is an addon —
write it as a resource.

## Adding a module

1. Decide the tier using the test in [ADR-002](adr/002-module-tiers.md). Most
   ideas are tier 3 and need nothing here.
2. Add a variant to `ModuleId` in `crates/baston-modules/src/lib.rs`, at the
   end, and add it to `ALL`.
3. Gate it at **one** place — where it registers or spawns. Neither
   `if config.x.enabled` nor `#[cfg(feature = "x")]` belongs in domain logic.
4. For tier 2, forward the Cargo feature to `baston-modules` as well, or
   `--modules` will under-report what the build contains.
