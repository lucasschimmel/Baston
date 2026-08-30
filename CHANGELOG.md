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

- Added verified CFX server identity through an operator-supplied, unmodified
  FXServer broker.
- Added policy-derived slot ceilings for the official 48, 64, 128, and 2048
  player tiers, with a conservative 48-slot fallback.
- Added standard `sv_licenseKeyToken` publication, enabling the FiveM client to
  resolve its granted streaming and clothing policies normally.
- Added opt-in public CFX server-list registration with validated interface,
  port, and public-address configuration.
- Added a real-FXServer authentication smoke test driven by uncommitted
  environment secrets.
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
- `baston-cfx-shim` moved into `baston-cfx-platform`, the crate that compiles
  it. It used to sit in a shared `resources/` directory alongside a dev sample
  and a test fixture, which read as three things of the same kind and was not.
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
- Moved global CFX identity ownership from zone processes to the public gateway.
- Restricted zone-local FXServer sidecars to the deferred Asset Escrow path.
- Bound the HTTP and ENet game transports to `server.bind_address` so the
  official broker can use loopback on the same game port.

### Security

- The monitoring/control API, the `displayinfo` overlay and the script profiler
  are now off by default. Each widens what a caller can do to a running server,
  so they open where an operator asks rather than by default. Enable them with
  `[modules] enable = ["admin-api", "debug-overlay", "profiler"]`.
- Authentication now fails closed before public listeners open and the gateway
  shuts down if its authenticated broker exits.
- Licence keys and identity tokens use redacted debug output, bounded IPC, and
  randomized lifetime-scoped temporary files.
- Policy requests reject redirects, cap response size, and never grant paid
  capabilities on failure.
- Colocated sidecars use isolated shim resources, IPC directories, and
  cancellation-aware startup to prevent cross-process response confusion or
  orphaned public heartbeats.

[Unreleased]: https://github.com/lucasschimmel/Baston/compare/develop...HEAD
