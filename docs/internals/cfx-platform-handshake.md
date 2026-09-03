---
title: "CFX platform handshake"
description: "What the client and the platform exchange before a player reaches the game."
---

> **Implemented, as BASTON.** This documents the CFX platform flow (licence
> validation → nucleus register → server-list ingress) as captured from a live
> FXServer in July 2026. Steps ② and ③ are also in the public tree
> (`ServerNucleusMock.cpp`, `GameServer.cpp`); step ① is the one that lives
> inside the closed `svadhesive` component, and it is what this capture added.
>
> BASTON performs all three in `baston-cfx` when `[license] mode = "cfx"`,
> sending `User-Agent: BASTON/…` — **never FXServer's**. The decision, and the
> line it does not cross, are in
> [ADR-004](../adr/004-cfx-identity-without-fxserver.md).
>
> Two details below are *not* implemented as described, deliberately: the
> `User-Agent` values in ① and ②, and the >2048 gap in the client's slot ladder
> (see ④). Both are recorded here because they are what FXServer does, not
> because BASTON does them.


Reverse-engineered from a live FXServer 31623 boot (mitmproxy capture,
2026-07-04), filling the gap the engine-source mirror leaves open. This is the
sequence a **registered** server goes through (server list, and — with a paid
key — OneSync slots / policy features).

Re-observed 2026-08-30 against a BASTON-identified client: ① returns HTTP 200
and one additional field, `hashid`.

All calls use plain HTTPS, no client certs. FXServer sends
`User-Agent: FXServer/1 (...)` for the license call and `CitizenFX/1` for
register/ingress.

## ① License validation — the missing link

```
GET https://portal-api.cfx.re//v1/key/validate/<sv_licenseKey>
    User-Agent: FXServer/1 (master SERVER v1.0.0.31623 win32)
```

Note the **double slash** after the host (FXServer builds `EP + "/v1/..."`
where `LICENSING_EP` already ends in `/`). The key is in the URL path, not a
header or body.

Response (200):

```json
{
  "success": true,
  "valid": true,
  "key_user": 19334846,
  "token": "<sv_licenseKeyToken>",        // "<serverId>_<userId>:<hex>" — served in info.json vars
  "grants_token": "<JWT, aud=https://cfx.re>",     // entitlements (see below)
  "nucleus_token": "<JWT, aud=https://cfx.re>",    // used for ② register
  "listing_token": "<JWT, aud=https://servers-live.fivem.net>", // used for ③ ingress
  "policy": []                              // top-level policy array
}
```

### grants_token JWT claims

```
iss: https://keymaster.fivem.net
sub: [userId, "Loxus-a855c8"(serverName), serverId]
aud: https://cfx.re
exp/nbf/iat: ~22-day validity window
grants_clk: {}      // ← entitlement grants, EMPTY on a free (Pebble) key
grants: {}          // ← EMPTY on Pebble
disabled: []
```

**Key finding:** a **Pebble (free)** key returns `grants: {}`, `grants_clk: {}`
and top-level `policy: []`. So registration alone gives listing visibility but
**no OneSync entitlement and no policy features** (custom clothing streaming,
pool increases). Those require a paid tier (Argentum/Aurum/Platinum) — the
grants would then be non-empty and `policy-live.fivem.net` would return the
entitlement strings the client checks.

## ② Nucleus registration

```
POST https://cfx.re/api/register/?v=2
     Content-Type: application/x-www-form-urlencoded
     User-Agent: CitizenFX/1

     token=<nucleus_token>&port=30120&ipOverride=
```

Response (200):

```json
{ "host": "deprecated-dg3rx8d.users.cfx.re" }
```

The `host` is the reverse-proxy hostname assigned to this server on the
`users.cfx.re` domain (the "deprecated-" prefix is what a free key gets).

## ③ Server-list ingress heartbeat

```
POST https://servers-frontend.fivem.net/api/serverlist/ingress
     Content-Type: application/json; charset=utf-8
     User-Agent: CitizenFX/1
```

Body:

```json
{
  "port": 30120,
  "listingToken": "<listing_token>",
  "ipOverride": "",
  "private": false,
  "fallbackData": {
    "dynamic": { "clients": 0, "gametype": "", "hostname": "...",
                 "iv": "1903551649", "mapname": "", "sv_maxclients": "48" },
    "info": {
      "enhancedHostSupport": true, "requestSteamTicket": "unset",
      "resources": ["hardcap"], "server": "FXServer-...31623 win32",
      "vars": {
        "gamename": "gta5", "onesync_enabled": "false",
        "sv_enforceGameBuild": "3258", "sv_defaultGameBuild": "3258",
        "sv_licenseKeyToken": "<token from ①>", "sv_maxClients": "48",
        "sv_pureLevel": "0", "sv_scriptHookAllowed": "false", ...
      },
      "version": 1903551649
    },
    "players": []
  }
}
```

Response (200): `{ "success": true }`. Repeats on a fixed cadence (~gated on a
non-empty `sv_licenseKeyToken`). The `fallbackData.info` block is essentially
the `/info.json` payload — BASTON already produces most of this.

## ④ Client policy (not captured — needs a connecting client)

The client reads `sv_licenseKeyToken` from `/info.json` and GETs
`https://policy-live.fivem.net/api/policy/<token>` → array of entitlement
strings (`onesync`, `onesync_plus`, …). On a free key this is empty, so the
client caps at 48 slots. Not captured here because no client connected during
the run; re-run with `connect 127.0.0.1:30120` to grab it.

## How BASTON implements it

`crates/baston-cfx/`, gated on `[license] mode = "cfx"`.

- **① `identity.rs`** — including the double slash, which is reproduced
  exactly: a normalised path is a different request, and this one is known to
  work.
- **④ `policy.rs`** — BASTON reads the same `policy-live` list the client will,
  and applies the ladder *before opening a listener*. One divergence: the
  client's `if/else` chain has no branch above 2048, so a server declaring more
  falls through to plain `"onesync"`. BASTON caps at 2048 rather than using the
  gap.
- **② ③ `listing.rs`** — transcribed from the public tree, cadence and backoff
  included. `fallbackData.info` is the same value `/info.json` serves, from one
  function, so the two cannot diverge.

A free (Pebble) key still yields listing visibility and nothing else: empty
`grants`, empty `policy`, 48 slots. That is the licence working, not a
limitation of this implementation.
