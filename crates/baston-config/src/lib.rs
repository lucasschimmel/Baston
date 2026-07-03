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
    pub udp: UdpConfig,
    #[serde(default)]
    pub nats: NatsConfig,
    #[serde(default)]
    pub state_sync: StateSyncConfig,
    #[serde(default)]
    pub metrics: MetricsConfig,
    #[serde(default)]
    pub dev: DevConfig,
}

/// `[nats]` section — state-sync IPC (Phase C).
#[derive(Debug, Clone, Deserialize)]
pub struct NatsConfig {
    #[serde(default = "default_nats_url")]
    pub url: String,
    /// Zone identifier used in the `baston.zone.{zone_id}.state` subject.
    #[serde(default = "default_zone_id")]
    pub zone_id: String,
}

/// `[state_sync]` section — Phase C pipeline tuning.
#[derive(Debug, Clone, Deserialize)]
pub struct StateSyncConfig {
    /// Zone → NATS emit interval (ms). 16ms = 62.5 fps.
    #[serde(default = "default_sync_interval_ms")]
    pub sync_interval_ms: u64,
    /// Gateway → clients push interval (ms). 50ms = 20 fps.
    #[serde(default = "default_push_interval_ms")]
    pub push_interval_ms: u64,
    /// Area-of-interest radius in meters (OneSync uses ~424; BASTON 450).
    #[serde(default = "default_aoi_radius")]
    pub aoi_radius: f32,
    /// Basic anti-cheat: max plausible displacement speed (m/s) for a ped.
    #[serde(default = "default_max_speed")]
    pub max_speed_mps: f32,
    /// Network-ownership reassignment scan interval (s) — anti flip-flop.
    #[serde(default = "default_ownership_interval")]
    pub ownership_interval_secs: u64,
}

/// `[metrics]` section — Prometheus exporter.
#[derive(Debug, Clone, Deserialize)]
pub struct MetricsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_metrics_port")]
    pub port: u16,
}

/// `[udp]` section — game transport (ENet).
#[derive(Debug, Clone, Deserialize)]
pub struct UdpConfig {
    /// UDP port for the ENet game channel. Defaults to `server.port`
    /// (FiveM muxes TCP and UDP on the same port number).
    #[serde(default)]
    pub port: Option<u16>,
    /// How often the ENet host is polled, in milliseconds.
    #[serde(default = "default_udp_poll_interval")]
    pub poll_interval_ms: u64,
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
fn default_udp_poll_interval() -> u64 {
    5
}
fn default_true() -> bool {
    true
}
fn default_nats_url() -> String {
    "nats://127.0.0.1:4222".to_owned()
}
fn default_zone_id() -> String {
    "zone-a".to_owned()
}
fn default_sync_interval_ms() -> u64 {
    16
}
fn default_push_interval_ms() -> u64 {
    50
}
fn default_aoi_radius() -> f32 {
    450.0
}
fn default_max_speed() -> f32 {
    200.0
}
fn default_ownership_interval() -> u64 {
    5
}
fn default_metrics_port() -> u16 {
    9090
}

impl Default for NatsConfig {
    fn default() -> Self {
        Self {
            url: default_nats_url(),
            zone_id: default_zone_id(),
        }
    }
}
impl Default for StateSyncConfig {
    fn default() -> Self {
        Self {
            sync_interval_ms: default_sync_interval_ms(),
            push_interval_ms: default_push_interval_ms(),
            aoi_radius: default_aoi_radius(),
            max_speed_mps: default_max_speed(),
            ownership_interval_secs: default_ownership_interval(),
        }
    }
}
impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            port: default_metrics_port(),
        }
    }
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
impl Default for UdpConfig {
    fn default() -> Self {
        Self {
            port: None,
            poll_interval_ms: default_udp_poll_interval(),
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
