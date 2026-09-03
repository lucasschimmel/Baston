---
title: "Natives"
description: "How natives are dispatched, which families are implemented, and how to find out what your resource is missing."
---

## One implementation, both languages

A native is implemented once, in Rust, and both runtimes call the same code. A
native behaves identically whether a JavaScript or a Lua resource invoked it.

What differs is how you reach it.

```javascript
// JavaScript: every native has a generated global.
const name = GetPlayerName(source);
```

```lua
-- Lua: an unknown capitalised global resolves to a native on first use,
-- converting camelCase to the SCREAMING_SNAKE name, and is then memoised.
local coords = GetEntityCoords(entity)

-- Or explicitly:
local coords = Citizen.InvokeNative("GET_ENTITY_COORDS", entity)
```

Note that Lua's `Citizen.InvokeNative` takes the **name**, not a hash.

## Four families

| Family | Count | What it is |
| --- | --- | --- |
| Shared and server natives | ~127 | KVP, state bags, players, entities, resources, voice |
| World-mirror natives | ~71 | Read-only vehicle and ped state from the authoritative world |
| Context (RPC) natives | ~69 | Executed **on a client** and awaited |
| Everything else | — | Neutral value, warned once, counted |

### World-mirror natives

Getters answered from BASTON's mirror of the networked world:
`GetVehicleEngineHealth`, `GetVehicleDoorLockStatus`, `IsPedInAnyVehicle`,
`GetPedArmour`, `GetVehicleNumberPlateText`, and so on.

They answer from what clients have reported. An entity BASTON has never seen
reports "no such entity" rather than a fabricated value — which is the right
answer, but means a freshly created entity may briefly read as absent.

### Context natives are executed on a client

Some natives cannot run on a server at all — they need the game engine. BASTON
routes those to the client that owns the entity, runs them there, and returns
the result.

This is not a FiveM protocol feature; there is no native-call packet. BASTON
tunnels them over net events to its client shim.

Consequences you should design around:

- **They need an owner.** No owning client, no result —
  `script_native_rpc_no_owner_total` counts it, and it is *expected* on a
  non-OneSync server.
- **They cost a network round trip**, with a 1 second timeout.
- **While a resource awaits one, that resource services nothing else.**

```javascript
const ped = await InvokeNativeOnClient(source, "0x43A66C31C68491C0", [1], true);
```

```lua
CreateThread(function()
    local ped = InvokeNativeOnClient(source, "0x43A66C31C68491C0", { 1 }, true)
end)
```

Fire-and-forget (`expectsReturn = false`) is synchronous and cheap. An awaited
call in Lua must be inside `Citizen.CreateThread`.

## Finding out what you are missing

**This is the important part of this page.** A native BASTON does not implement
does not throw. It returns a type-appropriate neutral value — `0`, `false`,
`""`, `[]`, null — logs a warning once per native name, and increments a
counter.

```bash
curl -s localhost:9090/metrics | grep script_native_unimplemented_total
```

```
script_native_unimplemented_total{native="GET_VEHICLE_NUMBER_PLATE_TEXT"} 14
script_native_unimplemented_total{native="SET_PED_ARMOUR"} 3
```

Run your resource, exercise it, read that list. It is the precise gap between
your resource and a working one.

The curated coverage list is in
[Native coverage](../reference/natives-gap.md).

## Natives that exist but do nothing

Worse than missing, because they look implemented. Do not build on these:

| Native | Behaviour |
| --- | --- |
| `CancelEvent`, `WasEventCanceled` | **Event cancellation does not work.** |
| `IsPlayerAceAllowed`, `IsAceAllowed`, `IsPrincipalAceAllowed` | **Always false.** There is no ACE system. |
| `ExecuteCommand` | Warns, does nothing. |
| `TempBanPlayer` | Drops the player, **keeps no ban list**. |
| `ProfilerEnterScope`, `ProfilerExitScope`, `ProfilerIsRecording` | No-ops. Script scopes never appear in a trace. |
| `GetRegisteredCommands`, `GetResourceCommands`, `GetGamePool`, `GetEntitiesInRadius` | Return `[]`. |
| `GetGameBuildNumber`, `GetInstanceId` | Return `0`. |
| `RegisterConsoleListener`, `ScheduleResourceTick` | No-ops. |
| Commerce natives (`DoesPlayerOwnSku`, …) | Permanent `false` / null. |
| Latent client events | Sent whole, **unpaced** — the point of "latent" is lost. |

## Language-specific surfaces

Some things are not natives at all — they are runtime bindings, and they exist
in only one language.

**JavaScript only.** Implemented as V8 ops, so a Lua resource calling one gets
the neutral value:

`GetPlayerName` · `GetPlayers` · `DoesPlayerExist` · `GetPlayerIdentifier*` ·
`GetPlayerPing` · `GetPlayerEndpoint` · `GetPlayerGuid` · `GetPlayerToken*` ·
`GetConvar*` · `SetConvar` · `GetCurrentResourceName` · `GetResourceState` ·
`GetResourcePath` · `GetResourceMetadata` · `GetNumResources` ·
`GetResourceByFindIndex` · `LoadResourceFile` · `SaveResourceFile` ·
`GetGameTimer` · `SetHttpHandler` · `PerformHttpRequest`

In Lua, use `GetInvokingResource()` for the current resource name. For the
others, put the lookup in a small JavaScript resource and ask it over an event.

**Lua only.** `Citizen.CreateThread`, `Citizen.Wait`, `Citizen.SetTimeout` —
JavaScript has no working timers at all.

## Entity natives and the world

Server-side entity manipulation works through BASTON's authoritative world:

```javascript
const id     = CreateVehicle(model, x, y, z, heading, true, false);
const coords = GetEntityCoords(id);
SetEntityCoords(id, x, y, z);
SetEntityHealth(id, 200);
DeleteEntity(id);
```

Creation is asynchronous in effect: the network id comes back immediately — a
script needs its handle at once — and the entity is created on the world's next
sync tick.

Routing buckets work: `SetPlayerRoutingBucket`,
`SetEntityRoutingBucket`, `SetRoutingBucketEntityLockdownMode`,
`SetRoutingBucketPopulationEnabled`.

## Voice natives

With the `voice` module on: `MumbleCreateChannel`, `MumbleDoesChannelExist`,
`MumbleSetPlayerMuted`, `MumbleIsPlayerMuted`, and the proximity-override
family.

**Proximity culling is not implemented.** Everyone in a speaker's channel hears
them, and `NetworkSetVoiceProximityOverrideForPlayer` stores a position that
nothing currently reads. Channels and muting work; distance does not. See
[Voice](../server/voice.md).

## Adding a native

If you need one that is missing, it is usually a small, self-contained change —
and because the natives layer is engine-neutral, implementing it once gives it
to both languages. See
[Adding a native](../develop/adding-a-native.md).

## Next

- [Native coverage](../reference/natives-gap.md)
- [Coming from FXServer](from-fivem.md)
- [Adding a native](../develop/adding-a-native.md)
