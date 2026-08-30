---
title: "CFX licensing"
description: "How BASTON proves a legitimate CFX server identity without reimplementing it."
---

Baston integrates with the official CFX platform without copying or patching
its closed authentication code. The operator supplies an official
`FXServer.exe`; Baston runs it unmodified as a broker and consumes only the
local identity result exposed by a small server resource.

Asset Escrow is a separate, deferred capability. This document covers server
identity, slot policy, client entitlements, and public server-list presence.

## Security boundary

- The genuine FXServer process owns Keymaster authentication and public-list
  heartbeats.
- Baston never loads `svadhesive.dll` through an invented FFI, patches it,
  extracts its private listing token, or reimplements the closed handshake.
- The licence key is written to a temporary launch configuration, never to the
  command line or logs. The file is removed when the broker stops, including
  startup failures.
- The authenticated `sv_licenseKeyToken` uses a redacted Rust type. It is
  published only where the FiveM client expects it: `info.json` under
  `vars.sv_licenseKeyToken`.
- Policy lookup uses the official public policy endpoint with redirects
  disabled, a fixed timeout, and a 64 KiB response limit.
- Authentication fails closed. Policy lookup fails conservatively at 48 slots
  and never fabricates a paid grant.

## Runtime architecture

The gateway is the sole owner of the CFX server identity:

1. It starts a private official FXServer broker and waits for a valid local
   identity token before opening any public listener.
2. It resolves the token's CFX policy and lowers `server.max_players` when
   needed.
3. Baston starts its HTTP accept loop and UDP game transport on the selected
   interface.
4. If public listing is enabled, it replaces the private broker with an
   official broker carrying the already-capped public metadata. Registration
   and heartbeats therefore begin only after the advertised endpoint is bound.
5. FXServer binds
   only `127.0.0.1` on the same numeric port, so it can own CFX registration
   without proxying gameplay.
6. If the broker exits, the authenticated gateway shuts down instead of
   continuing with a stale identity.

Zone processes never authenticate or register independently. A zone may run a
separate, non-listing sidecar only when Asset Escrow is explicitly enabled.

## Configuration

Development and private verified mode:

```toml
[server]
name = "Baston"
port = 30120
bind_address = "0.0.0.0"
max_players = 64

[license]
mode = "verified"
sv_license_key = "cfxk_REPLACE_WITH_OPERATOR_SECRET"
fxserver_path = "C:/FXServer/FXServer.exe"
sidecar_port = 30130
public_listing = false
```

Public-list mode:

```toml
[server]
name = "Baston Production"
port = 30120
# A concrete local interface assigned to this machine. Do not use 0.0.0.0 or
# 127.0.0.1: FXServer needs 127.0.0.1:30120 for its listing endpoint.
bind_address = "192.0.2.10"
max_players = 128

[udp]
# Omit this field or keep it equal to server.port.
port = 30120

[license]
mode = "verified"
sv_license_key = "cfxk_REPLACE_WITH_OPERATOR_SECRET"
fxserver_path = "C:/FXServer/FXServer.exe"
sidecar_port = 30130
public_listing = true
# The public address players use. NAT port forwarding must map TCP and UDP
# 30120 to server.bind_address:30120.
listing_ip_override = "203.0.113.10"
```

`listing_ip_override` may equal `bind_address` on a directly addressed host. On
a NAT deployment they are normally different.

### Modes

| Mode | Behaviour | Production identity |
|---|---|---|
| `off` | No server-licence check; startup warning | No |
| `gate` | Validates only the configured key's shape | No |
| `verified` | Requires a genuine FXServer broker and a valid token | Yes |

`public_listing = true` is accepted only with `mode = "verified"`, a concrete
unicast `listing_ip_override`, a concrete non-loopback `bind_address`, and the
same TCP/UDP game port.

## Slots and client entitlements

The policy names are mapped to the same slot ceilings used by the public CFX
server code:

| Authenticated policy | Ceiling |
|---|---:|
| base | 48 |
| `onesync` | 64 |
| `onesync_plus` or `onesync_medium` | 128 |
| `onesync_big` | 2048 |

Baston only lowers the configured value. It never raises `max_players` beyond
the operator's setting.

The token in `info.json` lets the standard FiveM client query its normal CFX
policy, including granted streaming/clothing features. Baston retains unknown
policy names instead of guessing their meaning. Server-side feature switches
must still be implemented explicitly before Baston can enforce a new grant
locally.

## Live validation

Use a real key through a local, uncommitted configuration:

```powershell
$env:BASTON_CONFIG = "C:\secure\baston.production.toml"
cargo run --release -p baston-gateway
```

The broker-only smoke test accepts the secret exclusively through the process
environment:

```powershell
$env:BASTON_TEST_FXSERVER = "C:\FXServer\FXServer.exe"
$env:BASTON_TEST_LICENSE_KEY = "<real-key>"
cargo test -p baston-cfx-platform real_fxserver_reports_licence_status -- --ignored --exact
Remove-Item Env:BASTON_TEST_LICENSE_KEY
```

Verify the local contract without printing the token:

```powershell
$Info = Invoke-RestMethod "http://203.0.113.10:30120/info.json"
$Info.vars.sv_maxClients
[bool]$Info.vars.sv_licenseKeyToken
```

Then verify:

- the effective slot value matches or is below the Keymaster entitlement;
- the endpoint is reachable externally on both TCP and UDP;
- the server appears in the CFX list after the normal heartbeat delay;
- stopping the official broker also stops the authenticated gateway;
- an invalid key prevents every public game listener from starting.

Never paste a real licence key into an issue, log capture, test fixture, or
tracked TOML file.

## Troubleshooting

| Symptom | Resolution |
|---|---|
| `mode = "verified" but fxserver_path is not set` | Point to an official `FXServer.exe`. |
| The broker returns no authenticated token | Check the key and outbound CFX connectivity. |
| Policy lookup falls back to 48 slots | Restore access to the official policy endpoint; Baston intentionally remains conservative. |
| Public listing rejects `0.0.0.0` | Select the machine's concrete LAN/public interface in `server.bind_address`. |
| Address already in use | Ensure Baston uses the concrete interface and no other process owns either that interface or `127.0.0.1` on the game port. |
| Server is authenticated but absent from the list | Check NAT/firewall forwarding, `listing_ip_override`, and that the official broker remains alive. |
