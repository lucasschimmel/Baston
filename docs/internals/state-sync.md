---
title: "State synchronisation"
description: "How entity state travels from one client to another — the two interest systems, the adaptive tick, and zone handoffs."
---

## Two interest systems, not one

This is the first thing to understand, because conflating them makes the rest
incomprehensible.

| | OneSync-NG (`interest_ng.rs`) | Phase C (`state_aggregator.rs`) |
| --- | --- | --- |
| Serves | **real FiveM clients** | binary / loadtest clients only |
| Runs in | the gateway's UDP task | the gateway's push loop |
| Selection | priority accumulator + byte budget | hard radius + distance bands |
| Hysteresis | yes | no |
| In a zone process | **never** | never |

When someone says "the interest budget", they mean the first. It does **not**
apply to the mesh state pipeline.

## The pipeline

### Client in

Two ingress paths, both producing a client state update:

- `__baston:stateUpdate`, a net event from the client shim — the real-client
  path, because client scripts can only `emitNet`, not send raw ENet packets
- `msgBastonState`, a bincode packet — loadtest clients only

Under OneSync, clients instead send `netClones` inside `msgRoute`, parsed by the
OneSync ingest.

### Gateway → zone (mesh only)

Subject `baston.zone.{id}.ingest`, core NATS, bincode `(source, update)`.

Forwarding is sharded across up to 8 tasks keyed by `source`, each with an 8192
deep queue. Overflow **drops** state updates (`mesh_forward_dropped_total`);
control messages instead defer with an awaited send, because losing a
disconnect is worse than delaying it.

### Zone → NATS

Subject `baston.zone.{id}.state`, JetStream, bincode `Vec<DirtyEntity>`, every
`sync_interval_ms` (16 ms ≈ 62 Hz). Nothing is published on an empty tick.

The dirty queue coalesces per entity: flags are OR-ed, and **`DELETED` wins and
is never resurrected** — a delete cannot be overtaken by a stale update.

#### The JetStream sizing trap

The stream is Memory storage with 5 seconds retention and
`max_messages_per_subject = 600` — which is `5 s × 120 Hz`, the maximum tick
rate.

Raise `tick_max_hz` above 120 without resizing the stream and the age limit
becomes decorative: messages are dropped **by count**, including `DELETED`
markers, leaving permanent ghost entities. `state_batches_lost_total` is the
signal.

### NATS → gateway

A durable **pull** consumer merges batches last-write-wins into the world state.
Sequence gaps are detected and counted. Every message is acked, including poison
ones — a message that cannot be parsed must not block the consumer forever.

**One gateway only.** Two gateways would share the durable, which
load-balances, so each would see a subset of batches and build a silently
fragmented world.

### Gateway → clients

Every `push_interval_ms` (50 ms ≈ 20 Hz), a spatial grid is built once and
shared, and per-client work is fanned out across up to 16 tasks.

## The OneSync-NG interest model

Real clients get a priority accumulator rather than fixed distance bands.

Every tick, for every candidate entity:

```
dist_term    = distance_weight × max(0, 1 − dist / aoi_radius)
closing_term = closing_weight  × max(0, −radial_velocity)     // XY only
priority    += dist_term + closing_term + staleness_weight
```

Z is deliberately excluded from the closing term — a helicopter overhead is not
approaching you in any sense that matters.

Then, per client per tick:

1. Filter — owner echo suppressed, routing buckets isolated.
2. **Hysteresis** — an entity already in scope is retained out to
   `aoi_radius + hysteresis_m`; a new one must be inside `aoi_radius`. This is
   what stops flicker at the edge. The spatial index must be built with the
   *outer* radius or hysteresis-held entities silently vanish.
3. Sort by descending priority, ties broken by object id for determinism.
4. Fill the budget — cost is `22 + data_len` bytes per record. **The first send
   of a tick always passes** even if it alone exceeds the budget; without that,
   one large entity starves a client forever.
5. On send, priority resets to zero. An entity created and unchanged sends
   nothing and *keeps* its priority.

Removals have their own budget, so a burst of departures cannot crowd out
arrivals. Unsent removals stay in the view and retry.

Reliability is NAK-driven: `gameStateNAck` triggers rollback, and `ack_frame` is
deliberately a no-op.

## The adaptive tick

The server lowers its own rate rather than falling behind.

**Measures**, once per tick: `work / period` as a fraction of the scheduled
period, smoothed by an EWMA (α = 0.125); queue pressure; whether the deadline
was missed. Utilisation is `None` before the first sample, so "unknown" is never
confused with "idle".

**Ladder:** 20, 30, 40, 60, 90, 120 Hz, clipped to `[tick_min_hz, tick_max_hz]`.

**Down, immediately**, on the first of: a missed deadline, queue pressure above
`tick_high_utilization`, or work overload. Target is
`current × tick_overload_backoff`, snapped **down** to a safe rate.

**Up, reluctantly**: utilisation *and* pressure below `tick_low_utilization` for
`tick_recovery_window` **consecutive** samples, then one step. Any bad sample
resets the counter.

Asymmetric on purpose: shed load instantly, recover slowly. On a rate change the
interval is rebuilt from now, so there is no catch-up burst.

## Ownership

Two models, depending on the path.

**Phase C / zone:** the nearest connected player owns an entity, re-evaluated
every `ownership_interval_secs` — the cadence doubles as flip-flop damping. The
scan runs on a blocking task so it cannot delay a heartbeat and get the zone
declared dead. Player peds are skipped; they always belong to their own client.

Two exceptions to the cadence: mounting a vehicle transfers it immediately, and
a client registering an unknown entity becomes its owner.

An update for an entity the sender does not own is rejected as `not_owner`.

**OneSync-NG:** the creator owns it. Duplicate creates with a different owner
or uniqifier are rejected; `first_owner` is preserved for
`NetworkGetFirstEntityOwner`.

Server-created entities **survive their simulator**: on disconnect they fall to
owner 0 and are reassigned to the nearest player at the top of the next tick.
Client-owned entities are destroyed and their ids freed.

## Zone handoff

The careful part.

### Detect

Every `boundary_scan_interval_ms` (500 ms), a zone looks for players within
`boundary_margin` (300 m) of an edge **and moving outward** at ≥ 0.5 m/s.
Idlers and players moving away are ignored.

The predicted crossing point is computed with a **1 second overshoot**, so it
lands inside the neighbour rather than on the shared border. Bounds are
min-inclusive and max-exclusive, so zones tile with no overlap.

### Prepare

The zone asks the gateway to resolve the target zone and prepare it. The target
stores a *pending ghost* — **`playerJoining` does not fire yet**.

The snapshot carries identity, position, health, the ped as a full entity, every
entity the player owns, and each resource's zone-transfer state, collected
through every resource isolate.

### Commit

The atomic point. The gateway flips the routing table under a global mutex with
**no await or IO held** — designed for well under a millisecond, and measured by
`handoff_routing_lock_held_us`.

### Hold

For 50 ms after the commit, updates for that player are buffered and then
replayed **in order** to the new zone. Without it, updates in flight during the
switch would be delivered to the old zone and lost.

### Activate

The old zone tells the new one to activate. State is restored **before** scripts
observe the join — entity ids preserved, and the anti-cheat baseline seeded at
the handed-off position so the first update is not read as a teleport. Then
`playerJoining` fires with the transferred script state.

**On failure, the old zone rolls the routing back** and keeps the player. A
rollback that itself fails logs that routing is now inconsistent and increments
`handoff_rollback_failures_total` — that metric above zero needs a human.

### Anti ping-pong

A cooldown (`handoff_cooldown_secs`, 5 s) is applied on both cancellation and
success. Preparations older than twice the prepare timeout are swept; ghosts
expire after 30 seconds.

Ownerless entities are handed over separately, with zone lookups batched into
64 m grid cells so a crowded border does not flood the gateway.

## Zone failure recovery

Zones heartbeat every 5 s; the gateway scans every 5 s and evicts a zone silent
for `zone_timeout_secs` (15 s — three missed heartbeats).

Recovery plans the whole burst at once rather than per player: survivors sorted
deterministically, each orphan assigned to the least-loaded survivor **that
still has room**, incrementing as it goes. Per-player "least loaded" lookups
were wrong — with 5-second-stale counts, everyone piles onto one zone.

Players who fit are committed in **one** acquisition of the routing lock. Those
who do not are kicked with a reason.

**The data loss is documented and real:** the dead zone's snapshot died with it,
so recovery carries only gateway-held identity. Coordinates, health, owned
entities and script state are gone; the ped respawns from the client's next
update.

`drain_zone` — the planned-shutdown path — has the same limitation. It moves
routing, not state.

## Known gaps

- **`find_zones_in_aabb` is dead code.** It was built for a cross-zone AoI that
  the architecture made unnecessary — client visibility never passes through the
  zones (see [multi-zone](../server/multi-zone.md)). It has no callers outside
  its own tests and should be removed rather than wired.
- **Zone failure and drain lose player state**, as above.
- **Multi-gateway HA is unsupported** — the shared durable fragments the world.
- **`ack_frame` is a deliberate no-op**; NG relies on NAK.
- Ghost expiry (30 s), liveness scan (5 s), the global player list (2 s) and
  routing GC (10 s) are **hardcoded**, not configurable.

## Tuning

| Symptom | Look at |
| --- | --- |
| `onesync_tick_hz` below default | `onesync_tick_utilization`, then per-resource cost |
| Entities pop in late | `interest_budget_bytes`, `interest_distance_weight` |
| Entities flicker at the edge | `interest_hysteresis_m` |
| Handoffs complete after the crossing | `boundary_margin`, `boundary_scan_interval_ms` |
| Players ping-pong on a border | `handoff_cooldown_secs` |
| `state_batches_lost_total` climbing | the JetStream sizing trap above |

## Where the code is

| Area | Path |
| --- | --- |
| Entity store, dirty tracking | `crates/baston-zone/src/entity_manager.rs` |
| Ingest, ownership, plausibility | `crates/baston-zone/src/state_ingest.rs` |
| Dirty flush to NATS | `crates/baston-zone/src/state_sync.rs` |
| OneSync-NG game state | `crates/baston-zone/src/onesync/` |
| Interest management | `crates/baston-zone/src/interest_ng.rs` |
| Adaptive tick controller | `crates/baston-zone/src/adaptive_tick.rs` |
| Boundary detection, handoff | `crates/baston-zone/src/boundary_*.rs`, `handoff_manager.rs` |
| Routing table, recovery | `crates/baston-gateway/src/connection_router.rs`, `mesh.rs` |
| Forwarding and the hold | `crates/baston-gateway/src/mesh_forward.rs` |
| Snapshot push | `crates/baston-gateway/src/state_aggregator.rs` |

## Next

- [The wire protocol](protocol.md)
- [Multi-zone servers](../server/multi-zone.md)
