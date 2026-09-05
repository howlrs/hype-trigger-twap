//! `hype-twap` — trigger-gated TWAP execution for Hyperliquid perps.
//!
//! Startup sequence (§4):
//! 1. validate args
//! 2. `/info meta` → asset index + szDecimals (unknown symbol aborts before
//!    any order can be sent)
//! 3. build the signer (skipped in read-only) and verify HL_AGENT_ADDRESS
//! 4. `/info l2Book` → mid
//! 5. log the trigger condition
//! 6. wait for the trigger → pre-flight sizing → TWAP loop

use std::fs::{self, OpenOptions};
use std::future::Future;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, ValueEnum};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

use hype_trigger_twap::client::{HlClient, HlConfig, Network, Role, ValidatedMarketSnapshot};
use hype_trigger_twap::errors::HlError;
use hype_trigger_twap::format::human;
use hype_trigger_twap::observability::{
    ExecutionMode, JournalEventObserver, ObservabilityConfig, ObservabilityRuntime,
};
use hype_trigger_twap::position::{
    apply_signed_fill, is_between_frozen_endpoints, FlattenConfirmation, PositionExecutionPlan,
    PositionPhase, PositionPhaseKind,
};
use hype_trigger_twap::risk::{pre_send_summary, RiskEnvelope};
use hype_trigger_twap::signer::{Eip712AgentSigner, Signer};
use hype_trigger_twap::trigger::{
    wait_for_trigger, TriggerConfig, TriggerOutcome, TriggerReason, TriggerWhen,
};
use hype_trigger_twap::twap::{
    check_clock_skew, compute_sizing, fetch_fresh_book, usd_to_coin, wall_clock_now_ms, ChildAlgo,
    ShutdownSignal, TwapPlan, DEFAULT_SETTLE_RETRIES, MIN_NOTIONAL_USD, READ_ONLY_BANNER,
};
use hype_trigger_twap::types::SignedPerpPosition;
use hype_trigger_twap::types::{Address, Side, Symbol};

/// `--report-json -` reserves stdout for the final machine-readable report.
/// Tracing uses stderr by default; normal operator text in this module goes
/// through the two macros below and is suppressed in that mode.
static REPORT_JSON_STDOUT: AtomicBool = AtomicBool::new(false);

/// Release gate for new or resumed real-money mainnet order placement. The
/// funded testnet conformance checklist tracked in Issue #16 is not complete
/// yet, so a CLI typo or an omitted `--network testnet` must never silently
/// reach mainnet. Recovery-only `--abandon-incomplete-run` remains available
/// so pre-gate journals can be reconciled/cancelled without placing new orders.
/// Lifting the placement gate requires a reviewed source change.
const MAINNET_LIVE_ENABLED: bool = false;

macro_rules! println {
    ($($arg:tt)*) => {
        if !REPORT_JSON_STDOUT.load(Ordering::Relaxed) {
            ::std::println!($($arg)*);
        }
    };
}

macro_rules! print {
    ($($arg:tt)*) => {
        if !REPORT_JSON_STDOUT.load(Ordering::Relaxed) {
            ::std::print!($($arg)*);
        }
    };
}

/// Read and validate a resume journal before any external reconciliation.
/// This is deliberately separate from the full fingerprint check, which is
/// possible only after pre-flight sizing has resolved the plan.
fn validated_resume_replay(
    state_dir: &std::path::Path,
    run_id: &str,
    network: &Network,
    agent: Option<&Address>,
    configured_or_resolved_master: Option<&Address>,
    symbol: &Symbol,
    ordinary_side: Option<Side>,
) -> Result<hype_trigger_twap::journal::ValidatedJournalReplay, String> {
    let records = hype_trigger_twap::journal::ExecutionJournal::read_all(state_dir, run_id)
        .map_err(|e| format!("--resume {run_id}: failed to read journal: {e}"))?;
    let replay =
        hype_trigger_twap::journal::ValidatedJournalReplay::replay(&records).map_err(|e| {
            format!("--resume {run_id}: invalid journal; refusing external API calls: {e}")
        })?;
    let h = replay
        .summary
        .header
        .as_ref()
        .ok_or_else(|| format!("--resume {run_id}: journal has no Header"))?;
    let mut mismatch = Vec::new();
    if h.run_id != run_id {
        mismatch.push(format!("run_id (requested {run_id}, journal {})", h.run_id));
    }
    if h.network != network.to_string() {
        mismatch.push(format!(
            "network (expected {}, journal {})",
            network, h.network
        ));
    }
    if h.agent.as_ref() != agent {
        mismatch.push(format!(
            "agent (expected {:?}, journal {:?})",
            agent.map(Address::as_str),
            h.agent.as_ref().map(Address::as_str)
        ));
    }
    if configured_or_resolved_master.is_some() && h.master.as_ref() != configured_or_resolved_master
    {
        mismatch.push(format!(
            "master (expected {:?}, journal {:?})",
            configured_or_resolved_master.map(Address::as_str),
            h.master.as_ref().map(Address::as_str)
        ));
    }
    if &h.symbol != symbol {
        mismatch.push(format!(
            "symbol (expected {}, journal {})",
            symbol, h.symbol
        ));
    }
    if ordinary_side.is_some_and(|side| h.side != side) {
        mismatch.push(format!(
            "side (expected {}, journal {})",
            ordinary_side.unwrap_or(h.side),
            h.side
        ));
    }
    for (cloid, intent) in &replay.prepared {
        if intent.symbol != h.symbol {
            mismatch.push(format!(
                "Prepared symbol for cloid {cloid} (header {}, record {})",
                h.symbol, intent.symbol
            ));
        }
        if ordinary_side.is_some_and(|side| intent.side != side) {
            mismatch.push(format!(
                "Prepared side for cloid {cloid} (expected {}, record {})",
                ordinary_side.unwrap_or(h.side),
                intent.side
            ));
        }
    }
    if mismatch.is_empty() {
        Ok(replay)
    } else {
        Err(format!(
            "--resume {run_id}: journal identity mismatch before external API calls: {}",
            mismatch.join(", ")
        ))
    }
}

fn parse_public_address(label: &str, value: &str) -> Result<Address, String> {
    let value = value.trim();
    let valid = value.len() == 42
        && value.starts_with("0x")
        && value[2..].bytes().all(|byte| byte.is_ascii_hexdigit());
    if !valid {
        return Err(format!(
            "{label} must be a public 0x-prefixed 20-byte hex address"
        ));
    }
    Ok(Address::new(value.to_ascii_lowercase()))
}

/// Stable decimal spelling for journal identity. `rust_decimal::Display`
/// preserves input scale, so normalising first keeps equivalent CLI values
/// such as `1`, `1.0`, and `1.000` from producing false resume mismatches.
fn canonical_decimal(value: Decimal) -> String {
    value.normalize().to_string()
}

/// Remaining wall-clock window for a resumed logical run. A strictly
/// positive remainder is executable even when it is shorter than the
/// original slice interval; the TWAP loop compresses its continuation into
/// this window and still checks the same absolute deadline before every send.
/// At or after the boundary, callers may reconcile/cancel only.
fn remaining_execution_window(deadline_unix_ms: u64, now_unix_ms: u64) -> Option<Duration> {
    deadline_unix_ms
        .checked_sub(now_unix_ms)
        .filter(|remaining_ms| *remaining_ms > 0)
        .map(Duration::from_millis)
}

fn requested_mode_and_value(cli: &Cli) -> (&'static str, String) {
    if let Some(value) = cli.size {
        ("size", canonical_decimal(value))
    } else if let Some(value) = cli.usd {
        ("usd", canonical_decimal(value))
    } else if cli.flatten {
        ("flatten", "0".to_owned())
    } else if let Some(value) = cli.target_sz {
        ("target_sz", canonical_decimal(value))
    } else if let Some(value) = cli.target_usd {
        ("target_usd", canonical_decimal(value))
    } else {
        ("unknown", String::new())
    }
}

fn execution_sizing(
    total_coin: Decimal,
    slices: u32,
    sz_decimals: u32,
    mid: Decimal,
    require_exact_position_target: bool,
) -> Result<hype_trigger_twap::twap::Sizing, hype_trigger_twap::twap::PreflightError> {
    let mut sizing = compute_sizing(total_coin, slices, sz_decimals, mid)?;
    if require_exact_position_target {
        // Position phases start and end on the exchange size grid. Keep the
        // exact phase delta as the final cumulative target so the last slice
        // absorbs any per-slice truncation; otherwise flatten/target mode can
        // stop one or more ticks short and can never pass authoritative final
        // position verification.
        sizing.total_adjusted = total_coin;
    }
    Ok(sizing)
}

fn child_algo_name(value: ChildAlgoArg) -> &'static str {
    match value {
        ChildAlgoArg::Market => "market",
        ChildAlgoArg::Passive => "passive",
        ChildAlgoArg::Follow => "follow",
    }
}

fn phase_fingerprint(
    phase: &PositionPhase,
) -> hype_trigger_twap::journal::PositionPhaseFingerprint {
    hype_trigger_twap::journal::PositionPhaseFingerprint {
        kind: match phase.kind {
            PositionPhaseKind::Adjust => "adjust",
            PositionPhaseKind::CloseToFlat => "close_to_flat",
            PositionPhaseKind::OpenFromFlat => "open_from_flat",
        }
        .to_owned(),
        side: phase.side.to_string(),
        size: canonical_decimal(phase.size),
        reduce_only: phase.reduce_only,
    }
}

fn parse_canonical_fingerprint_decimal(
    run_id: &str,
    field: &'static str,
    value: &str,
) -> Result<Decimal, String> {
    let parsed: Decimal = value
        .parse()
        .map_err(|_| format!("--resume {run_id}: invalid stored {field}"))?;
    if canonical_decimal(parsed) != value {
        return Err(format!(
            "--resume {run_id}: stored {field} is not canonical; refusing new orders"
        ));
    }
    Ok(parsed)
}

/// Prove the durable sizing tuple is the deterministic result of one frozen
/// requested coin quantity. This prevents any one persisted sizing field from
/// silently becoming authoritative after journal corruption or manual edits.
fn validate_fingerprint_sizing(
    run_id: &str,
    fingerprint: &hype_trigger_twap::journal::ExecutionPlanFingerprint,
    expected_total_requested: Decimal,
    expected_reduce_only: bool,
    sz_decimals: u32,
    require_exact_position_target: bool,
) -> Result<(), String> {
    if fingerprint.slices == 0 {
        return Err(format!(
            "--resume {run_id}: stored slices must be positive; refusing new orders"
        ));
    }
    let per_slice =
        parse_canonical_fingerprint_decimal(run_id, "per_slice", &fingerprint.per_slice)?;
    let total_adjusted =
        parse_canonical_fingerprint_decimal(run_id, "total_adjusted", &fingerprint.total_adjusted)?;
    let total_requested = parse_canonical_fingerprint_decimal(
        run_id,
        "total_requested",
        &fingerprint.total_requested,
    )?;
    if expected_total_requested <= Decimal::ZERO
        || per_slice <= Decimal::ZERO
        || total_adjusted <= Decimal::ZERO
        || total_requested <= Decimal::ZERO
    {
        return Err(format!(
            "--resume {run_id}: stored sizing must be positive; refusing new orders"
        ));
    }

    let expected_per_slice = hype_trigger_twap::format::round_size(
        expected_total_requested / Decimal::from(fingerprint.slices),
        sz_decimals,
    );
    let expected_total_adjusted = if require_exact_position_target {
        expected_total_requested
    } else {
        expected_per_slice
            .checked_mul(Decimal::from(fingerprint.slices))
            .ok_or_else(|| format!("--resume {run_id}: stored sizing overflows"))?
    };
    let mut mismatches = Vec::new();
    if total_requested != expected_total_requested {
        mismatches.push("total_requested");
    }
    if per_slice != expected_per_slice {
        mismatches.push("per_slice");
    }
    if total_adjusted != expected_total_adjusted {
        mismatches.push("total_adjusted");
    }
    if fingerprint.reduce_only != expected_reduce_only {
        mismatches.push("reduce_only");
    }
    if !mismatches.is_empty() {
        return Err(format!(
            "--resume {run_id}: execution plan fingerprint mismatch in {}; refusing new orders",
            mismatches.join(", ")
        ));
    }
    Ok(())
}

/// Validate every execution-affecting CLI field after unresolved cloids were
/// reconciled but before a trigger/book/position request can lead to a new
/// order. Legacy fingerprints remain readable for reconciliation, but are
/// intentionally not sufficient authority for continuation.
fn validate_resume_execution_fingerprint(
    run_id: &str,
    replay: &hype_trigger_twap::journal::ValidatedJournalReplay,
    cli: &Cli,
    network: &Network,
    agent: Option<&Address>,
    master: Option<&Address>,
    sz_decimals: u32,
) -> Result<Option<PositionExecutionPlan>, String> {
    let header = replay
        .summary
        .header
        .as_ref()
        .ok_or_else(|| format!("--resume {run_id}: journal has no Header"))?;
    let Some(stored) = header.execution_fingerprint.as_ref() else {
        return Err(format!(
            "--resume {run_id}: legacy journal has no typed execution fingerprint; unresolved \
             orders were reconciled, but safely reconstructing new orders is impossible. \
             Inspect the run, then use --abandon-incomplete-run."
        ));
    };
    if stored.version != hype_trigger_twap::journal::ExecutionPlanFingerprint::VERSION {
        return Err(format!(
            "--resume {run_id}: fingerprint version {} is unsupported (current {}); unresolved \
             orders were reconciled, but no new order will be sent. Inspect the run, then use \
             --abandon-incomplete-run.",
            stored.version,
            hype_trigger_twap::journal::ExecutionPlanFingerprint::VERSION
        ));
    }
    let mut mismatches = Vec::new();
    macro_rules! expect_eq {
        ($name:literal, $actual:expr, $expected:expr) => {
            if $actual != $expected {
                mismatches.push($name);
            }
        };
    }
    let (request_mode, request_value) = requested_mode_and_value(cli);
    expect_eq!("request_mode", stored.request_mode.as_str(), request_mode);
    expect_eq!("request_value", stored.request_value, request_value);
    expect_eq!("network", stored.network, network.to_string());
    expect_eq!("agent", stored.agent.as_deref(), agent.map(Address::as_str));
    expect_eq!(
        "master",
        stored.master.as_deref(),
        master.map(Address::as_str)
    );
    expect_eq!("symbol", stored.symbol, header.symbol.as_str());
    expect_eq!("side", stored.side, header.side.to_string());
    expect_eq!("header.slices", stored.slices, header.slices);
    expect_eq!("slices", stored.slices, cli.slices);
    expect_eq!(
        "duration_ms",
        stored.duration_ms,
        cli.duration.as_millis().try_into().unwrap_or(u64::MAX)
    );
    expect_eq!(
        "slippage_bps",
        stored.slippage_bps,
        canonical_decimal(cli.slippage_bps)
    );
    expect_eq!(
        "max_notional_usd",
        stored.max_notional_usd,
        canonical_decimal(cli.max_notional_usd.unwrap_or(Decimal::MAX))
    );
    expect_eq!(
        "max_book_age_ms",
        stored.max_book_age_ms,
        cli.max_book_age_ms
    );
    expect_eq!("settle_retries", stored.settle_retries, cli.settle_retries);
    expect_eq!(
        "child_algo",
        stored.child_algo.as_str(),
        child_algo_name(cli.child_algo)
    );
    expect_eq!(
        "follow_poll_secs",
        stored.follow_poll_secs,
        cli.follow_poll_secs
    );
    expect_eq!(
        "follow_repost_secs",
        stored.follow_repost_secs,
        cli.follow_repost_secs
    );
    expect_eq!(
        "follow_threshold_bps",
        stored.follow_threshold_bps,
        canonical_decimal(cli.follow_threshold_bps)
    );
    expect_eq!(
        "absolute_deadline_unix_ms",
        stored.absolute_deadline_unix_ms,
        header.execution_deadline_unix_ms
    );
    if let Some(requested_deadline) = cli.flatten_deadline_unix_ms {
        if Some(requested_deadline) != header.execution_deadline_unix_ms {
            mismatches.push("flatten_deadline_unix_ms");
        }
    }
    if !mismatches.is_empty() {
        return Err(format!(
            "--resume {run_id}: execution plan fingerprint mismatch in {}; refusing new orders",
            mismatches.join(", ")
        ));
    }
    let position_mode = stored.position_mode.as_deref();
    if position_mode.is_none() {
        if cli.flatten || cli.target_sz.is_some() || cli.target_usd.is_some() {
            return Err(format!(
                "--resume {run_id}: ordinary journal cannot be resumed as a position target"
            ));
        }
        if stored.initial_position_szi.is_some()
            || stored.target_position_szi.is_some()
            || stored.position_requested_value.is_some()
            || stored.position_reference_price.is_some()
            || !stored.position_phases.is_empty()
        {
            return Err(format!(
                "--resume {run_id}: ordinary fingerprint contains position-only fields; refusing new orders"
            ));
        }
        let stored_total = parse_canonical_fingerprint_decimal(
            run_id,
            "total_requested",
            &stored.total_requested,
        )?;
        let expected_total = match request_mode {
            "size" => {
                parse_canonical_fingerprint_decimal(run_id, "request_value", &stored.request_value)?
            }
            // USD sizing was frozen against the original book. The raw USD
            // request is checked above; its durable coin quantity is then
            // checked for canonical, internally consistent rounding below.
            "usd" => stored_total,
            _ => {
                return Err(format!(
                    "--resume {run_id}: invalid ordinary request mode {request_mode}"
                ))
            }
        };
        validate_fingerprint_sizing(run_id, stored, expected_total, false, sz_decimals, false)?;
        return Ok(None);
    }
    if position_mode != Some(request_mode) {
        return Err(format!(
            "--resume {run_id}: stored position mode {:?} does not match --{request_mode}",
            position_mode.unwrap_or_default()
        ));
    }
    let initial_szi = parse_canonical_fingerprint_decimal(
        run_id,
        "initial_position_szi",
        stored
            .initial_position_szi
            .as_deref()
            .ok_or_else(|| format!("--resume {run_id}: fingerprint lacks initial_position_szi"))?,
    )?;
    let target_szi = parse_canonical_fingerprint_decimal(
        run_id,
        "target_position_szi",
        stored
            .target_position_szi
            .as_deref()
            .ok_or_else(|| format!("--resume {run_id}: fingerprint lacks target_position_szi"))?,
    )?;
    let initial = SignedPerpPosition {
        symbol: header.symbol.clone(),
        szi: initial_szi,
    };
    let frozen = match position_mode.unwrap_or_default() {
        "flatten" => PositionExecutionPlan::flatten(&initial, &header.symbol, sz_decimals),
        "target_sz" => PositionExecutionPlan::target_size(
            &initial,
            &header.symbol,
            cli.target_sz.ok_or_else(|| {
                format!("--resume {run_id}: stored target_sz requires --target-sz")
            })?,
            sz_decimals,
        ),
        "target_usd" => {
            let reference = parse_canonical_fingerprint_decimal(
                run_id,
                "position_reference_price",
                stored.position_reference_price.as_deref().ok_or_else(|| {
                    format!("--resume {run_id}: target_usd fingerprint lacks reference price")
                })?,
            )?;
            PositionExecutionPlan::target_usd(
                &initial,
                &header.symbol,
                cli.target_usd.ok_or_else(|| {
                    format!("--resume {run_id}: stored target_usd requires --target-usd")
                })?,
                reference,
                sz_decimals,
            )
        }
        other => {
            return Err(format!(
                "--resume {run_id}: unknown stored position mode {other}"
            ))
        }
    }
    .map_err(|e| format!("--resume {run_id}: invalid frozen position plan: {e}"))?;
    if frozen.target_szi != target_szi {
        return Err(format!(
            "--resume {run_id}: frozen target mismatch (fingerprint {target_szi}, reconstructed {})",
            frozen.target_szi
        ));
    }
    let phases: Vec<_> = frozen.phases.iter().map(phase_fingerprint).collect();
    if phases != stored.position_phases
        || frozen
            .phases
            .first()
            .is_some_and(|phase| phase.side != header.side)
    {
        return Err(format!(
            "--resume {run_id}: frozen position phase sequence is inconsistent; refusing new orders"
        ));
    }
    let expected_position_requested = match position_mode.unwrap_or_default() {
        "flatten" => None,
        _ => Some(request_value.as_str()),
    };
    if stored.position_requested_value.as_deref() != expected_position_requested {
        return Err(format!(
            "--resume {run_id}: execution plan fingerprint mismatch in position_requested_value; refusing new orders"
        ));
    }
    if position_mode != Some("target_usd") && stored.position_reference_price.is_some() {
        return Err(format!(
            "--resume {run_id}: execution plan fingerprint mismatch in position_reference_price; refusing new orders"
        ));
    }
    let first_phase = frozen.phases.first().ok_or_else(|| {
        format!("--resume {run_id}: stored position plan has no executable phase")
    })?;
    validate_fingerprint_sizing(
        run_id,
        stored,
        first_phase.size,
        first_phase.reduce_only,
        sz_decimals,
        true,
    )?;
    Ok(Some(frozen))
}

fn expected_position_from_replay(
    run_id: &str,
    replay: &hype_trigger_twap::journal::ValidatedJournalReplay,
    frozen: &PositionExecutionPlan,
) -> Result<Decimal, String> {
    let expected_side = if frozen.target_szi > frozen.current_szi {
        Some(Side::Long)
    } else if frozen.target_szi < frozen.current_szi {
        Some(Side::Short)
    } else {
        None
    };
    let mut expected = frozen.current_szi;
    for (cloid, state) in &replay.summary.cloids {
        let hype_trigger_twap::journal::CloidState::Terminal { filled_sz, .. } = state else {
            continue;
        };
        let intent = replay.prepared.get(cloid).ok_or_else(|| {
            format!("--resume {run_id}: terminal cloid {cloid} lacks validated Prepared intent")
        })?;
        if expected_side.is_some_and(|side| intent.side != side) {
            return Err(format!(
                "--resume {run_id}: durable cloid {cloid} moves away from the frozen target"
            ));
        }
        let filled: Decimal = filled_sz
            .parse()
            .map_err(|_| format!("--resume {run_id}: invalid terminal fill for {cloid}"))?;
        expected = apply_signed_fill(expected, intent.side, filled)
            .map_err(|e| format!("--resume {run_id}: cannot reconstruct position: {e}"))?;
        if !is_between_frozen_endpoints(expected, frozen.current_szi, frozen.target_szi) {
            return Err(format!(
                "--resume {run_id}: durable fills imply position {expected} beyond frozen target {}; refusing automatic reversal",
                frozen.target_szi
            ));
        }
    }
    Ok(expected)
}

/// Stable, secret-free operator preview of every value bound into a live
/// flatten confirmation token.  Keep field labels equal to the serialized
/// [`FlattenConfirmation`] fields so an operator can compare a read-only
/// preparation with the later live invocation without reverse-engineering a
/// hash.  This deliberately does not include the agent key or any endpoint.
fn format_flatten_confirmation_preflight(confirmation: &FlattenConfirmation) -> String {
    format!(
        "FLATTEN PREFLIGHT:\nnetwork: {}\nmaster: {}\nsymbol: {}\ninitial_szi: {}\nclose_side: {}\nmax_close_size: {}\nmax_notional_usd: {}\nchild_algo: {}\nexecution_deadline_unix_ms: {}",
        confirmation.network,
        confirmation.master,
        confirmation.symbol,
        canonical_decimal(confirmation.initial_szi),
        confirmation.close_side,
        canonical_decimal(confirmation.max_close_size),
        canonical_decimal(confirmation.max_notional_usd),
        confirmation.child_algo,
        confirmation.execution_deadline_unix_ms,
    )
}

fn execution_fingerprint(
    network: &Network,
    plan: &TwapPlan,
    position_plan: Option<&PositionExecutionPlan>,
    cli: &Cli,
    position_reference_price: Option<Decimal>,
) -> hype_trigger_twap::journal::ExecutionPlanFingerprint {
    let child_algo = match plan.child_algo {
        ChildAlgo::Market => "market",
        ChildAlgo::Passive => "passive",
        ChildAlgo::Follow => "follow",
    };
    let position_mode = if cli.flatten {
        Some("flatten".to_owned())
    } else if cli.target_sz.is_some() {
        Some("target_sz".to_owned())
    } else if cli.target_usd.is_some() {
        Some("target_usd".to_owned())
    } else {
        None
    };
    let position_requested_value = cli.target_sz.or(cli.target_usd).map(canonical_decimal);
    let position_phases = position_plan
        .map(|value| {
            value
                .phases
                .iter()
                .map(
                    |phase| hype_trigger_twap::journal::PositionPhaseFingerprint {
                        kind: match phase.kind {
                            PositionPhaseKind::Adjust => "adjust",
                            PositionPhaseKind::CloseToFlat => "close_to_flat",
                            PositionPhaseKind::OpenFromFlat => "open_from_flat",
                        }
                        .to_owned(),
                        side: phase.side.to_string(),
                        size: canonical_decimal(phase.size),
                        reduce_only: phase.reduce_only,
                    },
                )
                .collect()
        })
        .unwrap_or_default();
    let (request_mode, request_value) = if let Some(value) = cli.size {
        ("size", canonical_decimal(value))
    } else if let Some(value) = cli.usd {
        ("usd", canonical_decimal(value))
    } else if cli.flatten {
        ("flatten", "0".to_owned())
    } else if let Some(value) = cli.target_sz {
        ("target_sz", canonical_decimal(value))
    } else if let Some(value) = cli.target_usd {
        ("target_usd", canonical_decimal(value))
    } else {
        ("unknown", String::new())
    };
    hype_trigger_twap::journal::ExecutionPlanFingerprint {
        version: hype_trigger_twap::journal::ExecutionPlanFingerprint::VERSION,
        symbol: plan.symbol.as_str().to_owned(),
        side: plan.side.to_string(),
        request_mode: request_mode.to_owned(),
        request_value,
        per_slice: canonical_decimal(plan.per_slice),
        total_adjusted: canonical_decimal(plan.total_adjusted),
        total_requested: canonical_decimal(plan.total_requested),
        slices: plan.slices,
        duration_ms: plan.duration.as_millis().try_into().unwrap_or(u64::MAX),
        slippage_bps: canonical_decimal(plan.slippage_bps),
        max_notional_usd: canonical_decimal(plan.max_notional_usd),
        max_book_age_ms: plan.max_book_age_ms,
        settle_retries: plan.settle_retries,
        child_algo: child_algo.to_owned(),
        follow_poll_secs: plan.follow_poll_secs,
        follow_repost_secs: plan.follow_repost_secs,
        follow_threshold_bps: canonical_decimal(plan.follow_threshold_bps),
        network: network.to_string(),
        agent: plan.agent.as_ref().map(|a| a.as_str().to_owned()),
        master: plan.master.as_ref().map(|a| a.as_str().to_owned()),
        position_mode,
        initial_position_szi: position_plan.map(|value| canonical_decimal(value.current_szi)),
        target_position_szi: position_plan.map(|value| canonical_decimal(value.target_szi)),
        position_requested_value,
        position_reference_price: cli
            .target_usd
            .and(position_reference_price)
            .map(canonical_decimal),
        position_phases,
        reduce_only: plan.reduce_only,
        absolute_deadline_unix_ms: plan.absolute_deadline_unix_ms,
    }
}

/// F3: `long_about = None` makes clap drop the struct's doc comment, so the
/// environment contract would otherwise be invisible to `--help`. These are the
/// variables that decide whether the tool can trade at all, so they belong in
/// the help output rather than only in the README.
const ENV_HELP: &str = "\
ENVIRONMENT VARIABLES:
  HL_AGENT_PK         Required in live mode (`--live`; legacy
                      `--read-only false`). The AGENT (API wallet)
                      private key, `0x` + 64 hex. Accepted ONLY from the
                      environment — never as a flag — so it cannot reach shell
                      history or `ps` output. Never logged, not even on error.

  HL_AGENT_ADDRESS    Optional. The AGENT (API wallet) address — NOT the master
                      account. If set, it is checked against the address derived
                      from HL_AGENT_PK and startup fails on a mismatch.

  HL_MASTER_ADDRESS   The MASTER account the agent belongs to. Optional for a
                      new live run, which discovers it via `userRole`; required
                      for --resume/--abandon-incomplete-run so journal identity
                      is checked before any external API call. When supplied,
                      it must agree with the `userRole` response.

  HL_INFO_URL         Optional. Override the /info endpoint (testing). In LIVE
                      mode this is rejected by default (Issue #3) unless
                      --allow-custom-endpoints is also passed, and even then
                      only an https:// URL is accepted. A known official origin
                      for the opposite --network is always rejected.
  HL_EXCHANGE_URL     Optional. Override the /exchange endpoint (testing).
                      Same live-mode restriction as HL_INFO_URL above.
  RUST_LOG            Optional. Log filter; defaults to `info`.
";

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum SideArg {
    Long,
    Short,
}

impl From<SideArg> for Side {
    fn from(s: SideArg) -> Self {
        match s {
            SideArg::Long => Side::Long,
            SideArg::Short => Side::Short,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum WhenArg {
    Above,
    Below,
}

impl From<WhenArg> for TriggerWhen {
    fn from(w: WhenArg) -> Self {
        match w {
            WhenArg::Above => TriggerWhen::Above,
            WhenArg::Below => TriggerWhen::Below,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum NetworkArg {
    Mainnet,
    Testnet,
}

impl From<NetworkArg> for Network {
    fn from(n: NetworkArg) -> Self {
        match n {
            NetworkArg::Mainnet => Network::Mainnet,
            NetworkArg::Testnet => Network::Testnet,
        }
    }
}

/// Child-order algorithm for each slice (Issue #1).
///
/// `market` (default): unchanged pre-Issue-#1 behaviour — an IOC taker limit
/// at `mid +/- slippage-bps`.
///
/// `passive`: a post-only (ALO) limit resting at the best bid (long) / best
/// ask (short), waiting the full slice interval. See docs/DESIGN.md for the
/// cancel-then-settle reconciliation this mode performs at each slice
/// boundary and the in-flight-order cap that bounds it to at most one
/// resting child order at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
enum ChildAlgoArg {
    #[default]
    Market,
    Passive,
    Follow,
}

impl From<ChildAlgoArg> for ChildAlgo {
    fn from(a: ChildAlgoArg) -> Self {
        match a {
            ChildAlgoArg::Market => ChildAlgo::Market,
            ChildAlgoArg::Passive => ChildAlgo::Passive,
            ChildAlgoArg::Follow => ChildAlgo::Follow,
        }
    }
}

/// Trigger-gated TWAP for Hyperliquid perps.
///
/// Waits for a price and/or time trigger, then works the requested quantity
/// into the market as evenly-spaced IOC (taker) slices.
///
/// The private key is read ONLY from the HL_AGENT_PK environment variable —
/// never from a flag — so it cannot land in shell history or a process list.
#[derive(Debug, Parser)]
#[command(name = "hype-twap", version, about, long_about = None, after_help = ENV_HELP)]
struct Cli {
    /// Perp symbol, e.g. HYPE. Validated against /info meta before anything
    /// is sent; an unknown symbol aborts immediately.
    #[arg(long)]
    symbol: String,

    /// Trade direction for ordinary `--size` / `--usd` execution. Position
    /// modes derive it from the signed current/target exposure instead.
    #[arg(long, value_enum)]
    side: Option<SideArg>,

    /// Quantity in coin units. Mutually exclusive with --usd.
    #[arg(long, conflicts_with_all = ["usd", "flatten", "target_sz", "target_usd"])]
    size: Option<Decimal>,

    /// Notional in USD. Converted to a coin quantity at the mid observed when
    /// the trigger fires, and FIXED from then on — if price moves during the
    /// window the executed notional will drift from this number. Mutually
    /// exclusive with --size.
    #[arg(long, conflicts_with_all = ["size", "flatten", "target_sz", "target_usd"])]
    usd: Option<Decimal>,

    /// Safely close this symbol's current perpetual position. The side and
    /// maximum close size are read from clearinghouseState; every child order
    /// is reduce-only. Live mode additionally requires --confirm-flatten.
    #[arg(long, conflicts_with_all = ["target_sz", "target_usd"])]
    flatten: bool,

    /// Operator token printed by the corresponding flatten preflight.
    #[arg(long, requires = "flatten")]
    confirm_flatten: Option<String>,

    /// Absolute Unix-ms deadline used to bind a live flatten confirmation.
    /// Supplying it makes a token reproducible across the read-only prepare
    /// and live confirm invocations.
    #[arg(long)]
    flatten_deadline_unix_ms: Option<u64>,

    /// Signed final perpetual size (positive long, negative short).
    #[arg(long, conflicts_with = "target_usd", allow_hyphen_values = true)]
    target_sz: Option<Decimal>,

    /// Signed final USD exposure. Converted once at the validated preflight
    /// mid and frozen as a conservatively rounded target size.
    #[arg(long, conflicts_with = "target_sz", allow_hyphen_values = true)]
    target_usd: Option<Decimal>,

    /// Render the preflight position plan as JSON (the default is a compact
    /// operator-readable plan). This never changes execution semantics.
    #[arg(long, default_value_t = false)]
    json: bool,

    /// Master account used for read-only position planning and required for
    /// live resume/abandon identity validation before any external API call.
    /// On a new live run, userRole remains authoritative and must agree with
    /// this value when supplied.
    #[arg(long)]
    master_address: Option<String>,

    /// Execution window, e.g. 30m, 2h.
    #[arg(long, value_parser = parse_duration)]
    duration: Duration,

    /// Number of slices; interval = duration / slices.
    #[arg(long, default_value_t = 10)]
    slices: u32,

    /// Price trigger threshold. Requires --trigger-when.
    #[arg(long, requires = "trigger_when")]
    trigger_price: Option<Decimal>,

    /// Fire when mid rises to/above (above) or falls to/below (below) the
    /// trigger price. Required with --trigger-price; never inferred.
    #[arg(long, value_enum, requires = "trigger_price")]
    trigger_when: Option<WhenArg>,

    /// Also fire after this much time elapses. Combined with --trigger-price
    /// the two are OR'd — whichever comes first wins. With neither set the
    /// run starts immediately.
    #[arg(long, value_parser = parse_duration)]
    start_after: Option<Duration>,

    /// Explicitly enable order submission. Live execution currently requires
    /// --network testnet while the mainnet conformance gate remains closed.
    /// The legacy `--read-only false` spelling remains accepted for 0.1.x.
    #[arg(long, default_value_t = false, conflicts_with = "read_only")]
    live: bool,

    /// Dry run. true (the DEFAULT) signs nothing and sends no orders; each
    /// slice prints the order it would have placed from the live book.
    /// `--read-only false` is a deprecated compatibility alias for --live.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    read_only: bool,

    /// Optional Prometheus listen address. If set, loopback addresses are
    /// accepted by default; a non-loopback bind additionally requires
    /// --allow-external-metrics.
    #[arg(long, env = "HL_METRICS_BIND")]
    metrics_bind: Option<SocketAddr>,

    /// Explicitly allow a metrics listener on a non-loopback interface.
    #[arg(long, env = "HL_ALLOW_EXTERNAL_METRICS", default_value_t = false)]
    allow_external_metrics: bool,

    /// Optional state-free JSONL event stream for a read-only simulation.
    /// Live runs always use their journal-adjacent events.jsonl sidecar.
    #[arg(long)]
    event_jsonl: Option<PathBuf>,

    /// Selects both the API endpoints and the EIP-712 Agent.source domain
    /// ("a" mainnet / "b" testnet) — the two can never disagree.
    #[arg(long, value_enum, default_value = "mainnet")]
    network: NetworkArg,

    /// Slippage cushion for the IOC limit price, in basis points. Rejected
    /// unconditionally at or above 10000 bps or if it produces a non-positive
    /// limit price; above 1000 bps requires --allow-high-slippage (Issue #3).
    #[arg(long, default_value = "20")]
    slippage_bps: Decimal,

    /// Unsafe override: allow --slippage-bps above the 1000 bps warn
    /// threshold. Has NO effect on the unconditional >= 10000 bps hard cap or
    /// the non-positive-limit-price rejection — neither can be overridden.
    #[arg(long, default_value_t = false)]
    allow_high_slippage: bool,

    /// REQUIRED in live mode (`--live`; legacy `--read-only false`): the maximum cumulative
    /// USD notional for the entire logical run, including prior fills after
    /// `--resume`. `--usd` is checked as the requested notional; `--size` via
    /// a freshly computed conservative limit price. Before every order, the
    /// already-filled notional plus the exact (catch-up-aware) order size at
    /// its actual limit price is re-checked. Not required in read-only mode
    /// (Issue #3, breaking change for live users — see docs/USAGE.md).
    #[arg(long)]
    max_notional_usd: Option<Decimal>,

    /// Unsafe override: allow HL_INFO_URL / HL_EXCHANGE_URL to be overridden
    /// in LIVE mode. Requires the override URL(s) to be https://. A known
    /// official mainnet/testnet origin must still match --network. Read-only
    /// mode and tests are unaffected by this flag — the restriction it lifts
    /// only ever applies to live mode (Issue #3).
    #[arg(long, default_value_t = false)]
    allow_custom_endpoints: bool,

    /// Reject an l2Book snapshot older than this many ms. 0 disables the
    /// check. A negative age (HL clock ahead of ours) counts as fresh.
    #[arg(long, default_value_t = 3000)]
    max_book_age_ms: u64,

    /// Maximum orderStatus polls while settling a known resting child after
    /// cancellation.  Does not alter ambiguous-place reconciliation/resend.
    #[arg(long, env = "HL_SETTLE_RETRIES", default_value_t = DEFAULT_SETTLE_RETRIES)]
    settle_retries: u32,

    /// Seconds between l2Book polls while waiting for the trigger.
    #[arg(long, default_value_t = 2)]
    trigger_poll_secs: u64,

    /// How long a consecutive trigger-poll failure streak (network error or
    /// empty book) may run before the wait hard-stops. Timed from the first
    /// failure, not a retry count; resets on any single successful poll. The
    /// wait phase holds no position, so it can afford to ride out ordinary
    /// network blips instead of exiting after a handful of failed polls.
    #[arg(long, value_parser = parse_duration, default_value = "30m")]
    wait_network_grace: Duration,

    /// Terminate the wait, placing NOTHING, if no trigger condition fires
    /// within this duration (Issue #8). Not a fallback start like
    /// `--start-after` — an unmet expiry means the run ends without ever
    /// starting the TWAP. Unspecified = wait indefinitely (unchanged
    /// default). If a trigger condition and the expiry are both satisfied
    /// on the same evaluated tick, the trigger wins. Exits with code 3 and
    /// prints `EXPIRED: no trigger fired within <dur>` on expiry.
    #[arg(long, value_parser = parse_duration)]
    expire_after: Option<Duration>,

    /// Root directory for run-state persistence (Issue #4). Defaults to
    /// `$XDG_STATE_HOME/hype-twap` if set, else `~/.local/state/hype-twap`.
    /// A read-only run never touches this — no directory is created, no
    /// journal is written. Live runs write `<dir>/runs/<run-id>/journal.jsonl`.
    #[arg(long)]
    state_dir: Option<std::path::PathBuf>,

    /// Resume a specific incomplete run by its run id (Issue #4). Every
    /// submitted/unknown cloid recorded in that run's journal is reconciled
    /// via `orderStatus` BEFORE the run continues; fills already credited in
    /// the journal are never re-requested. Mutually exclusive with starting
    /// a brand-new run when an incomplete run already exists for the same
    /// network+agent — one of `--resume` or `--abandon-incomplete-run` is
    /// required in that situation.
    #[arg(long, conflicts_with = "abandon_incomplete_run")]
    resume: Option<String>,

    /// Explicit confirmation to abandon an incomplete run for this
    /// network+agent WITHOUT resuming it (Issue #4). Every submitted/unknown
    /// cloid is still force-reconciled via `orderStatus` first — this flag
    /// never skips reconciliation, it only accepts that whatever remainder
    /// was not yet executed will NOT be continued.
    #[arg(long, default_value_t = false)]
    abandon_incomplete_run: bool,

    /// How long SIGINT/SIGTERM shutdown may spend reconciling in-flight
    /// orders and cancelling confirmed resting ones before giving up
    /// (Issue #4). Exceeding this exits non-zero with any still-unresolved
    /// cloid journaled as `outcome_unknown`.
    #[arg(long, value_parser = parse_duration, default_value = "60s")]
    shutdown_grace: Duration,

    /// Write the final live journal report as schema-versioned JSON. Use `-`
    /// to reserve stdout for that JSON; any file target is atomically replaced.
    #[arg(long)]
    report_json: Option<PathBuf>,

    /// Child-order algorithm for each slice (Issue #1). `market` (default)
    /// reproduces pre-Issue-#1 behaviour exactly: an IOC taker limit at
    /// mid +/- slippage-bps. `passive` places a post-only (ALO) limit at the
    /// best bid (long) / best ask (short) and waits the full slice interval.
    /// `follow` is `passive` plus mid-slice re-quoting: it polls the book and
    /// re-quotes the resting order to keep following the touch until the
    /// slice ends (`--follow-poll-secs`/`--follow-repost-secs`/
    /// `--follow-threshold-bps` tune this; no taker fallback). See
    /// README/docs/DESIGN.md for the semantics.
    #[arg(long, value_enum, default_value = "market")]
    child_algo: ChildAlgoArg,

    /// `--child-algo follow` only: seconds between book polls inside a
    /// slice's follow loop. Ignored (with a warning) by other child algos.
    #[arg(long, default_value_t = 2)]
    follow_poll_secs: u64,

    /// `--child-algo follow` only: minimum seconds between reposts of the
    /// resting order within one slice (throttle, counted from that slice's
    /// last place). Ignored (with a warning) by other child algos.
    #[arg(long, default_value_t = 10)]
    follow_repost_secs: u64,

    /// `--child-algo follow` only: minimum relative distance (basis points)
    /// the touch must move AWAY from our resting price before a repost is
    /// worth burning queue priority for (hysteresis). Ignored (with a
    /// warning) by other child algos.
    #[arg(long, default_value = "1.0")]
    follow_threshold_bps: Decimal,

    /// Pair-launcher coordination only.  These four arguments are an
    /// all-or-nothing interface: after this process has completed every
    /// pre-flight step it atomically publishes readiness, then it waits for
    /// the launcher's authenticated common start timestamp before any order
    /// can be submitted.
    #[arg(long)]
    pair_ready_file: Option<PathBuf>,

    /// File written by the pair launcher with the common future start time.
    #[arg(long)]
    pair_start_file: Option<PathBuf>,

    /// Opaque pair-run identity, supplied by the pair launcher.
    #[arg(long)]
    pair_run_id: Option<String>,

    /// Maximum time to wait for the common start release.
    #[arg(long, value_parser = parse_duration)]
    pair_barrier_timeout: Option<Duration>,
}

#[derive(Debug, Clone)]
struct PairBarrier {
    ready_file: PathBuf,
    start_file: PathBuf,
    run_id: String,
    timeout: Duration,
}

#[derive(Debug, Serialize)]
struct PairReadyFile<'a> {
    run_id: &'a str,
    ready_at_unix_ms: u64,
    pid: u32,
    /// Live legs create their durable journal before entering the pair
    /// barrier.  Publish that exact run id so the launcher never has to
    /// guess by scanning a shared state root.  Read-only has no journal.
    #[serde(skip_serializing_if = "Option::is_none")]
    journal_run_id: Option<&'a str>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PairStartFile {
    run_id: String,
    start_at_unix_ms: u64,
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    humantime::parse_duration(s).map_err(|e| format!("invalid duration '{s}': {e}"))
}

fn valid_pair_run_id(run_id: &str) -> bool {
    !run_id.is_empty()
        && run_id.len() <= 128
        && run_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn validate_barrier_path(path: &Path, flag: &str) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(format!("{flag} must be an absolute path"));
    }
    let parent = path
        .parent()
        .ok_or_else(|| format!("{flag} must have a parent directory"))?;
    let metadata = fs::symlink_metadata(parent)
        .map_err(|e| format!("{flag} parent {} is unavailable: {e}", parent.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!(
            "{flag} parent {} must be a real directory (not a symlink)",
            parent.display()
        ));
    }
    Ok(())
}

impl Cli {
    fn pair_barrier(&self) -> Result<Option<PairBarrier>, String> {
        let supplied = [
            self.pair_ready_file.is_some(),
            self.pair_start_file.is_some(),
            self.pair_run_id.is_some(),
            self.pair_barrier_timeout.is_some(),
        ];
        if supplied.iter().any(|v| *v) && !supplied.iter().all(|v| *v) {
            return Err("--pair-ready-file, --pair-start-file, --pair-run-id, and --pair-barrier-timeout must be supplied together".into());
        }
        if !supplied.iter().all(|v| *v) {
            return Ok(None);
        }
        let (ready_file, start_file, run_id, timeout) = match (
            self.pair_ready_file.clone(),
            self.pair_start_file.clone(),
            self.pair_run_id.clone(),
            self.pair_barrier_timeout,
        ) {
            (Some(ready_file), Some(start_file), Some(run_id), Some(timeout)) => {
                (ready_file, start_file, run_id, timeout)
            }
            _ => return Err("internal pair barrier option validation error".into()),
        };
        if timeout.is_zero() {
            return Err("--pair-barrier-timeout must be > 0".into());
        }
        if !valid_pair_run_id(&run_id) {
            return Err("--pair-run-id must contain only ASCII letters, digits, '.', '_' or '-' (1..128 bytes)".into());
        }
        validate_barrier_path(&ready_file, "--pair-ready-file")?;
        validate_barrier_path(&start_file, "--pair-start-file")?;
        if ready_file == start_file {
            return Err("--pair-ready-file and --pair-start-file must differ".into());
        }
        if fs::symlink_metadata(&ready_file).is_ok() {
            return Err(format!(
                "--pair-ready-file already exists (refusing stale/reused barrier): {}",
                ready_file.display()
            ));
        }
        Ok(Some(PairBarrier {
            ready_file,
            start_file,
            run_id,
            timeout,
        }))
    }
}

fn write_pair_ready_file(
    barrier: &PairBarrier,
    journal_run_id: Option<&str>,
) -> Result<(), String> {
    let parent = barrier
        .ready_file
        .parent()
        .ok_or_else(|| "pair ready file has no parent directory".to_string())?;
    let temporary = parent.join(format!(
        ".pair-ready-{}-{}-{}.tmp",
        barrier.run_id,
        std::process::id(),
        uuid::Uuid::now_v7()
    ));
    let result = (|| -> Result<(), String> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|e| format!("creating pair ready file failed: {e}"))?;
        serde_json::to_writer(
            &mut file,
            &PairReadyFile {
                run_id: &barrier.run_id,
                ready_at_unix_ms: wall_clock_now_ms(),
                pid: std::process::id(),
                journal_run_id,
            },
        )
        .map_err(|e| format!("encoding pair ready file failed: {e}"))?;
        file.write_all(b"\n")
            .map_err(|e| format!("writing pair ready file failed: {e}"))?;
        file.sync_all()
            .map_err(|e| format!("syncing pair ready file failed: {e}"))?;
        // hard_link is create-only at the final name, unlike rename which
        // could silently replace a file supplied by another process.
        fs::hard_link(&temporary, &barrier.ready_file).map_err(|e| {
            format!("publishing pair ready file failed (it may already exist): {e}")
        })?;
        fs::File::open(parent)
            .and_then(|dir| dir.sync_all())
            .map_err(|e| format!("syncing pair ready directory failed: {e}"))?;
        Ok(())
    })();
    let _ = fs::remove_file(&temporary);
    result
}

async fn wait_for_pair_start(
    barrier: &PairBarrier,
    journal_run_id: Option<&str>,
) -> Result<(), String> {
    write_pair_ready_file(barrier, journal_run_id)?;
    let deadline = tokio::time::Instant::now() + barrier.timeout;
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "pair barrier timed out after {:?}; no order was placed",
                barrier.timeout
            ));
        }
        match fs::symlink_metadata(&barrier.start_file) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Err(
                        "pair start file must be a regular non-symlink file; no order was placed"
                            .into(),
                    );
                }
                let contents = fs::read_to_string(&barrier.start_file).map_err(|e| {
                    format!("reading pair start file failed; no order was placed: {e}")
                })?;
                let start: PairStartFile = serde_json::from_str(&contents)
                    .map_err(|e| format!("invalid pair start file; no order was placed: {e}"))?;
                if start.run_id != barrier.run_id {
                    return Err("pair start file run_id mismatch; no order was placed".into());
                }
                let now = wall_clock_now_ms();
                if start.start_at_unix_ms <= now {
                    return Err("pair start time is not in the future; no order was placed".into());
                }
                let wait = Duration::from_millis(start.start_at_unix_ms.saturating_sub(now));
                if tokio::time::Instant::now() + wait > deadline {
                    return Err(
                        "pair start time exceeds barrier timeout; no order was placed".into(),
                    );
                }
                tokio::time::sleep(wait).await;
                return Ok(());
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => {
                return Err(format!(
                    "checking pair start file failed; no order was placed: {e}"
                ))
            }
        }
    }
}

impl Cli {
    /// Resolve the two accepted spellings onto one execution mode. Clap
    /// rejects an explicit `--live --read-only ...` combination, while the
    /// default `read_only = true` does not conflict with `--live`.
    fn is_read_only(&self) -> bool {
        self.read_only && !self.live
    }

    fn is_live(&self) -> bool {
        !self.is_read_only()
    }

    /// §4 step 1: argument validation that clap cannot express.
    fn validate(&self) -> Result<(), String> {
        if self.is_live()
            && self.network == NetworkArg::Mainnet
            && !MAINNET_LIVE_ENABLED
            && !self.abandon_incomplete_run
        {
            let recovery_guidance = if self.resume.is_some() {
                " --resume cannot continue a mainnet run while the gate is closed; use --abandon-incomplete-run to reconcile/cancel its outstanding orders without placing new ones."
            } else {
                " Existing mainnet journals can still be reconciled and closed with --abandon-incomplete-run, which never places a new order."
            };
            return Err(
                format!(
                    "mainnet live execution is disabled pending the funded-testnet conformance checklist in Issue #16; use --network testnet for live execution or the default read-only mode for a mainnet rehearsal.{recovery_guidance}"
                ),
            );
        }
        if self.is_live() && self.event_jsonl.is_some() {
            return Err(
                "--event-jsonl is for read-only simulations; live runs use the run-directory events.jsonl sidecar"
                    .into(),
            );
        }
        let position_mode = self.flatten || self.target_sz.is_some() || self.target_usd.is_some();
        if position_mode {
            if self.size.is_some() || self.usd.is_some() {
                return Err("position modes cannot be combined with --size or --usd".into());
            }
            if self.side.is_some() {
                return Err(
                    "position modes derive side from current and target exposure; omit --side"
                        .into(),
                );
            }
        } else {
            if self.size.is_none() && self.usd.is_none() {
                return Err("exactly one of --size or --usd is required".into());
            }
            if self.side.is_none() {
                return Err("--side is required with --size or --usd".into());
            }
        }
        if let Some(sz) = self.size {
            if sz <= Decimal::ZERO {
                return Err(format!("--size must be > 0, got {sz}"));
            }
        }
        if let Some(usd) = self.usd {
            if usd <= Decimal::ZERO {
                return Err(format!("--usd must be > 0, got {usd}"));
            }
        }
        if self.duration.is_zero() {
            return Err("--duration must be > 0".into());
        }
        if self.slices == 0 {
            return Err("--slices must be > 0".into());
        }
        if self.settle_retries == 0 {
            return Err("--settle-retries must be > 0".into());
        }
        if self.flatten_deadline_unix_ms.is_some() && !self.flatten {
            return Err("--flatten-deadline-unix-ms requires --flatten".into());
        }
        // Issue #3: slippage bounds are enforced by the single risk-policy
        // module (src/risk.rs) — CLI validation and the twap.rs slice loop
        // both call into RiskEnvelope so the bounds can never drift apart.
        RiskEnvelope::validate_slippage(self.slippage_bps, self.allow_high_slippage)
            .map_err(|e| e.to_string())?;
        // Issue #3: live mode requires --max-notional-usd (breaking change,
        // documented in docs/USAGE.md / docs/OPERATIONS.md).
        RiskEnvelope::validate_max_notional_required(self.is_read_only(), self.max_notional_usd)
            .map_err(|e| e.to_string())?;
        if self.trigger_price.is_some() != self.trigger_when.is_some() {
            return Err("--trigger-price and --trigger-when must be given together".into());
        }
        if let Some(px) = self.trigger_price {
            if px <= Decimal::ZERO {
                return Err(format!("--trigger-price must be > 0, got {px}"));
            }
        }
        if self.trigger_poll_secs == 0 {
            return Err("--trigger-poll-secs must be > 0".into());
        }
        if self.wait_network_grace.is_zero() {
            return Err("--wait-network-grace must be > 0".into());
        }
        if let Some(expire_after) = self.expire_after {
            if expire_after.is_zero() {
                return Err("--expire-after must be > 0".into());
            }
            if let Some(start_after) = self.start_after {
                if expire_after <= start_after {
                    return Err(
                        "--expire-after must be greater than --start-after (start would always fire first, making expiry unreachable)".into(),
                    );
                }
            }
            if self.trigger_price.is_none() && self.start_after.is_none() {
                return Err(
                    "--expire-after requires a trigger (--trigger-price or --start-after); it is meaningless with immediate start".into(),
                );
            }
        }
        if self.follow_poll_secs == 0 {
            return Err("--follow-poll-secs must be > 0".into());
        }
        if self.follow_repost_secs == 0 {
            return Err("--follow-repost-secs must be > 0".into());
        }
        if self.follow_threshold_bps < Decimal::ZERO {
            return Err(format!(
                "--follow-threshold-bps must be >= 0, got {}",
                self.follow_threshold_bps
            ));
        }
        // The follow-* flags are meaningless for any other child-algo — warn
        // (not error) rather than reject, since a non-default value here is
        // far more likely to be a leftover flag from switching child-algos
        // than an operator mistake worth hard-failing on. Compared against
        // each flag's own `default_value`/`default_value_t`, so this only
        // fires when a value was actually given that differs from what a
        // `follow` run would silently assume anyway.
        if self.child_algo != ChildAlgoArg::Follow
            && (self.follow_poll_secs != 2
                || self.follow_repost_secs != 10
                || self.follow_threshold_bps != dec!(1.0))
        {
            tracing::warn!(
                "--follow-poll-secs/--follow-repost-secs/--follow-threshold-bps are ignored \
                 with --child-algo {:?} (only --child-algo follow uses them)",
                self.child_algo
            );
        }
        Ok(())
    }

    fn trigger_config(&self) -> TriggerConfig {
        TriggerConfig {
            price: match (self.trigger_price, self.trigger_when) {
                (Some(px), Some(w)) => Some((w.into(), px)),
                _ => None,
            },
            start_after: self.start_after,
            poll_interval: Duration::from_secs(self.trigger_poll_secs),
            max_book_age_ms: self.max_book_age_ms,
            wait_network_grace: self.wait_network_grace,
            expire_after: self.expire_after,
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        // stdout may be reserved for `--report-json -`; diagnostics must
        // never corrupt that single-document machine-readable channel.
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    match run().await {
        Ok(code) => code,
        Err(e) => {
            tracing::error!("{e}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<ExitCode, String> {
    run_with_cli(Cli::parse()).await
}

/// Race the complete live execution lifecycle against one grace window that
/// starts only after cooperative shutdown is requested.  Keeping this outside
/// the TWAP loop is intentional: position-aware execution has additional
/// close-to-flat and final-position reads after a phase returns, and those
/// reads must not outlive `--shutdown-grace` either.
async fn complete_before_shutdown_grace<T>(
    execution: impl Future<Output = T>,
    mut shutdown: ShutdownSignal,
    grace: Duration,
) -> Result<T, ()> {
    tokio::select! {
        output = execution => Ok(output),
        _ = async move {
            shutdown.wait().await;
            tokio::time::sleep(grace).await;
        } => Err(()),
    }
}

/// The body of `run()`, taking an already-parsed [`Cli`] rather than reading
/// `std::env::args()` itself. This split exists purely as a test seam: it
/// lets `#[cfg(test)]` drive a full pre-wait/post-trigger `run()` execution
/// against a mock HTTP server (via `Cli::try_parse_from` + `HL_INFO_URL`/
/// `HL_EXCHANGE_URL` env overrides, both of which `run()` already reads),
/// without needing `Cli::parse()` to read real process argv. `run()` itself
/// is unchanged in every other respect — same validation, same behavior.
async fn run_with_cli(cli: Cli) -> Result<ExitCode, String> {
    let report_to_stdout = cli
        .report_json
        .as_deref()
        .is_some_and(|path| path == Path::new("-"));
    REPORT_JSON_STDOUT.store(report_to_stdout, Ordering::Relaxed);
    cli.validate()?;
    let read_only = cli.is_read_only();
    if !cli.read_only {
        tracing::warn!("--read-only false is deprecated; use --live");
    }
    // Observability is never part of the trading critical path. The hook URL
    // is env-only so a bearer token cannot reach argv/history/ps. Any invalid
    // URL, listener bind, or hook setup simply disables observability while
    // preserving the same trading flow and exit status.
    let observability_config = ObservabilityConfig {
        metrics_bind: cli.metrics_bind,
        allow_external_metrics_bind: cli.allow_external_metrics,
        alert_hook_url: std::env::var("HL_ALERT_HOOK_URL").ok(),
        ..ObservabilityConfig::default()
    };
    let observability = match ObservabilityRuntime::start(&observability_config).await {
        Ok(runtime) => runtime,
        Err(_) => {
            tracing::warn!("observability setup failed; continuing with journal-only execution");
            ObservabilityRuntime::disabled()
        }
    };
    let result = async {
    if read_only && cli.report_json.is_some() {
        return Err("--report-json is available only in live mode (--live)".into());
    }
    // Resolve this before any network operation.  The actual ready publication
    // is deliberately deferred until all normal pre-flight/journal work is
    // complete below.
    let pair_barrier = cli.pair_barrier()?;

    let symbol = Symbol::new(&cli.symbol);
    // Position modes derive a side later from the signed position plan.  The
    // placeholder is never used to construct an order before it is replaced.
    let mut side: Side = cli.side.map(Into::into).unwrap_or(Side::Long);
    let network: Network = cli.network.into();
    let position_mode_requested =
        cli.flatten || cli.target_sz.is_some() || cli.target_usd.is_some();
    let configured_master_raw = cli
        .master_address
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            std::env::var("HL_MASTER_ADDRESS")
                .ok()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        });
    let configured_master = configured_master_raw
        .as_deref()
        .map(|value| parse_public_address("master address", value))
        .transpose()?;
    if !read_only
        && (cli.resume.is_some() || cli.abandon_incomplete_run)
        && configured_master.is_none()
    {
        return Err(
            "--resume and --abandon-incomplete-run require --master-address or HL_MASTER_ADDRESS so journal master identity can be verified before any external API call"
                .into(),
        );
    }

    if read_only {
        println!("{READ_ONLY_BANNER}");
    } else {
        tracing::warn!("LIVE MODE: orders WILL be sent to {network}");
    }

    // Issue #3: the RiskEnvelope is resolved HERE, immediately after CLI
    // validation and BEFORE any network access (including the endpoint
    // override check right below, and everything that follows). This is the
    // single point in the whole run where the risk policy is fixed for good —
    // later tasks that need to observe or extend the resolved risk config
    // (e.g. a run journal, or a passive/post-only build mode) should hook in
    // at this construction point rather than re-deriving it elsewhere.
    let risk = RiskEnvelope {
        slippage_bps: cli.slippage_bps,
        allow_high_slippage: cli.allow_high_slippage,
        max_notional_usd: cli.max_notional_usd,
    };

    // Issue #3: live + a custom HL_INFO_URL/HL_EXCHANGE_URL override is
    // rejected by default. This MUST run before any network access — it sits
    // ahead of the signer/client construction below, which is itself already
    // ahead of the first network call. Read-only is UNAFFECTED (the check
    // below is gated on live mode), which is what keeps the existing
    // mockito-based read-only test seam working unchanged.
    if !read_only {
        for url in [
            std::env::var("HL_INFO_URL").ok(),
            std::env::var("HL_EXCHANGE_URL").ok(),
        ]
        .into_iter()
        .flatten()
        {
            RiskEnvelope::validate_endpoint_override(&url, cli.allow_custom_endpoints)
                .map_err(|e| e.to_string())?;
            RiskEnvelope::validate_official_endpoint_network(&url, network)
                .map_err(|e| e.to_string())?;
        }
    }

    // §4 step 3 (partial): build the signer before any network call so a bad
    // key fails fast. Read-only never touches the key at all.
    let signer: Option<Box<dyn Signer>> = if read_only {
        None
    } else {
        let pk = std::env::var("HL_AGENT_PK").map_err(|_| {
            "HL_AGENT_PK is required in live mode (0x + 64 hex, env var only)".to_string()
        })?;
        let s = Eip712AgentSigner::from_secret(SecretString::new(pk.into()), network.is_mainnet())
            .map_err(|e| e.to_string())?;
        let derived = s.address();
        // HL_AGENT_ADDRESS is the AGENT's address (the API wallet), NOT the
        // master account. A mismatch means the wrong key is loaded.
        if let Ok(expected) = std::env::var("HL_AGENT_ADDRESS") {
            let expected = parse_public_address("HL_AGENT_ADDRESS", &expected)?;
            if expected != derived {
                return Err(format!(
                    "HL_AGENT_ADDRESS mismatch: env says {expected}, key derives {derived}. \
                     HL_AGENT_ADDRESS must be the AGENT (API wallet) address, not the master account."
                ));
            }
            tracing::info!(agent = %derived, "agent address verified against HL_AGENT_ADDRESS");
        } else {
            tracing::info!(agent = %derived, "agent address (HL_AGENT_ADDRESS not set; unverified)");
        }
        Some(Box::new(s))
    };
    let agent_address = signer.as_ref().map(|s| s.address());

    // Issue #4: state-dir resolution and incomplete-run detection. Read-only
    // never creates a directory or touches the journal at all — this whole
    // block is gated on live mode, mirroring the endpoint-override
    // gate above. Live mode checks BEFORE any network call (fetch_meta is
    // the very next one below), so a blocked startup never wastes a request.
    let resolved_state_dir = hype_trigger_twap::journal::state_dir(cli.state_dir.as_deref());

    // Issue #5: single-writer advisory lock, keyed by network+agent_address.
    // Taken HERE — immediately after `resolved_state_dir` is known and
    // strictly BEFORE the incomplete-run detection block below — so that two
    // concurrent live processes for the same network+agent can never both
    // run reconciliation/resume concurrently: the acceptance criterion
    // "stale lock recovery does not skip reconcile" depends on this
    // lock-then-reconcile ordering (see `src/lock.rs`'s test
    // `stale_lock_scenario_does_not_bypass_incomplete_run_reconciliation`
    // in this file's test module for the regression this guards).
    //
    // Read-only is completely unaffected (no lock file, no lock directory,
    // nothing written) — gated on live mode exactly like every other
    // live-only block in this function, preserving the "read-only creates
    // nothing" invariant `read_only_creates_no_state_dir_or_journal_file`
    // already covers for the journal.
    //
    // The lock is held for the rest of this function's scope (and the
    // entire live run, since `_process_lock` is not dropped until
    // `run_with_cli` returns) by keeping the guard bound in an outer `let`;
    // Task 9 #1 (passive/post-only) runs entirely inside this same
    // `run_with_cli` body and needs no changes here — one live process still
    // equals one lock holder regardless of order style.
    let _process_lock = if !read_only {
        let agent = agent_address.as_ref().ok_or_else(|| {
            "internal error: live mode must have an agent address by this point".to_string()
        })?;
        let lock_key = hype_trigger_twap::lock::lock_key(&network.to_string(), agent);
        let plan_summary = format!(
            "{symbol} {side:?} usd={} slices={} network={network}",
            cli.usd.map(|d| d.to_string()).unwrap_or_default(),
            cli.slices
        );
        let metadata = hype_trigger_twap::lock::LockMetadata::new(plan_summary);
        Some(
            hype_trigger_twap::lock::ProcessLock::acquire(
                &resolved_state_dir,
                &lock_key,
                &metadata,
            )
            .map_err(|e| e.to_string())?,
        )
    } else {
        None
    };

    // #27: a requested journal is parsed and its static identity is checked
    // before the first HTTP request (`meta` below). A position run has no
    // CLI side; its immutable Header/Prepared sides are validated internally
    // and the phase fingerprint is checked before any new order later.
    if !read_only {
        if let Some(resume_id) = &cli.resume {
            validated_resume_replay(
                &resolved_state_dir,
                resume_id,
                &network,
                agent_address.as_ref(),
                configured_master.as_ref(),
                &symbol,
                (!position_mode_requested).then_some(side),
            )?;
        }
    }

    if !read_only {
        let incomplete = hype_trigger_twap::journal::find_incomplete_run(
            &resolved_state_dir,
            network.to_string().as_str(),
            agent_address.as_ref(),
        )
        .map_err(|e| format!("checking for an incomplete prior run failed: {e}"))?;
        if let Some(incomplete_run_id) = incomplete {
            let is_the_one_being_resumed =
                cli.resume.as_deref() == Some(incomplete_run_id.as_str());
            if !is_the_one_being_resumed && !cli.abandon_incomplete_run {
                return Err(format!(
                    "an incomplete run ({incomplete_run_id}) exists for this network+agent \
                     (state dir: {}); refusing to start a new overlapping live run. \
                     Pass --resume {incomplete_run_id} to continue it, or \
                     --abandon-incomplete-run to force-reconcile and abandon it \
                     (nothing further from that run will be executed).",
                    resolved_state_dir.display()
                ));
            }
            if let Some(requested) = &cli.resume {
                if requested != &incomplete_run_id {
                    return Err(format!(
                        "--resume {requested} does not match the incomplete run on disk \
                         ({incomplete_run_id}); pass the correct run id, or \
                         --abandon-incomplete-run to abandon {incomplete_run_id} instead."
                    ));
                }
            }
            if cli.abandon_incomplete_run && cli.resume.is_none() {
                validated_resume_replay(
                    &resolved_state_dir,
                    &incomplete_run_id,
                    &network,
                    agent_address.as_ref(),
                    configured_master.as_ref(),
                    &symbol,
                    (!position_mode_requested).then_some(side),
                )?;
            }
        } else if cli.resume.is_some() {
            return Err(format!(
                "--resume {} was given but no incomplete run exists for this network+agent \
                 (state dir: {})",
                cli.resume.as_deref().unwrap_or_default(),
                resolved_state_dir.display()
            ));
        }
    }

    let config = HlConfig::new(network).with_overrides(
        std::env::var("HL_INFO_URL").ok(),
        std::env::var("HL_EXCHANGE_URL").ok(),
    );
    let client = HlClient::new(config, signer).map_err(|e| e.to_string())?;

    // Issue #5: seed the durable nonce high-water mark, right after
    // `HlClient::new` and strictly BEFORE the first `/exchange` call could
    // ever happen (the first network call at all is `fetch_meta` directly
    // below, which never mints a nonce, but seeding here — before ANY
    // network call — keeps the ordering simple to audit). Read-only never
    // seeds anything: it has no signer and never calls `next_nonce`, and
    // seeding would touch disk under the state dir, violating the
    // read-only-creates-nothing invariant this whole function's live-only
    // blocks already preserve.
    if !read_only {
        if let Some(agent) = agent_address.as_ref() {
            let key = hype_trigger_twap::lock::lock_key(&network.to_string(), agent);
            let hwm = hype_trigger_twap::lock::NonceHwm::load(&resolved_state_dir, &key)
                .map_err(|e| format!("failed to load nonce high-water mark: {e}"))?;
            client.seed_nonce(hwm);
        }
    }

    // §4 step 2: resolve the symbol. Unknown → abort with zero orders sent.
    let meta = client.fetch_meta().await.map_err(|e| e.to_string())?;
    let asset = meta.resolve(&symbol).map_err(|e| match e {
        HlError::UnknownSymbol(s) => format!(
            "unknown symbol '{s}' — not in the HL perp universe ({} symbols); nothing was sent",
            meta.universe.len()
        ),
        other => other.to_string(),
    })?;
    tracing::info!(
        symbol = %symbol,
        asset_index = asset.asset_index,
        sz_decimals = asset.sz_decimals,
        network = %network,
        "resolved symbol"
    );

    // §4 step 3 (rest), F1: resolve the MASTER account behind the agent key.
    //
    // HL books an agent's orders under its master, so every orderStatus query
    // must use the master address — with the agent address HL answers
    // `unknownOid` for orders that genuinely exist, and the fill-recovery path
    // would read that as "nothing filled". The probe doubles as a registration
    // check: an unregistered key is refused here, at startup, instead of
    // exploding on the first order.
    //
    // Read-only never probes: it places nothing, so it needs no master, and the
    // mode's contract is that it makes no calls a dry run does not require.
    let master: Option<Address> = match agent_address.as_ref() {
        // Read-only position planning has no agent key to probe.  It must
        // therefore name a public master explicitly (flag or environment),
        // rather than accidentally querying an agent address or an implicit
        // account.
        None => configured_master.clone(),
        Some(agent) => {
            let role = client
                .fetch_user_role(agent)
                .await
                .map_err(|e| format!("userRole probe for agent {agent} failed: {e}"))?;
            let master = match role {
                Role::Agent { master } => master,
                other => {
                    return Err(format!(
                        "agent {agent} is not registered with Hyperliquid as an Agent \
                         (role = {}). Authorize this address as an API wallet on the master \
                         account first, or set HL_AGENT_PK to a registered agent key. \
                         Nothing was sent.",
                        other.label()
                    ))
                }
            };
            // If the operator also declared the master, the two must agree —
            // a mismatch means the key belongs to a different account than
            // they think.
            if let Some(declared) = configured_master.as_ref() {
                if declared.as_str() != master.as_str().to_ascii_lowercase() {
                    return Err(format!(
                        "master address mismatch: configured {declared}, but HL reports agent \
                         {agent} belongs to master {master}. Nothing was sent."
                    ));
                }
            }
            tracing::info!(
                agent = %agent,
                master = %master,
                "userRole probe: agent registered; orderStatus will query the master"
            );
            Some(master)
        }
    };

    // Issue #4: resolve `--resume`/`--abandon-incomplete-run` reconciliation
    // HERE — immediately after `master` is known and strictly BEFORE the
    // trigger wait / any l2Book call below. Both flags force-reconcile every
    // submitted/unknown cloid from the PRIOR run via `orderStatus`; this
    // must happen before this process risks placing anything of its own
    // (resume) or before it declares itself done (abandon) — waiting until
    // after the trigger fires would let a long wait (or an immediate
    // trigger's sizing/pre-flight calls) run first, which is wrong for
    // abandon (nothing should happen at all) and unnecessarily late for
    // resume. Only `symbol`/`side`/`master` are needed for reconciliation
    // (`orderStatus` cross-check + the master-address query), so a minimal
    // plan fragment is built here rather than waiting for the full `TwapPlan`
    // (which needs sizing that has not happened yet).
    //
    // Ordering note: the `--resume` plan_hash consistency check (below, near
    // where the full `TwapPlan` and journal are opened) intentionally runs
    // AFTER this reconciliation, not before. Reconciliation itself never
    // risks a duplicate fill (it only queries `orderStatus`, never places),
    // so there is no safety reason to gate it on the plan matching — and
    // gating it the other way round would mean a `--resume` with a
    // mismatched plan leaves the prior run's cloids permanently unresolved
    // every time the operator retries with the wrong parameters. Running
    // reconciliation unconditionally first means the run's on-disk state
    // only ever gets MORE resolved, never less, regardless of which flags
    // the operator got wrong.
    let mut resume_observability_started = false;
    if !read_only && (cli.resume.is_some() || cli.abandon_incomplete_run) {
        // #27: identity and journal-state validation precede the first
        // `orderStatus` request.  A wrong run id must never be able to add a
        // reconciliation result to another account/network's journal.
        if let Some(resume_id) = &cli.resume {
            validated_resume_replay(
                &resolved_state_dir,
                resume_id,
                &network,
                agent_address.as_ref(),
                master.as_ref(),
                &symbol,
                (!position_mode_requested).then_some(side),
            )?;
        }
        let reconcile_plan = TwapPlan {
            symbol: symbol.clone(),
            side,
            // Metadata was resolved before this branch.  Resume may need to
            // cancel a still-live passive order, so the signed cancel must
            // carry the real asset index rather than a placeholder zero.
            asset_index: asset.asset_index,
            sz_decimals: asset.sz_decimals,
            per_slice: Decimal::ZERO,
            total_adjusted: Decimal::ZERO,
            total_requested: Decimal::ZERO,
            slices: 1,
            duration: Duration::ZERO,
            absolute_deadline_unix_ms: None,
            slippage_bps: cli.slippage_bps,
            max_book_age_ms: cli.max_book_age_ms,
            settle_retries: cli.settle_retries,
            read_only: false,
            reduce_only: false,
            max_notional_usd: Decimal::MAX,
            agent: agent_address.clone(),
            master: master.clone(),
            child_algo: cli.child_algo.into(),
            follow_poll_secs: cli.follow_poll_secs,
            follow_repost_secs: cli.follow_repost_secs,
            follow_threshold_bps: cli.follow_threshold_bps,
        };

        if let Some(resume_id) = &cli.resume {
            let mut j = hype_trigger_twap::journal::ExecutionJournal::open_existing(
                &resolved_state_dir,
                resume_id,
            )
            .map_err(|e| format!("--resume {resume_id}: failed to open journal: {e}"))?;
            let existing = hype_trigger_twap::journal::ExecutionJournal::read_all(
                &resolved_state_dir,
                resume_id,
            )
            .map_err(|e| format!("--resume {resume_id}: failed to seed observability: {e}"))?;
            let mut observer = live_journal_observer(&j, &observability);
            observer.seed_from_records(&existing, true);
            observer.emit_resume();
            j.set_observer(Box::new(observer));
            resume_observability_started = true;
            if let Err(error) = reconcile_incomplete_run(&client, &reconcile_plan, &mut j).await {
                let records = hype_trigger_twap::journal::ExecutionJournal::read_all(
                    &resolved_state_dir,
                    resume_id,
                )
                .map_err(|read_error| {
                    format!(
                        "--resume {resume_id}: reconciliation failed ({error}); reading its durable result also failed: {read_error}"
                    )
                })?;
                let replay = hype_trigger_twap::journal::ValidatedJournalReplay::replay(&records)
                    .map_err(|replay_error| {
                        format!(
                            "--resume {resume_id}: reconciliation failed ({error}); journal validation also failed: {replay_error}"
                        )
                    })?;
                j.record(&hype_trigger_twap::journal::JournalRecord::FinalReport {
                    completed: false,
                    filled_total: replay.fill_totals.filled_sz.to_string(),
                    outcome_unknown_cloids: replay.summary.unresolved_cloids(),
                    note: format!("resume reconciliation failed: {error}"),
                    whole_run: Some(whole_run_from_replay(&replay)),
                })
                .map_err(|record_error| {
                    format!(
                        "--resume {resume_id}: reconciliation failed ({error}); recording failure also failed: {record_error}"
                    )
                })?;
                emit_live_report_json(&cli, &resolved_state_dir, resume_id)?;
                return Err(format!(
                    "--resume {resume_id}: reconciliation failed: {error}"
                ));
            }
        } else if cli.abandon_incomplete_run {
            let incomplete_id = hype_trigger_twap::journal::find_incomplete_run(
                &resolved_state_dir,
                network.to_string().as_str(),
                agent_address.as_ref(),
            )
            .map_err(|e| e.to_string())?
            .ok_or_else(|| {
                "--abandon-incomplete-run given but no incomplete run was found".to_string()
            })?;
            validated_resume_replay(
                &resolved_state_dir,
                &incomplete_id,
                &network,
                agent_address.as_ref(),
                master.as_ref(),
                &symbol,
                (!position_mode_requested).then_some(side),
            )?;
            let mut j = hype_trigger_twap::journal::ExecutionJournal::open_existing(
                &resolved_state_dir,
                &incomplete_id,
            )
            .map_err(|e| format!("--abandon-incomplete-run: failed to open journal: {e}"))?;
            let existing = hype_trigger_twap::journal::ExecutionJournal::read_all(
                &resolved_state_dir,
                &incomplete_id,
            )
            .map_err(|e| format!("--abandon-incomplete-run: observability seed failed: {e}"))?;
            let mut observer = live_journal_observer(&j, &observability);
            observer.seed_from_records(&existing, true);
            j.set_observer(Box::new(observer));
            if let Err(error) = reconcile_incomplete_run(&client, &reconcile_plan, &mut j).await {
                let records = hype_trigger_twap::journal::ExecutionJournal::read_all(
                    &resolved_state_dir,
                    &incomplete_id,
                )
                .map_err(|read_error| {
                    format!(
                        "--abandon-incomplete-run: reconciliation failed ({error}); reading its durable result also failed: {read_error}"
                    )
                })?;
                let replay = hype_trigger_twap::journal::ValidatedJournalReplay::replay(&records)
                    .map_err(|replay_error| {
                        format!(
                            "--abandon-incomplete-run: reconciliation failed ({error}); journal validation also failed: {replay_error}"
                        )
                    })?;
                j.record(&hype_trigger_twap::journal::JournalRecord::FinalReport {
                    completed: false,
                    filled_total: replay.fill_totals.filled_sz.to_string(),
                    outcome_unknown_cloids: replay.summary.unresolved_cloids(),
                    note: format!("abandon reconciliation failed: {error}"),
                    whole_run: Some(whole_run_from_replay(&replay)),
                })
                .map_err(|record_error| {
                    format!(
                        "--abandon-incomplete-run: reconciliation failed ({error}); recording failure also failed: {record_error}"
                    )
                })?;
                emit_live_report_json(&cli, &resolved_state_dir, &incomplete_id)?;
                return Err(format!(
                    "--abandon-incomplete-run: reconciliation failed: {error}"
                ));
            }
            j.record(&hype_trigger_twap::journal::JournalRecord::Abandoned {
                note: format!(
                    "operator passed --abandon-incomplete-run; run {incomplete_id} force-\
                     reconciled and closed without continuing"
                ),
            })
            .map_err(|e| e.to_string())?;
            emit_live_report_json(&cli, &resolved_state_dir, &incomplete_id)?;
            println!(
                "Abandoned incomplete run {incomplete_id} after forced reconciliation; \
                 nothing further will be executed for it."
            );
            return Ok(ExitCode::SUCCESS);
        }
    }

    // Re-read after reconciliation: every subsequent accounting, deadline,
    // and position decision is made from this one validated state-machine
    // result. Unsupported/legacy plans are allowed to reconcile but are
    // stopped here, before trigger/book/position preflight can create a new
    // order intent.
    let resume_replay_after_reconcile = if !read_only {
        if let Some(resume_id) = &cli.resume {
            Some(validated_resume_replay(
                &resolved_state_dir,
                resume_id,
                &network,
                agent_address.as_ref(),
                master.as_ref(),
                &symbol,
                (!position_mode_requested).then_some(side),
            )?)
        } else {
            None
        }
    } else {
        None
    };
    let resumed_frozen_position = match (&cli.resume, resume_replay_after_reconcile.as_ref()) {
        (Some(resume_id), Some(replay)) => match validate_resume_execution_fingerprint(
            resume_id,
            replay,
            &cli,
            &network,
            agent_address.as_ref(),
            master.as_ref(),
            asset.sz_decimals,
        ) {
            Ok(position) => position,
            Err(error) => {
                let mut journal =
                    hype_trigger_twap::journal::ExecutionJournal::open_existing(
                        &resolved_state_dir,
                        resume_id,
                    )
                    .map_err(|open_error| {
                        format!(
                            "{error}; failed to reopen journal for durable refusal report: {open_error}"
                        )
                    })?;
                let records = hype_trigger_twap::journal::ExecutionJournal::read_all(
                    &resolved_state_dir,
                    resume_id,
                )
                .map_err(|read_error| {
                    format!("{error}; failed to read journal for refusal report: {read_error}")
                })?;
                let mut observer = live_journal_observer(&journal, &observability);
                observer.seed_from_records(&records, !resume_observability_started);
                if !resume_observability_started {
                    observer.emit_resume();
                }
                journal.set_observer(Box::new(observer));
                journal
                    .record(&hype_trigger_twap::journal::JournalRecord::FinalReport {
                        completed: false,
                        filled_total: replay.fill_totals.filled_sz.to_string(),
                        outcome_unknown_cloids: replay.summary.unresolved_cloids(),
                        note: format!("resume execution-plan validation failed: {error}"),
                        whole_run: Some(whole_run_from_replay(replay)),
                    })
                    .map_err(|record_error| {
                        format!("{error}; recording durable refusal also failed: {record_error}")
                    })?;
                emit_live_report_json(&cli, &resolved_state_dir, resume_id)?;
                return Err(error);
            }
        },
        _ => None,
    };

    // An expired logical deadline permits only the reconciliation above.  Do
    // this before trigger/pre-flight book access, so a late resume cannot
    // fetch a book or place a fresh child order.
    if !read_only {
        if let (Some(resume_id), Some(replay)) =
            (&cli.resume, resume_replay_after_reconcile.as_ref())
        {
            let header = replay
                .summary
                .header
                .as_ref()
                .ok_or_else(|| format!("--resume {resume_id}: journal has no Header"))?;
            let deadline = header.execution_deadline_unix_ms.ok_or_else(|| {
                format!("--resume {resume_id}: typed journal lacks absolute deadline")
            })?;
            if remaining_execution_window(deadline, wall_clock_now_ms()).is_none() {
                let unresolved = replay.summary.unresolved_cloids();
                let mut completed = unresolved.is_empty();
                let note;
                if let Some(frozen) = resumed_frozen_position.as_ref() {
                    let expected = expected_position_from_replay(resume_id, replay, frozen)?;
                    let master = master.as_ref().ok_or_else(|| {
                        "position resume lost resolved master address".to_string()
                    })?;
                    let actual = match client.fetch_perp_position(master, &symbol).await {
                        Ok(actual) => actual,
                        Err(error) => {
                            let mut journal =
                                hype_trigger_twap::journal::ExecutionJournal::open_existing(
                                    &resolved_state_dir,
                                    resume_id,
                                )
                                .map_err(|open_error| {
                                    format!(
                                        "--resume {resume_id}: deadline elapsed and final position could not be verified ({error}); journal reopen also failed: {open_error}"
                                    )
                                })?;
                            record_position_incomplete(
                                &resolved_state_dir,
                                &mut journal,
                                frozen.target_szi,
                                Err(error.to_string()),
                                "resume deadline elapsed; final position verification failed",
                            )?;
                            emit_live_report_json(&cli, &resolved_state_dir, resume_id)?;
                            return Err(format!(
                                "--resume {resume_id}: deadline elapsed and final position could not be verified: {error}"
                            ));
                        }
                    };
                    completed &= expected == frozen.target_szi && actual.szi == frozen.target_szi;
                    note = if completed {
                        "resume: deadline elapsed, reconciliation complete, and authoritative position matches frozen target".into()
                    } else {
                        format!(
                            "resume: deadline elapsed; no new order allowed; durable expected position {expected}, authoritative position {}, frozen target {}",
                            actual.szi, frozen.target_szi
                        )
                    };
                } else {
                    let adjusted_target: Decimal = header
                        .execution_fingerprint
                        .as_ref()
                        .ok_or_else(|| {
                            format!("--resume {resume_id}: expired journal lacks fingerprint")
                        })?
                        .total_adjusted
                        .parse()
                        .map_err(|_| {
                            format!(
                                "--resume {resume_id}: expired journal has invalid total_adjusted"
                            )
                        })?;
                    completed &= replay.fill_totals.filled_sz >= adjusted_target;
                    note = if completed {
                        "resume: deadline elapsed after durable fills already satisfied the original adjusted target".into()
                    } else {
                        format!(
                            "resume: original deadline elapsed; durable filled size {} is below adjusted target {adjusted_target}; no new order or book fetch allowed",
                            replay.fill_totals.filled_sz
                        )
                    };
                }
                let mut j = hype_trigger_twap::journal::ExecutionJournal::open_existing(
                    &resolved_state_dir,
                    resume_id,
                )
                .map_err(|e| format!("--resume {resume_id}: failed to open journal: {e}"))?;
                j.record(&hype_trigger_twap::journal::JournalRecord::FinalReport {
                    completed,
                    filled_total: replay.fill_totals.filled_sz.to_string(),
                    outcome_unknown_cloids: unresolved.clone(),
                    note,
                    whole_run: Some(whole_run_from_replay(replay)),
                })
                .map_err(|e| e.to_string())?;
                emit_live_report_json(&cli, &resolved_state_dir, resume_id)?;
                println!("TWAP resume stopped: original execution deadline elapsed; no new orders or book fetches were permitted.");
                return Ok(if completed {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                });
            }
        }
    }

    // §4 step 5 (moved ahead of step 4: the gate below needs to know whether
    // this run is time-only before it may touch l2Book at all).
    let trigger_cfg = cli.trigger_config();
    println!("{}", trigger_cfg.describe());

    // §4 step 4: initial mid, for the startup log.
    //
    // A time-only trigger (`--start-after` with no `--trigger-price`) must
    // NEVER call l2Book before its deadline (Issue #6) — there is nothing to
    // log a price for yet, and the pinned test
    // `time_only_trigger_fires_after_deadline_without_network` in
    // `trigger.rs` enforces the same contract on the wait loop itself.
    if !trigger_cfg.is_time_only() {
        let book = client
            .fetch_l2_book(&symbol)
            .await
            .map_err(|e| format!("initial l2Book: {e}"))?;
        let snapshot = ValidatedMarketSnapshot::validate(&book, &symbol, 0)
            .map_err(|e| format!("initial l2Book: {e}"))?;
        tracing::info!(symbol = %symbol, mid = %human(snapshot.mid), "initial mid");
    }

    // §4 step 6a: wait.
    let outcome = wait_for_trigger(&client, &symbol, &trigger_cfg)
        .await
        .map_err(|e| format!("trigger wait: {e}"))?;
    let reason = match outcome {
        TriggerOutcome::Fired(reason) => reason,
        TriggerOutcome::Expired(dur) => {
            // Issue #8: nothing was placed, no TwapReport — just the EXPIRED
            // line and exit code 3.
            println!(
                "EXPIRED: no trigger fired within {}",
                humantime::format_duration(dur)
            );
            return Ok(ExitCode::from(3));
        }
    };
    println!("Triggered: {reason}");

    // §8 pre-flight, F2: size against a mid that passed the SAME freshness gate
    // the slice loop uses. This snapshot fixes the coin quantity for the entire
    // run (and, with --usd, the notional too), so it is the single most
    // consequential price the tool reads — it must not be allowed to be the one
    // price that skips the staleness check.
    //
    // If the trigger fired on a price condition, the ALREADY-VALIDATED
    // snapshot that satisfied it is reused as-is rather than re-fetched: that
    // is the one and only meaning of "trigger-time mid" (Issue #6). Re-fetching
    // here would let a fresh, no-longer-crossing snapshot silently size an
    // order the trigger snapshot never actually justified.
    let snapshot = match &reason {
        TriggerReason::Price { snapshot, .. } => snapshot.clone(),
        TriggerReason::Immediate | TriggerReason::Elapsed { .. } => {
            fetch_fresh_book(&client, &symbol, cli.max_book_age_ms, None)
                .await
                .map_err(|e| format!("pre-flight l2Book: {e}"))?
        }
    };

    // Issue #2 (Finding 3): live-preflight clock-skew check, relocated to
    // execution entry. `expiresAfter` is a wall-clock Unix ms value trusted
    // by both the local `ExecutionDeadline` and Hyperliquid's own
    // exchange-side enforcement of the same field — if this host's clock is
    // skewed against HL's, the two enforcers disagree about when the run's
    // orders actually expire. Read-only / paper mode signs nothing, so a bad
    // local clock there is harmless; this check is therefore live-only (see
    // docs/DESIGN.md "クロックずれ").
    //
    // This check MUST run here — after the trigger has fired and a snapshot
    // is already in hand — rather than pre-wait: a pre-wait check would
    // require its own dedicated l2Book call, which a time-only trigger
    // (`--start-after` with no `--trigger-price`) must never make before its
    // deadline (Issue #6/#8; see `time_only_trigger_fires_after_deadline_without_network`
    // in `trigger.rs`). Reading `server_ts_ms` off the snapshot ALREADY
    // obtained above (whichever branch produced it) applies the same check
    // uniformly to both trigger modes, exactly once, with no extra l2Book
    // call of its own — it fails closed before ANY place happens.
    if !read_only {
        check_clock_skew(wall_clock_now_ms() as i64, snapshot.server_ts_ms)
            .map_err(|e| e.to_string())?;
    }

    let mid = snapshot.mid;

    // Position-aware modes take exactly one signed snapshot after market
    // metadata and the validated preflight price are known.  A malformed or
    // absent master is a hard stop before any order intent is constructed.
    // `target_usd` deliberately uses this one `mid` and freezes the resulting
    // size in `position_plan`; later slice prices never alter the target.
    let position_plan = if position_mode_requested {
        if symbol.as_str().contains(':') {
            return Err(
                "position modes are limited to standard Hyperliquid perpetual markets; HIP-3/deployer markets are not accepted"
                    .into(),
            );
        }
        let master = master.as_ref().ok_or_else(|| {
            "position modes require --master-address or HL_MASTER_ADDRESS in read-only mode; live mode resolves it from userRole".to_string()
        })?;
        let position = client
            .fetch_perp_position(master, &symbol)
            .await
            .map_err(|e| {
                format!("clearinghouseState position preflight failed; no order was sent: {e}")
            })?;
        let plan = if let (Some(resume_id), Some(frozen), Some(replay)) = (
            cli.resume.as_deref(),
            resumed_frozen_position.as_ref(),
            resume_replay_after_reconcile.as_ref(),
        ) {
            if !replay.summary.unresolved_cloids().is_empty() {
                let mut journal =
                    hype_trigger_twap::journal::ExecutionJournal::open_existing(
                        &resolved_state_dir,
                        resume_id,
                    )
                    .map_err(|error| {
                        format!("--resume {resume_id}: cannot record unresolved position stop: {error}")
                    })?;
                let records = hype_trigger_twap::journal::ExecutionJournal::read_all(
                    &resolved_state_dir,
                    resume_id,
                )
                .map_err(|error| format!("--resume {resume_id}: observability replay: {error}"))?;
                let mut observer = live_journal_observer(&journal, &observability);
                observer.seed_from_records(&records, !resume_observability_started);
                journal.set_observer(Box::new(observer));
                record_position_incomplete(
                    &resolved_state_dir,
                    &mut journal,
                    frozen.target_szi,
                    Ok(position.szi),
                    "position resume reconciliation left unresolved orders",
                )?;
                emit_live_report_json(&cli, &resolved_state_dir, resume_id)?;
                return Err(format!(
                    "--resume {resume_id}: reconciliation left unresolved cloids; no new order was sent"
                ));
            }
            let expected = expected_position_from_replay(resume_id, replay, frozen)?;
            if position.szi != expected {
                let mut journal =
                    hype_trigger_twap::journal::ExecutionJournal::open_existing(
                        &resolved_state_dir,
                        resume_id,
                    )
                    .map_err(|error| {
                        format!("--resume {resume_id}: cannot record position mismatch: {error}")
                    })?;
                let records = hype_trigger_twap::journal::ExecutionJournal::read_all(
                    &resolved_state_dir,
                    resume_id,
                )
                .map_err(|error| format!("--resume {resume_id}: observability replay: {error}"))?;
                let mut observer = live_journal_observer(&journal, &observability);
                observer.seed_from_records(&records, !resume_observability_started);
                journal.set_observer(Box::new(observer));
                record_position_incomplete(
                    &resolved_state_dir,
                    &mut journal,
                    frozen.target_szi,
                    Ok(position.szi),
                    "position resume authoritative state mismatches durable fills",
                )?;
                emit_live_report_json(&cli, &resolved_state_dir, resume_id)?;
                return Err(format!(
                    "--resume {resume_id}: current position {} does not match durable expected position {expected}; external/manual activity or missing fills detected, so no new order was sent",
                    position.szi
                ));
            }
            PositionExecutionPlan::target_size(
                &position,
                &symbol,
                frozen.target_szi,
                asset.sz_decimals,
            )
        } else if cli.flatten {
            PositionExecutionPlan::flatten(&position, &symbol, asset.sz_decimals)
        } else if let Some(target) = cli.target_sz {
            PositionExecutionPlan::target_size(&position, &symbol, target, asset.sz_decimals)
        } else if let Some(target) = cli.target_usd {
            PositionExecutionPlan::target_usd(&position, &symbol, target, mid, asset.sz_decimals)
        } else {
            unreachable!("position mode checked above")
        }
        .map_err(|e| format!("invalid position preflight; no order was sent: {e}"))?;

        if cli.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&plan).map_err(|e| e.to_string())?
            );
        } else {
            println!(
                "POSITION PLAN: current={} target={} symbol={} phases={:?}",
                human(plan.current_szi),
                human(plan.target_szi),
                plan.symbol,
                plan.phases
            );
        }
        if plan.is_noop() {
            println!(
                "POSITION PLAN: no-op (current exposure already equals target); no order was sent"
            );
            if let (Some(resume_id), Some(replay)) = (
                cli.resume.as_deref(),
                resume_replay_after_reconcile.as_ref(),
            ) {
                let mut journal = hype_trigger_twap::journal::ExecutionJournal::open_existing(
                    &resolved_state_dir,
                    resume_id,
                )
                .map_err(|e| format!("--resume {resume_id}: failed to open journal: {e}"))?;
                journal
                    .record(&hype_trigger_twap::journal::JournalRecord::FinalReport {
                        completed: true,
                        filled_total: replay.fill_totals.filled_sz.to_string(),
                        outcome_unknown_cloids: Vec::new(),
                        note: "resume: authoritative position already equals frozen target; no new order sent".into(),
                        whole_run: Some(whole_run_from_replay(replay)),
                })
                    .map_err(|e| format!("--resume {resume_id}: recording completion: {e}"))?;
                emit_live_report_json(&cli, &resolved_state_dir, resume_id)?;
            }
            emit_read_only_position_lifecycle(&cli, &observability, &plan, 0);
            return Ok(ExitCode::SUCCESS);
        }
        let first_phase_notional = plan.phases[0]
            .size
            .checked_mul(mid)
            .ok_or_else(|| "position delta notional overflow; no order was sent".to_string())?;
        if first_phase_notional < MIN_NOTIONAL_USD {
            if plan.crosses_zero() {
                return Err(format!(
                    "position reversal cannot safely reach flat first: close-to-flat notional {} is below minimum {}; no order was sent",
                    human(first_phase_notional),
                    human(MIN_NOTIONAL_USD)
                ));
            }
            println!(
                "POSITION PLAN: no-op (delta notional {} is below minimum {}); no order was sent",
                human(first_phase_notional),
                human(MIN_NOTIONAL_USD)
            );
            if let (Some(resume_id), Some(replay)) = (
                cli.resume.as_deref(),
                resume_replay_after_reconcile.as_ref(),
            ) {
                let mut journal = hype_trigger_twap::journal::ExecutionJournal::open_existing(
                    &resolved_state_dir,
                    resume_id,
                )
                .map_err(|e| format!("--resume {resume_id}: failed to open journal: {e}"))?;
                journal
                    .record(&hype_trigger_twap::journal::JournalRecord::FinalReport {
                        completed: false,
                        filled_total: replay.fill_totals.filled_sz.to_string(),
                        outcome_unknown_cloids: Vec::new(),
                        note: format!(
                            "resume: remaining position delta is below minimum notional {}; no order sent and frozen target not asserted complete",
                            MIN_NOTIONAL_USD
                        ),
                        whole_run: Some(whole_run_from_replay(replay)),
                })
                    .map_err(|e| format!("--resume {resume_id}: recording no-op: {e}"))?;
                emit_live_report_json(&cli, &resolved_state_dir, resume_id)?;
            }
            emit_read_only_position_lifecycle(&cli, &observability, &plan, 0);
            // A fresh target request may legitimately classify a sub-minimum
            // delta as a no-op (#43). A resumed logical run is different: its
            // durable report above remains explicitly incomplete and must not
            // be surfaced to automation as successful target convergence.
            return Ok(if cli.resume.is_some() {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            });
        }
        if cli.flatten {
            // A resumed flatten may have partially reduced the position, so
            // `plan` describes only the remaining delta.  The operator
            // confirmation must stay bound to the ORIGINAL frozen exposure,
            // maximum close size, and deadline recorded in the Header; using
            // the smaller continuation plan here would silently mint a new
            // token after every partial fill and, before this check existed,
            // `--flatten --resume` bypassed confirmation altogether.
            let confirmation_plan = resumed_frozen_position.as_ref().unwrap_or(&plan);
            let phase = confirmation_plan.phases.first().ok_or_else(|| {
                "flatten confirmation cannot be constructed without an executable close phase"
                    .to_string()
            })?;
            let deadline = if let (Some(resume_id), Some(replay)) =
                (cli.resume.as_deref(), resume_replay_after_reconcile.as_ref())
            {
                Some(
                    replay
                        .summary
                        .header
                        .as_ref()
                        .and_then(|header| header.execution_deadline_unix_ms)
                        .ok_or_else(|| {
                            format!(
                                "--resume {resume_id}: flatten journal lacks its original execution deadline"
                            )
                        })?,
                )
            } else {
                cli.flatten_deadline_unix_ms
            };
            if !read_only && deadline.is_none() {
                return Err("live --flatten requires --flatten-deadline-unix-ms so its confirmation token is stable across prepare/confirm".into());
            }
            if deadline.is_none() && !cli.json {
                println!("FLATTEN CONFIRMATION: set --flatten-deadline-unix-ms to print a reusable live confirmation token");
            }
            let Some(deadline) = deadline else {
                emit_read_only_position_lifecycle(&cli, &observability, &plan, 0);
                return Ok(ExitCode::SUCCESS);
            };
            let confirmation = FlattenConfirmation {
                schema_version: FlattenConfirmation::SCHEMA_VERSION,
                network: network.to_string(),
                master: master.clone(),
                symbol: symbol.clone(),
                initial_szi: confirmation_plan.current_szi,
                close_side: phase.side,
                max_close_size: phase.size,
                max_notional_usd: cli.max_notional_usd.unwrap_or(Decimal::ZERO),
                child_algo: child_algo_name(cli.child_algo).to_owned(),
                execution_deadline_unix_ms: deadline,
            };
            if confirmation.execution_deadline_unix_ms <= wall_clock_now_ms() {
                return Err(
                    "--flatten-deadline-unix-ms must be in the future; no order was sent".into(),
                );
            }
            let token = confirmation
                .token()
                .map_err(|e| format!("flatten confirmation encoding failed: {e}"))?;
            // `--json` reserves stdout for the position-plan JSON.  The
            // normal operator path prints the fully bound, secret-free
            // preflight BEFORE the opaque token; the `println!` wrapper also
            // suppresses both lines when `--report-json -` owns stdout.
            if !cli.json {
                println!("{}", format_flatten_confirmation_preflight(&confirmation));
                println!("FLATTEN CONFIRMATION: {token}");
            }
            if !read_only && cli.confirm_flatten.as_deref() != Some(token.as_str()) {
                return Err("live --flatten requires the exact --confirm-flatten token printed by this preflight; no order was sent".into());
            }
        }
        if read_only {
            let planned_slices = cli
                .slices
                .saturating_mul(u32::try_from(plan.phases.len()).unwrap_or(u32::MAX));
            emit_read_only_position_lifecycle(
                &cli,
                &observability,
                &plan,
                planned_slices,
            );
            println!("POSITION PLAN: read-only; no order or simulated fill was sent");
            return Ok(ExitCode::SUCCESS);
        }
        Some(plan)
    } else {
        None
    };

    let (total_coin, requested_desc) = if let Some(position_plan) = position_plan.as_ref() {
        let phase = &position_plan.phases[0];
        side = phase.side;
        (
            phase.size,
            format!(
                "position-aware: current {} → target {} ({} {:?}, reduce_only={})",
                human(position_plan.current_szi),
                human(position_plan.target_szi),
                human(phase.size),
                phase.kind,
                phase.reduce_only
            ),
        )
    } else {
        match (cli.size, cli.usd) {
            (Some(sz), _) => (sz, format!("{} {symbol}", human(sz))),
            (_, Some(usd)) => {
                let coin = usd_to_coin(usd, mid).map_err(|e| e.to_string())?;
                (
                    coin,
                    format!(
                        "${} → {} {symbol} at mid {}",
                        human(usd),
                        human(coin),
                        human(mid)
                    ),
                )
            }
            (None, None) => return Err("no size specified".into()),
        }
    };

    let sizing = execution_sizing(
        total_coin,
        cli.slices,
        asset.sz_decimals,
        mid,
        position_plan.is_some(),
    )
    .map_err(|e| e.to_string())?;
    println!(
        "Rounded per-slice: {} × {} = {} (~${} at mid {}) [requested {}]",
        human(sizing.per_slice),
        cli.slices,
        human(sizing.total_adjusted),
        human((sizing.total_adjusted * mid).round_dp(2)),
        human(mid),
        requested_desc
    );
    if sizing.total_adjusted < total_coin {
        tracing::warn!(
            dropped = %human(total_coin - sizing.total_adjusted),
            "rounding dropped a residual below one slice tick"
        );
    }
    tracing::info!(min_notional_usd = %human(MIN_NOTIONAL_USD), "per-slice min-notional gate");

    // Issue #3: resolve the notional cap (required in live, Decimal::MAX —
    // effectively unbounded — in read-only) and pre-flight-check the
    // requested notional against it, BEFORE any `/exchange` call. `--usd` is
    // checked as the requested notional directly; `--size` is checked via a
    // freshly computed CONSERVATIVE limit price (the same taker_limit_price
    // formula the slice loop uses, evaluated at this snapshot's touch) so a
    // size-denominated request cannot dodge the cap by omitting price
    // entirely. The slice loop re-checks this same cap before EVERY slice
    // against that slice's actual order price (src/twap.rs) — both call
    // sites share the one risk module (src/risk.rs), never duplicated
    // constants.
    let max_notional_usd =
        RiskEnvelope::validate_max_notional_required(read_only, risk.max_notional_usd)
            .map_err(|e| e.to_string())?;
    let preflight_notional = match (cli.size, cli.usd, position_plan.as_ref()) {
        (Some(_), _, _) => {
            let conservative_px = hype_trigger_twap::format::taker_limit_price(
                snapshot.best_bid,
                snapshot.best_ask,
                side,
                risk.slippage_bps,
                asset.sz_decimals,
            );
            RiskEnvelope::validate_limit_price(
                conservative_px,
                side,
                risk.slippage_bps,
                snapshot.best_bid,
                snapshot.best_ask,
            )
            .map_err(|e| e.to_string())?;
            total_coin * conservative_px
        }
        (_, Some(usd), _) => usd,
        (None, None, Some(_)) => {
            let conservative_px = hype_trigger_twap::format::taker_limit_price(
                snapshot.best_bid,
                snapshot.best_ask,
                side,
                risk.slippage_bps,
                asset.sz_decimals,
            );
            RiskEnvelope::validate_limit_price(
                conservative_px,
                side,
                risk.slippage_bps,
                snapshot.best_bid,
                snapshot.best_ask,
            )
            .map_err(|e| e.to_string())?;
            total_coin * conservative_px
        }
        (None, None, None) => return Err("no size specified".into()),
    };
    RiskEnvelope::check_notional_cap(preflight_notional, max_notional_usd)
        .map_err(|e| e.to_string())?;

    // Issue #3: one-shot pre-send summary, printed once before execution
    // begins.
    println!(
        "{}",
        pre_send_summary(
            network,
            &client.config().info_url,
            &client.config().exchange_url,
            symbol.as_str(),
            side,
            &requested_desc,
            risk.slippage_bps,
            if read_only {
                None
            } else {
                Some(max_notional_usd)
            },
        )
    );

    // Fix one absolute deadline for the logical run before the journal is
    // created/reopened. Every local send gate and wire `expiresAfter` uses
    // this exact value, including ordinary and position-aware resumes.
    let fixed_execution_deadline_unix_ms = if read_only {
        cli.flatten_deadline_unix_ms
    } else if let Some(replay) = resume_replay_after_reconcile.as_ref() {
        Some(
            replay
                .summary
                .header
                .as_ref()
                .and_then(|header| header.execution_deadline_unix_ms)
                .ok_or_else(|| {
                    "resume journal has no absolute execution deadline; reconciliation succeeded but new orders are refused"
                        .to_string()
                })?,
        )
    } else {
        Some(
            cli.flatten_deadline_unix_ms.unwrap_or_else(|| {
                wall_clock_now_ms().saturating_add(cli.duration.as_millis() as u64)
            }),
        )
    };

    // §8: the loop.
    let mut original_plan = TwapPlan {
        symbol: symbol.clone(),
        side,
        asset_index: asset.asset_index,
        sz_decimals: asset.sz_decimals,
        per_slice: sizing.per_slice,
        total_adjusted: sizing.total_adjusted,
        total_requested: total_coin,
        slices: cli.slices,
        duration: cli.duration,
        absolute_deadline_unix_ms: fixed_execution_deadline_unix_ms,
        slippage_bps: risk.slippage_bps,
        max_book_age_ms: cli.max_book_age_ms,
        settle_retries: cli.settle_retries,
        read_only,
        reduce_only: position_plan
            .as_ref()
            .and_then(|plan| plan.phases.first())
            .is_some_and(|phase| phase.reduce_only),
        max_notional_usd,
        agent: agent_address.clone(),
        master: master.clone(),
        child_algo: cli.child_algo.into(),
        follow_poll_secs: cli.follow_poll_secs,
        follow_repost_secs: cli.follow_repost_secs,
        follow_threshold_bps: cli.follow_threshold_bps,
    };

    // USD sizing normally depends on the current book.  A resumed logical
    // run must instead retain the durable original sizing, otherwise a price
    // move creates a false fingerprint mismatch (or, worse, reinterprets the
    // intended size). The current typed fingerprint is authoritative.
    if resumed_frozen_position.is_none() {
        if let Some(resume_id) = &cli.resume {
            let replay = validated_resume_replay(
                &resolved_state_dir,
                resume_id,
                &network,
                agent_address.as_ref(),
                master.as_ref(),
                &symbol,
                (!position_mode_requested).then_some(side),
            )?;
            let stored = replay
                .summary
                .header
                .as_ref()
                .and_then(|h| h.execution_fingerprint.as_ref());
            if let Some(stored) = stored {
                if stored.version != hype_trigger_twap::journal::ExecutionPlanFingerprint::VERSION {
                    return Err(format!("--resume {resume_id}: unsupported execution fingerprint version {}; refusing new orders", stored.version));
                }
                original_plan.per_slice = stored.per_slice.parse().map_err(|_| {
                    format!("--resume {resume_id}: invalid stored fingerprint per_slice")
                })?;
                original_plan.total_adjusted = stored.total_adjusted.parse().map_err(|_| {
                    format!("--resume {resume_id}: invalid stored fingerprint total_adjusted")
                })?;
                original_plan.total_requested = stored.total_requested.parse().map_err(|_| {
                    format!("--resume {resume_id}: invalid stored fingerprint total_requested")
                })?;
                original_plan.duration = Duration::from_millis(stored.duration_ms);
            }
        }
    }

    // Retain the compact legacy hash for older tooling. Typed resume safety
    // is enforced earlier by `validate_resume_execution_fingerprint`, which
    // checks every named field plus the sizing/position relationships.
    let plan_hash = hype_trigger_twap::journal::hash_plan_params(&[
        original_plan.symbol.as_str(),
        &original_plan.side.to_string(),
        &original_plan.per_slice.to_string(),
        &original_plan.total_adjusted.to_string(),
        &original_plan.slices.to_string(),
        &original_plan.duration.as_secs().to_string(),
        &original_plan.slippage_bps.to_string(),
        &original_plan.max_notional_usd.to_string(),
    ]);
    let fingerprint = if let Some(replay) = resume_replay_after_reconcile.as_ref() {
        replay
            .summary
            .header
            .as_ref()
            .and_then(|header| header.execution_fingerprint.clone())
            .ok_or_else(|| "resume journal lost its validated typed fingerprint".to_string())?
    } else {
        execution_fingerprint(
            &network,
            &original_plan,
            position_plan.as_ref(),
            &cli,
            Some(mid),
        )
    };

    // Read-only must remain state-directory free, but operators may opt into
    // the same schema-versioned event contract at an explicit standalone
    // path. This stream contains simulation lifecycle events only; it is not
    // a journal and never becomes resume authority.
    let mut read_only_observer = if read_only {
        cli.event_jsonl.as_ref().map(|path| {
            let mut observer = JournalEventObserver::open(
                path,
                ExecutionMode::ReadOnly,
                Arc::clone(&observability.metrics),
                observability.alerts.clone(),
            )
            .unwrap_or_else(|_| {
                tracing::warn!(
                    path = %path.display(),
                    "read-only event log unavailable; simulation continues without JSONL"
                );
                JournalEventObserver::without_event_log(
                    ExecutionMode::ReadOnly,
                    Arc::clone(&observability.metrics),
                    observability.alerts.clone(),
                )
            });
            observer.observe_record(&hype_trigger_twap::journal::JournalRecord::Header(
                hype_trigger_twap::journal::RunHeader {
                    run_id: uuid::Uuid::now_v7().to_string(),
                    network: network.to_string(),
                    agent: agent_address.clone(),
                    master: master.clone(),
                    symbol: original_plan.symbol.clone(),
                    side: original_plan.side,
                    slices: original_plan.slices,
                    plan_hash: plan_hash.clone(),
                    execution_fingerprint: Some(fingerprint.clone()),
                    started_at_unix_ms: wall_clock_now_ms(),
                    execution_deadline_unix_ms: fixed_execution_deadline_unix_ms,
                },
            ));
            observer.emit_preflight();
            observer
        })
    } else {
        None
    };

    // Issue #4: open (or resume) the journal for a LIVE run only — read-only
    // never creates the state dir or a journal file (mirrors the incomplete-
    // run-detection gate above). `--abandon-incomplete-run` already returned
    // above (right after reconciliation, before the trigger wait), so only
    // two cases remain here: `--resume` re-opens the ALREADY-reconciled
    // run's journal for append and continues it, or a brand-new run starts
    // a fresh journal.
    // Issue #4 Finding 1 fix: the amount already credited (via prior
    // Terminal journal records) by the run being resumed, replayed from the
    // ALREADY-reconciled journal below. `None` for a brand-new run (no prior
    // fills to subtract). Used after the journal is opened to compute a
    // continuation plan that targets only the remainder, so a `--resume`
    // never re-executes the full original plan on top of fills the prior
    // process already made.
    let mut already_filled: Option<Decimal> = None;
    let mut logical_execution_deadline_unix_ms: Option<u64> = None;
    // The notional counterpart to `already_filled`. Unlike the continuation
    // plan's size target, the risk envelope is scoped to the entire logical
    // run, so this offset must survive a process boundary on `--resume`.
    let mut prior_filled_notional = Decimal::ZERO;
    // The header is projected into the sidecar only after the journal exists;
    // it is retained here solely for that best-effort replay.
    let mut observability_header: Option<hype_trigger_twap::journal::RunHeader> = None;

    let mut journal = if read_only {
        None
    } else if let Some(resume_id) = &cli.resume {
        let replay = validated_resume_replay(
            &resolved_state_dir,
            resume_id,
            &network,
            agent_address.as_ref(),
            master.as_ref(),
            &symbol,
            (!position_mode_requested).then_some(side),
        )?;
        let header = replay
            .summary
            .header
            .as_ref()
            .ok_or_else(|| format!("--resume {resume_id}: journal has no Header"))?;
        observability_header = Some(header.clone());
        logical_execution_deadline_unix_ms =
            Some(header.execution_deadline_unix_ms.unwrap_or_else(|| {
                if header.started_at_unix_ms == 0 {
                    wall_clock_now_ms().saturating_add(original_plan.duration.as_millis() as u64)
                } else {
                    header
                        .started_at_unix_ms
                        .saturating_add(original_plan.duration.as_millis() as u64)
                }
            }));
        if header.execution_fingerprint.is_none() {
            return Err(format!(
                "--resume {resume_id}: legacy journal cannot safely reconstruct all execution fields; reconciliation completed, but new orders are refused. Inspect and use --abandon-incomplete-run"
            ));
        }
        let records =
            hype_trigger_twap::journal::ExecutionJournal::read_all(&resolved_state_dir, resume_id)
                .map_err(|e| format!("--resume {resume_id}: failed to read journal: {e}"))?;
        // Issue #4 Finding 1 fix: `records` here reflects the journal AFTER
        // forced reconciliation ran above (reconciliation reopened/appended
        // to the SAME file via `reconcile_incomplete_run`, which fsyncs
        // every record it writes) — so this replay already includes every
        // cloid's resolved Terminal outcome, not just what the prior
        // (crashed) process itself observed.
        let restored = hype_trigger_twap::journal::ValidatedJournalReplay::replay(&records)
            .map_err(|e| {
                format!(
                "--resume {resume_id}: failed to restore prior fill accounting from the journal: \
                 {e}; refusing to continue because the notional cap cannot be enforced safely"
            )
            })?
            .fill_totals;
        if resumed_frozen_position.is_none() {
            already_filled = Some(restored.filled_sz);
        }
        prior_filled_notional = restored.notional;
        Some(
            hype_trigger_twap::journal::ExecutionJournal::open_existing(
                &resolved_state_dir,
                resume_id,
            )
            .map_err(|e| format!("--resume {resume_id}: failed to re-open journal: {e}"))?,
        )
    } else {
        let run_id = uuid::Uuid::now_v7().to_string();
        let started_at_unix_ms = hype_trigger_twap::twap::wall_clock_now_ms();
        let execution_deadline_unix_ms =
            original_plan.absolute_deadline_unix_ms.unwrap_or_else(|| {
                started_at_unix_ms.saturating_add(original_plan.duration.as_millis() as u64)
            });
        if execution_deadline_unix_ms <= started_at_unix_ms {
            return Err(
                "execution deadline already elapsed before journal start; no order was sent".into(),
            );
        }
        logical_execution_deadline_unix_ms = Some(execution_deadline_unix_ms);
        let header = hype_trigger_twap::journal::RunHeader {
            run_id: run_id.clone(),
            network: network.to_string(),
            agent: agent_address.clone(),
            master: master.clone(),
            symbol: original_plan.symbol.clone(),
            side: original_plan.side,
            slices: original_plan.slices,
            plan_hash,
            execution_fingerprint: Some(fingerprint),
            started_at_unix_ms,
            execution_deadline_unix_ms: Some(execution_deadline_unix_ms),
        };
        observability_header = Some(header.clone());
        Some(
            hype_trigger_twap::journal::ExecutionJournal::start(
                &resolved_state_dir,
                run_id,
                header,
            )
            .map_err(|e| format!("failed to start execution journal: {e}"))?,
        )
    };

    // Attach only after `ExecutionJournal::start/open_existing` succeeded.
    // Failed event-log creation is intentionally a warning: journal records,
    // order placement, cancellation, resume, and exit status stay unchanged.
    if let (Some(j), Some(header)) = (journal.as_mut(), observability_header.as_ref()) {
        let mut observer = live_journal_observer(j, &observability);
        if cli.resume.is_some() {
            let records = hype_trigger_twap::journal::ExecutionJournal::read_all(
                &resolved_state_dir,
                j.run_id(),
            )
            .map_err(|error| format!("resume observability replay failed: {error}"))?;
            observer.seed_from_records(&records, !resume_observability_started);
            if !resume_observability_started {
                observer.emit_resume();
            }
        } else {
            observer.observe_record(&hype_trigger_twap::journal::JournalRecord::Header(
                header.clone(),
            ));
        }
        observer.emit_preflight();
        j.set_observer(Box::new(observer));
    }

    // Issue #4 Finding 1 fix: `--resume` must continue for the REMAINDER of
    // the original plan, never re-execute it from scratch — a slice already
    // journaled Terminal (a real fill) is excluded from
    // `unresolved_cloids()`, so the forced reconciliation above does not by
    // itself prevent double-placing it; only this continuation-plan
    // computation does.
    //
    // Captured before `original_plan` is potentially moved into `plan`
    // below (the no-resume arm), so the final report (after the run
    // completes) can still be rendered against the ORIGINAL target.
    let original_total_adjusted = original_plan.total_adjusted;
    let original_total_requested = original_plan.total_requested;
    //
    // Continuation-plan computation: `remaining = original_total_adjusted -
    // already_filled`. If the
    // remainder cannot clear the same per-slice min-notional gate the
    // original plan was sized against (using the same trigger-time `mid`),
    // there is nothing this process can legally place. A genuinely satisfied
    // target completes successfully; a positive but unplaceable remainder is
    // durably incomplete and exits non-zero. This mirrors
    // `PreflightError::PerSliceBelowMinNotional`'s gate (same
    // `MIN_NOTIONAL_USD` constant, no new threshold invented).
    let plan = match already_filled {
        None => original_plan,
        Some(filled) => {
            let remaining = original_plan.total_adjusted - filled;
            if remaining <= Decimal::ZERO {
                tracing::info!(
                    already_filled = %human(filled),
                    original_total = %human(original_plan.total_adjusted),
                    "--resume: prior fills already satisfy the original plan; nothing further will be executed"
                );
                let whole_run = if let Some(j) = journal.as_mut() {
                    let whole_run = replay_whole_run(&resolved_state_dir, j.run_id())
                    .map_err(|error| {
                        format!(
                            "--resume: prior fills satisfy the plan, but its durable whole-run accounting could not be validated: {error}"
                        )
                    })?;
                    j.record(&hype_trigger_twap::journal::JournalRecord::FinalReport {
                        completed: true,
                        filled_total: filled.to_string(),
                        outcome_unknown_cloids: Vec::new(),
                        note: "resume: prior fills already satisfy the original plan; nothing further executed"
                            .to_string(),
                        whole_run: Some(whole_run.clone()),
                    })
                    .map_err(|record_error| {
                        format!(
                            "--resume: prior fills satisfy the plan, but recording its durable completion failed: {record_error}"
                        )
                    })?;
                    Some(whole_run)
                } else {
                    None
                };
                let report = hype_trigger_twap::twap::TwapReport {
                    symbol: original_plan.symbol.clone(),
                    side: original_plan.side,
                    total_requested: original_plan.total_requested,
                    total_adjusted: original_plan.total_adjusted,
                    filled,
                    avg_px: whole_run
                        .as_ref()
                        .and_then(|whole| whole.trusted_vwap.as_deref())
                        .and_then(|value| value.parse().ok()),
                    slices_executed: 0,
                    slices_skipped: 0,
                    elapsed: whole_run
                        .as_ref()
                        .map(|whole| Duration::from_millis(whole.logical_elapsed_ms))
                        .unwrap_or(Duration::ZERO),
                    abort_reason: None,
                    read_only,
                };
                print!("{}", report.render());
                if let Some(j) = journal.as_ref() {
                    emit_live_report_json(&cli, &resolved_state_dir, j.run_id())?;
                }
                return Ok(ExitCode::SUCCESS);
            }
            if remaining * mid < MIN_NOTIONAL_USD {
                let reason = format!(
                    "resume: positive remainder {} is below minimum notional {}; no order sent and original target not asserted complete",
                    human(remaining),
                    human(MIN_NOTIONAL_USD)
                );
                tracing::warn!(
                    already_filled = %human(filled),
                    original_total = %human(original_plan.total_adjusted),
                    remaining = %human(remaining),
                    "{reason}"
                );
                let whole_run = if let Some(j) = journal.as_mut() {
                    let whole_run = replay_whole_run(&resolved_state_dir, j.run_id()).map_err(
                        |error| {
                            format!(
                                "--resume: remainder is below minimum notional, but durable whole-run accounting could not be validated: {error}"
                            )
                        },
                    )?;
                    j.record(&hype_trigger_twap::journal::JournalRecord::FinalReport {
                        completed: false,
                        filled_total: filled.to_string(),
                        outcome_unknown_cloids: Vec::new(),
                        note: reason.clone(),
                        whole_run: Some(whole_run.clone()),
                    })
                    .map_err(|record_error| {
                        format!(
                            "--resume: recording the below-minimum incomplete result failed: {record_error}"
                        )
                    })?;
                    Some(whole_run)
                } else {
                    None
                };
                let report = hype_trigger_twap::twap::TwapReport {
                    symbol: original_plan.symbol.clone(),
                    side: original_plan.side,
                    total_requested: original_plan.total_requested,
                    total_adjusted: original_plan.total_adjusted,
                    filled,
                    avg_px: whole_run
                        .as_ref()
                        .and_then(|whole| whole.trusted_vwap.as_deref())
                        .and_then(|value| value.parse().ok()),
                    slices_executed: 0,
                    slices_skipped: 0,
                    elapsed: whole_run
                        .as_ref()
                        .map(|whole| Duration::from_millis(whole.logical_elapsed_ms))
                        .unwrap_or(Duration::ZERO),
                    abort_reason: Some(reason),
                    read_only,
                };
                print!("{}", report.render());
                if let Some(j) = journal.as_ref() {
                    emit_live_report_json(&cli, &resolved_state_dir, j.run_id())?;
                }
                return Ok(ExitCode::FAILURE);
            }
            // Continuation plan: SAME per-slice size as the original plan
            // (preserving the schedule's granularity), slices = ceil(
            // remaining / per_slice) so the last slice absorbs whatever
            // remainder is smaller than a full per-slice size — the same
            // "final slice carries the residual" convention
            // `target_at_slice`/`slice_order_size` already use for a normal
            // (non-resumed) run. `total_adjusted` is set to exactly
            // `remaining` so `run_twap_journaled`'s own early-finish check
            // (`stats.filled >= plan.total_adjusted`) and min-notional gating
            // on the final slice both apply to the true remainder, not the
            // original total.
            let per_slice = original_plan.per_slice;
            let continuation_slices: u32 = {
                let whole = (remaining / per_slice).floor();
                let has_remainder = whole * per_slice < remaining;
                let n = whole.to_string().parse::<u32>().unwrap_or(0) + u32::from(has_remainder);
                n.max(1)
            };
            tracing::info!(
                already_filled = %human(filled),
                prior_filled_notional = %human(prior_filled_notional),
                original_total = %human(original_plan.total_adjusted),
                remaining = %human(remaining),
                continuation_slices,
                "--resume: continuing with a plan scoped to the remainder only"
            );
            let remaining_duration = remaining_execution_window(
                logical_execution_deadline_unix_ms
                    .ok_or_else(|| "live run has no execution deadline".to_string())?,
                wall_clock_now_ms(),
            )
            .unwrap_or(Duration::ZERO);
            TwapPlan {
                symbol: original_plan.symbol.clone(),
                side: original_plan.side,
                asset_index: original_plan.asset_index,
                sz_decimals: original_plan.sz_decimals,
                per_slice,
                total_adjusted: remaining,
                total_requested: original_plan.total_requested,
                slices: continuation_slices,
                // Preserve the original absolute wall-clock deadline across
                // process restarts.  `run_twap` turns this remaining window
                // into its local monotonic deadline and exchange expiry.
                duration: remaining_duration,
                absolute_deadline_unix_ms: original_plan.absolute_deadline_unix_ms,
                slippage_bps: original_plan.slippage_bps,
                max_book_age_ms: original_plan.max_book_age_ms,
                settle_retries: original_plan.settle_retries,
                read_only: original_plan.read_only,
                reduce_only: original_plan.reduce_only,
                max_notional_usd: original_plan.max_notional_usd,
                agent: original_plan.agent.clone(),
                master: original_plan.master.clone(),
                child_algo: original_plan.child_algo,
                follow_poll_secs: original_plan.follow_poll_secs,
                follow_repost_secs: original_plan.follow_repost_secs,
                follow_threshold_bps: original_plan.follow_threshold_bps,
            }
        }
    };

    // The pair launcher is allowed to release execution only after both legs
    // reached this point: signer/agent/master checks, metadata, trigger,
    // sizing, and (for live runs) a durable journal are all ready.  This is
    // immediately before `run_twap_*`, the sole path that can place orders.
    if let Some(barrier) = pair_barrier.as_ref() {
        if let Err(error) = wait_for_pair_start(barrier, journal.as_ref().map(|j| j.run_id())).await {
            if let Some(j) = journal.as_mut() {
                let whole_run = replay_whole_run(&resolved_state_dir, j.run_id())
                .map_err(|accounting_error| {
                    format!(
                        "pair barrier aborted before any order: {error}; additionally failed to validate durable accounting: {accounting_error}"
                    )
                })?;
                if let Err(record_error) =
                    j.record(&hype_trigger_twap::journal::JournalRecord::FinalReport {
                        completed: false,
                        filled_total: Decimal::ZERO.to_string(),
                        outcome_unknown_cloids: Vec::new(),
                        note: format!("pair barrier aborted before any order: {error}"),
                        whole_run: Some(whole_run),
                    })
                {
                    return Err(format!(
                        "pair barrier aborted before any order: {error}; additionally failed to record the durable incomplete report: {record_error}"
                    ));
                }
            }
            if let Some(j) = journal.as_ref() {
                emit_live_report_json(&cli, &resolved_state_dir, j.run_id())?;
            }
            return Err(error);
        }
    }

    // Issue #4: SIGINT/SIGTERM cooperative shutdown. A tokio::sync::watch
    // channel is the shutdown token both the real signal task and (in
    // src/twap.rs's tests) a test harness can drive identically. The signal
    // task itself only flips the watch value — all the actual
    // stop-scheduling / reconcile / cancel / report behaviour lives in
    // `run_twap_journaled`, so there is exactly one code path for both a
    // normal end-of-run and a signal-interrupted one.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let shutdown_signal = hype_trigger_twap::twap::ShutdownSignal::new(shutdown_rx);
    let first_phase_shutdown = shutdown_signal.clone();
    let grace_shutdown = shutdown_signal.clone();
    let signal_task = if read_only {
        None
    } else {
        Some(tokio::spawn(async move {
            let mut sigterm =
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to install SIGTERM handler");
                        return;
                    }
                };
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    tracing::warn!("SIGINT received; requesting graceful shutdown");
                }
                _ = sigterm.recv() => {
                    tracing::warn!("SIGTERM received; requesting graceful shutdown");
                }
            }
            let _ = shutdown_tx.send(true);
        }))
    };

    // Everything from the first phase through position verification belongs
    // to one shutdown-grace scope.  In particular, do not let a completed
    // close phase drop the grace timer before zero-crossing's exact-zero read
    // or the non-reduce-only open phase begins.
    let execution_result = {
    let execution_fut = async {
    let first_phase_position_guard = position_plan
        .as_ref()
        .map(|position| {
            let phase_target = if position.crosses_zero() {
                Decimal::ZERO
            } else {
                position.target_szi
            };
            let guard_master = master
                .as_ref()
                .ok_or_else(|| "position mode lost its resolved master address".to_string())?
                .clone();
            Ok::<_, String>(hype_trigger_twap::twap::PositionTargetGuard::new(
                guard_master,
                phase_target,
                plan.side,
            ))
        })
        .transpose()?;
    let run_fut = async {
        if let Some(position_guard) = first_phase_position_guard.clone() {
            hype_trigger_twap::twap::run_twap_journaled_with_prior_notional_deferred_position_guard(
                &client,
                &plan,
                prior_filled_notional,
                journal.as_mut(),
                Some(first_phase_shutdown),
                position_guard,
            )
            .await
        } else {
            hype_trigger_twap::twap::run_twap_journaled_with_prior_notional(
                &client,
                &plan,
                prior_filled_notional,
                journal.as_mut(),
                Some(first_phase_shutdown),
            )
            .await
        }
    };

    let report = run_fut.await;

    // Issue #4 Finding 1 fix: a resumed run's `report` only reflects fills
    // this process itself placed (the continuation plan, scoped to
    // `remaining`) — the prior process's already-journaled fills must be
    // folded back in so the PRINTED report (and its `total_adjusted`) read
    // against the ORIGINAL plan the operator asked for, not the truncated
    // continuation. The journal itself is already the source of truth for
    // exactly-once accounting (`RunSummary::total_filled` sums each
    // Terminal cloid once, whether journaled by this process or a prior
    // one) — this only affects the human-readable summary.
    let report = match already_filled {
        Some(prior) => hype_trigger_twap::twap::TwapReport {
            total_adjusted: original_total_adjusted,
            total_requested: original_total_requested,
            filled: prior + report.filled,
            ..report
        },
        None => report,
    };
    // A sign-changing target is two strictly ordered executions under the
    // same journal. The close plan is reduce-only, so twap.rs deliberately
    // leaves its FinalReport incomplete. Only after every close cloid is
    // terminal and a fresh authoritative snapshot is exactly zero may the
    // non-reduce-only open plan be constructed.
    let report = if position_plan
        .as_ref()
        .is_some_and(PositionExecutionPlan::crosses_zero)
    {
        if report.exit_code() != 0 {
            let frozen_target = position_plan
                .as_ref()
                .map(|position| position.target_szi)
                .unwrap_or(Decimal::ZERO);
            let observation = match master.as_ref() {
                Some(master) => client
                    .fetch_perp_position(master, &symbol)
                    .await
                    .map(|position| position.szi)
                    .map_err(|error| error.to_string()),
                None => Err("resolved master address is unavailable".into()),
            };
            if let Some(j) = journal.as_mut() {
                record_position_incomplete(
                    &resolved_state_dir,
                    j,
                    frozen_target,
                    observation,
                    "zero-crossing close-to-flat phase failed",
                )?;
            }
            if let Some(j) = journal.as_ref() {
                emit_live_report_json(&cli, &resolved_state_dir, j.run_id())?;
            }
            return Err("close-to-flat phase did not complete; open phase was not started".into());
        }
        let close_has_unresolved = if let Some(j) = journal.as_ref() {
            let records = hype_trigger_twap::journal::ExecutionJournal::read_all(
                &resolved_state_dir,
                j.run_id(),
            )
            .map_err(|e| format!("zero-crossing: cannot read close journal: {e}"))?;
            let replay = hype_trigger_twap::journal::ValidatedJournalReplay::replay(&records)
                .map_err(|e| format!("zero-crossing: invalid close journal: {e}"))?;
            !replay.summary.unresolved_cloids().is_empty()
        } else {
            false
        };
        if close_has_unresolved {
            let frozen_target = position_plan
                .as_ref()
                .map(|position| position.target_szi)
                .unwrap_or(Decimal::ZERO);
            let observation = match master.as_ref() {
                Some(master) => client
                    .fetch_perp_position(master, &symbol)
                    .await
                    .map(|position| position.szi)
                    .map_err(|error| error.to_string()),
                None => Err("resolved master address is unavailable".into()),
            };
            if let Some(j) = journal.as_mut() {
                record_position_incomplete(
                    &resolved_state_dir,
                    j,
                    frozen_target,
                    observation,
                    "zero-crossing close phase left unresolved orders",
                )?;
            }
            if let Some(j) = journal.as_ref() {
                emit_live_report_json(&cli, &resolved_state_dir, j.run_id())?;
            }
            return Err(
                "zero-crossing: close phase has unresolved cloids; open phase was not started"
                    .into(),
            );
        }
        let master = master
            .as_ref()
            .ok_or_else(|| "zero-crossing: missing master address".to_string())?;
        let latest = match client.fetch_perp_position(master, &symbol).await {
            Ok(position) => position,
            Err(error) => {
                if let (Some(j), Some(position)) = (journal.as_mut(), position_plan.as_ref()) {
                    record_position_incomplete(
                        &resolved_state_dir,
                        j,
                        position.target_szi,
                        Err(error.to_string()),
                        "zero-crossing close-to-flat verification unavailable",
                    )?;
                }
                if let Some(j) = journal.as_ref() {
                    emit_live_report_json(&cli, &resolved_state_dir, j.run_id())?;
                }
                return Err(format!(
                    "zero-crossing: cannot verify close-to-flat position; open phase was not started: {error}"
                ));
            }
        };
        if latest.szi != Decimal::ZERO {
            if let (Some(j), Some(position)) = (journal.as_mut(), position_plan.as_ref()) {
                record_position_incomplete(
                    &resolved_state_dir,
                    j,
                    position.target_szi,
                    Ok(latest.szi),
                    "zero-crossing close-to-flat verification found residual position",
                )?;
            }
            if let Some(j) = journal.as_ref() {
                emit_live_report_json(&cli, &resolved_state_dir, j.run_id())?;
            }
            return Err(format!(
                "zero-crossing: close phase expected exact zero, got {}; open phase was not started",
                latest.szi
            ));
        }
        let position = position_plan
            .as_ref()
            .ok_or_else(|| "zero-crossing: missing frozen position plan".to_string())?;
        let open = &position.phases[1];
        let open_sizing = execution_sizing(open.size, cli.slices, asset.sz_decimals, mid, true)
            .map_err(|e| format!("zero-crossing: open phase sizing invalid: {e}"))?;
        let remaining_duration = remaining_execution_window(
            logical_execution_deadline_unix_ms.unwrap_or_else(|| {
                wall_clock_now_ms().saturating_add(cli.duration.as_millis() as u64)
            }),
            wall_clock_now_ms(),
        )
        .unwrap_or(Duration::ZERO);
        if remaining_duration.is_zero() {
            if let Some(j) = journal.as_mut() {
                record_position_incomplete(
                    &resolved_state_dir,
                    j,
                    position.target_szi,
                    Ok(latest.szi),
                    "zero-crossing deadline elapsed after close-to-flat",
                )?;
            }
            if let Some(j) = journal.as_ref() {
                emit_live_report_json(&cli, &resolved_state_dir, j.run_id())?;
            }
            return Err(
                "zero-crossing: deadline elapsed after close phase; open phase was not started"
                    .into(),
            );
        }
        let open_plan = TwapPlan {
            symbol: symbol.clone(),
            side: open.side,
            asset_index: asset.asset_index,
            sz_decimals: asset.sz_decimals,
            per_slice: open_sizing.per_slice,
            total_adjusted: open_sizing.total_adjusted,
            total_requested: open.size,
            slices: cli.slices,
            duration: remaining_duration,
            absolute_deadline_unix_ms: logical_execution_deadline_unix_ms,
            slippage_bps: risk.slippage_bps,
            max_book_age_ms: cli.max_book_age_ms,
            settle_retries: cli.settle_retries,
            read_only,
            reduce_only: false,
            max_notional_usd,
            agent: agent_address.clone(),
            master: Some(master.clone()),
            child_algo: cli.child_algo.into(),
            follow_poll_secs: cli.follow_poll_secs,
            follow_repost_secs: cli.follow_repost_secs,
            follow_threshold_bps: cli.follow_threshold_bps,
        };
        let prior = if let Some(j) = journal.as_ref() {
            let records = hype_trigger_twap::journal::ExecutionJournal::read_all(
                &resolved_state_dir,
                j.run_id(),
            )
            .map_err(|e| format!("zero-crossing: cannot restore cap accounting: {e}"))?;
            hype_trigger_twap::journal::ValidatedJournalReplay::replay(&records)
                .map_err(|e| format!("zero-crossing: invalid cap accounting journal: {e}"))?
                .fill_totals
                .notional
        } else {
            Decimal::ZERO
        };
        let open_report = hype_trigger_twap::twap::run_twap_journaled_with_prior_notional_deferred_position_guard(
            &client,
            &open_plan,
            prior,
            journal.as_mut(),
            Some(shutdown_signal),
            hype_trigger_twap::twap::PositionTargetGuard::new(
                master.clone(),
                position.target_szi,
                open_plan.side,
            ),
        )
        .await;
        hype_trigger_twap::twap::TwapReport {
            filled: report
                .filled
                .checked_add(open_report.filled)
                .unwrap_or(Decimal::MAX),
            slices_executed: report
                .slices_executed
                .saturating_add(open_report.slices_executed),
            slices_skipped: report
                .slices_skipped
                .saturating_add(open_report.slices_skipped),
            elapsed: report
                .elapsed
                .checked_add(open_report.elapsed)
                .unwrap_or(Duration::MAX),
            ..open_report
        }
    } else {
        report
    };
    // The durable journal, not this process-local FillStats, is the source
    // of truth for a resumed report.  In particular its execution VWAP is
    // withheld when any terminal lacks exchange `avg_px`; cap-accounting's
    // conservative Prepared-price fallback is never presented as a fill.
    let (report, durable_whole_run) = if let Some(j) = journal.as_ref() {
        let records = hype_trigger_twap::journal::ExecutionJournal::read_all(
            &resolved_state_dir,
            j.run_id(),
        )
        .map_err(|error| {
            format!(
                "execution finished, but its durable journal cannot be read; refusing an in-memory success report: {error}"
            )
        })?;
        let replay = hype_trigger_twap::journal::ValidatedJournalReplay::replay(&records)
            .map_err(|error| {
                format!(
                    "execution finished, but its durable journal is invalid; refusing an in-memory success report: {error}"
                )
            })?;
        let whole_run = whole_run_from_replay(&replay);
        (
            hype_trigger_twap::twap::TwapReport {
                filled: replay.fill_totals.filled_sz,
                avg_px: replay.execution_vwap,
                ..report
            },
            Some(whole_run),
        )
    } else {
        (report, None)
    };
    let logical_position_total = resumed_frozen_position
        .as_ref()
        .or(position_plan.as_ref())
        .map(|position| {
            position
                .phases
                .iter()
                .try_fold(Decimal::ZERO, |total, phase| total.checked_add(phase.size))
                .ok_or_else(|| "position phase total overflowed".to_string())
        })
        .transpose()?;
    let mut report = match logical_position_total {
        Some(total) => hype_trigger_twap::twap::TwapReport {
            total_requested: total,
            total_adjusted: total,
            ..report
        },
        None => report,
    };
    print!("{}", report.render());
    if let Some(whole) = durable_whole_run.as_ref() {
        println!(
            "whole run: accounted_notional={} cap_remaining={} unresolved={} logical_elapsed_ms={}",
            whole.accounted_notional,
            whole.cap_remaining.as_deref().unwrap_or("unbounded"),
            whole.unresolved_cloids,
            whole.logical_elapsed_ms
        );
    }
    if position_plan.is_some() && report.exit_code() != 0 {
        let expected = position_plan
            .as_ref()
            .ok_or_else(|| "position plan disappeared while recording abort".to_string())?;
        let observation = match master.as_ref() {
            Some(master) => client
                .fetch_perp_position(master, &symbol)
                .await
                .map(|position| position.szi)
                .map_err(|error| error.to_string()),
            None => Err("resolved master address is unavailable".into()),
        };
        // A reduce-only child can be rejected as already-flat even though an
        // external/manual fill reached the frozen target in the meantime.
        // The TWAP report correctly treats the exchange rejection as an
        // abort, but an authoritative exact-target read is stronger evidence
        // for position modes.  Promote it to completion only when the
        // journal itself replays cleanly and has no unresolved cloids; never
        // use this path to hide an ambiguous order.
        if observation.as_ref() == Ok(&expected.target_szi) {
            let unresolved = if let Some(j) = journal.as_ref() {
                let records = hype_trigger_twap::journal::ExecutionJournal::read_all(
                    &resolved_state_dir,
                    j.run_id(),
                )
                .map_err(|error| {
                    format!(
                        "position target matched after abort, but journal cannot be read; refusing completion: {error}"
                    )
                })?;
                hype_trigger_twap::journal::ValidatedJournalReplay::replay(&records)
                    .map_err(|error| {
                        format!(
                            "position target matched after abort, but journal is invalid; refusing completion: {error}"
                        )
                    })?
                    .summary
                    .unresolved_cloids()
            } else {
                Vec::new()
            };
            if unresolved.is_empty() {
                tracing::warn!(
                    target = %human(expected.target_szi),
                    "authoritative position matched frozen target after child abort; accepting safe completion"
                );
                report.abort_reason = None;
            } else {
                if let Some(j) = journal.as_mut() {
                    record_position_incomplete(
                        &resolved_state_dir,
                        j,
                        expected.target_szi,
                        observation,
                        "position target matched after abort but journal has unresolved cloids",
                    )?;
                }
                if let Some(j) = journal.as_ref() {
                    emit_live_report_json(&cli, &resolved_state_dir, j.run_id())?;
                }
                return Err(
                    "position target matched after abort, but unresolved child orders prevent safe completion"
                        .into(),
                );
            }
        }
        if report.exit_code() == 0 {
            // Continue into the normal terminal verification below, which
            // records the sole completed FinalReport from authoritative
            // exchange state.
        } else if let Some(j) = journal.as_mut() {
            record_position_incomplete(
                &resolved_state_dir,
                j,
                expected.target_szi,
                observation,
                "position-aware execution aborted",
            )?;
        }
        if report.exit_code() != 0 {
            if let Some(j) = journal.as_ref() {
                emit_live_report_json(&cli, &resolved_state_dir, j.run_id())?;
            }
            return Ok(ExitCode::FAILURE);
        }
    }
    // A reduce-only exchange rejection (including the common "already flat"
    // case) is not evidence that a close succeeded.  Position modes always
    // re-read the authoritative signed size after all child orders have
    // settled; any mismatch or failed read leaves the last journal report
    // incomplete so `--resume` remains the only recovery path.
    if let Some(expected) = position_plan.as_ref() {
        let master = master
            .as_ref()
            .ok_or_else(|| "position mode lost its resolved master address".to_string())?;
        let verified = client.fetch_perp_position(master, &symbol).await;
        let verified_szi = match verified {
            Ok(position) if position.szi == expected.target_szi => position.szi,
            Ok(position) => {
                if let Some(j) = journal.as_mut() {
                    record_position_incomplete(
                        &resolved_state_dir,
                        j,
                        expected.target_szi,
                        Ok(position.szi),
                        "position-aware terminal verification mismatched frozen target",
                    )?;
                }
                if let Some(j) = journal.as_ref() {
                    emit_live_report_json(&cli, &resolved_state_dir, j.run_id())?;
                }
                return Err(format!(
                    "position-aware terminal verification failed: expected {}, got {}; no further order was sent",
                    expected.target_szi, position.szi
                ));
            }
            Err(error) => {
                if let Some(j) = journal.as_mut() {
                    record_position_incomplete(
                        &resolved_state_dir,
                        j,
                        expected.target_szi,
                        Err(error.to_string()),
                        "position-aware terminal verification unavailable",
                    )?;
                }
                if let Some(j) = journal.as_ref() {
                    emit_live_report_json(&cli, &resolved_state_dir, j.run_id())?;
                }
                return Err(format!(
                    "position-aware terminal verification failed closed: {error}; no further order was sent"
                ));
            }
        };
        println!("POSITION VERIFIED: signed size {}", human(verified_szi));
        if let Some(j) = journal.as_mut() {
            let records = hype_trigger_twap::journal::ExecutionJournal::read_all(
                &resolved_state_dir,
                j.run_id(),
            )
            .map_err(|error| {
                format!(
                    "position verification matched the target, but its durable journal cannot be read: {error}"
                )
            })?;
            let replay = hype_trigger_twap::journal::ValidatedJournalReplay::replay(&records)
                .map_err(|error| {
                    format!(
                        "position verification matched the target, but its durable journal is invalid: {error}"
                    )
                })?;
            let whole_run = whole_run_from_replay(&replay);
            j.record(&hype_trigger_twap::journal::JournalRecord::FinalReport {
                completed: true,
                filled_total: replay.fill_totals.filled_sz.to_string(),
                outcome_unknown_cloids: Vec::new(),
                note: "position-aware terminal verification matched frozen target".into(),
                whole_run: Some(whole_run),
            })
            .map_err(|e| format!("recording position verification failed: {e}"))?;
        }
    }
    if let Some(j) = journal.as_ref() {
        emit_live_report_json(&cli, &resolved_state_dir, j.run_id())?;
    }

    if let Some(observer) = read_only_observer.as_mut() {
        observer.emit_simulation_final(report.slices_executed, report.exit_code() == 0);
    }

    if report.exit_code() == 0 {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::FAILURE)
    }
    };

    if signal_task.is_some() {
        complete_before_shutdown_grace(execution_fut, grace_shutdown, cli.shutdown_grace)
            .await
    } else {
        Ok(execution_fut.await)
    }
    };
    let result = match execution_result {
        Ok(result) => result,
        Err(()) => {
            tracing::error!(
                grace = ?cli.shutdown_grace,
                "shutdown grace period exceeded; giving up with outcome_unknown"
            );
            if let Some(j) = journal.as_mut() {
                // Derive every FinalReport accounting field from one validated
                // replay. A malformed/unreadable journal must not be papered
                // over with guessed zero/empty values.
                let replay = grace_timeout_report_fields(&resolved_state_dir, j.run_id())
                    .map_err(|replay_error| {
                        format!(
                            "shutdown grace period ({:?}) exceeded before reconciliation finished; durable journal accounting could not be validated: {replay_error}",
                            cli.shutdown_grace
                        )
                    })?;
                let whole_run = whole_run_from_replay(&replay);
                j.record(&hype_trigger_twap::journal::JournalRecord::FinalReport {
                    completed: false,
                    filled_total: replay.fill_totals.filled_sz.to_string(),
                    outcome_unknown_cloids: replay.summary.unresolved_cloids(),
                    note: format!(
                        "shutdown grace period ({:?}) exceeded before reconciliation finished",
                        cli.shutdown_grace
                    ),
                    whole_run: Some(whole_run),
                })
                .map_err(|record_error| {
                    format!(
                        "shutdown grace period ({:?}) exceeded before reconciliation finished; additionally failed to record the durable incomplete report: {record_error}",
                        cli.shutdown_grace
                    )
                })?;
            }
            if let Some(j) = journal.as_ref() {
                emit_live_report_json(&cli, &resolved_state_dir, j.run_id())?;
            }
            Ok(ExitCode::FAILURE)
        }
    };
    result
    }
    .await;
    // All journal/event observers captured by the execution future are gone
    // here. Close the final alert sender and give the isolated worker a
    // bounded flush window before Tokio tears the runtime down.
    observability.shutdown().await;
    result
}

/// Emit the same schema-v1 DTO used by `hype-twap-runs`, after re-reading the
/// durable journal.  The live process never serializes its in-memory report,
/// ensuring resume/abort/early-complete output shares the one validated
/// interpretation used by the dedicated inspection command.
fn emit_live_report_json(cli: &Cli, state_dir: &Path, run_id: &str) -> Result<(), String> {
    let Some(destination) = cli.report_json.as_deref() else {
        return Ok(());
    };
    let report = hype_trigger_twap::run_reports::inspect(state_dir, run_id);
    let json = serde_json::to_vec(&report).map_err(|e| format!("encoding --report-json: {e}"))?;
    if destination == Path::new("-") {
        std::io::stdout()
            .write_all(&json)
            .and_then(|_| std::io::stdout().write_all(b"\n"))
            .map_err(|e| format!("writing --report-json stdout: {e}"))?;
        return Ok(());
    }
    let parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let temporary = parent.join(format!(
        ".hype-twap-report-{}-{}.tmp",
        std::process::id(),
        uuid::Uuid::now_v7()
    ));
    let write_result = (|| -> Result<(), String> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|e| format!("creating --report-json temporary file: {e}"))?;
        file.write_all(&json)
            .and_then(|_| file.write_all(b"\n"))
            .and_then(|_| file.sync_all())
            .map_err(|e| format!("writing --report-json temporary file: {e}"))?;
        fs::rename(&temporary, destination)
            .map_err(|e| format!("publishing --report-json file: {e}"))?;
        fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|e| format!("syncing --report-json directory: {e}"))?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_result
}

fn whole_run_from_replay(
    replay: &hype_trigger_twap::journal::ValidatedJournalReplay,
) -> hype_trigger_twap::journal::WholeRunSummary {
    let started_at_unix_ms = replay
        .summary
        .header
        .as_ref()
        .map(|header| header.started_at_unix_ms)
        .unwrap_or_else(wall_clock_now_ms);
    hype_trigger_twap::journal::WholeRunSummary {
        requested_total: replay
            .summary
            .header
            .as_ref()
            .and_then(|header| header.execution_fingerprint.as_ref())
            .map(|fingerprint| {
                fingerprint
                    .logical_position_total()
                    .unwrap_or_else(|| fingerprint.total_requested.clone())
            }),
        adjusted_total: replay
            .summary
            .header
            .as_ref()
            .and_then(|header| header.execution_fingerprint.as_ref())
            .map(|fingerprint| {
                fingerprint
                    .logical_position_total()
                    .unwrap_or_else(|| fingerprint.total_adjusted.clone())
            }),
        accounted_notional: replay.fill_totals.notional.to_string(),
        cap_remaining: replay.fingerprint_max_notional.map(|cap| {
            (cap - replay.fill_totals.notional)
                .max(Decimal::ZERO)
                .to_string()
        }),
        trusted_vwap: replay.execution_vwap.map(|value| value.to_string()),
        logical_elapsed_ms: wall_clock_now_ms().saturating_sub(started_at_unix_ms),
        unresolved_cloids: replay.summary.unresolved_cloids().len(),
    }
}

fn replay_whole_run(
    state_root: &Path,
    run_id: &str,
) -> Result<hype_trigger_twap::journal::WholeRunSummary, String> {
    let records = hype_trigger_twap::journal::ExecutionJournal::read_all(state_root, run_id)
        .map_err(|e| format!("reading journal for whole-run report: {e}"))?;
    let replay = hype_trigger_twap::journal::ValidatedJournalReplay::replay(&records)
        .map_err(|e| format!("validating journal for whole-run report: {e}"))?;
    Ok(whole_run_from_replay(&replay))
}

fn live_journal_observer(
    journal: &hype_trigger_twap::journal::ExecutionJournal,
    observability: &ObservabilityRuntime,
) -> JournalEventObserver {
    match JournalEventObserver::open(
        &journal.dir().join("events.jsonl"),
        ExecutionMode::Live,
        Arc::clone(&observability.metrics),
        observability.alerts.clone(),
    ) {
        Ok(observer) => observer,
        Err(error) => {
            tracing::warn!(
                error = %error,
                "observability event log unavailable; continuing with metrics/hook only"
            );
            JournalEventObserver::without_event_log(
                ExecutionMode::Live,
                Arc::clone(&observability.metrics),
                observability.alerts.clone(),
            )
        }
    }
}

/// Emit a complete, state-free lifecycle for every successful position-mode
/// read-only exit, including preflight-only no-op branches that never build a
/// [`TwapPlan`]. The side is the first executable phase when one exists;
/// otherwise it describes the already-held target orientation required by the
/// schema-v1 DTO. Event-log failures remain best-effort and never change the
/// trading/simulation result.
fn emit_read_only_position_lifecycle(
    cli: &Cli,
    observability: &ObservabilityRuntime,
    plan: &PositionExecutionPlan,
    planned_slices: u32,
) {
    if !cli.is_read_only() {
        return;
    }
    let Some(path) = cli.event_jsonl.as_ref() else {
        return;
    };
    let mut observer = JournalEventObserver::open(
        path,
        ExecutionMode::ReadOnly,
        Arc::clone(&observability.metrics),
        observability.alerts.clone(),
    )
    .unwrap_or_else(|_| {
        tracing::warn!(
            path = %path.display(),
            "read-only event log unavailable; position plan continues without JSONL"
        );
        JournalEventObserver::without_event_log(
            ExecutionMode::ReadOnly,
            Arc::clone(&observability.metrics),
            observability.alerts.clone(),
        )
    });
    let side = plan.phases.first().map_or_else(
        || {
            if plan.target_szi < Decimal::ZERO {
                Side::Short
            } else {
                Side::Long
            }
        },
        |phase| phase.side,
    );
    observer.emit_simulation_started(
        uuid::Uuid::now_v7(),
        plan.symbol.clone(),
        side,
        planned_slices,
    );
    observer.emit_simulation_final(0, true);
}

/// Append a fail-closed position terminal report using the durable replay for
/// fill/unknown accounting. The authoritative position observation is kept
/// in the journal note for operator recovery, while the observability sidecar
/// projects only its closed, secret-free failure vocabulary.
fn record_position_incomplete(
    state_root: &Path,
    journal: &mut hype_trigger_twap::journal::ExecutionJournal,
    target_szi: Decimal,
    observation: Result<Decimal, String>,
    context: &str,
) -> Result<(), String> {
    let records =
        hype_trigger_twap::journal::ExecutionJournal::read_all(state_root, journal.run_id())
            .map_err(|error| format!("{context}: cannot read durable journal: {error}"))?;
    let replay = hype_trigger_twap::journal::ValidatedJournalReplay::replay(&records)
        .map_err(|error| format!("{context}: cannot validate durable journal: {error}"))?;
    let note = match observation {
        Ok(actual_szi) => format!(
            "{context}: authoritative signed position is {actual_szi}; frozen target is {target_szi}; automatic continuation stopped"
        ),
        Err(error) => format!(
            "{context}: authoritative position unavailable ({error}); frozen target is {target_szi}; automatic continuation stopped"
        ),
    };
    journal
        .record(&hype_trigger_twap::journal::JournalRecord::FinalReport {
            completed: false,
            filled_total: replay.fill_totals.filled_sz.to_string(),
            outcome_unknown_cloids: replay.summary.unresolved_cloids(),
            note,
            whole_run: Some(whole_run_from_replay(&replay)),
        })
        .map_err(|error| format!("{context}: recording incomplete position report: {error}"))
}

/// Finding 2 fix: the `(filled_total, outcome_unknown_cloids)` a
/// grace-timeout `FinalReport` should carry, derived by replaying THIS
/// run's own journal at the moment the grace period expired — rather than
/// the pre-fix hardcoded `(0, [])`, which discarded every real fill and
/// every genuinely-still-open cloid.
///
/// A read or validation failure is returned to the caller. A grace-timeout
/// is already a failure, but it must never append a plausible-looking report
/// with fabricated zero/empty accounting.
fn grace_timeout_report_fields(
    state_root: &std::path::Path,
    run_id: &str,
) -> Result<hype_trigger_twap::journal::ValidatedJournalReplay, String> {
    let records = hype_trigger_twap::journal::ExecutionJournal::read_all(state_root, run_id)
        .map_err(|error| format!("cannot read journal: {error}"))?;
    hype_trigger_twap::journal::ValidatedJournalReplay::replay(&records)
        .map_err(|error| format!("cannot validate journal: {error}"))
}

/// Issue #4: force-reconcile every submitted/unknown cloid in an incomplete
/// run's journal via `orderStatus`, appending the resolved
/// `Acknowledged`/`Terminal` records. Used by both `--resume` (which then
/// continues the run) and `--abandon-incomplete-run` (which reconciles then
/// marks the run `Abandoned` without continuing it) — reconciliation itself
/// is identical either way, only what happens AFTER it differs.
///
/// Reuses [`hype_trigger_twap::twap::reconcile_unresolved_cloid`], which
/// wraps the same `orderStatus`-by-cloid policy `place_slice_reconciled`
/// already uses for an ambiguous send (Issue #7's `reconcile_by_cloid`) —
/// there is exactly one orderStatus reconciliation policy in this codebase,
/// not a second one for the resume path.
async fn reconcile_incomplete_run(
    client: &HlClient,
    plan: &TwapPlan,
    journal: &mut hype_trigger_twap::journal::ExecutionJournal,
) -> Result<(), String> {
    let records = hype_trigger_twap::journal::ExecutionJournal::read_all(
        journal
            .dir()
            .parent()
            .and_then(|p| p.parent())
            .ok_or_else(|| {
                "malformed journal run directory (expected <state_dir>/runs/<run_id>)".to_string()
            })?,
        journal.run_id(),
    )
    .map_err(|e| e.to_string())?;
    let replay = hype_trigger_twap::journal::ValidatedJournalReplay::replay(&records)
        .map_err(|e| format!("cannot reconcile invalid journal: {e}"))?;

    // The validated replay is the sole source of both state and original
    // intent. Reconciliation cross-checks each cloid with its own durable
    // side (important for a zero-crossing position run), never a CLI
    // placeholder or a separately reconstructed map.
    for cloid in replay.summary.unresolved_cloids() {
        let stored = replay.prepared.get(&cloid).ok_or_else(|| {
            format!(
                "cannot reconcile cloid {cloid}: no Prepared record found in this run's own \
                 journal — the journal is malformed (every cloid reachable via \
                 unresolved_cloids() must have been journaled with a Prepared record before \
                 the send that made it unresolved)"
            )
        })?;
        let prepared = hype_trigger_twap::twap::PreparedIntent {
            symbol: stored.symbol.clone(),
            side: stored.side,
            tif: stored.tif,
            px: stored.px.parse().map_err(|_| {
                format!("cannot reconcile cloid {cloid}: invalid durable Prepared.px")
            })?,
            sz: stored.sz.parse().map_err(|_| {
                format!("cannot reconcile cloid {cloid}: invalid durable Prepared.sz")
            })?,
        };
        let mut cloid_plan = plan.clone();
        cloid_plan.symbol = stored.symbol.clone();
        cloid_plan.side = stored.side;
        hype_trigger_twap::twap::reconcile_unresolved_cloid(
            client,
            &cloid_plan,
            cloid,
            stored.slice_idx,
            &prepared,
            journal,
        )
        .await
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use clap::CommandFactory;

    fn base_args() -> Vec<&'static str> {
        vec![
            "hype-twap",
            "--symbol",
            "HYPE",
            "--side",
            "long",
            "--usd",
            "1500",
            "--duration",
            "30m",
        ]
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_grace_bounds_blocking_later_position_work() {
        // Model the exact-zero/final-position read after a first phase has
        // returned: it is outside the slice loop, but still inside the one
        // lifecycle future passed to `complete_before_shutdown_grace`.
        let (tx, rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(complete_before_shutdown_grace(
            std::future::pending::<()>(),
            ShutdownSignal::new(rx),
            Duration::from_secs(1),
        ));
        tokio::task::yield_now().await;
        tx.send(true).unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(task.await.unwrap(), Err(()));
    }

    #[test]
    fn report_json_is_atomic_secret_safe_and_matches_inspection_schema() {
        let tmp = TempDir::new();
        let state_dir = tmp.path().join("state");
        let run_id = "report-fixture";
        let mut journal = hype_trigger_twap::journal::ExecutionJournal::start(
            &state_dir,
            run_id.into(),
            hype_trigger_twap::journal::RunHeader {
                run_id: run_id.into(),
                network: "testnet".into(),
                agent: None,
                master: None,
                symbol: Symbol::new("HYPE"),
                side: Side::Long,
                slices: 1,
                plan_hash: "public-plan-hash".into(),
                execution_fingerprint: None,
                started_at_unix_ms: 1,
                execution_deadline_unix_ms: Some(2),
            },
        )
        .unwrap();
        journal
            .record(&hype_trigger_twap::journal::JournalRecord::FinalReport {
                completed: true,
                filled_total: "0".into(),
                outcome_unknown_cloids: Vec::new(),
                note: "potential-secret-that-must-not-appear".into(),
                whole_run: None,
            })
            .unwrap();
        let report_path = tmp.path().join("final-report.json");
        fs::write(&report_path, b"stale").unwrap();
        let cli = Cli::try_parse_from([
            "hype-twap",
            "--symbol",
            "HYPE",
            "--side",
            "long",
            "--usd",
            "1",
            "--duration",
            "1m",
            "--report-json",
            report_path.to_str().unwrap(),
        ])
        .unwrap();

        emit_live_report_json(&cli, &state_dir, run_id).unwrap();
        let written = fs::read_to_string(&report_path).unwrap();
        let actual: serde_json::Value = serde_json::from_str(&written).unwrap();
        let expected =
            serde_json::to_value(hype_trigger_twap::run_reports::inspect(&state_dir, run_id))
                .unwrap();
        assert_eq!(actual, expected);
        assert_eq!(actual["schema_version"], 1);
        assert!(!written.contains("potential-secret-that-must-not-appear"));
        assert!(
            !tmp.path()
                .read_dir()
                .unwrap()
                .filter_map(Result::ok)
                .any(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .contains(".hype-twap-report-")),
            "atomic write must not leave a temporary report behind"
        );
    }

    fn test_pair_barrier() -> (PathBuf, PairBarrier) {
        let dir =
            std::env::temp_dir().join(format!("hype-twap-pair-test-{}", uuid::Uuid::now_v7()));
        fs::create_dir(&dir).unwrap();
        let barrier = PairBarrier {
            ready_file: dir.join("leg.ready.json"),
            start_file: dir.join("start.json"),
            run_id: "pair-test-1".into(),
            timeout: Duration::from_millis(200),
        };
        (dir, barrier)
    }

    #[test]
    fn pair_barrier_flags_are_all_or_nothing() {
        let mut args = base_args();
        args.extend(["--pair-run-id", "pair-1"]);
        let cli = Cli::try_parse_from(args).unwrap();
        assert!(cli
            .pair_barrier()
            .unwrap_err()
            .contains("supplied together"));
    }

    #[tokio::test]
    async fn pair_barrier_publishes_ready_then_waits_for_common_release() {
        let (dir, barrier) = test_pair_barrier();
        let start = PairStartFile {
            run_id: barrier.run_id.clone(),
            start_at_unix_ms: wall_clock_now_ms() + 40,
        };
        fs::write(&barrier.start_file, serde_json::to_vec(&start).unwrap()).unwrap();
        wait_for_pair_start(&barrier, Some("live-journal-123"))
            .await
            .unwrap();
        let ready: serde_json::Value =
            serde_json::from_slice(&fs::read(&barrier.ready_file).unwrap()).unwrap();
        assert_eq!(ready["run_id"], barrier.run_id);
        assert!(ready["ready_at_unix_ms"].as_u64().is_some());
        assert_eq!(ready["journal_run_id"], "live-journal-123");
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn pair_barrier_timeout_fails_before_execution() {
        let (dir, mut barrier) = test_pair_barrier();
        barrier.timeout = Duration::from_millis(25);
        let error = wait_for_pair_start(&barrier, None).await.unwrap_err();
        assert!(error.contains("no order was placed"));
        assert!(barrier.ready_file.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    // === F3: --help documents the environment contract ===

    #[test]
    fn help_output_documents_every_environment_variable() {
        // `long_about = None` makes clap discard the struct doc comment, so
        // without `after_help` the variables that decide whether the tool can
        // trade at all would appear nowhere in `--help`.
        let help = Cli::command().render_help().to_string();
        for var in [
            "HL_AGENT_PK",
            "HL_AGENT_ADDRESS",
            "HL_MASTER_ADDRESS",
            "HL_INFO_URL",
            "HL_EXCHANGE_URL",
        ] {
            assert!(help.contains(var), "--help must mention {var}\n{help}");
        }
        assert!(help.contains("ENVIRONMENT VARIABLES"), "{help}");
    }

    #[test]
    fn help_says_agent_address_is_the_agent_not_the_master() {
        // The single most dangerous confusion in this tool's configuration:
        // pointing HL_AGENT_ADDRESS at the master account silently describes
        // the wrong wallet. The help must call it out explicitly.
        let help = Cli::command().render_help().to_string();
        assert!(
            help.contains("AGENT (API wallet) address — NOT the master"),
            "{help}"
        );
        // New runs may auto-probe it, while resume/abandon must supply it
        // before any API call so durable identity cannot be rebound.
        assert!(help.contains("userRole"), "{help}");
        assert!(help.contains("required"), "{help}");
        assert!(help.contains("--resume/--abandon-incomplete-run"), "{help}");
    }

    #[test]
    fn help_never_suggests_passing_the_key_as_a_flag() {
        let help = Cli::command().render_help().to_string();
        assert!(help.contains("never as a flag"), "{help}");
        assert!(
            !help.contains("--hl-agent-pk"),
            "there must be no PK flag, not even in the help text"
        );
    }

    #[test]
    fn parses_the_spec_example_invocation() {
        let cli = Cli::try_parse_from([
            "hype-twap",
            "--symbol",
            "HYPE",
            "--side",
            "long",
            "--usd",
            "1500",
            "--duration",
            "30m",
            "--network",
            "testnet",
            "--max-notional-usd",
            "5000",
            "--live",
        ])
        .unwrap();
        assert_eq!(cli.symbol, "HYPE");
        assert_eq!(cli.side, Some(SideArg::Long));
        assert_eq!(cli.usd, Some(Decimal::from(1500)));
        assert_eq!(cli.duration, Duration::from_secs(1800));
        assert!(cli.live);
        assert!(cli.is_live());
        cli.validate().unwrap();
    }

    #[test]
    fn legacy_read_only_false_remains_a_live_compatibility_alias() {
        let cli = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain([
                    "--network",
                    "testnet",
                    "--max-notional-usd",
                    "5000",
                    "--read-only",
                    "false",
                ])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert!(!cli.live);
        assert!(!cli.read_only);
        assert!(cli.is_live());
        cli.validate().unwrap();
    }

    #[test]
    fn read_only_defaults_to_true() {
        let cli = Cli::try_parse_from(base_args()).unwrap();
        assert!(cli.is_read_only(), "read-only MUST default to true (§3)");
        assert!(!cli.live);
    }

    #[test]
    fn defaults_match_the_spec_table() {
        let cli = Cli::try_parse_from(base_args()).unwrap();
        assert_eq!(cli.slices, 10);
        assert_eq!(cli.network, NetworkArg::Mainnet);
        assert_eq!(cli.slippage_bps, Decimal::from(20));
        assert_eq!(cli.max_book_age_ms, 3000);
        assert_eq!(cli.trigger_poll_secs, 2);
        assert_eq!(cli.wait_network_grace, Duration::from_secs(30 * 60));
    }

    #[test]
    fn equivalent_decimal_spellings_have_one_fingerprint_representation() {
        for spelling in ["1", "1.0", "1.000000"] {
            let value: Decimal = spelling.parse().unwrap();
            assert_eq!(canonical_decimal(value), "1");
        }
        let a = Cli::try_parse_from([
            "hype-twap",
            "--symbol",
            "HYPE",
            "--side",
            "long",
            "--size",
            "1.0",
            "--duration",
            "1m",
        ])
        .unwrap();
        let b = Cli::try_parse_from([
            "hype-twap",
            "--symbol",
            "HYPE",
            "--side",
            "long",
            "--size",
            "1.000",
            "--duration",
            "1m",
        ])
        .unwrap();
        assert_eq!(requested_mode_and_value(&a), requested_mode_and_value(&b));
    }

    #[test]
    fn position_sizing_preserves_the_final_grid_aligned_remainder() {
        let ordinary = execution_sizing(dec!(1.01), 2, 2, dec!(50), false).unwrap();
        assert_eq!(
            (ordinary.per_slice, ordinary.total_adjusted),
            (dec!(0.50), dec!(1.00))
        );

        let position = execution_sizing(dec!(1.01), 2, 2, dec!(50), true).unwrap();
        assert_eq!(
            (position.per_slice, position.total_adjusted),
            (dec!(0.50), dec!(1.01))
        );
        assert_eq!(
            hype_trigger_twap::twap::target_at_slice(
                2,
                2,
                position.per_slice,
                position.total_adjusted,
            ),
            dec!(1.01),
            "the final slice must target the exact frozen position delta"
        );
    }

    #[test]
    fn public_addresses_are_strict_and_canonical() {
        let upper = "0xABCDEFABCDEFABCDEFABCDEFABCDEFABCDEFABCD";
        assert_eq!(
            parse_public_address("address", upper).unwrap().as_str(),
            "0xabcdefabcdefabcdefabcdefabcdefabcdefabcd"
        );
        for invalid in [
            "abcdefabcdefabcdefabcdefabcdefabcdefabcd",
            "0x1234",
            "0xgggggggggggggggggggggggggggggggggggggggg",
            "0Xabcdefabcdefabcdefabcdefabcdefabcdefabcd",
        ] {
            assert!(
                parse_public_address("address", invalid).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn flatten_deadline_is_rejected_outside_flatten_mode() {
        let mut args = base_args();
        args.extend(["--flatten-deadline-unix-ms", "9999999999999"]);
        let cli = Cli::try_parse_from(args).unwrap();
        assert_eq!(
            cli.validate().unwrap_err(),
            "--flatten-deadline-unix-ms requires --flatten"
        );
    }

    // === Issue #1: --child-algo ===

    #[test]
    fn child_algo_defaults_to_market_and_maps_to_the_market_variant() {
        let cli = Cli::try_parse_from(base_args()).unwrap();
        assert_eq!(cli.child_algo, ChildAlgoArg::Market);
        assert_eq!(ChildAlgo::from(cli.child_algo), ChildAlgo::Market);
    }

    #[test]
    fn child_algo_passive_parses_and_maps_to_the_passive_variant() {
        let mut args = base_args();
        args.push("--child-algo");
        args.push("passive");
        let cli = Cli::try_parse_from(args).unwrap();
        assert_eq!(cli.child_algo, ChildAlgoArg::Passive);
        assert_eq!(ChildAlgo::from(cli.child_algo), ChildAlgo::Passive);
    }

    #[test]
    fn child_algo_rejects_an_unknown_value() {
        let mut args = base_args();
        args.push("--child-algo");
        args.push("aggressive");
        assert!(Cli::try_parse_from(args).is_err());
    }

    #[test]
    fn size_and_usd_are_mutually_exclusive() {
        let r = Cli::try_parse_from([
            "hype-twap",
            "--symbol",
            "HYPE",
            "--side",
            "long",
            "--usd",
            "1500",
            "--size",
            "10",
            "--duration",
            "30m",
        ]);
        assert!(r.is_err(), "--size and --usd must conflict");
    }

    #[test]
    fn one_of_size_or_usd_is_required() {
        let cli = Cli::try_parse_from([
            "hype-twap",
            "--symbol",
            "HYPE",
            "--side",
            "long",
            "--duration",
            "30m",
        ])
        .unwrap();
        let err = cli.validate().unwrap_err();
        assert!(err.contains("--size or --usd"), "{err}");
    }

    #[test]
    fn trigger_price_requires_trigger_when() {
        let r = Cli::try_parse_from([
            "hype-twap",
            "--symbol",
            "HYPE",
            "--side",
            "long",
            "--usd",
            "1500",
            "--duration",
            "30m",
            "--trigger-price",
            "40",
        ]);
        assert!(
            r.is_err(),
            "--trigger-price alone must be rejected (fail-fast, no inference)"
        );
    }

    #[test]
    fn trigger_when_requires_trigger_price() {
        let r = Cli::try_parse_from([
            "hype-twap",
            "--symbol",
            "HYPE",
            "--side",
            "long",
            "--usd",
            "1500",
            "--duration",
            "30m",
            "--trigger-when",
            "above",
        ]);
        assert!(r.is_err());
    }

    #[test]
    fn trigger_pair_parses_and_builds_config() {
        let cli = Cli::try_parse_from([
            "hype-twap",
            "--symbol",
            "HYPE",
            "--side",
            "long",
            "--usd",
            "1500",
            "--duration",
            "30m",
            "--trigger-price",
            "40.5",
            "--trigger-when",
            "above",
            "--start-after",
            "10m",
        ])
        .unwrap();
        cli.validate().unwrap();
        let cfg = cli.trigger_config();
        assert_eq!(
            cfg.price,
            Some((TriggerWhen::Above, "40.5".parse().unwrap()))
        );
        assert_eq!(cfg.start_after, Some(Duration::from_secs(600)));
        assert!(cfg.describe().contains("OR after"));
    }

    #[test]
    fn no_trigger_flags_means_immediate() {
        let cli = Cli::try_parse_from(base_args()).unwrap();
        assert!(cli.trigger_config().is_immediate());
    }

    #[test]
    fn zero_slices_is_rejected() {
        let cli = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain(["--slices", "0"])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert!(cli.validate().unwrap_err().contains("--slices"));
    }

    #[test]
    fn zero_duration_is_rejected() {
        let cli = Cli::try_parse_from([
            "hype-twap",
            "--symbol",
            "HYPE",
            "--side",
            "long",
            "--usd",
            "1500",
            "--duration",
            "0s",
        ])
        .unwrap();
        assert!(cli.validate().unwrap_err().contains("--duration"));
    }

    #[test]
    fn non_positive_sizes_are_rejected() {
        let cli = Cli::try_parse_from([
            "hype-twap",
            "--symbol",
            "HYPE",
            "--side",
            "long",
            "--usd",
            "0",
            "--duration",
            "30m",
        ])
        .unwrap();
        assert!(cli.validate().unwrap_err().contains("--usd"));

        // `--size=-1` (attached form): clap would treat a bare `-1` as a flag,
        // so the attached form is what actually reaches the validator.
        let cli = Cli::try_parse_from([
            "hype-twap",
            "--symbol",
            "HYPE",
            "--side",
            "short",
            "--size=-1",
            "--duration",
            "30m",
        ])
        .unwrap();
        assert!(cli.validate().unwrap_err().contains("--size"));
    }

    #[test]
    fn negative_slippage_is_rejected() {
        let cli = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain(["--slippage-bps=-1"])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert!(cli.validate().unwrap_err().contains("--slippage-bps"));
    }

    // === Issue #3: risk envelope CLI wiring ===

    #[test]
    fn slippage_at_hard_cap_is_rejected_even_with_allow_high_slippage() {
        let cli = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain(["--slippage-bps", "10000", "--allow-high-slippage"])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let err = cli.validate().unwrap_err();
        assert!(err.contains("hard cap"), "{err}");
    }

    #[test]
    fn slippage_over_warn_threshold_is_rejected_without_the_flag() {
        let cli = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain(["--slippage-bps", "1001"])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let err = cli.validate().unwrap_err();
        assert!(err.contains("--allow-high-slippage"), "{err}");
    }

    #[test]
    fn slippage_over_warn_threshold_is_accepted_with_the_flag() {
        let cli = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain(["--slippage-bps", "1001", "--allow-high-slippage"])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        cli.validate().unwrap();
    }

    #[test]
    fn live_without_max_notional_usd_is_rejected() {
        let cli = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain(["--network", "testnet", "--live"])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let err = cli.validate().unwrap_err();
        assert!(err.contains("--max-notional-usd"), "{err}");
    }

    #[test]
    fn live_with_max_notional_usd_is_accepted() {
        let cli = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain([
                    "--network",
                    "testnet",
                    "--live",
                    "--max-notional-usd",
                    "5000",
                ])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert!(cli.is_live());
        cli.validate().unwrap();
    }

    #[test]
    fn live_conflicts_with_an_explicit_read_only_setting() {
        for value in ["true", "false"] {
            let result = Cli::try_parse_from(
                base_args()
                    .into_iter()
                    .chain(["--live", "--read-only", value])
                    .collect::<Vec<_>>(),
            );
            assert!(
                result.is_err(),
                "--live must conflict with --read-only {value}"
            );
        }
    }

    #[test]
    fn mainnet_live_is_rejected_while_issue_16_gate_is_closed() {
        let cli = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain(["--live", "--max-notional-usd", "5000"])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let error = cli.validate().unwrap_err();
        assert!(
            error.contains("mainnet live execution is disabled"),
            "{error}"
        );
        assert!(error.contains("Issue #16"), "{error}");
        assert!(error.contains("--network testnet"), "{error}");
    }

    #[test]
    fn mainnet_live_gate_also_covers_the_legacy_alias() {
        let cli = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain(["--read-only", "false", "--max-notional-usd", "5000"])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert!(cli.is_live());
        let error = cli.validate().unwrap_err();
        assert!(
            error.contains("mainnet live execution is disabled"),
            "{error}"
        );
    }

    #[test]
    fn mainnet_resume_is_rejected_with_a_recovery_only_escape_hatch() {
        let cli = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain([
                    "--live",
                    "--max-notional-usd",
                    "5000",
                    "--resume",
                    "existing-run",
                ])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let error = cli.validate().unwrap_err();
        assert!(error.contains("--resume cannot continue"), "{error}");
        assert!(error.contains("--abandon-incomplete-run"), "{error}");
    }

    #[test]
    fn mainnet_abandon_recovery_remains_available_while_gate_is_closed() {
        let cli = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain([
                    "--live",
                    "--max-notional-usd",
                    "5000",
                    "--abandon-incomplete-run",
                ])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert!(cli.is_live());
        cli.validate().unwrap();
    }

    #[test]
    fn mainnet_read_only_remains_available() {
        let cli = Cli::try_parse_from(base_args()).unwrap();
        assert_eq!(cli.network, NetworkArg::Mainnet);
        assert!(cli.is_read_only());
        cli.validate().unwrap();
    }

    #[test]
    fn read_only_does_not_require_max_notional_usd() {
        // Default read-only (true), no --max-notional-usd given at all.
        let cli = Cli::try_parse_from(base_args()).unwrap();
        cli.validate().unwrap();
    }

    #[test]
    fn allow_custom_endpoints_defaults_to_false() {
        let cli = Cli::try_parse_from(base_args()).unwrap();
        assert!(!cli.allow_custom_endpoints);
    }

    #[test]
    fn allow_high_slippage_defaults_to_false() {
        let cli = Cli::try_parse_from(base_args()).unwrap();
        assert!(!cli.allow_high_slippage);
    }

    #[test]
    fn zero_poll_interval_is_rejected() {
        let cli = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain(["--trigger-poll-secs", "0"])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert!(cli.validate().unwrap_err().contains("--trigger-poll-secs"));
    }

    #[test]
    fn zero_wait_network_grace_is_rejected() {
        let cli = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain(["--wait-network-grace", "0s"])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert!(cli.validate().unwrap_err().contains("--wait-network-grace"));
    }

    #[test]
    fn wait_network_grace_parses_and_wires_into_trigger_config() {
        let cli = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain(["--wait-network-grace", "45m"])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        cli.validate().unwrap();
        assert_eq!(cli.wait_network_grace, Duration::from_secs(45 * 60));
        assert_eq!(
            cli.trigger_config().wait_network_grace,
            Duration::from_secs(45 * 60)
        );
    }

    #[test]
    fn humantime_durations_parse() {
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_secs(1800));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert!(parse_duration("banana").is_err());
    }

    #[test]
    fn network_arg_maps_to_matching_urls_and_domain() {
        let n: Network = NetworkArg::Testnet.into();
        assert!(!n.is_mainnet());
        assert!(HlConfig::new(n).exchange_url.contains("testnet"));
        let n: Network = NetworkArg::Mainnet.into();
        assert!(n.is_mainnet());
        assert!(HlConfig::new(n)
            .exchange_url
            .contains("api.hyperliquid.xyz"));
    }

    #[test]
    fn pk_is_not_accepted_as_a_flag() {
        // The key must come from the environment only — never argv.
        let r = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain([
                    "--hl-agent-pk",
                    "0x0123456789012345678901234567890123456789012345678901234567890123",
                ])
                .collect::<Vec<_>>(),
        );
        assert!(r.is_err(), "there must be no PK flag");
    }

    // === Issue #8: --expire-after ===

    #[test]
    fn zero_expire_after_is_rejected() {
        let cli = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain([
                    "--trigger-price",
                    "40",
                    "--trigger-when",
                    "above",
                    "--expire-after",
                    "0s",
                ])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert!(cli.validate().unwrap_err().contains("--expire-after"));
    }

    #[test]
    fn expire_after_equal_to_start_after_is_rejected() {
        let cli = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain(["--start-after", "10m", "--expire-after", "10m"])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let err = cli.validate().unwrap_err();
        assert!(err.contains("--expire-after"), "{err}");
        assert!(err.contains("--start-after"), "{err}");
    }

    #[test]
    fn expire_after_less_than_start_after_is_rejected() {
        let cli = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain(["--start-after", "10m", "--expire-after", "5m"])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let err = cli.validate().unwrap_err();
        assert!(err.contains("--expire-after"), "{err}");
        assert!(err.contains("--start-after"), "{err}");
    }

    #[test]
    fn expire_after_with_no_trigger_at_all_is_rejected() {
        let cli = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain(["--expire-after", "1h"])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let err = cli.validate().unwrap_err();
        assert!(err.contains("--expire-after"), "{err}");
    }

    #[test]
    fn expire_after_with_price_trigger_is_accepted() {
        let cli = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain([
                    "--trigger-price",
                    "40",
                    "--trigger-when",
                    "above",
                    "--expire-after",
                    "1h",
                ])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        cli.validate().unwrap();
        assert_eq!(
            cli.trigger_config().expire_after,
            Some(Duration::from_secs(3600))
        );
    }

    #[test]
    fn expire_after_greater_than_start_after_is_accepted() {
        let cli = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain(["--start-after", "5m", "--expire-after", "10m"])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        cli.validate().unwrap();
        assert_eq!(
            cli.trigger_config().expire_after,
            Some(Duration::from_secs(600))
        );
        assert_eq!(
            cli.trigger_config().start_after,
            Some(Duration::from_secs(300))
        );
    }

    #[test]
    fn expire_after_unset_defaults_to_none_and_does_not_change_immediate_or_time_only() {
        let cli = Cli::try_parse_from(base_args()).unwrap();
        cli.validate().unwrap();
        assert_eq!(cli.trigger_config().expire_after, None);
        assert!(cli.trigger_config().is_immediate());
    }

    #[test]
    fn describe_includes_expiry_wording_when_flag_is_set() {
        let cli = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain([
                    "--trigger-price",
                    "40",
                    "--trigger-when",
                    "above",
                    "--start-after",
                    "2h",
                    "--expire-after",
                    "4h",
                ])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        cli.validate().unwrap();
        let d = cli.trigger_config().describe();
        assert_eq!(
            d,
            "Trigger: price above 40 OR after 2h (whichever comes first); EXPIRES after 4h (no order if not fired)"
        );
    }

    // === Finding 3 (Issue #2 relocation): clock-skew check moved from
    // pre-wait to execution entry ===

    const TEST_PK: &str = "0x0123456789012345678901234567890123456789012345678901234567890123";
    // The address TEST_PK actually derives to (same key used by
    // tests/signing_cross_check.rs's vectors — see expected_address there).
    const AGENT: &str = "0x14791697260e4c9a71f18484c9f997b308e59325";
    const MASTER: &str = "0x00000000000000000000000000000000000000aa";

    #[test]
    fn flatten_confirmation_preflight_renders_every_token_bound_field_in_stable_order() {
        let confirmation = FlattenConfirmation {
            schema_version: FlattenConfirmation::SCHEMA_VERSION,
            network: "testnet".into(),
            master: Address::new(MASTER),
            symbol: Symbol::new("HYPE"),
            initial_szi: dec!(-2.50),
            close_side: Side::Long,
            max_close_size: dec!(2.50),
            max_notional_usd: dec!(1234.50),
            child_algo: "passive".into(),
            execution_deadline_unix_ms: 1_800_000_000_123,
        };
        assert_eq!(
            format_flatten_confirmation_preflight(&confirmation),
            format!(
                "FLATTEN PREFLIGHT:\nnetwork: testnet\nmaster: {MASTER}\nsymbol: HYPE\ninitial_szi: -2.5\nclose_side: long\nmax_close_size: 2.5\nmax_notional_usd: 1234.5\nchild_algo: passive\nexecution_deadline_unix_ms: 1800000000123"
            )
        );
    }

    #[test]
    fn resume_deadline_boundary_and_multiple_restarts_never_extend_the_window() {
        let absolute_deadline = 10_000;
        let attempts = [1_000, 7_500, 9_999];
        let remaining: Vec<_> = attempts
            .into_iter()
            .map(|now| {
                remaining_execution_window(absolute_deadline, now)
                    .expect("every pre-deadline resume has a positive window")
            })
            .collect();

        assert_eq!(
            remaining,
            [
                Duration::from_millis(9_000),
                Duration::from_millis(2_500),
                Duration::from_millis(1),
            ]
        );
        for (now, window) in attempts.into_iter().zip(remaining) {
            assert_eq!(
                now + u64::try_from(window.as_millis()).unwrap(),
                absolute_deadline,
                "each restarted process must reconstruct the same immutable deadline"
            );
        }
        assert!(
            remaining_execution_window(absolute_deadline, 9_999).is_some(),
            "a positive remainder shorter than one nominal slice interval is allowed; the send gate still owns the exact deadline"
        );
        assert_eq!(
            remaining_execution_window(absolute_deadline, absolute_deadline),
            None,
            "the exact boundary is reconciliation-only"
        );
        assert_eq!(
            remaining_execution_window(absolute_deadline, absolute_deadline + 1),
            None,
            "a post-boundary restart cannot regain an execution window"
        );
    }

    fn book_body_at(coin: &str, bid: &str, ask: &str, time_ms: i64) -> String {
        format!(
            r#"{{"coin":"{coin}","time":{time_ms},"levels":[
                [{{"px":"{bid}","sz":"100","n":1}}],
                [{{"px":"{ask}","sz":"100","n":1}}]]}}"#
        )
    }

    /// Regression guard for the invariant Finding 3 protects: a live
    /// time-only trigger (`--start-after`, no `--trigger-price`) must make
    /// ZERO network calls before its deadline elapses. This was true before
    /// commit 9986b46 (`wait_for_trigger` itself never touches the network
    /// for a time-only wait — see `time_only_trigger_fires_after_deadline_without_network`
    /// in `trigger.rs`) but 9986b46 broke it end-to-end by adding a
    /// dedicated pre-wait `l2Book` call in `main.rs::run()` purely for the
    /// clock-skew check. This test exercises `run_with_cli` (the `Cli`-
    /// injectable body of `run()`) against a mock server that has NO route
    /// registered for `/info` at all — any network call made before the
    /// wait completes would fail loudly (mockito 501s an unmatched request),
    /// which would surface as an error well before the deadline. Instead,
    /// the run must reach the deadline (virtual time elapses the full
    /// `--start-after`), THEN make its post-trigger `l2Book` call (which the
    /// mock now serves) for sizing/clock-skew, and finally fail on
    /// `--size`/exchange being absent from this minimal fixture — the
    /// precise failure mode after that point does not matter; what matters
    /// is that virtual time advanced the full wait duration first, proving
    /// no network call happened during the wait itself.
    // Real time, not `start_paused`: this test drives `run_with_cli` against
    // a real (localhost) mockito HTTP server, and mixing tokio's virtual-time
    // pause with real socket I/O is unreliable (the mock server's own
    // accept/response loop runs on real time regardless of the test's
    // virtual clock). A short, real --start-after keeps the test fast while
    // still proving the ordering.
    // `#[serial]`: this test mutates process-global env vars (HL_INFO_URL,
    // HL_EXCHANGE_URL) that `run_with_cli` reads mid-flight over several real
    // seconds. `cargo test` runs test fns concurrently on separate threads
    // within one process, and env vars are process-wide — without
    // serialization this races against the other env-mutating test below
    // (`issue2_live_skew_beyond_tolerance_...`), letting one test's
    // in-flight `run_with_cli` pick up the other's HL_INFO_URL/HL_EXCHANGE_URL
    // mid-run and hit the wrong mockito server (observed as a spurious
    // mockito 501 on an unmatched route, e.g. a "userRole probe" landing on
    // a server that never registered that route).
    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn issue2_live_time_only_trigger_makes_no_network_call_before_its_deadline() {
        let mut server = mockito::Server::new_async().await;
        // meta: needed at startup, before the wait begins — this IS allowed
        // (it happens at §4 step 2, well before the trigger wait).
        let _meta = server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({"type": "meta"})))
            .with_status(200)
            .with_body(r#"{"universe":[{"name":"HYPE","szDecimals":2,"maxLeverage":10,"onlyIsolated":false}]}"#)
            .create_async()
            .await;
        // l2Book: only served AFTER the trigger fires (post-trigger pre-flight
        // fetch + clock-skew read). If this were hit DURING the wait, the
        // virtual clock would not have advanced the full --start-after
        // duration by the time the run errors out below.
        let now_ms = chrono::Utc::now().timestamp_millis();
        let _book = server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "l2Book"}),
            ))
            .with_status(200)
            .with_body(book_body_at("HYPE", "49.9", "50.1", now_ms))
            .create_async()
            .await;

        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));

        let cli = Cli::try_parse_from([
            "hype-twap",
            "--symbol",
            "HYPE",
            "--side",
            "long",
            "--usd",
            "1500",
            "--duration",
            "5m",
            "--start-after",
            "2s",
            "--read-only",
            "true",
        ])
        .unwrap();

        let start = std::time::Instant::now();
        // This test only cares about the ordering up to (and just past) the
        // trigger firing; once triggered, the run continues into sizing and
        // the slice loop, which — with no `/exchange` mock registered here —
        // eventually retries a going-stale book for several seconds before
        // aborting. Bound the whole call so the test does not pay for that
        // unrelated retry storm.
        let _ = tokio::time::timeout(Duration::from_secs(10), run_with_cli(cli)).await;
        let elapsed = start.elapsed();

        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");

        // The load-bearing assertion: real time advanced by (at least) the
        // full 2s --start-after wait. If a pre-wait skew check like the one
        // 9986b46 added were still present, its l2Book call would be served
        // by the mock immediately at T=0 (the mock has no artificial delay),
        // making `elapsed` far short of 2s.
        assert!(
            elapsed >= Duration::from_secs(2),
            "a time-only trigger must not resolve before its --start-after deadline; \
             elapsed = {elapsed:?} (expected >= 2s) — a network call before the wait \
             would let this finish early"
        );
    }

    /// Adapts the "skew > 5s fails closed" behavior to assert it now fires at
    /// EXECUTION ENTRY (after "Triggered:", using the trigger's own
    /// snapshot) rather than pre-wait. Uses an immediate trigger (no wait) so
    /// the ONLY l2Book call in the whole run is the one that both fires the
    /// trigger (well, immediate doesn't need one) and doubles as the
    /// post-trigger pre-flight snapshot the skew check reads — proving the
    /// relocated check reads `server_ts_ms` off that snapshot rather than
    /// issuing a second dedicated l2Book call.
    // `#[serial]`: see the comment on
    // `issue2_live_time_only_trigger_makes_no_network_call_before_its_deadline`
    // above — same process-global env var race (this test additionally sets
    // HL_AGENT_PK / HL_AGENT_ADDRESS).
    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn issue2_live_skew_beyond_tolerance_fails_closed_at_execution_entry_not_prewait() {
        let state_dir = TempDir::new();
        let state_dir_arg = state_dir.path().to_string_lossy().into_owned();
        let mut server = mockito::Server::new_async().await;
        let _meta = server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({"type": "meta"})))
            .with_status(200)
            .with_body(r#"{"universe":[{"name":"HYPE","szDecimals":2,"maxLeverage":10,"onlyIsolated":false}]}"#)
            .create_async()
            .await;
        let _role = server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "userRole"}),
            ))
            .with_status(200)
            .with_body(format!(
                r#"{{"role":"agent","data":{{"user":"{MASTER}"}}}}"#
            ))
            .create_async()
            .await;
        // A time-only trigger (`--start-after`) is used so `wait_for_trigger`
        // itself makes NO l2Book call (Issue #6/#8 contract) — the ONLY
        // l2Book call in the whole run is the post-trigger pre-flight fetch
        // (`fetch_fresh_book(..., None)` at main.rs, `TriggerReason::Elapsed`
        // branch), which per the Finding 3 relocation is also the source of
        // `server_ts_ms` for the skew check. This proves the relocated check
        // reads the ALREADY-obtained snapshot rather than issuing a second,
        // dedicated l2Book call. The response is stamped far in the PAST
        // (skew < -5s) rather than the future, so it fails only the
        // clock-skew check and not `ValidatedMarketSnapshot`'s own
        // unconditional `MAX_FUTURE_SKEW_MS` (2s) future-skew check;
        // `--max-book-age-ms 0` disables the (unrelated) staleness half of
        // that same validation so an old timestamp does not fail for the
        // wrong reason.
        let now_ms = chrono::Utc::now().timestamp_millis();
        let skewed_ms = now_ms - 10_000; // 10s in the past: beyond MAX_CLOCK_SKEW_MS (5s)
        let _book = server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "l2Book"}),
            ))
            .with_status(200)
            .with_body(book_body_at("HYPE", "39.9", "40.1", skewed_ms))
            .expect(1)
            .create_async()
            .await;

        std::env::set_var("HL_AGENT_PK", TEST_PK);
        std::env::set_var("HL_AGENT_ADDRESS", AGENT);
        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));

        let cli = Cli::try_parse_from([
            "hype-twap",
            "--symbol",
            "HYPE",
            "--side",
            "long",
            "--usd",
            "1500",
            "--duration",
            "5m",
            "--network",
            "testnet",
            "--start-after",
            "1s",
            "--max-book-age-ms",
            "0",
            "--max-notional-usd",
            "1000000",
            // Issue #3: live + a custom endpoint override is rejected by
            // default; this test's mock server is loopback-only (mockito has
            // no TLS support), which the loopback carve-out on
            // validate_endpoint_override permits once this flag is passed.
            "--allow-custom-endpoints",
            "--state-dir",
            &state_dir_arg,
            "--read-only",
            "false",
        ])
        .unwrap();

        let result = run_with_cli(cli).await;

        std::env::remove_var("HL_AGENT_PK");
        std::env::remove_var("HL_AGENT_ADDRESS");
        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");

        let err = result.expect_err("a >5s clock skew must fail the run closed");
        assert!(err.contains("skew"), "{err}");

        // Exactly one l2Book call: the trigger-poll snapshot doubles as the
        // skew-check source. A pre-wait-style implementation would need TWO
        // l2Book calls (one for the old pre-wait skew preflight, one for the
        // trigger poll); the relocated implementation needs only one.
        _book.assert_async().await;
    }

    /// Issue #3: live + a custom `HL_INFO_URL`/`HL_EXCHANGE_URL` override
    /// (without `--allow-custom-endpoints`) must be rejected before ANY
    /// network call — not even the `/info meta` call that normally happens
    /// first at startup. The mock server below registers NO routes at all,
    /// so any network call would surface as a mockito 501 rather than the
    /// expected endpoint-override error, proving the rejection happens
    /// strictly before the first request.
    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn issue3_live_custom_endpoint_without_override_flag_rejected_before_any_network_call() {
        let server = mockito::Server::new_async().await;
        // No mocks registered — a network call here would 501.

        std::env::set_var("HL_AGENT_PK", TEST_PK);
        std::env::set_var("HL_AGENT_ADDRESS", AGENT);
        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));

        let cli = Cli::try_parse_from([
            "hype-twap",
            "--symbol",
            "HYPE",
            "--side",
            "long",
            "--usd",
            "1500",
            "--duration",
            "5m",
            "--network",
            "testnet",
            "--max-notional-usd",
            "5000",
            "--read-only",
            "false",
            // Deliberately NOT passing --allow-custom-endpoints.
        ])
        .unwrap();

        let result = run_with_cli(cli).await;

        std::env::remove_var("HL_AGENT_PK");
        std::env::remove_var("HL_AGENT_ADDRESS");
        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");

        let err = result.expect_err("a live custom endpoint override must be rejected");
        assert!(err.contains("--allow-custom-endpoints"), "{err}");
    }

    /// The temporary Issue #16 release gate is a true safety boundary, not
    /// documentation alone: it must fire before credentials, state, locks,
    /// journals, or HTTP can be touched.
    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn mainnet_live_gate_rejects_before_network_or_state_access() {
        let tmp = TempDir::new();
        let state_dir = tmp.path().join("state");
        let mut server = mockito::Server::new_async().await;
        let info = server.mock("POST", "/info").expect(0).create_async().await;

        std::env::set_var("HL_AGENT_PK", TEST_PK);
        std::env::set_var("HL_AGENT_ADDRESS", AGENT);
        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));

        let cli = Cli::try_parse_from([
            "hype-twap",
            "--symbol",
            "HYPE",
            "--side",
            "long",
            "--usd",
            "50",
            "--duration",
            "1m",
            "--max-notional-usd",
            "100",
            "--allow-custom-endpoints",
            "--state-dir",
            &state_dir.display().to_string(),
            "--live",
        ])
        .unwrap();
        let result = run_with_cli(cli).await;

        std::env::remove_var("HL_AGENT_PK");
        std::env::remove_var("HL_AGENT_ADDRESS");
        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");

        let error = result.expect_err("mainnet live must remain release-gated");
        assert!(
            error.contains("mainnet live execution is disabled"),
            "{error}"
        );
        info.assert_async().await;
        assert!(
            !state_dir.exists(),
            "the gate must fire before state-dir or lock creation"
        );
    }

    // === Issue #4: state-dir / journal / incomplete-run tests ===

    /// Hand-rolled temp-dir guard (no `tempfile` dependency, matching
    /// `src/journal.rs`'s own test module).
    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new() -> Self {
            let dir =
                std::env::temp_dir().join(format!("hype-twap-main-test-{}", uuid::Uuid::now_v7()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// **Finding 2 regression test.** Before the fix, a grace-timeout
    /// `FinalReport` hardcoded `filled_total: "0"` and
    /// `outcome_unknown_cloids: []` regardless of what the run had actually
    /// done — silently discarding real fills and hiding genuinely-unresolved
    /// cloids from the operator. This test seeds a journal with a prior
    /// Terminal fill (slice 1, 3 HYPE) AND a still-unresolved
    /// SubmittedUnknown cloid (slice 2) — i.e. exactly the state the journal
    /// would be in if the grace timer fired while slice 2 was ambiguous —
    /// then asserts `grace_timeout_report_fields` (the function the
    /// grace-timeout branch in `run_with_cli` calls) reports the REAL
    /// filled total and the REAL unresolved cloid, not zero/empty.
    #[test]
    fn grace_timeout_report_fields_reflect_actual_journal_state_not_zero_or_empty() {
        let tmp = TempDir::new();
        let state_dir = tmp.path().join("state");
        let filled_cloid = hype_trigger_twap::types::Cloid::new();
        let unresolved_cloid = hype_trigger_twap::types::Cloid::new();

        let mut j = hype_trigger_twap::journal::ExecutionJournal::start(
            &state_dir,
            "run-grace-timeout".into(),
            hype_trigger_twap::journal::RunHeader {
                run_id: "run-grace-timeout".into(),
                network: "testnet".into(),
                agent: Some(hype_trigger_twap::types::Address::new(AGENT)),
                master: Some(hype_trigger_twap::types::Address::new(MASTER)),
                symbol: hype_trigger_twap::types::Symbol::new("HYPE"),
                side: hype_trigger_twap::types::Side::Long,
                slices: 2,
                plan_hash: "irrelevant".into(),
                execution_fingerprint: None,
                started_at_unix_ms: 0,
                execution_deadline_unix_ms: None,
            },
        )
        .unwrap();
        j.record(&hype_trigger_twap::journal::JournalRecord::Prepared {
            slice_idx: 1,
            cloid: filled_cloid,
            nonce: None,
            symbol: hype_trigger_twap::types::Symbol::new("HYPE"),
            side: hype_trigger_twap::types::Side::Long,
            px: "50".into(),
            sz: "3".into(),
            tif: None,
        })
        .unwrap();
        j.record(&hype_trigger_twap::journal::JournalRecord::Terminal {
            slice_idx: 1,
            cloid: filled_cloid,
            status: "filled".into(),
            filled_sz: "3".into(),
            avg_px: Some("50".into()),
        })
        .unwrap();
        j.record(&hype_trigger_twap::journal::JournalRecord::Prepared {
            slice_idx: 2,
            cloid: unresolved_cloid,
            nonce: None,
            symbol: hype_trigger_twap::types::Symbol::new("HYPE"),
            side: hype_trigger_twap::types::Side::Long,
            px: "50".into(),
            sz: "2".into(),
            tif: None,
        })
        .unwrap();
        j.record(
            &hype_trigger_twap::journal::JournalRecord::SubmittedUnknown {
                slice_idx: 2,
                cloid: unresolved_cloid,
            },
        )
        .unwrap();
        // No FinalReport: this is the still-open state a grace-timeout
        // would fire against.

        let replay = grace_timeout_report_fields(&state_dir, "run-grace-timeout").unwrap();

        assert_eq!(
            replay.fill_totals.filled_sz,
            rust_decimal::Decimal::from(3),
            "must reflect the ACTUAL prior fill (3), not the hardcoded 0"
        );
        assert_eq!(
            replay.summary.unresolved_cloids(),
            vec![unresolved_cloid],
            "must list the ACTUAL still-unresolved cloid, not an empty list"
        );
    }

    #[test]
    fn grace_timeout_report_fields_refuse_missing_journal_instead_of_fabricating_zeroes() {
        let tmp = TempDir::new();
        let error = grace_timeout_report_fields(tmp.path(), "missing-run").unwrap_err();
        assert!(error.contains("cannot read journal"), "{error}");
    }

    fn live_cli(extra: &[&str], state_dir: &std::path::Path) -> Vec<String> {
        let mut args: Vec<String> = vec![
            "hype-twap".into(),
            "--symbol".into(),
            "HYPE".into(),
            "--side".into(),
            "long".into(),
            "--usd".into(),
            "50".into(),
            "--duration".into(),
            "2s".into(),
            "--slices".into(),
            "1".into(),
            "--max-notional-usd".into(),
            "1000000".into(),
            "--master-address".into(),
            MASTER.into(),
            "--allow-custom-endpoints".into(),
            "--read-only".into(),
            "false".into(),
            "--state-dir".into(),
            state_dir.display().to_string(),
        ];
        args.extend(extra.iter().map(|s| (*s).to_string()));
        args
    }

    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn resume_requires_an_explicit_master_before_any_external_api_call() {
        let tmp = TempDir::new();
        let state_dir = tmp.path().join("state");
        let mut server = mockito::Server::new_async().await;
        let info = server.mock("POST", "/info").expect(0).create_async().await;

        let prior_master = std::env::var_os("HL_MASTER_ADDRESS");
        std::env::remove_var("HL_MASTER_ADDRESS");
        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));
        let cli = Cli::try_parse_from([
            "hype-twap",
            "--symbol",
            "HYPE",
            "--side",
            "long",
            "--usd",
            "50",
            "--duration",
            "1m",
            "--network",
            "testnet",
            "--max-notional-usd",
            "100",
            "--allow-custom-endpoints",
            "--read-only",
            "false",
            "--state-dir",
            &state_dir.display().to_string(),
            "--resume",
            "run-never-read",
        ])
        .unwrap();

        let result = run_with_cli(cli).await;

        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");
        if let Some(value) = prior_master {
            std::env::set_var("HL_MASTER_ADDRESS", value);
        }
        let error = result.expect_err("resume without an explicit master must fail closed");
        assert!(error.contains("require --master-address"), "{error}");
        assert!(error.contains("before any external API call"), "{error}");
        info.assert_async().await;
        assert!(
            !state_dir.exists(),
            "identity refusal must happen before journal/state access"
        );
    }

    #[test]
    fn resume_identity_rejects_each_header_field_before_a_client_exists() {
        type Mutation = Box<dyn Fn(&mut hype_trigger_twap::journal::RunHeader)>;
        let cases: Vec<(&str, Mutation)> = vec![
            (
                "network",
                Box::new(|header| header.network = "mainnet".into()),
            ),
            (
                "agent",
                Box::new(|header| {
                    header.agent = Some(Address::new("0x3333333333333333333333333333333333333333"))
                }),
            ),
            (
                "master",
                Box::new(|header| {
                    header.master = Some(Address::new("0x4444444444444444444444444444444444444444"))
                }),
            ),
            (
                "symbol",
                Box::new(|header| header.symbol = Symbol::new("BTC")),
            ),
            ("side", Box::new(|header| header.side = Side::Short)),
        ];

        for (field, mutate) in cases {
            let tmp = TempDir::new();
            let state_dir = tmp.path().join("state");
            let run_id = format!("identity-{field}");
            let mut header = hype_trigger_twap::journal::RunHeader {
                run_id: run_id.clone(),
                network: "testnet".into(),
                agent: Some(Address::new(AGENT)),
                master: Some(Address::new(MASTER)),
                symbol: Symbol::new("HYPE"),
                side: Side::Long,
                slices: 1,
                plan_hash: "legacy-is-readable-for-identity".into(),
                execution_fingerprint: None,
                started_at_unix_ms: 1,
                execution_deadline_unix_ms: Some(2),
            };
            mutate(&mut header);
            drop(
                hype_trigger_twap::journal::ExecutionJournal::start(
                    &state_dir,
                    run_id.clone(),
                    header,
                )
                .unwrap(),
            );

            // This validator has no HTTP client parameter. Reaching this
            // typed mismatch therefore proves rejection precedes meta,
            // userRole, orderStatus, and every other external request.
            let error = validated_resume_replay(
                &state_dir,
                &run_id,
                &Network::Testnet,
                Some(&Address::new(AGENT)),
                Some(&Address::new(MASTER)),
                &Symbol::new("HYPE"),
                Some(Side::Long),
            )
            .unwrap_err();
            assert!(error.contains(field), "{field}: {error}");
        }
    }

    #[test]
    fn resume_rejects_each_typed_fingerprint_field_before_order_construction() {
        let cli = Cli::try_parse_from([
            "hype-twap",
            "--symbol",
            "HYPE",
            "--side",
            "long",
            "--size",
            "10.0",
            "--network",
            "testnet",
            "--duration",
            "1m",
            "--slices",
            "10",
            "--max-notional-usd",
            "1000.0",
            "--child-algo",
            "market",
            "--read-only",
            "false",
        ])
        .unwrap();
        cli.validate().unwrap();
        let deadline = 1_900_000_000_000;
        let plan = TwapPlan {
            symbol: Symbol::new("HYPE"),
            side: Side::Long,
            asset_index: 0,
            sz_decimals: 2,
            per_slice: Decimal::ONE,
            total_adjusted: Decimal::TEN,
            total_requested: Decimal::TEN,
            slices: 10,
            duration: Duration::from_secs(60),
            absolute_deadline_unix_ms: Some(deadline),
            slippage_bps: cli.slippage_bps,
            max_book_age_ms: cli.max_book_age_ms,
            settle_retries: cli.settle_retries,
            read_only: false,
            reduce_only: false,
            max_notional_usd: Decimal::from(1000),
            agent: Some(Address::new(AGENT)),
            master: Some(Address::new(MASTER)),
            child_algo: ChildAlgo::Market,
            follow_poll_secs: cli.follow_poll_secs,
            follow_repost_secs: cli.follow_repost_secs,
            follow_threshold_bps: cli.follow_threshold_bps,
        };
        let base = execution_fingerprint(&Network::Testnet, &plan, None, &cli, None);
        type Mutation = Box<dyn Fn(&mut hype_trigger_twap::journal::ExecutionPlanFingerprint)>;
        let cases: Vec<(&str, Mutation)> = vec![
            ("version", Box::new(|f| f.version += 1)),
            ("symbol", Box::new(|f| f.symbol = "BTC".into())),
            ("side", Box::new(|f| f.side = "short".into())),
            ("request_mode", Box::new(|f| f.request_mode = "usd".into())),
            ("request_value", Box::new(|f| f.request_value = "11".into())),
            ("per_slice", Box::new(|f| f.per_slice = "2".into())),
            (
                "total_adjusted",
                Box::new(|f| f.total_adjusted = "11".into()),
            ),
            (
                "total_requested",
                Box::new(|f| f.total_requested = "11".into()),
            ),
            ("slices", Box::new(|f| f.slices = 11)),
            ("duration_ms", Box::new(|f| f.duration_ms += 1)),
            ("slippage_bps", Box::new(|f| f.slippage_bps = "21".into())),
            (
                "max_notional_usd",
                Box::new(|f| f.max_notional_usd = "999".into()),
            ),
            ("max_book_age_ms", Box::new(|f| f.max_book_age_ms += 1)),
            ("settle_retries", Box::new(|f| f.settle_retries += 1)),
            ("child_algo", Box::new(|f| f.child_algo = "passive".into())),
            ("follow_poll_secs", Box::new(|f| f.follow_poll_secs += 1)),
            (
                "follow_repost_secs",
                Box::new(|f| f.follow_repost_secs += 1),
            ),
            (
                "follow_threshold_bps",
                Box::new(|f| f.follow_threshold_bps = "2".into()),
            ),
            ("network", Box::new(|f| f.network = "mainnet".into())),
            ("agent", Box::new(|f| f.agent = None)),
            ("master", Box::new(|f| f.master = None)),
            (
                "position_mode",
                Box::new(|f| f.position_mode = Some("target_sz".into())),
            ),
            (
                "initial_position_szi",
                Box::new(|f| f.initial_position_szi = Some("0".into())),
            ),
            (
                "target_position_szi",
                Box::new(|f| f.target_position_szi = Some("10".into())),
            ),
            (
                "position_requested_value",
                Box::new(|f| f.position_requested_value = Some("10".into())),
            ),
            (
                "position_reference_price",
                Box::new(|f| f.position_reference_price = Some("50".into())),
            ),
            (
                "position_phases",
                Box::new(|f| {
                    f.position_phases =
                        vec![hype_trigger_twap::journal::PositionPhaseFingerprint {
                            kind: "adjust".into(),
                            side: "long".into(),
                            size: "10".into(),
                            reduce_only: false,
                        }];
                }),
            ),
            ("reduce_only", Box::new(|f| f.reduce_only = true)),
            (
                "absolute_deadline_unix_ms",
                Box::new(move |f| f.absolute_deadline_unix_ms = Some(deadline + 1)),
            ),
        ];

        let valid_replay = |fingerprint| {
            hype_trigger_twap::journal::ValidatedJournalReplay::replay(&[
                hype_trigger_twap::journal::JournalRecord::Header(
                    hype_trigger_twap::journal::RunHeader {
                        run_id: "fingerprint-test".into(),
                        network: "testnet".into(),
                        agent: Some(Address::new(AGENT)),
                        master: Some(Address::new(MASTER)),
                        symbol: Symbol::new("HYPE"),
                        side: Side::Long,
                        slices: 10,
                        plan_hash: "legacy-compat-only".into(),
                        execution_fingerprint: Some(fingerprint),
                        started_at_unix_ms: deadline - 60_000,
                        execution_deadline_unix_ms: Some(deadline),
                    },
                ),
            ])
            .unwrap()
        };
        let replay = valid_replay(base.clone());
        assert!(validate_resume_execution_fingerprint(
            "fingerprint-test",
            &replay,
            &cli,
            &Network::Testnet,
            Some(&Address::new(AGENT)),
            Some(&Address::new(MASTER)),
            2,
        )
        .unwrap()
        .is_none());

        let mut mismatched_header = valid_replay(base.clone()).summary.header.expect("header");
        mismatched_header.slices = 9;
        let error = hype_trigger_twap::journal::ValidatedJournalReplay::replay(&[
            hype_trigger_twap::journal::JournalRecord::Header(mismatched_header),
        ])
        .expect_err("header/fingerprint slices mismatch");
        assert!(
            matches!(
                error,
                hype_trigger_twap::journal::JournalReplayError::InvalidFingerprint {
                    field: "slices",
                    ..
                }
            ),
            "{error}"
        );

        for (field, mutate) in cases {
            let mut changed = base.clone();
            mutate(&mut changed);
            let error = match hype_trigger_twap::journal::ValidatedJournalReplay::replay(&[
                hype_trigger_twap::journal::JournalRecord::Header(
                    hype_trigger_twap::journal::RunHeader {
                        run_id: "fingerprint-test".into(),
                        network: "testnet".into(),
                        agent: Some(Address::new(AGENT)),
                        master: Some(Address::new(MASTER)),
                        symbol: Symbol::new("HYPE"),
                        side: Side::Long,
                        slices: 10,
                        plan_hash: "legacy-compat-only".into(),
                        execution_fingerprint: Some(changed),
                        started_at_unix_ms: deadline - 60_000,
                        execution_deadline_unix_ms: Some(deadline),
                    },
                ),
            ]) {
                Ok(replay) => validate_resume_execution_fingerprint(
                    "fingerprint-test",
                    &replay,
                    &cli,
                    &Network::Testnet,
                    Some(&Address::new(AGENT)),
                    Some(&Address::new(MASTER)),
                    2,
                )
                .expect_err(field),
                Err(error) => error.to_string(),
            };
            let field_words = field.replace('_', " ");
            assert!(
                error.contains(field)
                    || error.contains(&field_words)
                    || error.contains("position-only fields")
                    || error.contains("position-only values")
                    || error.contains("position target"),
                "{field}: {error}"
            );
        }
    }

    fn resumable_header_from_probe(
        run_id: &str,
        records: &[hype_trigger_twap::journal::JournalRecord],
    ) -> hype_trigger_twap::journal::RunHeader {
        let mut header = match records.first() {
            Some(hype_trigger_twap::journal::JournalRecord::Header(header)) => header.clone(),
            other => panic!("expected probe Header, got {other:?}"),
        };
        // The probe's own short deadline may have elapsed while the fixture
        // was copied. Give the synthetic crash journal a future immutable
        // deadline while preserving every other canonical plan field.
        let deadline = wall_clock_now_ms().saturating_add(60_000);
        header.run_id = run_id.to_owned();
        header.started_at_unix_ms = wall_clock_now_ms();
        header.execution_deadline_unix_ms = Some(deadline);
        header
            .execution_fingerprint
            .as_mut()
            .expect("fresh probe writes a typed fingerprint")
            .absolute_deadline_unix_ms = Some(deadline);
        header
    }

    /// Registers the full mock set a complete one-slice live run needs: meta,
    /// userRole, l2Book (served for both the startup snapshot and the
    /// pre-flight/slice-loop fetch — `.expect_at_least(1)` since exactly how
    /// many times it is called is an implementation detail this test does
    /// not pin), and one filled `/exchange` response.
    async fn mock_full_live_run(server: &mut mockito::ServerGuard) {
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({"type": "meta"})))
            .with_status(200)
            .with_body(r#"{"universe":[{"name":"HYPE","szDecimals":2,"maxLeverage":10,"onlyIsolated":false}]}"#)
            .create_async()
            .await;
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "userRole"}),
            ))
            .with_status(200)
            .with_body(format!(
                r#"{{"role":"agent","data":{{"user":"{MASTER}"}}}}"#
            ))
            .create_async()
            .await;
        let now_ms = chrono::Utc::now().timestamp_millis();
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "l2Book"}),
            ))
            .with_status(200)
            .with_body(book_body_at("HYPE", "49.9", "50.1", now_ms))
            .expect_at_least(1)
            .create_async()
            .await;
        server
            .mock("POST", "/exchange")
            .with_status(200)
            .with_body(
                r#"{"status":"ok","response":{"type":"order","data":{"statuses":[
                    {"filled":{"oid":1,"totalSz":"1","avgPx":"50"}}]}}}"#,
            )
            .create_async()
            .await;
    }

    async fn mock_position_preflight(
        server: &mut mockito::ServerGuard,
        position_body: &str,
        include_role: bool,
    ) {
        mock_position_preflight_common(server, include_role).await;
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "clearinghouseState"}),
            ))
            .with_status(200)
            .with_body(position_body)
            .expect(1)
            .create_async()
            .await;
    }

    async fn mock_position_preflight_common(server: &mut mockito::ServerGuard, include_role: bool) {
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({"type": "meta"})))
            .with_status(200)
            .with_body(r#"{"universe":[{"name":"HYPE","szDecimals":2,"maxLeverage":10,"onlyIsolated":false}]}"#)
            .create_async()
            .await;
        if include_role {
            server
                .mock("POST", "/info")
                .match_body(mockito::Matcher::PartialJson(
                    serde_json::json!({"type": "userRole"}),
                ))
                .with_status(200)
                .with_body(format!(
                    r#"{{"role":"agent","data":{{"user":"{MASTER}"}}}}"#
                ))
                .create_async()
                .await;
        }
        let now_ms = chrono::Utc::now().timestamp_millis();
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "l2Book"}),
            ))
            .with_status(200)
            .with_body(book_body_at("HYPE", "49.9", "50.1", now_ms))
            .expect_at_least(1)
            .create_async()
            .await;
    }

    async fn mock_position_preflight_sequence(
        server: &mut mockito::ServerGuard,
        position_bodies: &[&str],
        include_role: bool,
    ) -> mockito::Mock {
        assert!(!position_bodies.is_empty());
        mock_position_preflight_common(server, include_role).await;
        let bodies: Vec<Vec<u8>> = position_bodies
            .iter()
            .map(|body| body.as_bytes().to_vec())
            .collect();
        let response_count = bodies.len();
        let next = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "clearinghouseState"}),
            ))
            .with_status(200)
            .with_body_from_request(move |_| {
                let index = next.fetch_add(1, Ordering::Relaxed);
                bodies
                    .get(index)
                    .or_else(|| bodies.last())
                    .expect("position response sequence is non-empty")
                    .clone()
            })
            .expect(response_count)
            .create_async()
            .await
    }

    async fn mock_position_snapshot(server: &mut mockito::ServerGuard, body: &str) {
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "clearinghouseState"}),
            ))
            .with_status(200)
            .with_body(body)
            .expect(1)
            .create_async()
            .await;
    }

    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn position_invalid_state_and_flat_noop_send_zero_exchange_calls() {
        for (position_body, expect_success) in [
            (r#"{"assetPositions":{}}"#, false),
            (r#"{"assetPositions":[]}"#, true),
        ] {
            let mut server = mockito::Server::new_async().await;
            mock_position_preflight(&mut server, position_body, false).await;
            let exchange = server
                .mock("POST", "/exchange")
                .expect(0)
                .create_async()
                .await;
            std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
            std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));
            let cli = Cli::try_parse_from([
                "hype-twap",
                "--symbol",
                "HYPE",
                "--flatten",
                "--master-address",
                MASTER,
                "--duration",
                "1m",
                "--slices",
                "1",
            ])
            .unwrap();
            let result = run_with_cli(cli).await;
            std::env::remove_var("HL_INFO_URL");
            std::env::remove_var("HL_EXCHANGE_URL");
            assert_eq!(result.is_ok(), expect_success, "{result:?}");
            exchange.assert_async().await;
        }
    }

    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn read_only_position_noop_emits_complete_state_free_lifecycle() {
        let tmp = TempDir::new();
        let state_dir = tmp.path().join("would-be-state-dir");
        let event_path = tmp.path().join("position-noop-events.jsonl");
        let mut server = mockito::Server::new_async().await;
        mock_position_preflight(&mut server, r#"{"assetPositions":[]}"#, false).await;
        let exchange = server
            .mock("POST", "/exchange")
            .expect(0)
            .create_async()
            .await;
        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));
        let cli = Cli::try_parse_from([
            "hype-twap",
            "--symbol",
            "HYPE",
            "--flatten",
            "--master-address",
            MASTER,
            "--duration",
            "1m",
            "--slices",
            "1",
            "--state-dir",
            &state_dir.display().to_string(),
            "--event-jsonl",
            &event_path.display().to_string(),
        ])
        .unwrap();
        let result = run_with_cli(cli).await;
        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");

        assert_eq!(result.unwrap(), ExitCode::SUCCESS);
        assert!(!state_dir.exists());
        exchange.assert_async().await;
        let events: Vec<hype_trigger_twap::observability::ExecutionEvent> =
            std::fs::read_to_string(event_path)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
        assert!(matches!(
            events.first().map(|event| &event.payload),
            Some(
                hype_trigger_twap::observability::ExecutionEventPayload::RunStarted {
                    planned_slices: 0,
                    mode: ExecutionMode::ReadOnly,
                    ..
                }
            )
        ));
        assert!(matches!(
            events.last().map(|event| &event.payload),
            Some(
                hype_trigger_twap::observability::ExecutionEventPayload::FinalReport {
                    outcome: hype_trigger_twap::observability::RunOutcome::Completed,
                    completed_slices: 0,
                }
            )
        ));
    }

    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn live_flatten_without_exact_confirmation_sends_zero_exchange_calls() {
        let tmp = TempDir::new();
        let state_dir = tmp.path().join("state");
        let mut server = mockito::Server::new_async().await;
        mock_position_preflight(
            &mut server,
            r#"{"assetPositions":[{"position":{"coin":"HYPE","szi":"1"}}]}"#,
            true,
        )
        .await;
        let exchange = server
            .mock("POST", "/exchange")
            .expect(0)
            .create_async()
            .await;
        std::env::set_var("HL_AGENT_PK", TEST_PK);
        std::env::set_var("HL_AGENT_ADDRESS", AGENT);
        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));
        let deadline = wall_clock_now_ms().saturating_add(60_000).to_string();
        let args = vec![
            "hype-twap".to_owned(),
            "--symbol".into(),
            "HYPE".into(),
            "--flatten".into(),
            "--network".into(),
            "testnet".into(),
            "--duration".into(),
            "1m".into(),
            "--slices".into(),
            "1".into(),
            "--flatten-deadline-unix-ms".into(),
            deadline,
            "--max-notional-usd".into(),
            "1000".into(),
            "--master-address".into(),
            MASTER.into(),
            "--allow-custom-endpoints".into(),
            "--read-only".into(),
            "false".into(),
            "--state-dir".into(),
            state_dir.display().to_string(),
        ];
        let result = run_with_cli(Cli::try_parse_from(args).unwrap()).await;
        std::env::remove_var("HL_AGENT_PK");
        std::env::remove_var("HL_AGENT_ADDRESS");
        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");
        let error = result.unwrap_err();
        assert!(error.contains("--confirm-flatten"), "{error}");
        exchange.assert_async().await;
    }

    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn resumed_live_flatten_without_original_confirmation_sends_zero_exchange_calls() {
        let tmp = TempDir::new();
        let state_dir = tmp.path().join("state");
        let run_id = "flatten-resume-needs-confirmation";
        let deadline = wall_clock_now_ms().saturating_add(60_000);
        let args = vec![
            "hype-twap".to_owned(),
            "--symbol".into(),
            "HYPE".into(),
            "--flatten".into(),
            "--network".into(),
            "testnet".into(),
            "--duration".into(),
            "1m".into(),
            "--slices".into(),
            "1".into(),
            "--max-notional-usd".into(),
            "1000".into(),
            "--master-address".into(),
            MASTER.into(),
            "--allow-custom-endpoints".into(),
            "--read-only".into(),
            "false".into(),
            "--state-dir".into(),
            state_dir.display().to_string(),
            "--resume".into(),
            run_id.into(),
        ];
        let cli = Cli::try_parse_from(&args).unwrap();
        let frozen_position = PositionExecutionPlan::flatten(
            &SignedPerpPosition {
                symbol: Symbol::new("HYPE"),
                szi: Decimal::ONE,
            },
            &Symbol::new("HYPE"),
            2,
        )
        .unwrap();
        let plan = TwapPlan {
            symbol: Symbol::new("HYPE"),
            side: Side::Short,
            asset_index: 0,
            sz_decimals: 2,
            per_slice: Decimal::ONE,
            total_adjusted: Decimal::ONE,
            total_requested: Decimal::ONE,
            slices: 1,
            duration: Duration::from_secs(60),
            absolute_deadline_unix_ms: Some(deadline),
            slippage_bps: cli.slippage_bps,
            max_book_age_ms: cli.max_book_age_ms,
            settle_retries: cli.settle_retries,
            read_only: false,
            reduce_only: true,
            max_notional_usd: Decimal::from(1000),
            agent: Some(Address::new(AGENT)),
            master: Some(Address::new(MASTER)),
            child_algo: ChildAlgo::Market,
            follow_poll_secs: cli.follow_poll_secs,
            follow_repost_secs: cli.follow_repost_secs,
            follow_threshold_bps: cli.follow_threshold_bps,
        };
        let fingerprint =
            execution_fingerprint(&Network::Testnet, &plan, Some(&frozen_position), &cli, None);
        drop(
            hype_trigger_twap::journal::ExecutionJournal::start(
                &state_dir,
                run_id.into(),
                hype_trigger_twap::journal::RunHeader {
                    run_id: run_id.into(),
                    network: "testnet".into(),
                    agent: Some(Address::new(AGENT)),
                    master: Some(Address::new(MASTER)),
                    symbol: Symbol::new("HYPE"),
                    side: Side::Short,
                    slices: 1,
                    plan_hash: "typed-flatten-plan".into(),
                    execution_fingerprint: Some(fingerprint),
                    started_at_unix_ms: deadline.saturating_sub(60_000),
                    execution_deadline_unix_ms: Some(deadline),
                },
            )
            .unwrap(),
        );

        let mut server = mockito::Server::new_async().await;
        mock_position_preflight(
            &mut server,
            r#"{"assetPositions":[{"position":{"coin":"HYPE","szi":"1"}}]}"#,
            true,
        )
        .await;
        let exchange = server
            .mock("POST", "/exchange")
            .expect(0)
            .create_async()
            .await;
        std::env::set_var("HL_AGENT_PK", TEST_PK);
        std::env::set_var("HL_AGENT_ADDRESS", AGENT);
        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));

        let result = run_with_cli(cli).await;

        std::env::remove_var("HL_AGENT_PK");
        std::env::remove_var("HL_AGENT_ADDRESS");
        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");
        let error = result.expect_err("a resumed live flatten still requires confirmation");
        assert!(error.contains("--confirm-flatten"), "{error}");
        exchange.assert_async().await;
    }

    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn confirmed_flatten_sends_only_reduce_only_and_verifies_flat() {
        let tmp = TempDir::new();
        let state_dir = tmp.path().join("state");
        let mut server = mockito::Server::new_async().await;
        mock_position_preflight(
            &mut server,
            r#"{"assetPositions":[{"position":{"coin":"HYPE","szi":"1"}}]}"#,
            true,
        )
        .await;
        // The position-aware runner now checks the authoritative size again
        // immediately before placing.  Keep this snapshot distinct from the
        // terminal flat verification below.
        mock_position_snapshot(
            &mut server,
            r#"{"assetPositions":[{"position":{"coin":"HYPE","szi":"1"}}]}"#,
        )
        .await;
        // The second fresh snapshot is the immediate commit-point guard,
        // after the slice-level guard above and before `/exchange`.
        mock_position_snapshot(
            &mut server,
            r#"{"assetPositions":[{"position":{"coin":"HYPE","szi":"1"}}]}"#,
        )
        .await;
        let final_flat = server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "clearinghouseState"}),
            ))
            .with_status(200)
            .with_body(r#"{"assetPositions":[]}"#)
            .expect(1)
            .create_async()
            .await;
        let reduce_order = server
            .mock("POST", "/exchange")
            .match_body(mockito::Matcher::Regex(r#""r":true"#.into()))
            .with_status(200)
            .with_body(
                r#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"filled":{"oid":1,"totalSz":"1","avgPx":"50"}}]}}}"#,
            )
            .expect(1)
            .create_async()
            .await;

        std::env::set_var("HL_AGENT_PK", TEST_PK);
        std::env::set_var("HL_AGENT_ADDRESS", AGENT);
        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));
        let deadline = wall_clock_now_ms().saturating_add(60_000);
        let confirmation = FlattenConfirmation {
            schema_version: FlattenConfirmation::SCHEMA_VERSION,
            network: Network::Testnet.to_string(),
            master: Address::new(MASTER),
            symbol: Symbol::new("HYPE"),
            initial_szi: Decimal::ONE,
            close_side: Side::Short,
            max_close_size: Decimal::ONE,
            max_notional_usd: Decimal::from(1000),
            child_algo: "market".into(),
            execution_deadline_unix_ms: deadline,
        };
        let token = confirmation.token().unwrap();
        let args = vec![
            "hype-twap".to_owned(),
            "--symbol".into(),
            "HYPE".into(),
            "--flatten".into(),
            "--network".into(),
            "testnet".into(),
            "--duration".into(),
            "1m".into(),
            "--slices".into(),
            "1".into(),
            "--flatten-deadline-unix-ms".into(),
            deadline.to_string(),
            "--confirm-flatten".into(),
            token,
            "--max-notional-usd".into(),
            "1000".into(),
            "--master-address".into(),
            MASTER.into(),
            "--allow-custom-endpoints".into(),
            "--read-only".into(),
            "false".into(),
            "--state-dir".into(),
            state_dir.display().to_string(),
        ];
        let result = run_with_cli(Cli::try_parse_from(args).unwrap()).await;
        std::env::remove_var("HL_AGENT_PK");
        std::env::remove_var("HL_AGENT_ADDRESS");
        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");
        assert_eq!(result.unwrap(), ExitCode::SUCCESS);
        reduce_order.assert_async().await;
        final_flat.assert_async().await;
    }

    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn already_flat_reduce_only_rejection_completes_after_exact_authoritative_recheck() {
        let tmp = TempDir::new();
        let state_dir = tmp.path().join("state");
        let mut server = mockito::Server::new_async().await;
        mock_position_preflight(
            &mut server,
            r#"{"assetPositions":[{"position":{"coin":"HYPE","szi":"1"}}]}"#,
            true,
        )
        .await;
        // Pre-place guard: the original long is still present, so attempting
        // the reduce-only close is legitimate.
        mock_position_snapshot(
            &mut server,
            r#"{"assetPositions":[{"position":{"coin":"HYPE","szi":"1"}}]}"#,
        )
        .await;
        // Immediate commit-point guard: the close is still required at the
        // last possible point before the exchange request.
        mock_position_snapshot(
            &mut server,
            r#"{"assetPositions":[{"position":{"coin":"HYPE","szi":"1"}}]}"#,
        )
        .await;
        // An external fill wins the race and HL rejects our now-unneeded
        // reduce-only close.  Both the abort-path check and normal terminal
        // verification must see exact flat before success is recorded.
        mock_position_snapshot(&mut server, r#"{"assetPositions":[]}"#).await;
        mock_position_snapshot(&mut server, r#"{"assetPositions":[]}"#).await;
        let rejected_close = server
            .mock("POST", "/exchange")
            .match_body(mockito::Matcher::Regex(r#""r":true"#.into()))
            .with_status(200)
            .with_body(
                r#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"error":"Reduce only order would increase position"}]}}}"#,
            )
            .expect(1)
            .create_async()
            .await;

        std::env::set_var("HL_AGENT_PK", TEST_PK);
        std::env::set_var("HL_AGENT_ADDRESS", AGENT);
        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));
        let deadline = wall_clock_now_ms().saturating_add(60_000);
        let token = FlattenConfirmation {
            schema_version: FlattenConfirmation::SCHEMA_VERSION,
            network: Network::Testnet.to_string(),
            master: Address::new(MASTER),
            symbol: Symbol::new("HYPE"),
            initial_szi: Decimal::ONE,
            close_side: Side::Short,
            max_close_size: Decimal::ONE,
            max_notional_usd: Decimal::from(1000),
            child_algo: "market".into(),
            execution_deadline_unix_ms: deadline,
        }
        .token()
        .unwrap();
        let args = vec![
            "hype-twap".to_owned(),
            "--symbol".into(),
            "HYPE".into(),
            "--flatten".into(),
            "--network".into(),
            "testnet".into(),
            "--duration".into(),
            "1m".into(),
            "--slices".into(),
            "1".into(),
            "--flatten-deadline-unix-ms".into(),
            deadline.to_string(),
            "--confirm-flatten".into(),
            token,
            "--max-notional-usd".into(),
            "1000".into(),
            "--allow-custom-endpoints".into(),
            "--read-only".into(),
            "false".into(),
            "--state-dir".into(),
            state_dir.display().to_string(),
        ];
        let result = run_with_cli(Cli::try_parse_from(args).unwrap()).await;
        std::env::remove_var("HL_AGENT_PK");
        std::env::remove_var("HL_AGENT_ADDRESS");
        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");

        assert_eq!(result.unwrap(), ExitCode::SUCCESS);
        rejected_close.assert_async().await;
        let run_id = std::fs::read_dir(state_dir.join("runs"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .file_name()
            .to_string_lossy()
            .into_owned();
        let records =
            hype_trigger_twap::journal::ExecutionJournal::read_all(&state_dir, &run_id).unwrap();
        let replay = hype_trigger_twap::journal::ValidatedJournalReplay::replay(&records).unwrap();
        assert_eq!(replay.summary.last_final_report_completed, Some(true));
        assert!(replay.summary.unresolved_cloids().is_empty());
    }

    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn zero_crossing_residual_position_blocks_non_reduce_open_order() {
        let tmp = TempDir::new();
        let state_dir = tmp.path().join("state");
        let mut server = mockito::Server::new_async().await;
        let positions = mock_position_preflight_sequence(
            &mut server,
            &[
                r#"{"assetPositions":[{"position":{"coin":"HYPE","szi":"1"}}]}"#,
                // Slice-level and commit-point guards before the reduce-only
                // child, followed by the residual exact-flat verification.
                r#"{"assetPositions":[{"position":{"coin":"HYPE","szi":"1"}}]}"#,
                r#"{"assetPositions":[{"position":{"coin":"HYPE","szi":"1"}}]}"#,
                r#"{"assetPositions":[{"position":{"coin":"HYPE","szi":"0.1"}}]}"#,
            ],
            true,
        )
        .await;
        let close = server
            .mock("POST", "/exchange")
            .match_body(mockito::Matcher::Regex(r#""r":true"#.into()))
            .with_status(200)
            .with_body(
                r#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"filled":{"oid":1,"totalSz":"1","avgPx":"50"}}]}}}"#,
            )
            .expect(1)
            .create_async()
            .await;
        let forbidden_open = server
            .mock("POST", "/exchange")
            .match_body(mockito::Matcher::Regex(r#""r":false"#.into()))
            .expect(0)
            .create_async()
            .await;
        std::env::set_var("HL_AGENT_PK", TEST_PK);
        std::env::set_var("HL_AGENT_ADDRESS", AGENT);
        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));
        let args = vec![
            "hype-twap".to_owned(),
            "--symbol".into(),
            "HYPE".into(),
            "--target-sz".into(),
            "-1".into(),
            "--network".into(),
            "testnet".into(),
            "--duration".into(),
            "1m".into(),
            "--slices".into(),
            "1".into(),
            "--max-notional-usd".into(),
            "1000".into(),
            "--master-address".into(),
            MASTER.into(),
            "--allow-custom-endpoints".into(),
            "--read-only".into(),
            "false".into(),
            "--state-dir".into(),
            state_dir.display().to_string(),
        ];
        let result = run_with_cli(Cli::try_parse_from(args).unwrap()).await;
        std::env::remove_var("HL_AGENT_PK");
        std::env::remove_var("HL_AGENT_ADDRESS");
        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");
        let error = result.unwrap_err();
        assert!(error.contains("expected exact zero"), "{error}");
        close.assert_async().await;
        positions.assert_async().await;
        forbidden_open.assert_async().await;

        let run_id = std::fs::read_dir(state_dir.join("runs"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .file_name()
            .to_string_lossy()
            .into_owned();
        let records =
            hype_trigger_twap::journal::ExecutionJournal::read_all(&state_dir, &run_id).unwrap();
        let replay = hype_trigger_twap::journal::ValidatedJournalReplay::replay(&records).unwrap();
        assert_eq!(replay.summary.last_final_report_completed, Some(false));
        assert!(matches!(
            records.last(),
            Some(hype_trigger_twap::journal::JournalRecord::FinalReport { note, .. })
                if note.contains("0.1") && note.contains("-1")
        ));
    }

    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn zero_crossing_opens_only_after_exact_flat_and_finishes_at_target() {
        let tmp = TempDir::new();
        let state_dir = tmp.path().join("state");
        let mut server = mockito::Server::new_async().await;
        let positions = mock_position_preflight_sequence(
            &mut server,
            &[
                r#"{"assetPositions":[{"position":{"coin":"HYPE","szi":"1"}}]}"#,
                // Close-phase slice guard and immediate commit-point guard,
                // then exact-flat verification.
                r#"{"assetPositions":[{"position":{"coin":"HYPE","szi":"1"}}]}"#,
                r#"{"assetPositions":[{"position":{"coin":"HYPE","szi":"1"}}]}"#,
                r#"{"assetPositions":[]}"#,
                // Open-phase slice guard and immediate commit-point guard,
                // then final target verification.
                r#"{"assetPositions":[]}"#,
                r#"{"assetPositions":[]}"#,
                r#"{"assetPositions":[{"position":{"coin":"HYPE","szi":"-1"}}]}"#,
            ],
            true,
        )
        .await;
        let close = server
            .mock("POST", "/exchange")
            .match_body(mockito::Matcher::Regex(r#""r":true"#.into()))
            .with_status(200)
            .with_body(
                r#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"filled":{"oid":1,"totalSz":"1","avgPx":"50"}}]}}}"#,
            )
            .expect(1)
            .create_async()
            .await;
        let open = server
            .mock("POST", "/exchange")
            .match_body(mockito::Matcher::Regex(r#""r":false"#.into()))
            .with_status(200)
            .with_body(
                r#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"filled":{"oid":2,"totalSz":"1","avgPx":"50"}}]}}}"#,
            )
            .expect(1)
            .create_async()
            .await;
        std::env::set_var("HL_AGENT_PK", TEST_PK);
        std::env::set_var("HL_AGENT_ADDRESS", AGENT);
        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));
        let args = vec![
            "hype-twap".to_owned(),
            "--symbol".into(),
            "HYPE".into(),
            "--target-sz".into(),
            "-1".into(),
            "--network".into(),
            "testnet".into(),
            "--duration".into(),
            "1m".into(),
            "--slices".into(),
            "1".into(),
            "--max-notional-usd".into(),
            "1000".into(),
            "--allow-custom-endpoints".into(),
            "--read-only".into(),
            "false".into(),
            "--state-dir".into(),
            state_dir.display().to_string(),
        ];
        let result = run_with_cli(Cli::try_parse_from(args).unwrap()).await;
        std::env::remove_var("HL_AGENT_PK");
        std::env::remove_var("HL_AGENT_ADDRESS");
        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");
        assert_eq!(result.unwrap(), ExitCode::SUCCESS);
        close.assert_async().await;
        positions.assert_async().await;
        open.assert_async().await;

        let run_id = std::fs::read_dir(state_dir.join("runs"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .file_name()
            .to_string_lossy()
            .into_owned();
        let records =
            hype_trigger_twap::journal::ExecutionJournal::read_all(&state_dir, &run_id).unwrap();
        let replay = hype_trigger_twap::journal::ValidatedJournalReplay::replay(&records).unwrap();
        assert_eq!(replay.summary.last_final_report_completed, Some(true));
        assert_eq!(replay.fill_totals.filled_sz, Decimal::from(2));
        let events: Vec<hype_trigger_twap::observability::ExecutionEvent> =
            std::fs::read_to_string(state_dir.join("runs").join(&run_id).join("events.jsonl"))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    &event.payload,
                    hype_trigger_twap::observability::ExecutionEventPayload::RunStopped { .. }
                ))
                .count(),
            1,
            "phase checkpoints must not announce a stopped logical run: {events:?}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    &event.payload,
                    hype_trigger_twap::observability::ExecutionEventPayload::FinalReport { .. }
                ))
                .count(),
            1,
            "only authoritative target verification finishes the run: {events:?}"
        );
    }

    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn position_resume_state_mismatch_is_durable_and_places_nothing() {
        let tmp = TempDir::new();
        let state_dir = tmp.path().join("state");
        let run_id = uuid::Uuid::now_v7().to_string();
        let deadline = wall_clock_now_ms().saturating_add(60_000);
        let args = vec![
            "hype-twap".to_owned(),
            "--symbol".into(),
            "HYPE".into(),
            "--target-sz".into(),
            "-1".into(),
            "--network".into(),
            "testnet".into(),
            "--duration".into(),
            "1m".into(),
            "--slices".into(),
            "1".into(),
            "--max-notional-usd".into(),
            "1000".into(),
            "--master-address".into(),
            MASTER.into(),
            "--allow-custom-endpoints".into(),
            "--read-only".into(),
            "false".into(),
            "--state-dir".into(),
            state_dir.display().to_string(),
            "--resume".into(),
            run_id.clone(),
        ];
        let cli = Cli::try_parse_from(&args).unwrap();
        let initial = SignedPerpPosition {
            symbol: Symbol::new("HYPE"),
            szi: Decimal::ONE,
        };
        let frozen =
            PositionExecutionPlan::target_size(&initial, &Symbol::new("HYPE"), -Decimal::ONE, 2)
                .unwrap();
        let plan = TwapPlan {
            symbol: Symbol::new("HYPE"),
            side: Side::Short,
            asset_index: 0,
            sz_decimals: 2,
            per_slice: Decimal::ONE,
            total_adjusted: Decimal::ONE,
            total_requested: Decimal::ONE,
            slices: 1,
            duration: Duration::from_secs(60),
            absolute_deadline_unix_ms: Some(deadline),
            slippage_bps: cli.slippage_bps,
            max_book_age_ms: cli.max_book_age_ms,
            settle_retries: cli.settle_retries,
            read_only: false,
            reduce_only: true,
            max_notional_usd: Decimal::from(1000),
            agent: Some(Address::new(AGENT)),
            master: Some(Address::new(MASTER)),
            child_algo: ChildAlgo::Market,
            follow_poll_secs: cli.follow_poll_secs,
            follow_repost_secs: cli.follow_repost_secs,
            follow_threshold_bps: cli.follow_threshold_bps,
        };
        let fingerprint = execution_fingerprint(
            &Network::Testnet,
            &plan,
            Some(&frozen),
            &cli,
            Some(Decimal::from(50)),
        );
        let close_cloid = hype_trigger_twap::types::Cloid::new();
        let mut journal = hype_trigger_twap::journal::ExecutionJournal::start(
            &state_dir,
            run_id.clone(),
            hype_trigger_twap::journal::RunHeader {
                run_id: run_id.clone(),
                network: "testnet".into(),
                agent: Some(Address::new(AGENT)),
                master: Some(Address::new(MASTER)),
                symbol: Symbol::new("HYPE"),
                side: Side::Short,
                slices: 1,
                plan_hash: "typed-plan".into(),
                execution_fingerprint: Some(fingerprint),
                started_at_unix_ms: wall_clock_now_ms(),
                execution_deadline_unix_ms: Some(deadline),
            },
        )
        .unwrap();
        journal
            .record(&hype_trigger_twap::journal::JournalRecord::Prepared {
                slice_idx: 1,
                cloid: close_cloid,
                nonce: Some(1),
                symbol: Symbol::new("HYPE"),
                side: Side::Short,
                tif: Some(hype_trigger_twap::types::Tif::Ioc),
                px: "49".into(),
                sz: "1".into(),
            })
            .unwrap();
        journal
            .record(&hype_trigger_twap::journal::JournalRecord::Terminal {
                slice_idx: 1,
                cloid: close_cloid,
                status: "filled".into(),
                filled_sz: "1".into(),
                avg_px: Some("50".into()),
            })
            .unwrap();
        journal
            .record(&hype_trigger_twap::journal::JournalRecord::FinalReport {
                completed: false,
                filled_total: "1".into(),
                outcome_unknown_cloids: Vec::new(),
                note: "crashed after close before authoritative flat verification".into(),
                whole_run: None,
            })
            .unwrap();
        drop(journal);

        let mut server = mockito::Server::new_async().await;
        mock_position_preflight(
            &mut server,
            r#"{"assetPositions":[{"position":{"coin":"HYPE","szi":"0.2"}}]}"#,
            true,
        )
        .await;
        let exchange = server
            .mock("POST", "/exchange")
            .expect(0)
            .create_async()
            .await;
        std::env::set_var("HL_AGENT_PK", TEST_PK);
        std::env::set_var("HL_AGENT_ADDRESS", AGENT);
        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));
        let result = run_with_cli(cli).await;
        std::env::remove_var("HL_AGENT_PK");
        std::env::remove_var("HL_AGENT_ADDRESS");
        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");
        let error = result.unwrap_err();
        assert!(error.contains("durable expected position 0"), "{error}");
        exchange.assert_async().await;

        let records =
            hype_trigger_twap::journal::ExecutionJournal::read_all(&state_dir, &run_id).unwrap();
        let replay = hype_trigger_twap::journal::ValidatedJournalReplay::replay(&records).unwrap();
        assert_eq!(replay.summary.last_final_report_completed, Some(false));
        assert!(matches!(
            records.last(),
            Some(hype_trigger_twap::journal::JournalRecord::FinalReport { note, .. })
                if note.contains("0.2") && note.contains("-1")
        ));
    }

    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn position_resume_below_minimum_stays_incomplete_and_returns_failure() {
        let tmp = TempDir::new();
        let state_dir = tmp.path().join("state");
        let run_id = uuid::Uuid::now_v7().to_string();
        let deadline = wall_clock_now_ms().saturating_add(60_000);
        let args = vec![
            "hype-twap".to_owned(),
            "--symbol".into(),
            "HYPE".into(),
            "--target-sz".into(),
            "0".into(),
            "--network".into(),
            "testnet".into(),
            "--duration".into(),
            "1m".into(),
            "--slices".into(),
            "1".into(),
            "--max-notional-usd".into(),
            "1000".into(),
            "--master-address".into(),
            MASTER.into(),
            "--allow-custom-endpoints".into(),
            "--read-only".into(),
            "false".into(),
            "--state-dir".into(),
            state_dir.display().to_string(),
            "--resume".into(),
            run_id.clone(),
        ];
        let cli = Cli::try_parse_from(&args).unwrap();
        let initial = SignedPerpPosition {
            symbol: Symbol::new("HYPE"),
            szi: Decimal::ONE,
        };
        let frozen =
            PositionExecutionPlan::target_size(&initial, &Symbol::new("HYPE"), Decimal::ZERO, 2)
                .unwrap();
        let plan = TwapPlan {
            symbol: Symbol::new("HYPE"),
            side: Side::Short,
            asset_index: 0,
            sz_decimals: 2,
            per_slice: Decimal::ONE,
            total_adjusted: Decimal::ONE,
            total_requested: Decimal::ONE,
            slices: 1,
            duration: Duration::from_secs(60),
            absolute_deadline_unix_ms: Some(deadline),
            slippage_bps: cli.slippage_bps,
            max_book_age_ms: cli.max_book_age_ms,
            settle_retries: cli.settle_retries,
            read_only: false,
            reduce_only: true,
            max_notional_usd: Decimal::from(1000),
            agent: Some(Address::new(AGENT)),
            master: Some(Address::new(MASTER)),
            child_algo: ChildAlgo::Market,
            follow_poll_secs: cli.follow_poll_secs,
            follow_repost_secs: cli.follow_repost_secs,
            follow_threshold_bps: cli.follow_threshold_bps,
        };
        let fingerprint = execution_fingerprint(
            &Network::Testnet,
            &plan,
            Some(&frozen),
            &cli,
            Some(Decimal::from(50)),
        );
        let cloid = hype_trigger_twap::types::Cloid::new();
        let mut journal = hype_trigger_twap::journal::ExecutionJournal::start(
            &state_dir,
            run_id.clone(),
            hype_trigger_twap::journal::RunHeader {
                run_id: run_id.clone(),
                network: "testnet".into(),
                agent: Some(Address::new(AGENT)),
                master: Some(Address::new(MASTER)),
                symbol: Symbol::new("HYPE"),
                side: Side::Short,
                slices: 1,
                plan_hash: "typed-plan".into(),
                execution_fingerprint: Some(fingerprint),
                started_at_unix_ms: wall_clock_now_ms(),
                execution_deadline_unix_ms: Some(deadline),
            },
        )
        .unwrap();
        journal
            .record(&hype_trigger_twap::journal::JournalRecord::Prepared {
                slice_idx: 1,
                cloid,
                nonce: Some(1),
                symbol: Symbol::new("HYPE"),
                side: Side::Short,
                tif: Some(hype_trigger_twap::types::Tif::Ioc),
                px: "49".into(),
                sz: "1".into(),
            })
            .unwrap();
        journal
            .record(&hype_trigger_twap::journal::JournalRecord::Terminal {
                slice_idx: 1,
                cloid,
                status: "filled".into(),
                filled_sz: "0.9".into(),
                avg_px: Some("50".into()),
            })
            .unwrap();
        drop(journal);

        let mut server = mockito::Server::new_async().await;
        mock_position_preflight(
            &mut server,
            r#"{"assetPositions":[{"position":{"coin":"HYPE","szi":"0.1"}}]}"#,
            true,
        )
        .await;
        let exchange = server
            .mock("POST", "/exchange")
            .expect(0)
            .create_async()
            .await;
        std::env::set_var("HL_AGENT_PK", TEST_PK);
        std::env::set_var("HL_AGENT_ADDRESS", AGENT);
        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));
        let result = run_with_cli(cli).await;
        std::env::remove_var("HL_AGENT_PK");
        std::env::remove_var("HL_AGENT_ADDRESS");
        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");

        assert_eq!(
            result.unwrap(),
            ExitCode::FAILURE,
            "a still-unmet frozen target must not be reported as success"
        );
        exchange.assert_async().await;
        let records =
            hype_trigger_twap::journal::ExecutionJournal::read_all(&state_dir, &run_id).unwrap();
        let replay = hype_trigger_twap::journal::ValidatedJournalReplay::replay(&records).unwrap();
        assert_eq!(replay.summary.last_final_report_completed, Some(false));
        assert!(matches!(
            records.last(),
            Some(hype_trigger_twap::journal::JournalRecord::FinalReport { note, .. })
                if note.contains("below minimum") && note.contains("not asserted complete")
        ));
    }

    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn ordinary_resume_below_minimum_stays_incomplete_and_returns_failure() {
        let tmp = TempDir::new();
        let state_dir = tmp.path().join("state");
        let run_id = uuid::Uuid::now_v7().to_string();
        let deadline = wall_clock_now_ms().saturating_add(60_000);
        let args = live_cli(
            &["--network", "testnet", "--resume", run_id.as_str()],
            &state_dir,
        );
        let cli = Cli::try_parse_from(&args).unwrap();
        let plan = TwapPlan {
            symbol: Symbol::new("HYPE"),
            side: Side::Long,
            asset_index: 0,
            sz_decimals: 2,
            per_slice: Decimal::ONE,
            total_adjusted: Decimal::ONE,
            total_requested: Decimal::ONE,
            slices: 1,
            duration: Duration::from_secs(2),
            absolute_deadline_unix_ms: Some(deadline),
            slippage_bps: cli.slippage_bps,
            max_book_age_ms: cli.max_book_age_ms,
            settle_retries: cli.settle_retries,
            read_only: false,
            reduce_only: false,
            max_notional_usd: Decimal::from(1_000_000),
            agent: Some(Address::new(AGENT)),
            master: Some(Address::new(MASTER)),
            child_algo: ChildAlgo::Market,
            follow_poll_secs: cli.follow_poll_secs,
            follow_repost_secs: cli.follow_repost_secs,
            follow_threshold_bps: cli.follow_threshold_bps,
        };
        let fingerprint = execution_fingerprint(
            &Network::Testnet,
            &plan,
            None,
            &cli,
            Some(Decimal::from(50)),
        );
        let cloid = hype_trigger_twap::types::Cloid::new();
        let mut journal = hype_trigger_twap::journal::ExecutionJournal::start(
            &state_dir,
            run_id.clone(),
            hype_trigger_twap::journal::RunHeader {
                run_id: run_id.clone(),
                network: "testnet".into(),
                agent: Some(Address::new(AGENT)),
                master: Some(Address::new(MASTER)),
                symbol: Symbol::new("HYPE"),
                side: Side::Long,
                slices: 1,
                plan_hash: "typed-plan".into(),
                execution_fingerprint: Some(fingerprint),
                started_at_unix_ms: wall_clock_now_ms(),
                execution_deadline_unix_ms: Some(deadline),
            },
        )
        .unwrap();
        journal
            .record(&hype_trigger_twap::journal::JournalRecord::Prepared {
                slice_idx: 1,
                cloid,
                nonce: Some(1),
                symbol: Symbol::new("HYPE"),
                side: Side::Long,
                tif: Some(hype_trigger_twap::types::Tif::Ioc),
                px: "50".into(),
                sz: "1".into(),
            })
            .unwrap();
        journal
            .record(&hype_trigger_twap::journal::JournalRecord::Terminal {
                slice_idx: 1,
                cloid,
                status: "filled".into(),
                filled_sz: "0.9".into(),
                avg_px: Some("50".into()),
            })
            .unwrap();
        drop(journal);

        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({"type": "meta"})))
            .with_status(200)
            .with_body(r#"{"universe":[{"name":"HYPE","szDecimals":2,"maxLeverage":10,"onlyIsolated":false}]}"#)
            .expect(1)
            .create_async()
            .await;
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "userRole"}),
            ))
            .with_status(200)
            .with_body(format!(
                r#"{{"role":"agent","data":{{"user":"{MASTER}"}}}}"#
            ))
            .expect(1)
            .create_async()
            .await;
        let now_ms = chrono::Utc::now().timestamp_millis();
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "l2Book"}),
            ))
            .with_status(200)
            .with_body(book_body_at("HYPE", "49.9", "50.1", now_ms))
            .expect_at_least(1)
            .create_async()
            .await;
        let exchange = server
            .mock("POST", "/exchange")
            .expect(0)
            .create_async()
            .await;
        std::env::set_var("HL_AGENT_PK", TEST_PK);
        std::env::set_var("HL_AGENT_ADDRESS", AGENT);
        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));
        let result = run_with_cli(cli).await;
        std::env::remove_var("HL_AGENT_PK");
        std::env::remove_var("HL_AGENT_ADDRESS");
        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");

        assert_eq!(result.unwrap(), ExitCode::FAILURE);
        exchange.assert_async().await;
        let records =
            hype_trigger_twap::journal::ExecutionJournal::read_all(&state_dir, &run_id).unwrap();
        let replay = hype_trigger_twap::journal::ValidatedJournalReplay::replay(&records).unwrap();
        assert_eq!(replay.summary.last_final_report_completed, Some(false));
        assert!(matches!(
            records.last(),
            Some(hype_trigger_twap::journal::JournalRecord::FinalReport { note, .. })
                if note.contains("below minimum") && note.contains("not asserted complete")
        ));
    }

    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn expired_resume_reconciles_then_fetches_no_book_and_stays_incomplete() {
        let tmp = TempDir::new();
        let state_dir = tmp.path().join("state");
        let run_id = uuid::Uuid::now_v7().to_string();
        let deadline = wall_clock_now_ms().saturating_sub(1);
        let args = vec![
            "hype-twap".to_owned(),
            "--symbol".into(),
            "HYPE".into(),
            "--side".into(),
            "long".into(),
            "--usd".into(),
            "50".into(),
            "--network".into(),
            "testnet".into(),
            "--duration".into(),
            "1m".into(),
            "--slices".into(),
            "1".into(),
            "--max-notional-usd".into(),
            "1000".into(),
            "--master-address".into(),
            MASTER.into(),
            "--allow-custom-endpoints".into(),
            "--read-only".into(),
            "false".into(),
            "--state-dir".into(),
            state_dir.display().to_string(),
            "--resume".into(),
            run_id.clone(),
        ];
        let cli = Cli::try_parse_from(&args).unwrap();
        let plan = TwapPlan {
            symbol: Symbol::new("HYPE"),
            side: Side::Long,
            asset_index: 0,
            sz_decimals: 2,
            per_slice: Decimal::ONE,
            total_adjusted: Decimal::ONE,
            total_requested: Decimal::ONE,
            slices: 1,
            duration: Duration::from_secs(60),
            absolute_deadline_unix_ms: Some(deadline),
            slippage_bps: cli.slippage_bps,
            max_book_age_ms: cli.max_book_age_ms,
            settle_retries: cli.settle_retries,
            read_only: false,
            reduce_only: false,
            max_notional_usd: Decimal::from(1000),
            agent: Some(Address::new(AGENT)),
            master: Some(Address::new(MASTER)),
            child_algo: ChildAlgo::Market,
            follow_poll_secs: cli.follow_poll_secs,
            follow_repost_secs: cli.follow_repost_secs,
            follow_threshold_bps: cli.follow_threshold_bps,
        };
        let fingerprint = execution_fingerprint(
            &Network::Testnet,
            &plan,
            None,
            &cli,
            Some(Decimal::from(50)),
        );
        drop(
            hype_trigger_twap::journal::ExecutionJournal::start(
                &state_dir,
                run_id.clone(),
                hype_trigger_twap::journal::RunHeader {
                    run_id: run_id.clone(),
                    network: "testnet".into(),
                    agent: Some(Address::new(AGENT)),
                    master: Some(Address::new(MASTER)),
                    symbol: Symbol::new("HYPE"),
                    side: Side::Long,
                    slices: 1,
                    plan_hash: "typed-plan".into(),
                    execution_fingerprint: Some(fingerprint),
                    started_at_unix_ms: deadline.saturating_sub(60_000),
                    execution_deadline_unix_ms: Some(deadline),
                },
            )
            .unwrap(),
        );

        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({"type": "meta"})))
            .with_status(200)
            .with_body(r#"{"universe":[{"name":"HYPE","szDecimals":2,"maxLeverage":10,"onlyIsolated":false}]}"#)
            .expect(1)
            .create_async()
            .await;
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "userRole"}),
            ))
            .with_status(200)
            .with_body(format!(
                r#"{{"role":"agent","data":{{"user":"{MASTER}"}}}}"#
            ))
            .expect(1)
            .create_async()
            .await;
        let no_book = server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "l2Book"}),
            ))
            .expect(0)
            .create_async()
            .await;
        let no_exchange = server
            .mock("POST", "/exchange")
            .expect(0)
            .create_async()
            .await;
        std::env::set_var("HL_AGENT_PK", TEST_PK);
        std::env::set_var("HL_AGENT_ADDRESS", AGENT);
        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));
        let result = run_with_cli(cli).await;
        std::env::remove_var("HL_AGENT_PK");
        std::env::remove_var("HL_AGENT_ADDRESS");
        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");
        assert_eq!(result.unwrap(), ExitCode::FAILURE);
        no_book.assert_async().await;
        no_exchange.assert_async().await;
        let records =
            hype_trigger_twap::journal::ExecutionJournal::read_all(&state_dir, &run_id).unwrap();
        let replay = hype_trigger_twap::journal::ValidatedJournalReplay::replay(&records).unwrap();
        assert_eq!(replay.summary.last_final_report_completed, Some(false));
        assert_eq!(replay.fill_totals.filled_sz, Decimal::ZERO);
    }

    /// Read-only regression (Issue #4 acceptance criterion): a read-only run
    /// must create NEITHER the state directory NOR a journal file, even when
    /// `--state-dir` points at a path that does not exist yet.
    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn read_only_creates_no_state_dir_or_journal_file() {
        let tmp = TempDir::new();
        let state_dir = tmp.path().join("would-be-state-dir");
        let event_path = tmp.path().join("simulation-events.jsonl");
        assert!(!state_dir.exists());

        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({"type": "meta"})))
            .with_status(200)
            .with_body(r#"{"universe":[{"name":"HYPE","szDecimals":2,"maxLeverage":10,"onlyIsolated":false}]}"#)
            .create_async()
            .await;
        let now_ms = chrono::Utc::now().timestamp_millis();
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "l2Book"}),
            ))
            .with_status(200)
            .with_body(book_body_at("HYPE", "49.9", "50.1", now_ms))
            .create_async()
            .await;

        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));

        let cli = Cli::try_parse_from([
            "hype-twap",
            "--symbol",
            "HYPE",
            "--side",
            "long",
            "--usd",
            "50",
            "--duration",
            "2s",
            "--slices",
            "1",
            "--state-dir",
            &state_dir.display().to_string(),
            "--event-jsonl",
            &event_path.display().to_string(),
            "--read-only",
            "true",
        ])
        .unwrap();

        let _ = run_with_cli(cli).await;

        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");

        assert!(
            !state_dir.exists(),
            "a read-only run must never create the state directory"
        );
        let events: Vec<hype_trigger_twap::observability::ExecutionEvent> =
            std::fs::read_to_string(event_path)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
        assert!(matches!(
            events.first().map(|event| &event.payload),
            Some(
                hype_trigger_twap::observability::ExecutionEventPayload::RunStarted {
                    mode: ExecutionMode::ReadOnly,
                    ..
                }
            )
        ));
        assert!(matches!(
            events.last().map(|event| &event.payload),
            Some(
                hype_trigger_twap::observability::ExecutionEventPayload::FinalReport {
                    outcome: hype_trigger_twap::observability::RunOutcome::Completed,
                    ..
                }
            )
        ));
    }

    /// A complete one-slice LIVE run creates the state dir and a journal
    /// file containing a Header record and a FinalReport (completed: true).
    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn live_run_creates_state_dir_and_a_completed_journal() {
        let tmp = TempDir::new();
        let state_dir = tmp.path().join("state");

        let mut server = mockito::Server::new_async().await;
        mock_full_live_run(&mut server).await;

        std::env::set_var("HL_AGENT_PK", TEST_PK);
        std::env::set_var("HL_AGENT_ADDRESS", AGENT);
        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));

        let args = live_cli(&["--network", "testnet"], &state_dir);
        let cli = Cli::try_parse_from(&args).unwrap();
        let result = run_with_cli(cli).await;

        std::env::remove_var("HL_AGENT_PK");
        std::env::remove_var("HL_AGENT_ADDRESS");
        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");

        result.expect("a fully-mocked one-slice live run must succeed");
        assert!(state_dir.exists(), "a live run must create the state dir");

        let runs_dir = state_dir.join("runs");
        let run_ids: Vec<_> = std::fs::read_dir(&runs_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().into_string().unwrap())
            .collect();
        assert_eq!(run_ids.len(), 1, "exactly one run directory: {run_ids:?}");

        let records =
            hype_trigger_twap::journal::ExecutionJournal::read_all(&state_dir, &run_ids[0])
                .unwrap();
        assert!(
            matches!(
                records[0],
                hype_trigger_twap::journal::JournalRecord::Header(_)
            ),
            "the first record must be the run header: {records:?}"
        );
        assert!(
            records.iter().any(|r| matches!(
                r,
                hype_trigger_twap::journal::JournalRecord::FinalReport {
                    completed: true,
                    ..
                }
            )),
            "a completed run must end with a completed FinalReport: {records:?}"
        );

        let events_path = runs_dir.join(&run_ids[0]).join("events.jsonl");
        let events: Vec<hype_trigger_twap::observability::ExecutionEvent> =
            std::fs::read_to_string(&events_path)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
        assert_eq!(
            events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            (1..=events.len() as u64).collect::<Vec<_>>()
        );
        let kinds: Vec<_> = events
            .iter()
            .map(|event| {
                serde_json::to_value(&event.payload).unwrap()["kind"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "run_started",
                "cap_remaining",
                "preflight_completed",
                "slice_prepared",
                "slice_terminal",
                "fill",
                "slice_completed",
                "cap_remaining",
                "cap_remaining",
                "run_stopped",
                "final_report",
            ]
        );
        assert!(matches!(
            events.first().map(|event| &event.payload),
            Some(
                hype_trigger_twap::observability::ExecutionEventPayload::RunStarted {
                    mode: ExecutionMode::Live,
                    ..
                }
            )
        ));
    }

    /// Issue #4 acceptance criterion: an incomplete run for the same
    /// network+agent must block a brand-new overlapping live run (no
    /// `--resume`, no `--abandon-incomplete-run`).
    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn incomplete_run_blocks_a_new_live_run_without_resume_or_abandon() {
        let tmp = TempDir::new();
        let state_dir = tmp.path().join("state");

        // Seed an incomplete run directly via the journal API (no
        // FinalReport, one unresolved SubmittedUnknown cloid) — simulating a
        // prior process that crashed mid-run.
        {
            let mut j = hype_trigger_twap::journal::ExecutionJournal::start(
                &state_dir,
                "prior-incomplete-run".into(),
                hype_trigger_twap::journal::RunHeader {
                    run_id: "prior-incomplete-run".into(),
                    network: "testnet".into(),
                    agent: Some(hype_trigger_twap::types::Address::new(AGENT)),
                    master: Some(hype_trigger_twap::types::Address::new(MASTER)),
                    symbol: hype_trigger_twap::types::Symbol::new("HYPE"),
                    side: hype_trigger_twap::types::Side::Long,
                    slices: 1,
                    plan_hash: "irrelevant".into(),
                    execution_fingerprint: None,
                    started_at_unix_ms: 0,
                    execution_deadline_unix_ms: None,
                },
            )
            .unwrap();
            let cloid = hype_trigger_twap::types::Cloid::new();
            j.record(&hype_trigger_twap::journal::JournalRecord::Prepared {
                slice_idx: 1,
                cloid,
                nonce: None,
                symbol: hype_trigger_twap::types::Symbol::new("HYPE"),
                side: hype_trigger_twap::types::Side::Long,
                tif: None,
                px: "50".into(),
                sz: "1".into(),
            })
            .unwrap();
            j.record(
                &hype_trigger_twap::journal::JournalRecord::SubmittedUnknown {
                    slice_idx: 1,
                    cloid,
                },
            )
            .unwrap();
        }

        let server = mockito::Server::new_async().await; // no mocks: must never be reached

        std::env::set_var("HL_AGENT_PK", TEST_PK);
        std::env::set_var("HL_AGENT_ADDRESS", AGENT);
        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));

        let args = live_cli(&["--network", "testnet"], &state_dir);
        let cli = Cli::try_parse_from(&args).unwrap();
        let result = run_with_cli(cli).await;

        std::env::remove_var("HL_AGENT_PK");
        std::env::remove_var("HL_AGENT_ADDRESS");
        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");

        let err = result.expect_err("a new overlapping live run must be refused");
        assert!(err.contains("incomplete run"), "{err}");
        assert!(err.contains("prior-incomplete-run"), "{err}");
    }

    /// Issue #5 acceptance criterion 1: a second live process for the SAME
    /// network+agent must fail BEFORE any order is placed. Simulated here by
    /// pre-acquiring the lock directly (standing in for "another process
    /// already holds it") and then calling `run_with_cli` — no `/exchange`
    /// mock is registered, so any attempt to place an order would panic the
    /// mock server, proving the failure happens before send.
    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn second_live_process_for_the_same_agent_fails_before_any_order() {
        let tmp = TempDir::new();
        let state_dir = tmp.path().join("state");

        let key = hype_trigger_twap::lock::lock_key(
            "testnet",
            &hype_trigger_twap::types::Address::new(AGENT),
        );
        let held_lock = hype_trigger_twap::lock::ProcessLock::acquire(
            &state_dir,
            &key,
            &hype_trigger_twap::lock::LockMetadata::new("first holder, still running"),
        )
        .expect("first acquire (simulating the already-running process) must succeed");

        // No mocks registered at all: /info and /exchange must never be hit.
        let server = mockito::Server::new_async().await;

        std::env::set_var("HL_AGENT_PK", TEST_PK);
        std::env::set_var("HL_AGENT_ADDRESS", AGENT);
        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));

        let args = live_cli(&["--network", "testnet"], &state_dir);
        let cli = Cli::try_parse_from(&args).unwrap();
        let result = run_with_cli(cli).await;

        std::env::remove_var("HL_AGENT_PK");
        std::env::remove_var("HL_AGENT_ADDRESS");
        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");

        let err = result.expect_err("a second live process for the same network+agent must fail");
        assert!(
            err.contains("already holds the writer lock"),
            "expected a lock-contention error, got: {err}"
        );

        drop(held_lock);
    }

    /// Issue #5 acceptance criterion 2 (part 1): a DIFFERENT agent must run
    /// concurrently unaffected by another agent's held lock.
    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn different_agent_runs_concurrently_unaffected_by_another_agents_lock() {
        const OTHER_AGENT: &str = "0x00000000000000000000000000000000000bad";
        let tmp = TempDir::new();
        let state_dir = tmp.path().join("state");

        let other_key = hype_trigger_twap::lock::lock_key(
            "testnet",
            &hype_trigger_twap::types::Address::new(OTHER_AGENT),
        );
        let _held_by_other = hype_trigger_twap::lock::ProcessLock::acquire(
            &state_dir,
            &other_key,
            &hype_trigger_twap::lock::LockMetadata::new("a different agent's run"),
        )
        .unwrap();

        let mut server = mockito::Server::new_async().await;
        mock_full_live_run(&mut server).await;

        std::env::set_var("HL_AGENT_PK", TEST_PK);
        std::env::set_var("HL_AGENT_ADDRESS", AGENT);
        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));

        let args = live_cli(&["--network", "testnet"], &state_dir);
        let cli = Cli::try_parse_from(&args).unwrap();
        let result = run_with_cli(cli).await;

        std::env::remove_var("HL_AGENT_PK");
        std::env::remove_var("HL_AGENT_ADDRESS");
        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");

        result.expect(
            "a different agent's live run must succeed even while another agent's lock is held",
        );
    }

    /// Issue #5 acceptance criterion 2 (part 2): a read-only process must run
    /// concurrently unaffected by a held live lock for the SAME agent, and
    /// must not itself touch the lock/state dir at all.
    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn read_only_runs_concurrently_unaffected_by_a_held_live_lock_for_the_same_agent() {
        let tmp = TempDir::new();
        let state_dir = tmp.path().join("state");

        let key = hype_trigger_twap::lock::lock_key(
            "testnet",
            &hype_trigger_twap::types::Address::new(AGENT),
        );
        let _held = hype_trigger_twap::lock::ProcessLock::acquire(
            &state_dir,
            &key,
            &hype_trigger_twap::lock::LockMetadata::new("a live run for the same agent"),
        )
        .unwrap();

        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({"type": "meta"})))
            .with_status(200)
            .with_body(r#"{"universe":[{"name":"HYPE","szDecimals":2,"maxLeverage":10,"onlyIsolated":false}]}"#)
            .create_async()
            .await;
        let now_ms = chrono::Utc::now().timestamp_millis();
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "l2Book"}),
            ))
            .with_status(200)
            .with_body(book_body_at("HYPE", "49.9", "50.1", now_ms))
            .create_async()
            .await;

        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));

        let cli = Cli::try_parse_from([
            "hype-twap",
            "--symbol",
            "HYPE",
            "--side",
            "long",
            "--usd",
            "50",
            "--duration",
            "2s",
            "--slices",
            "1",
            "--network",
            "testnet",
            "--state-dir",
            &state_dir.display().to_string(),
            "--read-only",
            "true",
        ])
        .unwrap();

        let result = run_with_cli(cli).await;

        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");

        result.expect(
            "a read-only run must succeed even while a live lock is held for the same network+agent",
        );
    }

    /// Issue #5 acceptance criterion 3: even in a "stale lock" scenario (the
    /// held lock's owner is simulated as gone by dropping it before the new
    /// process starts, so acquisition succeeds), the incomplete-run
    /// reconciliation flow that Task 7/#4 already implemented is NOT
    /// skipped — lock acquisition happens strictly BEFORE incomplete-run
    /// detection in `run_with_cli`, so a stale lock's recovery can never
    /// shortcut past reconciliation. This test proves the ordering by
    /// combining both fixtures: a prior incomplete run (unresolved
    /// `SubmittedUnknown` cloid) AND a lock that has just been released
    /// (simulating the crash that left both behind) — the resuming process
    /// must still be forced through reconciliation (verified by the
    /// resolved cloid ending up Terminal in the journal), not just allowed
    /// to barrel past it because the lock happened to be free.
    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn stale_lock_recovery_does_not_skip_incomplete_run_reconciliation() {
        let tmp = TempDir::new();
        let state_dir = tmp.path().join("state");
        let prior_cloid = hype_trigger_twap::types::Cloid::new();

        // Simulate the crashed process: it held the lock, then died (flock
        // auto-releases on process death — modeled here by simply dropping
        // the guard) leaving an incomplete run behind.
        {
            let key = hype_trigger_twap::lock::lock_key(
                "testnet",
                &hype_trigger_twap::types::Address::new(AGENT),
            );
            let crashed_holder = hype_trigger_twap::lock::ProcessLock::acquire(
                &state_dir,
                &key,
                &hype_trigger_twap::lock::LockMetadata::new("the crashed process"),
            )
            .unwrap();
            drop(crashed_holder); // simulates process death releasing the flock
        }

        std::env::set_var("HL_AGENT_PK", TEST_PK);
        std::env::set_var("HL_AGENT_ADDRESS", AGENT);

        // Same plan_hash-matching trick as `resume_reconciles_the_incomplete_
        // cloid_before_continuing`: derive the real plan_hash via a throwaway
        // probe run rather than hand-computing DefaultHasher output.
        let probe_state_dir = tmp.path().join("probe-state");
        {
            let mut probe_server = mockito::Server::new_async().await;
            mock_full_live_run(&mut probe_server).await;
            std::env::set_var("HL_INFO_URL", format!("{}/info", probe_server.url()));
            std::env::set_var(
                "HL_EXCHANGE_URL",
                format!("{}/exchange", probe_server.url()),
            );
            let probe_args = live_cli(&["--network", "testnet"], &probe_state_dir);
            let probe_cli = Cli::try_parse_from(&probe_args).unwrap();
            run_with_cli(probe_cli)
                .await
                .expect("probe run must succeed to derive a real plan_hash");
            std::env::remove_var("HL_INFO_URL");
            std::env::remove_var("HL_EXCHANGE_URL");
        }
        let probe_run_id = std::fs::read_dir(probe_state_dir.join("runs"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .file_name()
            .into_string()
            .unwrap();
        let probe_records =
            hype_trigger_twap::journal::ExecutionJournal::read_all(&probe_state_dir, &probe_run_id)
                .unwrap();
        let resume_header =
            resumable_header_from_probe("stale-lock-incomplete-run", &probe_records);

        // Seed the incomplete run this process will --resume, with the same
        // plan_hash derived above.
        {
            let mut j = hype_trigger_twap::journal::ExecutionJournal::start(
                &state_dir,
                "stale-lock-incomplete-run".into(),
                resume_header,
            )
            .unwrap();
            j.record(&hype_trigger_twap::journal::JournalRecord::Prepared {
                slice_idx: 1,
                cloid: prior_cloid,
                nonce: None,
                symbol: hype_trigger_twap::types::Symbol::new("HYPE"),
                side: hype_trigger_twap::types::Side::Long,
                px: "50".into(),
                sz: "1".into(),
                tif: None,
            })
            .unwrap();
            j.record(
                &hype_trigger_twap::journal::JournalRecord::SubmittedUnknown {
                    slice_idx: 1,
                    cloid: prior_cloid,
                },
            )
            .unwrap();
        }

        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({"type": "meta"})))
            .with_status(200)
            .with_body(r#"{"universe":[{"name":"HYPE","szDecimals":2,"maxLeverage":10,"onlyIsolated":false}]}"#)
            .create_async()
            .await;
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "userRole"}),
            ))
            .with_status(200)
            .with_body(format!(
                r#"{{"role":"agent","data":{{"user":"{MASTER}"}}}}"#
            ))
            .create_async()
            .await;
        // Reconciliation queries orderStatus for the unresolved cloid: report
        // it as filled, so the reconciled Terminal record is unambiguous.
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "orderStatus", "oid": prior_cloid.to_hex_string()}),
            ))
            .with_status(200)
            .with_body(format!(
                r#"{{"status":"order","order":{{"order":{{"oid":42,"coin":"HYPE","side":"B","cloid":"{prior_cloid}","origSz":"1","sz":"0"}},"status":"filled","statusTimestamp":0}}}}"#
            ))
            .create_async()
            .await;
        let now_ms = chrono::Utc::now().timestamp_millis();
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "l2Book"}),
            ))
            .with_status(200)
            .with_body(book_body_at("HYPE", "49.9", "50.1", now_ms))
            .expect_at_least(0)
            .create_async()
            .await;
        server
            .mock("POST", "/exchange")
            .with_status(200)
            .with_body(
                r#"{"status":"ok","response":{"type":"order","data":{"statuses":[
                    {"filled":{"oid":2,"totalSz":"1","avgPx":"50"}}]}}}"#,
            )
            .expect_at_least(0)
            .create_async()
            .await;

        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));

        let mut args = live_cli(&["--network", "testnet"], &state_dir);
        args.push("--resume".into());
        args.push("stale-lock-incomplete-run".into());
        let cli = Cli::try_parse_from(&args).unwrap();
        let result = run_with_cli(cli).await;

        std::env::remove_var("HL_AGENT_PK");
        std::env::remove_var("HL_AGENT_ADDRESS");
        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");

        result.expect("resuming after a stale-lock scenario must succeed");

        let records = hype_trigger_twap::journal::ExecutionJournal::read_all(
            &state_dir,
            "stale-lock-incomplete-run",
        )
        .unwrap();
        assert!(
            records.iter().any(|r| matches!(
                r,
                hype_trigger_twap::journal::JournalRecord::Terminal { cloid, .. }
                    if *cloid == prior_cloid
            )),
            "reconciliation must have resolved the prior unresolved cloid to Terminal, \
             proving the stale-lock recovery path did not skip it: {records:?}"
        );
    }

    /// `--resume <run-id>` on an incomplete run force-reconciles its
    /// unresolved cloid via `orderStatus`, then CONTINUES the run for the
    /// remaining plan — proving the resumed run's own journal ends up with
    /// the reconciled cloid resolved to Terminal, not left dangling.
    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn resume_reconciles_the_incomplete_cloid_before_continuing() {
        let tmp = TempDir::new();
        let state_dir = tmp.path().join("state");
        let prior_cloid = hype_trigger_twap::types::Cloid::new();

        std::env::set_var("HL_AGENT_PK", TEST_PK);
        std::env::set_var("HL_AGENT_ADDRESS", AGENT);

        // Issue #4's --resume verifies the stored plan_hash against a
        // freshly recomputed one before resuming, so the fixture seeded
        // below must carry the SAME hash `live_cli`'s resolved TwapPlan will
        // compute at runtime. Rather than hand-reconstruct rust_decimal's
        // exact string formatting (fragile / easy to drift from the real
        // formatter), derive it from a real probe run: start a fresh live
        // run with the SAME `live_cli` args against a throwaway state dir,
        // let it write its own header, then read the plan_hash back out.
        let probe_state_dir = tmp.path().join("probe-state");
        {
            let mut probe_server = mockito::Server::new_async().await;
            mock_full_live_run(&mut probe_server).await;
            std::env::set_var("HL_INFO_URL", format!("{}/info", probe_server.url()));
            std::env::set_var(
                "HL_EXCHANGE_URL",
                format!("{}/exchange", probe_server.url()),
            );
            let probe_args = live_cli(&["--network", "testnet"], &probe_state_dir);
            run_with_cli(Cli::try_parse_from(&probe_args).unwrap())
                .await
                .expect("probe run to derive plan_hash must succeed");
        }
        let probe_run_id = std::fs::read_dir(probe_state_dir.join("runs"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .file_name()
            .into_string()
            .unwrap();
        let probe_records =
            hype_trigger_twap::journal::ExecutionJournal::read_all(&probe_state_dir, &probe_run_id)
                .unwrap();
        let resume_header = resumable_header_from_probe("run-to-resume", &probe_records);

        {
            let mut j = hype_trigger_twap::journal::ExecutionJournal::start(
                &state_dir,
                "run-to-resume".into(),
                resume_header,
            )
            .unwrap();
            j.record(&hype_trigger_twap::journal::JournalRecord::Prepared {
                slice_idx: 1,
                cloid: prior_cloid,
                nonce: None,
                symbol: hype_trigger_twap::types::Symbol::new("HYPE"),
                side: hype_trigger_twap::types::Side::Long,
                px: "50".into(),
                sz: "1".into(),
                tif: None,
            })
            .unwrap();
            j.record(
                &hype_trigger_twap::journal::JournalRecord::SubmittedUnknown {
                    slice_idx: 1,
                    cloid: prior_cloid,
                },
            )
            .unwrap();
        }

        let mut server = mockito::Server::new_async().await;
        mock_full_live_run(&mut server).await;
        // The forced reconciliation for the PRIOR cloid: orderStatus by
        // cloid must report it as terminal/filled so no resend occurs.
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "orderStatus", "oid": prior_cloid.to_hex_string()}),
            ))
            .with_status(200)
            .with_body(format!(
                r#"{{"status":"order","order":{{"order":{{"oid":42,"coin":"HYPE","side":"B","cloid":"{prior_cloid}","origSz":"1","sz":"0"}},"status":"filled","statusTimestamp":0}}}}"#
            ))
            .create_async()
            .await;

        std::env::set_var("HL_AGENT_PK", TEST_PK);
        std::env::set_var("HL_AGENT_ADDRESS", AGENT);
        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));

        let args = live_cli(
            &["--network", "testnet", "--resume", "run-to-resume"],
            &state_dir,
        );
        let cli = Cli::try_parse_from(&args).unwrap();
        let result = run_with_cli(cli).await;

        std::env::remove_var("HL_AGENT_PK");
        std::env::remove_var("HL_AGENT_ADDRESS");
        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");

        result.expect("resume of a fully-mocked incomplete run must succeed");

        let records =
            hype_trigger_twap::journal::ExecutionJournal::read_all(&state_dir, "run-to-resume")
                .unwrap();
        let summary = hype_trigger_twap::journal::summarize(&records).unwrap();
        assert!(
            summary.unresolved_cloids().is_empty(),
            "the resumed cloid must be resolved, not left dangling: {records:?}"
        );
    }

    /// `--usd`/`--slices` args for a TWO-slice plan (`$100` at mid `50` →
    /// 2 HYPE total, 1 HYPE/slice) — used by the Finding 1 regression test
    /// below, which needs a resumable run with a real remainder left after
    /// one slice's worth of prior fill.
    fn live_cli_two_slice(extra: &[&str], state_dir: &std::path::Path) -> Vec<String> {
        let mut args: Vec<String> = vec![
            "hype-twap".into(),
            "--symbol".into(),
            "HYPE".into(),
            "--side".into(),
            "long".into(),
            "--usd".into(),
            "100".into(),
            "--duration".into(),
            "2s".into(),
            "--slices".into(),
            "2".into(),
            "--max-notional-usd".into(),
            "1000000".into(),
            "--master-address".into(),
            MASTER.into(),
            "--allow-custom-endpoints".into(),
            "--read-only".into(),
            "false".into(),
            "--state-dir".into(),
            state_dir.display().to_string(),
        ];
        args.extend(extra.iter().map(|s| (*s).to_string()));
        args
    }

    /// **Finding 1 (CRITICAL) regression test.** Before the fix, `--resume`
    /// rebuilt the ORIGINAL plan (`total_adjusted` = the full 2-slice
    /// target) and started `run_twap_journaled` fresh at `slice_idx=1` with
    /// zero in-memory `filled` — so a prior run's slice-1 fill (already
    /// journaled `Terminal`, therefore excluded from
    /// `unresolved_cloids()` and invisible to forced reconciliation) was
    /// silently re-executed on top of, doubling the total placed.
    ///
    /// Seeds a prior journal with BOTH:
    /// - a `Terminal`, partially-filled cloid for slice 1 (1 of 2 HYPE,
    ///   simulating "slice 1 filled, then the process crashed") — this is
    ///   the path Finding 1 is specifically about: a fill the ORIGINAL
    ///   implementation's `--resume` tests never seeded, so this exact bug
    ///   was never exercised or caught before now.
    /// - a `SubmittedUnknown` cloid for slice 2 (simulating "slice 2 was
    ///   sent, then the process crashed before reading the response") — so
    ///   both the reconciliation path AND the continuation-plan path are
    ///   exercised together in one run, per the finding's explicit
    ///   requirement.
    ///
    /// Asserts, via a mockito body-matcher + `.expect(1)` call-count on
    /// `/exchange`, that the resumed run places EXACTLY ONE new order of
    /// size `1` (the remainder: 2 HYPE total − 1 HYPE already filled), never
    /// the full original 2 HYPE — and that the final accounting (prior
    /// journaled fill + this run's own newly-executed fill) sums to exactly
    /// the original 2 HYPE target, each fill counted exactly once.
    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn resume_continues_only_the_remainder_not_the_full_original_plan() {
        let tmp = TempDir::new();
        let state_dir = tmp.path().join("state");
        let slice1_cloid = hype_trigger_twap::types::Cloid::new();
        let slice2_cloid = hype_trigger_twap::types::Cloid::new();

        std::env::set_var("HL_AGENT_PK", TEST_PK);
        std::env::set_var("HL_AGENT_ADDRESS", AGENT);

        // Derive the real plan_hash for this 2-slice/$100 plan the same way
        // the existing --resume tests do: a throwaway probe run against its
        // own state dir, reading the hash back out of its own header.
        let probe_state_dir = tmp.path().join("probe-state");
        {
            let mut probe_server = mockito::Server::new_async().await;
            mock_full_live_run(&mut probe_server).await;
            std::env::set_var("HL_INFO_URL", format!("{}/info", probe_server.url()));
            std::env::set_var(
                "HL_EXCHANGE_URL",
                format!("{}/exchange", probe_server.url()),
            );
            let probe_args = live_cli_two_slice(&["--network", "testnet"], &probe_state_dir);
            run_with_cli(Cli::try_parse_from(&probe_args).unwrap())
                .await
                .expect("probe run to derive plan_hash must succeed");
        }
        let probe_run_id = std::fs::read_dir(probe_state_dir.join("runs"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .file_name()
            .into_string()
            .unwrap();
        let probe_records =
            hype_trigger_twap::journal::ExecutionJournal::read_all(&probe_state_dir, &probe_run_id)
                .unwrap();
        let resume_header = resumable_header_from_probe("run-to-resume-partial", &probe_records);
        let original_deadline = resume_header
            .execution_deadline_unix_ms
            .expect("resumable typed header stores an absolute execution deadline");

        // Seed the incomplete run: slice 1 already Terminal/filled (1 HYPE),
        // slice 2 SubmittedUnknown (ambiguous — needs reconciliation).
        {
            let mut j = hype_trigger_twap::journal::ExecutionJournal::start(
                &state_dir,
                "run-to-resume-partial".into(),
                resume_header,
            )
            .unwrap();
            j.record(&hype_trigger_twap::journal::JournalRecord::Prepared {
                slice_idx: 1,
                cloid: slice1_cloid,
                nonce: None,
                symbol: hype_trigger_twap::types::Symbol::new("HYPE"),
                side: hype_trigger_twap::types::Side::Long,
                px: "50".into(),
                sz: "1".into(),
                tif: None,
            })
            .unwrap();
            j.record(&hype_trigger_twap::journal::JournalRecord::Terminal {
                slice_idx: 1,
                cloid: slice1_cloid,
                status: "filled".into(),
                filled_sz: "1".into(),
                avg_px: Some("50".into()),
            })
            .unwrap();
            j.record(&hype_trigger_twap::journal::JournalRecord::Prepared {
                slice_idx: 2,
                cloid: slice2_cloid,
                nonce: None,
                symbol: hype_trigger_twap::types::Symbol::new("HYPE"),
                side: hype_trigger_twap::types::Side::Long,
                px: "50".into(),
                sz: "1".into(),
                tif: None,
            })
            .unwrap();
            j.record(
                &hype_trigger_twap::journal::JournalRecord::SubmittedUnknown {
                    slice_idx: 2,
                    cloid: slice2_cloid,
                },
            )
            .unwrap();
        }

        // NOTE: deliberately NOT reusing `mock_full_live_run` here (unlike
        // the other --resume tests) — its `/exchange` mock has no body
        // matcher and no `.expect()`, so mockito treats it as perpetually
        // "missing hits" and prefers it over ANY later, more specific
        // `/exchange` mock regardless of registration order. This test
        // needs the `/exchange` mock itself to be the size assertion, so
        // meta/userRole/l2Book are registered individually instead.
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({"type": "meta"})))
            .with_status(200)
            .with_body(r#"{"universe":[{"name":"HYPE","szDecimals":2,"maxLeverage":10,"onlyIsolated":false}]}"#)
            .create_async()
            .await;
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "userRole"}),
            ))
            .with_status(200)
            .with_body(format!(
                r#"{{"role":"agent","data":{{"user":"{MASTER}"}}}}"#
            ))
            .create_async()
            .await;
        let now_ms = chrono::Utc::now().timestamp_millis();
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "l2Book"}),
            ))
            .with_status(200)
            .with_body(book_body_at("HYPE", "49.9", "50.1", now_ms))
            .expect_at_least(1)
            .create_async()
            .await;
        // Forced reconciliation for slice 2's ambiguous cloid: HL reports it
        // as never received (unknownOid), so it resolves to a zero-fill
        // Terminal, NOT a resend on this codepath (resume/abandon never
        // resend — only a live run's own place_slice_reconciled may).
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "orderStatus", "oid": slice2_cloid.to_hex_string()}),
            ))
            .with_status(200)
            .with_body(r#"{"status":"unknownOid"}"#)
            .expect_at_least(2) // W1 policy requires >=2 consecutive unknownOid observations
            .create_async()
            .await;
        // The continuation's own new place: MUST be size 1 (the remainder:
        // 2 total - 1 already filled), never size 2 (the full original
        // total). `.expect(1)`: exactly one new order is placed — asserted
        // explicitly below via `.assert_async()`, since mockito's
        // `.expect(n)` is only CHECKED when a caller asks it to (it is not
        // enforced automatically on drop). This is the ONLY `/exchange`
        // mock on this server, so an unexpected size-2 (full-original-plan)
        // body would simply 501 rather than silently match a fallback.
        let exchange_mock = server
            .mock("POST", "/exchange")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({
                "action": { "orders": [{ "s": "1" }] },
                // The continuation is started in a later process, but its
                // signed/wire expiry must remain bounded by the Header from
                // the original logical run — never "now + --duration".
                "expiresAfter": original_deadline,
            })))
            .with_status(200)
            .with_body(
                r#"{"status":"ok","response":{"type":"order","data":{"statuses":[
                    {"filled":{"oid":99,"totalSz":"1","avgPx":"50"}}]}}}"#,
            )
            .expect(1)
            .create_async()
            .await;

        std::env::set_var("HL_AGENT_PK", TEST_PK);
        std::env::set_var("HL_AGENT_ADDRESS", AGENT);
        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));

        let args = live_cli_two_slice(
            &["--network", "testnet", "--resume", "run-to-resume-partial"],
            &state_dir,
        );
        let cli = Cli::try_parse_from(&args).unwrap();
        let result = run_with_cli(cli).await;

        std::env::remove_var("HL_AGENT_PK");
        std::env::remove_var("HL_AGENT_ADDRESS");
        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");

        result.expect("resume continuing only the remainder must succeed");

        // Explicitly assert the `/exchange` mock's call-count expectation:
        // exactly one new order was placed, sized to the remainder only.
        // The pre-fix double-execution bug (placing the full original 2
        // HYPE on top of the already-filled 1 HYPE) would either fail
        // body-matching (a size-2 order would not match this mock's
        // `s: "1"` matcher, surfacing as an unmatched-request 501) or, if
        // some other body shape happened to match, would trip THIS
        // assertion by calling the mock more than once.
        exchange_mock.assert_async().await;

        // Final accounting: prior journaled fill (1, slice 1) + this run's
        // own newly-executed fill (1, slice 2's remainder) sums to EXACTLY
        // the original 2 HYPE target — each fill counted exactly once.
        let records = hype_trigger_twap::journal::ExecutionJournal::read_all(
            &state_dir,
            "run-to-resume-partial",
        )
        .unwrap();
        let summary = hype_trigger_twap::journal::summarize(&records).unwrap();
        assert_eq!(
            summary.total_filled(),
            rust_decimal::Decimal::from(2),
            "resumed run must account for exactly the original 2 HYPE target, \
             each fill counted once: {records:?}"
        );
        assert!(
            summary.unresolved_cloids().is_empty(),
            "every cloid must be resolved by the end of the resumed run: {records:?}"
        );
    }

    /// A legacy hash-only journal is reconciled, but can never authorize a
    /// new order because it omits execution-affecting fields added later.
    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn legacy_hash_only_resume_reconciles_then_refuses_new_orders() {
        let tmp = TempDir::new();
        let state_dir = tmp.path().join("state");
        let prior_cloid = hype_trigger_twap::types::Cloid::new();

        {
            let mut j = hype_trigger_twap::journal::ExecutionJournal::start(
                &state_dir,
                "run-mismatched".into(),
                hype_trigger_twap::journal::RunHeader {
                    run_id: "run-mismatched".into(),
                    network: "testnet".into(),
                    agent: Some(hype_trigger_twap::types::Address::new(AGENT)),
                    master: Some(hype_trigger_twap::types::Address::new(MASTER)),
                    symbol: hype_trigger_twap::types::Symbol::new("HYPE"),
                    side: hype_trigger_twap::types::Side::Long,
                    slices: 1,
                    plan_hash: "this-will-never-match-a-real-hash".into(),
                    execution_fingerprint: None,
                    started_at_unix_ms: 0,
                    execution_deadline_unix_ms: None,
                },
            )
            .unwrap();
            j.record(&hype_trigger_twap::journal::JournalRecord::Prepared {
                slice_idx: 1,
                cloid: prior_cloid,
                nonce: None,
                symbol: hype_trigger_twap::types::Symbol::new("HYPE"),
                side: hype_trigger_twap::types::Side::Long,
                px: "50".into(),
                sz: "1".into(),
                tif: None,
            })
            .unwrap();
            j.record(
                &hype_trigger_twap::journal::JournalRecord::SubmittedUnknown {
                    slice_idx: 1,
                    cloid: prior_cloid,
                },
            )
            .unwrap();
        }

        // Reconciliation (which the brief requires to run BEFORE any
        // plan-consistency check, since it must never be skipped) is mocked
        // to succeed quickly via orderStatus; meta/userRole/l2Book are
        // mocked so sizing can complete and reach the plan_hash comparison.
        // NO `/exchange` mock is registered: the mismatch must be caught
        // before any NEW place is attempted, so a place attempt here would
        // 501 and fail this test as intended evidence that it never happened.
        let mut server = mockito::Server::new_async().await;
        mock_full_live_run(&mut server).await;
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "orderStatus", "oid": prior_cloid.to_hex_string()}),
            ))
            .with_status(200)
            .with_body(format!(
                r#"{{"status":"order","order":{{"order":{{"oid":42,"coin":"HYPE","side":"B","cloid":"{prior_cloid}","origSz":"1","sz":"0"}},"status":"filled","statusTimestamp":0}}}}"#
            ))
            .create_async()
            .await;

        std::env::set_var("HL_AGENT_PK", TEST_PK);
        std::env::set_var("HL_AGENT_ADDRESS", AGENT);
        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));

        let args = live_cli(
            &["--network", "testnet", "--resume", "run-mismatched"],
            &state_dir,
        );
        let cli = Cli::try_parse_from(&args).unwrap();
        let result = run_with_cli(cli).await;

        std::env::remove_var("HL_AGENT_PK");
        std::env::remove_var("HL_AGENT_ADDRESS");
        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");

        let err = result.expect_err("a legacy journal must reject continuation");
        assert!(err.contains("legacy journal"), "{err}");
        assert!(err.contains("reconciled"), "{err}");
        assert!(err.contains("--abandon-incomplete-run"), "{err}");
        let records =
            hype_trigger_twap::journal::ExecutionJournal::read_all(&state_dir, "run-mismatched")
                .unwrap();
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(
                    record,
                    hype_trigger_twap::journal::JournalRecord::Prepared { .. }
                ))
                .count(),
            1,
            "continuation must not add a new Prepared intent: {records:?}"
        );
    }

    /// `--abandon-incomplete-run` force-reconciles the incomplete run's
    /// live cloid, signs its cancel with the symbol's metadata-resolved asset
    /// index, marks the run `Abandoned`, and never places anything new.
    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn abandon_incomplete_run_reconciles_then_stops_without_placing_anything_new() {
        let tmp = TempDir::new();
        let state_dir = tmp.path().join("state");
        let prior_cloid = hype_trigger_twap::types::Cloid::new();

        {
            let mut j = hype_trigger_twap::journal::ExecutionJournal::start(
                &state_dir,
                "run-to-abandon".into(),
                hype_trigger_twap::journal::RunHeader {
                    run_id: "run-to-abandon".into(),
                    network: "testnet".into(),
                    agent: Some(hype_trigger_twap::types::Address::new(AGENT)),
                    master: Some(hype_trigger_twap::types::Address::new(MASTER)),
                    symbol: hype_trigger_twap::types::Symbol::new("HYPE"),
                    side: hype_trigger_twap::types::Side::Long,
                    slices: 1,
                    plan_hash: "irrelevant".into(),
                    execution_fingerprint: None,
                    started_at_unix_ms: 0,
                    execution_deadline_unix_ms: None,
                },
            )
            .unwrap();
            j.record(&hype_trigger_twap::journal::JournalRecord::Prepared {
                slice_idx: 1,
                cloid: prior_cloid,
                nonce: None,
                symbol: hype_trigger_twap::types::Symbol::new("HYPE"),
                side: hype_trigger_twap::types::Side::Long,
                px: "50".into(),
                sz: "1".into(),
                tif: None,
            })
            .unwrap();
            j.record(
                &hype_trigger_twap::journal::JournalRecord::SubmittedUnknown {
                    slice_idx: 1,
                    cloid: prior_cloid,
                },
            )
            .unwrap();
        }

        let mut server = mockito::Server::new_async().await;
        // Put HYPE at index 1. The historical resume placeholder used index
        // 0, which would sign a cancel for the wrong asset and leave this
        // resting order live.
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({"type": "meta"})))
            .with_status(200)
            .with_body(r#"{"universe":[{"name":"BTC","szDecimals":5,"maxLeverage":40,"onlyIsolated":false},{"name":"HYPE","szDecimals":2,"maxLeverage":10,"onlyIsolated":false}]}"#)
            .create_async()
            .await;
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "userRole"}),
            ))
            .with_status(200)
            .with_body(format!(
                r#"{{"role":"agent","data":{{"user":"{MASTER}"}}}}"#
            ))
            .create_async()
            .await;
        let status_bodies = [
            format!(
                r#"{{"status":"order","order":{{"order":{{"oid":42,"coin":"HYPE","side":"B","cloid":"{prior_cloid}","origSz":"1","sz":"1"}},"status":"open","statusTimestamp":0}}}}"#
            )
            .into_bytes(),
            format!(
                r#"{{"status":"order","order":{{"order":{{"oid":42,"coin":"HYPE","side":"B","cloid":"{prior_cloid}","origSz":"1","sz":"1"}},"status":"canceled","statusTimestamp":1}}}}"#
            )
            .into_bytes(),
        ];
        let status_index = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let statuses = server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "orderStatus"}),
            ))
            .with_status(200)
            .with_body_from_request(move |_| {
                let index = status_index.fetch_add(1, Ordering::Relaxed);
                status_bodies
                    .get(index)
                    .or_else(|| status_bodies.last())
                    .expect("orderStatus sequence is non-empty")
                    .clone()
            })
            .expect(2)
            .create_async()
            .await;
        let cancel = server
            .mock("POST", "/exchange")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({
                "action": {
                    "type": "cancelByCloid",
                    "cancels": [{"asset": 1, "cloid": prior_cloid.to_hex_string()}]
                }
            })))
            .with_status(200)
            .with_body(
                r#"{"status":"ok","response":{"type":"cancel","data":{"statuses":["success"]}}}"#,
            )
            .expect(1)
            .create_async()
            .await;

        std::env::set_var("HL_AGENT_PK", TEST_PK);
        std::env::set_var("HL_AGENT_ADDRESS", AGENT);
        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));

        let args = live_cli(
            &["--network", "testnet", "--abandon-incomplete-run"],
            &state_dir,
        );
        let cli = Cli::try_parse_from(&args).unwrap();
        let result = run_with_cli(cli).await;

        std::env::remove_var("HL_AGENT_PK");
        std::env::remove_var("HL_AGENT_ADDRESS");
        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");

        result.expect("abandon of a fully-mocked incomplete run must succeed");
        statuses.assert_async().await;
        cancel.assert_async().await;

        let records =
            hype_trigger_twap::journal::ExecutionJournal::read_all(&state_dir, "run-to-abandon")
                .unwrap();
        let summary = hype_trigger_twap::journal::summarize(&records).unwrap();
        assert!(
            summary.abandoned,
            "the run must be marked Abandoned: {records:?}"
        );
        assert!(
            summary.unresolved_cloids().is_empty(),
            "the abandoned cloid must still have been reconciled: {records:?}"
        );
    }

    // === Issue #4 / #10: no secrets in the journal (CLI-level audit) ===

    /// End-to-end audit: a completed live run's ENTIRE journal file, byte
    /// for byte, must never contain the private key, its raw hex digits, or
    /// the literal string "SecretString"/"private". This is in addition to
    /// `journal_never_serializes_secret_material` in `src/journal.rs` (which
    /// audits the record TYPES in isolation) — this test audits what an
    /// actual live run, driven through `HL_AGENT_PK`, writes to disk.
    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn live_run_journal_file_never_contains_the_private_key() {
        let tmp = TempDir::new();
        let state_dir = tmp.path().join("state");

        let mut server = mockito::Server::new_async().await;
        mock_full_live_run(&mut server).await;

        std::env::set_var("HL_AGENT_PK", TEST_PK);
        std::env::set_var("HL_AGENT_ADDRESS", AGENT);
        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));

        let args = live_cli(&["--network", "testnet"], &state_dir);
        let cli = Cli::try_parse_from(&args).unwrap();
        let result = run_with_cli(cli).await;

        std::env::remove_var("HL_AGENT_PK");
        std::env::remove_var("HL_AGENT_ADDRESS");
        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");

        result.expect("fully-mocked live run must succeed");

        let runs_dir = state_dir.join("runs");
        let run_id = std::fs::read_dir(&runs_dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .file_name()
            .into_string()
            .unwrap();
        let journal_path = runs_dir.join(&run_id).join("journal.jsonl");
        let raw = std::fs::read_to_string(&journal_path).unwrap();

        let pk_hex = TEST_PK.trim_start_matches("0x");
        assert!(
            !raw.to_lowercase().contains(&pk_hex.to_lowercase()),
            "journal file must never contain the raw private key hex"
        );
        for banned in ["secretstring", "private_key", "signature"] {
            assert!(
                !raw.to_lowercase().contains(banned),
                "journal file must never contain {banned:?}: {raw}"
            );
        }
    }
}
