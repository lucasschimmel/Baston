//! The CFX native implementations, independent of any scripting engine.
//!
//! Every native takes a [`NativeState`](crate::native_state::NativeState) and
//! returns JSON, so one implementation serves both the V8 and the Lua runtime
//! (ADR-002, Tier 2). The engine layers are thin: they convert their own
//! values to and from JSON strings and call in here.
//!
//! - [`server`] — the shared CFX natives (KVP, state bags, convars, resources)
//!   and the server-only natives backed by the synthetic entity store, the
//!   player directory and the voice control surface.
//! - [`world`] — natives answered from the authoritative world mirror.
//! - [`client`] — server → client native dispatch over the net bridge.
//! - [`rpc`] — the CFX context-routing table that decides which client a
//!   "server" native is really executed on.

pub(crate) mod client;
pub(crate) mod console;
pub(crate) mod rpc;
pub(crate) mod server;
pub(crate) mod world;

pub(crate) use console::console_buffer_text;

// The natives were written against a service locator and keep borrowing from
// one; re-exported here so their `use super::{...}` imports still resolve.
pub(crate) use crate::native_state::{
    NativeState, RuntimeContext, SharedConvars, SharedEntityWorld, SharedHttp, SharedKvp, SharedNet, SharedObservability, SharedPlayers,
    SharedResourceControl, SharedResources, SharedRouting, SharedStateBags, SharedVoice,
    SharedWorldControl, VoiceControl,
};
