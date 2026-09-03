---
title: "The zone map"
description: "Drawing zone territories with rectangles, circles and traced outlines."
---

A zone map is an ordered list of regions in its own TOML file. The owner of a
point is the **first region that contains it**, so it reads top to bottom like
a firewall ruleset.

Without a map file, each zone declares a rectangle for itself and nothing
changes from how meshing has always worked. See
[Zone configuration](zone-config.md) for that.

## Turning it on

```toml
# baston.toml, on the Gateway
[meshing]
enabled = true
map_file = "map.toml"      # relative to the directory holding baston.toml
```

Relative to the config file rather than to the working directory, so a mounted
`config/` works without anyone knowing where the process was launched from.
`BASTON_MAP_FILE` overrides it.

`config/map.example.toml` in the repository is a working starting point — copy
it to `config/map.toml` and edit.

## Drawing one

Nobody writes forty vertices by hand. `tools/zone-map-editor` builds a single
HTML file — no install, no server — where you drop a top-down map image, draw
rectangles, circles and traced outlines, reorder them, and export `map.toml`.

```bash
bun tools/zone-map-editor/build.mjs
```

It validates with **the server's own code**: `baston-zonemap`, the crate the
Gateway parses maps with, compiled to WebAssembly and inlined into the page. The
errors and warnings it shows are the Gateway's strings, and the region it names
under your cursor is the one `region_at` would pick. A second implementation of
these rules in JavaScript would drift, and a validator that drifts is worse than
none.

## What a map looks like

```toml
# config/map.toml

[[region]]
name = "maze-bank-arena"
zone = "zone-arena"
shape = "circle"
center = [-250.0, -2000.0]
radius = 240.0

[[region]]
name = "los-santos"
zone = "zone-city"
shape = "rect"
bounds = [-4000.0, -4000.0, 4000.0, 500.0]

[[region]]
name = "the-countryside"
zone = "zone-country"
shape = "everywhere"
```

The arena sits geometrically **inside** the Los Santos rectangle and still owns
its ground, because it comes first. That is the whole mechanism: you never have
to cut a hole in one outline to make room for another.

## Order is priority

There is no `priority` number, on purpose. A number invites ties, and a tie has
to be broken by *something* — insertion order, a hash iteration — that does not
survive a restart. An array is a total order by construction.

The cost is that "who wins here" means reading the file in order. At the map
sizes anyone writes by hand that is not a real cost, and it is what makes the
answer reproducible.

## The last region must be `everywhere`

The Gateway refuses to start otherwise.

A gap in the map is not a cosmetic problem: a player standing in one has no
owning zone, falls back to whichever zone is least loaded, and gets reassigned
again on the next scan. The catch-all makes that unreachable rather than
unlikely.

## Shapes

| `shape` | Keys | Notes |
| --- | --- | --- |
| `rect` | `bounds = [x_min, y_min, x_max, y_max]` | Same order as `ZONE_BOUNDS`. |
| `circle` | `center = [x, y]`, `radius` | |
| `poly` | `points = [[x, y], …]` | Closed automatically; any number of vertices. |
| `everywhere` | — | Last region only. |

All 2D. There is no `z` range yet: the routing surface speaks in `(x, y)` from
the connection router down to a handoff's predicted coordinates, and giving
shapes an altitude means giving it one everywhere it has nowhere to get one.

### Tracing an outline

```toml
[[region]]
name = "los-santos"
zone = "zone-city"
shape = "poly"
points = [
  [-1600.0, -1000.0],
  [-1350.0, -1900.0],
  [ -800.0, -2700.0],
  [  200.0, -3300.0],
]
```

One vertex per line is worth the verbosity: when you iterate on a contour, a
flat `[x1, y1, x2, y2, …]` list turns every adjustment into one 400-character
red line in `git diff`.

Winding order does not matter. A repeated closing vertex — which most tracing
tools emit — is dropped rather than refused. A self-intersecting outline **is**
refused: which side is inside would depend on the fill rule.

### One zone, several areas

List it more than once. The union is its territory; there is no separate
"combo" concept.

```toml
[[region]]
zone = "zone-city"
shape = "poly"
points = [ [0.0, 0.0], [100.0, 0.0], [100.0, 100.0] ]

[[region]]
name = "docks"
zone = "zone-city"
shape = "rect"
bounds = [-1200.0, -3300.0, 500.0, -2100.0]
```

## What the Gateway checks at boot

Refused, with the region named:

- the last region is not `everywhere`, or an `everywhere` appears before the
  end (everything after it would be dead);
- a polygon with fewer than three vertices, or one that crosses itself;
- a degenerate rectangle, or a radius at or below zero;
- a key belonging to another shape (`radius` on a `rect`), or a misspelled one.

Warned about, because the map still runs:

- a region **entirely inside an earlier one**, which will never own anything.
  This catches the likeliest authoring mistake — listing the arena *after* the
  city that already covers it — which otherwise looks like the arena silently
  refusing to open.

A zone that registers but claims no region in the map is refused, and the
refusal lists the zone ids the map does know. A typo in a `zone` id stops the
zone rather than leaving it running with no ground.

## What the zones are told

A zone does not read the map. At registration the Gateway answers with two
lists:

- **its regions** — what it owns;
- **the overlays** — the higher-priority regions that cut into them.

The second list matters more than it looks. A player walking from Los Santos
*into* the arena never leaves the Los Santos outline, so without the overlay
the city's boundary scan sees someone a kilometre from any edge and nothing
ever fires: the player stays on a zone that no longer owns the ground under
them. With it, entering the arena reads as leaving the city, which is what it
is.

This also means `ZONE_BOUNDS` and `meshing.zone_bounds` stop mattering once a
map exists. The zone still sends them and the map overrules them; the zone logs
the territory it was given at boot.

## When a zone is down

Its regions are skipped, so the ground falls through to whatever is underneath
— usually the catch-all. Players there keep an owner instead of becoming
unroutable. When the zone comes back and re-registers, it takes its ground
again.

## Changing the map

Restart the Gateway. There is no hot reload: repartitioning a live world moves
players between processes, and doing that from a file watcher is a good way to
find out how the handoff protocol behaves under a partition that changed
underneath it.

## Performance

Not a concern, and worth saying why, because the FiveM library this resembles —
PolyZone — is famous for costing frames.

PolyZone runs a point-in-polygon test **every frame, on the client, for every
zone**. Here, ownership is resolved when a player connects and when a handoff
is prepared: a handful of times per player per session. The only repeated cost
is the boundary scan, twice a second per player, server-side, against a
bounding-box-filtered outline.
