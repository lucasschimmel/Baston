//! Licence-oracle tests, driven by the `stub_sidecar` binary over the real
//! file-drop channel (no FXServer required). The stub shapes its `license_status`
//! reply from the `STUB_LICENSE` env var. The real-FXServer path is the
//! `#[ignore]`d test at the bottom.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use baston_cfx_platform::{CfxPlatformError, LicenseOracle, Sidecar, SidecarParams};
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
    let oracle = oracle(&h, None);
    let status = oracle.query_once().expect("query");
    assert!(status.valid && !status.banned);
    assert!(
        !h.ipc.join("response.json").exists(),
        "the authenticated token must not remain in the IPC response file"
    );
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

#[test]
fn retry_does_not_accept_nominal_valid_without_token() {
    let h = harness();
    let start = Instant::now();
    let status = oracle(&h, Some("valid_no_token"))
        .query(Duration::from_millis(300), Duration::from_millis(50))
        .expect("query");
    assert!(!status.is_authenticated());
    assert!(
        start.elapsed() >= Duration::from_millis(250),
        "an incomplete verdict must be retried until the startup budget expires"
    );
}

#[test]
fn cancellation_stops_an_in_flight_licence_query() {
    let h = harness();
    let oracle = oracle(&h, None);
    let cancelled = AtomicBool::new(true);

    let error = oracle
        .query_with_cancellation(Duration::from_secs(20), Duration::from_secs(1), &cancelled)
        .unwrap_err();

    assert!(matches!(error, CfxPlatformError::SidecarCancelled));
}

/// Real end-to-end path: requires a Windows FXServer install with svadhesive.dll
/// and a genuine CFX server licence. Excluded from CI; run manually on a
/// configured dev box with `cargo test -- --ignored`.
#[test]
#[ignore = "needs a real FXServer install + a genuine CFX server licence"]
fn real_fxserver_reports_licence_status() {
    let fxserver_path = std::env::var_os("BASTON_TEST_FXSERVER")
        .map(PathBuf::from)
        .expect("set BASTON_TEST_FXSERVER to an official FXServer.exe");
    let license_key = std::env::var("BASTON_TEST_LICENSE_KEY")
        .expect("set BASTON_TEST_LICENSE_KEY without committing or printing it");
    let port = std::env::var("BASTON_TEST_SIDECAR_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(30_130);
    let temp = tempfile::tempdir().expect("temporary sidecar resources");
    let params = SidecarParams {
        fxserver_path,
        resources_dir: temp.path().join("resources"),
        license_key: Some(license_key),
        port,
        public_listing: None,
    };

    let sidecar = Sidecar::start(&params).expect("official FXServer broker startup");
    let status = LicenseOracle::from_sidecar(sidecar)
        .query(Duration::from_secs(20), Duration::from_secs(1))
        .expect("official broker licence query");

    assert!(
        status.is_authenticated(),
        "official broker did not return an authenticated identity"
    );
}
