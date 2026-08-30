---
title: "Developing BASTON"
description: "How the server is put together, and where to make a change."
---

For people changing BASTON itself. If you write resources, go to
[Scripting](../scripting/index.md).

## The shape of it

Two processes, one binary each, and a rule about who owns what.

```
                    ┌──────────────────────────────────────────────┐
FiveM client ──────▶│ baston-gateway                               │
   ENet + HTTP      │                                              │
                    │  http/     the FiveM-facing endpoints        │
                    │  udp/      ENet, OneSync, interest, ingress  │
                    │  api/      monitoring and control            │
                    │  mesh/     zone registry, handoffs           │
                    └───────┬──────────────────────────────┬───────┘
                            │ gRPC                         │ NATS
                    ┌───────▼──────────────────────────────▼───────┐
                    │ baston-zone  ×N                              │
                    │  entities · state sync · resources · scripts │
                    └──────────────────────────────────────────────┘
```

**The gateway is the only process a client ever talks to.** It owns the
protocol, authentication, the routing table, OneSync and interest management.

**A zone owns entities and runs resources** for a rectangle of the map. Clients
never connect to one. In single-process mode the gateway does the zone's work
in-process; the code is the same.

## The three seams that matter

Understanding these three explains most of the codebase.

### 1. The natives are engine-neutral

`crates/baston-scripting/src/natives/` implements the CFX natives against a
`NativeState` type-map — not against V8. `extensions/` (V8) and `lua.rs` (mlua)
are thin bridges that convert their values to JSON and call in.

**One implementation, two engines.** Logic that accumulates in an engine bridge
is logic the other engine will not get. Keep bridges thin.

### 2. Modules are gated at exactly one point

A capability registers or spawns in one place, and that is the only place it is
gated. Neither `if config.x.enabled` nor `#[cfg(feature = "x")]` belongs in
domain logic.

If a capability needs conditional behaviour deeper than its registration site,
that is a missing trait, not a second gate. See
[ADR-002](../adr/002-module-tiers.md).

### 3. Every resource runtime owns a thread

V8 isolates are `!Send`, and V8 panics if two isolates share a thread. So each
resource gets a dedicated OS thread with its own current-thread tokio runtime,
and the host holds a `Send` handle and talks to it over a channel.

This is why a runaway script wedges only its own resource, and why dispatches
carry a watchdog.

## Where to make a change

| You want to | Go to |
| --- | --- |
| Add or fix a native | [Adding a native](adding-a-native.md) |
| Add a capability | [Adding a module](adding-a-module.md) |
| Change the wire protocol | [`crates/baston-protocol/`](../internals/protocol.md) |
| Change entity sync | [`crates/baston-zone/`](../internals/state-sync.md) |
| Add an HTTP endpoint | `crates/baston-gateway/src/http/` or `api/` |
| Change configuration | `crates/baston-config/src/lib.rs` |

The crate map is in [Crates](crates.md).

## Building

Two independent toolchains. Cargo builds the server; pnpm builds the docs site.
Neither needs the other.

```bash
cargo build --release -p baston-gateway     # the js bundle (default)
cargo run -p baston-gateway --bin baston-gateway -- --modules
```

The first build compiles V8 and takes a while. `--no-default-features
--features scripting-lua` builds without V8 and is much faster to iterate on if
you are not touching the JS path.

The dev profile carries `opt-level = 1` because V8 is unusable unoptimised.

## Before you push

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI additionally builds and tests **every bundle**, because a capability behind a
Cargo feature can rot without the default build noticing. Run the one you
touched:

```bash
cargo test -p baston-gateway -p baston-scripting --no-default-features --features scripting-lua
```

See [Testing](testing.md).

## House style

The codebase has a consistent voice; matching it matters more than any
individual rule.

- **Comments explain *why*, never *what*.** The code says what. A comment that
  restates it is noise; a comment carrying a constraint, a reverse-engineered
  fact or a rejected alternative is the most valuable thing in the file.
- **Errors name the fix.** Every configuration error says what to change. A
  refusal that leaves the reader guessing is a bug.
- **No `unwrap()` on anything touching external input.**
- **Neutral, not silent.** An unimplemented native returns a neutral value,
  warns once, and increments a counter — never fails silently, never panics.
- **Flag what is a stub.** Doc comments say so explicitly. Half the value of
  this codebase's comments is knowing what *not* to trust.

Conventional Commits, scoped to the area touched. No `Co-Authored-By` trailers
crediting AI tooling.

## Reading order

If you are new to the codebase:

1. [Crates](crates.md) — what each one owns
2. [The wire protocol](../internals/protocol.md) — how a client connects
3. [State synchronisation](../internals/state-sync.md) — how the world moves
4. [ADR-002](../adr/002-module-tiers.md) — why the module boundaries are where
   they are

## Next

- [Crates](crates.md)
- [Adding a native](adding-a-native.md)
- [Adding a module](adding-a-module.md)
- [Testing](testing.md)
