---
title: "Multi-zone servers"
description: "Splitting the map across zone processes: what it buys you, what it costs, and what is not implemented yet."
---

A single BASTON process runs a whole map. Multi-zone splits it across several
processes, each owning a rectangle, with one gateway in front.

**Read the limitations before you build on this.** Federation works, and the
handoff protocol is careful, but what happens when a zone dies changes what you
can build.

## Should you?

**No, if** you are running a server for friends. One process handles that with
room to spare, and single-process has none of the caveats below.

**Maybe, if** you are CPU-bound on script or entity work — check
`onesync_tick_utilization` before assuming it. Multi-zone splits work by
*geography*, so it only helps when your load is geographically spread.

**Not yet, if** you cannot afford to lose player state when a zone process
dies. See the first limitation.

## Server-created entities

`CreateVehicle`, `CreatePed` and `CreateObject` work in a zone process, and the
entity is a real networked one. There is nothing to configure.

It is worth knowing how, because the constraint shapes the failure modes. The
world clients talk to lives in the **gateway**, not in the zones, so a zone has
to cross a process boundary to create anything — while `CreateVehicle` still has
to return its handle on the same line, with no room for a round trip.

So the two halves are split:

- **Ids are leased ahead.** At startup, and again whenever it runs low, a zone
  asks the gateway for a block of network ids. It then mints from that block
  locally, with no I/O, so the native answers immediately. Blocks are carved
  from the gateway's own allocator, which makes them exclusive by construction:
  two zones cannot mint the same id, and a zone cannot collide with a client.
- **Spawns are shipped asynchronously.** A single task per zone batches and
  sends them, so a `Despawn` can never overtake the `Spawn` it undoes.

What follows from that:

- **A spawn can be lost if the gateway is unreachable.** The batch is retried
  three times, then dropped with an error naming how many entities the world is
  missing. Watch `zone_world_commands_dropped_total`.
- **Entities outlive the zone that made them.** They live in the gateway's
  world, so a zone restart does not remove them — but the script state that
  referenced them is gone with the zone.
- **The id space is 8192 wide and shared with client leases.** Each zone holds a
  256-id block at a time. With many zones this is worth watching:
  `network_ids_leased_total` and `network_id_lease_failures_total`.
- **With `onesync off` there is no authoritative world at all**, so the natives
  return a server-local record instead — the same as a single-process server in
  that mode. The zone logs it once at boot.

When a zone genuinely cannot get an id, the native returns **0**, the invalid
handle, rather than a plausible number for an entity that will never exist.

## Drawing the split

By default each zone declares one rectangle and the Gateway takes its word for
it. Nothing checks that the rectangles cover the plane, and a gap leaves any
player standing in it without an owner.

A [zone map](zone-map.md) replaces that: one ordered file of regions on the
Gateway, with rectangles, circles and traced outlines, handed to each zone at
registration. Regions may overlap — the first one wins — which is what lets an
arena be carved out of the city around it, and a mandatory catch-all makes a
gap impossible.

## Known limitations

### Zone failure loses player state

When a zone dies, its players are rerouted to survivors, but the dead zone's
snapshot died with it. Only **name and identifiers** survive. Coordinates,
health, armour, owned vehicles and script state are gone; the ped respawns from
the client's next update.

Recovery keeps players connected. It does not keep them intact.

### `drain_zone` moves routing, not state

The admin drain endpoint reroutes players away from a zone for a planned
shutdown. It does not migrate their state either — same consequence as a
failure, at a time you chose.

### One gateway only

Two gateways cannot share a zone mesh today. They would share a NATS durable
consumer, which load-balances, so each would see a *subset* of state batches and
build a silently fragmented world. The gateway is your single point of failure.

## How it fits together

```
FiveM client ──▶ baston-gateway            :30120 game · :50050 gRPC · :8080 admin
                   │
                   ├── gRPC ──▶ zone registry, handoff prepare/confirm
                   └── NATS ──▶ ┌─ zone-a  (-4000,-4000 → 0,4000)
                                └─ zone-b  (0,-4000 → 4000,4000)
```

- **The gateway** is the only process a client talks to. It owns the FiveM
  protocol, the routing table, OneSync, and the interest calculation for real
  clients.
- **A zone** runs the resources for its rectangle and holds the server-side
  entity state for it. Clients never connect to it. It cannot start without
  NATS.

### Zones do not decide what players see

Worth being explicit, because it is the opposite of what a zoned architecture
usually implies: **entity visibility never passes through the zones.**

A FiveM client syncs one of two ways, and both terminate at the gateway:

- `onesync off` — the gateway relays each client's sync blob to the target
  netId, filtered only by routing bucket. The GTA netcode on each client does
  its own scoping; the server is a router. This mirrors FXServer's
  `RoutingPacketHandler`, which has no spatial test either.
- `onesync on` — the gateway runs a single global OneSync game state, fed
  directly by the clients' clone streams.

Neither consults the routing table. Two players either side of a border see each
other exactly as they would on a single-process server, and there is no border
effect on visibility to design around.

The per-client area-of-interest filter in `StateAggregator` is not a
counterexample: it serves only binary-protocol clients (the loadtest harness),
and it merges every zone's state into one world before filtering, so it is not
per-zone either.

Resources run **in every zone process**, once per zone. A resource holding state
in a variable holds a different copy per zone. State that must be shared belongs
in the database, KVP, or state bags.

## Setting it up

The compose file is the reference and the fastest way to see it work:

```bash
docker compose -f deploy/docker/docker-compose.yml up
```

That starts a gateway, two zones, NATS, Prometheus and Grafana.

By hand, the gateway:

```toml
[meshing]
enabled = true
gateway_grpc_addr = "0.0.0.0:50050"
admin_port = 8080

[nats]
url = "nats://127.0.0.1:4222"
```

And each zone — same binary, different identity:

```bash
ZONE_ID=zone-a \
ZONE_BOUNDS="-4000,-4000,0,4000" \
GATEWAY_GRPC=127.0.0.1:50050 \
ZONE_GRPC_ADDR=0.0.0.0:50051 \
ZONE_PUBLIC_GRPC_ADDR=127.0.0.1:50051 \
NATS_URL=nats://127.0.0.1:4222 \
  baston-zone
```

`ZONE_BOUNDS` is `x_min,y_min,x_max,y_max`. Bounds are **min-inclusive,
max-exclusive**, so adjacent zones tile with no overlap and no gap. Getting this
wrong is the most common setup mistake — see
[Zone configuration](zone-config.md).

## What happens when a player crosses

Worth understanding, because the tuning knobs map onto it directly.

1. **Detect.** Every `boundary_scan_interval_ms` (500 ms), each zone checks
   whether a player is within `boundary_margin` (300 m) of an edge *and moving
   outward* at ≥ 0.5 m/s. Idling near a border triggers nothing.
2. **Prepare.** The zone asks the gateway which zone owns the predicted crossing
   point. The gateway resolves it and asks that zone to prepare, handing over a
   snapshot: identity, position, health, the ped, owned entities, and each
   resource's zone-transfer state.
3. **Commit.** On the actual crossing, the gateway flips the routing table under
   a lock held for well under a millisecond. This is the atomic point.
4. **Hold.** For 50 ms after the commit, client updates for that player are
   buffered, then replayed in order to the new zone — so nothing is lost
   mid-switch.
5. **Activate.** The old zone tells the new one to activate the player. State is
   restored *before* scripts see the join, then `playerJoining` fires with the
   transferred script state.

If activation fails, the old zone rolls the routing back and keeps the player.

A `handoff_cooldown_secs` (5 s) stops a player walking along a border from
ping-ponging.

For a resource to carry state across, it must register a collector — see
[Zone transfer state](../scripting/events.md#zone-transfer-state). A resource
that does not simply starts fresh in the new zone.

## Ownerless entities

Vehicles and NPCs with no local owner are handed to the zone that contains them,
independently of players. Zone lookups are batched into 64 m grid cells so a
crowded border does not flood the gateway with resolution requests.

## Tuning

| Setting | Default | Raise it when | Lower it when |
| --- | --- | --- | --- |
| `boundary_margin` | 300 m | Handoffs complete after the crossing (fast vehicles) | Handoffs prepare far too early |
| `boundary_scan_interval_ms` | 500 ms | — | Fast vehicles cross without preparation |
| `handoff_cooldown_secs` | 5 s | Players ping-pong on a border | Legitimate quick re-crossings are blocked |
| `zone_timeout_secs` | 15 s | Zones are falsely declared dead under load | You want faster failure detection |
| `heartbeat_interval_secs` | 5 s | — | You lowered `zone_timeout_secs` |

`zone_timeout_secs` should stay at roughly three heartbeats.

## Watching it

The mesh dashboard is in `deploy/monitoring/dashboards/baston-mesh.json`. The
signals that matter:

| Metric | Watch for |
| --- | --- |
| `handoffs_committed_total` vs `entity_handoffs_total` | A widening gap is a boundary problem, not load. |
| `handoff_total_duration_ms` | Should be tens of milliseconds. |
| `handoff_prepare_timeouts_total{zone}` | One zone is not answering — check its health. |
| `handoff_rollback_failures_total` | **Routing is now inconsistent.** Investigate immediately. |
| `zone_failures_total{zone}` | A zone was declared dead. |
| `mesh_forward_dropped_total` | Updates dropped under backpressure. |
| `state_batches_lost_total` | The gateway missed state from a zone — see below. |
| `zone_world_commands_dropped_total` | Server-created entities that never reached the world. |
| `network_id_lease_failures_total` | The id space is running out; zones cannot create entities. |

### A JetStream trap worth knowing

The stream carrying zone state is sized for **5 seconds of retention at up to
120 Hz**. Raise `tick_max_hz` above 120 without resizing the stream and the age
limit becomes decorative: messages are dropped by count, including entity
*deletion* markers, which leaves permanent ghost entities in the world.

If `state_batches_lost_total` is climbing, check this first.

## Admin operations

With the `admin-api` module on:

```bash
# What zones exist and what they hold
curl -H "Authorization: Bearer $TOKEN" localhost:8080/api/v1/zones

# Drain a zone before a planned restart (routing only — state is lost)
curl -X POST -H "Authorization: Bearer $TOKEN" \
  localhost:8080/api/v1/zones/zone-a/drain
```

## Next

- [Zone configuration](zone-config.md) — laying out bounds correctly
- [State synchronisation](../internals/state-sync.md) — how it actually works
- [Monitoring](monitoring.md)
