# Branching workflow

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
