//! Runtime configuration for BASTON, loaded from `baston.toml` with
//! environment-variable overrides (`BASTON_PORT`, `BASTON_RESOURCES_PATH`).

use std::collections::BTreeMap;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub mod modules;
pub use baston_modules::{Bundle, ModuleId, ModuleSet};
pub use modules::{LegacyToggles, ModulesConfig};

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
    #[error("[{section}] invalid configuration: {reason}")]
    Invalid {
        section: &'static str,
        reason: String,
    },
    #[error("invalid environment override {var}={value}: {reason}")]
    EnvOverride {
        var: &'static str,
        value: String,
        reason: String,
    },
    #[error(
        "[license] mode = \"{0}\" requires a licence key\n  \
         → set [license] sv_license_key = \"cfxk_...\" (create one at https://portal.cfx.re)\n  \
         → for local dev/LAN only: set [license] mode = \"off\""
    )]
    LicenseMissingKey(String),
    #[error(
        "[license] sv_license_key does not look like a real CFX key (it is empty, a \
         placeholder, or malformed)\n  \
         → paste your real key from https://portal.cfx.re\n  \
         → note: \"gate\" only checks the key's shape — BASTON does not validate it \
         against CFX"
    )]
    LicenseMalformedKey,
    #[error(
        "[[api.keys]] entry without a name\n  \
         → give every key a name, e.g. [[api.keys]] name = \"discord-bot\" — it \
         identifies the key in the audit log"
    )]
    ApiKeyMissingName,
    #[error(
        "[[api.keys]] duplicate key name \"{0}\"\n  \
         → key names must be unique; rename one of the entries"
    )]
    ApiKeyDuplicateName(String),
    #[error(
        "[[api.keys]] key \"{0}\" has a weak or placeholder token\n  \
         → tokens must be at least 32 characters, without whitespace\n  \
         → generate one: openssl rand -hex 32"
    )]
    ApiKeyWeakToken(String),
    #[error(
        "[[api.keys]] key \"{0}\" reuses another key's token\n  \
         → every key needs its own token, or the audit log can't tell them apart"
    )]
    ApiKeyDuplicateToken(String),
    #[error(
        "[[api.keys]] key \"{0}\" has no permissions\n  \
         → grant at least one of: \"monitor.read\", \"resource.control\", \
         \"player.kick\", \"zone.drain\", \"profiler.control\", \"profiler.read\", \
         \"console.execute\""
    )]
    ApiKeyNoPermissions(String),
    #[error(
        "module \"{module}\" is configured in two places that disagree\n  \
         → {legacy_site} says {legacy_value}, but [modules] {list} says the opposite\n  \
         → keep one of the two; {legacy_site} is the older spelling and still works"
    )]
    ModuleConflict {
        module: &'static str,
        legacy_site: &'static str,
        legacy_value: bool,
        list: &'static str,
    },
    #[error(
        "module \"{module}\" is not compiled into this build\n  \
         → it ships in bundle {bundle}\n  \
         → run `baston-gateway --modules` to see what this binary contains"
    )]
    ModuleNotCompiledIn {
        module: &'static str,
        bundle: &'static str,
    },
    #[error(
        "invalid module override {var}={value}\n  \
         → expected one of: true/false, 1/0, yes/no, on/off"
    )]
    ModuleEnvOverride { var: String, value: String },
    #[error(
        "[db] the db module is enabled but url is empty\n  \
         → set [db] url = \"postgres://user:pass@host/base\" (or sqlite:baston.db)\n  \
         → to disable database access: remove \"db\" from [modules] enable"
    )]
    DbMissingUrl,
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
    pub debug: DebugConfig,
    #[serde(default)]
    pub meshing: MeshingConfig,
    #[serde(default)]
    pub license: LicenseConfig,
    #[serde(default)]
    pub listing: ListingConfig,
    #[serde(default)]
    pub api: ApiConfig,
    #[serde(default)]
    pub voice: VoiceConfig,
    #[serde(default)]
    pub db: DbConfig,
    #[serde(default)]
    pub modules: ModulesConfig,
    pub tls: Option<TlsConfig>,
    /// Modules resolved from `[modules]`, the legacy section flags and the
    /// environment. Populated by [`BastonConfig::load`]; a config built straight
    /// from `toml::from_str` in a test carries an empty set until
    /// [`BastonConfig::resolve_modules`] runs.
    #[serde(skip)]
    pub enabled_modules: ModuleSet,
    /// `(section, module)` pairs whose module is off, so the boot path can warn
    /// that those settings are inert instead of leaving the operator guessing.
    #[serde(skip)]
    pub inert_sections: Vec<(&'static str, &'static str)>,
    /// Directory the configuration was read from. Paths named inside the file
    /// resolve against it rather than against the working directory.
    #[serde(skip)]
    pub config_dir: Option<PathBuf>,
}

/// `[voice]` section — the embedded Mumble-compatible voice server.
#[derive(Debug, Clone, Deserialize)]
pub struct VoiceConfig {
    /// Off by default: enabling binds a TCP(TLS)+UDP listener on `port`.
    #[serde(default)]
    pub enabled: bool,
    /// Voice port (TCP control + UDP voice share the number). Defaults to
    /// game port + 1 by convention; must differ from `server.port`.
    #[serde(default = "default_voice_port")]
    pub port: u16,
    /// Address advertised to clients via the replicated
    /// `voice_externalAddress` convar (their embedded Mumble connects to
    /// `external_address:port`). Empty = do not advertise — clients will not
    /// find the voice server. Set it to the address players use to reach the
    /// server (e.g. `127.0.0.1` for local tests, your public IP otherwise).
    #[serde(default)]
    pub external_address: String,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: default_voice_port(),
            external_address: String::new(),
        }
    }
}

fn default_voice_port() -> u16 {
    30121
}

/// `[api]` section — keys for the monitoring/control HTTP API (served on the
/// admin port alongside the legacy `/admin/*` routes).
///
/// Each `[[api.keys]]` entry grants a set of permissions to one bearer token.
/// A key can only call routes covered by its permissions — a monitoring key
/// gets 403 on every control route. The legacy `meshing.admin_token` keeps
/// working as an implicit full-permission key.
#[derive(Debug, Clone, Deserialize)]
pub struct ApiConfig {
    #[serde(default)]
    pub keys: Vec<ApiKey>,
    /// Append-only JSONL audit log for control actions. Every kick, resource
    /// start/stop and zone drain is recorded with the key name that did it.
    #[serde(default = "default_audit_log")]
    pub audit_log: PathBuf,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            keys: Vec::new(),
            audit_log: default_audit_log(),
        }
    }
}

fn default_audit_log() -> PathBuf {
    PathBuf::from("baston-audit.jsonl")
}

/// One API key: a named bearer token with explicit permissions.
#[derive(Debug, Clone, Deserialize)]
pub struct ApiKey {
    /// Identifies the key in the audit log (e.g. `"discord-bot"`, `"panel"`).
    pub name: String,
    pub token: String,
    pub permissions: Vec<ApiPermission>,
}

/// Granular API permissions. Unknown strings are rejected at parse time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum ApiPermission {
    /// Read-only monitoring: status, players, zones, resources.
    #[serde(rename = "monitor.read")]
    MonitorRead,
    /// Start / stop / restart resources.
    #[serde(rename = "resource.control")]
    ResourceControl,
    /// Kick players.
    #[serde(rename = "player.kick")]
    PlayerKick,
    /// Drain zones (reroute their players).
    #[serde(rename = "zone.drain")]
    ZoneDrain,
    /// Start/stop profiler recordings.
    #[serde(rename = "profiler.control")]
    ProfilerControl,
    /// Read profiler captures.
    #[serde(rename = "profiler.read")]
    ProfilerRead,
    /// Execute bounded admin console commands.
    #[serde(rename = "console.execute")]
    ConsoleExecute,
}

impl ApiConfig {
    /// Validate the key list: named, well-formed tokens, no duplicates, at
    /// least one permission each. Tokens are secrets — errors mention the key
    /// *name*, never the token value.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut names = std::collections::HashSet::new();
        let mut tokens = std::collections::HashSet::new();
        for key in &self.keys {
            if key.name.trim().is_empty() {
                return Err(ConfigError::ApiKeyMissingName);
            }
            if !names.insert(key.name.as_str()) {
                return Err(ConfigError::ApiKeyDuplicateName(key.name.clone()));
            }
            let token = key.token.trim();
            if token.len() < 32
                || token.chars().any(char::is_whitespace)
                || token.to_ascii_uppercase().contains("REPLACE_ME")
            {
                return Err(ConfigError::ApiKeyWeakToken(key.name.clone()));
            }
            if !tokens.insert(token) {
                return Err(ConfigError::ApiKeyDuplicateToken(key.name.clone()));
            }
            if key.permissions.is_empty() {
                return Err(ConfigError::ApiKeyNoPermissions(key.name.clone()));
            }
        }
        Ok(())
    }
}

/// `[license]` section — the operator's CFX server-licence key.
///
/// Modes:
/// - `"off"`: no check (dev/LAN only). Emits a visible warning each boot.
/// - `"gate"`: require a well-formed `sv_license_key` — shape only, never
///   validity. Catches the empty or placeholder key before you go live.
/// - `"cfx"`: validate the key with CFX, read the entitlements it grants, and
///   apply them **restrictively** before any listener opens. Required for
///   [`ListingConfig`].
///
/// `"cfx"` couples two things an operator should understand before choosing
/// it: the server becomes discoverable, *and* its slot count becomes bounded
/// by what the licence actually grants. A server that wants more slots than
/// its tier allows wants `"off"`. See
/// `docs/adr/004-cfx-identity-without-fxserver.md`.
#[derive(Clone, Deserialize)]
pub struct LicenseConfig {
    /// `off` | `gate` | `cfx`.
    #[serde(default)]
    pub mode: LicenseMode,
    /// CFX server licence key, created at <https://portal.cfx.re>.
    #[serde(default)]
    pub sv_license_key: String,
}

impl fmt::Debug for LicenseConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LicenseConfig")
            .field("mode", &self.mode)
            .field("sv_license_key", &"[REDACTED]")
            .finish()
    }
}

impl Default for LicenseConfig {
    fn default() -> Self {
        Self {
            mode: LicenseMode::Off,
            sv_license_key: String::new(),
        }
    }
}

/// Licence enforcement mode (`[license] mode`). Missing → `Off` so existing
/// dev/LAN configs keep booting (see `docs/operations/licensing.md`). Unknown
/// values are rejected by serde at parse time — including the removed
/// `"verified"`, which must stop a boot rather than silently downgrade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LicenseMode {
    /// No check (dev/LAN only). Emits a visible warning each boot.
    #[default]
    Off,
    /// Require a well-formed `sv_license_key`. Shape only — never validity.
    Gate,
    /// Authenticate with CFX and enforce what the licence grants.
    Cfx,
}

impl LicenseMode {
    /// Whether this mode talks to CFX at boot.
    #[must_use]
    pub fn authenticates(self) -> bool {
        matches!(self, Self::Cfx)
    }
}

impl LicenseConfig {
    /// A key is well-formed when it is non-empty, whitespace-free, long enough,
    /// and not a placeholder. This is a *shape* check only — it never proves the
    /// key is valid; BASTON has no authenticated path to CFX.
    pub fn is_well_formed_key(&self) -> bool {
        let key = self.sv_license_key.trim();
        !key.is_empty()
            && key.len() >= 20
            && !key.chars().any(char::is_whitespace)
            && !key.to_ascii_uppercase().contains("REPLACE_ME")
    }

    /// Validate the licence section. Fatal, actionable errors on
    /// misconfiguration; no-op for `"off"`.
    pub fn validate(&self) -> Result<(), ConfigError> {
        match self.mode {
            LicenseMode::Off => Ok(()),
            LicenseMode::Gate | LicenseMode::Cfx => {
                let mode = if self.mode == LicenseMode::Cfx {
                    "cfx"
                } else {
                    "gate"
                };
                if self.sv_license_key.trim().is_empty() {
                    return Err(ConfigError::LicenseMissingKey(mode.into()));
                }
                if !self.is_well_formed_key() {
                    return Err(ConfigError::LicenseMalformedKey);
                }
                Ok(())
            }
        }
    }
}

/// `[listing]` section — presence in the public CFX server list.
///
/// Off by default, and requires `[license] mode = "cfx"`: the heartbeat is
/// signed with a credential only that exchange produces, and being listed
/// without publishing the licence token would mean being discoverable while no
/// client ever checks the slot count.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListingConfig {
    #[serde(default)]
    pub enabled: bool,
    /// The public address players connect to, advertised to the server list.
    /// Required when enabled; BASTON refuses to guess it.
    #[serde(default)]
    pub ip_override: Option<IpAddr>,
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
    ///
    /// What a zone declares about itself. A Gateway holding a `map_file`
    /// overrules it: the map is then the single source of truth and the zone
    /// is told its territory in the registration reply.
    #[serde(default)]
    pub zone_bounds: Option<String>,
    /// Zone map, relative to the directory holding this file (env
    /// `BASTON_MAP_FILE` overrides). Gateway only.
    ///
    /// Unset means zones declare their own rectangles, which is how meshing
    /// worked before maps existed and stays the default.
    #[serde(default)]
    pub map_file: Option<String>,
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
    /// OneSync mode. `Off` keeps the msgRoute P2P relay (default, battle-tested);
    /// `On` makes the server parse entity clones authoritatively (OneSync-NG).
    #[serde(default)]
    pub onesync: OneSyncMode,
    /// Dynamically adjust the authoritative outbound rate based on measured
    /// work and transport pressure.
    #[serde(default = "default_true")]
    pub adaptive_tick_enabled: bool,
    /// Lowest rate selected while overloaded.
    #[serde(default = "default_tick_min_hz")]
    pub tick_min_hz: u16,
    /// Initial operating rate before measurements are available.
    #[serde(default = "default_tick_default_hz")]
    pub tick_default_hz: u16,
    /// Hard ceiling. Validation rejects values above 120 Hz.
    #[serde(default = "default_tick_max_hz")]
    pub tick_max_hz: u16,
    /// Utilization at which the controller immediately backs off.
    #[serde(default = "default_tick_high_utilization")]
    pub tick_high_utilization: f32,
    /// Utilization below which a sample counts as headroom.
    #[serde(default = "default_tick_low_utilization")]
    pub tick_low_utilization: f32,
    /// Consecutive headroom samples required before increasing the rate.
    #[serde(default = "default_tick_recovery_window")]
    pub tick_recovery_window: u32,
    /// Multiplicative rate retained on overload (`0 < value < 1`).
    #[serde(default = "default_tick_overload_backoff")]
    pub tick_overload_backoff: f32,
    /// Per-client clone payload budget for one outbound tick.
    #[serde(default = "default_interest_budget_bytes")]
    pub interest_budget_bytes: usize,
    /// Maximum bytes spent on scope removals per client and tick.
    #[serde(default = "default_interest_remove_budget_bytes")]
    pub interest_remove_budget_bytes: usize,
    #[serde(default = "default_interest_distance_weight")]
    pub interest_distance_weight: f32,
    #[serde(default = "default_interest_closing_weight")]
    pub interest_closing_weight: f32,
    #[serde(default = "default_interest_staleness_weight")]
    pub interest_staleness_weight: f32,
    /// Extra distance retained for entities already in scope, preventing
    /// boundary jitter from causing create/remove churn.
    #[serde(default = "default_interest_hysteresis_m")]
    pub interest_hysteresis_m: f32,
}

/// Server entity-sync mode advertised to clients and driving the game-state
/// path. Mirrors FiveM's `onesync` convar (`Off` / `On`); BASTON's `On` maps
/// to Infinity-style big mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OneSyncMode {
    /// P2P relay via msgRoute — the GTA netcode syncs entities client-to-client.
    #[default]
    Off,
    /// Server-authoritative clone parsing (OneSync-NG, big mode).
    On,
}

impl OneSyncMode {
    pub fn is_enabled(self) -> bool {
        matches!(self, OneSyncMode::On)
    }

    /// The string advertised in the `onesync` replicated convar.
    pub fn convar_value(self) -> &'static str {
        match self {
            OneSyncMode::Off => "off",
            OneSyncMode::On => "on",
        }
    }
}

impl StateSyncConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.tick_min_hz == 0
            || self.tick_min_hz > self.tick_default_hz
            || self.tick_default_hz > self.tick_max_hz
            || self.tick_max_hz > 120
        {
            return Err(ConfigError::Invalid {
                section: "state_sync",
                reason: format!(
                    "tick rates must satisfy 1 <= min ({}) <= default ({}) <= max ({}) <= 120 Hz",
                    self.tick_min_hz, self.tick_default_hz, self.tick_max_hz
                ),
            });
        }
        if !(0.0..1.0).contains(&self.tick_low_utilization)
            || !(0.0..=1.0).contains(&self.tick_high_utilization)
            || self.tick_low_utilization >= self.tick_high_utilization
        {
            return Err(ConfigError::Invalid {
                section: "state_sync",
                reason: "tick utilization thresholds must satisfy 0 <= low < high <= 1".to_owned(),
            });
        }
        if !(0.0..1.0).contains(&self.tick_overload_backoff) {
            return Err(ConfigError::Invalid {
                section: "state_sync",
                reason: "tick_overload_backoff must be greater than 0 and less than 1".to_owned(),
            });
        }
        if self.tick_recovery_window == 0
            || self.interest_budget_bytes == 0
            || self.interest_remove_budget_bytes == 0
            || !self.aoi_radius.is_finite()
            || self.aoi_radius <= 0.0
            || !self.interest_hysteresis_m.is_finite()
            || self.interest_hysteresis_m < 0.0
        {
            return Err(ConfigError::Invalid {
                section: "state_sync",
                reason: "recovery window, budgets and AoI must be positive; hysteresis must be finite and non-negative".to_owned(),
            });
        }
        for (name, value) in [
            ("interest_distance_weight", self.interest_distance_weight),
            ("interest_closing_weight", self.interest_closing_weight),
            ("interest_staleness_weight", self.interest_staleness_weight),
        ] {
            if !value.is_finite() || value < 0.0 {
                return Err(ConfigError::Invalid {
                    section: "state_sync",
                    reason: format!("{name} must be finite and non-negative"),
                });
            }
        }
        Ok(())
    }
}

/// `[db]` section — pooled database access for scripts (ADR-002, Tier 2).
///
/// One `url` rather than discrete host/user/password fields: it is what every
/// driver documents, what a hosting panel hands out, and it keeps the
/// credential in one place an operator can move to an environment variable.
#[derive(Clone, Deserialize)]
pub struct DbConfig {
    /// Connection URL. `sqlite:…`, `postgres://…` or `mysql://…`.
    ///
    /// Empty means the module has nothing to connect to; the loader says so
    /// rather than starting a server whose first query fails.
    #[serde(default)]
    pub url: String,
    #[serde(default = "default_db_pool_size")]
    pub pool_size: u32,
    #[serde(default = "default_db_query_timeout")]
    pub query_timeout_secs: u64,
}

impl Default for DbConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            pool_size: default_db_pool_size(),
            query_timeout_secs: default_db_query_timeout(),
        }
    }
}

/// The URL carries a password, so it never reaches a log or a bug report.
impl fmt::Debug for DbConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DbConfig")
            .field(
                "url",
                &if self.url.is_empty() {
                    "<unset>"
                } else {
                    "<redacted>"
                },
            )
            .field("pool_size", &self.pool_size)
            .field("query_timeout_secs", &self.query_timeout_secs)
            .finish()
    }
}

impl DbConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.url.trim().is_empty() {
            return Err(ConfigError::DbMissingUrl);
        }
        if self.pool_size == 0 {
            return Err(ConfigError::Invalid {
                section: "db",
                reason: "pool_size must be at least 1".to_owned(),
            });
        }
        Ok(())
    }
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

/// Game build enforced when the operator states none.
///
/// It must stay equal to `baston_protocol::rage::sync_parse::GameBuild::default()`
/// — the build the sync-tree decoder falls back to. A test in `baston-gateway`,
/// which depends on both crates, holds the two together; this crate does not
/// depend on `baston-protocol` for one integer.
pub const DEFAULT_GAME_BUILD: u32 = 3258;

/// Oldest build FiveM still lets a client run.
const MIN_GAME_BUILD: u32 = 1604;

/// Upper bound on a game build.
///
/// A typo catcher, not an allowlist: a build Rockstar ships next has to work
/// without a code change, the same way `[server.vars]` passes through fields
/// BASTON has never heard of. It exists so `"32258"` fails at boot with its own
/// name in the error instead of at connect time, in the client, as a build
/// switch that never happens.
const MAX_GAME_BUILD: u32 = 4999;

/// `[server]` section.
#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_server_name")]
    pub name: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Local interface for the public HTTP and UDP game listeners.
    #[serde(default = "default_bind_address")]
    pub bind_address: IpAddr,
    #[serde(default = "default_max_players")]
    pub max_players: u32,
    /// Game build advertised as `sv_enforceGameBuild` in `info.json` vars.
    ///
    /// The client reads it *before* connecting and build-switches to it
    /// (NetLibrary.cpp), which is what decides the game content a player has:
    /// the weapons, vehicles and DLC props of that build, and no others. It is
    /// also the build whose sync-tree node layouts the server decodes against,
    /// so the two are one setting on purpose — see [`ServerConfig::game_build`].
    ///
    /// Empty string = no enforcement: every client keeps whatever build it
    /// launched, mixed-build clients end up in the same non-OneSync session and
    /// the GTA P2P join fails ("Could not connect to session provider"). The
    /// default is [`DEFAULT_GAME_BUILD`] rather than empty, because a server
    /// that enforces nothing still has to decode *some* build's layouts, and an
    /// unstated one is the same choice made silently.
    #[serde(default = "default_enforce_game_build")]
    pub enforce_game_build: String,
    /// A 96×96 PNG published as `info.json` `icon`, base64-encoded — the
    /// picture the FiveM server browser shows next to the name. Both the size
    /// and the format are the client's requirement, not BASTON's, and a file
    /// that is neither is refused at load rather than silently dropped.
    #[serde(default)]
    pub icon: Option<PathBuf>,
    /// Replicated server variables, published in `info.json` `vars` and
    /// advertised to the server list.
    ///
    /// This is CFX's `sets` mechanism. FXServer does not know the name of any
    /// individual field either — `InfoHttpHandler.cpp` iterates every convar
    /// carrying `ConVar_ServerInfo` and publishes what it finds. So this map
    /// is a passthrough, and the fields the server browser reads today
    /// (`sv_projectName`, `sv_projectDesc`, `tags`, `locale`, `banner_detail`,
    /// `banner_connecting`) work without BASTON knowing them, as will whatever
    /// CFX adds next.
    ///
    /// **Everything here is public.** It is served to anyone who asks for
    /// `/info.json`, before authentication. Do not put a secret in it.
    ///
    /// A `BTreeMap` so `/info.json` is byte-stable across boots: an unstable
    /// key order would make the document's hash change for no reason.
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
}

impl ServerConfig {
    /// The enforced game build, as a number.
    ///
    /// `None` means `enforce_game_build` is empty: nothing is advertised and
    /// the server cannot know which build a connecting client runs.
    ///
    /// This is the **only** place the string becomes a number. [`Self::validate`]
    /// and the caller that feeds the sync-tree decoder both come through here,
    /// so a value that boots is a value the decoder agrees with — the failure
    /// this closes is a config typo that used to be swallowed by a parse
    /// fallback, leaving the server decoding one build's node layouts while its
    /// clients ran another.
    pub fn game_build(&self) -> Result<Option<u32>, ConfigError> {
        let raw = self.enforce_game_build.as_str();
        if raw.is_empty() {
            return Ok(None);
        }
        let invalid = |reason: String| ConfigError::Invalid {
            section: "server",
            reason: format!(
                "enforce_game_build = \"{raw}\" is not a game build ({reason})\n  \
                 → use the build number your resources need, e.g. \"{DEFAULT_GAME_BUILD}\"\n  \
                 → use \"\" to enforce nothing and let every client keep its own build"
            ),
        };
        // Not `parse` alone: it accepts "+3258" and "03258", and the
        // `<build>_<revision>` form is what the *client* reports, never what an
        // operator enforces.
        if !raw.chars().all(|c| c.is_ascii_digit()) {
            return Err(invalid("expected decimal digits only".to_owned()));
        }
        let build: u32 = raw
            .parse()
            .map_err(|_| invalid("out of range".to_owned()))?;
        if !(MIN_GAME_BUILD..=MAX_GAME_BUILD).contains(&build) {
            return Err(invalid(format!(
                "expected {MIN_GAME_BUILD}..={MAX_GAME_BUILD}"
            )));
        }
        Ok(Some(build))
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        self.game_build().map(|_| ())
    }
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
    /// Idle timeout applied to each streamed resource-file read.
    #[serde(default = "default_file_download_timeout_secs")]
    pub file_download_timeout_secs: u64,
    /// Disk read size for streamed resource responses.
    #[serde(default = "default_file_download_chunk_bytes")]
    pub file_download_chunk_bytes: usize,
    /// Maximum number of concurrent resource-file streams.
    #[serde(default = "default_file_download_concurrency")]
    pub file_download_concurrency: usize,
    /// Where the persistent resource KVP lives.
    ///
    /// Scripts treat `SetResourceKvp` as durable storage, so this file holds
    /// real player data: back it up with the rest of the server state.
    #[serde(default = "default_kvp_path")]
    pub kvp_path: PathBuf,
    /// How often a KVP store with pending `_NO_SYNC` writes is flushed.
    ///
    /// Bounds what a crash can cost: at most this much deferred data, never
    /// the whole session.
    #[serde(default = "default_kvp_flush_interval_secs")]
    pub kvp_flush_interval_secs: u64,
    /// Deadline for one outbound `PerformHttpRequest`.
    ///
    /// A resource's callback fires with an error once it expires, so a dead
    /// endpoint costs a slot for this long rather than forever.
    #[serde(default = "default_http_request_timeout_secs")]
    pub http_request_timeout_secs: u64,
    /// How many outbound requests may be in flight at once. Beyond this they
    /// queue; the queue itself is bounded inside the bridge.
    #[serde(default = "default_http_concurrency")]
    pub http_concurrency: usize,
    /// Cap on an outbound response body. A larger response is refused rather
    /// than buffered, because the whole body crosses into a V8 isolate.
    #[serde(default = "default_http_response_max_bytes")]
    pub http_response_max_bytes: usize,
    /// Deadline for a resource's `SetHttpHandler` callback to answer.
    #[serde(default = "default_http_handler_timeout_secs")]
    pub http_handler_timeout_secs: u64,
    /// Cap on an inbound request body handed to a resource handler.
    #[serde(default = "default_http_request_max_bytes")]
    pub http_request_max_bytes: usize,
}

fn default_http_request_timeout_secs() -> u64 {
    30
}

fn default_http_concurrency() -> usize {
    32
}

fn default_http_response_max_bytes() -> usize {
    5 * 1024 * 1024
}

fn default_http_handler_timeout_secs() -> u64 {
    15
}

fn default_http_request_max_bytes() -> usize {
    1024 * 1024
}

fn default_kvp_path() -> PathBuf {
    PathBuf::from("baston-kvp.json")
}

fn default_kvp_flush_interval_secs() -> u64 {
    30
}

impl ResourcesConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.file_download_timeout_secs == 0 {
            return Err(ConfigError::Invalid {
                section: "resources",
                reason: "file_download_timeout_secs must be greater than zero".to_owned(),
            });
        }
        if !(4 * 1024..=4 * 1024 * 1024).contains(&self.file_download_chunk_bytes) {
            return Err(ConfigError::Invalid {
                section: "resources",
                reason: "file_download_chunk_bytes must be between 4096 and 4194304 bytes"
                    .to_owned(),
            });
        }
        if self.file_download_concurrency == 0 {
            return Err(ConfigError::Invalid {
                section: "resources",
                reason: "file_download_concurrency must be greater than zero".to_owned(),
            });
        }
        // Zero here would mean "give up before starting"; a resource would see
        // every request fail with a timeout it never asked for.
        if self.http_request_timeout_secs == 0 || self.http_handler_timeout_secs == 0 {
            return Err(ConfigError::Invalid {
                section: "resources",
                reason: "http_request_timeout_secs and http_handler_timeout_secs must be greater \
                         than zero"
                    .to_owned(),
            });
        }
        if self.http_concurrency == 0 {
            return Err(ConfigError::Invalid {
                section: "resources",
                reason: "http_concurrency must be greater than zero".to_owned(),
            });
        }
        if self.http_response_max_bytes == 0 || self.http_request_max_bytes == 0 {
            return Err(ConfigError::Invalid {
                section: "resources",
                reason: "http_response_max_bytes and http_request_max_bytes must be greater than \
                         zero"
                    .to_owned(),
            });
        }
        Ok(())
    }
}

/// `[connection]` section.
#[derive(Debug, Clone, Deserialize)]
pub struct ConnectionConfig {
    /// Seconds to wait for `playerConnecting` deferrals before kicking.
    #[serde(default = "default_deferral_timeout")]
    pub deferral_timeout_secs: u64,
}

/// Who may turn the `displayinfo` overlay on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DisplayInfoAccess {
    /// The overlay does not exist: the builtin resource is not advertised to
    /// any client, and toggle requests are refused.
    #[default]
    Off,
    /// Only players whose identifiers appear in `[debug].allow`.
    Allowlist,
    /// Anyone connected. The snapshot exposes zone topology and per-player
    /// network stats, so this is a development setting.
    Everyone,
}

impl DisplayInfoAccess {
    pub fn is_enabled(self) -> bool {
        !matches!(self, Self::Off)
    }
}

/// `[debug]` section — the server-assembled `displayinfo` overlay.
#[derive(Debug, Clone, Deserialize)]
pub struct DebugConfig {
    #[serde(default)]
    pub display_info: DisplayInfoAccess,
    /// Identifiers cleared for the overlay when `display_info = "allowlist"`,
    /// in `GetPlayerIdentifiers` form (`license:abc…`, `steam:110000…`).
    /// Matching is exact and case-insensitive.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Snapshots per second, per subscriber. Each one is a reliable client
    /// event carrying the mesh topology, so this is deliberately far below
    /// the sync tick.
    #[serde(default = "default_display_info_hz")]
    pub refresh_hz: u32,
}

impl Default for DebugConfig {
    fn default() -> Self {
        Self {
            display_info: DisplayInfoAccess::default(),
            allow: Vec::new(),
            refresh_hz: default_display_info_hz(),
        }
    }
}

impl DebugConfig {
    /// Whether `source`'s identifiers clear it for the overlay.
    pub fn allows(&self, identifiers: &[String]) -> bool {
        match self.display_info {
            DisplayInfoAccess::Off => false,
            DisplayInfoAccess::Everyone => true,
            DisplayInfoAccess::Allowlist => identifiers.iter().any(|id| {
                self.allow
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(id.trim()))
            }),
        }
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if !(1..=30).contains(&self.refresh_hz) {
            return Err(ConfigError::Invalid {
                section: "debug",
                reason: format!(
                    "refresh_hz must be between 1 and 30, got {}",
                    self.refresh_hz
                ),
            });
        }
        // An allowlist mode with nothing in it is almost always a half-finished
        // edit, and it fails silently: the overlay simply never appears.
        if self.display_info == DisplayInfoAccess::Allowlist && self.allow.is_empty() {
            return Err(ConfigError::Invalid {
                section: "debug",
                reason: "display_info = \"allowlist\" but allow is empty — \
                         list the identifiers cleared for the overlay, or use \"off\""
                    .to_owned(),
            });
        }
        Ok(())
    }
}

fn default_display_info_hz() -> u32 {
    5
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
fn default_bind_address() -> IpAddr {
    IpAddr::V4(Ipv4Addr::UNSPECIFIED)
}
fn default_max_players() -> u32 {
    32
}
fn default_enforce_game_build() -> String {
    DEFAULT_GAME_BUILD.to_string()
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
fn default_tick_min_hz() -> u16 {
    20
}
fn default_tick_default_hz() -> u16 {
    60
}
fn default_tick_max_hz() -> u16 {
    120
}
fn default_tick_high_utilization() -> f32 {
    0.85
}
fn default_tick_low_utilization() -> f32 {
    0.50
}
fn default_tick_recovery_window() -> u32 {
    180
}
fn default_tick_overload_backoff() -> f32 {
    0.5
}
fn default_interest_budget_bytes() -> usize {
    24 * 1024
}
fn default_interest_remove_budget_bytes() -> usize {
    4 * 1024
}
fn default_interest_distance_weight() -> f32 {
    10.0
}
fn default_interest_closing_weight() -> f32 {
    0.5
}
fn default_interest_staleness_weight() -> f32 {
    1.0
}
fn default_interest_hysteresis_m() -> f32 {
    20.0
}
fn default_file_download_timeout_secs() -> u64 {
    30
}
fn default_file_download_chunk_bytes() -> usize {
    64 * 1024
}
fn default_file_download_concurrency() -> usize {
    64
}
fn default_db_pool_size() -> u32 {
    // Enough for a busy resource set without exhausting a small managed
    // database's connection budget.
    10
}
fn default_db_query_timeout() -> u64 {
    15
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
            map_file: None,
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
            onesync: OneSyncMode::default(),
            adaptive_tick_enabled: true,
            tick_min_hz: default_tick_min_hz(),
            tick_default_hz: default_tick_default_hz(),
            tick_max_hz: default_tick_max_hz(),
            tick_high_utilization: default_tick_high_utilization(),
            tick_low_utilization: default_tick_low_utilization(),
            tick_recovery_window: default_tick_recovery_window(),
            tick_overload_backoff: default_tick_overload_backoff(),
            interest_budget_bytes: default_interest_budget_bytes(),
            interest_remove_budget_bytes: default_interest_remove_budget_bytes(),
            interest_distance_weight: default_interest_distance_weight(),
            interest_closing_weight: default_interest_closing_weight(),
            interest_staleness_weight: default_interest_staleness_weight(),
            interest_hysteresis_m: default_interest_hysteresis_m(),
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
            file_download_timeout_secs: default_file_download_timeout_secs(),
            file_download_chunk_bytes: default_file_download_chunk_bytes(),
            file_download_concurrency: default_file_download_concurrency(),
            kvp_path: default_kvp_path(),
            kvp_flush_interval_secs: default_kvp_flush_interval_secs(),
            http_request_timeout_secs: default_http_request_timeout_secs(),
            http_concurrency: default_http_concurrency(),
            http_response_max_bytes: default_http_response_max_bytes(),
            http_handler_timeout_secs: default_http_handler_timeout_secs(),
            http_request_max_bytes: default_http_request_max_bytes(),
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
    /// Absolute path of `meshing.map_file`, resolved against the directory of
    /// the configuration file it was read from.
    ///
    /// Relative to the config rather than to the working directory: a mounted
    /// `config/` then works without anyone having to know where the process
    /// was launched from.
    pub fn map_file_path(&self) -> Option<PathBuf> {
        let map = self.meshing.map_file.as_ref()?;
        let path = PathBuf::from(map);
        if path.is_absolute() {
            return Some(path);
        }
        Some(match self.config_dir.as_ref() {
            Some(dir) => dir.join(path),
            None => path,
        })
    }

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
        config.config_dir = path.parent().map(Path::to_path_buf);
        config.apply_env_overrides()?;
        config.resolve_modules(&raw)?;
        config.validate()?;
        Ok(config)
    }

    /// Where `load` looks when `BASTON_CONFIG` is not set, in order.
    ///
    /// `config/baston.toml` is where the repository keeps it; a bare
    /// `baston.toml` next to the binary is what a deployed server usually has.
    /// Both work, so neither layout has to know about the other.
    pub const SEARCH_PATHS: &'static [&'static str] = &["baston.toml", "config/baston.toml"];

    /// The configuration file to load: `BASTON_CONFIG` if set, else the first
    /// of [`Self::SEARCH_PATHS`] that exists.
    ///
    /// Returns the last candidate when none exist, so the caller's error names
    /// a concrete path instead of reporting that nothing was found anywhere.
    pub fn discover() -> PathBuf {
        if let Ok(path) = std::env::var("BASTON_CONFIG") {
            return PathBuf::from(path);
        }
        Self::SEARCH_PATHS
            .iter()
            .map(PathBuf::from)
            .find(|path| path.is_file())
            .unwrap_or_else(|| PathBuf::from(Self::SEARCH_PATHS[Self::SEARCH_PATHS.len() - 1]))
    }

    /// Parse a configuration document, including module resolution.
    ///
    /// `load` reads a file; this is the same pipeline over an in-memory
    /// document, so tests exercise module resolution instead of the empty set
    /// a bare `toml::from_str` leaves behind.
    pub fn parse(raw: &str) -> Result<Self, ConfigError> {
        let mut config: Self = toml::from_str(raw).map_err(|source| ConfigError::Parse {
            path: PathBuf::from("<memory>"),
            source,
        })?;
        config.apply_env_overrides()?;
        config.resolve_modules(raw)?;
        config.validate()?;
        Ok(config)
    }

    /// Resolve `[modules]` against the legacy section flags and the
    /// environment, then record which configured sections are inert.
    ///
    /// Runs before [`Self::validate`] so section validators can assume the
    /// module set is known — a section belonging to a disabled module is not
    /// held to the invariants that only matter when it runs.
    pub fn resolve_modules(&mut self, raw: &str) -> Result<(), ConfigError> {
        self.enabled_modules = self.modules.resolve(LegacyToggles::from_toml(raw))?;
        self.inert_sections = modules::inert_sections(self.enabled_modules, raw);
        Ok(())
    }

    /// Whether a module runs in this process.
    pub fn module_enabled(&self, module: ModuleId) -> bool {
        self.enabled_modules.is_enabled(module)
    }

    /// Validate cross-section invariants after env overrides. Kept in `load`
    /// so every entry point fails fast at startup with the section-specific,
    /// actionable error messages — callers must not have to remember to call
    /// the per-section validators themselves.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.server.validate()?;
        self.license.validate()?;
        self.api.validate()?;
        self.state_sync.validate()?;
        self.resources.validate()?;
        self.debug.validate()?;
        // Only held to its invariants when it actually runs: a `[db]` block
        // left in a config with the module off is not a reason to refuse boot.
        if self.enabled_modules.is_enabled(ModuleId::Db) {
            self.db.validate()?;
        }
        if self.listing.enabled {
            // Being listed and being slot-checked are the same bargain: the
            // heartbeat needs a credential only `cfx` produces, and the client
            // only checks entitlements when it finds the token that comes with
            // it. Allowing a listing without one would be the whole point of
            // the licence, skipped.
            if !self.license.mode.authenticates() {
                return Err(ConfigError::Invalid {
                    section: "listing",
                    reason: "listing requires [license] mode = \"cfx\"; a server cannot be \
                             listed without an authenticated CFX identity"
                        .to_owned(),
                });
            }
            let ip = self
                .listing
                .ip_override
                .ok_or_else(|| ConfigError::Invalid {
                    section: "listing",
                    reason: "listing requires ip_override, the public address players connect to"
                        .to_owned(),
                })?;
            if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() {
                return Err(ConfigError::Invalid {
                    section: "listing",
                    reason: format!(
                        "ip_override ({ip}) must be a concrete public address, not a wildcard, \
                         loopback or multicast one"
                    ),
                });
            }
        }
        if self.voice.enabled && self.voice.port == self.server.port {
            return Err(ConfigError::Invalid {
                section: "voice",
                reason: format!(
                    "voice.port ({}) must differ from server.port (the game transport owns it)",
                    self.voice.port
                ),
            });
        }
        Ok(())
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
        if let Ok(map) = std::env::var("BASTON_MAP_FILE") {
            self.meshing.map_file = Some(map);
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
        if let Ok(enabled) = std::env::var("BASTON_VOICE_ENABLED") {
            self.voice.enabled = matches!(enabled.as_str(), "true" | "1" | "yes");
        }
        if let Ok(port) = std::env::var("BASTON_VOICE_PORT") {
            self.voice.port =
                port.parse()
                    .map_err(|e: std::num::ParseIntError| ConfigError::EnvOverride {
                        var: "BASTON_VOICE_PORT",
                        value: port.clone(),
                        reason: e.to_string(),
                    })?;
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
mod tests;
