---
title: "Events"
description: "Every event BASTON fires, with its real signature — including the ones whose arguments differ from FXServer."
---

## How dispatch works

`TriggerEvent` does not call handlers directly. It queues the event on the Rust
side; the host then broadcasts it to **every loaded resource**, including the
one that triggered it. All resources begin their dispatch before any reply is
collected, so one slow resource does not delay the others.

A handler that throws is caught, logged and counted. The other handlers still
run.

Chained events are capped at **64 dispatches** per originating event. Two
handlers triggering each other cannot wedge the server; the chain is dropped
with a warning.

## Registering

```javascript
// JavaScript — `on` and `onNet` are aliases of AddEventHandler
AddEventHandler("myResource:doThing", (a, b) => { /* … */ });
const handle = on("other:event", handler);
RemoveEventHandler(handle);
```

```lua
-- Lua
AddEventHandler("myResource:doThing", function(a, b) end)
RegisterNetEvent("myResource:fromClient", function(a) end)  -- required for client events
local token = AddEventHandler("x", handler)
RemoveEventHandler(token)
```

**The opt-in difference matters.** In Lua a client → server event reaches a
resource only if it called `RegisterNetEvent`. In JavaScript `RegisterNetEvent`
is a no-op and every handler receives client traffic. See
[the security note](index.md#one-security-difference-worth-stating-plainly).

## The events BASTON fires

There are six. That is the complete list — there is no `onResourceStarting`, no
`onServerResourceStart`, no `onResourceListRefresh`.

### `onResourceStart(resourceName)`

Fires after **all** of a resource's scripts have finished loading, and is
broadcast to every resource — including the one that just started.

Inside your own resource, this fires *after* your top-level code has run. It is
the right place for work that needs other resources to exist; it is not needed
for your own initialisation.

```javascript
on("onResourceStart", (name) => {
  if (name !== GetCurrentResourceName()) return;
  console.log("we are up");
});
```

### `onResourceStop(resourceName)`

Fires **before** the runtime is destroyed, so handlers can still run — flush
state here.

```lua
AddEventHandler("onResourceStop", function(name)
    if name == GetInvokingResource() then saveEverything() end
end)
```

### `playerConnecting(playerName, setKickReason, deferrals)`

The whitelist and queue hook. Fires during the HTTP join, before the player
reaches the game.

```javascript
on("playerConnecting", async (name, setKickReason, deferrals) => {
  deferrals.defer();
  deferrals.update("Checking the whitelist…");

  if (!(await isAllowed(source))) {
    deferrals.done("You are not on the whitelist.");
    return;
  }
  deferrals.done();
});
```

```lua
AddEventHandler("playerConnecting", function(name, setKickReason, deferrals)
    deferrals.defer()
    deferrals.update("Checking the whitelist…")
    if not isAllowed(source) then
        deferrals.done("You are not on the whitelist.")
        return
    end
    deferrals.done()
end)
```

- `deferrals.done()` with no reason **accepts**; with a reason **rejects** and
  the player sees it.
- If **no** handler calls `defer()`, the connection is accepted automatically.
- The whole flow is bounded by `connection.deferral_timeout_secs` (default 10 s).
  Raise it if you run a queue.
- A handler that throws after deferring releases the connection with a server
  error rather than stranding the player.

Two caveats:

- **`deferrals.update()` is logged server-side and never shown to the player.**
- **`deferrals.presentCard()` is a stub.** It does nothing.

`setKickReason(reason)` stores a reason used if the connection is cancelled
without an explicit `done(reason)`.

### `playerJoining(oldId)`

Fires once the player's game connection is up. `source` is bound.

The argument is the source **as a string**. In a multi-zone server, the
zone-side variant instead receives `(playerId, scriptState)` where `scriptState`
is what your [zone transfer collector](#zone-transfer-state) returned.

### `playerDropped(reason)`

**This does not match FXServer, and the difference will break ported code.**

On the normal disconnect paths, the handler receives exactly one argument —
the string `"Disconnected."` — and **`source` is not bound**. You cannot tell
which player left from the event alone.

```javascript
// This does NOT work — source is undefined here.
on("playerDropped", (reason) => {
  savePlayer(source);          // ✗
});
```

Track players yourself, on `playerJoining`, and reconcile with `GetPlayers()`.
Only the multi-zone handoff path passes `(source, reason)`.

### `onEntityOwnerChanged(entityId, newOwner)`

Fires when an entity's simulating client changes. `entityId` is a **string**,
`newOwner` a number.

Not fired by the gateway in mesh mode — zones own it there.

## Events that never reach you

The gateway intercepts these client → server names and consumes them:

`__baston:stateUpdate` · `__baston:nativeResult` · `baston:displayInfo:toggle` ·
`hostingSession` / `hostedSession`

And two internal events are delivered to a *single* resource, not broadcast:
`__cfx_internal:httpRequest` and `__cfx_internal:httpResponse`. Do not handle
them directly; use `SetHttpHandler` and `PerformHttpRequest`.

## Sending events

```javascript
TriggerEvent("my:serverEvent", 1, "two");          // server-side, all resources
TriggerClientEvent("my:clientEvent", source, data); // to one client
emit("my:serverEvent", 1);                          // alias
emitNet("my:clientEvent", source, data);            // alias
```

```lua
TriggerEvent("my:serverEvent", 1, "two")
TriggerClientEvent("my:clientEvent", source, data)
```

Arguments cross as JSON server-side and msgpack on the wire, so anything
JSON-representable travels. Functions do not.

## `source`

Bound during net-event, `playerConnecting` and command dispatch, and restored
afterwards. It is **not** bound during `onResourceStart`, `onResourceStop`, or
`playerDropped`.

In Lua it is a global set with `rawset`; in JavaScript, `globalThis.source`.

If you `await` inside a handler, capture `source` first — it is restored when
the synchronous part of the dispatch ends:

```javascript
on("my:netEvent", async () => {
  const player = source;        // capture immediately
  await something();
  use(player);                  // `source` is no longer reliable here
});
```

## Zone transfer state

Only relevant in [multi-zone](../server/multi-zone.md) servers. Register a
collector and BASTON carries what it returns to the next zone.

```javascript
RegisterZoneTransferState((src) => ({ cash: getCash(src), job: getJob(src) }));
```

```lua
RegisterZoneTransferState(function(src)
    return { cash = getCash(src), job = getJob(src) }
end)
```

All collectors in a resource are merged into one object. It arrives as the
second argument of the zone-side `playerJoining`.

A resource without a collector simply starts fresh in the new zone.

## Cross-zone events

In a multi-zone server, an event you trigger is mirrored to sibling zones —
**unless its name begins with `player`, `onResource`, or `__baston`**, or is
`onEntityOwnerChanged`. Those stay local.

This is a naming rule, not a flag. An event you named `playerShopBought` will
silently not cross zones. Name custom events after your resource
(`myshop:bought`) and this never bites you.

## Commands

```javascript
RegisterCommand("give", (source, args, raw) => { /* … */ }, false);
```

```lua
RegisterCommand("give", function(source, args, raw) end, false)
```

Two things to know:

- **There is no chat or console path to a command.** Commands are reachable
  only through the authenticated admin API:
  `POST /api/v1/commands/execute`.
- **The `restricted` flag is stored and never enforced.** Do your own permission
  check — and note `IsPlayerAceAllowed` always returns false, so it cannot help.

## Next

- [Natives](natives.md) — what you can call
- [State bags](state-bags.md)
- [Coming from FXServer](from-fivem.md)
