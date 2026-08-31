---
title: "The crates"
description: "What each crate owns, what it depends on, and where its interesting code lives."
---

Eleven crates. The dependency direction is strict: `baston-protocol` and
`baston-modules` sit at the bottom and depend on almost nothing; the binaries
sit at the top.

```
baston-gateway ──┬─▶ baston-zone ──┬─▶ baston-scripting ──▶ baston-protocol
                 ├─▶ baston-voice  ├─▶ baston-core
                 └─▶ baston-db     └─▶ baston-config ──▶ baston-modules
```

`baston-db` is optional (the `db` feature). `baston-loadtest` is a standalone
benchmark client: it links `baston-protocol` and nothing else in the tree.

| Crate | Lines | Owns |
| --- | --- | --- |
| [`baston-gateway`](#baston-gateway) | ~10 400 | The FiveM-facing process |
| [`baston-scripting`](#baston-scripting) | ~11 000 | Script host, natives, both runtimes |
| [`baston-protocol`](#baston-protocol) | ~8 500 | The wire protocol |
| [`baston-zone`](#baston-zone) | ~7 000 | Entities, state sync, resource loading |
| [`baston-voice`](#baston-voice) | ~3 000 | Mumble-compatible voice |
| [`baston-config`](#baston-config) | ~2 300 | `baston.toml` and its validation |
| [`baston-loadtest`](#baston-loadtest) | ~900 | The benchmark client |
| [`baston-db`](#baston-db) | ~750 | Pooled SQL for scripts |
| [`baston-modules`](#baston-modules) | ~500 | The module registry |
| [`baston-cfx`](#baston-cfx) | ~700 | CFX identity, entitlements, server list |
| [`baston-core`](#baston-core) | ~160 | The script-decryptor seam |

---

## `baston-gateway`

The only process a FiveM client talks to. Also the binary you run for a
single-process server.

| Module | Owns |
| --- | --- |
| `http/` | `/info.json`, `/client`, `/files`, resource endpoints, packfile and stream caches |
| `udp/` | ENet loop, ingress, OneSync outbound, adaptive tick, debug feed |
| `api/` | `/api/v1` monitoring and control, the keyring, the audit log |
| `auth/` | Offline CFX ticket verification |
| `mesh.rs`, `zone_registry.rs`, `connection_router.rs` | Zone federation and handoffs |
| `mesh_forward.rs` | Client updates → the owning zone, with the handoff hold |
| `state_aggregator.rs` | NATS → per-client snapshots |
| `cfx.rs` | Licence bootstrap, ordered so listeners open after authentication |
| `db.rs`, `voice.rs` | Adapters implementing scripting traits over real services |

The adapters are worth noting as a pattern: `baston-scripting` defines
`DbAccess` and `VoiceControl` traits, and the gateway implements them over the
real pool and the real voice handle. That is what keeps `baston-scripting` free
of sqlx and of the voice crate.

## `baston-scripting`

The largest crate, and the one with the most interesting internal boundary.

| Module | Owns |
| --- | --- |
| `natives/` | **Engine-neutral** CFX native implementations |
| `native_state.rs` | The type-map every native reads from |
| `extensions/` | The V8 bridge (deno ops) — `js` feature |
| `lua.rs` | The mlua bridge — `lua` feature |
| `runtime.rs` | One V8 isolate per resource, plus the watchdog |
| `host.rs` | Orchestrates every resource runtime; one thread each |
| `engine.rs` | Picks the runtime from script extensions |
| `observability.rs` | resmon, the profiler, dispatch metrics |
| `kvp.rs`, `state_bag.rs`, `http_bridge.rs`, `http_handler.rs` | Resource-scoped services |
| `assets/bootstrap.js`, `assets/prelude.lua` | The script-side halves |

**The rule:** logic belongs in `natives/`. Anything in `extensions/` or `lua.rs`
serves one engine only.

## `baston-protocol`

The reverse-engineered wire format. Almost dependency-free and heavily
unit-tested, because everything here is a fact about someone else's software.

| Module | Owns |
| --- | --- |
| `udp/` | Message framing, `hash_rage_string`, handshake, time sync, object ids |
| `connection.rs` | `initConnect` and `getConfiguration` shapes |
| `events.rs` | Net-event framing, JSON ↔ msgpack |
| `rage/` | The OneSync clone stream |
| `rage/buffer.rs` | MSB-first bit buffer, C++ quirks preserved |
| `rage/sync_trees.rs` | 13 sync trees, preorder — the order *is* the format |
| `rage/sync_parse.rs` | Traversal, `shouldRead`, build gates |
| `rage/lz4dict.rs` | The 64 KiB inbound dictionary |
| `native.rs` | The native registry and the client round-trip protocol |

Read [The wire protocol](../internals/protocol.md) before changing anything
here. The fuzz targets in `fuzz/` cover the parsers; add one for any new parser
that touches attacker-controlled bytes.

## `baston-zone`

Entities, their synchronisation, and resource loading.

| Module | Owns |
| --- | --- |
| `entity_manager.rs` | The authoritative entity store and dirty tracking |
| `state_ingest.rs` | Client updates in, with ownership and plausibility checks |
| `state_sync.rs` | The dirty flush onto NATS JetStream |
| `onesync/` | The OneSync-NG game state and ingest |
| `interest_ng.rs` | Priority-and-budget interest management |
| `adaptive_tick.rs` | The tick-rate controller |
| `boundary_detector.rs`, `boundary_loop.rs`, `handoff_manager.rs` | Zone handoffs |
| `resource_loader/` | Discovery, manifests, topological start order, hot reload |
| `packfile.rs` | RPF2 generation |

## `baston-config`

Every setting, its default, and its validation. Also the module resolution that
reconciles `[modules]` with the legacy per-section flags.

The house rule lives here most visibly: **every error names the fix.** When you
add a setting, add a validation with an actionable message.

## `baston-modules`

The registry from [ADR-002](../adr/002-module-tiers.md): module ids, tiers,
defaults, which are compiled in, and the `--modules` report. A leaf crate with
one dependency (serde), so anything may depend on it.

## `baston-db`

Pooled SQL behind the `db` capability. Drivers are features: `sqlite` (the
default), `postgres`, `mysql`.

`pool.rs` is an enum over the three sqlx pools rather than a trait object —
there are exactly three, chosen at build time. `value.rs` handles JSON ↔ SQL,
which is where the drivers genuinely differ.

## `baston-voice`

A Mumble-compatible server: "Mumble at the wire, custom brain". Speaks the
Mumble control and voice protocol to the stock FiveM client, with its own
routing core. A leaf crate with no V8, so its tests compile fast.

Currently emits **no metrics**, and proximity culling is not implemented.

### Replicated server variables

`/info.json` publishes `[server.vars]` merged with whatever the running scripts
have set, and the server-list heartbeat advertises the same document. Both read
`ScriptHost::server_vars()`.

**Known limitation:** that store lives in the process running the resources. In
single-process mode that is the gateway, so a script's `SetConvarServerInfo`
shows up immediately. Under `[meshing]`, resources run in zone processes and
the gateway publishes only what `[server.vars]` configured — a script-set
variable does not cross the process boundary yet.

Five names are reserved and cannot be written from either source, because the
FiveM client acts on them and BASTON must be the only thing that states them:
`sv_licenseKeyToken`, `sv_maxClients`, `sv_enforceGameBuild`, `onesync`,
`onesync_enabled`. See `RESERVED_VARS` in `http/info.rs`.

### Where a zone's territory is decided

`baston-zonemap` holds the geometry (`ZoneShape`, `Polygon`, `ZoneCoverage`)
and the ordered region list read from `meshing.map_file`. It is its own crate,
with only serde and toml, so it reaches `wasm32-unknown-unknown`: the map
editor in `tools/zone-map-editor` compiles it to WebAssembly and enforces
exactly what the Gateway enforces, rather than a JavaScript reimplementation
that would drift. `baston-protocol` re-exports the types and owns the protobuf
conversions, which is why nothing else had to learn a new crate name.

The Gateway is the only process that reads the file:
`ZoneRegistry` resolves ownership through it and hands each zone its
`ZoneCoverage` in the `RegisterZone` reply.

A coverage is two lists, not one — the zone's own regions, and the
higher-priority regions carved out of them. The second is what lets a zone
notice a player *entering* an area taken from the middle of its own, which
leaving-my-outline cannot see. `BoundaryDetector` then asks the only question
that has an answer for every shape: extrapolate the position, and is that
ground still ours?

There is no spatial tree. Ordered regions must be walked in order, which is
what a tree cannot do without collecting every candidate and re-sorting them.

### Server-created entities across the mesh

The world clients talk to lives in the gateway, so a zone's `CreateVehicle`
leases network ids from it (`GatewayService::LeaseNetworkIds`) and ships the
spawn to it (`SubmitWorldCommands`). `ZoneWorldControl` holds the leased block
and mints from it synchronously, because the native returns its handle with no
room for a round trip; a single drain task batches and sends, which is what
keeps a `Despawn` behind the `Spawn` it undoes.

Blocks come out of `GatewayWorldControl`'s own descending allocator rather than
from a partition agreed up front. One allocator is then the single authority, so
two zones cannot mint the same id and "spawn refused: id already in use" is not
a state a zone can reach.

## `baston-cfx`

CFX platform identity, without FXServer: key validation, the entitlement
ladder, nucleus registration and the server-list heartbeat. Identifies itself
as BASTON and never as FXServer — see
[ADR-004](../adr/004-cfx-identity-without-fxserver.md).

Two invariants live here rather than in the gateway, because they have to hold
whoever calls them. A licence may lower a slot count and never raise one; and
`Listing::heartbeat` refuses to advertise a server whose `/info.json` does not
publish the identity's token, because being listed and being slot-checked are
the same bargain.

## `baston-core`

The `ScriptDecryptor` seam the zone's resource loader passes every script
through, plus CFX-encryption (`.fxap`) detection. `PlainDecryptor` is the only
implementation: it passes plaintext through and refuses an escrowed file with
an error the operator can act on.

There used to be a second implementation, and two crates behind it —
`baston-cfx-platform` and `baston-escrow-plugin` — driving an operator-supplied
FXServer as a licence broker and escrow decryptor. Both were removed; see
[ADR-003](../adr/003-remove-the-fxserver-sidecar.md). The trait stays because it
is where escrow support would return and the one place that knows an `.fxap`
payload is not loadable.

## `baston-loadtest`

A headless client that speaks the binary `msgBastonState` protocol rather than
the full FiveM one. Used for the published benchmarks. Not shipped.

## Adding a crate

Rare, and usually the wrong answer. A new crate earns its place when it has a
dependency the rest of the workspace should not carry — that is why `baston-db`
(sqlx) and `baston-voice` (rustls, prost) exist.

If it is just organisation, use a module.

## Next

- [Adding a native](adding-a-native.md)
- [Adding a module](adding-a-module.md)
- [The wire protocol](../internals/protocol.md)
