---
title: "Adding a module"
description: "Deciding which tier a capability belongs to, and wiring it without spreading gates through the codebase."
---

Read [ADR-002](../adr/002-module-tiers.md) first — it is the decision, this is
the procedure.

## First: does it belong in BASTON at all?

The rule:

> Integrate what requires privileged access to the tick loop, the network stack,
> or the process; externalise everything that can be built on the public API
> surface.

Operational test: *could a competent developer build this in JS or Lua on top of
the public natives and get a correct result?* If yes, it is a Tier 3 addon —
write it as a resource, and the answer to putting it in the core is no.

An administration dashboard is the canonical example: it consumes `/api/v1` with
a scoped key, deploys on its own cadence, and cannot take the server down.

## Then: which tier?

| | Tier 1 — module | Tier 2 — capability |
| --- | --- | --- |
| Mechanism | compiled in, runtime toggle | Cargo feature, shipped as a bundle |
| Use when | inert when off, no new dependency | it changes the dependency graph |
| Examples | `voice`, `metrics`, `admin-api`, `profiler` | `scripting-lua`, `escrow`, `db` |

The test for Tier 2: **would an operator who does not want this have to compile,
download, or be exposed to a dependency tree because of it?** V8 and sqlx
qualify; an HTTP route does not.

Default to Tier 1. Tier 2 costs a bundle in the CI matrix.

## Wiring a Tier 1 module

### 1. Register it

`crates/baston-modules/src/lib.rs` — append to the enum (the discriminants index
a bitmask, so order is stable), and add it to `ALL`:

```rust
pub enum ModuleId {
    // …
    Escrow = 8,
    Db = 9,
    MyThing = 10,        // append
}
```

Then fill in `slug`, `tier`, `summary`, `config_section`, `default_enabled`,
`is_compiled_in` and `provided_by`.

**Default to off** for anything that opens a listener or widens what a caller
can do. The exceptions are capabilities that define the product — `voice` is on
because a headline feature an operator must discover in the docs effectively
does not exist.

A test asserts `ALL` covers every discriminant, so forgetting it fails the build
rather than silently vanishing from every report.

### 2. Give it configuration

If it has settings, add a section in `crates/baston-config/src/lib.rs` and
declare it in `config_section()`. Validate it **only when the module is on**:

```rust
if self.enabled_modules.is_enabled(ModuleId::MyThing) {
    self.my_thing.validate()?;
}
```

A section left in a config with the module off is normal and must not refuse
boot.

Every validation error names the fix. That is the house rule and it is not
optional.

### 3. Gate it at exactly one place

Where it spawns or registers, in the binary:

```rust
if modules.is_enabled(ModuleId::MyThing) {
    my_thing::spawn(&config.my_thing)?;
}
```

**That is the only gate.** If you find yourself writing a second one deeper in,
stop — you are missing a trait. The pattern that works is the one voice and db
already use: define a trait in the consumer crate, implement it in the gateway
over the real service, and hand `None` when the module is off. The consumer then
has no conditional at all.

### 4. Document it

- `docs/server/modules.md` — the module table
- `docs/reference/configuration.md` — its settings
- `docs/reference/metrics.md` — any metrics it emits
- `baston.toml` — a commented example section

## Wiring a Tier 2 capability

Everything above, plus:

### Cargo features, forwarded twice

```toml
# crates/baston-gateway/Cargo.toml
[features]
my-thing = ["dep:baston-my-thing", "baston-modules/my-thing"]
```

**Both halves matter.** Forwarding to the implementing crate makes the code
exist; forwarding to `baston-modules` makes `--modules` report it. A build that
does the first and not the second under-reports what it contains.

Internal dependency edges take `default-features = false`, so a bundle without
the capability cannot pull it in transitively.

### Compiled-in detection

```rust
Self::MyThing => cfg!(feature = "my-thing"),
```

Then a build cannot misreport itself: `--modules` derives everything from the
features that are actually on.

### The bundle matrix

Add it to a bundle in `.github/workflows/ci.yml` — usually `full` — and to the
build commands in `docs/server/modules.md`.

Arbitrary combinations remain buildable but unsupported; that is what bounds the
matrix at four instead of 2ⁿ.

### Keep the crate out of the consumer

A capability with a heavy dependency should not put it in a shared crate. `db`
is the reference: `baston-scripting` defines a `DbAccess` trait and never sees
sqlx; the gateway implements it over the real pool.

## Checklist

- [ ] Tier decided against ADR-002's test, not by preference.
- [ ] Registered in `baston-modules` and in `ALL`.
- [ ] Off by default unless it defines the product.
- [ ] **One** gate, at the registration site.
- [ ] Config validated only when enabled; errors name the fix.
- [ ] Tier 2: features forwarded to *both* the implementing crate and
      `baston-modules`.
- [ ] Tier 2: added to a CI bundle.
- [ ] `--modules` reports it correctly in every bundle.
- [ ] Docs updated: modules, configuration, metrics, `baston.toml`.

## Verifying

```bash
cargo run -p baston-gateway --bin baston-gateway -- --modules
```

Check all three states are right: **on**, **off** (compiled in, switched off)
and **absent** (needs another bundle). Collapsing those three is how support
threads go in circles.

Then confirm the module really is inert when off — no thread, no listener, no
allocation. "Off" must be indistinguishable from "absent" at runtime.

## Next

- [ADR-002 — the four-tier module system](../adr/002-module-tiers.md)
- [Crates](crates.md)
