//! Test-only stub implementing the **file-drop** sidecar protocol, so the
//! transport and the [`baston_escrow_plugin::SidecarDecryptor`] /
//! [`baston_escrow_plugin::LicenseOracle`] logic can be exercised in CI without a
//! real FXServer install. Integration tests spawn it via `CARGO_BIN_EXE_stub_sidecar`.
//!
//! Environment:
//! - `BASTON_SIDECAR_IPC`  — the ipc dir to poll (required).
//! - `BASTON_SIDECAR_RES`  — resources root for `decrypt` reads (optional).
//! - `STUB_LICENSE`        — shapes the `license_status` reply:
//!   `invalid` / `banned` / `slots48` / (default) valid.
//!
//! Protocol: write `ready.json`, then for each `request.json` (matched by a
//! monotonic `id`) write the matching `response.json`.
//!
//! `op = "license_status"` → a verdict shaped by `STUB_LICENSE`.
//! `op = "decrypt"` (or absent) → reads `<RES>/<resource>/<file>` and returns it
//! base64-encoded. Special resource names trigger failure modes for tests:
//! - `__die__`   → exit without replying (simulates a crash).
//! - `__freeze__`→ block forever (simulates a hang; caller must time out).
//! - `__error__` → reply `{ error: "..." }`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;

fn main() {
    let ipc = PathBuf::from(std::env::var("BASTON_SIDECAR_IPC").expect("BASTON_SIDECAR_IPC set"));
    std::fs::create_dir_all(&ipc).ok();
    write_atomic(&ipc.join("ready.json"), br#"{"ready":true,"protocol":2}"#);

    let mut last_id = 0u64;
    loop {
        std::thread::sleep(Duration::from_millis(5));
        let raw = match std::fs::read_to_string(ipc.join("request.json")) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let req: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let id = match req.get("id").and_then(|v| v.as_u64()) {
            Some(i) if i > last_id => i,
            _ => continue,
        };

        let op = req.get("op").and_then(|v| v.as_str()).unwrap_or("decrypt");
        let mut reply = if op == "license_status" {
            license_reply()
        } else {
            match req.get("resource").and_then(|v| v.as_str()).unwrap_or("") {
                "__die__" => std::process::exit(0),
                "__freeze__" => loop {
                    std::thread::sleep(Duration::from_secs(3600));
                },
                "__error__" => serde_json::json!({ "error": "entitlement denied" }),
                _ => decrypt_reply(&req),
            }
        };
        reply["id"] = serde_json::Value::from(id);
        write_atomic(&ipc.join("response.json"), reply.to_string().as_bytes());
        last_id = id;
    }
}

fn license_reply() -> serde_json::Value {
    match std::env::var("STUB_LICENSE").as_deref() {
        Ok("invalid") => {
            serde_json::json!({ "valid": false, "banned": false, "reason": "stub invalid" })
        }
        Ok("banned") => {
            serde_json::json!({ "valid": false, "banned": true, "reason": "stub banned" })
        }
        Ok("slots48") => serde_json::json!({
            "valid": true, "banned": false,
            "entitlements": { "max_slots": 48, "features": [] }
        }),
        _ => serde_json::json!({
            "valid": true, "banned": false, "entitlements": { "features": [] }
        }),
    }
}

/// Mirror the real shim: return the (already-"decrypted") on-disk file bytes.
fn decrypt_reply(req: &serde_json::Value) -> serde_json::Value {
    let resource = req.get("resource").and_then(|v| v.as_str()).unwrap_or("");
    let file = req.get("file").and_then(|v| v.as_str()).unwrap_or("");
    let root = match std::env::var("BASTON_SIDECAR_RES") {
        Ok(r) => PathBuf::from(r),
        Err(_) => return serde_json::json!({ "error": "stub: BASTON_SIDECAR_RES not set" }),
    };
    let path = root.join(resource).join(file);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::json!({ "data": B64.encode(bytes) }),
        Err(_) => serde_json::json!({ "error": format!("file not found: {}", path.display()) }),
    }
}

fn write_atomic(path: &Path, data: &[u8]) {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    if std::fs::write(&tmp, data).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}
