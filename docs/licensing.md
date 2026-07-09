# CFX server-licence integration

BASTON runs on the **official** FiveM/CFX licence system. It requires a real
licence and honours whatever that licence grants — **without** reimplementing,
spoofing, or bypassing anything.

## Principle — how this stays 100% legal

The one piece the open-source FXServer does **not** ship is the licence
validation itself: it lives in the closed CFX component (`svadhesive`, the
server *adhesive*). BASTON does **not** reverse-engineer or reimplement it.
Instead, in the strongest mode, BASTON runs a **genuine, unmodified FXServer**
as a sidecar — the component's own native host — which validates the operator's
key against CFX exactly as it always does. BASTON then **reads the local
verdict** and enforces it. In short:

- ✅ The official binary runs unmodified, in its intended host, with the
  operator's own licence.
- ✅ BASTON only *reads* the result and enforces it **restrictively** (it can
  lower a limit or keep a feature off — never raise a limit or unlock a feature
  that was not granted).
- ⛔ BASTON never talks to a CFX service, never replays a token, never
  impersonates FXServer, never patches or decompiles `svadhesive.dll`.

The only component that ever contacts CFX is the genuine FXServer, doing what it
normally does. That is the whole compliance argument.

## Prerequisites

- A CFX account and a **server licence key**, created at <https://portal.cfx.re>.
- For `verified` mode: an **official FXServer** you downloaded yourself from CFX
  (Windows). BASTON never ships `FXServer.exe` or `svadhesive.dll` — the
  operator provides them, exactly like the `[escrow]` setup.

## The three modes (`baston.toml` → `[license]`)

```toml
[license]
mode = "off"            # "off" | "gate" | "verified"
sv_license_key = ""     # your key from https://portal.cfx.re
# fxserver_path = "Artifacts/windows/31623/FXServer.exe"   # verified only
# sidecar_port = 30130  # private, localhost-only port for the sidecar (verified/escrow)
```

| Mode | What it does | Requires | Use when |
|------|--------------|----------|----------|
| `off` | No check. Warns every boot. | nothing | Local dev / LAN only. **Not** production. |
| `gate` | Requires a well-formed `sv_license_key` in config (shape only — not validated, no sidecar). | a key | Cross-platform prod where you can't run the sidecar, as a minimum bar. |
| `verified` | Runs the official FXServer sidecar, which validates the key against CFX; BASTON enforces the verdict + entitlements locally. | Windows + `--features escrow` + `fxserver_path` + a real key | Production on Windows. Recommended. |

In `verified` mode BASTON **fails closed**: an invalid or banned licence — or a
sidecar that doesn't answer within the startup budget — **refuses to start**,
with an actionable message. It never boots optimistically.

### One process for licence + escrow

If both `[license] mode = "verified"` and `[escrow] enabled = true`, BASTON
starts **one** FXServer sidecar and uses it for both — it never boots a second
FXServer.

### How `verified` talks to the component (technical)

- BASTON writes a private launch config carrying your `sv_licenseKey` (kept off
  the command line and out of every log) and starts the genuine `FXServer.exe`
  **off the public server list** (`sv_master1 ""`, never `sv_lan` — LAN mode
  would suppress the very licence validation we rely on), bound to a private
  localhost port (`sidecar_port`) so it never clashes with BASTON's public port.
- The component validates the key against CFX exactly as it always does, and
  publishes the verdict locally as the `sv_licenseKeyToken` convar.
- A tiny materialised resource (`baston-cfx-shim`) reads that convar and answers
  BASTON over a **file-drop** channel (request/response files under its `ipc/`
  dir). This is used because the CitizenFX server Lua sandbox exposes no
  `io.read`; it runs only at boot (and, for escrow, at resource load) — never on
  a hot path, so it does not affect BASTON's runtime performance.
- BASTON reads the verdict and **fails closed**: no valid token within the
  startup budget → it refuses to start.

Run several BASTON instances on one host? Give each a distinct `sidecar_port`.

## What gets enforced

- **Validity gate** — on a valid licence, BASTON starts; otherwise it refuses.
  This is the clean, locally-readable signal (`sv_licenseKeyToken` present).
- **Slot cap** — if the licence entitlement reports a maximum slot count,
  `max_players` is capped to it (never raised). If no entitlement signal is
  locally available, `max_players` is left as configured — BASTON presumes
  **no** grant it can't confirm.
- **Features** — a feature stays enabled only if it is both requested and
  granted. Unconfirmed grants never enable anything.

## Out of scope (deliberately not implemented) — the compliance boundary

The following are **not** done by BASTON, because doing them from a non-FXServer
binary would require impersonating FXServer to CFX (spoofing) or replaying
platform tokens — outside the legal line above:

- **Public server-list presence** (registering on the CFX server list).
- **Client policy features via `policy-live`** (e.g. custom-clothing streaming,
  pool increases).

Realising these cleanly requires either running a **genuine FXServer as the
network-facing front** (which *is* legitimately the holder of CFX's trust
chain), or an **explicit authorisation / API from CFX**. If you need them,
contact CFX or front BASTON with a real FXServer — BASTON will not synthesise
them.

> The reverse-engineered platform handshake in
> [`cfx-platform-handshake.md`](cfx-platform-handshake.md) documents *how* that
> closed flow works, but it is a **NON-retained** approach (ToS risk). It is kept
> for reference only; the retained path is this document.

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `[license] mode = "gate" requires a licence key` | Set `sv_license_key`, or use `mode = "off"` for dev. |
| `sv_license_key does not look like a real CFX key` | Empty, placeholder, or malformed — paste your real key from the Portal. |
| `[license] mode = "verified" but fxserver_path is not set` | Point `fxserver_path` at a real `FXServer.exe`. |
| `[license] fxserver_path "..." not found` | Wrong path, or use `mode = "gate"`. |
| `CFX licence check failed — refusing to start: ...` | The official component rejected the key (missing/invalid/banned) — verify it at the Portal. |
| `mode = "verified"` warns "not validated on this build" | You're on Linux or built without `--features escrow`; the key passed the gate but was not sidecar-validated. Build the Windows `escrow` variant for real verification. |
| `max_players capped N -> M (CFX licence entitlement)` | Working as intended — your licence grants fewer slots than configured. |
