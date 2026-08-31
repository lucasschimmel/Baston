---
title: "Zone configuration"
description: "Splitting the map across zone processes without gaps or overlap."
---

The world plane runs from −4000 to +4000 on X and Y. Zone bounds are
min-inclusive / max-exclusive so adjacent zones tile without a coordinate
belonging to two zones.

This page covers the case where every zone is one rectangle it declares for
itself. When a rectangle cannot say what you mean — a district traced along its
real outline, an event venue carved out of the zone around it, one zone owning
several separate areas — draw a map instead: [The zone map](zone-map.md). A map
also removes the gap risk below, since it requires a catch-all region.

## 2 zones (Phase D benchmark)

| Zone | Bounds (x_min,y_min,x_max,y_max) | Area |
|---|---|---|
| zone-a | `-4000,-4000,0,4000` | west half (Blaine County west, Paleto, LSIA) |
| zone-b | `0,-4000,4000,4000` | east half (east LS, Sandy Shores east) |

The split at x=0 crosses central Los Santos — deliberately: it exercises
handoffs where player density is highest.

## 4 zones (recommended production split)

Population is concentrated in the south (Los Santos), so the south gets
smaller zones:

| Zone | Bounds | Contents |
|---|---|---|
| zone-ls-west | `-4000,-4000,0,-500` | west LS, LSIA, Del Perro, Vespucci |
| zone-ls-east | `0,-4000,4000,-500` | east LS, port, Mirror Park |
| zone-north-west | `-4000,-500,0,4000` | Chumash, Zancudo, Paleto west |
| zone-north-east | `0,-500,4000,4000` | Vinewood Hills, Sandy Shores, Grapeseed |

Rules of thumb:
- Keep boundaries OUT of interiors/apartment clusters when possible — every
  crossing costs a handoff (cheap, but not free).
- `boundary_margin` (default 300m) must be smaller than the smallest zone
  dimension, or every player is permanently "approaching a boundary".
- Aim for < 1500 active players per zone; use `/admin/zones` player counts
  and split the densest zone when it trends above.

## Config knobs (`baston.toml [meshing]`)

| Key | Default | Notes |
|---|---|---|
| `boundary_margin` | 300.0 | handoff preparation distance (m) |
| `boundary_scan_interval_ms` | 500 | detection cadence |
| `handoff_cooldown_secs` | 5 | anti ping-pong per player |
| `heartbeat_interval_secs` | 5 | zone → gateway |
| `zone_timeout_secs` | 15 | eviction after 3 missed heartbeats |
