//! Resource loading: manifest parsing, dependency ordering, lifecycle,
//! and dev hot reload.

pub mod hot_reload;
pub mod manifest;
pub mod resource_manager;
pub mod topo;

pub use hot_reload::spawn_hot_reload;
pub use manifest::{discover, parse_manifest, DiscoveredResource};
pub use resource_manager::{ResourceManager, ResourceState};
pub use topo::topo_sort;
