//! Licence-oracle tests, driven by the `stub_sidecar` binary (no FXServer
//! required). The stub shapes its `license_status` reply from the `STUB_LICENSE`
//! env var. The real-FXServer path is the `#[ignore]`d test at the bottom.

use std::process::Command;
use std::time::{Duration, Instant};

use baston_escrow_plugin::LicenseOracle;

fn stub_command(mode: Option<&str>) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_stub_sidecar"));
    if let Some(m) = mode {
        cmd.env("STUB_LICENSE", m);
    }
    cmd
}

#[test]
fn valid_licence_is_reported_valid() {
    let oracle = LicenseOracle::spawn_with_command(stub_command(None)).expect("start");
    let status = oracle.query_once().expect("query");
    assert!(status.valid && !status.banned);
}

#[test]
fn invalid_licence_is_reported_with_reason() {
    let oracle = LicenseOracle::spawn_with_command(stub_command(Some("invalid"))).expect("start");
    let status = oracle.query_once().expect("query");
    assert!(!status.valid);
    assert_eq!(status.reason.as_deref(), Some("stub invalid"));
}

#[test]
fn banned_licence_is_reported_banned() {
    let oracle = LicenseOracle::spawn_with_command(stub_command(Some("banned"))).expect("start");
    let status = oracle.query_once().expect("query");
    assert!(status.banned);
    assert!(!baston_core::license::boot_decision(&status).is_allowed());
}

#[test]
fn entitlement_slots_are_reported() {
    let oracle = LicenseOracle::spawn_with_command(stub_command(Some("slots48"))).expect("start");
    let status = oracle.query_once().expect("query");
    assert_eq!(status.entitlements.max_slots, Some(48));
    // And enforcement caps a higher configured value.
    let (eff, capped) = baston_core::license::effective_max_players(64, &status.entitlements);
    assert_eq!((eff, capped), (48, true));
}

#[test]
fn retry_returns_valid_promptly() {
    let oracle = LicenseOracle::spawn_with_command(stub_command(None)).expect("start");
    let status = oracle
        .query(Duration::from_secs(5), Duration::from_millis(200))
        .expect("query");
    assert!(status.valid);
}

#[test]
fn retry_fails_closed_on_persistent_invalid() {
    // An invalid licence never flips to valid → after the budget, the oracle
    // returns the invalid status (never an optimistic "valid"), promptly.
    let oracle = LicenseOracle::spawn_with_command(stub_command(Some("invalid"))).expect("start");
    let start = Instant::now();
    let status = oracle
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
    // Wiring left to the operator: start a SidecarHandle against
    // Artifacts/windows/<build>/FXServer.exe with a real sv_licenseKey, then
    // assert `oracle().query_once()` reports valid = true.
}
