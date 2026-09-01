//! deno_core-based scripting runtime for BASTON.
//!
//! - [`ScriptRuntime`]: one V8 isolate per resource (FiveM polyfills injected).
//! - [`ScriptHost`]: owns all runtimes on a dedicated thread; the process
//!   talks to it via a `Send` handle.
//! - [`DeferralRegistry`]: `playerConnecting` deferral state shared with the
//!   HTTP gateway.

pub mod cfx_lua;
mod deferrals;
mod engine;
mod entity_world;
mod error;
#[cfg(feature = "js")]
mod extensions;
mod host;
mod http_bridge;
mod http_handler;
mod kvp;
#[cfg(feature = "lua")]
pub mod lua;
// The natives and the state behind them exist to serve a scripting engine. The
// `lite` bundle compiles none, so nothing calls them. They are still built —
// the linker drops what is unreachable — rather than cfg-fragmented item by
// item, which would make the neutral layer harder to read than it is useful.
#[cfg_attr(not(any(feature = "js", feature = "lua")), allow(dead_code))]
mod native_state;
#[cfg_attr(not(any(feature = "js", feature = "lua")), allow(dead_code))]
mod natives;
pub mod net_bridge;
pub mod observability;
mod resource_control;
mod resource_registry;
#[cfg(feature = "js")]
mod runtime;
mod state_bag;

pub use deferrals::{DeferralOutcome, DeferralRegistry};
pub use engine::Engine;
pub use entity_world::{
    EntitySummary, EntityWorldView, NoWorldControl, ScriptEntityType, WorldCommand, WorldControl,
};
pub use error::ScriptError;
pub use host::{ScriptHost, ScriptSource};
pub use http_bridge::{
    parse_request as parse_http_request, HttpBridge, HttpReply, HttpRequest, HttpRequestSpec,
    HTTP_RESPONSE_EVENT,
};
pub use http_handler::{HttpHandlerRegistry, ScriptHttpResponse, HTTP_REQUEST_EVENT};
pub use kvp::KvpStore;
pub use native_state::SharedGameState;
pub use native_state::{
    DbAccess, NativeState, RuntimeContext, SharedDb, SharedVoice, VoiceControl,
};
pub use net_bridge::{EventTarget, NetBridge, NetOutbound};
pub use observability::{
    DispatchKind, HandlerPerfStats, Observability, ProfilerRecordOptions, ProfilerStatus,
    ResMonSnapshot, ResourcePerfStats,
};
pub use resource_control::{
    NoResourceControl, QueuedResourceControl, ResourceCommand, ResourceControl,
};
pub use resource_registry::{ResourceRegistry, ScriptResourceState};
#[cfg(feature = "js")]
pub use runtime::ScriptRuntime;
pub use state_bag::{
    entity_from_state_bag_name, entity_state_bag_name, player_from_state_bag_name,
    player_state_bag_name, InMemoryRoutingControl, RoutingControl, RoutingLockdownMode,
    StateBagChange, StateBagDelivery, StateBagSource, StateBagStore,
};
