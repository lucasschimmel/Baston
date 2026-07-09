//! Licence-oracle tests, driven by the `stub_sidecar` binary over the real
//! file-drop channel (no FXServer required). The stub shapes its `license_status`
//! reply from the `STUB_LICENSE` env var. The real-FXServer path is the
//! `#[ignore]`d test at the bottom.

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use baston_escrow_plugin::LicenseOracle;
use tempfile::TempDir;

struct Harness {
    _tmp: TempDir,
    ipc: PathBuf,
}

fn harness() -> Harness {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ipc = tmp.path().join("ipc");
    std::fs::create_dir_all(&ipc).unwrap();
    Harness { _tmp: tmp, ipc }
}

fn stub_command(h: &Harness, mode: Option<&str>) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_stub_sidecar"));
    cmd.env("BASTON_SIDECAR_IPC", &h.ipc);
    if let Some(m) = mode {
        cmd.env("STUB_LICENSE", m);
    }
    cmd
}

fn oracle(h: &Harness, mode: Option<&str>) -> LicenseOracle {
    LicenseOracle::spawn_with_command(stub_command(h, mode), h.ipc.clone()).expect("start")
}

#[test]
fn valid_licence_is_reported_valid() {
    let h = harness();
    let status = oracle(&h, None).query_once().expect("query");
    assert!(status.valid && !status.banned);
}

#[test]
fn invalid_licence_is_reported_with_reason() {
    let h = harness();
    let status = oracle(&h, Some("invalid")).query_once().expect("query");
    assert!(!status.valid);
    assert_eq!(status.reason.as_deref(), Some("stub invalid"));
}

#[test]
fn banned_licence_is_reported_banned() {
    let h = harness();
    let status = oracle(&h, Some("banned")).query_once().expect("query");
    assert!(status.banned);
    assert!(!baston_core::license::boot_decision(&status).is_allowed());
}

#[test]
fn entitlement_slots_are_reported() {
    let h = harness();
    let status = oracle(&h, Some("slots48")).query_once().expect("query");
    assert_eq!(status.entitlements.max_slots, Some(48));
    // And enforcement caps a higher configured value.
    let (eff, capped) = baston_core::license::effective_max_players(64, &status.entitlements);
    assert_eq!((eff, capped), (48, true));
}

#[test]
fn retry_returns_valid_promptly() {
    let h = harness();
    let status = oracle(&h, None)
        .query(Duration::from_secs(5), Duration::from_millis(200))
        .expect("query");
    assert!(status.valid);
}

#[test]
fn retry_fails_closed_on_persistent_invalid() {
    // An invalid licence never flips to valid → after the budget, the oracle
    // returns the invalid status (never an optimistic "valid"), promptly.
    let h = harness();
    let start = Instant::now();
    let status = oracle(&h, Some("invalid"))
        .query(Duration::from_millis(600), Duration::from_millis(200))
        .expect("query");
    assert!(!status.valid);
    assert!(start.elapsed() < Duration::from_secs(3));
}

/// Real end-to-end path: requires a Windows FXServer install with svadhesive.dll
/// and a genuine CFX server licence. Excluded from CI; run manually on a
/// configured dev box with `cargo test -- --ignored`.
#[test]
#[ignore = "needs a real FXServer install + a genuine CFX server licence"]
fn real_fxserver_reports_licence_status() {
    // Wiring left to the operator: start a SidecarHandle with SidecarParams against
    // Artifacts/windows/<build>/FXServer.exe with a real sv_licenseKey, then assert
    // oracle().query_once() reports valid = true.
}
