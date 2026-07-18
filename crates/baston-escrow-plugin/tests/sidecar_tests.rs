//! Transport + decryptor tests, driven by the `stub_sidecar` test binary over the
//! real file-drop channel (no FXServer required). The real-FXServer path is the
//! `#[ignore]`d test at the bottom.

use std::path::PathBuf;
use std::process::Command;

use baston_core::script_decryptor::ScriptDecryptor;
use baston_escrow_plugin::SidecarDecryptor;
use tempfile::TempDir;

/// One isolated ipc + resources dir pair per test.
struct Harness {
    _tmp: TempDir,
    ipc: PathBuf,
    res: PathBuf,
}

fn harness() -> Harness {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ipc = tmp.path().join("ipc");
    let res = tmp.path().join("res");
    std::fs::create_dir_all(&ipc).unwrap();
    std::fs::create_dir_all(&res).unwrap();
    Harness {
        _tmp: tmp,
        ipc,
        res,
    }
}

fn stub_command(h: &Harness) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_stub_sidecar"));
    cmd.env("BASTON_SIDECAR_IPC", &h.ipc);
    cmd.env("BASTON_SIDECAR_RES", &h.res);
    cmd
}

/// Write a plaintext resource file the stub will "decrypt" (return verbatim).
fn write_resource_file(h: &Harness, resource: &str, file: &str, contents: &[u8]) {
    let path = h.res.join(resource).join(file);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn fxap(payload: &[u8]) -> Vec<u8> {
    let mut v = b"FXAP".to_vec();
    v.extend_from_slice(payload);
    v
}

#[test]
fn starts_and_reports_ready() {
    let h = harness();
    let sidecar = SidecarDecryptor::spawn_with_command(stub_command(&h), h.ipc.clone());
    assert!(sidecar.is_ok(), "stub sidecar should become ready");
}

#[test]
fn decrypts_encrypted_payload() {
    let h = harness();
    write_resource_file(
        &h,
        "escrow-test",
        "dist/server/index.js",
        b"console.log('ok')",
    );
    let sidecar =
        SidecarDecryptor::spawn_with_command(stub_command(&h), h.ipc.clone()).expect("start");
    let out = sidecar
        .decrypt(
            "escrow-test",
            "dist/server/index.js",
            &fxap(b"ciphertext-ignored"),
            None,
        )
        .expect("decrypt");
    assert_eq!(out, b"console.log('ok')");
}

#[test]
fn plain_payload_bypasses_sidecar() {
    let h = harness();
    let sidecar =
        SidecarDecryptor::spawn_with_command(stub_command(&h), h.ipc.clone()).expect("start");
    let plain = b"console.log('plain')";
    let out = sidecar
        .decrypt("axiom-core", "dist/server/index.js", plain, None)
        .expect("passthrough");
    assert_eq!(out, plain);
}

#[test]
fn sidecar_error_reply_surfaces_cleanly() {
    let h = harness();
    let sidecar =
        SidecarDecryptor::spawn_with_command(stub_command(&h), h.ipc.clone()).expect("start");
    let err = sidecar
        .decrypt("__error__", "f.js", &fxap(b"x"), None)
        .expect_err("must error");
    assert!(err.to_string().contains("entitlement denied"), "got: {err}");
}

#[test]
fn sidecar_crash_is_reported_not_panicked() {
    let h = harness();
    let sidecar =
        SidecarDecryptor::spawn_with_command(stub_command(&h), h.ipc.clone()).expect("start");
    // The stub exits without replying → clean error, no panic.
    let err = sidecar
        .decrypt("__die__", "f.js", &fxap(b"x"), None)
        .expect_err("must error");
    assert!(!err.to_string().is_empty());
}

#[test]
fn frozen_sidecar_times_out_without_deadlock() {
    let h = harness();
    let sidecar =
        SidecarDecryptor::spawn_with_command(stub_command(&h), h.ipc.clone()).expect("start");
    // The stub blocks forever; decrypt must return within REQUEST_TIMEOUT (8s).
    let start = std::time::Instant::now();
    let err = sidecar
        .decrypt("__freeze__", "f.js", &fxap(b"x"), None)
        .expect_err("must time out");
    assert!(start.elapsed() < std::time::Duration::from_secs(12));
    assert!(err.to_string().contains("timed out"), "got: {err}");
}

/// Real end-to-end path: requires a Windows FXServer install with svadhesive.dll
/// and a genuine escrow resource + server licence. Excluded from CI; run manually
/// on a configured dev box with `cargo test -- --ignored`.
#[test]
#[ignore = "needs a real FXServer install + escrow resource + server licence"]
fn real_fxserver_sidecar_decrypts_escrow_resource() {
    // Wiring left to the operator: start a SidecarHandle with SidecarParams
    // pointing at Artifacts/windows/<build>/FXServer.exe, a real sv_licenseKey and
    // a resources dir containing an escrowed resource, then assert the decrypted
    // bytes compile.
}
