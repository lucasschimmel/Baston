---
title: "Using a database"
description: "Pooled SQL for resources, in both languages, without blocking the tick."
---

The `db` module gives resources SQL over SQLite, PostgreSQL or MySQL/MariaDB.
No `oxmysql`, no third-party resource.

Queries **never run on a script thread**. Your resource submits one and the pool
executes it on the server's runtime; a slow statement cannot stall your events.

## Turning it on

Operator side — see [Configuration](../reference/configuration.md#db--database-access-for-scripts):

```toml
[modules]
enable = ["db"]

[db]
url = "sqlite:baston.db"      # or postgres://…  or mysql://…
pool_size = 10
query_timeout_secs = 15
```

The driver must be in your bundle. `--features db` gives SQLite; `db-postgres`
and `db-mysql` add theirs; `full` has all three. Check with `--modules`.

Querying with the module off raises an error naming the module — it does not
return an empty result you would read as "no rows".

## The four calls

Identical semantics in both languages; only the spelling and the async model
differ.

| | Returns |
| --- | --- |
| `Query` | every matching row, as objects |
| `Execute` | number of rows affected |
| `Scalar` | first column of the first row, or null |
| `Insert` | the id the insert generated |

### JavaScript — promises

```javascript
const rows  = await Db.query("SELECT * FROM players WHERE cash > ?", [1000]);
const name  = await Db.scalar("SELECT name FROM players WHERE id = ?", [1]);
const n     = await Db.execute("UPDATE players SET cash = ? WHERE id = ?", [0, 1]);
const id    = await Db.insert("INSERT INTO players (name) VALUES (?)", ["Lucas"]);
```

Lowercase method names. A failed query rejects with `Db.<kind>: <message>`.

### Lua — inside a thread

```lua
CreateThread(function()
    local rows = Db.Query("SELECT * FROM players WHERE cash > ?", { 1000 })
    local name = Db.Scalar("SELECT name FROM players WHERE id = ?", { 1 })
    local n    = Db.Execute("UPDATE players SET cash = ? WHERE id = ?", { 0, 1 })
    local id   = Db.Insert("INSERT INTO players (name) VALUES (?)", { "Lucas" })
end)
```

PascalCase method names, and **every call must be inside
`Citizen.CreateThread`**. The query runs off-thread and the result arrives on a
later tick, so the coroutine yields until it does. Calling one outside a thread
raises an error naming the fix; it does not hang.

```lua
-- Wrong: not in a thread.
RegisterNetEvent("bank:balance", function()
    local cash = Db.Scalar("SELECT cash FROM players WHERE id = ?", { source })
end)

-- Right.
RegisterNetEvent("bank:balance", function()
    local player = source
    CreateThread(function()
        local cash = Db.Scalar("SELECT cash FROM players WHERE id = ?", { player })
        TriggerClientEvent("bank:balance", player, cash)
    end)
end)
```

Capture `source` **before** entering the thread — it is not bound inside.

## Always pass parameters as parameters

```javascript
// Right — bound by the driver, never part of the SQL text.
await Db.query("SELECT * FROM players WHERE name = ?", [userInput]);

// Wrong — this is how servers get emptied.
await Db.query(`SELECT * FROM players WHERE name = '${userInput}'`);
```

Parameters are bound by the driver. `Robert'); DROP TABLE players;--` is stored
as a name, not executed. This is not a convention — it is the mechanism.

Values map as you would expect: null, booleans, integers, floats and strings go
across natively; arrays and objects are sent as their JSON text, because most
schemas store them in a plain `TEXT` column.

## Driver differences that matter

| | SQLite | PostgreSQL | MySQL / MariaDB |
| --- | --- | --- | --- |
| URL | `sqlite:file.db` | `postgres://…` | `mysql://…`, `mariadb://…` |
| Needs a server | no | yes | yes |
| `Insert` returns an id | yes | **no — always null** | yes |
| Placeholder | `?` | `$1`, `$2`, … | `?` |

**PostgreSQL reports generated ids through `RETURNING`, not out of band**, so
`Db.Insert` gives you `null` there. Use a scalar instead:

```javascript
const id = await Db.scalar(
  "INSERT INTO players (name) VALUES ($1) RETURNING id", ["Lucas"]);
```

Note the placeholder difference too: PostgreSQL uses `$1`, the others use `?`.
Queries are not portable across drivers for free.

## Which driver to pick

- **SQLite** — zero configuration, no server, one file. The right default for a
  server for friends, and the fastest way to try the module.
- **PostgreSQL** — the recommendation for anything that will grow.
- **MySQL / MariaDB** — because the FiveM ecosystem runs on it. If you are
  migrating a server with an existing dump, start here and change later.

## Schema and migrations

BASTON does not manage them. It gives you a pool and four calls; the schema is
yours.

The simple pattern, run once at start:

```lua
CreateThread(function()
    Db.Execute([[
        CREATE TABLE IF NOT EXISTS players (
            license TEXT PRIMARY KEY,
            name    TEXT,
            cash    REAL DEFAULT 0
        )
    ]])
end)
```

For anything more, use your database's own migration tool outside BASTON.
Schemas, migrations and ORMs are deliberately out of scope — see
[ADR-002](../adr/002-module-tiers.md).

## Errors and timeouts

A failing query surfaces as an error in the calling script — it does not crash
the resource:

```lua
local ok, err = pcall(function()
    return Db.Query("SELECT * FROM nope")
end)
if not ok then print("query failed: " .. tostring(err)) end
```

A query exceeding `query_timeout_secs` (default 15) is abandoned and reported
the same way.

## Watching it

```bash
curl -s localhost:9090/metrics | grep baston_db_
```

- `baston_db_queries_total{resource,status}` — who queries, and how it goes
- `baston_db_query_duration_seconds` — latency, **including time queued waiting
  for a connection**

If latency is high while your database sits idle, `pool_size` is too small: the
time is spent waiting for a connection, not running SQL.

## Practical notes

- **The pool is shared** by every resource. One resource running slow queries
  starves the others — that is what `pool_size` and the timeout bound.
- **Keep `pool_size` under your database's own limit**, especially on managed
  PostgreSQL where the ceiling is often 20–100 for the whole account.
- **In a multi-zone server, every zone process opens its own pool.** Three zones
  at `pool_size = 10` is 30 connections. Size accordingly.

## Next

- [Your first resource](your-first-resource.md)
- [Configuration reference](../reference/configuration.md#db--database-access-for-scripts)
