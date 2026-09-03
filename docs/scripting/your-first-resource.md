---
title: "Your first resource"
description: "A working resource in JavaScript and in Lua, from an empty directory."
---

## The smallest thing that works

```
resources/hello/
  manifest.json
  server.js
```

`manifest.json`:

```json
{
  "name": "hello",
  "server_scripts": ["server.js"]
}
```

`server.js`:

```javascript
console.log("hello from BASTON");
```

Start the server. The resource is discovered and started automatically — there
is no `ensure`, no `.cfg`.

```
INFO scripting: runtime selected resource=hello engine=js
INFO script: hello from BASTON resource="hello"
INFO scripting: resource started resource=hello
```

Edit `server.js` and save: with `hot-reload` on (the default) the resource
restarts by itself. *(Hot reload watches `.js` only — a Lua resource must be
restarted by hand.)*

## The same in Lua

```
resources/hello-lua/
  manifest.json
  sv_main.lua
```

```json
{ "name": "hello-lua", "server_scripts": ["sv_main.lua"] }
```

```lua
print("hello from BASTON")
```

The extension picks the runtime. This resource needs the `lua` or `full`
bundle — on a `js` build the load fails and tells you which bundle would run it.

## Something that does something

A shop that greets players, keeps a per-player counter, and answers a client.

### JavaScript

```javascript
// Runs after every resource has loaded.
on("onResourceStart", (name) => {
  if (name !== GetCurrentResourceName()) return;
  console.log(`[shop] ready, ${GetPlayers().length} players online`);
});

// A client → server event.
//
// SECURITY: in JavaScript every handler receives client traffic, whether or
// not you called RegisterNetEvent. Treat this as a public endpoint: validate
// `source` and every argument.
on("shop:buy", async (itemId) => {
  const player = source;                       // capture before any await
  if (typeof itemId !== "string") return;      // never trust the client

  const key = `purchases:${player}`;
  const count = GetResourceKvpInt(key) + 1;
  SetResourceKvpInt(key, count);

  TriggerClientEvent("shop:bought", player, { itemId, count });
  console.log(`[shop] ${GetPlayerName(player)} bought ${itemId} (#${count})`);
});
```

### Lua

```lua
AddEventHandler("onResourceStart", function(name)
    if name == GetInvokingResource() then
        print("[shop] ready")
    end
end)

-- In Lua, a client event needs RegisterNetEvent. AddEventHandler alone does
-- not receive client traffic.
RegisterNetEvent("shop:buy", function(itemId)
    local player = source
    if type(itemId) ~= "string" then return end

    local key = "purchases:" .. player
    local count = GetResourceKvpInt(key) + 1
    SetResourceKvpInt(key, count)

    TriggerClientEvent("shop:bought", player, { itemId = itemId, count = count })
end)
```

Note the differences already: `GetInvokingResource()` instead of
`GetCurrentResourceName()`, and `GetPlayerName` is not available in Lua.

## A periodic loop

**JavaScript cannot do this.** `setInterval` never fires. Write it in Lua:

```lua
CreateThread(function()
    while true do
        Wait(60000)            -- one minute
        payEveryone()
    end
end)
```

If your gamemode is JavaScript and needs a tick, add a small Lua resource that
loops and emits an event the JS resource handles:

```lua
-- resources/ticker/sv_ticker.lua
CreateThread(function()
    while true do
        Wait(60000)
        TriggerEvent("ticker:minute")
    end
end)
```

```javascript
// in your JS resource
on("ticker:minute", () => payEveryone());
```

## Talking to a database

```lua
CreateThread(function()
    Db.Execute([[
        CREATE TABLE IF NOT EXISTS players (
            license TEXT PRIMARY KEY,
            cash    REAL DEFAULT 0
        )
    ]])
end)

RegisterNetEvent("bank:deposit", function(amount)
    local player = source
    CreateThread(function()
        Db.Execute("UPDATE players SET cash = cash + ? WHERE license = ?",
                   { amount, licenseOf(player) })
    end)
end)
```

```javascript
on("bank:deposit", async (amount) => {
  const player = source;
  await Db.execute("UPDATE players SET cash = cash + ? WHERE license = ?",
                   [amount, licenseOf(player)]);
});
```

Requires the `db` module. Parameters are always bound, never spliced — that is
what makes injection impossible. Full details in
[Using a database](database.md).

## A whitelist

The most common reason to have server-side script at all:

```javascript
on("playerConnecting", async (name, setKickReason, deferrals) => {
  deferrals.defer();
  deferrals.update("Checking the whitelist…");

  const license = GetPlayerIdentifierByType(source, "license");
  const allowed = await Db.scalar(
    "SELECT 1 FROM whitelist WHERE license = ?", [license]);

  if (!allowed) {
    deferrals.done("You are not on the whitelist.");
    return;
  }
  deferrals.done();
});
```

`deferrals.update()` is logged server-side and **not shown to the player** —
the message above is for your logs, not their loading screen.

## Structuring a resource

Load order is the order of `server_scripts`, and each script runs to completion
before the next starts:

```json
{
  "name": "my-gamemode",
  "dependencies": ["my-lib"],
  "server_scripts": [
    "server/config.js",
    "server/db.js",
    "server/gameplay.js"
  ]
}
```

- **No modules.** No `require`, no `import`, no `export`. Scripts share one
  global scope, in order — declare shared things in the first file.
- **`dependencies`** controls which *resources* start first, not files.
- **Cross-resource `exports` throw.** Use events between resources.

## Cleaning up

```javascript
on("onResourceStop", (name) => {
  if (name !== GetCurrentResourceName()) return;
  FlushResourceKvp();          // force pending KVP writes to disk
});
```

This fires before the runtime is destroyed, so it can still do work. A reload
destroys everything held in a variable; KVP and your database survive.

## Seeing what it does

With the `admin-api` module on:

```bash
# Per-resource CPU, dispatch counts, memory
curl -H "Authorization: Bearer $TOKEN" localhost:8080/api/v1/resmon | jq

# Per-handler p95/p99 and error counts
curl -H "Authorization: Bearer $TOKEN" localhost:8080/api/v1/resmon/events | jq
```

And if something silently does nothing, check whether you called a native that
does not exist yet:

```bash
curl -s localhost:9090/metrics | grep script_native_unimplemented_total
```

## Next

- [Events](events.md) — every event, with real signatures
- [Using a database](database.md)
- [Coming from FXServer](from-fivem.md)
