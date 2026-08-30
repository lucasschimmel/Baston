# BASTON

A from-scratch FiveM server core, written in Rust. Not a fork of the C++
FXServer: the protocol was reverse-engineered, and a stock FiveM client
connects to it unmodified.

```bash
cargo run -p baston-gateway --bin baston-gateway
```

That boots the default `js` bundle against `config/baston.toml` and the sample
resources in `examples/`. New here? Read
[docs/guides/getting-started.md](docs/guides/getting-started.md).

## What is in this repository

```
apps/       things with a user interface
  docs/       the documentation website (Astro Starlight)
crates/     the server itself — one Rust crate per concern
docs/       the documentation, in Markdown
config/     baston.toml and its variants
deploy/     Dockerfiles, compose, Prometheus, Grafana
examples/   sample and fixture FiveM resources
tools/      developer scripts
fuzz/       libFuzzer targets (nightly, out of the stable build)
```

The rule: **one directory per kind of thing**, and nothing at the root that
belongs inside one of them.

### crates/

| Crate | What it owns |
| --- | --- |
| `baston-gateway` | the FiveM-facing process: HTTP, ENet, admin API, zone mesh |
| `baston-zone` | zone process: entities, state sync, resource loading |
| `baston-protocol` | the wire protocol — packets, sync trees, natives table |
| `baston-scripting` | the script host, its natives, and both runtimes |
| `baston-modules` | the module registry (ADR-002) |
| `baston-config` | `baston.toml` and its validation |
| `baston-db` | pooled SQL for scripts (SQLite / PostgreSQL / MySQL) |
| `baston-voice` | Mumble-compatible voice server |
| `baston-core` | the script-decryptor seam |
| `baston-loadtest` | the benchmark client |

## Building

BASTON ships as **bundles** — a bundle is a build with a given set of Tier 2
capabilities. See [docs/guides/modules.md](docs/guides/modules.md).

```bash
cargo build --release -p baston-gateway                                   # js (default)
cargo build --release -p baston-gateway --no-default-features             # lite
cargo build --release -p baston-gateway --no-default-features --features scripting-lua
cargo build --release -p baston-gateway --features scripting-lua,db-postgres,db-mysql
```

What a binary actually contains:

```bash
cargo run -p baston-gateway --bin baston-gateway -- --modules
```

## The multi-zone dev environment

```bash
docker compose -f deploy/docker/docker-compose.yml up
```

Gateway, two zones, NATS, Prometheus and Grafana. Details in
[docs/operations/running.md](docs/operations/running.md).

## The documentation website

The Markdown in `docs/` is the source of truth; `apps/docs` renders it.

```bash
bun install
bun run docs:dev
```

## Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md). The short version:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI additionally builds and tests every bundle, because a capability behind a
Cargo feature can rot without the default build noticing.

## Licence

MIT — see [Cargo.toml](Cargo.toml).
