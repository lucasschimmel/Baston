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
    #[error(
        "[escrow] enabled = true but server_license is empty\n  \
         → set [escrow] server_license = \"license:...\" in baston.toml\n  \
         → to disable escrow support: set [escrow] enabled = false"
    )]
    EscrowMissingLicense,
    #[error(
        "[escrow] backend = \"direct\" but dll_path is not set\n  \
         → set [escrow] dll_path = \"C:/FXServer/svadhesive.dll\" in baston.toml"
    )]
    EscrowMissingDllPath,
    #[error(
        "[escrow] dll_path \"{0}\" not found\n  \
         → install FXServer and point dll_path at its svadhesive.dll\n  \
         → to disable escrow support: set [escrow] enabled = false"
    )]
    EscrowDllNotFound(String),
    #[error(
        "[escrow] backend = \"sidecar\" but fxserver_path is not set\n  \
         → set [escrow] fxserver_path = \"C:/FXServer/FXServer.exe\" in baston.toml"
    )]
    EscrowMissingFxserverPath,
    #[error(
        "[escrow] fxserver_path \"{0}\" not found\n  \
         → install FXServer and point fxserver_path at its FXServer.exe\n  \
         → to disable escrow support: set [escrow] enabled = false"
    )]
    EscrowFxserverNotFound(String),
    #[error("[escrow] unknown backend \"{0}\" (expected \"sidecar\" or \"direct\")")]
    EscrowUnknownBackend(String),
}

/// `[tls]` section — HTTPS for packfile downloads (required by FiveM canary 31725+).
#[derive(Debug, Clone, Deserialize)]
pub struct TlsConfig {
    pub cert_pem: std::path::PathBuf,
    pub key_pem: std::path::PathBuf,
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
    #[serde(default)]
    pub meshing: MeshingConfig,
    #[serde(default)]
    pub escrow: EscrowConfig,
    pub tls: Option<TlsConfig>,
}

/// `[escrow]` section — CFX Asset Escrow support (Phase D-bis).
///
/// Off by default. When enabled, the composition-root binary (built with the
/// `escrow` feature, on Windows) installs `baston-escrow-plugin`. The default
/// backend is `sidecar`: preliminary research showed `svadhesive.dll` exposes
/// no FFI-callable decrypt symbol, so the `direct` backend is unsupported.
#[derive(Debug, Clone, Deserialize)]
pub struct EscrowConfig {
    /// Enable escrow support. Never activates without this being explicitly true.
    #[serde(default)]
    pub enabled: bool,
    /// `"sidecar"` (supported) or `"direct"` (unsupported — see crate docs).
    #[serde(default = "default_escrow_backend")]
    pub backend: String,
    /// CFX server licence (`"license:..."`). Required when `enabled`.
    #[serde(default)]
    pub server_license: String,
    /// Path to `svadhesive.dll` (backend = `"direct"`).
    #[serde(default)]
    pub dll_path: Option<PathBuf>,
    /// Path to `FXServer.exe` (backend = `"sidecar"`).
    #[serde(default)]
    pub fxserver_path: Option<PathBuf>,
}

impl Default for EscrowConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: default_escrow_backend(),
            server_license: String::new(),
            dll_path: None,
            fxserver_path: None,
        }
    }
}

fn default_escrow_backend() -> String {
    "sidecar".to_owned()
}

impl EscrowConfig {
    /// Validate the escrow section. No-op when disabled; otherwise checks the
    /// licence and the backend-specific binary path exist, with actionable
    /// error messages for a fatal startup failure.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !self.enabled {
            return Ok(());
        }
        if self.server_license.is_empty() {
            return Err(ConfigError::EscrowMissingLicense);
        }
        match self.backend.as_str() {
            "sidecar" => {
                let path = self
                    .fxserver_path
                    .as_ref()
                    .ok_or(ConfigError::EscrowMissingFxserverPath)?;
                if !path.exists() {
                    return Err(ConfigError::EscrowFxserverNotFound(
                        path.display().to_string(),
                    ));
                }
            }
            "direct" => {
                let path = self
                    .dll_path
                    .as_ref()
                    .ok_or(ConfigError::EscrowMissingDllPath)?;
                if !path.exists() {
                    return Err(ConfigError::EscrowDllNotFound(path.display().to_string()));
                }
            }
            other => return Err(ConfigError::EscrowUnknownBackend(other.to_owned())),
        }
        Ok(())
    }
}

/// `[meshing]` section — Phase D zone federation.
#[derive(Debug, Clone, Deserialize)]
pub struct MeshingConfig {
    /// Enables the federation layer (gRPC servers, registration, heartbeats).
    #[serde(default)]
    pub enabled: bool,
    /// Gateway gRPC listen address (GatewayService).
    #[serde(default = "default_gateway_grpc_addr")]
    pub gateway_grpc_addr: String,
    /// Gateway gRPC address as seen from the zones (registration target).
    #[serde(default = "default_gateway_grpc_target")]
    pub gateway_grpc: String,
    /// Zone gRPC listen address (ZoneService).
    #[serde(default = "default_zone_grpc_addr")]
    pub zone_grpc_addr: String,
    /// Zone gRPC address as seen from the Gateway (what we register).
    /// Defaults to `{ZONE_ID}:50051` under Docker; falls back to
    /// `zone_grpc_addr` with 0.0.0.0 replaced by 127.0.0.1.
    #[serde(default)]
    pub zone_public_grpc_addr: Option<String>,
    /// Zone bounds `x_min,y_min,x_max,y_max` (env `ZONE_BOUNDS` overrides).
    #[serde(default)]
    pub zone_bounds: Option<String>,
    #[serde(default = "default_heartbeat_interval")]
    pub heartbeat_interval_secs: u64,
    /// Silence window before the Gateway evicts a zone (3 missed heartbeats).
    #[serde(default = "default_zone_timeout")]
    pub zone_timeout_secs: u64,
    /// Distance to the zone edge that triggers handoff preparation (m).
    #[serde(default = "default_boundary_margin")]
    pub boundary_margin: f32,
    /// Boundary scan interval (ms) — coarser than state sync on purpose.
    #[serde(default = "default_boundary_scan_interval_ms")]
    pub boundary_scan_interval_ms: u64,
    /// Anti ping-pong: minimum seconds between two handoffs of one player.
    #[serde(default = "default_handoff_cooldown")]
    pub handoff_cooldown_secs: u64,
    /// Admin HTTP API port (Gateway).
    #[serde(default = "default_admin_port")]
    pub admin_port: u16,
    /// Bearer token for the admin API. Empty = admin API disabled.
    #[serde(default)]
    pub admin_token: String,
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
fn default_gateway_grpc_addr() -> String {
    "0.0.0.0:50050".to_owned()
}
fn default_gateway_grpc_target() -> String {
    "127.0.0.1:50050".to_owned()
}
fn default_zone_grpc_addr() -> String {
    "0.0.0.0:50051".to_owned()
}
fn default_heartbeat_interval() -> u64 {
    5
}
fn default_zone_timeout() -> u64 {
    15
}
fn default_boundary_margin() -> f32 {
    300.0
}
fn default_boundary_scan_interval_ms() -> u64 {
    500
}
fn default_handoff_cooldown() -> u64 {
    5
}
fn default_admin_port() -> u16 {
    8080
}

impl Default for MeshingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            gateway_grpc_addr: default_gateway_grpc_addr(),
            gateway_grpc: default_gateway_grpc_target(),
            zone_grpc_addr: default_zone_grpc_addr(),
            zone_public_grpc_addr: None,
            zone_bounds: None,
            heartbeat_interval_secs: default_heartbeat_interval(),
            zone_timeout_secs: default_zone_timeout(),
            boundary_margin: default_boundary_margin(),
            boundary_scan_interval_ms: default_boundary_scan_interval_ms(),
            handoff_cooldown_secs: default_handoff_cooldown(),
            admin_port: default_admin_port(),
            admin_token: String::new(),
        }
    }
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
        // Phase D federation overrides (Docker Compose contract).
        if let Ok(zone_id) = std::env::var("ZONE_ID") {
            self.nats.zone_id = zone_id;
        }
        if let Ok(bounds) = std::env::var("ZONE_BOUNDS") {
            self.meshing.zone_bounds = Some(bounds);
        }
        if let Ok(addr) = std::env::var("GATEWAY_GRPC") {
            self.meshing.gateway_grpc = addr;
        }
        if let Ok(url) = std::env::var("NATS_URL") {
            self.nats.url = url;
        }
        if let Ok(addr) = std::env::var("BASTON_GRPC_ADDR") {
            self.meshing.gateway_grpc_addr = addr;
        }
        if let Ok(addr) = std::env::var("ZONE_GRPC_ADDR") {
            self.meshing.zone_grpc_addr = addr;
        }
        if let Ok(addr) = std::env::var("ZONE_PUBLIC_GRPC_ADDR") {
            self.meshing.zone_public_grpc_addr = Some(addr);
        }
        if let Ok(token) = std::env::var("BASTON_ADMIN_TOKEN") {
            self.meshing.admin_token = token;
        }
        if let Ok(enabled) = std::env::var("BASTON_MESHING_ENABLED") {
            self.meshing.enabled = matches!(enabled.as_str(), "true" | "1" | "yes");
        }
        if let Ok(port) = std::env::var("BASTON_METRICS_PORT") {
            self.metrics.port =
                port.parse()
                    .map_err(|e: std::num::ParseIntError| ConfigError::EnvOverride {
                        var: "BASTON_METRICS_PORT",
                        value: port.clone(),
                        reason: e.to_string(),
                    })?;
        }
        Ok(())
    }

    /// The ZoneService address to register with the Gateway.
    pub fn zone_public_grpc_addr(&self) -> String {
        if let Some(addr) = &self.meshing.zone_public_grpc_addr {
            return addr.clone();
        }
        // Docker DNS convention: the service is reachable as {ZONE_ID}:{port}.
        let port = self
            .meshing
            .zone_grpc_addr
            .rsplit(':')
            .next()
            .unwrap_or("50051");
        if std::env::var("ZONE_ID").is_ok() {
            format!("{}:{}", self.nats.zone_id, port)
        } else {
            format!("127.0.0.1:{port}")
        }
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

    #[test]
    fn escrow_defaults_off_and_validates_trivially() {
        let config: BastonConfig = toml::from_str("[server]\nport = 30120\n").unwrap();
        assert!(!config.escrow.enabled);
        assert_eq!(config.escrow.backend, "sidecar");
        config.escrow.validate().expect("disabled escrow is always valid");
    }

    #[test]
    fn escrow_enabled_without_license_is_rejected() {
        let escrow = EscrowConfig {
            enabled: true,
            ..Default::default()
        };
        assert!(matches!(
            escrow.validate(),
            Err(ConfigError::EscrowMissingLicense)
        ));
    }

    #[test]
    fn escrow_sidecar_missing_fxserver_path_is_rejected() {
        let escrow = EscrowConfig {
            enabled: true,
            backend: "sidecar".into(),
            server_license: "license:abc".into(),
            ..Default::default()
        };
        assert!(matches!(
            escrow.validate(),
            Err(ConfigError::EscrowMissingFxserverPath)
        ));
    }

    #[test]
    fn escrow_unknown_backend_is_rejected() {
        let escrow = EscrowConfig {
            enabled: true,
            backend: "carrier-pigeon".into(),
            server_license: "license:abc".into(),
            ..Default::default()
        };
        assert!(matches!(
            escrow.validate(),
            Err(ConfigError::EscrowUnknownBackend(_))
        ));
    }
}
