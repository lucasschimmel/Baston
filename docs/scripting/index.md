---
title: "Writing resources"
description: "The BASTON scripting API for JavaScript and Lua — what exists, what does not, and which language to pick."
---

This track is for people writing resources. If you are running a server, start
at [Running a BASTON server](../server/index.md).

## Read this first

BASTON runs FiveM resources, but it is not FXServer, and the differences are not
all cosmetic. Some things you rely on **do not work**, and they fail quietly
rather than loudly. The full list is in
[Coming from FXServer](from-fivem.md); the ones that bite hardest:

- **State bags do not reach clients.** `replicated = true` is recorded and never
  sent. State bags are server-local today.
- **`CancelEvent()` does nothing.** `WasEventCanceled()` always returns false.
- **ACE permissions always deny.** `IsPlayerAceAllowed` is always `false`.
- **Cross-resource `exports` throw.** Only same-resource exports work.
- **`RegisterCommand` is not reachable from chat.** Commands only arrive through
  the admin API.
- **`playerDropped` gives you a string, not a source.**

None of these are hidden: BASTON logs and counts unimplemented natives, so
`script_native_unimplemented_total` tells you exactly what your resource asked
for and did not get.

## Choosing a language

Both languages share one implementation of the CFX natives, so a native behaves
identically in either. What differs is the surface *around* the natives — and
**neither language has everything**.

### JavaScript has no threads or timers

This is the single biggest constraint and it surprises everyone:

```javascript
setInterval(() => doThing(), 1000);   // never fires
setTimeout(() => doThing(), 5000);    // fires immediately, delay ignored
```

There is no `Citizen.CreateThread`, no `Wait`, no `setTick`. **A JavaScript
resource cannot run a periodic loop.** If your resource needs one, write it in
Lua, or drive it from outside — an event, an HTTP request, a client.

JavaScript does have real async: handlers may be `async`, and the host awaits
returned promises before completing the dispatch.

### Lua is missing the player and resource surface

These exist only in JavaScript, because they are implemented as V8 ops rather
than as natives:

`GetPlayerName` · `GetPlayers` · `DoesPlayerExist` · `GetPlayerIdentifier*` ·
`GetPlayerPing` · `GetPlayerEndpoint` · `GetPlayerGuid` · `GetConvar*` ·
`SetConvar` · `GetCurrentResourceName` · `GetResourceState` · `GetResourcePath` ·
`GetResourceMetadata` · `LoadResourceFile` · `SaveResourceFile` ·
`GetGameTimer` · `SetHttpHandler` · `PerformHttpRequest`

Calling one from Lua returns a neutral value and increments
`script_native_unimplemented_total`. In Lua, use `GetInvokingResource()` instead
of `GetCurrentResourceName()`.

### Side by side

| | JavaScript | Lua |
| --- | --- | --- |
| Periodic loops | **no** | `Citizen.CreateThread` + `Wait` |
| `async` / promises | yes | coroutines |
| Player / resource / convar lookups | yes | **no** |
| Inbound and outbound HTTP | yes | **no** |
| Database | `Db.query(...)` (promise) | `Db.Query(...)` (in a thread) |
| Net-event opt-in | **none — every handler is remotely reachable** | `RegisterNetEvent` required |
| Hot reload on file change | yes | **no** |
| `exports.res:fn()` syntax | no | yes |
| Memory in resmon | reported | not reported |
| Console | `console.log` | `print` |

### So which?

- **Most resources: JavaScript.** It has the fuller surface, and hot reload.
- **Anything with a loop: Lua.** A tick, a timer, a queue drain.
- **Both: the `full` bundle**, with resources talking over events. This is also
  the answer when a Lua resource needs a player name — put the lookup in a
  small JS resource and ask it.

### One security difference worth stating plainly

In Lua, a resource receives client → server events only for names it passed to
`RegisterNetEvent`. **In JavaScript, `RegisterNetEvent` is a no-op and every
`AddEventHandler` receives client traffic.** A JS handler you meant as internal
is remotely triggerable by any connected player.

Until that is fixed, treat every JS event handler as a public endpoint:
validate `source`, validate arguments, and do not put privileged operations
behind an event name you assumed was private.

## The pages

| | |
| --- | --- |
| [Your first resource](your-first-resource.md) | a working resource in both languages |
| [Events](events.md) | every event BASTON fires, with real signatures |
| [Natives](natives.md) | what is implemented, what is stubbed |
| [State bags](state-bags.md) | shared state, and its current limits |
| [Using a database](database.md) | SQL that never blocks the tick |
| [HTTP](http.md) | serving and calling out |
| [Coming from FXServer](from-fivem.md) | the porting checklist |

## Reference

- [Native coverage](../reference/natives-gap.md)
- [Configuration](../reference/configuration.md) — the limits your resource runs under
- [Metrics](../reference/metrics.md) — how to see what your resource is doing
