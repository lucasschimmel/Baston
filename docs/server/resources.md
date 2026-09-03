---
title: "Installing resources"
description: "How BASTON discovers, loads and reloads resources — and what replaced fxmanifest.lua and ensure."
---

## There is no `resources.cfg`

In FXServer you write `ensure my-resource` in a `.cfg` file. BASTON has nothing
like it. The resource manager scans `resources.path` and automatically starts
**every subdirectory containing a valid `manifest.json`**.

To stop a resource: remove its directory, remove its `manifest.json`, or stop it
at runtime through the [admin API](../reference/api.md).

```
resources/
  my-gamemode/
    manifest.json          ← this is what makes it a resource
    dist/server/index.js
  carpack/
    manifest.json
    stream/vehicles/adder2.yft
```

A directory with an invalid manifest is skipped with a warning — one broken
resource never takes the server down.

## The manifest

`manifest.json` replaces `fxmanifest.lua`. The complete schema — there is
nothing else:

```json
{
  "name": "my-gamemode",
  "version": "0.2.0",
  "dependencies": ["some-library"],
  "server_scripts": ["dist/server/index.js"],
  "client_scripts": ["dist/client/index.js"],
  "files": ["dist/client/index.js"]
}
```

| Field | Required | What it does |
| --- | --- | --- |
| `name` | yes | The resource's identity. Should match the directory. |
| `version` | no | Informational. |
| `dependencies` | no | Load order. Dependencies start first. |
| `server_scripts` | no | Run on the server. Their extension picks the runtime. |
| `client_scripts` | no | Packed into a generated `resource.rpf` and sent to clients. |
| `files` | no | Extra files packed for the client. |

Deliberately minimal: no `shared_scripts`, no `data_file`, no declarative
`exports` or `provide`. What is above is all of it.

You never write client-side `fxmanifest.lua` yourself — BASTON generates one
into the client packfile from this JSON.

### Which runtime runs my scripts

The extension of your `server_scripts` decides, and there is no manifest key
for it:

| Extension | Runtime | Needs bundle |
| --- | --- | --- |
| `.js`, `.mjs`, `.cjs` | deno_core / V8 | `js` or `full` |
| `.lua` | mlua / Lua 5.4 | `lua` or `full` |

A resource may use **both**, as it can in FiveM: its `.lua` scripts run in one
runtime and its `.js` scripts in another, and the resource gets one of each.
`cfx-server-data`'s `runcode` is written that way.

The two halves share everything the server owns — events, state bags, KVP, the
player directory — because none of that lives inside a runtime. An event
addressed to the resource reaches both, and a handler in either language runs.

What they do not share is `exports`, which are registered *inside* a runtime:
the Lua half cannot call an export the JavaScript half declared. That is the
same limit exports already have between resources.

One more thing to know: `RegisterZoneTransferState` is stored per resource, so
if both halves register some, only one crosses a zone handoff. The server logs
which resource that happened to.

If your bundle does not contain the runtime a resource asks for, the load fails
naming the bundle that would run it:

```
this build has no lua runtime
  → it ships in bundle lua (or full)
  → run `baston-gateway --modules` to see what this binary contains
```

A resource with **no** `server_scripts` — client-only, or a pure asset pack —
starts normally and gets no runtime at all. That is deliberate: a server with a
large streaming set would otherwise pay for an empty V8 isolate per pack.

## Streaming assets

Drop `.yft` / `.ytd` / `.ydr` / `.ydd` / … into a `stream/` directory anywhere
in the resource, at any depth. **Nothing goes in the manifest** — the directory
is scanned automatically.

```
resources/carpack/
  manifest.json          { "name": "carpack" } is enough
  stream/
    vehicles/adder2.yft
    vehicles/adder2.ytd
```

Each file is hashed (SHA-1), announced to clients by *basename* — so basenames
must be unique within a resource — and served from `/files/<resource>/<basename>`.
Caches invalidate on mtime and size, so replacing a file hot-reloads it. Files
over 4 GiB are ignored.

Details in [Streaming assets](streaming.md).

## Load order

`dependencies` decides it. BASTON topologically sorts resources and starts
dependencies first. A dependency cycle is an error, not a hang.

Nothing else affects order — there is no directory-name ordering, no priority
field.

## Hot reload

With `[dev] hot_reload = true` (the default, and the `hot-reload` module), a
change to a resource's scripts on disk restarts that resource: `onResourceStop`,
a fresh runtime, `onResourceStart`.

Two things to know:

- **State does not survive.** A reload destroys the runtime. Anything in a
  variable is gone; anything in KVP or your database is not.
- **Turn it off in production.** A deploy that writes files one at a time will
  restart the resource several times.

## Runtime control

With the `admin-api` module on, resources can be driven without touching disk:

```bash
curl -X POST -H "Authorization: Bearer $TOKEN" \
  http://localhost:8080/api/v1/resources/my-gamemode/restart
```

Actions: `start`, `stop`, `restart`. All need the `resource.control` permission
and all are written to the audit log. Scripts can do the same through the
`StartResource` / `StopResource` natives.

## Encrypted (escrow) resources

**They do not run.** A resource whose server scripts are CFX Asset
Escrow-encrypted (`.fxap`) is refused at load:

```
file is CFX-encrypted (escrow) and BASTON cannot decrypt it
```

BASTON has no way to decrypt them: doing so meant hosting an FXServer process
alongside itself, which was Windows-only and was removed
([ADR-003](../adr/003-remove-the-fxserver-sidecar.md)). Ask the resource's
author for an unescrowed build.

## When a resource does not start

| Log line | Meaning |
| --- | --- |
| `skipping resource with invalid manifest` | `manifest.json` is missing or malformed JSON. |
| `server script "…" has no runtime` | An extension BASTON does not run. |
| `server_scripts mixes js and lua` | Split it into two resources. |
| `this build has no … runtime` | Wrong bundle. Run `--modules`. |
| `no server scripts — no runtime spawned` | Not an error. Client-only resources are normal. |

If a resource starts but misbehaves, the usual cause is a native BASTON does
not implement yet. It returns a neutral value and counts
`script_native_unimplemented_total{native="…"}` — check that metric before
debugging your own code. See [native coverage](../reference/natives-gap.md).

## Next

- [Writing your first resource](../scripting/your-first-resource.md)
- [Coming from FXServer](../scripting/from-fivem.md)
- [Streaming assets](streaming.md)
