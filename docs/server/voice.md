---
title: "Voice"
description: "The embedded Mumble-compatible voice server: setting it up, what works, and what does not yet."
---

BASTON ships a voice server that the stock FiveM client connects to with no
third-party resource. "Mumble at the wire, custom brain": it speaks the Mumble
protocol the client already knows, with its own routing core.

## What works, and what does not

Read this before you plan around it.

**Works**

- Players connect and hear each other, with no extra resource installed
- Channels — `MumbleCreateChannel`, `MumbleDoesChannelExist`
- Server-forced muting — `MumbleSetPlayerMuted`, and a muted speaker's packets
  are dropped at routing, not just hidden
- A UDP path with an automatic TLS-tunnel fallback when UDP is blocked

**Does not**

- **Proximity culling is not implemented.** Everyone in a speaker's channel
  hears them, regardless of distance. This matches stock `pma-voice` behaviour,
  but it is not what most roleplay servers want.
- **`NetworkSetVoiceProximityOverrideForPlayer` stores a position nothing
  reads.** Setting it changes nothing about who hears whom.
- **No authentication.** A client authenticates by claiming a net id. There is
  no password or token check — treat the voice port as unauthenticated.
- **No metrics.** The voice crate emits nothing to Prometheus; logs are your
  only signal.

If you need real proximity voice today, you need distinct channels and a
resource that moves players between them.

## Setting it up

```toml
[modules]
enable = ["voice"]

[voice]
port = 30121
external_address = "203.0.113.10"
```

Then open **TCP and UDP on 30121**. Both share the port number: TLS control on
TCP, voice on UDP, as Mumble does.

### `external_address` is the setting people miss

Clients learn where the voice server is from convars BASTON pushes right after
the game connection is established. If `external_address` is empty, **that value
is never sent and no client ever connects** — while the server runs perfectly
and the port sits open.

It must be the address players actually reach you at:

| Situation | Value |
| --- | --- |
| Local testing | `127.0.0.1` |
| A LAN server | the machine's LAN address |
| A public server | your public IP or hostname |

Not `0.0.0.0`. BASTON logs a warning at boot when it is empty.

Note the voice listener always binds `0.0.0.0` — it does not follow
`server.bind_address`.

## TLS

A **fresh self-signed certificate is generated on every boot** and never
persisted. This matches FXServer's embedded voice server, and the FiveM client
does not validate it. There is no option to supply your own certificate, and
nothing to configure.

## Controlling it from a resource

```javascript
MumbleCreateChannel(1);
if (MumbleDoesChannelExist(1)) { /* … */ }

MumbleSetPlayerMuted(source, true);
const muted = MumbleIsPlayerMuted(source);
```

```lua
MumbleCreateChannel(1)
MumbleSetPlayerMuted(source, true)
```

Channels are permanent once created; creating an existing one is a no-op.

The proximity-override natives exist and store values, but nothing consumes
them — do not build on them yet:

```javascript
NetworkSetVoiceProximityOverrideForPlayer(source, x, y, z);  // stored, unused
NetworkGetVoiceProximityOverrideForPlayer(source);           // reads it back
NetworkClearVoiceProximityOverrideForPlayer(source);
```

When voice is off, these natives return neutral values rather than failing, so a
resource can feature-detect safely.

## When it does not work

| Symptom | Cause |
| --- | --- |
| Nobody connects, server looks fine | `external_address` empty, or wrong |
| Works locally, not remotely | UDP not open on the voice port |
| Choppy audio | UDP blocked — traffic fell back to the TLS tunnel |
| Everyone hears everyone | **Expected.** Proximity culling is not implemented. |
| Distance overrides do nothing | **Expected.** They are stored and unread. |
| Server refuses to start | `voice.port` equals `server.port` |

Sessions are torn down both on voice disconnect and on game disconnect, so a
player who crashes out does not linger.

## Security

The voice port is **unauthenticated**: a client identifies itself by claiming a
net id, and nothing verifies the claim. Permissions are answered permissively
with no ACLs, matching FiveM's embedded server.

Practically: someone who can reach the voice port can claim to be a connected
player. Do not treat voice presence as proof of identity, and do not build
gameplay authorisation on it.

## Next

- [Going public](going-public.md)
- [Configuration reference](../reference/configuration.md#voice--the-embedded-mumble-server)
