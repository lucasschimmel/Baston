---
title: "ADR-002 — Four-tier module system"
description: "Core, modules, capabilities and addons: the rule that decides where a feature lives."
---

Date: 2026-08-30
Status: accepted — amended by [ADR-003](003-remove-the-fxserver-sidecar.md)

> **Amendment (2026-08-30).** The `escrow` capability named below was removed
> with the FXServer sidecar; see
> [ADR-003](003-remove-the-fxserver-sidecar.md). Tier 2 now holds
> `scripting-js`, `scripting-lua` and `db`. Nothing else in this ADR changes —
> the tier rule is what it decided, not the list of capabilities that happened
> to exist when it was written.

## Context

Baston is growing a long tail of capabilities that not every deployment wants:
an embedded Mumble voice server, a Prometheus exporter, a monitoring/control
HTTP API, an in-game debug overlay, a script profiler, CFX Asset Escrow, and —
soon — a second scripting language and a database access layer.

Two failure modes are already visible.

The first is architectural. Every capability that lands in the core makes the
core the owner of that capability's dependencies, its attack surface, its
support burden, and its dependency-graph weight. `deno_core`/V8 alone dominates
the build: the workspace carries `opt-level = 1` in the dev profile purely
because V8 is unusable unoptimised. Adding a second scripting VM next to V8
inside one binary compounds that cost for every operator, including the ones who
will only ever run one of the two.

The second is doctrinal. Without a stated rule for what belongs in the core,
every feature request is adjudicated ad hoc, and "no" is indistinguishable from
"not yet". A module boundary is how a project says no to a feature without
saying no to the user.

An informal version of the answer already exists in the tree and predates this
ADR:

- `baston-escrow-plugin` is a separate crate behind a `escrow` Cargo feature,
  and `baston-zone` documents that "the core never depends on this crate".
- `config.voice.enabled` and `config.metrics.enabled` already gate a listener
  each at exactly one point in `main()`.

The mechanism is therefore not new. What is missing is a name, a rule for
deciding which tier a capability belongs to, a uniform way to configure it, and
a distribution story.

### What the cost actually is

The Unreal Engine analogy — install-time packages that swing an install between
30 GB and 90 GB — is a poor guide here, because it optimises disk footprint for
a workstation. Baston is a server process. Its real costs, in descending order
of importance:

1. CPU per tick and per player.
2. Resident memory.
3. Attack surface: every listener, every parser, every credential path.
4. Configuration and cognitive surface, which gates adoption.
5. Startup latency.
6. Binary size — which, next to a GTA server's asset tree, is noise.

A module system for Baston is therefore justified primarily by *not running*
code that was not asked for, and only marginally by *not shipping* it. The
single exception is a scripting VM, where the dependency graph, the build time
and the resident footprint are all large enough that compile-time exclusion is
warranted. That exception is what forces a tier above a simple runtime toggle.

## Decision

Capabilities are classified into four tiers. The tier determines the mechanism,
and the mechanism is not negotiable per capability.

### Tier 0 — Core

The FiveM wire protocol, ENet transport, entity synchronisation, resource
loading, events, state bags, configuration, player sessions, KVP, and the HTTP
gateway.

**Test:** if disabling it breaks compatibility with a stock FiveM client, it is
core. Core is never optional and is never gated.

### Tier 1 — Modules

Compiled into the binary unconditionally, toggled at runtime, and *off by
default unless the capability defines the product*. Cost when off must be
indistinguishable from absence: no thread, no listener, no allocation, no
generated certificate, no background task.

Current members: `voice`, `metrics`, `admin-api`, `debug-overlay`, `profiler`,
`hot-reload`.

**Test:** the capability is inert when unused and adds no dependency that would
otherwise be absent.

Instrumentation stays in the core even when its consumer is a module. The
`metrics` facade compiles to a near-no-op when no recorder is installed, so
`metrics::counter!` call sites remain unconditional in core code and only the
*exporter* is a module. (Label arguments are still evaluated at the call site:
core code must not build label strings with `format!` in hot paths.)

### Tier 2 — Capabilities

Selected at build time via Cargo features, because enabling them changes the
dependency graph. Shipped to operators as prebuilt **bundles**, so that
selecting a capability never requires the operator to own a Rust toolchain.

Current members: `scripting-js` (deno_core/V8), `scripting-lua` (mlua/Lua 5.4),
and `db` with a driver per backend (`sqlite`, `postgres`, `mysql`).

Lua 5.4 rather than LuaJIT because `luajit-src` bootstraps through `minilua`,
which does not build on every target BASTON ships; CFX supports both (`lua54`),
and the interpreter is swappable behind the same feature later.

**Test:** enabling it pulls a dependency tree that an operator who does not want
the capability should not have to compile, download, or be exposed to.

### Tier 3 — Addons

Out-of-process, or written in a scripting language Baston already hosts. Not
part of the binary, not part of this repository's release artefacts, and not
gated by anything.

Baston already provides the two integration surfaces this tier needs: the NATS
bus that carries gateway/zone traffic, and the monitoring/control HTTP API with
scoped, per-key permissions (`monitor.read`, `resource.control`, `player.kick`,
`zone.drain`, `profiler.*`, `console.execute`) and an audit log.

**Consequence:** an administration dashboard is a Tier 3 addon, not a module. It
consumes the API with a scoped key, deploys on its own cadence, and cannot take
the server down when it fails. The existing Prometheus/Grafana setup under
`deploy/monitoring/` is already exactly this and is the reference example.

### The selection rule

> Integrate what requires privileged access to the tick loop, the network
> stack, or the process; externalise everything that can be built on the public
> API surface.

Operational test: *could a competent developer build this in JS or Lua on top of
the public natives, and get a correct result?* If yes, it is Tier 3 and the
answer to a request for it in the core is no.

The corollary matters as much: a database access layer is *not* Tier 3, because
implementing connection pooling and non-blocking queries on top of raw sockets
from a script thread produces a structurally wrong result — this is precisely
why `oxmysql` exists in the FiveM ecosystem. The driver and pool are Tier 2 and
ship as the `db` module; the ORM, schema and business logic stay Tier 3.

`db` also settles the MySQL question. The industry has moved to PostgreSQL, but
the FiveM ecosystem has not: every existing resource speaks MySQL, and a server
migrating to BASTON arrives with a MariaDB dump. Shipping only PostgreSQL would
not express a preference, it would refuse the migration. So the module ships
three drivers — SQLite as the zero-configuration default, PostgreSQL as the
documented recommendation, MySQL as the compatibility bridge — and the operator
chooses.

### Bundles

Arbitrary Tier 2 combinations are buildable from source but not supported. A
small set of bundles is built and tested in CI:

| Bundle | Tier 2 capabilities | Intent |
| --- | --- | --- |
| `lite` | none | zone worker, relay, benchmarking |
| `js` | `scripting-js` | **default** |
| `lua` | `scripting-lua` | Lua-only servers |
| `full` | `scripting-js`, `scripting-lua`, every `db` driver | migration and mixed estates |

This bounds the test matrix at four, instead of 2^n.

### Naming

The three concepts are named **module** (Tier 1), **capability**/**bundle**
(Tier 2) and **addon** (Tier 3).

"Crate" is explicitly rejected as a user-facing name. This repository is a Cargo
workspace whose members live in `crates/`; overloading the word would make the
code, the issue tracker and the documentation ambiguous for no benefit.

## Consequences

### Positive

- Operators run only what they asked for, and can prove it: the boot banner and
  `--modules` report the resolved set.
- Attack surface shrinks by default. `admin-api`, `debug-overlay` and
  `profiler` are control surfaces that most deployments never use.
- A second scripting language becomes tractable: `scripting-lua` is additive
  and does not tax JS-only operators.
- Feature requests acquire a default answer. Most are Tier 3, and Tier 3 costs
  the core nothing.
- The tier assignment is a design constraint that survives contributor
  turnover, unlike a convention.

### Negative

- Four bundles multiply release artefacts and CI time.
- A capability behind a Cargo feature can break without the default build
  noticing; CI must build every bundle, not just the default.
- Operators gain a way to misconfigure: enabling a `[section]` whose module is
  off. Mitigated by making that a hard, actionable configuration error rather
  than silence — see "Configuration" below.

### Configuration

Modules resolve from a `[modules]` section. Legacy per-section flags
(`[voice] enabled`, `[metrics] enabled`, `[debug] display_info`) continue to
work and continue to be authoritative for their own module, so existing
`config/baston.toml` files keep their meaning. Where both are present and disagree, the
load fails with an error naming both sites rather than silently picking one.

The single most likely operator failure mode is editing a section and observing
nothing happen, so silence is never an acceptable outcome. Two cases, two
answers: two configuration sites that *contradict* each other fail the load,
because one of them is a bug; a section that is merely inert because its module
is off produces a warning naming both, because leaving a `[voice]` block in a
file with voice switched off is normal and refusing to boot over it would be
hostile.

### Gate placement

A module is gated at exactly one point: where it registers or spawns. Neither
`if config.x.enabled` nor `#[cfg(feature = "x")]` may appear inside domain
logic. If a Tier 2 capability needs conditional behaviour deeper than its
registration site, that is evidence of a missing trait, not a reason for a
second gate.

## Alternatives considered

**Cargo features as the operator-facing mechanism.** Rejected: it requires every
operator to build from source. Cargo features remain the *implementation* of
Tier 2, with bundles as the operator-facing artefact.

**Dynamically loaded native plugins (`cdylib` + `libloading`).** Rejected for
the foreseeable future. Rust has no stable ABI, so every compiler or Baston
version bump would break every third-party plugin; loading foreign code into the
game process adds `unsafe` to the critical path, makes a plugin's crash
indistinguishable from a Baston crash in bug reports, and creates a support
burden out of proportion to the benefit. Tier 3 delivers the extensibility that
motivates dynamic loading, out of process, with a supportable failure boundary.

**One fat binary with runtime toggles only.** This is Tier 1, and it is the
right answer for everything except a scripting VM. It is rejected only as a
*universal* answer, because it would force V8 and a Lua VM into the same binary.

**A plugin marketplace.** Out of scope. A marketplace is a product — trust,
signing, moderation, versioning — and is separable from the architecture. The
architecture is what is decided here; the marketplace can be built later on
Tier 3 without revisiting this ADR.

## References

- `docs/guides/modules.md` — operator-facing module and bundle reference.
- ADR-001 — CFX trust broker (superseded), which is where the removed `escrow`
  capability's Tier 2 placement came from.
- [ADR-003](003-remove-the-fxserver-sidecar.md) — removes it.
