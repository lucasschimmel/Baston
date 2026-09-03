# Zone map editor

Draw a BASTON zone map — rectangles, circles, traced outlines — and export
`map.toml`. One HTML file, no install, no server.

```bash
bun tools/zone-map-editor/build.mjs
```

Then open `tools/zone-map-editor/zone-map-editor.html` in a browser. Once, if
the target is missing:

```bash
rustup target add wasm32-unknown-unknown
```

## Why it validates with WebAssembly

The editor must refuse exactly what the Gateway refuses at boot. A second
implementation of the rules in JavaScript would drift, and a validator that
drifts is worse than none — it tells you the map is fine and the server tells
you otherwise after you have drawn the whole thing.

So `baston-zonemap` — the crate the Gateway itself parses maps with — is
compiled to `wasm32-unknown-unknown` and inlined into the page. The errors and
warnings you see are the server's own strings, and "which region owns this
point" is answered by the same `region_at` the server routes with.

The editor never parses TOML. It writes documents and reads them back through
the validator, so importing a map cannot disagree with loading one.

The generated HTML is **not committed**, deliberately: a stale copy would
validate against rules the server no longer has, which is the drift the shared
crate exists to prevent. Build it when you need it.

No `wasm-bindgen` either — it would pin a `wasm-bindgen-cli` version to install
and keep in step, for an interface of five functions. The page talks to the
module over a raw C ABI, so `cargo build` is the whole toolchain.

## Using it

Load a top-down map image, then set the world coordinates of its four edges.
The readout follows the cursor: point at a landmark you know and adjust until
it matches. Everything works without an image too, just with less to aim at.

| | |
| --- | --- |
| **Select** | Click a region to select it, then drag its handles. |
| **Rect** | Drag corner to corner. |
| **Circle** | Click the centre, drag out the radius. |
| **Polygon** | Click each vertex. `Enter` or double-click closes it, `Backspace` undoes one, `Escape` cancels. |
| **+ catch-all** | Adds the `everywhere` region. Always last — it is what makes a gap impossible. |

Wheel zooms toward the cursor, right-drag pans, `Delete` removes the selection.

The region list is the map's priority, highest first, and `↑`/`↓` reorder it.
A new shape lands on top, because a shape drawn inside another is nearly always
meant to win. Regions of the same zone share a colour, so a zone owning several
separate areas reads as one thing.

The readout names the region owning the ground under the cursor. That is the
ordered-list rule made visible, and it is the fastest way to check a map says
what you think it says.

## What it will not do

Change the map on a running server. Repartitioning a live world moves players
between processes; `map.toml` is read at Gateway startup and nowhere else.

See [the zone map documentation](../../docs/server/zone-map.md) for the format
and what the Gateway checks.
