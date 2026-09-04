//! Black-box contract tests for the read-only `hype-twap-runs` binary.
//!
//! Every command writes exactly one JSON document to stdout.  This matters to
//! cron/alert tooling: even a useful-looking human prefix makes a non-zero
//! `verify` result impossible to parse safely.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

use hype_trigger_twap::journal::{ExecutionJournal, JournalRecord, RunHeader};
use hype_trigger_twap::types::{Side, Symbol};

struct TempState(PathBuf);

impl TempState {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("hype-twap-runs-cli-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
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

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_hype-twap-runs"))
}

fn header(run_id: &str) -> RunHeader {
    RunHeader {
        run_id: run_id.into(),
        network: "testnet".into(),
        agent: None,
        master: None,
        symbol: Symbol::new("ETH"),
        side: Side::Long,
        slices: 1,
        plan_hash: "public-plan-hash".into(),
        execution_fingerprint: None,
        started_at_unix_ms: 1,
        execution_deadline_unix_ms: None,
    }
}

fn assert_single_json(stdout: &[u8]) -> serde_json::Value {
    let stdout = std::str::from_utf8(stdout).expect("stdout must be UTF-8 JSON");
    assert_eq!(
        stdout.lines().count(),
        1,
        "stdout must contain one JSON document only: {stdout:?}"
    );
    serde_json::from_str(stdout).expect("stdout must be parseable JSON without a human prefix")
}

#[test]
fn list_writes_only_one_json_document_to_stdout() {
    let state = TempState::new();
    let output = binary()
        .args(["--state-dir", state.path().to_str().unwrap(), "list"])
        .output()
        .unwrap();

    assert!(output.status.success(), "list failed: {output:?}");
    let json = assert_single_json(&output.stdout);
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["runs"], serde_json::json!([]));
    assert!(output.stderr.is_empty(), "list stderr must stay empty");
}

#[test]
fn inspect_valid_journal_writes_only_one_json_document_to_stdout() {
    let state = TempState::new();
    let mut journal =
        ExecutionJournal::start(state.path(), "run-valid".into(), header("run-valid")).unwrap();
    journal
        .record(&JournalRecord::FinalReport {
            completed: true,
            filled_total: "0".into(),
            outcome_unknown_cloids: vec![],
            note: "completed".into(),
            whole_run: None,
        })
        .unwrap();
    drop(journal);

    let output = binary()
        .args([
            "--state-dir",
            state.path().to_str().unwrap(),
            "inspect",
            "run-valid",
        ])
        .output()
        .unwrap();

    assert!(output.status.success(), "inspect failed: {output:?}");
    let json = assert_single_json(&output.stdout);
    assert_eq!(json["run_id"], "run-valid");
    assert_eq!(json["status"], "success");
    assert_eq!(json["validation"]["valid"], true);
    assert!(output.stderr.is_empty(), "inspect stderr must stay empty");
}

#[test]
fn verify_corrupt_journal_is_nonzero_but_keeps_stdout_machine_readable() {
    let state = TempState::new();
    let run_dir = state.path().join("runs").join("run-corrupt");
    std::fs::create_dir_all(&run_dir).unwrap();
    // A valid JSON record in an invalid order exercises `verify`'s typed
    // corruption path rather than a shell/process error path.
    std::fs::write(
        run_dir.join("journal.jsonl"),
        r#"{"kind":"FinalReport","completed":true,"filled_total":"0","outcome_unknown_cloids":[],"note":"bad"}"#,
    )
    .unwrap();

    let output = binary()
        .args([
            "--state-dir",
            state.path().to_str().unwrap(),
            "verify",
            "run-corrupt",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success(), "corrupt verify must be non-zero");
    let json = assert_single_json(&output.stdout);
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["error"]["run_id"], "run-corrupt");
    assert_eq!(json["error"]["classification"], "header_not_first");
    assert!(
        output.stderr.is_empty(),
        "typed verify failure must not add a human stderr prefix"
    );
}
