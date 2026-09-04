//! Secret-safe, machine-readable reports over durable execution journals.
//!
//! [`ValidatedJournalReplay`] remains the only interpretation of a valid
//! journal.  This module only projects that interpretation into a stable JSON
//! DTO for the read-only `hype-twap-runs` command.

use std::path::Path;

use serde::Serialize;

use crate::journal::{
    validate_run_id, ExecutionJournal, JournalError, JournalRecord, JournalReplayError, RunSummary,
    ValidatedJournalReplay,
};

/// Version of the public run-report JSON schema.
pub const RUN_REPORT_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Clone, Serialize)]
pub struct RunsListReport {
    pub schema_version: u16,
    pub runs: Vec<RunReport>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunReport {
    pub schema_version: u16,
    pub run_id: String,
    pub identity: Option<RunIdentity>,
    pub status: RunStatus,
    pub started_at_unix_ms: Option<u64>,
    /// Journal records deliberately do not carry a completion timestamp.
    /// `null` therefore means "not durably known", rather than "still open".
    pub ended_at_unix_ms: Option<u64>,
    pub execution_deadline_unix_ms: Option<u64>,
    pub accounting: Option<RunAccounting>,
    pub cap: RunCap,
    pub unresolved: Vec<UnresolvedCloid>,
    pub resume_eligible: bool,
    pub path: String,
    pub validation: Validation,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunIdentity {
    pub network: String,
    pub agent: Option<String>,
    pub master: Option<String>,
    pub symbol: String,
    pub side: String,
    pub slices: u32,
    pub plan_hash: String,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Success,
    Incomplete,
    Abandoned,
    Corrupt,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunAccounting {
    pub requested_total: Option<String>,
    pub adjusted_total: Option<String>,
    pub filled_size: String,
    pub accounted_notional: String,
    pub trusted_vwap: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunCap {
    pub maximum_notional: Option<String>,
    pub remaining_notional: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UnresolvedCloid {
    pub cloid: String,
    pub reason: UnresolvedReason,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnresolvedReason {
    NotTerminal,
    FinalReportOutcomeUnknown,
}

#[derive(Debug, Clone, Serialize)]
pub struct Validation {
    pub valid: bool,
    pub classification: ValidationClassification,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ValidationClassification {
    Valid,
    JournalReadError,
    JournalParseError,
    InvalidRunId,
    MissingHeader,
    HeaderNotFirst,
    DuplicateHeader,
    MissingPrepared,
    ConflictingPrepared,
    SliceIndexChanged,
    InvalidTransition,
    InvalidTerminalStatus,
    AbandonedWithUnresolved,
    RecordAfterAbandoned,
    InvalidFingerprint,
    AccountingError,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunReportError {
    pub schema_version: u16,
    pub error: RunReportErrorBody,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunReportErrorBody {
    pub run_id: String,
    pub classification: ValidationClassification,
}

/// List direct children of `<state-dir>/runs` in deterministic run-id order.
pub fn list(state_root: &Path) -> Result<RunsListReport, std::io::Error> {
    let runs_dir = state_root.join("runs");
    if !runs_dir.is_dir() {
        return Ok(RunsListReport {
            schema_version: RUN_REPORT_SCHEMA_VERSION,
            runs: Vec::new(),
        });
    }
    let mut entries: Vec<_> = std::fs::read_dir(runs_dir)?
        .filter_map(Result::ok)
        .collect();
    entries.sort_by_key(|entry| entry.file_name());
    let runs = entries
        .into_iter()
        // A run directory is state owned by this tool.  Do not follow an
        // operator-created symlink outside the state root while producing a
        // supposedly read-only report.
        .filter(|entry| {
            std::fs::symlink_metadata(entry.path())
                .map(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
                .unwrap_or(false)
        })
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|run_id| validate_run_id(run_id).is_ok())
        .map(|run_id| inspect(state_root, &run_id))
        .collect();
    Ok(RunsListReport {
        schema_version: RUN_REPORT_SCHEMA_VERSION,
        runs,
    })
}

/// Produce a report even for an unreadable or invalid journal.  This is used
/// by `list` and `inspect`; `verify` turns invalid reports into a non-zero
/// JSON error.
pub fn inspect(state_root: &Path, run_id: &str) -> RunReport {
    if validate_run_id(run_id).is_err() {
        return corrupt_report(
            run_id,
            &state_root
                .join("runs")
                .join("<invalid-run-id>")
                .join("journal.jsonl"),
            ValidationClassification::InvalidRunId,
        );
    }
    let path = ExecutionJournal::journal_path(state_root, run_id);
    match ExecutionJournal::read_all(state_root, run_id) {
        Ok(records) => from_records(run_id, &path, &records),
        Err(error) => corrupt_report(run_id, &path, classification_for_read_error(&error)),
    }
}

pub fn verify(state_root: &Path, run_id: &str) -> Result<RunReport, RunReportError> {
    let report = inspect(state_root, run_id);
    if report.validation.valid {
        Ok(report)
    } else {
        Err(RunReportError {
            schema_version: RUN_REPORT_SCHEMA_VERSION,
            error: RunReportErrorBody {
                run_id: run_id.to_owned(),
                classification: report.validation.classification,
            },
        })
    }
}

fn from_records(run_id: &str, path: &Path, records: &[JournalRecord]) -> RunReport {
    let replay = match ValidatedJournalReplay::replay(records) {
        Ok(replay) => replay,
        Err(error) => return corrupt_report(run_id, path, classification_for_replay_error(&error)),
    };
    valid_report(run_id, path, replay)
}

fn valid_report(run_id: &str, path: &Path, replay: ValidatedJournalReplay) -> RunReport {
    let maximum_notional = replay.fingerprint_max_notional;
    let summary = replay.summary;
    let Some(header) = summary.header.as_ref() else {
        return corrupt_report(run_id, path, ValidationClassification::MissingHeader);
    };
    let max = maximum_notional.map(|value| value.to_string());
    let latest_whole_run = summary.last_whole_run.as_ref();
    // Recompute from the validated fingerprint and replay; `whole_run` is a
    // redundant projection and must never override the authoritative cap.
    let remaining = maximum_notional.and_then(|cap| {
        cap.checked_sub(replay.fill_totals.notional)
            .map(|n| n.max(rust_decimal::Decimal::ZERO).to_string())
    });
    let status = if summary.abandoned {
        RunStatus::Abandoned
    } else if summary.is_incomplete() {
        RunStatus::Incomplete
    } else {
        RunStatus::Success
    };
    let unresolved = unresolved(&summary);
    RunReport {
        schema_version: RUN_REPORT_SCHEMA_VERSION,
        run_id: run_id.to_owned(),
        identity: Some(RunIdentity {
            network: header.network.clone(),
            agent: header.agent.as_ref().map(ToString::to_string),
            master: header.master.as_ref().map(ToString::to_string),
            symbol: header.symbol.to_string(),
            side: header.side.to_string(),
            slices: header.slices,
            plan_hash: header.plan_hash.clone(),
        }),
        status,
        started_at_unix_ms: Some(header.started_at_unix_ms),
        ended_at_unix_ms: latest_whole_run.map(|whole| {
            header
                .started_at_unix_ms
                .saturating_add(whole.logical_elapsed_ms)
        }),
        execution_deadline_unix_ms: header.execution_deadline_unix_ms,
        accounting: Some(RunAccounting {
            requested_total: latest_whole_run
                .and_then(|whole| whole.requested_total.clone())
                .or_else(|| {
                    header.execution_fingerprint.as_ref().map(|fingerprint| {
                        fingerprint
                            .logical_position_total()
                            .unwrap_or_else(|| fingerprint.total_requested.clone())
                    })
                }),
            adjusted_total: latest_whole_run
                .and_then(|whole| whole.adjusted_total.clone())
                .or_else(|| {
                    header.execution_fingerprint.as_ref().map(|fingerprint| {
                        fingerprint
                            .logical_position_total()
                            .unwrap_or_else(|| fingerprint.total_adjusted.clone())
                    })
                }),
            filled_size: replay.fill_totals.filled_sz.to_string(),
            accounted_notional: replay.fill_totals.notional.to_string(),
            trusted_vwap: replay.execution_vwap.map(|v| v.to_string()),
        }),
        cap: RunCap {
            maximum_notional: max,
            remaining_notional: remaining,
        },
        unresolved,
        resume_eligible: resume_eligible(status, header),
        path: path.display().to_string(),
        validation: Validation {
            valid: true,
            classification: ValidationClassification::Valid,
        },
    }
}

/// A report must not advertise an unsafe legacy/malformed journal as
/// resumable.  The live command performs stricter invocation identity and
/// exchange reconciliation before any new order; this is the read-only,
/// durable minimum needed to avoid suggesting a resume that cannot preserve
/// the original deadline or position-phase identity.
fn resume_eligible(status: RunStatus, header: &crate::journal::RunHeader) -> bool {
    if status != RunStatus::Incomplete {
        return false;
    }
    let Some(fingerprint) = header.execution_fingerprint.as_ref() else {
        return false;
    };
    if fingerprint.version != crate::journal::ExecutionPlanFingerprint::VERSION
        || header.execution_deadline_unix_ms.is_none()
        || fingerprint.absolute_deadline_unix_ms != header.execution_deadline_unix_ms
    {
        return false;
    }
    match fingerprint.position_mode.as_deref() {
        None => {
            fingerprint.initial_position_szi.is_none()
                && fingerprint.target_position_szi.is_none()
                && fingerprint.position_requested_value.is_none()
                && fingerprint.position_reference_price.is_none()
                && fingerprint.position_phases.is_empty()
        }
        Some(mode) => {
            fingerprint.initial_position_szi.is_some()
                && fingerprint.target_position_szi.is_some()
                && !fingerprint.position_phases.is_empty()
                && match mode {
                    "flatten" => {
                        fingerprint.position_requested_value.is_none()
                            && fingerprint.position_reference_price.is_none()
                    }
                    "target_sz" => {
                        fingerprint.position_requested_value.is_some()
                            && fingerprint.position_reference_price.is_none()
                    }
                    "target_usd" => {
                        fingerprint.position_requested_value.is_some()
                            && fingerprint.position_reference_price.is_some()
                    }
                    _ => false,
                }
        }
    }
}

fn unresolved(summary: &RunSummary) -> Vec<UnresolvedCloid> {
    let mut out: Vec<_> = summary
        .unresolved_cloids()
        .into_iter()
        .map(|cloid| UnresolvedCloid {
            cloid: cloid.to_string(),
            reason: UnresolvedReason::NotTerminal,
        })
        .collect();
    if let Some(unknown) = &summary.last_final_report_unknown_cloids {
        for cloid in unknown {
            if !out
                .iter()
                .any(|existing| existing.cloid == cloid.to_string())
            {
                out.push(UnresolvedCloid {
                    cloid: cloid.to_string(),
                    reason: UnresolvedReason::FinalReportOutcomeUnknown,
                });
            }
        }
    }
    out
}

fn corrupt_report(
    run_id: &str,
    path: &Path,
    classification: ValidationClassification,
) -> RunReport {
    RunReport {
        schema_version: RUN_REPORT_SCHEMA_VERSION,
        run_id: run_id.to_owned(),
        identity: None,
        status: RunStatus::Corrupt,
        started_at_unix_ms: None,
        ended_at_unix_ms: None,
        execution_deadline_unix_ms: None,
        accounting: None,
        cap: RunCap {
            maximum_notional: None,
            remaining_notional: None,
        },
        unresolved: Vec::new(),
        resume_eligible: false,
        path: path.display().to_string(),
        validation: Validation {
            valid: false,
            classification,
        },
    }
}

fn classification_for_read_error(error: &JournalError) -> ValidationClassification {
    match error {
        JournalError::Parse { .. } => ValidationClassification::JournalParseError,
        JournalError::InvalidRunId { .. } => ValidationClassification::InvalidRunId,
        _ => ValidationClassification::JournalReadError,
    }
}

fn classification_for_replay_error(error: &JournalReplayError) -> ValidationClassification {
    match error {
        JournalReplayError::MissingHeader => ValidationClassification::MissingHeader,
        JournalReplayError::HeaderNotFirst => ValidationClassification::HeaderNotFirst,
        JournalReplayError::DuplicateHeader => ValidationClassification::DuplicateHeader,
        JournalReplayError::MissingPrepared { .. } => ValidationClassification::MissingPrepared,
        JournalReplayError::ConflictingPrepared { .. } => {
            ValidationClassification::ConflictingPrepared
        }
        JournalReplayError::SliceIndexChanged { .. } => ValidationClassification::SliceIndexChanged,
        JournalReplayError::InvalidTransition { .. } => ValidationClassification::InvalidTransition,
        JournalReplayError::InvalidTerminalStatus { .. } => {
            ValidationClassification::InvalidTerminalStatus
        }
        JournalReplayError::AbandonedWithUnresolved { .. } => {
            ValidationClassification::AbandonedWithUnresolved
        }
        JournalReplayError::RecordAfterAbandoned => ValidationClassification::RecordAfterAbandoned,
        JournalReplayError::CompletedFinalReportNotComplete
        | JournalReplayError::FinalReportUnknownCloidsMismatch { .. }
        | JournalReplayError::RecordAfterCompletedFinalReport => {
            ValidationClassification::InvalidTransition
        }
        JournalReplayError::InvalidFinalReportFilledTotal { .. }
        | JournalReplayError::FinalReportFilledTotalMismatch { .. }
        | JournalReplayError::InvalidFingerprintMaxNotional { .. }
        | JournalReplayError::InvalidWholeRunSummary { .. } => {
            ValidationClassification::AccountingError
        }
        JournalReplayError::InvalidFingerprint { .. } => {
            ValidationClassification::InvalidFingerprint
        }
        JournalReplayError::Accounting(_) => ValidationClassification::AccountingError,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::journal::{ExecutionPlanFingerprint, JournalRecord, RunHeader};
    use crate::types::{Cloid, Side, Symbol, Tif};
    use uuid::Uuid;

    fn header(id: &str) -> JournalRecord {
        JournalRecord::Header(RunHeader {
            run_id: id.into(),
            network: "testnet".into(),
            agent: None,
            master: None,
            symbol: Symbol::from("ETH"),
            side: Side::Long,
            slices: 1,
            plan_hash: "public-plan-hash".into(),
            execution_fingerprint: None,
            started_at_unix_ms: 10,
            execution_deadline_unix_ms: Some(20),
        })
    }

    fn typed_header(
        id: &str,
        slices: u32,
        per_slice: &str,
        total: &str,
        cap: &str,
    ) -> JournalRecord {
        JournalRecord::Header(RunHeader {
            run_id: id.into(),
            network: "testnet".into(),
            agent: None,
            master: None,
            symbol: Symbol::from("ETH"),
            side: Side::Long,
            slices,
            plan_hash: "typed-plan".into(),
            execution_fingerprint: Some(ExecutionPlanFingerprint {
                version: ExecutionPlanFingerprint::VERSION,
                symbol: "ETH".into(),
                side: "long".into(),
                request_mode: "size".into(),
                request_value: total.into(),
                per_slice: per_slice.into(),
                total_adjusted: total.into(),
                total_requested: total.into(),
                slices,
                duration_ms: 10_000,
                slippage_bps: "20".into(),
                max_notional_usd: cap.into(),
                max_book_age_ms: 3_000,
                settle_retries: 25,
                child_algo: "market".into(),
                follow_poll_secs: 2,
                follow_repost_secs: 10,
                follow_threshold_bps: "1".into(),
                network: "testnet".into(),
                agent: None,
                master: None,
                position_mode: None,
                initial_position_szi: None,
                target_position_szi: None,
                position_requested_value: None,
                position_reference_price: None,
                position_phases: Vec::new(),
                reduce_only: false,
                absolute_deadline_unix_ms: Some(20_000),
            }),
            started_at_unix_ms: 10_000,
            execution_deadline_unix_ms: Some(20_000),
        })
    }

    fn prepared(slice_idx: u32, cloid: Cloid, sz: &str, px: &str) -> JournalRecord {
        JournalRecord::Prepared {
            slice_idx,
            cloid,
            nonce: None,
            symbol: Symbol::from("ETH"),
            side: Side::Long,
            tif: Some(Tif::Ioc),
            px: px.into(),
            sz: sz.into(),
        }
    }

    fn terminal(slice_idx: u32, cloid: Cloid, filled: &str, px: &str) -> JournalRecord {
        JournalRecord::Terminal {
            slice_idx,
            cloid,
            status: "filled".into(),
            filled_sz: filled.into(),
            avg_px: Some(px.into()),
        }
    }

    fn whole_run(
        total: &str,
        notional: &str,
        cap_remaining: &str,
        vwap: Option<&str>,
        unresolved: usize,
    ) -> crate::journal::WholeRunSummary {
        crate::journal::WholeRunSummary {
            requested_total: Some(total.into()),
            adjusted_total: Some(total.into()),
            accounted_notional: notional.into(),
            cap_remaining: Some(cap_remaining.into()),
            trusted_vwap: vwap.map(str::to_owned),
            logical_elapsed_ms: 500,
            unresolved_cloids: unresolved,
        }
    }

    #[test]
    fn whole_run_reports_cover_normal_partial_abort_cap_and_resumed_completion() {
        struct Case {
            name: &'static str,
            records: Vec<JournalRecord>,
            status: RunStatus,
            filled: &'static str,
            notional: &'static str,
            vwap: Option<&'static str>,
            cap_remaining: &'static str,
            unresolved: usize,
        }

        let normal = Cloid::from_uuid(Uuid::from_u128(1));
        let partial = Cloid::from_uuid(Uuid::from_u128(2));
        let aborted = Cloid::from_uuid(Uuid::from_u128(3));
        let capped = Cloid::from_uuid(Uuid::from_u128(4));
        let resumed_first = Cloid::from_uuid(Uuid::from_u128(5));
        let resumed_second = Cloid::from_uuid(Uuid::from_u128(6));
        let cases = vec![
            Case {
                name: "normal",
                records: vec![
                    typed_header("normal-whole-run", 1, "2", "2", "100"),
                    prepared(1, normal, "2", "10"),
                    terminal(1, normal, "2", "10"),
                    JournalRecord::FinalReport {
                        completed: true,
                        filled_total: "2".into(),
                        outcome_unknown_cloids: vec![],
                        note: "completed".into(),
                        whole_run: Some(whole_run("2", "20", "80", Some("10"), 0)),
                    },
                ],
                status: RunStatus::Success,
                filled: "2",
                notional: "20",
                vwap: Some("10"),
                cap_remaining: "80",
                unresolved: 0,
            },
            Case {
                name: "partial",
                records: vec![
                    typed_header("partial-whole-run", 1, "2", "2", "100"),
                    prepared(1, partial, "2", "10"),
                    terminal(1, partial, "1", "10"),
                    JournalRecord::FinalReport {
                        completed: false,
                        filled_total: "1".into(),
                        outcome_unknown_cloids: vec![],
                        note: "partial".into(),
                        whole_run: Some(whole_run("2", "10", "90", Some("10"), 0)),
                    },
                ],
                status: RunStatus::Incomplete,
                filled: "1",
                notional: "10",
                vwap: Some("10"),
                cap_remaining: "90",
                unresolved: 0,
            },
            Case {
                name: "abort with unresolved order",
                records: vec![
                    typed_header("abort-whole-run", 1, "2", "2", "100"),
                    prepared(1, aborted, "2", "10"),
                    JournalRecord::SubmittedUnknown {
                        slice_idx: 1,
                        cloid: aborted,
                    },
                    JournalRecord::FinalReport {
                        completed: false,
                        filled_total: "0".into(),
                        outcome_unknown_cloids: vec![aborted],
                        note: "aborted".into(),
                        whole_run: Some(whole_run("2", "0", "100", None, 1)),
                    },
                ],
                status: RunStatus::Incomplete,
                filled: "0",
                notional: "0",
                vwap: None,
                cap_remaining: "100",
                unresolved: 1,
            },
            Case {
                name: "cap exhausted",
                records: vec![
                    typed_header("cap-whole-run", 1, "2", "2", "10"),
                    prepared(1, capped, "1", "10"),
                    terminal(1, capped, "1", "10"),
                    JournalRecord::FinalReport {
                        completed: false,
                        filled_total: "1".into(),
                        outcome_unknown_cloids: vec![],
                        note: "cap exhausted".into(),
                        whole_run: Some(whole_run("2", "10", "0", Some("10"), 0)),
                    },
                ],
                status: RunStatus::Incomplete,
                filled: "1",
                notional: "10",
                vwap: Some("10"),
                cap_remaining: "0",
                unresolved: 0,
            },
            Case {
                name: "resumed already complete",
                records: vec![
                    typed_header("resumed-whole-run", 2, "1", "2", "100"),
                    prepared(1, resumed_first, "1", "10"),
                    terminal(1, resumed_first, "1", "10"),
                    JournalRecord::FinalReport {
                        completed: false,
                        filled_total: "1".into(),
                        outcome_unknown_cloids: vec![],
                        note: "first process stopped".into(),
                        whole_run: Some(whole_run("2", "10", "90", Some("10"), 0)),
                    },
                    prepared(2, resumed_second, "1", "20"),
                    terminal(2, resumed_second, "1", "20"),
                    JournalRecord::FinalReport {
                        completed: true,
                        filled_total: "2".into(),
                        outcome_unknown_cloids: vec![],
                        note: "resume completed".into(),
                        whole_run: Some(whole_run("2", "30", "70", Some("15"), 0)),
                    },
                ],
                status: RunStatus::Success,
                filled: "2",
                notional: "30",
                vwap: Some("15"),
                cap_remaining: "70",
                unresolved: 0,
            },
        ];

        for case in cases {
            let report = from_records(case.name, Path::new("x"), &case.records);
            assert_eq!(report.status, case.status, "{}", case.name);
            assert_eq!(
                report.validation.classification,
                ValidationClassification::Valid,
                "{}",
                case.name
            );
            let accounting = report.accounting.expect("valid replay has accounting");
            assert_eq!(accounting.filled_size, case.filled, "{}", case.name);
            assert_eq!(
                accounting.accounted_notional, case.notional,
                "{}",
                case.name
            );
            assert_eq!(
                accounting.trusted_vwap.as_deref(),
                case.vwap,
                "{}",
                case.name
            );
            assert_eq!(
                report.cap.remaining_notional.as_deref(),
                Some(case.cap_remaining),
                "{}",
                case.name
            );
            assert_eq!(report.unresolved.len(), case.unresolved, "{}", case.name);
        }
    }

    #[test]
    fn success_fixture_is_pure_json_and_secret_safe() {
        let report = from_records(
            "success",
            Path::new("/state/runs/success/journal.jsonl"),
            &[
                header("success"),
                JournalRecord::FinalReport {
                    completed: true,
                    filled_total: "0".into(),
                    outcome_unknown_cloids: vec![],
                    note: "potential-secret-that-must-not-appear".into(),
                    whole_run: None,
                },
            ],
        );
        let json = serde_json::to_string(&report).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["status"], "success");
        assert!(!json.contains("private_key"));
        assert!(!json.contains("HL_AGENT_PK"));
        assert!(!json.contains("potential-secret-that-must-not-appear"));
    }

    #[test]
    fn incomplete_and_abandoned_fixtures_are_classified() {
        let incomplete = from_records(
            "incomplete",
            Path::new("x"),
            &[
                header("incomplete"),
                JournalRecord::Prepared {
                    slice_idx: 0,
                    cloid: Default::default(),
                    nonce: None,
                    symbol: Symbol::from("ETH"),
                    side: Side::Long,
                    tif: None,
                    px: "10".into(),
                    sz: "1".into(),
                },
            ],
        );
        assert_eq!(incomplete.status, RunStatus::Incomplete);
        assert!(
            !incomplete.resume_eligible,
            "legacy journals without a typed fingerprint/deadline must not be advertised as resumable"
        );
        serde_json::from_str::<serde_json::Value>(&serde_json::to_string(&incomplete).unwrap())
            .unwrap();
        let abandoned = from_records(
            "abandoned",
            Path::new("x"),
            &[
                header("abandoned"),
                JournalRecord::Abandoned {
                    note: "operator action".into(),
                },
            ],
        );
        assert_eq!(abandoned.status, RunStatus::Abandoned);
        serde_json::from_str::<serde_json::Value>(&serde_json::to_string(&abandoned).unwrap())
            .unwrap();
    }

    #[test]
    fn verify_rejects_false_terminal_and_unreconciled_abandonment() {
        let cloid = Default::default();
        let prepared = JournalRecord::Prepared {
            slice_idx: 0,
            cloid,
            nonce: None,
            symbol: Symbol::from("ETH"),
            side: Side::Long,
            tif: None,
            px: "10".into(),
            sz: "1".into(),
        };
        let false_terminal = from_records(
            "false-terminal",
            Path::new("x"),
            &[
                header("false-terminal"),
                prepared.clone(),
                JournalRecord::Terminal {
                    slice_idx: 0,
                    cloid,
                    status: "open".into(),
                    filled_sz: "0".into(),
                    avg_px: None,
                },
            ],
        );
        assert_eq!(false_terminal.status, RunStatus::Corrupt);
        assert_eq!(
            false_terminal.validation.classification,
            ValidationClassification::InvalidTerminalStatus
        );

        let false_abandoned = from_records(
            "false-abandoned",
            Path::new("x"),
            &[
                header("false-abandoned"),
                prepared,
                JournalRecord::Abandoned {
                    note: "marker cannot replace reconciliation".into(),
                },
            ],
        );
        assert_eq!(false_abandoned.status, RunStatus::Corrupt);
        assert_eq!(
            false_abandoned.validation.classification,
            ValidationClassification::AbandonedWithUnresolved
        );
    }

    #[test]
    fn corrupt_fixture_has_typed_validation() {
        let report = from_records(
            "corrupt",
            Path::new("x"),
            &[JournalRecord::FinalReport {
                completed: true,
                filled_total: "0".into(),
                outcome_unknown_cloids: vec![],
                note: "bad".into(),
                whole_run: None,
            }],
        );
        assert_eq!(report.status, RunStatus::Corrupt);
        assert_eq!(
            report.validation.classification,
            ValidationClassification::HeaderNotFirst
        );
        serde_json::from_str::<serde_json::Value>(&serde_json::to_string(&report).unwrap())
            .unwrap();
    }

    #[test]
    fn invalid_run_id_is_typed_corrupt_without_path_traversal() {
        let report = inspect(Path::new("/state"), "../outside");
        assert_eq!(report.status, RunStatus::Corrupt);
        assert_eq!(
            report.validation.classification,
            ValidationClassification::InvalidRunId
        );
        assert!(!report.path.contains("../outside"));
    }

    #[test]
    fn negative_whole_run_cap_is_rejected_as_corrupt() {
        let report = from_records(
            "cap-clamp",
            Path::new("x"),
            &[
                header("cap-clamp"),
                JournalRecord::FinalReport {
                    completed: true,
                    filled_total: "0".into(),
                    outcome_unknown_cloids: vec![],
                    note: "done".into(),
                    whole_run: Some(crate::journal::WholeRunSummary {
                        requested_total: None,
                        adjusted_total: None,
                        accounted_notional: "100".into(),
                        cap_remaining: Some("-1".into()),
                        trusted_vwap: None,
                        logical_elapsed_ms: 1,
                        unresolved_cloids: 0,
                    }),
                },
            ],
        );
        assert_eq!(report.status, RunStatus::Corrupt);
        assert_eq!(
            report.validation.classification,
            ValidationClassification::AccountingError
        );
    }

    #[test]
    fn resume_eligibility_requires_typed_fingerprint_and_matching_deadline() {
        let mut header = match header("eligible") {
            JournalRecord::Header(header) => header,
            _ => unreachable!(),
        };
        header.execution_fingerprint = Some(ExecutionPlanFingerprint {
            version: ExecutionPlanFingerprint::VERSION,
            symbol: "ETH".into(),
            side: "long".into(),
            request_mode: "size".into(),
            request_value: "1".into(),
            per_slice: "1".into(),
            total_adjusted: "1".into(),
            total_requested: "1".into(),
            slices: 1,
            duration_ms: 1,
            slippage_bps: "1".into(),
            max_notional_usd: "100".into(),
            max_book_age_ms: 1,
            settle_retries: 1,
            child_algo: "market".into(),
            follow_poll_secs: 1,
            follow_repost_secs: 1,
            follow_threshold_bps: "1".into(),
            network: "testnet".into(),
            agent: None,
            master: None,
            position_mode: None,
            initial_position_szi: None,
            target_position_szi: None,
            position_requested_value: None,
            position_reference_price: None,
            position_phases: Vec::new(),
            reduce_only: false,
            absolute_deadline_unix_ms: Some(20),
        });
        assert!(resume_eligible(RunStatus::Incomplete, &header));
        let false_remaining = from_records(
            "false-remaining",
            Path::new("x"),
            &[
                JournalRecord::Header(header.clone()),
                JournalRecord::FinalReport {
                    completed: true,
                    filled_total: "0".into(),
                    outcome_unknown_cloids: vec![],
                    note: "done".into(),
                    whole_run: Some(crate::journal::WholeRunSummary {
                        requested_total: None,
                        adjusted_total: None,
                        accounted_notional: "0".into(),
                        cap_remaining: Some("0".into()),
                        trusted_vwap: None,
                        logical_elapsed_ms: 1,
                        unresolved_cloids: 0,
                    }),
                },
            ],
        );
        assert_eq!(false_remaining.status, RunStatus::Corrupt);
        assert_eq!(
            false_remaining.validation.classification,
            ValidationClassification::AccountingError
        );
        let mut invalid_max = header.clone();
        invalid_max
            .execution_fingerprint
            .as_mut()
            .expect("fingerprint set above")
            .max_notional_usd = "0".into();
        let invalid_max = from_records(
            "invalid-max",
            Path::new("x"),
            &[JournalRecord::Header(invalid_max)],
        );
        assert_eq!(invalid_max.status, RunStatus::Corrupt);
        assert_eq!(
            invalid_max.validation.classification,
            ValidationClassification::AccountingError
        );
        let mut unsupported_invalid_max = header.clone();
        let unsupported_fingerprint = unsupported_invalid_max
            .execution_fingerprint
            .as_mut()
            .expect("fingerprint set above");
        unsupported_fingerprint.version = ExecutionPlanFingerprint::VERSION + 1;
        unsupported_fingerprint.max_notional_usd = "not-a-number".into();
        let unsupported_invalid_max = from_records(
            "unsupported-invalid-max",
            Path::new("x"),
            &[JournalRecord::Header(unsupported_invalid_max)],
        );
        assert_eq!(unsupported_invalid_max.status, RunStatus::Corrupt);
        assert_eq!(
            unsupported_invalid_max.validation.classification,
            ValidationClassification::AccountingError
        );
        let report = from_records(
            "accounting-totals",
            Path::new("x"),
            &[
                JournalRecord::Header(header.clone()),
                JournalRecord::FinalReport {
                    completed: true,
                    filled_total: "0".into(),
                    outcome_unknown_cloids: vec![],
                    note: "done".into(),
                    // Legacy WholeRunSummary omitted sizing. The report must
                    // fall back to the immutable Header fingerprint.
                    whole_run: None,
                },
            ],
        );
        let accounting = report.accounting.expect("valid run has accounting");
        assert_eq!(accounting.requested_total.as_deref(), Some("1"));
        assert_eq!(accounting.adjusted_total.as_deref(), Some("1"));
        header
            .execution_fingerprint
            .as_mut()
            .unwrap()
            .absolute_deadline_unix_ms = Some(21);
        assert!(!resume_eligible(RunStatus::Incomplete, &header));
    }

    #[cfg(unix)]
    #[test]
    fn list_does_not_follow_symlinked_run_directory() {
        let root =
            std::env::temp_dir().join(format!("hype-twap-report-test-{}", uuid::Uuid::now_v7()));
        let target = root.join("outside");
        std::fs::create_dir_all(&target).unwrap();
        let runs = root.join("runs");
        std::fs::create_dir_all(&runs).unwrap();
        std::os::unix::fs::symlink(&target, runs.join("run-link")).unwrap();
        let report = list(&root).unwrap();
        assert!(report.runs.is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
