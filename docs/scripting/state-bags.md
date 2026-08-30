---
title: "State bags and KVP"
description: "The two ways a resource keeps state — and the one important thing state bags do not do yet."
---

Two mechanisms, different jobs:

| | State bags | KVP |
| --- | --- | --- |
| Lives | in memory | on disk |
| Survives a restart | no | yes |
| Visible to other resources | yes | no (scoped to yours) |
| Visible to clients | **no — not yet** | no |
| Good for | live, shared, per-entity state | persistence |

## State bags

### The limitation to know first

**Replication to clients is not wired.** The `replicated` argument is recorded
and never sent:

```javascript
SetStateBagValue(`player:${source}`, "job", "police", 0, true);
//                                                       ^^^^ has no effect
```

State bags are server-local. Other *resources* on the same server see them;
*clients* do not. A client-side UI reading `Entity(e).state.job` will find
nothing.

To tell a client something, use `TriggerClientEvent`.

### Reading and writing

There is no `Entity(x).state` / `Player(x).state` / `GlobalState` sugar in
either language. Call the natives directly.

```javascript
SetStateBagValue("player:1", "job", "police", 0, false);
const job  = GetStateBagValue("player:1", "job");
const keys = GetStateBagKeys("player:1");
const has  = StateBagHasKey("player:1", "job");
```

```lua
SetStateBagValue("player:1", "job", "police", 0, false)
local job = GetStateBagValue("player:1", "job")
```

Bag names follow a convention BASTON understands:

| Bag | Meaning |
| --- | --- |
| `player:<source>` | a player |
| `entity:<netId>` | an entity |
| anything else | a free-form bag you invented |

`GetEntityFromStateBagName` and `GetPlayerFromStateBagName` reverse the first
two. `global` and `globalState` are **not** special — they are just names.

### Watching for changes

```javascript
const cookie = AddStateBagChangeHandler(
  "job",              // key filter   — null/"" matches any key
  "player:1",         // bag filter   — null/"" matches any bag
  (bag, key, value, _reserved, replicated) => {
    console.log(`${bag}.${key} = ${JSON.stringify(value)}`);
  }
);
RemoveStateBagChangeHandler(cookie);
```

```lua
local cookie = AddStateBagChangeHandler(nil, nil, function(bag, key, value)
    print(bag .. "." .. key .. " = " .. tostring(value))
end)
```

Three things that catch people:

- **Filters are exact equality, not prefixes or globs.** `"player:"` matches a
  bag literally named `player:`, not `player:1`. To watch every player, pass a
  null bag filter and check the name yourself.
- **Handlers are not called synchronously** inside `SetStateBagValue`. Changes
  are queued and delivered at the next dispatch boundary.
- **Handlers of *every* resource with a matching filter fire**, not just yours.

You may only remove a cookie your own resource created.

### Bounds

- **4096** pending deliveries per resource. Overflow drops the oldest and counts
  `state_bag_changes_dropped_total{queue="callback"}`.
- Handlers and pending deliveries are dropped when a resource stops or reloads.
- Deleting an entity removes its `entity:<id>` bag.

## KVP

Persistent key/value storage, scoped to your resource. This is where things that
must survive a restart go — unless you have a [database](database.md), which is
better for anything with structure.

```javascript
SetResourceKvp("greeting", "bonjour");
SetResourceKvpInt("visits", 42);
SetResourceKvpFloat("ratio", 0.75);

const s = GetResourceKvpString("greeting");   // null if absent
const i = GetResourceKvpInt("visits");        // 0 if absent
const f = GetResourceKvpFloat("ratio");       // 0.0 if absent

DeleteResourceKvp("greeting");
```

```lua
SetResourceKvp("greeting", "bonjour")
local s = GetResourceKvpString("greeting")
```

### Sync versus `NoSync`

This is the part worth understanding, because it is a real performance
trade-off:

| Call | Behaviour |
| --- | --- |
| `SetResourceKvp(…)` | **writes through to disk before returning** |
| `SetResourceKvpNoSync(…)` | marks the store dirty; written by the next flush |
| `FlushResourceKvp()` | forces pending writes out now, blocking |

Deferred writes are flushed every `kvp_flush_interval_secs` (default 30).

In a loop, use the `NoSync` variants and flush once:

```javascript
for (const [k, v] of manyThings) SetResourceKvpNoSync(k, String(v));
FlushResourceKvp();
```

Writing through on every iteration of a large loop will hurt.

### Iterating

```javascript
const handle = StartFindKvp("player:");
let key;
while ((key = FindKvp(handle)) !== null) { /* … */ }
EndFindKvp(handle);
```

The iterator walks a sorted snapshot; deletions made during iteration are not
observed.

### Practical notes

- **Values are always strings.** The Int/Float variants stringify on write and
  parse on read. A value written as an int and read as a string comes back as
  its decimal text.
- **Keys are scoped by resource name**, stored as `<resource>\0<key>` in one
  shared JSON file. Renaming a resource orphans its data.
- **Writes are atomic** — temp file plus rename, so a crash never leaves a
  half-written store.
- **KVP is not cleared on resource stop.** It persists until you delete it.
- **A failed flush is retried.** It is logged and counted as
  `kvp_flush_failures_total` — **watch that metric**, because a non-zero value
  means resources are silently losing data.
- **It is one file for the whole server.** Fine for settings and counters; use a
  [database](database.md) for anything that grows per player per session.

## Choosing between them

- **Live state other resources need** — state bags.
- **Small persistent settings and counters** — KVP.
- **Anything with structure, history, or queries** — a database.
- **Anything a client must see** — `TriggerClientEvent`, until state-bag
  replication lands.

## Next

- [Using a database](database.md)
- [Events](events.md)
