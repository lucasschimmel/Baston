# Changelog

All notable changes to Baston will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Added the four-tier module system (ADR-002): capabilities are now classified
  as core, runtime-toggled modules, build-time capabilities, or out-of-process
  addons, and the tier decides the mechanism.
- Added the `[modules]` configuration section, `BASTON_MODULE_*` environment
  overrides, and `baston-gateway --modules`, which reports the bundle and
  distinguishes modules that are *off* from capabilities that are *absent*.
- Added a Lua scripting runtime (mlua / Lua 5.4) as a Tier 2 capability. Lua
  resources run against the same native implementations as JavaScript ones; a
  resource selects its engine through its server-script extensions.
- Added four CI-tested bundles — `lite`, `js`, `lua`, `full` — so a capability
  behind a Cargo feature cannot rot without the build noticing.
- Added the `db` module: pooled SQL for scripts over SQLite, PostgreSQL or
  MySQL/MariaDB, with the same four calls in both languages. Queries run on the
  server's runtime and never on a script thread, so a slow statement cannot
  stall a resource's events. Parameters are bound by the driver, never spliced
  into the SQL.
- The Lua runtime reaches parity with JavaScript: a watchdog that terminates a
  runaway script, `playerConnecting` deferrals, `exports`, state-bag change
  handlers, zone transfer state, and server → client native dispatch.

- Added the `displayinfo` debug overlay: a server-assembled in-game readout of
  the zone mesh, OneSync state, and per-player link statistics, gated by
  `[debug] display_info` and reachable with `/displayinfo`.
- Added builtin resources — client code shipped inside the server binary,
  advertised straight into `getConfiguration` and served from memory, with no
  presence on disk and no way for a resources directory to replace it.

### Changed

- The repository is now a monorepo with one directory per kind of thing:
  `crates/` (the server), `docs/` (Markdown), `apps/` (the documentation
  website), `config/`, `deploy/`, `examples/`, `tools/`. Nothing that belongs
  inside one of them sits at the root any more.
- Configuration is discovered rather than assumed: `BASTON_CONFIG`, else
  `baston.toml`, else `config/baston.toml`. A deployed server and a checkout
  each keep their natural layout.
- Documentation is grouped by what the reader is doing — guides, reference,
  operations, internals, decisions — and rendered as a website by `apps/docs`,
  which reads `docs/` directly instead of holding a second copy.
- The CFX natives no longer depend on deno_core, so one implementation serves
  both scripting engines. `extensions/` is now only the V8 half of the bridge.
- A resource with no server scripts no longer spawns a runtime; client-only and
  streaming-only resources used to cost an empty V8 isolate each.

- Added CFX server identity **without FXServer** (`baston-cfx`,
  [ADR-004](docs/adr/004-cfx-identity-without-fxserver.md)). `[license] mode =
  "cfx"` validates the operator's key with CFX, reads the entitlements from the
  same endpoint the FiveM client checks, lowers `max_players` to the granted
  ceiling before any listener opens, and publishes `sv_licenseKeyToken`.
  `[listing] enabled` adds nucleus registration and the server-list heartbeat.

  BASTON identifies itself as BASTON — `User-Agent: BASTON/…`, never
  FXServer's. A refusal from CFX is reported as a refusal, with the agent that
  was sent, and the answer is `mode = "off"` rather than a forged agent.

  The two properties this couples are enforced structurally, not by convention.
  A licence may lower a slot count and never raise one, and the check runs at
  boot rather than leaving the client to bounce players at connect time. And a
  server cannot be listed while serving an `/info.json` without its licence
  token: `/info.json` and the heartbeat are built by one function, and
  `Listing::heartbeat` refuses a snapshot that omits it — being discoverable
  and being slot-checked are the same bargain.

  One deliberate divergence from the client: `NetLibrary.cpp`'s ladder has no
  branch above 2048, so a server declaring more is checked against plain
  `onesync`. BASTON caps at 2048 instead of using the gap.

- Added `[server.vars]` and `[server] icon`: the server browser's name,
  description, tags, locale, banners and logo. `[server.vars]` is CFX's `sets`
  mechanism — BASTON publishes every entry verbatim in `/info.json` `vars` and
  does not know or validate the names, exactly as FXServer iterates the
  `ConVar_ServerInfo` flag rather than looking fields up, so fields CFX adds
  later work without a code change. The icon is validated as a 96×96 PNG at
  boot, with its real dimensions in the error.

  This also fixes four natives that looked like they worked and did not.
  `SetGameType`, `SetMapName`, `FlagServerAsPrivate` and
  `EnableEnhancedHostSupport` wrote to a convar store the gateway had no
  accessor for, so nothing ever read it — a script calling `SetGameType` got no
  error and no effect. `SetConvar` / `SetConvarServerInfo` are now named
  natives rather than a JS-only op, so Lua reaches them too instead of falling
  through to the neutral-value path.

  Five names are reserved from both sources — `sv_licenseKeyToken`,
  `sv_maxClients`, `sv_enforceGameBuild`, `onesync`, `onesync_enabled` — because
  the client acts on them. That closes the other door on the ADR-004 invariant:
  the heartbeat already refused to list a server hiding its token, and
  configuration now cannot forge one, or advertise a slot count the licence
  never granted.

- Server-created entities now work in a zone process. `CreateVehicle`,
  `CreatePed` and `CreateObject` under `[meshing]` used to return a plausible
  non-zero handle and create nothing, for anyone, in any zone — only the gateway
  wired a `WorldControl`, so the native fell through to a server-local record
  and the usual `if veh == 0` guard did not catch it.

  The world clients talk to lives in the gateway, so the two halves of a
  creation are split. Ids are leased ahead in blocks (`LeaseNetworkIds`) and
  minted locally, because the native returns its handle with no room for a round
  trip; spawns are shipped asynchronously (`SubmitWorldCommands`) by a single
  drain task per zone, so a `Despawn` cannot overtake its `Spawn`. Blocks come
  out of the gateway's own descending allocator, which makes them exclusive
  without coordination — two zones cannot mint the same id.

  A world that exists and refuses is no longer confused with no world at all:
  `WorldControl::is_authoritative` separates them, so an exhausted id space
  returns 0, the invalid handle, instead of a synthetic one. The gateway also
  stops wiring a world control when OneSync is off, where a spawn was accepted
  and dropped at the tick; scripts there get the server-local record that path
  was designed around, which is what its own comment always claimed.

### Removed

- Removed the FXServer sidecar and everything that existed only to serve it:
  the `baston-cfx-platform` and `baston-escrow-plugin` crates, the `escrow`
  Cargo feature and module, `baston-core::license`, `[license] mode =
  "verified"` with `fxserver_path` / `sidecar_port` / `public_listing` /
  `listing_ip_override`, and the `[escrow]` section. See
  [ADR-003](docs/adr/003-remove-the-fxserver-sidecar.md).

  BASTON no longer appears in the public CFX server list, enforces no licence
  entitlement, and cannot run escrowed (`.fxap`) resources. The sidecar was
  Windows-only, was never validated against a real CFX key, and put a process
  BASTON does not control on the boot path.

  `[license]` keeps `mode = "off" | "gate"` and `sv_license_key`; `gate` checks
  the key's shape and nothing else. A config carrying `mode = "verified"` is
  **rejected at parse time** rather than silently downgraded, so a server does
  not boot unauthenticated while its operator believes CFX validated the key. A
  stale `[escrow]` section is ignored.

### Security

- The monitoring/control API, the `displayinfo` overlay and the script profiler
  are now off by default. Each widens what a caller can do to a running server,
  so they open where an operator asks rather than by default. Enable them with
  `[modules] enable = ["admin-api", "debug-overlay", "profiler"]`.
- `/info.json` no longer carries `sv_licenseKeyToken`. BASTON obtains no CFX
  token, and publishing an empty or invented one would tell a connecting client
  the server is licensed when it is not.

[Unreleased]: https://github.com/lucasschimmel/Baston/compare/develop...HEAD
