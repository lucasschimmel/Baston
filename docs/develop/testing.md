---
title: "Testing"
description: "What is tested, how to run it, and what CI adds that a local run does not."
---

## Running it

```bash
cargo test --workspace
```

That is the default `js` bundle. It does **not** cover the other three — see
[bundles](#bundles-are-not-optional) below.

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

Clippy runs with `-D warnings` in CI, so a warning is a failure. Fix it or
`#[allow]` it with a comment saying why.

## Bundles are not optional

A capability behind a Cargo feature can rot without the default build noticing.
CI builds and tests all four:

```bash
cargo test -p baston-gateway -p baston-scripting --no-default-features
cargo test -p baston-gateway -p baston-scripting
cargo test -p baston-gateway -p baston-scripting --no-default-features --features scripting-lua
cargo test -p baston-gateway -p baston-scripting --features scripting-lua,db-postgres,db-mysql
cargo test -p baston-db --features sqlite,postgres,mysql
```

Run the one you touched before pushing. If you changed anything in
`baston-scripting`, run the `lua` bundle too — it is where a JS-only assumption
shows up.

CI also runs `--modules` in every bundle, so a feature that forgets to forward
to `baston-modules` is caught.

## Feature-gating a test

A test that drives a JavaScript resource cannot run in the `lua` bundle. Gate
the whole file:

```rust
//! …
// Driven by JavaScript resources, so this runs in bundles containing V8.
#![cfg(feature = "scripting-js")]
```

Better, where it is cheap: make the test engine-agnostic. The API test fixture
writes a JS resource in the `js` bundle and a Lua one in the `lua` bundle, which
turns a JS-bundle test into proof that the API does not care about the engine.

## What is covered

| Area | Where |
| --- | --- |
| Protocol parsing and framing | `crates/baston-protocol/` unit tests |
| Sync trees, bit buffer, quaternions | `crates/baston-protocol/src/rage/` |
| Config parsing, validation, module resolution | `crates/baston-config/` |
| Module registry invariants | `crates/baston-modules/` |
| Script runtimes, natives, both engines | `crates/baston-scripting/` |
| Resource loading, topological order | `crates/baston-zone/` |
| Zone handoff, recovery, rebalancing | `crates/baston-gateway/tests/handoff_tests.rs`, `mesh.rs` |
| HTTP surface, admin API, permissions | `crates/baston-gateway/tests/` |
| Database round trips | `crates/baston-db/tests/sqlite_roundtrip.rs` |

Around 47 test suites in the default bundle.

### Tests needing NATS

The mesh integration tests **skip silently** when NATS is unreachable. A green
local run does not mean you exercised them:

```bash
docker compose -f deploy/docker/docker-compose.yml up -d nats
cargo test -p baston-gateway --test mesh_d4_tests
```

## Fuzzing

Every parser that touches attacker-controlled bytes has a target:

```
fuzz/fuzz_targets/
  decode_incoming        the inbound clone stream
  decode_downlink        the outbound format
  parse_ack, parse_nack  reliability records
  parse_object_ids       the run-length id encoding
  lz4dict_decompress     dictionary decompression
```

Nightly toolchain, and excluded from the stable workspace so CI ignores it:

```bash
cargo +nightly fuzz run decode_incoming
```

Scheduled weekly; findings do not block merges. **If you add a parser for bytes
a client controls, add a target.**

## Writing tests that earn their place

The suite has a voice; match it.

**Name the behaviour, not the function.**

```rust
#[test]
fn a_throwing_player_connecting_handler_never_strands_the_player() { … }
```

Not `test_deferrals_2`. A failing test name should tell you what broke without
opening the file.

**Assert the consequence, not the mechanics.** The deferral test asserts the
player was actually released, not that a function was called.

**Say why in the test body** when the case is subtle:

```rust
// A handler that defers and then throws would otherwise park the
// connection until the timeout — for every player, forever.
```

**Prove the negative.** A guard needs a test that it fires *and* one that it
does not:

```rust
#[test] fn the_watchdog_terminates_a_runaway_script_and_the_runtime_survives() { … }
#[test] fn the_watchdog_does_not_fire_on_a_normal_dispatch() { … }
```

**Make time a parameter.** The Lua watchdog's budget is a field precisely so
tests exercise the real arming path in milliseconds instead of waiting out ten
seconds. A test that sleeps for a production timeout is a test people will
delete.

## Things that will trip you

**A corrupt PDB on Windows.** `LNK1285: corrupt PDB file` is a stale build
artifact, not your code:

```bash
cargo clean -p baston-gateway
```

**Stale metadata after a feature change.** Making a dependency optional can
leave incompatible artifacts and produce an internal compiler error. Same fix.

**The first build is long.** V8. Iterate on the `lua` bundle if you are not
touching the JS path.

## The documentation build is a test

```bash
bun run docs:build
```

It fails on any broken internal documentation link. If you move or rename a
page, this is what tells you what you broke.

## Next

- [Crates](crates.md)
- [Adding a native](adding-a-native.md)
