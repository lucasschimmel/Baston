# CFX Asset Escrow support (Phase D-bis)

BASTON can run CFX Asset Escrow (encrypted) resources through an optional plugin,
`baston-escrow-plugin`. The core runtime has **no** dependency on
`svadhesive.dll`; escrow support is opt-in, off by default, and never activates
implicitly.

## How it works

```
ResourceManager ──reads raw bytes──▶ ScriptDecryptor (baston-core trait)
                                        ├── PlainDecryptor (default, no-op)
                                        └── SidecarDecryptor (baston-escrow-plugin)
                                                 │  JSON line protocol (stdin/stdout)
                                                 ▼
                                        FXServer subprocess + svadhesive.dll
                                                 (decrypts via its VFS hook)
```

A file is treated as encrypted when it starts with the CFX `FXAP` magic. Plain
files bypass the decryptor entirely (zero overhead). Decryption happens **once,
at resource start**, and the plaintext is held in memory for the resource's
lifetime — never per HTTP request.

### Why a sidecar and not direct FFI

Preliminary research (`svadhesive.dll` export table) found a **single** named
export, `CreateComponent` — the opaque CitizenFX component factory. There is no
flat, FFI-callable decrypt function: svadhesive decrypts through an internal C++
component interface wired as a VFS hook, keyed on the server licence fetched at
runtime. Calling it standalone would require booting the whole CitizenFX
component host. The `direct` backend is therefore **unsupported** and returns an
actionable error; use `backend = "sidecar"`.

## Prerequisites

- **Windows.** `svadhesive.dll` is a Windows binary; escrow support is
  Windows-only by nature.
- A local **FXServer** install (for `FXServer.exe` + `svadhesive.dll`). This repo
  ships one under `Artifacts/windows/<build>/`.
- A valid **CFX server licence** (`license:...`) with entitlements for the
  escrowed resources.

## Configuration

1. Build the zone binary with the escrow feature (Windows):

   ```bash
   cargo build -p baston-zone --features escrow
   ```

2. Configure `baston.toml`:

   ```toml
   [escrow]
   enabled = true
   backend = "sidecar"
   server_license = "license:REPLACE_ME"
   fxserver_path = "Artifacts/windows/31623/FXServer.exe"
   ```

3. Ensure the `baston-decrypt-shim` resource is present under your resources
   directory (shipped in `resources/baston-decrypt-shim/`).

On startup you should see:

```
[baston-zone] escrow plugin active
[baston-escrow] baston-escrow sidecar started (FXServer subprocess)
[baston-zone] resource <escrow-resource>: resource script decrypted OK
```

With `enabled = false` (or on Linux, or without `--features escrow`) BASTON logs
a clear line and runs all plain resources normally — no crash.

## Metrics

The loader emits (Prometheus, `/metrics`):

- `baston_scripts_loaded_total{status="plain"}` — plain scripts loaded.
- `baston_scripts_loaded_total{status="decrypted"}` — escrow scripts decrypted.
- `baston_scripts_loaded_total{status="error"}` — decrypt failures.
- `baston_decrypt_duration_seconds` — histogram of decrypt latency.

## Limitations

- **Linux is not supported** (svadhesive is a Windows DLL).
- **NUI / web assets are not supported** — a CFX escrow limitation, not BASTON's.
- **Streaming assets** (`.yft`, `.ydd`, `.ydr`) are **out of scope** for
  Phase D-bis; only server scripts are decrypted.
- The `direct` (FFI) backend is unsupported (see above).
- The Lua shim's stdin/stdout transport depends on the FXServer build; where raw
  process stdin is unavailable, use the alternate file-drop transport (the Rust
  `SidecarDecryptor` speaks the same JSON line protocol either way).

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `escrow.enabled = true but ... built without the 'escrow' feature` | Rebuild with `--features escrow`. |
| `escrow.enabled = true but this is not a Windows build` | Escrow is Windows-only; run the Windows build. |
| `[escrow] fxserver_path "..." not found` | Point `fxserver_path` at a real `FXServer.exe`. |
| `[escrow] enabled = true but server_license is empty` | Set `server_license = "license:..."`. |
| `the 'direct' (FFI) escrow backend is not supported` | Set `backend = "sidecar"`. |
| Resource fails with `NoDecryptorAvailable` | Encrypted file but plugin not active — enable escrow or use a plain resource. |
| `sidecar did not report READY within the startup timeout` | FXServer failed to boot; check the shim resource and licence. |
