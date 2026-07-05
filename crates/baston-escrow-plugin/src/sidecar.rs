//! Shared FXServer sidecar subprocess — the single channel to the genuine,
//! **unmodified** CFX component (`svadhesive`) running in its native host.
//!
//! `svadhesive.dll` cannot be called over FFI (it exposes only an opaque
//! CitizenFX component, single `CreateComponent` export — see the phase D-bis
//! impl notes). Instead we run a minimal **FXServer subprocess** that loads the
//! component normally, and a small Lua shim answers a line-delimited JSON
//! protocol on stdin/stdout. One sidecar process backs BOTH capabilities so we
//! never boot a second FXServer:
//!   - escrow decryption  — request `{ "op": "decrypt", ... }`
//!   - licence validation — request `{ "op": "license_status" }`
//!
//! This module owns only the transport: it ships a request line and returns the
//! parsed JSON reply. Callers ([`crate::SidecarDecryptor`], [`crate::LicenseOracle`])
//! build the request and interpret the reply. Every request waits on a bounded
//! `recv_timeout`, so a frozen or dead child surfaces as a clean `Err`, never a
//! panic or a deadlock.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::error::EscrowPluginError;

/// How long to wait for the sidecar to print `READY` on startup.
const READY_TIMEOUT: Duration = Duration::from_secs(30);
/// Per-request reply timeout (no deadlock on a frozen sidecar).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// One request handed to the actor thread.
struct Job {
    request_line: String,
    reply: Sender<Result<serde_json::Value, EscrowPluginError>>,
}

/// A running FXServer sidecar. Obtained as an `Arc<Sidecar>` so escrow and
/// licence can share one process; the child is killed when the last `Arc` drops.
pub struct Sidecar {
    jobs: Mutex<Sender<Job>>,
    actor: Mutex<Option<JoinHandle<()>>>,
    /// Shared with the actor so `Drop` can kill the child even while the actor
    /// is blocked on a `read_line` — killing closes stdout, unblocking it.
    child: Arc<Mutex<Child>>,
}

impl Sidecar {
    /// Start a real FXServer sidecar for production use.
    ///
    /// Launches `fxserver_path` with `sv_lan true` / `sv_maxclients 0` and the
    /// `baston-decrypt-shim` resource pointed at `resources_dir`.
    pub fn start(
        fxserver_path: &Path,
        resources_dir: &Path,
    ) -> Result<Arc<Self>, EscrowPluginError> {
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
    /// protocol (`READY\n`, then one JSON reply per JSON request line). Exposed
    /// so tests can drive it with a lightweight stub process instead of a full
    /// FXServer install.
    pub fn spawn_with_command(mut cmd: Command) -> Result<Arc<Self>, EscrowPluginError> {
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
            .name("cfx-sidecar".into())
            .spawn(move || actor_loop(actor_child, stdin, stdout, jobs_rx, ready_tx))
            .map_err(|e| EscrowPluginError::SidecarSpawn(e.to_string()))?;

        // Wait for the actor to confirm the sidecar printed READY.
        match ready_rx.recv_timeout(READY_TIMEOUT) {
            Ok(Ok(())) => {
                tracing::info!("baston CFX sidecar started (FXServer subprocess)");
                Ok(Arc::new(Sidecar {
                    jobs: Mutex::new(jobs_tx),
                    actor: Mutex::new(Some(actor)),
                    child,
                }))
            }
            Ok(Err(e)) => Err(e),
            Err(RecvTimeoutError::Timeout) => Err(EscrowPluginError::SidecarStartTimeout),
            Err(RecvTimeoutError::Disconnected) => Err(EscrowPluginError::SidecarIo(
                "actor exited during startup".into(),
            )),
        }
    }

    /// Send one request line and wait (bounded) for the parsed JSON reply.
    pub fn request(&self, request_line: String) -> Result<serde_json::Value, EscrowPluginError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        {
            // Recover a poisoned mutex rather than turning one panicking worker
            // into a cascade that kills every future decrypt request.
            let jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
            jobs.send(Job {
                request_line,
                reply: reply_tx,
            })
            .map_err(|_| EscrowPluginError::SidecarDied("actor thread gone".into()))?;
        }

        match reply_rx.recv_timeout(REQUEST_TIMEOUT) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => Err(EscrowPluginError::SidecarDecryptTimeout),
            Err(RecvTimeoutError::Disconnected) => {
                Err(EscrowPluginError::SidecarDied("actor dropped reply".into()))
            }
        }
    }
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        // Kill the child FIRST: this closes its stdout, which unblocks the
        // actor if it is parked in a blocking `read_line` (e.g. a frozen
        // sidecar). Only then can `join` return promptly.
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
        }
        // Drop the live sender so the actor's `jobs_rx.recv()` also observes the
        // hang-up if it happens to be idle.
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
        if let Ok(Some(status)) = child.lock().unwrap_or_else(|e| e.into_inner()).try_wait() {
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

/// Write one request line and read exactly one JSON reply line. Returns the
/// parsed JSON value verbatim; the caller interprets `data`/`error`/licence
/// fields.
fn round_trip(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    request_line: &str,
) -> Result<serde_json::Value, EscrowPluginError> {
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

    serde_json::from_str(response.trim())
        .map_err(|e| EscrowPluginError::SidecarProtocol(e.to_string()))
}
