//! deno_core-based scripting runtime for BASTON.
//!
//! - [`ScriptRuntime`]: one V8 isolate per resource (FiveM polyfills injected).
//! - [`ScriptHost`]: owns all runtimes on a dedicated thread; the process
//!   talks to it via a `Send` handle.
//! - [`DeferralRegistry`]: `playerConnecting` deferral state shared with the
//!   HTTP gateway.

mod deferrals;
mod entity_world;
mod error;
mod extensions;
mod host;
pub mod net_bridge;
pub mod observability;
mod resource_registry;
mod runtime;
mod state_bag;

pub use deferrals::{DeferralOutcome, DeferralRegistry};
pub use entity_world::{
    EntitySummary, EntityWorldView, NoWorldControl, ScriptEntityType, WorldCommand, WorldControl,
};
pub use error::ScriptError;
pub use extensions::{SharedVoice, VoiceControl};
pub use host::{ScriptHost, ScriptSource};
pub use net_bridge::{NetBridge, NetOutbound};
pub use observability::{
    DispatchKind, HandlerPerfStats, Observability, ProfilerRecordOptions, ProfilerStatus,
    ResMonSnapshot, ResourcePerfStats,
};
pub use resource_registry::{ResourceRegistry, ScriptResourceState};
pub use runtime::ScriptRuntime;
pub use state_bag::{
    entity_from_state_bag_name, entity_state_bag_name, player_from_state_bag_name,
    player_state_bag_name, InMemoryRoutingControl, RoutingControl, RoutingLockdownMode,
    StateBagChange, StateBagDelivery, StateBagSource, StateBagStore,
};
