//! Sidecar-subprocess decryptor (DB2b).
//!
//! `svadhesive.dll` cannot be called via FFI (it exposes only an opaque
//! CitizenFX component, see the phase D-bis impl notes). Instead we run a
//! minimal **FXServer subprocess** that loads escrow resources normally —
//! svadhesive decrypts them via its internal VFS hook — and a small Lua shim
//! streams the decrypted bytes back over a line-delimited JSON protocol on
//! stdin/stdout.
//!
//! The [`ScriptDecryptor::decrypt`] trait method is synchronous, so this module
//! uses blocking `std::process` pipes driven by a dedicated actor thread. The
//! caller never deadlocks: every request waits on a bounded `recv_timeout`, and
//! a frozen or dead child surfaces as a clean `Err`, never a panic.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use baston_core::script_decryptor::{
    is_cfx_encrypted, DecryptError, EntitlementContext, ScriptDecryptor,
};

use crate::error::EscrowPluginError;

/// How long to wait for the sidecar to print `READY` on startup.
const READY_TIMEOUT: Duration = Duration::from_secs(30);
/// Per-request decrypt timeout (mission requirement: no deadlock on freeze).
const DECRYPT_TIMEOUT: Duration = Duration::from_secs(5);

/// One decrypt job handed to the actor thread.
struct Job {
    request_line: String,
    reply: Sender<Result<Vec<u8>, EscrowPluginError>>,
}

/// A decryptor backed by an FXServer sidecar subprocess.
pub struct SidecarDecryptor {
    jobs: Mutex<Sender<Job>>,
    actor: Mutex<Option<JoinHandle<()>>>,
    /// Shared with the actor so `Drop` can kill the child even while the actor
    /// is blocked on a `read_line` — killing closes stdout, unblocking it.
    child: Arc<Mutex<Child>>,
}

impl SidecarDecryptor {
    /// Start a real FXServer sidecar for production use.
    ///
    /// Launches `fxserver_path` with `sv_lan true` / `sv_maxclients 0` and the
    /// `baston-decrypt-shim` resource pointed at `resources_dir`.
    pub fn start(
        fxserver_path: &Path,
        resources_dir: &Path,
    ) -> Result<Self, EscrowPluginError> {
        let mut cmd = Command::new(fxserver_path);
        cmd.arg("+set")
            .arg("sv_lan")
            .arg("true")
            .arg("+set")
            .arg("sv_maxclients")
            .arg("0")
            .arg("+set")
            .arg("resources_path")
            .arg(resources_dir)
            .arg("+ensure")
            .arg("baston-decrypt-shim");
        Self::spawn_with_command(cmd)
    }

    /// Spawn the actor around an arbitrary command implementing the sidecar
    /// protocol (`READY\n` then one JSON reply per JSON request line). Exposed
    /// so tests can drive it with a lightweight stub process instead of a full
    /// FXServer install.
    pub fn spawn_with_command(mut cmd: Command) -> Result<Self, EscrowPluginError> {
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = cmd
            .spawn()
            .map_err(|e| EscrowPluginError::SidecarSpawn(e.to_string()))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| EscrowPluginError::SidecarSpawn("no stdin pipe".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| EscrowPluginError::SidecarSpawn("no stdout pipe".into()))?;
        let stdout = BufReader::new(stdout);

        let child = Arc::new(Mutex::new(child));
        let (jobs_tx, jobs_rx) = mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), EscrowPluginError>>();

        let actor_child = Arc::clone(&child);
        let actor = std::thread::Builder::new()
            .name("escrow-sidecar".into())
            .spawn(move || actor_loop(actor_child, stdin, stdout, jobs_rx, ready_tx))
            .map_err(|e| EscrowPluginError::SidecarSpawn(e.to_string()))?;

        // Wait for the actor to confirm the sidecar printed READY.
        match ready_rx.recv_timeout(READY_TIMEOUT) {
            Ok(Ok(())) => {
                tracing::info!("baston-escrow sidecar started (FXServer subprocess)");
                Ok(SidecarDecryptor {
                    jobs: Mutex::new(jobs_tx),
                    actor: Mutex::new(Some(actor)),
                    child,
                })
            }
            Ok(Err(e)) => Err(e),
            Err(RecvTimeoutError::Timeout) => Err(EscrowPluginError::SidecarStartTimeout),
            Err(RecvTimeoutError::Disconnected) => {
                Err(EscrowPluginError::SidecarIo("actor exited during startup".into()))
            }
        }
    }

    /// Send one decrypt request to the actor and wait (bounded) for the reply.
    fn request(&self, resource: &str, file: &str, bytes: &[u8]) -> Result<Vec<u8>, EscrowPluginError> {
        let request_line = serde_json::json!({
            "resource": resource,
            "file": file,
            "data": B64.encode(bytes),
        })
        .to_string();

        let (reply_tx, reply_rx) = mpsc::channel();
        {
            let jobs = self.jobs.lock().expect("jobs sender mutex poisoned");
            jobs.send(Job {
                request_line,
                reply: reply_tx,
            })
            .map_err(|_| EscrowPluginError::SidecarDied("actor thread gone".into()))?;
        }

        match reply_rx.recv_timeout(DECRYPT_TIMEOUT) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => Err(EscrowPluginError::SidecarDecryptTimeout),
            Err(RecvTimeoutError::Disconnected) => {
                Err(EscrowPluginError::SidecarDied("actor dropped reply".into()))
            }
        }
    }
}

impl ScriptDecryptor for SidecarDecryptor {
    fn decrypt(
        &self,
        resource_name: &str,
        file_path: &str,
        bytes: &[u8],
        _entitlement: Option<&EntitlementContext>,
    ) -> Result<Vec<u8>, DecryptError> {
        // Plain files never touch the sidecar — zero overhead.
        if !is_cfx_encrypted(bytes) {
            return Ok(bytes.to_vec());
        }
        self.request(resource_name, file_path, bytes)
            .map_err(|e| DecryptError::DecryptionFailed {
                resource: resource_name.to_string(),
                file: file_path.to_string(),
                reason: e.to_string(),
            })
    }

    fn supports_encrypted(&self) -> bool {
        true
    }
}

impl Drop for SidecarDecryptor {
    fn drop(&mut self) {
        // Kill the child FIRST: this closes its stdout, which unblocks the
        // actor if it is parked in a blocking `read_line` (e.g. a frozen
        // sidecar). Only then can `join` return promptly.
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
        }
        // Drop the live sender so the actor's `jobs_rx.recv()` also observes
        // the hang-up if it happens to be idle.
        if let Ok(mut guard) = self.jobs.lock() {
            let (dead_tx, _) = mpsc::channel();
            *guard = dead_tx;
        }
        if let Ok(mut actor) = self.actor.lock() {
            if let Some(handle) = actor.take() {
                let _ = handle.join();
            }
        }
    }
}

/// Actor thread: drives the child, serializes requests, replies to each.
fn actor_loop(
    child: Arc<Mutex<Child>>,
    stdin: ChildStdin,
    mut stdout: BufReader<ChildStdout>,
    jobs_rx: mpsc::Receiver<Job>,
    ready_tx: Sender<Result<(), EscrowPluginError>>,
) {
    let kill_child = || {
        if let Ok(mut c) = child.lock() {
            let _ = c.kill();
            let _ = c.wait();
        }
    };
    let mut stdin = stdin;

    // Handshake: wait for the shim's READY line.
    let mut ready_line = String::new();
    match stdout.read_line(&mut ready_line) {
        Ok(0) => {
            let _ = ready_tx.send(Err(EscrowPluginError::SidecarIo(
                "sidecar closed stdout before READY".into(),
            )));
            kill_child();
            return;
        }
        Ok(_) => {
            if ready_line.trim().eq_ignore_ascii_case("READY") {
                let _ = ready_tx.send(Ok(()));
            } else {
                let _ = ready_tx.send(Err(EscrowPluginError::SidecarNotReady(
                    ready_line.trim().to_string(),
                )));
                kill_child();
                return;
            }
        }
        Err(e) => {
            let _ = ready_tx.send(Err(EscrowPluginError::SidecarIo(e.to_string())));
            kill_child();
            return;
        }
    }

    // Service loop.
    while let Ok(job) = jobs_rx.recv() {
        // Detect a dead child before trying to talk to it.
        if let Ok(Some(status)) = child
            .lock()
            .expect("child mutex poisoned")
            .try_wait()
        {
            let _ = job
                .reply
                .send(Err(EscrowPluginError::SidecarDied(status.to_string())));
            break;
        }

        let result = round_trip(&mut stdin, &mut stdout, &job.request_line);
        let fatal = result.is_err();
        let _ = job.reply.send(result);
        if fatal {
            // A protocol/I/O failure means the pipe is unreliable; stop.
            break;
        }
    }

    // Channel closed or fatal error → shut the child down.
    kill_child();
}

/// Write one request line and read exactly one JSON reply line.
fn round_trip(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    request_line: &str,
) -> Result<Vec<u8>, EscrowPluginError> {
    stdin
        .write_all(request_line.as_bytes())
        .and_then(|_| stdin.write_all(b"\n"))
        .and_then(|_| stdin.flush())
        .map_err(|e| EscrowPluginError::SidecarIo(e.to_string()))?;

    let mut response = String::new();
    let n = stdout
        .read_line(&mut response)
        .map_err(|e| EscrowPluginError::SidecarIo(e.to_string()))?;
    if n == 0 {
        return Err(EscrowPluginError::SidecarDied("stdout closed".into()));
    }

    let value: serde_json::Value = serde_json::from_str(response.trim())
        .map_err(|e| EscrowPluginError::SidecarProtocol(e.to_string()))?;

    if let Some(data) = value.get("data").and_then(|d| d.as_str()) {
        B64.decode(data)
            .map_err(|e| EscrowPluginError::SidecarProtocol(e.to_string()))
    } else {
        let msg = value
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("unknown error")
            .to_string();
        Err(EscrowPluginError::SidecarDecryptFailed(msg))
    }
}
