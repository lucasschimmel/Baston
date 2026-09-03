---
title: "ADR-003 — Remove the FXServer sidecar"
description: "Why BASTON no longer hosts an FXServer process, and what that costs."
---

Date: 2026-08-30
Status: accepted
Supersedes: [ADR-001](001-use-official-fxserver-as-cfx-trust-broker.md)

## Context

[ADR-001](001-use-official-fxserver-as-cfx-trust-broker.md) had BASTON host an
operator-supplied, unmodified `FXServer.exe` as a local trust broker. FXServer
authenticated the operator's CFX key, BASTON read the resulting
`sv_licenseKeyToken` over a file-drop IPC channel, applied the entitlements
restrictively, and let the same process register the endpoint in the public
server list. A second, zone-local sidecar decrypted CFX Asset Escrow resources
through `svadhesive`.

It was always a stopgap. The intent was to reach an agreement with CFX and
replace it with something real; the sidecar existed so that the rest of BASTON
could be built without waiting on that conversation. The conversation did not
happen — the request was dismissed without reaching anyone who could evaluate
it — so the stopgap has no endpoint to converge on.

That leaves it to stand on its own merits, and it does not:

- **It is Windows-only.** `svadhesive.dll` and the FXServer artifact BASTON
  drove are Windows binaries. Production FiveM servers run on Linux, so the
  one deployment target that mattered could never use it.
- **It was never validated end to end.** No test covers a live FXServer: the
  suite exercises a stub sidecar speaking the same file-drop protocol. Whether
  the real handshake works today is genuinely unknown.
- **It made BASTON's liveness depend on a foreign process.** The gateway
  monitored the broker and shut itself down when it died — a whole failure mode
  imported from a binary BASTON does not build, ship, or control, and one CFX
  can change under us at any release.
- **It is a large surface for a capability nothing yet uses.** Two crates, a
  Lua shim injected into a foreign resource directory, a spawn/IPC/timeout
  protocol, a cancellation path, and a stub binary to test it — for an
  entitlement result that no BASTON code path acts on beyond capping slots.

Meanwhile the point of BASTON is to run more players than FXServer can. Keeping
FXServer on the boot path to ask its permission is the wrong shape for that.

## Decision

Remove the FXServer sidecar and everything that existed only to serve it:

- the `baston-cfx-platform` crate (sidecar process, licence oracle, policy
  client, the injected Lua shim, the stub sidecar binary);
- the `baston-escrow-plugin` crate and the `escrow` Cargo feature;
- `[license] mode = "verified"`, `fxserver_path`, `sidecar_port`,
  `public_listing`, `listing_ip_override`, and the whole `[escrow]` section;
- `baston-core::license` — the entitlement model, which had no producer left;
- `sv_licenseKeyToken` in `/info.json`, which would otherwise always be absent
  while looking like a feature.

`[license]` keeps `mode = "off" | "gate"` and `sv_license_key`. `gate` checks
the key's *shape* — non-empty, no whitespace, long enough, not a placeholder —
and nothing more. Both modes warn at boot that no licence is enforced, because
an operator who reads "licence" in their config and assumes their entitlements
are being applied has the wrong model of what this server does.

`mode = "verified"` is **rejected at parse time** rather than silently treated
as `off`. A config that asks to be verified must stop, not boot unauthenticated
while its operator believes CFX validated their key.

## Consequences

### Negative

- **BASTON does not appear in the public CFX server list.** Nothing here
  registers or heartbeats.
- **Escrowed (`.fxap`) resources cannot run.** A resource whose scripts are
  CFX-encrypted is refused at load with an explicit error. There is no
  workaround short of an unescrowed build from the author.
- **No entitlement is enforced.** BASTON neither raises nor lowers a slot
  count on any licence basis; `max_players` is what the operator configured.

Those were the sidecar's three deliverables, and all three are gone. This ADR
does not claim otherwise: it claims the sidecar did not deliver them reliably
on the platform servers actually run on, and that carrying a broken bridge is
worse than carrying none.

### Positive

- No Windows-only path, no foreign process on the boot path, no failure mode
  imported from a binary BASTON does not control.
- The claim BASTON makes about itself is now true. It ran a licence-shaped
  code path that had never been proven against a real CFX key; it now says
  plainly that it authenticates nothing.
- Two crates, a feature flag, a config section and a bundle dimension less to
  keep working.

### Neutral

- The `ScriptDecryptor` seam in `baston-core` stays, with `PlainDecryptor` as
  its only implementation. It is the one place that recognises an `.fxap`
  payload and refuses it with an actionable message, and it is where escrow
  support would return.
- `Bundle::Full` now means "both scripting runtimes and the database pool"
  rather than "…and escrow". The CI matrix is still four bundles.
- The reverse-engineering notes in
  [`internals/cfx-platform-handshake.md`](../internals/cfx-platform-handshake.md)
  are kept. They describe the closed platform flow as captured from a live
  FXServer, they carry their own compliance warning, and they are the record of
  what is known — not a plan.

## What replaces it

Nothing, yet, and that is deliberate. Authenticating a CFX key from a
non-FXServer binary is a question with a legal answer before it has a technical
one, and it is not answered by leaving a half-working bridge in the tree while
we think about it.

Whatever comes next starts from three constraints this ADR records so they do
not have to be rediscovered:

1. **Restrictive by construction.** A licence signal may lower a limit or keep
   a feature off. It may never raise a limit or enable a feature that was not
   granted. The removed `baston-core::license` module encoded exactly this, and
   any replacement should.
2. **Fail-closed.** Anything other than an explicitly valid, non-banned verdict
   denies startup. Absence of a signal is not permission.
3. **Never claim more than is known.** A field BASTON cannot determine locally
   stays absent rather than defaulting to something plausible.

## Alternatives considered

### Keep the sidecar, port it to Linux

**Why rejected:** `svadhesive` is a Windows DLL. A Linux FXServer artifact
exists, but the escrow half would still not work, and the licence half would
still be an unvalidated bridge to a process CFX can change without notice. The
port cost buys a stopgap that is still a stopgap.

### Keep the code, disable it by default

**Why rejected:** it is already off by default, and that is how it reached the
state of nobody knowing whether it works. Dead code that claims a capability is
worse than absent code: it shapes the config file, the module report and the
documentation around a promise the binary does not keep.

### Implement the platform handshake directly in Rust

Reproduce key validation, nucleus registration and server-list ingress from
BASTON itself — the flow captured in
[`internals/cfx-platform-handshake.md`](../internals/cfx-platform-handshake.md).

**Why rejected here:** this ADR is a removal, and doing that would be a
different decision with a compliance question at its centre, not a technical
one. It needs its own ADR, made deliberately.
