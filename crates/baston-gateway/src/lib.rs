//! HTTP gateway: axum server, player registry, and the Phase A
//! `playerConnecting` connection pipeline.

pub mod http;
pub mod players;

pub use http::{router, AppState};
pub use players::PlayerRegistry;
