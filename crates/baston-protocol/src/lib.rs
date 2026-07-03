//! Shared types exchanged between BASTON components (gateway, zone, scripting).

pub mod players;

pub use players::PlayerDirectory;

use serde::{Deserialize, Serialize};

/// Strongly-typed resource name (avoids passing raw `String`s around).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ResourceName(pub String);

impl ResourceName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ResourceName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ResourceName {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

/// A connected (or connecting) player, as tracked by the gateway.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerInfo {
    /// FiveM-style numeric source id, unique per session.
    pub source: u32,
    pub name: String,
    /// FiveM identifier strings, e.g. `ip:127.0.0.1`. Phase A only has `ip:`.
    pub identifiers: Vec<String>,
}

/// A resource manifest (`manifest.json`), BASTON's JSON replacement for `fxmanifest.lua`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceManifest {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub server_scripts: Vec<String>,
    #[serde(default)]
    pub client_scripts: Vec<String>,
    #[serde(default)]
    pub files: Vec<String>,
}
