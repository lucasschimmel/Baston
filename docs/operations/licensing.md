---
title: "CFX licensing"
description: "What your CFX server key does, and the one choice it forces."
---

BASTON can authenticate your CFX server key, read what it grants, and appear in
the FiveM server list — without FXServer anywhere. It does this by performing
the same exchanges FXServer performs, identifying itself as BASTON. See
[ADR-004](../adr/004-cfx-identity-without-fxserver.md).

## The choice, first

`[license] mode = "cfx"` couples two things, and there is no configuration that
separates them:

| | `off` | `cfx` |
| --- | --- | --- |
| Appears in the FiveM server list | no | **yes** |
| Slot count | whatever you configure | **capped to what your key grants** |
| Needs a CFX key | no | yes |

That is not a BASTON decision. Publishing your licence token is what makes the
FiveM client look your entitlements up; a server that publishes nothing has
nothing looked up. You get discovery *or* unbounded slots.

**Running 500, 2000, 3000 players?** You want `off`. No tier sells you that,
and the server list is not your acquisition channel. Hand out
`connect your.host:30120`.

**Running 32 players with friends, and you want people to find you?** You want
`cfx`. The cap will never bind on you.

## `mode = "cfx"`

```toml
[license]
mode = "cfx"
sv_license_key = "cfxk_your_real_key"

[listing]
enabled = true
ip_override = "203.0.113.10"    # the public address players connect to
```

At boot, before any listener opens, BASTON:

1. validates the key with `portal-api.cfx.re`;
2. reads the entitlements from `policy-live.fivem.net` — the same endpoint the
   client checks, so the two cannot disagree;
3. **lowers `max_players`** if it exceeds what the key grants, with a warning
   naming both numbers;
4. publishes `sv_licenseKeyToken` in `/info.json`;
5. registers with CFX and starts the 3-minute server-list heartbeat.

A failure at any step stops the boot. A server that asked to be authenticated
does not start unauthenticated.

### The slot ladder

| Your policy grants | Ceiling |
| --- | --- |
| nothing (free key) | 48 |
| `onesync` | 64 |
| `onesync_plus` or `onesync_medium` | 128 |
| `onesync_big` | 2048 |

The ceiling only applies with OneSync on — the client's check is gated on it,
so BASTON does not invent a restriction where the client imposes none.

A licence can only **lower** your configured count. Set `max_players` to what
you want and let the licence reduce it if it must; it will never raise it.

### What BASTON tells CFX it is

```
User-Agent: BASTON/0.1.0-alpha (+https://github.com/lucasschimmel/Baston)
```

Never FXServer's. If CFX ever declines a self-identified third-party client,
you will get an error saying exactly that, and the answer is `mode = "off"` —
not a forged agent.

## `mode = "gate"` and `mode = "off"`

`gate` checks the key's *shape* — non-empty, no whitespace, at least 20
characters, not a placeholder — and nothing else. It contacts nobody. It is a
typo check for operators who keep a key in config but do not want the
authenticated path.

`off` does nothing at all. Both warn at every boot that no entitlement is
enforced.

## `mode = "verified"` is rejected

A config carrying it fails to parse:

```
[license] invalid configuration: unknown variant `verified`
```

It ran the FXServer sidecar, which no longer exists
([ADR-003](../adr/003-remove-the-fxserver-sidecar.md)). Treating it as `off`
would boot the server unauthenticated while its operator believed CFX had
validated the key, so it stops instead. Use `"cfx"`, which is what `verified`
was trying to be.

## What is still not possible

**Escrowed (`.fxap`) resources.** Decryption lives entirely inside
`svadhesive`, and there is no token that opens it from outside. A resource with
CFX-encrypted scripts is refused at load. Ask its author for an unescrowed
build.

## Next

- [Going public](../server/going-public.md) — the full checklist before you
  open the port
- [ADR-004](../adr/004-cfx-identity-without-fxserver.md) — how this works and
  why it is built the way it is
