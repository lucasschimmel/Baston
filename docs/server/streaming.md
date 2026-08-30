---
title: "Streaming assets"
description: "How BASTON discovers and serves stream/ assets to clients."
---

BASTON serves streamed assets (custom vehicles, clothing, weapons, map props —
`.yft`, `.ytd`, `.ydr`, `.ydd`, …) exactly like FXServer: drop files in a
resource's `stream/` folder (any depth of subfolders), start the resource,
done. No manifest declaration is needed — the folder is auto-scanned.

```
resources/
  carpack/
    manifest.json          # { "name": "carpack" } is enough
    stream/
      vehicles/adder2.yft
      vehicles/adder2.ytd
```

## How it works

- On `getConfiguration`, the gateway scans `stream/`, hashes each file (SHA1)
  and advertises them in `streamFiles`, keyed by **basename**, with the RSC
  metadata the client's streamer needs (`rscVersion`, `rscPagesVirtual/
  Physical` parsed from RSC5/RSC7/RSC8 headers; raw files carry size only).
- The client downloads each asset at `/files/<resource>/<basename>` and
  validates the hash. The download route resolves basenames through the
  streaming list, like FXServer's `FilesHttpHandler`.
- The scan is cached and invalidated by mtime+size fingerprint — replacing a
  file on disk is picked up on the next `getConfiguration` (hot reload).
- A resource with only streamed assets (no scripts) still gets a
  manifest-only `resource.rpf` so the client can mount it.

## Limits & notes

- Basenames must be unique within a resource (the wire format is a flat map);
  a duplicate in nested folders logs a warning and the later path wins.
- `X.stream_raw` companions are supported: the RSC header is read from the
  companion, content and hash from `X`.
- Files over 4 GiB are skipped with a warning (u32 size on the wire).
- CFX escrow-encrypted **stream** assets (`.yft`/`.ydd`/`.ydr` via escrow) are
  out of scope — escrow support covers scripts only (see
  `escrow-support.md`).
