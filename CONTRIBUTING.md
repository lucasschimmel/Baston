# Contributing

## Where things live

The repository is a monorepo with one directory per kind of thing. The full map
is in the [README](README.md); what matters when you are about to change
something:

| You are changing | It lives in |
| --- | --- |
| the server | `crates/` |
| documentation | `docs/` — Markdown, the source of truth |
| the documentation website | `apps/docs/` — renders `docs/`, never a copy |
| a `baston.toml` variant | `config/` |
| Docker, Prometheus, Grafana | `deploy/` |
| a sample or fixture resource | `examples/resources/` |
| a developer script | `tools/` |

Two independent toolchains: Cargo builds the server, pnpm builds the website.
Neither needs the other.

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

pnpm install
pnpm docs:build          # also fails on any broken documentation link
```

An asset a crate compiles into itself belongs to that crate — see
`crates/baston-cfx-platform/assets/`. Nothing that ships inside a binary should
sit in a shared top-level directory.

## Branching workflow

This repo uses a lightweight Gitflow. No remote is configured yet — this
document holds regardless of where it's eventually pushed.

## Branches

- **`main`** — releases only. Every commit on `main` is either a merge from
  `develop` or a tag. Nothing is ever committed directly on `main`; a local
  `pre-commit` hook (`.githooks/pre-commit`, wired via `core.hooksPath`)
  refuses direct commits on it.
- **`develop`** — integration branch. All day-to-day work happens here or on
  short-lived branches cut from it.
- **`feature/<name>`**, **`fix/<name>`**, **`chore/<name>`** — cut from
  `develop`, merged back into `develop` when done. Delete after merge.

## Cutting a release

```sh
git checkout main
git merge --no-ff develop
git tag -a vX.Y.Z -m "vX.Y.Z"
git checkout develop
```

## Commits

Conventional Commits, scoped to the crate/area touched (matches existing
history: `feat(baston): ...`, `fix(baston): ...`, `docs(baston): ...`,
`chore(baston): ...`). No `Co-Authored-By` trailers crediting AI tooling.
