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

- Added the zone map: an ordered list of regions in its own TOML file
  (`[meshing] map_file`), where the owner of a point is the first region that
  contains it. Regions are rectangles, circles or traced outlines, and they may
  overlap — so an event venue can be carved out of the city around it without
  cutting a hole in the city's outline, and one zone can own several separate
  areas by being listed more than once.

  Order is the priority, deliberately rather than a `priority` number: a number
  invites ties, and a tie has to be broken by something that does not survive a
  restart. The last region must match everything, which is what turns a gap in
  the map from unlikely into impossible — a player standing in one had no
  owning zone and was reassigned to the least-loaded one on every scan.

  The Gateway holds the map and hands each zone its territory in the
  `RegisterZone` reply, so `zone_bounds` / `ZONE_BOUNDS` stop being N separate
  claims taken on faith. It refuses a map with no catch-all, a self-intersecting
  outline, or a key belonging to another shape, and warns about a region an
  earlier one has made unreachable — the likeliest authoring mistake, which
  otherwise looks like a zone silently refusing to open. A zone claiming no
  region is refused by name.

  Without a `map_file` nothing changes: zones declare their own rectangles as
  before.

### Changed

- A zone's boundary scan now asks whether the ground it is about to stand on is
  still its own, instead of measuring the distance to the nearest rectangle
  edge and checking the outward speed. The old question only has an answer for
  a rectangle; the new one is the same for a circle, a coastline, or a
  territory with a hole in the middle.

  This closes a case the old test could not see at all: a player walking from a
  zone *into* an area carved out of it never leaves that zone's outline, so
  nothing fired and they stayed on a zone that no longer owned the ground under
  them. A zone is now told which higher-priority regions cut into its own.

- `[server] enforce_game_build` is now one setting with one meaning, instead of
  two halves that could disagree. It is validated at load — a decimal build
  between 1604 and 4999, or `""` — so `"latest"` or a mistyped `"32258"` stops
  the boot with the value named, rather than reaching `/info.json` verbatim and
  failing later inside the client as a build switch that never happens. The
  bound is a typo catcher, not an allowlist: a build Rockstar ships next still
  works without a code change.

  Three consequences that were previously reachable are now not.

  The value no longer has a **silent parse fallback**. The sync-tree decoder
  used to take `parse().unwrap_or(3258)`, so a config the server accepted could
  leave it decoding one build's node layouts while its clients ran another —
  the desync that looks like random rubber-banding. Config and decoder now go
  through one parser, and a server that enforces nothing says at boot which
  build it decodes against instead of leaving it to be discovered.

  The **default is `"3258"` rather than empty**, matching the build the decoder
  already fell back to. A server that stated no build was never unenforced in
  effect — only unstated. Operators who want no enforcement set `""`
  explicitly.

  And a client that did not switch build is **refused at `initConnect`**,
  naming both builds, instead of being accepted and desynchronising. The
  `gameBuild` the client reports was parsed and then dropped; the
  `<build>_<revision>` form is honoured, and a client that reports no build at
  all is still allowed, since absence is not evidence of a mismatch.

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

- Removed the Gateway's quadtree. Ordered regions have to be walked in order,
  which is what a tree cannot do without collecting every candidate and
  re-sorting them; `find_zones_in_aabb`, its only other caller, was documented
  dead code from a cross-zone AoI the architecture made unnecessary.

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
