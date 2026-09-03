---
title: "Adding a native"
description: "Implement a CFX native once and both scripting engines get it."
---

The most common contribution, and usually a small one. Because the natives layer
is engine-neutral, **implementing a native once gives it to JavaScript and Lua
at the same time**.

## Find out what is actually needed

Start from evidence, not a guess:

```bash
curl -s localhost:9090/metrics | grep script_native_unimplemented_total
```

Every name in that list is a native some resource called and did not get. The
count tells you how much it matters.

## Where it goes

| The native… | Lives in |
| --- | --- |
| reads or writes server state — KVP, state bags, players, resources, entities | `natives/server.rs`, in `shared_native_value` or `cfx_server_native` |
| reads vehicle or ped state from the world mirror | `natives/world.rs` |
| must run on a client | `natives/rpc/table.rs` |

Shared natives are the CFX set available on both client and server; server
natives are server-only. If you are unsure, put it in the shared set — the
dispatcher tries shared first and falls through.

## A worked example

Say `GET_PLAYER_WANTED_LEVEL` is missing. In `natives/server.rs`, inside
`cfx_server_native`'s match:

```rust
"GET_PLAYER_WANTED_LEVEL" => {
    let source = json_arg_netid(&args, 0);
    serde_json::json!(
        state
            .borrow::<SharedEntityWorld>()
            .0
            .wanted_level(source)
            .unwrap_or(0)
    )
}
```

Three things to keep to:

**Arguments come as JSON.** Use the helpers — `json_arg_netid`,
`json_arg_string`, `json_arg_i64`, `json_arg_f64`, `json_arg_bool`. They are
lenient in the same way the FiveM ecosystem is: `source` is stringly-typed in
much of it, so `json_arg_netid` accepts both a number and a numeric string.

**Return a `serde_json::Value`** of the shape the native documents. An `int`
native must not return a float — resources use ids as keys, and `1.0` is not
`1`.

**Never panic.** A malformed argument is a script bug, not a server bug. Return
the neutral value.

## Reaching services

Services come from the `NativeState` type-map:

```rust
let kvp      = Arc::clone(&state.borrow::<SharedKvp>().0);
let players  = &state.borrow::<SharedPlayers>().0;
let resource = state.borrow::<RuntimeContext>().resource_name.clone();
```

Available: `SharedKvp`, `SharedPlayers`, `SharedStateBags`, `SharedEntityWorld`,
`SharedWorldControl`, `SharedRouting`, `SharedConvars`, `SharedResources`,
`SharedHttp`, `SharedHttpHandlers`, `SharedResourceControl`, `SharedVoice`,
`SharedDeferrals`, `SharedNet`, `SharedObservability`, `SharedDb`, and the
per-runtime `RuntimeContext`.

If your native needs a service that is not there, add a `Shared*` newtype in
`native_state.rs` and install it in both runtimes' constructors — `runtime.rs`
and `lua.rs`. Keep the two in step; a service installed in one engine only is
exactly the asymmetry this layer exists to prevent.

## A native that may be absent

Voice is the pattern: the service is `Option`, and the native returns a neutral
value rather than failing when the module is off.

```rust
"MUMBLE_IS_PLAYER_MUTED" => {
    let muted = state
        .borrow::<SharedVoice>()
        .0
        .as_ref()
        .is_some_and(|voice| voice.is_player_muted(json_arg_netid(&args, 0)));
    serde_json::json!(muted)
}
```

A resource must be able to feature-detect without crashing.

## Client-executed natives

If the native needs the game engine, it cannot run server-side at all. Add it to
the RPC table in `natives/rpc/table.rs` with its hash and argument count, and
BASTON routes it to the owning client.

That table is generated from CFX's `rpc_natives.json` — see
`tools/gen-rpc-natives.mjs`. Prefer regenerating over hand-editing.

## Exposing it to scripts

**Lua: nothing to do.** An unknown capitalised global resolves to the
SCREAMING_SNAKE name on first use, so `GetPlayerWantedLevel(src)` works as soon
as the native exists.

**JavaScript: add the global** in `assets/bootstrap.js`, next to its neighbours:

```javascript
globalThis.GetPlayerWantedLevel = (...args) =>
  InvokeCfxServerNative("GET_PLAYER_WANTED_LEVEL", "int", args);
```

The result-kind string (`"int"`, `"string"`, `"bool"`, `"float"`, `"Vector3"`,
`"object"`) tells the dispatcher how to shape the answer.

## Testing it

Unit-test the native itself against a `NativeState` you build. If it is
interesting in both languages, test both — the Lua suite in `lua.rs` shows the
pattern, and a test there proves the neutral layer really is shared:

```rust
#[test]
fn wanted_level_reads_from_the_world() {
    let mut rt = runtime("test");
    rt.execute_script("main.lua", r#"level = GetPlayerWantedLevel(1)"#).unwrap();
    assert_eq!(rt.lua.globals().get::<i64>("level").unwrap(), 0);
}
```

```bash
cargo test -p baston-scripting --features lua
cargo test -p baston-scripting                 # the JS path
```

## Before you open the PR

- [ ] Return type matches what the native documents (ints are not floats).
- [ ] No panic on any argument a script could pass.
- [ ] A test, ideally in Lua — it proves the neutral layer works.
- [ ] `docs/reference/natives-gap.md` updated if it tracks this native.
- [ ] Nothing engine-specific leaked into `natives/`.

## Adding a *stub* instead

Sometimes the honest answer is a neutral value — the native needs client state
BASTON does not have.

Add it explicitly rather than leaving it to the fallthrough, and say so in a
comment. An explicit stub documents the decision; a fallthrough looks like an
oversight. Either way it keeps counting in
`script_native_unimplemented_total`, so the gap stays visible.

## Next

- [Crates](crates.md)
- [Testing](testing.md)
