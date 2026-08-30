---
title: "ADR-004 — CFX identity without FXServer"
description: "BASTON performs the platform exchanges itself, as BASTON."
---

Date: 2026-08-30
Status: accepted
Builds on: [ADR-003](003-remove-the-fxserver-sidecar.md)

## Context

[ADR-003](003-remove-the-fxserver-sidecar.md) removed the FXServer sidecar and
recorded what went with it: no server-list presence, no entitlement
enforcement, no escrow. It said nothing replaced it, deliberately.

Reading the public tree afterwards showed the gap is much smaller than the
sidecar implied.

**The closed surface is one HTTP call.** `ServerLicensingComponent` is a
31-line header holding three strings — `key`, `nucleusToken`, `listingToken`.
Both consumers of those strings are open source: nucleus registration in
`ServerNucleusMock.cpp`, the server-list heartbeat in `GameServer.cpp`, cadence
and backoff included. What is closed is the exchange that fills them in:
`GET portal-api.cfx.re/v1/key/validate/<key>`, performed inside `svadhesive`.

That call carries no client secret. The operator's own key is in the URL path;
there is no device binding, no challenge, no signature. FXServer's role in it is
the role of an HTTP client.

**And the slot ceiling turned out to be conditional.** `NetLibrary.cpp` reads
`vars.sv_licenseKeyToken` from `/info.json`; when it is absent, the client calls
`policySuccess()` immediately and never fetches the entitlement policy at all.
The 48-slot ceiling is what a token with empty grants produces — not what
publishing nothing produces.

So the design space was never "can BASTON make the request". It was **what
BASTON claims to be when it makes it**, and the sidecar conflated two separable
things:

| | |
| --- | --- |
| Making the call with the operator's key | interoperability |
| Sending `User-Agent: FXServer/1 (…)` | misrepresentation |

## Decision

BASTON performs all three exchanges itself, in `baston-cfx`, **identifying
itself as BASTON**:

```
User-Agent: BASTON/0.1.0-alpha (+https://github.com/lucasschimmel/Baston)
```

A refusal is an answer. If CFX declines a self-identified third-party client,
`CfxError::ValidateRefused` says so, names the agent that was sent, and the
operator falls back to `mode = "off"`. **Forging FXServer's agent to get past a
refusal is out of scope permanently** — at that point the agent has stopped
being a label and become an access control, and defeating it is the thing this
project exists not to do.

*Verified 2026-08-30: `portal-api.cfx.re` returns HTTP 200 to the honest
agent.*

### Two coupled properties, enforced structurally

**A licence may lower a limit, never raise one.** Entitlements are read from
`policy-live.fivem.net` — the same endpoint the client checks, so BASTON's
ceiling and the client's check cannot disagree — and applied *before any
listener opens*. This is stricter than FXServer, which lets a misconfigured
server boot and bounces players at connect time.

One deliberate divergence from the client: `NetLibrary.cpp`'s ladder has no
branch above 2048, so a server declaring more falls through to plain
`"onesync"` — a cheaper entitlement than the tier it exceeds. BASTON caps at
2048 instead of using the gap.

**Being listed and being slot-checked are the same bargain.** In FXServer they
cannot come apart: one convar flagged `ConVar_ServerInfo` feeds both
`/info.json` and the heartbeat. In BASTON they are separate code paths, so a
server could hold listing credentials while serving an `/info.json` with no
token — discoverable, and never checked by any client. That is a free key with
a paid key's slots, and it is reachable by *omission*, not only by intent.

Three things make it unreachable:

1. `CfxIdentity` carries the public token and both credentials together, and
   can only be built by `authenticate`, which has already applied the cap.
2. `/info.json` and the heartbeat are built by one function, `http::info::payload`.
3. `Listing::heartbeat` refuses to send a snapshot whose `vars` do not carry
   this identity's token — asserted, with tests in both directions.

### Configuration is per server, not per project

| | `mode = "off"` | `mode = "cfx"` |
| --- | --- | --- |
| Server list | no | yes |
| Slots | whatever you configure | capped to your tier |

Neither is the default-correct answer. A 3000-player operator wants `off` and
does not have a discovery problem; a small server wants `cfx` and does not have
a slot problem. The two are mutually exclusive because the token couples them,
not because BASTON chose to make them exclusive.

## Consequences

### Positive

- No FXServer. Not at build, not at boot, not at provisioning, not on Windows.
- Server-list presence and licence enforcement return together, which is the
  only way they should return.
- BASTON is visible to CFX as BASTON. That is a precondition for ever asking
  for first-party status with something better than a proposal.

### Negative

- **Registration and heartbeat are unverified against the live service.**
  Listing a server is an outward-facing act with a public result, so this code
  was written against the sources rather than tested against
  `servers-frontend`. The first operator to enable `[listing]` is the first
  real test; the ingress response body is logged for that reason.
- One thing remains outside BASTON's control: whether CFX's terms permit a
  server key to be used by something that is not FXServer. That is a contract
  between the operator and CFX. The honest agent does not answer it — it makes
  it answerable in good faith rather than assumed.

### Neutral

- Escrow stays out of scope, for the reason ADR-003 gave: it is inside
  `svadhesive` and reaching it means defeating a DRM mechanism.
- `grants_token` is not used. The top-level `policy` array and
  `policy-live.fivem.net` both describe entitlements, and the latter is what
  the client actually enforces, so it is the one BASTON reads.

## Alternatives considered

### Keep the operator's tokens in configuration, minted elsewhere

**Why rejected:** it needs a minting step, and every route to one runs through
FXServer. It moves the same question to a worse ergonomics.

### Send no `User-Agent` in particular

Let `reqwest`'s default through, neither claiming nor disclaiming.

**Why rejected:** it costs the same as being explicit and buys nothing. An
unlabelled request is not more honest than a labelled one, only less useful to
the person reading the logs on the other end.

### Wait for a first-party arrangement before implementing anything

**Why rejected:** the first approach was dismissed without reaching anyone
technical, and there is no second one to wait for. Implementing behind an
honest agent is what produces the evidence a later approach would need.
