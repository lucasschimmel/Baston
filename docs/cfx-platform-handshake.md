# CFX platform handshake — captured spec

> ⚠️ **NON-retained approach (ToS risk / outside the legal boundary) — reference only.**
> This documents the *closed* CFX platform flow (licence validation → nucleus
> register → server-list ingress) as captured by MITM. Reproducing it from a
> non-FXServer binary means impersonating FXServer to CFX (spoofing) and is **not
> implemented in BASTON**. The **retained** path is to run the genuine, unmodified
> FXServer component as a sidecar — see [`licensing.md`](licensing.md). Do not turn
> this file into an implementation plan.


Reverse-engineered from a live FXServer 31623 boot (mitmproxy capture,
2026-07-04), filling the gap the engine-source mirror leaves open. This is the
sequence BASTON must reproduce to become a **registered** server (server list,
and — with a paid key — OneSync slots / policy features).

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

## Implementation notes for BASTON

1. At boot, if a `sv_licenseKey` is configured: GET ① → cache the four tokens.
2. Serve `sv_licenseKeyToken` in `/info.json` `vars`.
3. POST ② once → store the assigned `host`.
4. POST ③ on a timer with the current player/info snapshot.
5. Steps 2–4 are ~200 lines of reqwest. The blocker was ①'s exact shape —
   now known.
6. Product caveat: a free key yields listing only. Slots + clothing need a paid
   subscription, and doing this on a non-FXServer binary is a CFX-ToS risk.
