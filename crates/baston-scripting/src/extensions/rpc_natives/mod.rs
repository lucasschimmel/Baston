//! Context-routed natives: the server → client RPC bridge.
//!
//! A large part of the FiveM "server" native surface is not server-side at all.
//! Natives like `TASK_PLAY_ANIM` or `SET_VEHICLE_DOORS_LOCKED` only exist inside
//! the game client; the server forwards them to exactly one client, which
//! executes them locally. *Which* client is decided by the call's **context**:
//! for most natives the client that owns the entity passed as an argument, for
//! a handful the player passed as an argument.
//!
//! CFX publishes that routing table as `rpc_natives.json`; [`table`] is its
//! generated Rust form, limited to the 69 `"type": "ctx"` entries. See
//! `assets/rpc_natives.json` for provenance and `tools/gen-rpc-natives.mjs` for
//! how to regenerate.
//!
//! ## Handles need no translation
//!
//! The spec marks entity arguments `"translate": true` because FXServer has to
//! rewrite a server handle into the target client's local handle. BASTON has
//! nothing to rewrite: a script handle *is* the network id (see
//! [`crate::entity_world`]) and the client shim resolves network ids locally,
//! so arguments are forwarded verbatim.
//!
//! ## Every ctx native is a void mutation
//!
//! No entry returns a value the caller can observe, so dispatch is
//! fire-and-forget: the call is queued on the net bridge and the native returns
//! immediately instead of stalling the isolate on a client round trip.
//!
//! ## A missing target is not an error
//!
//! Without OneSync — or simply before the first sync tick — the entity mirror is
//! empty and no handle has an owner. A script calling these natives then is not
//! doing anything wrong, it is early. So an unresolvable target drops the call,
//! counts it, and warns once per native; it never throws and never spams.

mod table;

use std::time::Instant;

use dashmap::DashMap;
use deno_core::OpState;

use super::natives_server::json_arg_netid;
use super::{RuntimeContext, SharedEntityWorld, SharedNet, SharedObservability};

/// Which argument decides where a context native is dispatched.
///
/// `idx` is the position of that argument in the native's argument list, taken
/// from the spec's `ctx.idx` — never assumed to be 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RpcContext {
    /// Dispatch to the client that owns the entity in `args[idx]`.
    Entity { idx: usize },
    /// Dispatch to the player whose server net id is in `args[idx]`.
    Player { idx: usize },
    /// The argument is a server-created object handle (a blip). Not routable
    /// yet: BASTON has no server-side object registry, which is the same work
    /// item as the spec's `"object"` constructors.
    ObjectRef { idx: usize },
    /// Like [`RpcContext::ObjectRef`], and the call also destroys the handle.
    ObjectDelete { idx: usize },
}

impl RpcContext {
    /// Index of the argument carrying the routing target.
    fn idx(self) -> usize {
        match self {
            Self::Entity { idx }
            | Self::Player { idx }
            | Self::ObjectRef { idx }
            | Self::ObjectDelete { idx } => idx,
        }
    }
}

/// One context-routed native, as described by the CFX RPC spec.
#[derive(Debug, Clone, Copy)]
pub(super) struct RpcNative {
    /// Screaming-snake native name, the key scripts reach it by.
    pub name: &'static str,
    /// Hash of the *client* native to invoke.
    pub hash: u64,
    /// How to resolve the target client.
    pub context: RpcContext,
    /// Argument count the client native expects.
    pub arg_count: usize,
}

/// Why a context native was recognised but not put on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SkipReason {
    /// No client owns the entity — server-owned, unknown handle, or no sync
    /// state published yet.
    NoOwner,
    /// The player/entity argument was 0, i.e. the script has no target.
    NoTarget,
    /// The routing argument is an object handle BASTON cannot resolve yet.
    UnsupportedContext,
    /// The script passed the wrong number of arguments; forwarding them would
    /// make the client invoke a native with a malformed stack.
    BadArity,
    /// The net bridge is full or closed (backpressure).
    BridgeFull,
}

impl SkipReason {
    /// Stable label for metrics and logs.
    fn as_str(self) -> &'static str {
        match self {
            Self::NoOwner => "no_owner",
            Self::NoTarget => "no_target",
            Self::UnsupportedContext => "unsupported_context",
            Self::BadArity => "bad_arity",
            Self::BridgeFull => "bridge_full",
        }
    }
}

/// Look up a native in the generated context table.
#[must_use]
pub(super) fn lookup(name: &str) -> Option<&'static RpcNative> {
    table::CTX_NATIVES
        .binary_search_by(|native| native.name.cmp(name))
        .ok()
        .map(|index| &table::CTX_NATIVES[index])
}

/// Route `name` to the owning client if it is a context native.
///
/// Returns `true` when the native belongs to the RPC surface — that is, when
/// the caller must *not* fall through to its own handling — regardless of
/// whether a target could be resolved. Returns `false` for anything else.
pub(super) fn try_dispatch(state: &OpState, name: &str, args: &[serde_json::Value]) -> bool {
    let Some(native) = lookup(name) else {
        return false;
    };
    if let Err(reason) = dispatch(state, native, args) {
        report_skip(native, reason);
    }
    true
}

/// Resolve the target client and queue the call. `Err` carries the reason the
/// call was dropped, which is always a fact worth counting, never a panic.
fn dispatch(
    state: &OpState,
    native: &RpcNative,
    args: &[serde_json::Value],
) -> Result<(), SkipReason> {
    if args.len() != native.arg_count {
        return Err(SkipReason::BadArity);
    }
    let target = resolve_target(&state.borrow::<SharedEntityWorld>().0, native, args)?;

    let net = state.borrow::<SharedNet>().0.clone();
    let observability = std::sync::Arc::clone(&state.borrow::<SharedObservability>().0);
    let resource = state.borrow::<RuntimeContext>().resource_name.clone();

    let started = Instant::now();
    // The shim answers every call, including ones nobody waits for, so an id is
    // still allocated — then immediately released so the waiter map does not
    // grow one dead entry per fire-and-forget dispatch.
    let (id, _rx) = net.pending_natives.register();
    let queued =
        super::natives_client::queue_native_call(&net, target, id, native.hash, args.to_vec());
    net.pending_natives.cancel(id);

    observability.record_native_roundtrip(
        &resource,
        native.hash,
        target,
        started.elapsed().as_micros() as u64,
        false,
        !queued,
    );
    if !queued {
        return Err(SkipReason::BridgeFull);
    }

    metrics::counter!(
        "script_native_rpc_dispatch_total",
        "native" => native.name,
    )
    .increment(1);
    tracing::trace!(
        target: "natives",
        native = native.name,
        %resource,
        target,
        "context native dispatched to owning client"
    );
    Ok(())
}

/// The client that must execute the native, per the spec's context rules.
///
/// Takes the world view rather than the whole `OpState` so the routing rules
/// — the part that decides which player receives someone else's mutation —
/// can be tested directly.
fn resolve_target(
    world: &crate::EntityWorldView,
    native: &RpcNative,
    args: &[serde_json::Value],
) -> Result<u32, SkipReason> {
    let argument = json_arg_netid(args, native.context.idx());
    match native.context {
        // A script handle is a network id, so the argument indexes the entity
        // mirror directly. Owner 0 means "nobody simulates this", which is the
        // same answer `NETWORK_GET_ENTITY_OWNER` gives.
        RpcContext::Entity { .. } => world
            .owner(argument)
            .filter(|owner| *owner != 0)
            .ok_or(SkipReason::NoOwner),
        // The argument already *is* the server net id of the target player; no
        // directory lookup is needed, exactly as for `TriggerClientEvent`.
        RpcContext::Player { .. } => {
            if argument == 0 {
                Err(SkipReason::NoTarget)
            } else {
                Ok(argument)
            }
        }
        RpcContext::ObjectRef { .. } | RpcContext::ObjectDelete { .. } => {
            Err(SkipReason::UnsupportedContext)
        }
    }
}

/// Natives already reported as skipped, so the warning fires once per
/// (native, reason) pair. A resource calling a context native from a tick loop
/// on a server with no sync state would otherwise drown the log.
fn reported_skips() -> &'static DashMap<(&'static str, &'static str), ()> {
    static REPORTED: std::sync::OnceLock<DashMap<(&'static str, &'static str), ()>> =
        std::sync::OnceLock::new();
    REPORTED.get_or_init(DashMap::new)
}

/// Count every dropped dispatch, log the first of each kind.
fn report_skip(native: &RpcNative, reason: SkipReason) {
    // `script_native_rpc_no_owner_total` is broken out on its own because "the
    // world has no owner for this handle" is the one skip that is expected on a
    // healthy server (no OneSync, or before the first sync tick) and therefore
    // the one an operator wants to alert on separately.
    if reason == SkipReason::NoOwner {
        metrics::counter!(
            "script_native_rpc_no_owner_total",
            "native" => native.name,
        )
        .increment(1);
    } else {
        metrics::counter!(
            "script_native_rpc_skipped_total",
            "native" => native.name,
            "reason" => reason.as_str(),
        )
        .increment(1);
    }

    if reported_skips()
        .insert((native.name, reason.as_str()), ())
        .is_none()
    {
        tracing::warn!(
            target: "natives",
            native = native.name,
            reason = reason.as_str(),
            "context native not dispatched — no client executed it"
        );
    }
}

#[cfg(test)]
mod tests;
