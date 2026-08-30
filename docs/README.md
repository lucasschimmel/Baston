---
title: "BASTON documentation"
description: "Everything about BASTON: running a server, writing resources, and working on the server itself."
---

BASTON is a FiveM server core written from scratch in Rust. A stock FiveM client
connects to it and cannot tell the difference. It is **not** a fork of the C++
FXServer.

Everything here is Markdown, readable on GitHub as-is. The same files are the
source of the documentation website under `apps/docs` — the site is a renderer,
never a second copy.

## Pick your path

### I want to run a server

Start at **[Running a BASTON server](server/index.md)**, then
[Quickstart](server/quickstart.md).

| | |
| --- | --- |
| [Quickstart](server/quickstart.md) | from nothing to a server you can join |
| [Installing resources](server/resources.md) | how BASTON finds and runs them |
| [Modules and bundles](server/modules.md) | what your binary contains |
| [Voice](server/voice.md) | the embedded Mumble server |
| [Streaming assets](server/streaming.md) | vehicles, clothes, props, map pieces |
| [Multi-zone](server/multi-zone.md) | splitting the map across processes |
| [Zone configuration](server/zone-config.md) | laying out bounds correctly |
| [displayinfo](server/displayinfo.md) | the in-game debug overlay |
| [Monitoring](server/monitoring.md) | Prometheus, Grafana, the profiler |
| [Going public](server/going-public.md) | CFX licence and the security checklist |
| [Troubleshooting](server/troubleshooting.md) | when it does not work |

### I want to write resources

Start at **[Writing resources](scripting/index.md)** — and read
[Coming from FXServer](scripting/from-fivem.md) before porting anything.

| | |
| --- | --- |
| [Writing resources](scripting/index.md) | the API, and which language to pick |
| [Your first resource](scripting/your-first-resource.md) | working code in both languages |
| [Events](scripting/events.md) | every event, with real signatures |
| [Natives](scripting/natives.md) | what is implemented, what is stubbed |
| [State bags and KVP](scripting/state-bags.md) | keeping state |
| [Using a database](scripting/database.md) | SQL that never blocks the tick |
| [HTTP](scripting/http.md) | serving and calling out |
| [Coming from FXServer](scripting/from-fivem.md) | the porting checklist |

### I want to work on BASTON

Start at **[Developing BASTON](develop/index.md)**.

| | |
| --- | --- |
| [Developing BASTON](develop/index.md) | architecture and house style |
| [The crates](develop/crates.md) | what each one owns |
| [Adding a native](develop/adding-a-native.md) | the most common contribution |
| [Adding a module](develop/adding-a-module.md) | new capabilities |
| [Testing](develop/testing.md) | what is covered, and the bundle matrix |

## Reference

Facts to look up.

- [Configuration](reference/configuration.md) — every setting and its default
- [Metrics](reference/metrics.md) — every Prometheus metric
- [Monitoring and control API](reference/api.md) — `/api/v1`
- [Native coverage](reference/natives-gap.md) — what is implemented

## Internals

How it works, for people changing it — or curious.

- [The wire protocol](internals/protocol.md) — how a client connects, and the
  reverse-engineered details you could not guess
- [State synchronisation](internals/state-sync.md) — interest management, the
  adaptive tick, zone handoffs
- [CFX platform handshake](internals/cfx-platform-handshake.md)
- [Code quality audit, July 2026](internals/code-quality-audit-2026-07-05.md)
- [Getting started (French, original)](internals/getting-started-fr.md) — the
  first end-to-end guide; superseded by the server track above

## Decisions

What was decided, and what it cost.

- [ADR-001 — Official FXServer as the CFX trust broker](adr/001-use-official-fxserver-as-cfx-trust-broker.md)
- [ADR-002 — Four-tier module system](adr/002-module-tiers.md)

## Operations

- [Running BASTON](operations/running.md) — topology and alerting detail
- [CFX licensing](operations/licensing.md)
- Runbooks: [local live test](operations/runbooks/local-live-test.md) ·
  [Phase C](operations/runbooks/phase-c.md) ·
  [Phase C testing](operations/runbooks/phase-c-testing.md)

## A note on honesty

BASTON is `0.1.0-alpha`. These pages state what does **not** work as plainly as
what does — unimplemented natives, state bags that do not reach clients, missing
cross-zone interest, JavaScript without timers. That is deliberate: a gap you
know about costs an afternoon, and a gap you discover in production costs a
weekend.

If you find documentation that overstates what BASTON does, that is a bug worth
reporting.

## Writing docs

- One page answers one question. If a page needs a three-level table of
  contents, it is two pages.
- Link with repository-relative paths (`../reference/api.md`) so links work on
  GitHub *and* on the website — a build-time check enforces this.
- Code an operator will paste must be complete and runnable from the repository
  root.
- Say what does not work. Every "not implemented" here saved someone a day.
