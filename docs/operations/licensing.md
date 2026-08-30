---
title: "CFX licensing"
description: "What BASTON does and does not do with your CFX server key."
---

**BASTON does not authenticate anything with CFX.** It holds your key, it can
refuse to boot without one, and that is the whole of it. If you are here to
find out how to get your server into the FiveM server list, the short answer
is that you cannot — see [going public](../server/going-public.md).

This page exists so nobody has to guess at that. It used to describe an
FXServer sidecar that authenticated the key on BASTON's behalf; that was
removed in [ADR-003](../adr/003-remove-the-fxserver-sidecar.md).

## What you get

```toml
[license]
# "off"  : no check at all. Warns every boot.
# "gate" : require a well-formed sv_license_key. Shape only, never validity.
mode = "gate"
sv_license_key = "cfxk_your_real_key_here"
```

`gate` checks that the key is non-empty, has no whitespace, is at least 20
characters, and is not a placeholder containing `REPLACE_ME`. A key that fails
any of those stops the boot with a message naming the fix.

That is a **typo check**, not a validation. A well-formed key that CFX revoked
this morning passes `gate` exactly like a valid one, because nothing here asks
CFX anything.

Both modes log a warning at every boot saying so. That warning is deliberate
and is not going away while the statement is true.

## What you do not get

| | |
| --- | --- |
| Public server-list presence | No. Nothing registers or heartbeats. |
| Slot entitlements from your key | No. `max_players` is what you configured. |
| Paid policy features (extra streaming pools, custom clothing) | No. The client reads these from a token BASTON never obtains. |
| `sv_licenseKeyToken` in `/info.json` | Absent. Publishing an empty or invented one would tell a client the server is licensed when it is not. |
| Asset Escrow (`.fxap`) resources | No. A resource with encrypted scripts is refused at load. |

## Then why configure a key at all?

Two reasons, both modest:

1. **`gate` catches the empty key before you go live** rather than three days
   later. It is the same value FiveM operators already keep in their config, in
   the same place, so nothing is lost by having it right.
2. **One place to read it from** when an authenticated integration exists. When
   that lands, it reads `[license] sv_license_key` and your config does not
   change.

If neither appeals, leave `mode = "off"`. Nothing else in BASTON reads the key.

## `mode = "verified"` is rejected

A config carrying `mode = "verified"` fails to parse:

```
[license] invalid configuration: unknown variant `verified`
```

That is intentional. `verified` used to run the FXServer sidecar; treating it
as `off` would boot the server unauthenticated while its operator believed CFX
had validated the key. It stops instead.

Change it to `"gate"` (same key requirement, no sidecar) or `"off"`.

## Where this is going

Authenticating a key from something that is not FXServer is a legal question
before it is a technical one, and the technical half is
[documented](../internals/cfx-platform-handshake.md) rather than implemented.
Whatever lands will be restrictive by construction and fail-closed —
[ADR-003](../adr/003-remove-the-fxserver-sidecar.md) records the constraints so
they do not have to be rediscovered.

## Next

- [Going public](../server/going-public.md) — what running a public BASTON
  server actually looks like today
- [ADR-003](../adr/003-remove-the-fxserver-sidecar.md) — why the sidecar was
  removed
