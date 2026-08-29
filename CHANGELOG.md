# Changelog

All notable changes to Baston will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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

- Moved global CFX identity ownership from zone processes to the public gateway.
- Restricted zone-local FXServer sidecars to the deferred Asset Escrow path.
- Bound the HTTP and ENet game transports to `server.bind_address` so the
  official broker can use loopback on the same game port.

### Security

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
