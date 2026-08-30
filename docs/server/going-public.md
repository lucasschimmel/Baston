---
title: "Going public"
description: "The CFX licence, the server list, and the security checklist before anyone outside your friends can reach the server."
---

Everything here is unnecessary for a server you and your friends reach over a
LAN or a private address. It becomes necessary the moment the port is open to
the internet.

## The security checklist

Do this before you open the port. In order of how badly it goes wrong.

### 1. `dev.auth_bypass` must be false

```toml
[dev]
auth_bypass = false
```

With it on, CFX tickets are **not validated**. Anyone can claim any identity —
including one your whitelist trusts. It also disables the token check on
`getConfiguration`, letting anyone enumerate your resources.

BASTON warns loudly on every boot when it is on. That warning is the feature.

### 2. Do not expose the admin and metrics ports

| Port | What it exposes |
| --- | --- |
| `8080` (admin) | player list, kick, resource control, **`console.execute`** |
| `9090` (metrics) | resource names, player counts, internal timings |

`console.execute` is remote code execution by design. Bind these to localhost or
a private network, or put them behind a VPN. Only the game port (`30120`, TCP
and UDP) needs to face the internet.

### 3. Give every API key the minimum it needs

```toml
[[api.keys]]
name = "discord-bot"
token = "…"                       # openssl rand -hex 32
permissions = ["monitor.read"]    # not console.execute
```

BASTON refuses weak, duplicated or placeholder tokens, and keys with no
permissions or no name. It cannot refuse a key you gave every permission to.

The legacy `meshing.admin_token` is an implicit key with **all seven
permissions** — prefer `[[api.keys]]`, and leave `admin_token` empty unless the
legacy `/admin/*` routes are needed.

Every control action is written to `api.audit_log`, including denied attempts.

### 4. Do not trust `ip:` identifiers

The `ip:` identifier comes from the `x-real-ip` header, which is
**attacker-controlled** unless a trusted reverse proxy sets it. Do not use it
for bans or allowlists. Use `license:`, which comes from the signed CFX ticket.

### 5. Turn off what you are not using

```toml
[modules]
disable = ["debug-overlay"]
```

`debug-overlay` with `display_info = "everyone"` exposes zone topology and
per-player network statistics to anyone connected. `admin-api` and `profiler`
are off by default; leave them off unless you need them.

### 6. Know what your resources expose

- A resource's `SetHttpHandler` serves on the **public game port** with no
  authentication.
- `PerformHttpRequest` has **no SSRF protection** — a resource that fetches a
  player-supplied URL lets that player probe your internal network.
- In JavaScript, **every event handler is remotely reachable** —
  `RegisterNetEvent` is a no-op there. Validate `source` and every argument.

---

## The CFX licence

A public FiveM server needs a CFX server key, from
[portal.cfx.re](https://portal.cfx.re). BASTON never validates it itself and
never talks to CFX — a genuine, unmodified FXServer you supply does that. See
[ADR-001](../adr/001-use-official-fxserver-as-cfx-trust-broker.md).

### Three modes

| Mode | What it does | For |
| --- | --- | --- |
| `off` | No check. Warns every boot. | LAN and development |
| `gate` | Checks the key's **shape only** — never contacts CFX | A quick sanity check |
| `verified` | Runs the official FXServer broker, validates against CFX, enforces the verdict | **Public servers** |

`gate` is not authentication. It checks the string is non-empty, at least 20
characters, has no whitespace and is not a placeholder. That is all.

```toml
[license]
mode = "verified"
sv_license_key = "cfxk_…"
fxserver_path = "Artifacts/windows/31623/FXServer.exe"
sidecar_port = 30130
```

`verified` needs an FXServer you downloaded from CFX. **BASTON never ships it.**
It is Windows-only in practice.

If the broker exits while the server is running, BASTON **shuts down**. An
unauthenticated server does not keep serving.

### Slot caps

Your licence's entitlements can **lower** `server.max_players`, never raise it:

| Policy | Slots |
| --- | --- |
| `onesync_big` | 2048 |
| `onesync_plus`, `onesync_medium` | 128 |
| `onesync` | 64 |
| none, or the policy fetch failed | **48** |

A failed policy fetch is not an error — it falls back to 48 conservatively. A
cap being applied is logged as a warning. Set `max_players` to what you actually
want and let the licence lower it if it must.

A free key returns empty grants: listing visibility, no OneSync entitlement.

---

## The server list

Opt in explicitly:

```toml
[license]
mode = "verified"          # required
public_listing = true
listing_ip_override = "203.0.113.10"
```

`listing_ip_override` is **required** — a concrete, non-loopback, unicast
address. `bind_address` must also select one concrete interface, and
`udp.port` must equal `server.port`.

The genuine FXServer component does the registration and the heartbeats; BASTON
never contacts the CFX list itself.

The ordering is deliberate and worth knowing: BASTON authenticates privately
first, resolves entitlements, applies the slot cap, brings up its listeners, and
**only then** activates public listing. The first heartbeat the world sees
cannot overstate your capacity.

---

## Escrowed assets

If you run resources from the CFX Keymaster, you need the `escrow` capability, a
Windows build, and an FXServer:

```toml
[modules]
enable = ["escrow"]

[escrow]
enabled = true
backend = "sidecar"
server_license = "license:…"
fxserver_path = "Artifacts/windows/31623/FXServer.exe"
```

Escrow covers **scripts only** — encrypted `stream/` assets are out of scope.
See [Asset escrow](../operations/escrow.md).

---

## Operational readiness

Not security, but the difference between a server you run and one that runs you.

- **Back up** `resources.kvp_path` and your database. Nothing else holds player
  state.
- **Watch `kvp_flush_failures_total`.** Non-zero means players are losing data
  and nobody will notice for hours.
- **Point Prometheus at it.** [Monitoring](monitoring.md).
- **Turn off hot reload.** A deploy that writes files one at a time restarts the
  resource several times.

```toml
[dev]
hot_reload = false
auth_bypass = false
```

- **Know the version story.** This is `0.1.0-alpha`; there is no upgrade
  guarantee between versions yet. Pin the binary you tested.

## Next

- [Monitoring](monitoring.md)
- [CFX licensing](../operations/licensing.md) — the full detail
- [Troubleshooting](troubleshooting.md)
