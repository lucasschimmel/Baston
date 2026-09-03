//! HTTP gateway: axum server, player registry, CFX authentication, and the
//! `playerConnecting` connection pipeline.

pub mod admin;
pub mod api;
pub mod auth;
pub mod cfx;
pub mod connection_router;
#[cfg(feature = "db")]
pub mod db;
pub mod debug_info;
pub mod http;
pub mod mesh;
pub mod mesh_forward;
pub mod players;
pub mod script_http;
pub mod state_aggregator;
pub mod udp;
pub mod voice;
pub mod world_control;
pub mod zone_registry;

pub use connection_router::ConnectionRouter;
pub use debug_info::{DebugInfoFeed, MeshView};
pub use mesh::GatewayMesh;
pub use state_aggregator::StateAggregator;
pub use world_control::GatewayWorldControl;
pub use zone_registry::ZoneRegistry;

pub use auth::{AuthService, ValidatedPlayer};
pub use http::{router, AppState};
pub use players::PlayerRegistry;
