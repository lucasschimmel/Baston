---
title: "BASTON documentation"
description: "Guides, reference, operations and the decisions behind a from-scratch FiveM server core."
---

Everything here is Markdown, readable on GitHub as-is. The same files are the
source of the documentation website under `apps/docs` — the site is a renderer,
never a second copy.

## Start here

| | |
| --- | --- |
| [Getting started](guides/getting-started.md) | boot a server, load resources, go multi-zone |
| [Modules and bundles](guides/modules.md) | what your binary contains and how to switch it |

## Guides

Doing a thing.

- [Getting started](guides/getting-started.md)
- [Modules, bundles and addons](guides/modules.md)
- [Zone configuration](guides/zone-config.md)
- [Streaming assets](guides/streaming.md)
- [The `displayinfo` overlay](guides/displayinfo.md)

## Reference

Facts to look up.

- [Monitoring and control API](reference/api.md) — `/api/v1`, permissions, audit
- [Server native coverage](reference/natives-gap.md) — what is implemented, and what is not

## Operations

Running it in anger.

- [Running BASTON](operations/running.md) — topology, zones, monitoring, alerting
- [CFX licensing](operations/licensing.md)
- [Asset escrow](operations/escrow.md)
- Runbooks: [local live test](operations/runbooks/local-live-test.md) ·
  [Phase C](operations/runbooks/phase-c.md) ·
  [Phase C testing](operations/runbooks/phase-c-testing.md)

## Internals

How it works, for people changing it.

- [CFX platform handshake](internals/cfx-platform-handshake.md)
- [Code quality audit, 2026-07-05](internals/code-quality-audit-2026-07-05.md)

## Decisions

Architecture Decision Records — what was decided, and what it cost.

- [ADR-001 — Official FXServer as the CFX trust broker](adr/001-use-official-fxserver-as-cfx-trust-broker.md)
- [ADR-002 — Four-tier module system](adr/002-module-tiers.md)

## Writing docs

- One page answers one question. If a page needs a table of contents three
  levels deep, it is two pages.
- Link with repository-relative paths (`../reference/api.md`), so a link works
  both on GitHub and on the website.
- A code block an operator will paste must be complete and runnable from the
  repository root.
