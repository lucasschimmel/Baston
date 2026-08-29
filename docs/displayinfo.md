# `displayinfo` — the in-game debug overlay

A server-assembled technical readout drawn over the game: mesh topology,
OneSync state, and the network link, as **the server** measures them.

It is modelled on Star Citizen's `r_DisplayInfo`, which is the only shipping
reference for what a meshed server should show an operator — the zone chain the
player sits in, the server's own tick health, the link — rather than the
client-side frame counters a game engine usually surfaces.

## What it is not

It is not a resource. Nothing is installed, nothing appears in `resources/`, and
an operator cannot start, stop, edit or replace it.

The reason it needs client-side code at all is a hard constraint of the
platform: BASTON is the server, and the FiveM client only draws what a client
script tells it to. No server — BASTON or FXServer — can paint the game window
on its own. So the renderer ships **inside the server binary**
(`crates/baston-gateway/assets/displayinfo/client.js`), is packed into an RPF in
memory on first request, and is advertised straight into `getConfiguration`
under the reserved name `baston_displayinfo`. It is version-locked to the
running server, and a resource directory that takes the same name is reported
and ignored rather than allowed to substitute its own code.

Everything with substance is on the server side:

- the snapshot is assembled in the ENet task, the only place that holds the
  peers, the OneSync game state, the tick controller and the player directory at
  the same instant;
- the subscription request is answered by the transport itself, before script
  dispatch, so the overlay works on a server running zero resources — and no
  resource can subscribe a player the operator did not clear;
- the client script calls **no** game native to measure anything. Every number
  is the server's own reading. An overlay that showed the client its own ping
  would agree with the client even when the server disagrees, and that
  disagreement is exactly what an operator is looking for.

## Enabling it

```toml
[debug]
display_info = "allowlist"
allow = ["license:0123456789abcdef0123456789abcdef01234567"]
refresh_hz = 5
```

| `display_info` | Who can turn it on |
| --- | --- |
| `off` (default) | nobody; the client is never even sent the renderer |
| `allowlist` | players with an identifier in `allow` (exact, case-insensitive) |
| `everyone` | anyone connected — development servers only |

`allowlist` with an empty `allow` is rejected at startup: it fails silently
otherwise, and a half-finished edit looks identical to a working one.

Identifiers are matched as `GetPlayerIdentifiers` reports them — `license:…`,
`steam:…`, `ip:…`.

## Using it

In-game:

```
/displayinfo        cycle off -> basic -> onesync -> mesh -> off
/displayinfo 2      jump straight to a level
/displayinfo 0      off
```

| Level | Shows |
| --- | --- |
| 1 | server identity and health, network link, player position |
| 2 | + OneSync: entity counts, scope, frame lag, object-id pool, routing bucket |
| 3 | + mesh: current zone, distance to its edge, every neighbour and whether a handoff into it is armed |

Refusals are reported on screen with their reason, so a mistyped identifier does
not require reading server logs.

## Reading it

**Server line.** `tick 20Hz 0.57ms 14% util` is the OneSync outbound cadence and
how much of its scheduled period the last ticks consumed. Utilisation turns
amber past 70% and red past 90%; the adaptive controller starts shedding rate
around there, so a falling `tick_hz` with high utilisation is the server telling
you it is out of headroom. `OneSync off` means there is no server sync tick at
all, which is different from a tick doing no work.

**Net line.** Round-trip time, its variance, and ENet's smoothed loss estimate,
all measured server-side. Throughput is sampled between snapshots; the first
reading after subscribing is zero on purpose, because the peer's byte counters
started accumulating at connect, not at subscribe.

**OneSync block.** `scope` is how many entities are cloned to *this* client, and
`lag` is the distance between the server's frame index and the one this client
last acknowledged — a widening gap is a client falling behind the clone stream.
The object-id pool is split into *used* (backing a live entity) and *leased*
(handed to a client that has not created on it yet): exhaustion is silent
otherwise, clients simply stop being able to create entities, and the split is
what distinguishes a full world from clients hoarding ids they never spend.

**Mesh block.** The zone that owns the player, its bounds, and the distance to
the nearest edge, against `[meshing].boundary_margin`. Inside that band the
gateway is already preparing a handoff, and the line says `HANDOFF ARMED`.
Neighbours are listed nearest-first with a compass bearing; a `>` marks the ones
close enough to be warmed up. `unrouted` means no zone owns the player, which
happens between a zone eviction and the recovery reroute.

A snapshot older than two seconds is drawn with an explicit `STALE` age rather
than left in place, so a frozen overlay is never read as a healthy one.

## Cost

One reliable client event per subscriber per tick, at `refresh_hz` (default 5).
Zone topology is read once per tick and shared across subscribers, and the world
is walked once for all of them rather than once each. With no subscribers the
tick returns immediately, and with `display_info = "off"` the timer is never
armed at all.
