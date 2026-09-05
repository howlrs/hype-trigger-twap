//! Black-box contracts for the executor's live-mode spelling and release gate.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct TempState(PathBuf);

impl TempState {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("hype-twap-live-mode-{}", uuid::Uuid::now_v7())))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempState {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hype-twap"));
    command.env("RUST_LOG", "info");
    for name in [
        "HL_AGENT_PK",
        "HL_AGENT_ADDRESS",
        "HL_MASTER_ADDRESS",
        "HL_INFO_URL",
        "HL_EXCHANGE_URL",
        "HL_METRICS_BIND",
        "HL_ALLOW_EXTERNAL_METRICS",
        "HL_ALERT_HOOK_URL",
    ] {
        command.env_remove(name);
    }
    command.args([
        "--symbol",
        "HYPE",
        "--side",
        "long",
        "--usd",
        "50",
        "--duration",
        "1m",
    ]);
    command
}

fn stderr(output: &Output) -> &str {
    std::str::from_utf8(&output.stderr).expect("stderr must be UTF-8")
}

#[test]
fn live_flag_enters_live_mode_without_a_deprecation_warning() {
    let output = command()
        .args([
            "--network",
            "testnet",
            "--max-notional-usd",
            "100",
            "--report-json",
            "-",
            "--live",
        ])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(
        output.stdout.is_empty(),
        "diagnostics must not leak to stdout"
    );
    let stderr = stderr(&output);
    assert!(
        stderr.contains("HL_AGENT_PK is required in live mode"),
        "{stderr}"
    );
    assert!(!stderr.contains("deprecated"), "{stderr}");
}

#[test]
fn legacy_read_only_false_is_live_equivalent_and_loud() {
    let output = command()
        .args([
            "--network",
            "testnet",
            "--max-notional-usd",
            "100",
            "--read-only",
            "false",
        ])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(
        output.stdout.is_empty(),
        "diagnostics must not leak to stdout"
    );
    let stderr = stderr(&output);
    assert!(
        stderr.contains("--read-only false is deprecated; use --live"),
        "{stderr}"
    );
    assert!(
        stderr.contains("HL_AGENT_PK is required in live mode"),
        "{stderr}"
    );
}

#[test]
fn mainnet_live_is_rejected_before_credentials_or_state_are_touched() {
    let state = TempState::new();
    let output = command()
        .args([
            "--max-notional-usd",
            "100",
            "--state-dir",
            state.path().to_str().unwrap(),
            "--live",
        ])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(
        output.stdout.is_empty(),
        "diagnostics must not leak to stdout"
    );
    let stderr = stderr(&output);
    assert!(
        stderr.contains("mainnet live execution is disabled"),
        "{stderr}"
    );
    assert!(!stderr.contains("HL_AGENT_PK"), "{stderr}");
    assert!(
        !state.path().exists(),
        "mainnet gate must precede state and lock creation"
    );
}

#[test]
fn testnet_live_rejects_an_official_mainnet_endpoint_before_credentials_or_state() {
    let state = TempState::new();
    let output = command()
        .env("HL_INFO_URL", "https://api.hyperliquid.xyz./info")
        .args([
            "--network",
            "testnet",
            "--max-notional-usd",
            "100",
            "--allow-custom-endpoints",
            "--state-dir",
            state.path().to_str().unwrap(),
            "--live",
        ])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty(), "diagnostics must stay on stderr");
    let stderr = stderr(&output);
    assert!(
        stderr.contains("does not match --network testnet"),
        "{stderr}"
    );
    assert!(!stderr.contains("HL_AGENT_PK"), "{stderr}");
    assert!(
        !state.path().exists(),
        "endpoint/network validation must precede state and lock creation"
    );
}

#[test]
fn live_and_explicit_read_only_are_a_usage_error() {
    let output = command()
        .args(["--live", "--read-only", "true"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(
        output.stdout.is_empty(),
        "usage errors must stay off stdout"
    );
    let stderr = stderr(&output);
    assert!(stderr.contains("cannot be used with"), "{stderr}");
    assert!(stderr.contains("--live"), "{stderr}");
    assert!(stderr.contains("--read-only"), "{stderr}");
}
