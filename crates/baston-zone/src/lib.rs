//! Zone server: resource loader + lifecycle. Phase A runs in-process with the
//! gateway; the standalone binary split comes in Phase D.

pub mod boundary_detector;
pub mod boundary_loop;
pub mod entity_manager;
pub mod handoff_manager;
pub mod interest_ng;
pub mod mesh;
pub mod onesync;
pub mod ownership;
pub mod packfile;
pub mod resource_loader;
pub mod state_ingest;
pub mod state_sync;

pub use entity_manager::EntityManager;
pub use ownership::OwnershipMonitor;
pub use state_ingest::StateIngest;
pub use state_sync::StateSyncEmitter;

use std::path::PathBuf;

pub use resource_loader::{ResourceManager, ResourceState};

/// Errors produced by the zone / resource layer.
#[derive(Debug, thiserror::Error)]
pub enum ZoneError {
    #[error("failed to read manifest at {path}: {source}")]
    ManifestRead {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse manifest at {path}: {source}")]
    ManifestParse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("resource {resource} depends on unknown resource {dependency}")]
    UnknownDependency {
        resource: String,
        dependency: String,
    },
    #[error("cyclic resource dependency involving: {0:?}")]
    CyclicDependency(Vec<String>),
    #[error("unknown resource {0}")]
    UnknownResource(String),
    #[error("failed to read script {path}: {source}")]
    ScriptRead {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to decrypt script {path}: {source}")]
    Decrypt {
        path: PathBuf,
        source: baston_core::script_decryptor::DecryptError,
    },
    #[error("decrypted script {path} is not valid UTF-8")]
    ScriptNotUtf8 { path: PathBuf },
    #[error("scripting error: {0}")]
    Scripting(#[from] baston_scripting::ScriptError),
    #[error("file watcher error: {0}")]
    Watcher(String),
    #[error("NATS error: {0}")]
    Nats(String),
}
