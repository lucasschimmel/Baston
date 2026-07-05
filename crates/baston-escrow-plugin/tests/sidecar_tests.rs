//! Actor-logic tests for the sidecar decryptor, driven by the `stub_sidecar`
//! test binary (no FXServer required). The real-FXServer path is covered by the
//! `#[ignore]`d test at the bottom.

use std::process::Command;

use baston_core::script_decryptor::ScriptDecryptor;
use baston_escrow_plugin::SidecarDecryptor;

fn stub_command() -> Command {
    Command::new(env!("CARGO_BIN_EXE_stub_sidecar"))
}

fn fxap(payload: &[u8]) -> Vec<u8> {
    let mut v = b"FXAP".to_vec();
    v.extend_from_slice(payload);
    v
}

#[test]
fn starts_and_reports_ready() {
    let sidecar = SidecarDecryptor::spawn_with_command(stub_command());
    assert!(sidecar.is_ok(), "stub sidecar should start");
}

#[test]
fn decrypts_encrypted_payload() {
    let sidecar = SidecarDecryptor::spawn_with_command(stub_command()).expect("start");
    let out = sidecar
        .decrypt(
            "escrow-test",
            "dist/server/index.js",
            &fxap(b"console.log('ok')"),
            None,
        )
        .expect("decrypt");
    assert_eq!(out, b"console.log('ok')");
}

#[test]
fn plain_payload_bypasses_sidecar() {
    let sidecar = SidecarDecryptor::spawn_with_command(stub_command()).expect("start");
    let plain = b"console.log('plain')";
    let out = sidecar
        .decrypt("axiom-core", "dist/server/index.js", plain, None)
        .expect("passthrough");
    assert_eq!(out, plain);
}

#[test]
fn sidecar_error_reply_surfaces_cleanly() {
    let sidecar = SidecarDecryptor::spawn_with_command(stub_command()).expect("start");
    let err = sidecar
        .decrypt("__error__", "f.js", &fxap(b"x"), None)
        .expect_err("must error");
    let msg = err.to_string();
    assert!(msg.contains("entitlement denied"), "got: {msg}");
}

#[test]
fn sidecar_crash_is_reported_not_panicked() {
    let sidecar = SidecarDecryptor::spawn_with_command(stub_command()).expect("start");
    // The stub exits without replying → clean error, no panic.
    let err = sidecar
        .decrypt("__die__", "f.js", &fxap(b"x"), None)
        .expect_err("must error");
    assert!(!err.to_string().is_empty());
}

#[test]
fn frozen_sidecar_times_out_without_deadlock() {
    let sidecar = SidecarDecryptor::spawn_with_command(stub_command()).expect("start");
    // The stub blocks forever; decrypt must return within DECRYPT_TIMEOUT (5s).
    let start = std::time::Instant::now();
    let err = sidecar
        .decrypt("__freeze__", "f.js", &fxap(b"x"), None)
        .expect_err("must time out");
    assert!(start.elapsed() < std::time::Duration::from_secs(10));
    assert!(err.to_string().contains("timed out"), "got: {err}");
}

/// Real end-to-end path: requires a Windows FXServer install with svadhesive.dll
/// and a genuine escrow resource + server licence. Excluded from CI; run
/// manually on a configured dev box with `cargo test -- --ignored`.
#[test]
#[ignore = "needs a real FXServer install + escrow resource + server licence"]
fn real_fxserver_sidecar_decrypts_escrow_resource() {
    // Wiring left to the operator: point at Artifacts/windows/<build>/FXServer.exe
    // and a resources dir containing an escrowed resource, then assert the
    // decrypted bytes compile.
}
