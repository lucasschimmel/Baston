//! Shared FXServer sidecar subprocess — the single channel to the genuine,
//! **unmodified** CFX component (`svadhesive`) running in its native host.
//!
//! ## Why a subprocess, and why file-drop IPC
//!
//! `svadhesive.dll` cannot be called over FFI (it exposes only an opaque
//! CitizenFX component, a single `CreateComponent` export). So we run a minimal
//! **FXServer subprocess** that loads the component normally, plus a tiny Lua
//! shim (`baston-cfx-shim`) that answers BASTON.
//!
//! The transport is a **file-drop** channel, not stdin/stdout: the CitizenFX
//! server Lua sandbox does not expose `io.read` (engine-source
//! `citizen-scripting-lua/src/LuaIO.cpp` — `read` is absent from `iolib`, `stdin`
//! is an empty stub), so a line protocol on the process pipes cannot work.
//! Instead:
//!   - BASTON writes `<ipc>/request.json` **atomically** (temp + rename), one
//!     request at a time (serialised by [`Sidecar::call_lock`]);
//!   - the shim reads it, processes it, and writes `<ipc>/response.json`;
//!   - each request carries a monotonic `id`; BASTON accepts only the response
//!     whose `id` matches, so a stale or half-written file is ignored and the
//!     poll simply retries.
//!
//! Every request is bounded by [`REQUEST_TIMEOUT`] and each poll re-checks that
//! the child is still alive, so a frozen or dead sidecar surfaces as a clean
//! `Err`, never a hang. One sidecar process backs BOTH capabilities
//! ([`crate::LicenseOracle`] and [`crate::SidecarDecryptor`]) so we never boot a
//! second FXServer, and none of this ever sits on BASTON's per-frame hot path —
//! it runs at boot (licence gate) and at resource load (escrow decrypt) only.

use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::Write;
use std::net::{IpAddr, TcpListener, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::error::CfxPlatformError;

/// How long to wait for the shim to publish its `ready.json` on startup. Covers a
/// cold FXServer boot plus the component's first licence round-trip.
const READY_TIMEOUT: Duration = Duration::from_secs(60);
/// Per-request reply budget (no deadlock on a frozen sidecar). Generous enough to
/// cover a server script being read and base64-encoded by the shim (streaming
/// assets are out of escrow scope, so payloads stay small).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
/// How often BASTON re-checks for the reply / for child death.
const POLL_INTERVAL: Duration = Duration::from_millis(10);
static SIDECAR_INSTANCE: AtomicU64 = AtomicU64::new(0);

/// The shim resource name (materialised on disk under the sidecar resources dir).
pub const SHIM_RESOURCE: &str = "baston-cfx-shim";

/// The shim source, embedded so the Lua stays in lockstep with this transport.
const SHIM_MANIFEST: &str = include_str!("../assets/baston-cfx-shim/fxmanifest.lua");
const SHIM_SERVER: &str = include_str!("../assets/baston-cfx-shim/server.lua");

/// Public-list metadata owned and transmitted by the official FXServer broker.
#[derive(Debug, Clone)]
pub struct PublicListing {
    /// Public address registered with the CFX server list.
    pub public_ip: IpAddr,
    /// Public TCP and UDP game port.
    pub public_port: u16,
    /// Server name advertised by the official broker.
    pub hostname: String,
    /// Already-enforced client capacity advertised by the official broker.
    pub max_clients: u32,
    /// Whether the official broker should advertise OneSync.
    pub onesync: bool,
}

/// Parameters to launch a real FXServer sidecar.
#[derive(Clone)]
pub struct SidecarParams {
    /// Path to the operator's official `FXServer.exe`.
    pub fxserver_path: PathBuf,
    /// Directory FXServer scans for resources. For escrow this is the operator's
    /// resources dir (so the escrowed resources are reachable); for licence-only
    /// it can be a minimal dir holding just the shim, for a fast boot.
    pub resources_dir: PathBuf,
    /// CFX server licence key (`cfxk_…`) fed to the component as `sv_licenseKey`.
    /// `None` starts the sidecar without one — the component then validates
    /// nothing and cannot derive escrow keys (a caller-level warning case).
    pub license_key: Option<String>,
    /// Private, localhost-only TCP port for the sidecar's endpoint. Kept distinct
    /// from BASTON's public port so the two never clash; nothing connects to it
    /// (the IPC is file-drop), it only satisfies FXServer's listener. `0`
    /// selects an ephemeral loopback port for a non-listing escrow broker.
    pub port: u16,
    /// When present, register and heartbeat the Baston endpoint publicly.
    pub public_listing: Option<PublicListing>,
}

impl fmt::Debug for SidecarParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SidecarParams")
            .field("fxserver_path", &self.fxserver_path)
            .field("resources_dir", &self.resources_dir)
            .field(
                "license_key",
                &self.license_key.as_ref().map(|_| "[REDACTED]"),
            )
            .field("port", &self.port)
            .field("public_listing", &self.public_listing)
            .finish()
    }
}

struct SensitiveConfigFile {
    path: Option<tempfile::TempPath>,
}

impl SensitiveConfigFile {
    fn create(contents: &str) -> Result<Self, CfxPlatformError> {
        let mut file = tempfile::Builder::new()
            .prefix("baston-sidecar-")
            .suffix(".cfg")
            .tempfile()
            .map_err(|e| {
                CfxPlatformError::SidecarSpawn(format!("creating private sidecar config: {e}"))
            })?;
        file.write_all(contents.as_bytes()).map_err(|e| {
            CfxPlatformError::SidecarSpawn(format!("writing private sidecar config: {e}"))
        })?;
        file.flush().map_err(|e| {
            CfxPlatformError::SidecarSpawn(format!("flushing private sidecar config: {e}"))
        })?;
        Ok(Self {
            path: Some(file.into_temp_path()),
        })
    }

    fn path(&self) -> &Path {
        self.path
            .as_ref()
            .map(tempfile::TempPath::as_ref)
            .expect("config path is present")
    }

    fn keep(mut self) -> tempfile::TempPath {
        self.path.take().expect("config path is present")
    }
}

/// A running FXServer sidecar. Obtained as an `Arc<Sidecar>` so escrow and licence
/// can share one process; the child is killed when the last `Arc` drops.
pub struct Sidecar {
    child: Mutex<Child>,
    ipc_dir: PathBuf,
    seq: AtomicU64,
    /// Serialises requests: at most one is ever in flight, which is what lets the
    /// file-drop channel use a single request/response file pair safely.
    call_lock: Mutex<()>,
    /// The generated launch config (carries `sv_licenseKey`). Deleted on drop so
    /// the key never lingers on disk after shutdown. `None` for test spawns.
    _cfg_path: Option<tempfile::TempPath>,
    /// Per-process shim materialised by production starts. Its unique name
    /// isolates file-drop IPC from every colocated broker.
    shim_dir: Option<PathBuf>,
}

impl Sidecar {
    /// Start a real FXServer sidecar for production use.
    ///
    /// Materialises the shim under `resources_dir`, writes a private launch config
    /// (carrying `sv_licenseKey` off the command line), starts FXServer off the
    /// public server list (`sv_master1 ""`, never `sv_lan` — LAN mode would
    /// suppress the very licence validation we depend on), and waits for the shim
    /// to report ready.
    ///
    /// # Errors
    ///
    /// Returns an error when configuration materialization, process startup,
    /// readiness, or config validation fails.
    pub fn start(params: &SidecarParams) -> Result<Arc<Self>, CfxPlatformError> {
        Self::start_with_cancellation(params, &AtomicBool::new(false))
    }

    /// Start a broker that aborts promptly when `cancelled` becomes true.
    ///
    /// # Errors
    ///
    /// Returns [`CfxPlatformError::SidecarCancelled`] on cancellation, or the
    /// same startup errors as [`Sidecar::start`].
    pub fn start_with_cancellation(
        params: &SidecarParams,
        cancelled: &AtomicBool,
    ) -> Result<Arc<Self>, CfxPlatformError> {
        let mut effective_params = params.clone();
        if effective_params.port == 0 {
            effective_params.port = available_loopback_port()?;
        }
        let params = &effective_params;
        let shim_resource = unique_shim_resource_name();
        let shim_dir = params.resources_dir.join(&shim_resource);
        let ipc_dir = shim_dir.join("ipc");
        materialise_shim(&shim_dir, &ipc_dir)?;
        reset_ipc(&ipc_dir);

        // The launch config carries the licence key, so it lives OUTSIDE the
        // resources tree (never in a resource dir a caller might commit) and is
        // removed on drop.
        let config = render_sidecar_cfg(params)?;
        let cfg_file = SensitiveConfigFile::create(&config)?;

        let mut cmd = Command::new(&params.fxserver_path);
        if let Some(dir) = params.fxserver_path.parent() {
            cmd.current_dir(dir);
        }
        cmd.arg("+set")
            .arg("resources_path")
            .arg(&params.resources_dir)
            .arg("+exec")
            .arg(cfg_file.path())
            .arg("+ensure")
            .arg(&shim_resource);
        Self::launch(cmd, ipc_dir, Some(cfg_file), Some(shim_dir), cancelled)
    }

    /// Spawn around an arbitrary command implementing the file-drop protocol
    /// against `ipc_dir`. Exposed so tests can drive it with a lightweight stub
    /// process instead of a full FXServer install.
    pub fn spawn_with_command(
        cmd: Command,
        ipc_dir: PathBuf,
    ) -> Result<Arc<Self>, CfxPlatformError> {
        fs::create_dir_all(&ipc_dir)
            .map_err(|e| CfxPlatformError::SidecarSpawn(format!("creating ipc dir: {e}")))?;
        reset_ipc(&ipc_dir);
        Self::launch(cmd, ipc_dir, None, None, &AtomicBool::new(false))
    }

    fn launch(
        mut cmd: Command,
        ipc_dir: PathBuf,
        cfg_file: Option<SensitiveConfigFile>,
        shim_dir: Option<PathBuf>,
        cancelled: &AtomicBool,
    ) -> Result<Arc<Self>, CfxPlatformError> {
        // We never read the child's console; keep BASTON's own output clean.
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = match cmd.spawn() {
            Ok(child) => child,
            Err(error) => {
                if let Some(shim_dir) = &shim_dir {
                    remove_owned_shim_dir(shim_dir);
                }
                return Err(CfxPlatformError::SidecarSpawn(error.to_string()));
            }
        };
        let cfg_path = cfg_file.map(SensitiveConfigFile::keep);

        let sidecar = Arc::new(Sidecar {
            child: Mutex::new(child),
            ipc_dir,
            seq: AtomicU64::new(0),
            call_lock: Mutex::new(()),
            _cfg_path: cfg_path,
            shim_dir,
        });

        sidecar.await_ready(cancelled)?;
        tracing::info!("baston CFX sidecar started (FXServer subprocess, file-drop IPC)");
        Ok(sidecar)
    }

    /// Block until the shim publishes `ready.json`, the child dies, or the budget
    /// is spent.
    fn await_ready(&self, cancelled: &AtomicBool) -> Result<(), CfxPlatformError> {
        let ready = self.ipc_dir.join("ready.json");
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            if cancelled.load(Ordering::Acquire) {
                return Err(CfxPlatformError::SidecarCancelled);
            }
            if ready.exists() {
                return Ok(());
            }
            if let Some(status) = self.child_exit_status() {
                return Err(CfxPlatformError::SidecarDied(format!(
                    "sidecar exited during startup ({status})"
                )));
            }
            if Instant::now() >= deadline {
                return Err(CfxPlatformError::SidecarStartTimeout);
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Send one request object and wait (bounded) for the matching JSON reply.
    /// A monotonic `id` is injected and echoed back by the shim, so a stale or
    /// partially-written reply file is ignored rather than mis-read.
    pub fn request(&self, op: serde_json::Value) -> Result<serde_json::Value, CfxPlatformError> {
        self.request_with_cancellation(op, &AtomicBool::new(false))
    }

    /// Send one request, aborting promptly when `cancelled` becomes true.
    ///
    /// # Errors
    ///
    /// Returns an error on cancellation, broker death, timeout, or IPC failure.
    pub fn request_with_cancellation(
        &self,
        mut op: serde_json::Value,
        cancelled: &AtomicBool,
    ) -> Result<serde_json::Value, CfxPlatformError> {
        // Recover a poisoned lock rather than turning one panicking caller into a
        // cascade that kills every future request.
        let _guard = self.call_lock.lock().unwrap_or_else(|e| e.into_inner());

        let id = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        if let Some(map) = op.as_object_mut() {
            map.insert("id".into(), serde_json::Value::from(id));
        }
        let request_path = self.ipc_dir.join("request.json");
        let response_path = self.ipc_dir.join("response.json");
        write_atomic(&request_path, op.to_string().as_bytes())
            .map_err(|e| CfxPlatformError::SidecarIo(format!("writing request: {e}")))?;

        let deadline = Instant::now() + REQUEST_TIMEOUT;
        loop {
            std::thread::sleep(POLL_INTERVAL);

            if cancelled.load(Ordering::Acquire) {
                return Err(CfxPlatformError::SidecarCancelled);
            }
            if let Some(status) = self.child_exit_status() {
                return Err(CfxPlatformError::SidecarDied(status.to_string()));
            }

            // A partial read / stale file just fails to match and we retry.
            // Accept the id whether the shim encoded it as an integer or a float.
            if let Ok(bytes) = fs::read(&response_path) {
                if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                    let matches = value
                        .get("id")
                        .is_some_and(|v| v.as_u64() == Some(id) || v.as_f64() == Some(id as f64));
                    if matches {
                        if let Err(error) = fs::remove_file(&response_path) {
                            if error.kind() != std::io::ErrorKind::NotFound {
                                return Err(CfxPlatformError::SidecarIo(format!(
                                    "removing consumed response: {error}"
                                )));
                            }
                        }
                        return Ok(value);
                    }
                }
            }

            if Instant::now() >= deadline {
                return Err(CfxPlatformError::SidecarRequestTimeout);
            }
        }
    }

    /// `Some(status)` once the child has exited; `None` while it is still running
    /// (or if the child cannot be queried — treated the same as "still running",
    /// so a spurious query error never fabricates a "died" verdict).
    fn child_exit_status(&self) -> Option<std::process::ExitStatus> {
        let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        child.try_wait().unwrap_or_default()
    }

    /// Return the child exit status once the broker has stopped.
    #[must_use]
    pub fn exit_status(&self) -> Option<std::process::ExitStatus> {
        self.child_exit_status()
    }
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
        // Best-effort: don't leave a stale request/response for a future run…
        reset_ipc(&self.ipc_dir);
        // `TempPath` removes the licence-bearing launch config after this drop.
        if let Some(shim_dir) = &self.shim_dir {
            remove_owned_shim_dir(shim_dir);
        }
    }
}

fn remove_owned_shim_dir(shim_dir: &Path) {
    let owned_shim = shim_dir.file_name().is_some_and(|name| {
        name.to_string_lossy()
            .starts_with(&format!("{SHIM_RESOURCE}-"))
    });
    if owned_shim {
        let _ = fs::remove_dir_all(shim_dir);
    }
}

fn unique_shim_resource_name() -> String {
    let instance = SIDECAR_INSTANCE.fetch_add(1, Ordering::Relaxed) + 1;
    format!("{SHIM_RESOURCE}-{}-{instance}", std::process::id())
}

fn available_loopback_port() -> Result<u16, CfxPlatformError> {
    for _ in 0..32 {
        let tcp = TcpListener::bind(("127.0.0.1", 0)).map_err(|e| {
            CfxPlatformError::SidecarSpawn(format!("selecting loopback endpoint: {e}"))
        })?;
        let port = tcp
            .local_addr()
            .map_err(|e| CfxPlatformError::SidecarSpawn(format!("reading loopback endpoint: {e}")))?
            .port();
        if let Ok(udp) = UdpSocket::bind(("127.0.0.1", port)) {
            drop(udp);
            drop(tcp);
            return Ok(port);
        }
    }
    Err(CfxPlatformError::SidecarSpawn(
        "no shared TCP/UDP loopback port was available".to_owned(),
    ))
}

/// Write the shim source to disk (idempotent) and ensure the ipc dir exists.
fn materialise_shim(shim_dir: &Path, ipc_dir: &Path) -> Result<(), CfxPlatformError> {
    fs::create_dir_all(ipc_dir)
        .map_err(|e| CfxPlatformError::SidecarSpawn(format!("creating shim dir: {e}")))?;
    write_if_changed(&shim_dir.join("fxmanifest.lua"), SHIM_MANIFEST)?;
    write_if_changed(&shim_dir.join("server.lua"), SHIM_SERVER)?;
    Ok(())
}

/// Avoid rewriting (and thus needlessly touching) an already-current shim file.
fn write_if_changed(path: &Path, contents: &str) -> Result<(), CfxPlatformError> {
    let current = fs::read_to_string(path).ok();
    if current.as_deref() != Some(contents) {
        fs::write(path, contents)
            .map_err(|e| CfxPlatformError::SidecarSpawn(format!("writing {path:?}: {e}")))?;
    }
    Ok(())
}

/// Remove leftover IPC state so a fresh run never observes a previous one's files.
fn reset_ipc(ipc_dir: &Path) {
    for name in [
        "request.json",
        "request.json.tmp",
        "response.json",
        "ready.json",
    ] {
        let _ = fs::remove_file(ipc_dir.join(name));
    }
}

/// Atomically replace `path` with `data` (write temp, then rename). On Windows and
/// Unix a rename over an existing file is atomic, so a reader never sees a partial
/// request.
fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut tmp: OsString = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, data)?;
    fs::rename(&tmp, path)
}

/// Render the launch config. `sv_licenseKey` lives here (never on the command
/// line or in logs). Private mode suppresses the master heartbeat; public mode
/// leaves registration to the genuine FXServer component.
fn render_sidecar_cfg(params: &SidecarParams) -> Result<String, CfxPlatformError> {
    let mut cfg = String::new();
    if let Some(key) = params.license_key.as_deref() {
        validate_config_value("licence key", key)?;
        cfg.push_str(&format!("sv_licenseKey \"{key}\"\n"));
    }
    let endpoint_port = if let Some(listing) = &params.public_listing {
        validate_config_value("hostname", &listing.hostname)?;
        cfg.push_str(&format!("sv_maxclients {}\n", listing.max_clients));
        cfg.push_str(&format!("sv_listingIpOverride \"{}\"\n", listing.public_ip));
        cfg.push_str(&format!("sv_hostname \"{}\"\n", listing.hostname));
        cfg.push_str(if listing.onesync {
            "onesync on\n"
        } else {
            "onesync off\n"
        });
        listing.public_port
    } else {
        cfg.push_str("sv_maxclients 1\n");
        cfg.push_str("sv_master1 \"\"\n");
        cfg.push_str("onesync off\n");
        params.port
    };
    cfg.push_str("sv_endpointprivacy true\n");
    cfg.push_str("sv_scriptHookAllowed false\n");
    cfg.push_str(&format!("endpoint_add_tcp \"127.0.0.1:{endpoint_port}\"\n"));
    cfg.push_str(&format!("endpoint_add_udp \"127.0.0.1:{endpoint_port}\"\n"));
    Ok(cfg)
}

fn validate_config_value(field: &'static str, value: &str) -> Result<(), CfxPlatformError> {
    if value
        .chars()
        .any(|character| character == '"' || character.is_control())
    {
        return Err(CfxPlatformError::InvalidBrokerConfig(field));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(license_key: Option<&str>) -> SidecarParams {
        SidecarParams {
            fxserver_path: PathBuf::from("FXServer.exe"),
            resources_dir: PathBuf::from("resources"),
            license_key: license_key.map(str::to_owned),
            port: 30_120,
            public_listing: None,
        }
    }

    #[test]
    fn sidecar_params_debug_redacts_licence_key() {
        let params = params(Some("cfxk_private"));
        let debug = format!("{params:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("cfxk_private"));
    }

    #[test]
    fn launch_failure_removes_sensitive_config() {
        let temp = tempfile::tempdir().unwrap();
        let cfg_file = SensitiveConfigFile::create("sv_licenseKey \"cfxk_private\"").unwrap();
        let cfg_path = cfg_file.path().to_owned();
        let ipc_dir = temp.path().join("ipc");
        fs::create_dir_all(&ipc_dir).unwrap();

        let command = Command::new(temp.path().join("missing-fxserver.exe"));
        let result = Sidecar::launch(
            command,
            ipc_dir,
            Some(cfg_file),
            None,
            &AtomicBool::new(false),
        );

        assert!(result.is_err());
        assert!(!cfg_path.exists());
    }

    #[test]
    fn private_config_disables_public_master() {
        let config = render_sidecar_cfg(&params(Some("cfxk_private"))).unwrap();
        assert!(config.contains("sv_master1 \"\""));
        assert!(config.contains("sv_maxclients 1"));
        assert!(config.contains("endpoint_add_tcp \"127.0.0.1:30120\""));
        assert!(config.contains("endpoint_add_udp \"127.0.0.1:30120\""));
    }

    #[test]
    fn public_config_delegates_listing_to_official_fxserver() {
        let mut params = params(Some("cfxk_private"));
        params.public_listing = Some(PublicListing {
            public_ip: "203.0.113.10".parse().unwrap(),
            public_port: 30_120,
            hostname: "Baston Production".to_owned(),
            max_clients: 128,
            onesync: true,
        });

        let config = render_sidecar_cfg(&params).unwrap();

        assert!(!config.contains("sv_master1 \"\""));
        assert!(config.contains("sv_listingIpOverride \"203.0.113.10\""));
        assert!(config.contains("sv_hostname \"Baston Production\""));
        assert!(config.contains("sv_maxclients 128"));
        assert!(config.contains("onesync on"));
        assert!(config.contains("endpoint_add_tcp \"127.0.0.1:30120\""));
        assert!(config.contains("endpoint_add_udp \"127.0.0.1:30120\""));
    }

    #[test]
    fn config_injection_is_rejected_without_echoing_secret() {
        let mut params = params(Some("cfxk_private\"\nquit"));
        let error = render_sidecar_cfg(&params).unwrap_err();
        assert!(!error.to_string().contains("cfxk_private"));

        params.license_key = Some("cfxk_private".to_owned());
        params.public_listing = Some(PublicListing {
            public_ip: "203.0.113.10".parse().unwrap(),
            public_port: 30_120,
            hostname: "bad\"\nquit".to_owned(),
            max_clients: 48,
            onesync: false,
        });
        assert!(render_sidecar_cfg(&params).is_err());
    }

    #[test]
    fn an_ephemeral_private_endpoint_uses_a_real_tcp_and_udp_port() {
        assert_ne!(available_loopback_port().unwrap(), 0);
    }

    #[test]
    fn each_broker_gets_an_isolated_shim_resource() {
        let first = unique_shim_resource_name();
        let second = unique_shim_resource_name();
        assert_ne!(first, second);
        assert!(first.starts_with(&format!("{SHIM_RESOURCE}-")));
        assert!(second.starts_with(&format!("{SHIM_RESOURCE}-")));
    }
}
