//! Test-only stub implementing the sidecar line protocol, so the actor logic in
//! [`baston_escrow_plugin::SidecarDecryptor`] can be exercised in CI without a
//! real FXServer install. Integration tests spawn it via `CARGO_BIN_EXE_stub_sidecar`.
//!
//! Protocol: print `READY`, then for each JSON request line reply once.
//!
//! `op = "decrypt"` (or absent): `{resource,file,data(b64)}` → `{data(b64)}`
//! where the payload is the decoded input minus a leading `FXAP` magic. Special
//! resource names trigger failure modes for tests:
//! - `__die__`   → exit without replying (simulates a crash).
//! - `__freeze__`→ block forever (simulates a hang; caller must time out).
//! - `__error__` → reply `{error: "..."}`.
//!
//! `op = "license_status"` → a licence verdict shaped by the `STUB_LICENSE` env
//! var: `invalid` / `banned` / `slots48` / (default) valid.

use std::io::{BufRead, Write};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    writeln!(stdout, "READY").unwrap();
    stdout.flush().unwrap();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let req: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                writeln!(stdout, "{{\"error\":\"invalid json\"}}").unwrap();
                stdout.flush().unwrap();
                continue;
            }
        };
        let op = req.get("op").and_then(|v| v.as_str()).unwrap_or("decrypt");
        if op == "license_status" {
            let reply = match std::env::var("STUB_LICENSE").as_deref() {
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
            };
            writeln!(stdout, "{reply}").unwrap();
            stdout.flush().unwrap();
            continue;
        }
        let resource = req.get("resource").and_then(|v| v.as_str()).unwrap_or("");
        match resource {
            "__die__" => std::process::exit(0),
            "__freeze__" => loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            },
            "__error__" => {
                writeln!(stdout, "{{\"error\":\"entitlement denied\"}}").unwrap();
                stdout.flush().unwrap();
            }
            _ => {
                let data = req.get("data").and_then(|v| v.as_str()).unwrap_or("");
                let bytes = B64.decode(data).unwrap_or_default();
                let plain = bytes.strip_prefix(b"FXAP").unwrap_or(&bytes);
                let reply = serde_json::json!({ "data": B64.encode(plain) });
                writeln!(stdout, "{reply}").unwrap();
                stdout.flush().unwrap();
            }
        }
    }
}
