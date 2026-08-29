# ADR-001: Use official FXServer as the CFX trust broker

Date: 2026-07-26  
Status: accepted

## Context

Baston must prove a legitimate CFX server identity, enforce Keymaster slot
entitlements, expose the standard client policy token, and optionally appear in
the public CFX server list. The closed `svadhesive` component has no supported
application-facing FFI for these operations. Copying its implementation,
patching the DLL, or replaying private platform tokens would create a fragile
and non-compliant trust boundary.

The gateway is the only FiveM-facing Baston process. A distributed deployment
may also contain multiple zone processes, but those zones must not register
independent identities for one logical server.

## Decision

We use an operator-supplied, unmodified official `FXServer.exe` as a local CFX
trust broker. It hosts `svadhesive` in its intended runtime, performs Keymaster
authentication, and owns public registration and heartbeats. A minimal server
resource exposes the resulting local `sv_licenseKeyToken` to Baston over an
isolated file-drop IPC channel.

The Baston gateway authenticates privately before opening public listeners,
resolves the official policy once, applies only restrictive slot changes, then
activates public listing after its HTTP and UDP endpoints are live. Zone
processes never own the global identity; their optional sidecars are restricted
to Asset Escrow resource handling.

## Consequences

### Positive

- The closed CFX component remains unmodified and executes in its supported
  host.
- Baston does not need to reproduce or retain CFX private authentication and
  listing protocols.
- Slot and feature decisions are derived from the same token contract used by
  the standard FiveM client.
- Broker death invalidates the gateway runtime instead of leaving a stale
  authenticated server online.

### Negative

- Verified mode requires an official Windows FXServer artifact supplied and
  updated by the operator.
- Startup includes a second process and bounded network round trips.
- Public listing requires Baston to bind a concrete interface so FXServer can
  use loopback on the same numeric game port.

### Neutral

- The licence key remains an operator secret and must be injected through
  untracked configuration or the process environment.
- Asset Escrow remains a separate capability and may use zone-local brokers
  with unique IPC resources.
- New CFX policy names remain inert until Baston explicitly implements their
  server-side behavior.

## Alternatives Considered

### Load `svadhesive.dll` directly

Call the DLL through a new Baston FFI boundary.

**Why rejected:** the DLL exposes the CitizenFX component entry point rather
than a supported licence or escrow API. Reconstructing its internal host would
couple Baston to a closed ABI and would not provide a legitimate integration
contract.

### Reimplement the CFX platform handshake

Reproduce Keymaster authentication, token exchange, and server-list requests in
Rust.

**Why rejected:** this would duplicate a private protocol, increase
compatibility and compliance risk, and make Baston responsible for security
properties already owned by the official component.

### Proxy all gameplay through FXServer

Use a genuine FXServer as the public network front and tunnel traffic to Baston.

**Why rejected:** this would put the existing FXServer game-server runtime back
on Baston's hot path and undermine the architectural goal of replacing that
runtime. The selected design delegates only platform identity and listing.

## References

- [CFX `ServerLicensingComponent`](https://github.com/citizenfx/fivem/blob/master/code/components/citizen-server-impl/include/ServerLicensingComponent.h)
- [CFX `NetLibrary.cpp` policy consumption](https://github.com/citizenfx/fivem/blob/master/code/components/net/src/NetLibrary.cpp)
- [Baston CFX identity and listing runbook](../licensing.md)
