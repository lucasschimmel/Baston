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
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::error::EscrowPluginError;

/// How long to wait for the shim to publish its `ready.json` on startup. Covers a
/// cold FXServer boot plus the component's first licence round-trip.
const READY_TIMEOUT: Duration = Duration::from_secs(60);
/// Per-request reply budget (no deadlock on a frozen sidecar). Generous enough to
/// cover a server script being read and base64-encoded by the shim (streaming
/// assets are out of escrow scope, so payloads stay small).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
/// How often BASTON re-checks for the reply / for child death.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// The shim resource name (materialised on disk under the sidecar resources dir).
pub const SHIM_RESOURCE: &str = "baston-cfx-shim";

/// The shim source, embedded so the Lua stays in lockstep with this transport.
const SHIM_MANIFEST: &str = include_str!("../../../resources/baston-cfx-shim/fxmanifest.lua");
const SHIM_SERVER: &str = include_str!("../../../resources/baston-cfx-shim/server.lua");

/// Parameters to launch a real FXServer sidecar.
#[derive(Debug, Clone)]
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
    /// (the IPC is file-drop), it only satisfies FXServer's listener.
    pub port: u16,
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
    cfg_path: Option<PathBuf>,
}

impl Sidecar {
    /// Start a real FXServer sidecar for production use.
    ///
    /// Materialises the shim under `resources_dir`, writes a private launch config
    /// (carrying `sv_licenseKey` off the command line), starts FXServer off the
    /// public server list (`sv_master1 ""`, never `sv_lan` — LAN mode would
    /// suppress the very licence validation we depend on), and waits for the shim
    /// to report ready.
    pub fn start(params: &SidecarParams) -> Result<Arc<Self>, EscrowPluginError> {
        let shim_dir = params.resources_dir.join(SHIM_RESOURCE);
        let ipc_dir = shim_dir.join("ipc");
        materialise_shim(&shim_dir, &ipc_dir)?;
        reset_ipc(&ipc_dir);

        // The launch config carries the licence key, so it lives OUTSIDE the
        // resources tree (never in a resource dir a caller might commit) and is
        // removed on drop.
        let cfg_dir = std::env::temp_dir().join(format!("baston-sidecar-{}", params.port));
        fs::create_dir_all(&cfg_dir)
            .map_err(|e| EscrowPluginError::SidecarSpawn(format!("creating cfg dir: {e}")))?;
        let cfg_path = cfg_dir.join("sidecar.cfg");
        fs::write(&cfg_path, render_sidecar_cfg(params))
            .map_err(|e| EscrowPluginError::SidecarSpawn(format!("writing sidecar.cfg: {e}")))?;

        let mut cmd = Command::new(&params.fxserver_path);
        cmd.arg("+set")
            .arg("resources_path")
            .arg(&params.resources_dir)
            .arg("+exec")
            .arg(&cfg_path)
            .arg("+ensure")
            .arg(SHIM_RESOURCE);
        Self::launch(cmd, ipc_dir, Some(cfg_path))
    }

    /// Spawn around an arbitrary command implementing the file-drop protocol
    /// against `ipc_dir`. Exposed so tests can drive it with a lightweight stub
    /// process instead of a full FXServer install.
    pub fn spawn_with_command(
        cmd: Command,
        ipc_dir: PathBuf,
    ) -> Result<Arc<Self>, EscrowPluginError> {
        fs::create_dir_all(&ipc_dir)
            .map_err(|e| EscrowPluginError::SidecarSpawn(format!("creating ipc dir: {e}")))?;
        reset_ipc(&ipc_dir);
        Self::launch(cmd, ipc_dir, None)
    }

    fn launch(
        mut cmd: Command,
        ipc_dir: PathBuf,
        cfg_path: Option<PathBuf>,
    ) -> Result<Arc<Self>, EscrowPluginError> {
        // We never read the child's console; keep BASTON's own output clean.
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = cmd
            .spawn()
            .map_err(|e| EscrowPluginError::SidecarSpawn(e.to_string()))?;

        let sidecar = Arc::new(Sidecar {
            child: Mutex::new(child),
            ipc_dir,
            seq: AtomicU64::new(0),
            call_lock: Mutex::new(()),
            cfg_path,
        });

        sidecar.await_ready()?;
        tracing::info!("baston CFX sidecar started (FXServer subprocess, file-drop IPC)");
        Ok(sidecar)
    }

    /// Block until the shim publishes `ready.json`, the child dies, or the budget
    /// is spent.
    fn await_ready(&self) -> Result<(), EscrowPluginError> {
        let ready = self.ipc_dir.join("ready.json");
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            if ready.exists() {
                return Ok(());
            }
            if let Some(status) = self.child_exit_status() {
                return Err(EscrowPluginError::SidecarDied(format!(
                    "sidecar exited during startup ({status})"
                )));
            }
            if Instant::now() >= deadline {
                return Err(EscrowPluginError::SidecarStartTimeout);
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Send one request object and wait (bounded) for the matching JSON reply.
    /// A monotonic `id` is injected and echoed back by the shim, so a stale or
    /// partially-written reply file is ignored rather than mis-read.
    pub fn request(
        &self,
        mut op: serde_json::Value,
    ) -> Result<serde_json::Value, EscrowPluginError> {
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
            .map_err(|e| EscrowPluginError::SidecarIo(format!("writing request: {e}")))?;

        let deadline = Instant::now() + REQUEST_TIMEOUT;
        loop {
            std::thread::sleep(POLL_INTERVAL);

            if let Some(status) = self.child_exit_status() {
                return Err(EscrowPluginError::SidecarDied(status.to_string()));
            }

            // A partial read / stale file just fails to match and we retry.
            // Accept the id whether the shim encoded it as an integer or a float.
            if let Ok(bytes) = fs::read(&response_path) {
                if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                    let matches = value.get("id").is_some_and(|v| {
                        v.as_u64() == Some(id) || v.as_f64() == Some(id as f64)
                    });
                    if matches {
                        return Ok(value);
                    }
                }
            }

            if Instant::now() >= deadline {
                return Err(EscrowPluginError::SidecarRequestTimeout);
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
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
        // Best-effort: don't leave a stale request/response for a future run…
        reset_ipc(&self.ipc_dir);
        // …and never leave the licence-bearing launch config on disk.
        if let Some(cfg) = &self.cfg_path {
            let _ = fs::remove_file(cfg);
        }
    }
}

/// Write the shim source to disk (idempotent) and ensure the ipc dir exists.
fn materialise_shim(shim_dir: &Path, ipc_dir: &Path) -> Result<(), EscrowPluginError> {
    fs::create_dir_all(ipc_dir)
        .map_err(|e| EscrowPluginError::SidecarSpawn(format!("creating shim dir: {e}")))?;
    write_if_changed(&shim_dir.join("fxmanifest.lua"), SHIM_MANIFEST)?;
    write_if_changed(&shim_dir.join("server.lua"), SHIM_SERVER)?;
    Ok(())
}

/// Avoid rewriting (and thus needlessly touching) an already-current shim file.
fn write_if_changed(path: &Path, contents: &str) -> Result<(), EscrowPluginError> {
    let current = fs::read_to_string(path).ok();
    if current.as_deref() != Some(contents) {
        fs::write(path, contents)
            .map_err(|e| EscrowPluginError::SidecarSpawn(format!("writing {path:?}: {e}")))?;
    }
    Ok(())
}

/// Remove leftover IPC state so a fresh run never observes a previous one's files.
fn reset_ipc(ipc_dir: &Path) {
    for name in ["request.json", "request.json.tmp", "response.json", "ready.json"] {
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

/// Render the private launch config. `sv_licenseKey` lives here (never on the
/// command line, never logged). `sv_master1 ""` keeps the sidecar off the public
/// server list while leaving licence validation active.
fn render_sidecar_cfg(params: &SidecarParams) -> String {
    let mut cfg = String::new();
    if let Some(key) = params.license_key.as_deref() {
        // Keys are `cfxk_…` (alphanumeric + underscore); no quote injection risk.
        cfg.push_str(&format!("sv_licenseKey \"{key}\"\n"));
    }
    cfg.push_str("sv_maxclients 1\n");
    cfg.push_str("sv_master1 \"\"\n");
    cfg.push_str("sv_endpointprivacy true\n");
    cfg.push_str("onesync off\n");
    cfg.push_str("sv_scriptHookAllowed false\n");
    cfg.push_str(&format!("endpoint_add_tcp \"127.0.0.1:{}\"\n", params.port));
    cfg
}
