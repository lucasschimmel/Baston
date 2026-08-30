---
title: "Going public"
description: "The security checklist before anyone outside your friends can reach the server — and what your CFX key does not buy you."
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

## The CFX licence, the server list, and escrow

Read this part before you plan anything around it.

**BASTON does not appear in the FiveM server list.** Nothing in it registers
with CFX or sends a heartbeat. Players reach your server by direct connect —
`connect your.host:30120`, or an `fivem://connect/` link you hand out yourself.

**Your CFX key buys you nothing here.** BASTON never contacts CFX, so it never
learns what your key grants and enforces no entitlement from it. `max_players`
is exactly what you configured, whatever tier you pay for.

**Escrowed (`.fxap`) resources do not run.** A resource whose scripts are CFX
Asset Escrow-encrypted is refused at load with an explicit error. There is no
flag for it; ask the author for an unescrowed build.

An earlier version of BASTON hosted an official FXServer alongside itself to
do all three. It was Windows-only, never validated end to end, and it put a
process BASTON does not control on the boot path — it was removed. The full
reasoning is in [ADR-003](../adr/003-remove-the-fxserver-sidecar.md).

### What `[license]` still does

```toml
[license]
mode = "gate"                       # "off" | "gate"
sv_license_key = "cfxk_…"
```

`gate` checks the key's **shape** — non-empty, no whitespace, at least 20
characters, not a placeholder — and refuses to boot if it fails. That is a
typo check, not authentication: a revoked key passes it. Both modes warn at
every boot that no licence is enforced.

`mode = "verified"` no longer exists and is **rejected at parse time** rather
than quietly downgraded, so a config carrying it stops instead of booting
unauthenticated. See [CFX licensing](../operations/licensing.md).

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
- [CFX licensing](../operations/licensing.md) — what the key does and does not do
- [Troubleshooting](troubleshooting.md)
