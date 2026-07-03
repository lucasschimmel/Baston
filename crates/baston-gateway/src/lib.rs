//! HTTP gateway: axum server, player registry, CFX authentication, and the
//! `playerConnecting` connection pipeline.

pub mod admin;
pub mod auth;
pub mod connection_router;
pub mod http;
pub mod mesh;
pub mod mesh_forward;
pub mod players;
pub mod quadtree;
pub mod state_aggregator;
pub mod udp;
pub mod zone_registry;

pub use connection_router::ConnectionRouter;
pub use mesh::GatewayMesh;
pub use state_aggregator::StateAggregator;
pub use zone_registry::ZoneRegistry;

pub use auth::{AuthService, ValidatedPlayer};
pub use http::{router, AppState};
pub use players::PlayerRegistry;
