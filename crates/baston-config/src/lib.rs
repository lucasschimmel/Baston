//! Runtime configuration for BASTON, loaded from `baston.toml` with
//! environment-variable overrides (`BASTON_PORT`, `BASTON_RESOURCES_PATH`).

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Errors produced while loading or parsing the configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse config file {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("invalid environment override {var}={value}: {reason}")]
    EnvOverride {
        var: &'static str,
        value: String,
        reason: String,
    },
}

/// Top-level BASTON configuration (`baston.toml`).
#[derive(Debug, Clone, Deserialize)]
pub struct BastonConfig {
    pub server: ServerConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub resources: ResourcesConfig,
    #[serde(default)]
    pub connection: ConnectionConfig,
    #[serde(default)]
    pub dev: DevConfig,
}

/// `[server]` section.
#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_server_name")]
    pub name: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_max_players")]
    pub max_players: u32,
}

/// `[auth]` section — CFX ticket validation (Phase B).
#[derive(Debug, Clone, Deserialize)]
pub struct AuthConfig {
    /// Endpoint serving the CFX ticket RSA public key
    /// (FXServer uses `CNL_ENDPOINT "api/ticket/pubkey"`).
    #[serde(default = "default_pubkey_url")]
    pub pubkey_url: String,
    /// Timeout for the public-key HTTP request, in seconds.
    #[serde(default = "default_auth_timeout")]
    pub http_timeout_secs: u64,
}

/// `[resources]` section.
#[derive(Debug, Clone, Deserialize)]
pub struct ResourcesConfig {
    #[serde(default = "default_resources_path")]
    pub path: PathBuf,
}

/// `[connection]` section.
#[derive(Debug, Clone, Deserialize)]
pub struct ConnectionConfig {
    /// Seconds to wait for `playerConnecting` deferrals before kicking.
    #[serde(default = "default_deferral_timeout")]
    pub deferral_timeout_secs: u64,
}

/// `[dev]` section.
#[derive(Debug, Clone, Deserialize)]
pub struct DevConfig {
    #[serde(default = "default_true")]
    pub hot_reload: bool,
    /// When true, skip CFX ticket validation and assign `license:dev-{source}`
    /// identifiers. Phase B default is false (real auth).
    #[serde(default)]
    pub auth_bypass: bool,
}

fn default_server_name() -> String {
    "BASTON Dev".to_owned()
}
fn default_port() -> u16 {
    30120
}
fn default_max_players() -> u32 {
    32
}
fn default_resources_path() -> PathBuf {
    PathBuf::from("resources")
}
fn default_deferral_timeout() -> u64 {
    10
}
fn default_pubkey_url() -> String {
    // FXServer: CNL_ENDPOINT "api/ticket/pubkey" (code/client/shared/CnlEndpoint.h)
    "https://lambda.fivem.net/api/ticket/pubkey".to_owned()
}
fn default_auth_timeout() -> u64 {
    5
}
fn default_true() -> bool {
    true
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            pubkey_url: default_pubkey_url(),
            http_timeout_secs: default_auth_timeout(),
        }
    }
}
impl Default for ResourcesConfig {
    fn default() -> Self {
        Self {
            path: default_resources_path(),
        }
    }
}
impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            deferral_timeout_secs: default_deferral_timeout(),
        }
    }
}
impl Default for DevConfig {
    fn default() -> Self {
        Self {
            hot_reload: true,
            auth_bypass: false,
        }
    }
}

impl BastonConfig {
    /// Load configuration from a TOML file, then apply environment overrides.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        let mut config: Self = toml::from_str(&raw).map_err(|source| ConfigError::Parse {
            path: path.to_owned(),
            source,
        })?;
        config.apply_env_overrides()?;
        Ok(config)
    }

    fn apply_env_overrides(&mut self) -> Result<(), ConfigError> {
        if let Ok(port) = std::env::var("BASTON_PORT") {
            self.server.port =
                port.parse()
                    .map_err(|e: std::num::ParseIntError| ConfigError::EnvOverride {
                        var: "BASTON_PORT",
                        value: port.clone(),
                        reason: e.to_string(),
                    })?;
        }
        if let Ok(path) = std::env::var("BASTON_RESOURCES_PATH") {
            self.resources.path = PathBuf::from(path);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_config() {
        let config: BastonConfig = toml::from_str("[server]\nport = 30120\n").unwrap();
        assert_eq!(config.server.port, 30120);
        assert!(!config.dev.auth_bypass);
        assert!(config.auth.pubkey_url.contains("lambda.fivem.net"));
        assert_eq!(config.connection.deferral_timeout_secs, 10);
        assert_eq!(config.resources.path, PathBuf::from("resources"));
    }
}
