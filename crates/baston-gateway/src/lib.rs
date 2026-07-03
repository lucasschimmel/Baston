//! HTTP gateway: axum server, player registry, CFX authentication, and the
//! `playerConnecting` connection pipeline.

pub mod auth;
pub mod http;
pub mod players;
pub mod udp;

pub use auth::{AuthService, ValidatedPlayer};
pub use http::{router, AppState};
pub use players::PlayerRegistry;
