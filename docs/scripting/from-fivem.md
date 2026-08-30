---
title: "Coming from FXServer"
description: "The porting checklist: what changes, what silently does not work, and how to find out which natives your resource needs."
---

Most FiveM server code runs on BASTON unchanged. This page is about the parts
that do not — and in particular the ones that fail **quietly**.

## The five-minute version

| FXServer | BASTON |
| --- | --- |
| `server.cfg` | `config/baston.toml` |
| `resources.cfg`, `ensure` | nothing — resources with a `manifest.json` start automatically |
| `fxmanifest.lua` | `manifest.json` |
| `shared_scripts` | does not exist |
| `ui_page` / NUI | does not exist |
| `chat` command routing | commands only via the admin API |
| ACE permissions | always deny |

## Step 1: the manifest

Replace `fxmanifest.lua` with `manifest.json`. The complete schema:

```json
{
  "name": "my-resource",
  "version": "1.0.0",
  "dependencies": ["some-lib"],
  "server_scripts": ["server/main.js"],
  "client_scripts": ["client/main.js"],
  "files": ["html/index.html"]
}
```

There is no `fx_version`, `games`, `shared_scripts`, `ui_page`, `data_file`,
`provide`, `escrow_ignore`, `author` or `description`. **Unknown keys are
silently ignored**, so a leftover `shared_scripts` does not error — it just does
nothing, and the file it names is never loaded.

`shared_scripts` is the one that catches people. Split it: put the shared code
in both `server_scripts` and `client_scripts`, or move it to a library resource.

Resource discovery is **one level deep**. FXServer's `[categories]` nesting does
not work; put every resource directly under `resources/`.

## Step 2: pick the runtime, and know its gaps

The file extension picks it — `.js`/`.mjs`/`.cjs` or `.lua`. A resource runs on
one engine; mixing is refused at load.

**Neither language is complete.** Before porting, check your resource against
[Choosing a language](index.md#choosing-a-language). The two that decide most
ports:

- **JavaScript has no working timers.** `setInterval` never fires and
  `setTimeout` ignores its delay. Any `while true do … Wait(n) end` loop, any
  polling tick, must be Lua.
- **Lua cannot look players up.** `GetPlayerName`, `GetPlayers`,
  `GetPlayerIdentifiers`, `GetConvar`, `GetResourceState`, `LoadResourceFile`,
  `SetHttpHandler` and `PerformHttpRequest` are JavaScript-only.

A resource that needs both a loop *and* player lookups has to be split in two,
talking over events.

## Step 3: the silent failures

These compile, run, and do nothing. In rough order of how much damage they do:

### State bags do not reach clients

```javascript
SetStateBagValue(`player:${source}`, "job", "police", 0, true);
//                                                        ^^^^ ignored
```

Replication is recorded and never sent. State bags work server-side — between
resources, on one server — and clients never see them. A UI driven by
`Entity(e).state` on the client will show nothing.

Use `TriggerClientEvent` for anything a client must know.

There is also no `Entity(x).state` / `Player(x).state` / `GlobalState` sugar in
either language. Call `SetStateBagValue` / `GetStateBagValue` directly, with bag
names `entity:<netId>` and `player:<source>`.

### Event cancellation does nothing

```javascript
on("someEvent", () => { CancelEvent(); });   // has no effect
WasEventCanceled();                          // always false
```

Any handler chain relying on cancellation to veto an action needs restructuring
— return a value, or check a condition in the acting handler.

### ACE permissions always deny

`IsPlayerAceAllowed`, `IsAceAllowed` and `IsPrincipalAceAllowed` return `false`
unconditionally. There is no ACE system, no `add_ace`, no principals.

An admin check written as `if (IsPlayerAceAllowed(src, "command.kick"))` will
deny everyone — safe, but non-functional. Implement permissions in your own
resource against identifiers or your database.

### Cross-resource exports throw

```javascript
exports["other-resource"].doThing();   // throws
exports("myThing", fn);                // works
exports["my-own-resource"].myThing();  // works
```

Only same-resource exports resolve. Between resources, use events.

### Commands are not reachable from chat

`RegisterCommand` registers, but nothing routes chat or a console into it. The
only path is `POST /api/v1/commands/execute` on the admin API. The `restricted`
flag is stored and never enforced.

If your resource's admin tooling is command-based, it needs another entry point
— an event from a client-side UI, or the admin API.

### `playerDropped` has no source

```javascript
on("playerDropped", (reason) => { save(source); });   // source is undefined
```

The normal disconnect paths pass only the string `"Disconnected."`. Track
players on `playerJoining` and reconcile.

### Smaller ones

| Native | Behaviour |
| --- | --- |
| `deferrals.update()` | logged server-side, never shown to the player |
| `deferrals.presentCard()` | does nothing |
| `ExecuteCommand()` | warns, does nothing |
| `TempBanPlayer()` | drops the player, **keeps no ban list** |
| `ProfilerEnterScope` / `ExitScope` | no-ops; scopes never appear in a trace |
| `GetRegisteredCommands`, `GetGamePool`, `GetEntitiesInRadius` | return `[]` |
| `GetGameBuildNumber`, `GetInstanceId` | return `0` |
| Latent client events | sent whole, unpaced |

## Step 4: find out what else your resource needs

This is the part that saves you the most time. BASTON does not crash on a
missing native — it returns a neutral value, warns **once per native name**, and
counts it:

```bash
curl -s localhost:9090/metrics | grep script_native_unimplemented_total
```

```
script_native_unimplemented_total{native="GET_VEHICLE_NUMBER_PLATE_TEXT"} 14
script_native_unimplemented_total{native="SET_PED_ARMOUR"} 3
```

Start your resource, exercise it, then read that list. It is the exact set of
things standing between you and a working port — and each name is a concrete,
implementable unit of work.

The full picture is in [native coverage](../reference/natives-gap.md).

## Step 5: things that work exactly as you expect

So you know where *not* to spend effort:

- Events, `TriggerEvent` / `TriggerClientEvent`, `source`
- `playerConnecting` with deferrals — the whitelist/queue pattern works
- KVP: `SetResourceKvp*` / `GetResourceKvp*`, with real persistence
- `stream/` assets — drop them in, no manifest declaration, same as FXServer
- Entity natives backed by the world mirror: coordinates, health, model, type,
  routing buckets
- Vehicle and ped state getters — a large, faithfully-ported set
- `MUMBLE_*` voice natives
- Escrowed resources, on Windows

## What you gain

Worth knowing, since you are porting anyway:

- **A real database layer.** `Db.query(...)` with a pool, in both languages, no
  `oxmysql` dependency. See [Using a database](database.md).
- **A profiler and per-resource metrics** out of the box — `resmon`, Chrome
  traces, Prometheus.
- **A watchdog.** A runaway script is terminated and the runtime survives,
  instead of wedging the server.
- **Modules.** You run only what you switched on.

## A porting order that works

1. Convert the manifest. Start the resource; confirm it loads.
2. Grep your code for the silent failures above. Fix them first — they are the
   ones you will not notice otherwise.
3. Run it, exercise every feature, and read
   `script_native_unimplemented_total`.
4. Decide per resource: implement the missing natives, work around them, or
   split the resource across both runtimes.

## Next

- [Your first resource](your-first-resource.md)
- [Events](events.md) — real signatures
- [Native coverage](../reference/natives-gap.md)
