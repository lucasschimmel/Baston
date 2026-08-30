---
title: "The FiveM wire protocol"
description: "How a stock FiveM client connects to BASTON, message by message — the reverse-engineered details you could not guess."
---

BASTON speaks the FiveM protocol without sharing a line of code with FXServer.
Everything here was reverse-engineered from client behaviour and the CitizenFX
sources. This page is the map; the code is in `crates/baston-protocol/`.

If you are here to change something, read
[the surprises](#things-you-could-not-have-guessed) first. Most of them are one
line of code and a day of debugging if you get them wrong.

## Connecting, end to end

### 1. Pre-connect probes

The client asks two independent questions before it tries to connect.

**`GET /info.json`** on the game port returns server metadata. The load-bearing
field is `vars.sv_enforceGameBuild`: the client reads it *before* connecting and
switches GTA build accordingly. Without it, clients on mixed builds cannot join
a non-OneSync session.

**An out-of-band UDP datagram** arrives on the game port before ENet does:
`0xFFFFFFFF` followed by `getinfo <challenge>`. It must be answered from the
same socket or the client aborts with "Failed to get info from server".

```
infoResponse\n\sv_maxclients\<n>\clients\<n>\challenge\<echo>\gamename\CitizenFX\protocol\4\hostname\<name>\...
```

The OOB socket wraps the one handed to ENet and swallows these datagrams so the
ENet state machine never sees them. It is rate-limited per source IP (5/s,
burst 5) and truncates the echoed challenge to 64 characters — both to blunt
UDP reflection and amplification, since this endpoint answers unauthenticated
strangers.

Note `protocol\4` here versus `protocol: 5` in the connect response. Different
fields, both fixed.

### 2. `POST /client`

One route, many methods, multiplexed on a `method` field — matching the
client's `ClientMethodRegistry`.

| `method` | Does |
| --- | --- |
| `initConnect` *(default)* | The real handshake |
| `getConfiguration` | Which resources and files to download |
| `getEndpoints` | Returns `[]` — no `sv_endpoints` equivalent exists |

**`initConnect`** parses its body as *either* url-encoded or JSON, then:

1. Rejects `protocol < 12` with the client's own wording so the player sees a
   sensible message.
2. Requires `gameName == "gta5"`.
3. Allocates a `source` id.
4. Verifies the CFX ticket offline (see [Authentication](#authentication)).
5. Fires `playerConnecting` and **blocks on deferrals** until they resolve or
   `connection.deferral_timeout_secs` elapses.
6. Issues a UUID session token bound to the source.

The response carries a set of fields the client is picky about:

| Field | Value | Why it matters |
| --- | --- | --- |
| `protocol` | `5` | |
| `bitVersion` | `0x2025_0101_0000` | A date packed as hex nibbles — 2025-01-01. |
| `sH` | `false` | `sv_scriptHookAllowed`. **The client hard-fails if this is null.** |
| `netlibVersion` | `2` | The game transport is ENet v2. |
| `onesync_lh` | always `false` | The length hack is never advertised. |
| `token` | UUID v4 | Presented later as `X-CitizenFX-Token`. |

Rejections mirror FXServer's shape — `{"error": reason}` on HTTP **200**.

### 3. `getConfiguration`

Authenticated by the `X-CitizenFX-Token` header carrying the token from
`initConnect`. Returns, per resource, a packfile hash and the stream files.

```json
{
  "fileServer": "http://<host>/files",
  "resources": [
    { "name": "…", "files": { "resource.rpf": "<sha1>" }, "streamFiles": { … } }
  ]
}
```

**The `fileServer` value is the trap.** The client rewrites *any* `%s`-templated
URL — including `"http://%s/"` — to `https://<peer>/`. BASTON has no TLS
listener on the game port, so it emits a **literal** URL built from the request's
`Host` header, which takes the client's CDN code path and keeps downloads on
plain HTTP. This is also why there is no `[tls]` section.

### 4. Downloading assets

`GET|HEAD /files/{resource}/{*path}`, strictly allowlisted: the literal
`resource.rpf`, a path the manifest declared, or a single-component `stream/`
basename. Builtin resources are checked *first*, so a resource on disk cannot
serve bytes under a server-owned name.

Packfiles are **RPF2**, built in memory, never written to disk, and a
`fxmanifest.lua` is generated into them from BASTON's JSON manifest — the client
still expects the Lua file even though BASTON does not use one.

### 5. The ENet handshake

Two channels. Every game message is framed as:

```
u32 LE message type | payload
```

where the type is either the literal `1` or `HashRageString(name)` — Jenkins
one-at-a-time, **case-sensitive**.

**Client → server, `msgType 1`:**

```
u32 1 | url-encoded "token=<connToken>&guid=<u64>"
```

An unknown token disconnects the peer.

**Server → client, `connectOK`:**

```
u32 1 | " <netId> <hostNetId> <hostNetBase> <slotId> <serverTimeMs>"
```

Note the **leading space**. `slotId` and `serverTimeMs` are currently hardcoded
`-1 -1`, even under OneSync.

**Then a burst:**

1. The client is registered in the OneSync game state.
2. **`msgConVars`** — a msgpack map. Carries `onesync`, and
   `voice_externalAddress` / `voice_externalPort` when voice is advertised. This
   is how the client's embedded Mumble is pointed away from the game port.
3. **`onPlayerJoining`** net events, O(n²): the joiner gets one per existing
   client *including itself*, and every existing client gets one about the
   joiner.
4. **`playerJoining`** fires server-side, dispatched detached — a handler may
   await a native round-trip that only this task can service, so blocking here
   would deadlock.

## Message types

| Name | Value | Dir | Chan | Payload |
| --- | --- | --- | --- | --- |
| *(literal 1)* | `1` | both | 0 | connect / connectOK |
| `msgTimeSyncReq` | `0x1C1303F8` | C→S | 0 | `u32 requestTime \| u32 sequence` |
| `msgTimeSync` | `0xE56E37ED` | S→C | 1 | echo + `u32 serverTimeMillis` |
| `msgRoute` | `0xE938445B` | both | 1 | `u16 netId \| u16 len \| data` |
| `msgNetEvent` | `0x7337FD7A` | S→C | 0 | see [Events](#events) |
| `msgServerEvent` | `0xFA776E18` | C→S | 0 | see [Events](#events) |
| `msgIQuit` | `0x522CADD1` | C→S | 0 | ignored; peer disconnected |
| `msgIHost` | `0xB3EA30DE` | both | 0 | session host arbitration |
| `msgConVars` | `0x6ACBD583` | S→C | 0 | msgpack map |
| `msgRequestObjectIds` | `0xB8E611CF` | C→S | 1 | empty |
| `msgObjectIds` | `0x48E39581` | S→C | 1 | `u16 count \| (u16 gap, u16 size)*` |
| `netClones` | `0xAB7FD26E` | C→S | *(in `msgRoute`)* | LZ4-with-dictionary |
| `netAcks` | `0xD52E61B7` | C→S | *(in `msgRoute`)* | LZ4-with-dictionary |
| `msgPackedClones` | `0x81E1C835` | S→C | 1, **unreliable** | plain LZ4 |
| `msgPackedAcks` | `0x258DFDB4` | S→C | 1, **reliable** | plain LZ4 |
| `gameStateNAck` | `0xD2F86A6E` | C→S | 1 | loss recovery |
| `gameStateAck` | `0xA5D4E2BC` | C→S | 1 | frame confirmation |
| `msgBastonState` | `0x3635C9F4` | C→S | 1 | **not FiveM** — bincode, loadtest only |
| `msgBastonSnapshot` | `0xE75CCEE5` | S→C | — | **not FiveM** — bincode, loadtest only |

**Channel separation is enforced**, more strictly than FXServer: control
messages only on channel 0, state messages only on channel 1. Violations are
dropped and counted as `udp_ingress_rejected_total{reason="wrong_channel"}`.

## Events

Server → client (`msgNetEvent`):

```
u32 hash | u16 sourceNetId | u16 nameLen | name bytes | 0x00 | msgpack array
```

- `sourceNetId` is always `0xFFFF` when the server originates the event.
- `nameLen` **includes the trailing NUL**.
- Arguments are a msgpack array, converted from the script side's JSON.

Client → server (`msgServerEvent`) is the same minus `sourceNetId`.

### Events the gateway intercepts

Four event names never reach your resources — the gateway consumes them:

| Event | Purpose |
| --- | --- |
| `hostingSession` / `hostedSession` | Session host arbitration |
| `__baston:nativeResult` | Resolves a pending server→client native call |
| `__baston:stateUpdate` | The client shim's position report |
| `baston:displayInfo:toggle` | Debug overlay subscription |

Everything else is dispatched to resources **detached**, for the deadlock reason
above.

## OneSync entity sync

### It rides inside `msgRoute`

The clone stream has no message type of its own. When OneSync is on, `msgRoute`
payloads are parsed as a clone stream instead of being relayed peer-to-peer.

### Packet framing

```
outbound: [u32 msgType][u64 frameIndex][lz4 body]
inbound:  [u32 msgType][lz4 body]              ← no frame index
```

`frameIndex` is a bitfield: `lastFragment:1 | currentFragment:7 | frameIndex:56`,
LSB-first as the C++ union packs on x86.

The body is bit-packed records terminated by a 3-bit `END`. Fragments are
self-terminated, and the **final fragment is always emitted even when blank**,
so the client keeps acknowledging.

### Compression is asymmetric

This surprises everyone:

- **Inbound** (`netClones`, `netAcks`) is compressed by the client *with a
  shared 64 KiB static dictionary*, embedded in the binary.
- **Outbound** (`msgPackedClones`, `msgPackedAcks`) is plain LZ4, **no
  dictionary**.

### Clone records

A 3-bit type tag: `CREATE=1, SYNC=2, REMOVE=3, TAKEOVER=4, TIMESTAMP=5,
INDEX=6, END=7`.

**Inbound and outbound use different field orders.** This is the most
error-prone part of the whole format:

```
inbound create/sync   u16 uniqifier | u13 objectId | …     ← uniqifier first
inbound remove        u13 objectId  | u16 uniqifier        ← objectId first
outbound create/sync  u3 type | u13 objectId | u16 ownerNetId | …
```

Within outbound, the dependent frame index is written **high word first**, while
the timestamp record is **low word first**. On a first-frame update, the
uniqifier is transmitted **bit-inverted**.

### The bit buffer

A port of `rl::MessageBuffer`, **MSB-first within each byte**, preserving
several C++ quirks deliberately:

- A failed read still advances the cursor.
- Reading a bit past the end returns 0 without failing, and still advances.
- Signed encoding is **not two's complement**: one sign bit, then `length-1`
  bits of `value XOR sign_extension`.
- The **length hack** widens every 13-bit field to 16. It is global in C++ and
  a per-buffer flag here. BASTON runs with it **off** — passing the wrong value
  desynchronises the bit cursor by three bits and turns every record into
  garbage.

### Sync trees

Thirteen tables, each a flattened **preorder** traversal of the engine's sync
tree. The order *is* the wire format.

Whether a node is read is decided in this exact order:

1. `Id1 & sync_type == 0` → skip, consuming **no bits**
2. `Id3 != 0 && obj_type & Id3 == 0` → skip, consuming **no bits**
3. `Id2 & sync_type != 0` → consume **one presence bit**; zero skips

Reading a presence bit too early desynchronises the stream. Every read leaf is
framed by a 13-bit length prefix, and the walk resynchronises unconditionally at
`start + length` — so a malformed node costs one node, never the packet. That is
the security property.

### Position is split across two nodes

```
x = (sectorX - 512) * 54 + posX
y = (sectorY - 512) * 54 + posY
z = (sectorZ * 69 + posZ) - 1700
```

### Game build matters

Sync-tree node widths changed between GTA builds. BASTON defaults to build
**3258** and declares gates for 2060, 2189, 2372, 2545, 2699, 3258, 3407 and
3717. From 3717, ped health fields are 14 bits instead of 13.

The honest caveat: the engine has more gates than are declared here. Only gates
sitting *before* a field BASTON decodes matter, because every decoder stops at
the last field it uses — 25 of roughly 70 nodes are decoded at all.

## Authentication

CFX tickets are verified **offline**. The only network call is fetching the RSA
public key once, from `auth.pubkey_url`; nothing per connection touches CFX.

Ticket layout:

```
u32 innerLength (=16) | u64 guid LE | u64 expiry LE
u32 sig1Length (=128) | 128-byte RSA signature over bytes[4..20]
u32 extraLength N     | N bytes: 20-byte entitlement SHA-1 + u32 jsonLen + JSON
u32 sig2Length (=128) | 128-byte RSA signature over bytes[4..156+N]
```

The signature scheme reproduces Botan's `EMSA_PKCS1(SHA-1)`, which means the
digest actually verified is `SHA1(0x02 || SHA1(payload))`.

Checks run in order: base64 → length → inner length → **expiry** → **guid
match** → sig1 → extra bounds → sig2. On an invalid signature with a key older
than five minutes, the key is refetched once and verification retried — that is
key rotation.

Replay protection is an in-memory set of `(expiry, guid)`, flushed wholesale
every 30 minutes. It is **per-process**: it is not shared across a mesh.

### Identifiers

A player ends up with, in order:

- `license:<hex>` — the CFX entitlement hash, omitted if the ticket had none
- everything in the ticket's `tk` array — `steam:`, `discord:`, …
- `ip:<peer ip>` — from the `x-real-ip` header, defaulting to `127.0.0.1`

**`ip:` is attacker-controlled** without a trusted reverse proxy in front. Do
not use it for bans or allowlists; the ENet peer address is the trustworthy one.

## Things you could not have guessed

The list that costs a day each:

1. **`sH` must not be null.** The client hard-fails on it.
2. **`slotId` in `onPlayerJoining` must be msgpack-*unsigned*.** A negative int
   throws `msgpack::type_error` in the client and crashes it. BASTON sends
   `u32::MAX`, not `-1`.
3. **`fileServer` must be a literal URL, not a `%s` template**, or the client
   forces HTTPS against a server that has no TLS listener.
4. **The clone stream has no message type** — it is a `msgRoute` payload.
5. **Compression is asymmetric** — dictionary inbound, none outbound.
6. **Inbound and outbound clone records order their fields differently.**
7. **First-frame updates bit-invert the uniqifier.**
8. **The quaternion constant is the literal `1.414214`, not `√2`.** Reproducing
   the literal is what makes round-trips land on the integers a real client
   sends.
9. **`Trailer` uses the automobile sync tree.** The engine defines a trailer
   tree and does not use it for GTA5.
10. **`bitVersion` is a date in hex nibbles.**
11. **Slot 31 is reserved** — every client writes its own sync data as player
    index 31, so it is never given to a remote player.
12. **`msgRoute`'s length field is ignored** by the client handler.
13. **Native dispatch is not a protocol feature.** The stock client has no
    native-call packet, so BASTON tunnels natives over net events:
    `__baston:invokeNative` → the shim runs `Citizen.invokeNative` →
    `__baston:nativeResult`.
14. **`bastonClient=1`** in the connect payload marks a loadtest client and
    skips the O(n²) join broadcast — 2000 joins would otherwise be ~4M reliable
    packets.

## Known gaps

Honest inventory of what is not there:

- **`rage/session.rs` is dead code.** `SlotAllocator` (big-mode virtual slots)
  and `WorldGrid` are implemented and unit-tested but referenced from nowhere.
  **`msgWorldGrid3` is never sent.**
- **`connectOK` always sends `slotId = -1`, `serverTimeMs = -1`**, including
  under OneSync.
- **The length hack (OneSync Beyond, 16-bit ids) is off** and never advertised.
- **`gameStateAck` is nearly a no-op** — the NG path relies on NAK.
- **`getEndpoints` returns a hardcoded `[]`.**
- **Ownership migration on client disconnect is incomplete** in the NG path.
- **No `/players.json` or `/dynamic.json`.**
- **Server-side entity outbound timestamps are a placeholder** (the frame index
  is used as a clock proxy).

## Where the code is

| Area | Path |
| --- | --- |
| Message framing, hashing | `crates/baston-protocol/src/udp/` |
| Connect / configuration shapes | `crates/baston-protocol/src/connection.rs` |
| Events | `crates/baston-protocol/src/events.rs` |
| Bit buffer, clone records, sync trees | `crates/baston-protocol/src/rage/` |
| LZ4 dictionary | `crates/baston-protocol/src/rage/lz4dict.rs` |
| HTTP endpoints | `crates/baston-gateway/src/http/` |
| ENet loop, ingress | `crates/baston-gateway/src/udp/` |
| Ticket verification | `crates/baston-gateway/src/auth/` |
| OneSync ingest and state | `crates/baston-zone/src/onesync/` |

Fuzz targets for the parsers live in `fuzz/fuzz_targets/`: `decode_incoming`,
`decode_downlink`, `parse_ack`, `parse_nack`, `parse_object_ids`,
`lz4dict_decompress`. Anything that parses attacker-controlled bytes should have
one.
