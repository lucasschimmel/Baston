---
title: "Going public"
description: "The security checklist before anyone outside your friends can reach the server, and the one choice your CFX key forces."
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

## How you look in the server browser

```toml
[server]
name = "Le Baston"
icon = "myLogo.png"             # 96x96 PNG, nothing else

[server.vars]
sv_projectName = "Chez Lucas"
sv_projectDesc = "Serveur roleplay entre potes"
tags = "roleplay, francais"
locale = "fr-FR"
banner_detail = "https://example.com/banner-1920x384.png"
banner_connecting = "https://example.com/connecting.png"
```

`[server.vars]` is a passthrough. BASTON does not know these names and does not
validate them — FXServer doesn't either, it publishes every variable carrying
the `ConVar_ServerInfo` flag and lets the browser recognise what it wants. So
these work, and so will whatever CFX adds next.

Scripts write to the same store (`SetConvarServerInfo`, `SetGameType`,
`SetMapName`) and win over the file, which is FXServer's behaviour for `sets`.

**Everything in `[server.vars]` is public** — `/info.json` is served before
authentication. Never put a secret there.

The icon is checked at boot: a file that is not a 96×96 PNG stops the server
with its actual dimensions in the message. The FiveM browser will not display
any other size, so accepting it and dropping it silently would leave you
staring at an empty square with nothing to go on.

## The CFX licence and the server list

There is one choice here, and it decides the shape of your server.

```toml
[license]
mode = "cfx"                    # "off" | "gate" | "cfx"
sv_license_key = "cfxk_…"

[listing]
enabled = true
ip_override = "203.0.113.10"    # the public address players connect to
```

`cfx` authenticates your key with CFX, applies what it grants, and puts the
server in the FiveM list. No FXServer involved — BASTON performs the same
exchanges itself, identifying itself as BASTON
([ADR-004](../adr/004-cfx-identity-without-fxserver.md)).

**It also caps your slots to what your key grants**, and those two things
cannot be separated. Publishing the licence token is what makes the FiveM
client look your entitlements up; a server that publishes nothing has nothing
looked up.

| | `off` | `cfx` |
| --- | --- | --- |
| In the FiveM server list | no | yes |
| Slots | whatever you configure | capped to your tier (48 / 64 / 128 / 2048) |

So: **500+ players → `off`**, and hand out your connect address. **A server
that needs to be found → `cfx`**, where the cap will never bind on you.

A failure to authenticate stops the boot. A server that asked to be
authenticated does not start unauthenticated.

**Escrowed (`.fxap`) resources still do not run** — decryption lives inside
`svadhesive` and no token opens it from outside. Ask the author for an
unescrowed build.

`mode = "verified"` is **rejected at parse time**; it ran the removed FXServer
sidecar. Use `"cfx"`. Full detail in
[CFX licensing](../operations/licensing.md).

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
