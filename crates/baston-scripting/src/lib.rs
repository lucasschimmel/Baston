//! deno_core-based scripting runtime for BASTON.
//!
//! - [`ScriptRuntime`]: one V8 isolate per resource (FiveM polyfills injected).
//! - [`ScriptHost`]: owns all runtimes on a dedicated thread; the process
//!   talks to it via a `Send` handle.
//! - [`DeferralRegistry`]: `playerConnecting` deferral state shared with the
//!   HTTP gateway.

mod deferrals;
mod error;
mod extensions;
mod host;
pub mod net_bridge;
pub mod observability;
mod runtime;

pub use deferrals::{DeferralOutcome, DeferralRegistry};
pub use error::ScriptError;
pub use host::{ScriptHost, ScriptSource};
pub use net_bridge::{NetBridge, NetOutbound};
pub use observability::{
    DispatchKind, HandlerPerfStats, Observability, ProfilerRecordOptions, ProfilerStatus,
    ResMonSnapshot, ResourcePerfStats,
};
pub use runtime::ScriptRuntime;
