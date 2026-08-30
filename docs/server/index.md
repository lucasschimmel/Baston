---
title: "Running a BASTON server"
description: "For anyone standing up a server — from a private one for friends to a public, multi-zone deployment."
---

This track is for the person who runs the server. If you are writing resources,
go to [Scripting](../scripting/index.md). If you are working on BASTON itself,
go to [Developing BASTON](../develop/index.md).

## What BASTON is

A FiveM server core written from scratch in Rust. The protocol was
reverse-engineered, so a stock, unmodified FiveM client connects to it and
cannot tell the difference. It is **not** a fork of the C++ FXServer, and that
has consequences you will hit in the first ten minutes:

| FXServer | BASTON |
| --- | --- |
| `server.cfg` | `config/baston.toml` |
| `resources.cfg` and `ensure` | nothing — resources are discovered automatically |
| `fxmanifest.lua` | `manifest.json` |
| everything compiled in | [modules and bundles](modules.md) |

If you come from FXServer, read [Coming from FXServer](../scripting/from-fivem.md)
before you port anything. Most of what you know still applies; the parts that do
not, fail loudly rather than silently.

## Is BASTON ready for your server?

Honest answer, so you do not find out the hard way.

**It works well for**

- A private server for friends. This is the case BASTON is most solid at today.
- Development and prototyping — hot reload, a real profiler, metrics out of the box.
- JavaScript resources. The JS runtime is the most complete surface.
- Experimenting with multi-zone topologies that FXServer cannot express.

**Be careful with**

- **Native coverage.** BASTON implements a large but incomplete set of server
  natives. A resource calling one that is missing gets a neutral value and a
  log line, not a crash — which means a subtly wrong gamemode rather than an
  obvious failure. Watch `script_native_unimplemented_total`
  ([metrics](../reference/metrics.md)) and read the
  [coverage list](../reference/natives-gap.md).
- **Escrow assets.** Supported, but only on Windows, and only for scripts —
  encrypted `stream/` assets are out of scope.
- **Version.** This is `0.1.0-alpha`. There is no upgrade guarantee between
  versions yet.

**It does not do**

- HTTPS on the game port. See [the `[tls]` note](../reference/configuration.md#tls--deliberately-absent).
- Cross-resource `exports` between resources.

## Start here

1. **[Quickstart](quickstart.md)** — a server you can actually join, from nothing.
2. **[Installing resources](resources.md)** — how BASTON finds and runs them.
3. **[Modules and bundles](modules.md)** — what your binary contains.

## Then, as you need it

| | |
| --- | --- |
| [Using a database](../scripting/database.md) | SQLite, PostgreSQL or MySQL for your resources |
| [Voice](voice.md) | the embedded Mumble server |
| [Streaming assets](streaming.md) | vehicles, clothes, props, map pieces |
| [Going public](going-public.md) | CFX licence, server list, and the security checklist |
| [Multi-zone](multi-zone.md) | splitting the map across processes |
| [Monitoring](monitoring.md) | Prometheus, Grafana, the admin API |
| [Troubleshooting](troubleshooting.md) | when it does not work |

## Reference

- [Configuration reference](../reference/configuration.md) — every setting
- [Metrics reference](../reference/metrics.md) — every metric
- [Monitoring and control API](../reference/api.md) — `/api/v1`
- [Native coverage](../reference/natives-gap.md) — what is implemented
