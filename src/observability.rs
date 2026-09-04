//! Small, dependency-free observability primitives for the execution path.
//!
//! This module deliberately has no knowledge of credentials, request bodies,
//! or exchange responses.  Its event payload is a closed enum, rather than a
//! `serde_json::Value` or arbitrary map, so a caller cannot accidentally add a
//! private key, signature, or raw HTTP payload to the JSONL stream.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use uuid::Uuid;

use crate::journal::{JournalRecord, JournalRecordObserver};
use crate::types::{Cloid, Side, Symbol, Tif};

/// Version of the stable JSONL event schema.
pub const EXECUTION_EVENT_SCHEMA_VERSION: u16 = 1;

/// An event suitable for emitting as one line of secret-safe JSONL.
///
/// `payload` is intentionally a closed enum with no free-form text, maps, or
/// raw request/response fields. Do not add secrets, signatures, or arbitrary
/// error strings to it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecutionEvent {
    pub schema_version: u16,
    pub sequence: u64,
    pub emitted_at: DateTime<Utc>,
    pub payload: ExecutionEventPayload,
}

impl ExecutionEvent {
    pub fn new(sequence: u64, payload: ExecutionEventPayload) -> Self {
        Self {
            schema_version: EXECUTION_EVENT_SCHEMA_VERSION,
            sequence,
            emitted_at: Utc::now(),
            payload,
        }
    }
}

/// The complete permitted execution-event vocabulary.
///
/// This is kept intentionally small and contains only public execution
/// metadata. In particular, it has no generic string or JSON-value variant.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExecutionEventPayload {
    RunStarted {
        run_id: Uuid,
        symbol: Symbol,
        side: Side,
        planned_slices: u32,
        /// Simulation and live executions are intentionally distinguishable.
        mode: ExecutionMode,
    },
    PreflightCompleted {
        mode: ExecutionMode,
    },
    RunResumed,
    SlicePrepared {
        slice_index: u32,
        cloid: Cloid,
    },
    SliceSubmitted {
        slice_index: u32,
        cloid: Cloid,
    },
    SliceAcknowledged {
        slice_index: u32,
        cloid: Cloid,
    },
    SliceTerminal {
        slice_index: u32,
        cloid: Cloid,
        status: SliceStatus,
    },
    Fill {
        slice_index: u32,
        filled_size: Decimal,
        filled_notional: Decimal,
    },
    CapNear {
        remaining_notional: Decimal,
    },
    CapRemaining {
        remaining_notional: Decimal,
    },
    Reconciliation {
        outcome: ReconciliationOutcome,
        unresolved_orders: u64,
    },
    SliceCompleted {
        slice_index: u32,
        status: SliceStatus,
        filled_size: Decimal,
    },
    FinalReport {
        outcome: RunOutcome,
        completed_slices: u32,
    },
    RunStopped {
        reason: StopReason,
    },
    PairLegAbnormal {
        leg: PairLeg,
        reason: FailureReason,
    },
    ExecutionFailed {
        stage: ExecutionStage,
        reason: FailureReason,
    },
}

/// Execution mode appears in JSONL only; it is never a Prometheus label.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    ReadOnly,
    Live,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReconciliationOutcome {
    Completed,
    Unresolved,
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Completed,
    CapReached,
    Deadline,
    Interrupted,
    Abort,
    ReconciliationFailed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PairLeg {
    One,
    Two,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SliceStatus {
    Filled,
    Cancelled,
    Rejected,
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    Completed,
    Aborted,
    Incomplete,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStage {
    Trigger,
    Preparation,
    Submission,
    Reconciliation,
    Shutdown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FailureReason {
    Timeout,
    Transport,
    ExchangeRejected,
    InvalidResponse,
    RiskLimit,
    Interrupted,
}

/// A sequential JSONL event writer. It owns no global state and can be
/// constructed around a file, stdout, or an in-memory writer in tests.
pub struct JsonlEventWriter<W> {
    writer: W,
    next_sequence: u64,
}

/// A best-effort event writer.  Unlike [`ExecutionJournal`][crate::journal::ExecutionJournal],
/// an event write failure is intentionally returned to the caller as data, not
/// a trading error: the journal remains the execution source of truth.
pub struct ObservedEventWriter<W> {
    writer: JsonlEventWriter<W>,
    metrics: Arc<MetricsRegistry>,
}

/// Deterministic projection from durable journal records to observational
/// events. The caller invokes this only *after* `ExecutionJournal::record`
/// succeeds, so an event stream can never claim a submitted/terminal action
/// that has not first become durable in the journal. Projection failure or an
/// unavailable event writer must be ignored by the execution path.
pub struct JournalEventProjector {
    mode: ExecutionMode,
    prepared_orders: HashMap<Cloid, (Decimal, Side, Option<Tif>)>,
    terminal_accounting: HashMap<Cloid, (Decimal, Decimal)>,
    completed_slices: HashSet<u32>,
    completed_slices_from_prior_phases: u32,
    max_notional: Option<Decimal>,
    accounted_notional: Decimal,
    cap_near_emitted: bool,
    reconciliation_seen: bool,
}

impl JournalEventProjector {
    pub fn new(mode: ExecutionMode) -> Self {
        Self {
            mode,
            prepared_orders: HashMap::new(),
            terminal_accounting: HashMap::new(),
            completed_slices: HashSet::new(),
            completed_slices_from_prior_phases: 0,
            max_notional: None,
            accounted_notional: Decimal::ZERO,
            cap_near_emitted: false,
            reconciliation_seen: false,
        }
    }

    pub fn project(&mut self, record: &JournalRecord) -> Vec<ExecutionEventPayload> {
        match record {
            JournalRecord::Header(header) => Uuid::parse_str(&header.run_id)
                .ok()
                .map(|run_id| {
                    self.max_notional = header
                        .execution_fingerprint
                        .as_ref()
                        .and_then(|fingerprint| {
                            fingerprint.max_notional_usd.parse::<Decimal>().ok()
                        })
                        .filter(|cap| *cap > Decimal::ZERO && *cap != Decimal::MAX);
                    let phase_count = header
                        .execution_fingerprint
                        .as_ref()
                        .map(|fingerprint| fingerprint.position_phases.len().max(1))
                        .unwrap_or(1);
                    let planned_slices = header
                        .slices
                        .saturating_mul(u32::try_from(phase_count).unwrap_or(u32::MAX));
                    let mut events = vec![ExecutionEventPayload::RunStarted {
                        run_id,
                        symbol: header.symbol.clone(),
                        side: header.side,
                        planned_slices,
                        mode: self.mode,
                    }];
                    if let Some(remaining_notional) = self.max_notional {
                        events.push(ExecutionEventPayload::CapRemaining { remaining_notional });
                    }
                    events
                })
                .unwrap_or_default(),
            JournalRecord::Prepared {
                slice_idx,
                cloid,
                side,
                tif,
                px,
                ..
            } => {
                if let Ok(price) = px.parse() {
                    self.prepared_orders.insert(*cloid, (price, *side, *tif));
                }
                vec![ExecutionEventPayload::SlicePrepared {
                    slice_index: *slice_idx,
                    cloid: *cloid,
                }]
            }
            JournalRecord::SubmittedUnknown { slice_idx, cloid } => {
                self.reconciliation_seen = true;
                vec![ExecutionEventPayload::SliceSubmitted {
                    slice_index: *slice_idx,
                    cloid: *cloid,
                }]
            }
            JournalRecord::Acknowledged {
                slice_idx, cloid, ..
            } => vec![ExecutionEventPayload::SliceAcknowledged {
                slice_index: *slice_idx,
                cloid: *cloid,
            }],
            JournalRecord::Terminal {
                slice_idx,
                cloid,
                status,
                filled_sz,
                avg_px,
            } => {
                let terminal_status = journal_status(status);
                let mut events = vec![ExecutionEventPayload::SliceTerminal {
                    slice_index: *slice_idx,
                    cloid: *cloid,
                    status: terminal_status,
                }];
                if let Ok(filled_size) = filled_sz.parse::<Decimal>() {
                    let price = avg_px
                        .as_ref()
                        .and_then(|value| value.parse::<Decimal>().ok())
                        .or_else(|| {
                            self.prepared_orders
                                .get(cloid)
                                .and_then(|(price, side, tif)| {
                                    // A buy limit is an upper bound on notional;
                                    // an ALO order fills at its exact resting
                                    // price. A short IOC/GTC limit is only a lower
                                    // bound, so missing avg_px must not fabricate a
                                    // reassuring cap metric.
                                    (*side == Side::Long || *tif == Some(Tif::Alo))
                                        .then_some(*price)
                                })
                        });
                    if let Some(filled_notional) =
                        price.and_then(|price| filled_size.checked_mul(price))
                    {
                        let previous = self
                            .terminal_accounting
                            .insert(*cloid, (filled_size, filled_notional))
                            .unwrap_or((Decimal::ZERO, Decimal::ZERO));
                        let delta_size = filled_size
                            .checked_sub(previous.0)
                            .unwrap_or(Decimal::ZERO)
                            .max(Decimal::ZERO);
                        let delta_notional = filled_notional
                            .checked_sub(previous.1)
                            .unwrap_or(Decimal::ZERO)
                            .max(Decimal::ZERO);
                        self.accounted_notional = self
                            .accounted_notional
                            .checked_add(delta_notional)
                            .unwrap_or(Decimal::MAX);
                        if delta_size > Decimal::ZERO || delta_notional > Decimal::ZERO {
                            events.push(ExecutionEventPayload::Fill {
                                slice_index: *slice_idx,
                                filled_size: delta_size,
                                filled_notional: delta_notional,
                            });
                        }
                    } else if filled_size > Decimal::ZERO {
                        // The durable replay will reject unsafe missing-price
                        // accounting. Still expose size progress without
                        // inventing notional in this best-effort projection.
                        events.push(ExecutionEventPayload::Fill {
                            slice_index: *slice_idx,
                            filled_size,
                            filled_notional: Decimal::ZERO,
                        });
                    }
                }
                self.completed_slices.insert(*slice_idx);
                events.push(ExecutionEventPayload::SliceCompleted {
                    slice_index: *slice_idx,
                    status: terminal_status,
                    filled_size: filled_sz.parse().unwrap_or(Decimal::ZERO),
                });
                if let Some(cap) = self.max_notional {
                    let remaining_notional = cap
                        .checked_sub(self.accounted_notional)
                        .unwrap_or(Decimal::ZERO)
                        .max(Decimal::ZERO);
                    events.push(ExecutionEventPayload::CapRemaining { remaining_notional });
                    let near_threshold = cap.checked_div(Decimal::TEN).unwrap_or(Decimal::ZERO);
                    if !self.cap_near_emitted && remaining_notional <= near_threshold {
                        self.cap_near_emitted = true;
                        events.push(ExecutionEventPayload::CapNear { remaining_notional });
                    }
                }
                events
            }
            JournalRecord::FinalReport {
                completed,
                outcome_unknown_cloids,
                whole_run,
                note,
                ..
            } => {
                let unresolved_orders = whole_run
                    .as_ref()
                    .map_or(outcome_unknown_cloids.len() as u64, |summary| {
                        summary.unresolved_cloids as u64
                    });
                // Position-aware execution deliberately writes an incomplete
                // checkpoint after each child phase, then performs an
                // authoritative clearinghouseState verification. It is not a
                // run terminal event: emitting FinalReport here would produce
                // a false completion/abort in the middle of a zero crossing.
                if !*completed
                    && unresolved_orders == 0
                    && note == "phase completed; final position verification deferred"
                {
                    self.completed_slices_from_prior_phases = self
                        .completed_slices_from_prior_phases
                        .saturating_add(self.completed_slices.len() as u32);
                    self.completed_slices.clear();
                    return whole_run
                        .as_ref()
                        .and_then(|summary| summary.cap_remaining.as_ref())
                        .and_then(|value| value.parse::<Decimal>().ok())
                        .map(|remaining_notional| {
                            vec![ExecutionEventPayload::CapRemaining { remaining_notional }]
                        })
                        .unwrap_or_default();
                }
                let outcome = if *completed {
                    RunOutcome::Completed
                } else if unresolved_orders > 0 {
                    RunOutcome::Incomplete
                } else {
                    RunOutcome::Aborted
                };
                let stop_reason = if unresolved_orders > 0 {
                    StopReason::ReconciliationFailed
                } else if *completed {
                    StopReason::Completed
                } else {
                    final_stop_reason(note)
                };
                let reconciliation_failed =
                    note.to_ascii_lowercase().contains("reconciliation failed");
                let mut events = Vec::new();
                if unresolved_orders > 0 || self.reconciliation_seen || reconciliation_failed {
                    events.push(ExecutionEventPayload::Reconciliation {
                        outcome: if reconciliation_failed {
                            ReconciliationOutcome::Failed
                        } else if unresolved_orders == 0 {
                            ReconciliationOutcome::Completed
                        } else {
                            ReconciliationOutcome::Unresolved
                        },
                        unresolved_orders,
                    });
                }
                if !*completed
                    && unresolved_orders == 0
                    && stop_reason == StopReason::Abort
                    && !reconciliation_failed
                {
                    events.push(ExecutionEventPayload::ExecutionFailed {
                        stage: failure_stage(note),
                        reason: failure_reason(note),
                    });
                }
                events.push(ExecutionEventPayload::RunStopped {
                    reason: stop_reason,
                });
                events.push(ExecutionEventPayload::FinalReport {
                    outcome,
                    completed_slices: self
                        .completed_slices_from_prior_phases
                        .saturating_add(self.completed_slices.len() as u32),
                });
                if let Some(remaining_notional) = whole_run
                    .as_ref()
                    .and_then(|summary| summary.cap_remaining.as_ref())
                    .and_then(|value| value.parse::<Decimal>().ok())
                {
                    let terminal_index = events.len().saturating_sub(2);
                    events.insert(
                        terminal_index,
                        ExecutionEventPayload::CapRemaining { remaining_notional },
                    );
                }
                events
            }
            JournalRecord::Abandoned { .. } => vec![
                ExecutionEventPayload::RunStopped {
                    reason: StopReason::Abort,
                },
                ExecutionEventPayload::FinalReport {
                    outcome: RunOutcome::Aborted,
                    completed_slices: self
                        .completed_slices_from_prior_phases
                        .saturating_add(self.completed_slices.len() as u32),
                },
            ],
        }
    }
}

fn journal_status(status: &str) -> SliceStatus {
    match status {
        "filled" => SliceStatus::Filled,
        "canceled" | "cancelled" => SliceStatus::Cancelled,
        "rejected" => SliceStatus::Rejected,
        _ => SliceStatus::Unknown,
    }
}

/// Classifies fixed operational conditions without exporting the free-form
/// journal note. Unknown text intentionally collapses to Abort.
fn final_stop_reason(note: &str) -> StopReason {
    let lower = note.to_ascii_lowercase();
    if lower.contains("notional cap") || lower.contains("cap reached") {
        StopReason::CapReached
    } else if lower.contains("deadline") || lower.contains("duration elapsed") {
        StopReason::Deadline
    } else if lower.contains("sigint")
        || lower.contains("sigterm")
        || lower.contains("interrupted")
        || lower.contains("shutdown requested")
    {
        StopReason::Interrupted
    } else {
        StopReason::Abort
    }
}

fn failure_stage(note: &str) -> ExecutionStage {
    let lower = note.to_ascii_lowercase();
    if lower.contains("reconcil") {
        ExecutionStage::Reconciliation
    } else if lower.contains("trigger") {
        ExecutionStage::Trigger
    } else if lower.contains("preflight")
        || lower.contains("position")
        || lower.contains("book")
        || lower.contains("meta")
    {
        ExecutionStage::Preparation
    } else if lower.contains("order")
        || lower.contains("exchange")
        || lower.contains("cancel")
        || lower.contains("settle")
        || lower.contains("fill")
    {
        ExecutionStage::Submission
    } else {
        ExecutionStage::Shutdown
    }
}

fn failure_reason(note: &str) -> FailureReason {
    let lower = note.to_ascii_lowercase();
    if lower.contains("timeout") || lower.contains("timed out") {
        FailureReason::Timeout
    } else if lower.contains("transport")
        || lower.contains("network")
        || lower.contains("http")
        || lower.contains("request failed")
    {
        FailureReason::Transport
    } else if lower.contains("reject") {
        FailureReason::ExchangeRejected
    } else if lower.contains("cap") || lower.contains("risk") {
        FailureReason::RiskLimit
    } else if lower.contains("interrupt")
        || lower.contains("signal")
        || lower.contains("shutdown requested")
    {
        FailureReason::Interrupted
    } else {
        FailureReason::InvalidResponse
    }
}

impl<W: Write> ObservedEventWriter<W> {
    pub fn new(writer: W, metrics: Arc<MetricsRegistry>) -> Self {
        Self {
            writer: JsonlEventWriter::new(writer),
            metrics,
        }
    }

    pub fn try_emit(&mut self, payload: ExecutionEventPayload) -> io::Result<ExecutionEvent> {
        // Metrics describe durable execution state, not sidecar health. Feed
        // them even when the best-effort JSONL write itself fails.
        self.metrics.observe_event(&payload);
        let result = self.writer.emit(payload);
        if result.is_err() {
            self.metrics.increment(Metric::EventWriteFailures);
        }
        result
    }

    pub fn into_inner(self) -> W {
        self.writer.into_inner()
    }
}

/// Open a sidecar JSONL event stream. This is intentionally a separate file
/// from `journal.jsonl`; callers must treat an error here as "observability
/// unavailable" and continue with the durable journal path.
pub fn open_event_log(
    path: &Path,
    metrics: Arc<MetricsRegistry>,
) -> io::Result<ObservedEventWriter<std::fs::File>> {
    // A crash may leave a partial JSON object at EOF.  Treat every complete,
    // valid line as authoritative for the sequence, then ensure the next
    // append starts on its own line.  This sidecar is deliberately
    // best-effort: an I/O error merely makes observability unavailable; it
    // must never affect journal durability or trading.
    let (next_sequence, needs_line_boundary) = match std::fs::read(path) {
        Ok(existing) => {
            let next_sequence = existing
                .split(|byte| *byte == b'\n')
                .filter_map(|line| std::str::from_utf8(line).ok())
                .filter_map(|line| serde_json::from_str::<ExecutionEvent>(line).ok())
                .map(|event| event.sequence)
                .max()
                .unwrap_or(0)
                .saturating_add(1);
            (
                next_sequence,
                !existing.is_empty() && !existing.ends_with(b"\n"),
            )
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => (1, false),
        Err(error) => return Err(error),
    };
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    if needs_line_boundary {
        file.write_all(b"\n")?;
    }
    Ok(ObservedEventWriter {
        writer: JsonlEventWriter::with_next_sequence(file, next_sequence),
        metrics,
    })
}

/// Concrete journal observer used by the binary for a sidecar `events.jsonl`.
/// It is intentionally best-effort: all errors are reduced to a fixed warning
/// and metric; no journal or trade error is returned to its caller.
pub struct JournalEventObserver {
    projector: JournalEventProjector,
    writer: Option<ObservedEventWriter<std::fs::File>>,
    metrics: Arc<MetricsRegistry>,
    alerts: AlertHook,
}

impl JournalEventObserver {
    pub fn open(
        path: &Path,
        mode: ExecutionMode,
        metrics: Arc<MetricsRegistry>,
        alerts: AlertHook,
    ) -> io::Result<Self> {
        let writer = open_event_log(path, Arc::clone(&metrics))?;
        Ok(Self {
            projector: JournalEventProjector::new(mode),
            writer: Some(writer),
            metrics,
            alerts,
        })
    }

    /// Metrics and hooks remain useful when the optional sidecar cannot be
    /// opened. This fallback never writes a file and therefore cannot affect
    /// journal durability or trading control flow.
    pub fn without_event_log(
        mode: ExecutionMode,
        metrics: Arc<MetricsRegistry>,
        alerts: AlertHook,
    ) -> Self {
        Self {
            projector: JournalEventProjector::new(mode),
            writer: None,
            metrics,
            alerts,
        }
    }

    /// Rebuild projector state from the durable journal without duplicating
    /// sidecar events or historical alerts. A fresh process may also ask to
    /// restore its in-memory metrics from the same canonical projection.
    pub fn seed_from_records(&mut self, records: &[JournalRecord], restore_metrics: bool) {
        for record in records {
            for event in self.projector.project(record) {
                if restore_metrics {
                    self.metrics.observe_event(&event);
                }
            }
        }
    }

    fn emit_payload(&mut self, event: ExecutionEventPayload) {
        if let Some(writer) = self.writer.as_mut() {
            if writer.try_emit(event.clone()).is_err() {
                tracing::warn!("observability event write failed; execution continues");
            }
        } else {
            self.metrics.observe_event(&event);
        }
        if let Some(alert) = Self::alert_for(&event) {
            if matches!(self.alerts.try_send(alert), AlertEnqueueOutcome::Dropped) {
                tracing::warn!(
                    "observability alert queue full; alert dropped; execution continues"
                );
            }
        }
    }

    fn alert_for(event: &ExecutionEventPayload) -> Option<Alert> {
        match event {
            ExecutionEventPayload::Reconciliation {
                outcome: ReconciliationOutcome::Failed,
                ..
            } => Some(Alert {
                stage: ExecutionStage::Reconciliation,
                reason: FailureReason::InvalidResponse,
            }),
            ExecutionEventPayload::Reconciliation {
                outcome: ReconciliationOutcome::Unresolved,
                ..
            } => Some(Alert {
                stage: ExecutionStage::Reconciliation,
                reason: FailureReason::Timeout,
            }),
            ExecutionEventPayload::RunStopped {
                reason: StopReason::CapReached,
            } => Some(Alert {
                stage: ExecutionStage::Shutdown,
                reason: FailureReason::RiskLimit,
            }),
            ExecutionEventPayload::RunStopped {
                reason: StopReason::Deadline,
            } => Some(Alert {
                stage: ExecutionStage::Shutdown,
                reason: FailureReason::Timeout,
            }),
            ExecutionEventPayload::CapNear { .. } => Some(Alert {
                stage: ExecutionStage::Shutdown,
                reason: FailureReason::RiskLimit,
            }),
            ExecutionEventPayload::ExecutionFailed { stage, reason } => Some(Alert {
                stage: *stage,
                reason: *reason,
            }),
            _ => None,
        }
    }

    /// Project a record already known to be durable (or an existing Header
    /// replayed when attaching to a resumed journal).
    pub fn observe_record(&mut self, record: &JournalRecord) {
        for event in self.projector.project(record) {
            self.emit_payload(event);
        }
    }

    pub fn emit_resume(&mut self) {
        self.emit_payload(ExecutionEventPayload::RunResumed);
    }

    pub fn emit_preflight(&mut self) {
        self.emit_payload(ExecutionEventPayload::PreflightCompleted {
            mode: self.projector.mode,
        });
    }

    pub fn emit_simulation_started(
        &mut self,
        run_id: Uuid,
        symbol: Symbol,
        side: Side,
        planned_slices: u32,
    ) {
        self.emit_payload(ExecutionEventPayload::RunStarted {
            run_id,
            symbol,
            side,
            planned_slices,
            mode: ExecutionMode::ReadOnly,
        });
        self.emit_preflight();
    }

    /// Close a state-free read-only simulation stream. Unlike live mode there
    /// are no durable order records to project, so the caller supplies only
    /// the aggregate simulated-slice count and outcome.
    pub fn emit_simulation_final(&mut self, completed_slices: u32, completed: bool) {
        if !completed {
            self.emit_payload(ExecutionEventPayload::ExecutionFailed {
                stage: ExecutionStage::Shutdown,
                reason: FailureReason::InvalidResponse,
            });
        }
        self.emit_payload(ExecutionEventPayload::RunStopped {
            reason: if completed {
                StopReason::Completed
            } else {
                StopReason::Abort
            },
        });
        self.emit_payload(ExecutionEventPayload::FinalReport {
            outcome: if completed {
                RunOutcome::Completed
            } else {
                RunOutcome::Aborted
            },
            completed_slices,
        });
    }
}

impl JournalRecordObserver for JournalEventObserver {
    fn observe(&mut self, record: &JournalRecord) {
        self.observe_record(record);
    }
}

impl<W: Write> JsonlEventWriter<W> {
    pub fn new(writer: W) -> Self {
        Self::with_next_sequence(writer, 1)
    }

    pub fn with_next_sequence(writer: W, next_sequence: u64) -> Self {
        Self {
            writer,
            next_sequence: next_sequence.max(1),
        }
    }

    /// Append exactly one JSON object followed by a newline and flush it.
    pub fn emit(&mut self, payload: ExecutionEventPayload) -> io::Result<ExecutionEvent> {
        let event = ExecutionEvent::new(self.next_sequence, payload);
        serde_json::to_writer(&mut self.writer, &event).map_err(io::Error::other)?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        self.next_sequence = self.next_sequence.saturating_add(1);
        Ok(event)
    }

    pub fn into_inner(self) -> W {
        self.writer
    }
}

/// The only supported metrics. There is intentionally no API accepting
/// labels: symbols, run IDs, cloids, addresses, and error text are all high
/// cardinality and must stay in JSONL events instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    ExecutionEvents,
    ExecutionFailures,
    EventWriteFailures,
    AlertsEnqueued,
    AlertsDropped,
    AlertDeliveryFailures,
}

/// A small fixed-cardinality Prometheus counter registry.
#[derive(Default)]
pub struct MetricsRegistry {
    execution_events: AtomicU64,
    execution_failures: AtomicU64,
    event_write_failures: AtomicU64,
    alerts_enqueued: AtomicU64,
    alerts_dropped: AtomicU64,
    alert_delivery_failures: AtomicU64,
    gauges: Mutex<RunMetricGauges>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RunMetricGauges {
    state: &'static str,
    filled_size: Decimal,
    filled_notional: Decimal,
    cap_remaining_notional: Decimal,
    unresolved_orders: u64,
    api_errors: u64,
    reconciliation_errors: u64,
    exit_reason: &'static str,
}

impl Default for RunMetricGauges {
    fn default() -> Self {
        Self {
            state: "idle",
            filled_size: Decimal::ZERO,
            filled_notional: Decimal::ZERO,
            cap_remaining_notional: Decimal::ZERO,
            unresolved_orders: 0,
            api_errors: 0,
            reconciliation_errors: 0,
            exit_reason: "none",
        }
    }
}

impl MetricsRegistry {
    pub fn increment(&self, metric: Metric) {
        let counter = match metric {
            Metric::ExecutionEvents => &self.execution_events,
            Metric::ExecutionFailures => &self.execution_failures,
            Metric::EventWriteFailures => &self.event_write_failures,
            Metric::AlertsEnqueued => &self.alerts_enqueued,
            Metric::AlertsDropped => &self.alerts_dropped,
            Metric::AlertDeliveryFailures => &self.alert_delivery_failures,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Feed metrics from the same closed event contract used for JSONL. This
    /// deliberately accepts no labels or arbitrary dimensions.
    pub fn observe_event(&self, event: &ExecutionEventPayload) {
        self.increment(Metric::ExecutionEvents);
        let mut g = self
            .gauges
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match event {
            ExecutionEventPayload::RunStarted { .. } | ExecutionEventPayload::RunResumed => {
                g.state = "running";
                g.exit_reason = "none";
            }
            ExecutionEventPayload::Fill {
                filled_size,
                filled_notional,
                ..
            } => {
                g.filled_size += *filled_size;
                g.filled_notional += *filled_notional;
            }
            // SubmittedUnknown exists only when the `/exchange` response was
            // ambiguous (transport/read failure after send). Reconciliation
            // may later recover the order, but the API failure itself remains
            // an operational signal and must not disappear from metrics.
            ExecutionEventPayload::SliceSubmitted { .. } => {
                g.api_errors = g.api_errors.saturating_add(1);
            }
            ExecutionEventPayload::CapNear { remaining_notional }
            | ExecutionEventPayload::CapRemaining { remaining_notional } => {
                g.cap_remaining_notional = *remaining_notional
            }
            ExecutionEventPayload::Reconciliation {
                outcome,
                unresolved_orders,
            } => match outcome {
                ReconciliationOutcome::Unresolved => {
                    g.unresolved_orders = *unresolved_orders;
                    g.reconciliation_errors = g.reconciliation_errors.saturating_add(1);
                }
                ReconciliationOutcome::Failed => {
                    g.unresolved_orders = *unresolved_orders;
                    g.reconciliation_errors = g.reconciliation_errors.saturating_add(1)
                }
                ReconciliationOutcome::Completed => g.unresolved_orders = 0,
            },
            ExecutionEventPayload::ExecutionFailed { stage, .. } => {
                self.increment(Metric::ExecutionFailures);
                if !matches!(stage, ExecutionStage::Shutdown) {
                    g.api_errors = g.api_errors.saturating_add(1);
                }
                if matches!(stage, ExecutionStage::Reconciliation) {
                    g.reconciliation_errors = g.reconciliation_errors.saturating_add(1);
                }
            }
            ExecutionEventPayload::RunStopped { reason } => {
                g.state = "stopped";
                g.exit_reason = stop_reason_name(*reason);
            }
            ExecutionEventPayload::FinalReport { outcome, .. } => {
                g.state = "finished";
                if g.exit_reason == "none" {
                    g.exit_reason = run_outcome_name(*outcome);
                }
            }
            _ => {}
        }
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            execution_events: self.execution_events.load(Ordering::Relaxed),
            execution_failures: self.execution_failures.load(Ordering::Relaxed),
            event_write_failures: self.event_write_failures.load(Ordering::Relaxed),
            alerts_enqueued: self.alerts_enqueued.load(Ordering::Relaxed),
            alerts_dropped: self.alerts_dropped.load(Ordering::Relaxed),
            alert_delivery_failures: self.alert_delivery_failures.load(Ordering::Relaxed),
            gauges: self
                .gauges
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
        }
    }

    /// Render the fixed registry in Prometheus text exposition format.
    pub fn prometheus_text(&self) -> String {
        self.snapshot().prometheus_text()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricsSnapshot {
    pub execution_events: u64,
    pub execution_failures: u64,
    pub event_write_failures: u64,
    pub alerts_enqueued: u64,
    pub alerts_dropped: u64,
    pub alert_delivery_failures: u64,
    gauges: RunMetricGauges,
}

impl MetricsSnapshot {
    pub fn prometheus_text(self) -> String {
        format!(
            "# TYPE hype_twap_execution_events_total counter\nhype_twap_execution_events_total {}\n# TYPE hype_twap_execution_failures_total counter\nhype_twap_execution_failures_total {}\n# TYPE hype_twap_event_write_failures_total counter\nhype_twap_event_write_failures_total {}\n# TYPE hype_twap_alerts_enqueued_total counter\nhype_twap_alerts_enqueued_total {}\n# TYPE hype_twap_alerts_dropped_total counter\nhype_twap_alerts_dropped_total {}\n# TYPE hype_twap_alert_delivery_failures_total counter\nhype_twap_alert_delivery_failures_total {}\n# TYPE hype_twap_run_state gauge\nhype_twap_run_state{{state=\"{}\"}} 1\n# TYPE hype_twap_filled_size gauge\nhype_twap_filled_size {}\n# TYPE hype_twap_filled_notional_usd gauge\nhype_twap_filled_notional_usd {}\n# TYPE hype_twap_cap_remaining_notional_usd gauge\nhype_twap_cap_remaining_notional_usd {}\n# TYPE hype_twap_unresolved_orders gauge\nhype_twap_unresolved_orders {}\n# TYPE hype_twap_api_errors_total counter\nhype_twap_api_errors_total {}\n# TYPE hype_twap_reconciliation_errors_total counter\nhype_twap_reconciliation_errors_total {}\n# TYPE hype_twap_exit_reason gauge\nhype_twap_exit_reason{{reason=\"{}\"}} 1\n",
            self.execution_events,
            self.execution_failures,
            self.event_write_failures,
            self.alerts_enqueued,
            self.alerts_dropped,
            self.alert_delivery_failures,
            self.gauges.state,
            self.gauges.filled_size,
            self.gauges.filled_notional,
            self.gauges.cap_remaining_notional,
            self.gauges.unresolved_orders,
            self.gauges.api_errors,
            self.gauges.reconciliation_errors,
            self.gauges.exit_reason,
        )
    }
}

fn stop_reason_name(reason: StopReason) -> &'static str {
    match reason {
        StopReason::Completed => "completed",
        StopReason::CapReached => "cap_reached",
        StopReason::Deadline => "deadline",
        StopReason::Interrupted => "interrupted",
        StopReason::Abort => "abort",
        StopReason::ReconciliationFailed => "reconciliation_failed",
    }
}
fn run_outcome_name(outcome: RunOutcome) -> &'static str {
    match outcome {
        RunOutcome::Completed => "completed",
        RunOutcome::Aborted => "aborted",
        RunOutcome::Incomplete => "incomplete",
    }
}

/// Start a minimal Prometheus HTTP endpoint. Bind addresses must be loopback
/// unless the caller explicitly opts into external exposure. This server has
/// no labels derived from request data and serves only `/metrics`.
pub async fn start_metrics_server(
    bind: SocketAddr,
    allow_external_bind: bool,
    metrics: Arc<MetricsRegistry>,
) -> io::Result<JoinHandle<()>> {
    if !allow_external_bind && !is_loopback(bind.ip()) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "metrics bind must be loopback unless external exposure is explicitly enabled",
        ));
    }
    let listener = TcpListener::bind(bind).await?;
    Ok(tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let metrics = Arc::clone(&metrics);
            tokio::spawn(async move {
                let mut request = [0_u8; 1024];
                let Ok(n) = stream.read(&mut request).await else {
                    return;
                };
                let path_is_metrics = request[..n].starts_with(b"GET /metrics ");
                let (status, body) = if path_is_metrics {
                    ("200 OK", metrics.prometheus_text())
                } else {
                    ("404 Not Found", String::new())
                };
                let response = format!("HTTP/1.1 {status}\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    }))
}

fn is_loopback(ip: IpAddr) -> bool {
    ip.is_loopback()
}

/// A compact, non-sensitive notification. Its closed payload shares the
/// event schema's no-free-form-secret property.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Alert {
    pub stage: ExecutionStage,
    pub reason: FailureReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlertHookConfig {
    pub queue_capacity: usize,
    pub delivery_timeout: Duration,
}

/// Startup configuration deliberately defaults all network-facing features to
/// disabled. It is suitable for CLI/environment decoding by the binary, but
/// contains no credentials or request payloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservabilityConfig {
    pub metrics_bind: Option<SocketAddr>,
    pub allow_external_metrics_bind: bool,
    pub alert_hook_url: Option<String>,
    pub alert_queue_capacity: usize,
    pub alert_delivery_timeout: Duration,
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            metrics_bind: None,
            allow_external_metrics_bind: false,
            alert_hook_url: None,
            alert_queue_capacity: AlertHookConfig::default().queue_capacity,
            alert_delivery_timeout: AlertHookConfig::default().delivery_timeout,
        }
    }
}

/// Long-lived optional observability services. Dropping this value does not
/// alter execution state; callers may abort the HTTP task during shutdown.
pub struct ObservabilityRuntime {
    pub metrics: Arc<MetricsRegistry>,
    pub alerts: AlertHook,
    pub metrics_server: Option<JoinHandle<()>>,
}

impl ObservabilityRuntime {
    pub fn disabled() -> Self {
        let metrics = Arc::new(MetricsRegistry::default());
        Self {
            alerts: AlertHook::disabled(Arc::clone(&metrics)),
            metrics,
            metrics_server: None,
        }
    }

    pub async fn start(config: &ObservabilityConfig) -> Result<Self, AlertDeliveryError> {
        let metrics = Arc::new(MetricsRegistry::default());
        let alerts = match &config.alert_hook_url {
            Some(url) => AlertHook::enabled(
                Arc::new(HttpAlertDelivery::new(url)?),
                AlertHookConfig {
                    queue_capacity: config.alert_queue_capacity,
                    delivery_timeout: config.alert_delivery_timeout,
                },
                Arc::clone(&metrics),
            ),
            None => AlertHook::disabled(Arc::clone(&metrics)),
        };
        let metrics_server = match config.metrics_bind {
            Some(bind) => Some(
                start_metrics_server(
                    bind,
                    config.allow_external_metrics_bind,
                    Arc::clone(&metrics),
                )
                .await
                .map_err(|_| AlertDeliveryError)?,
            ),
            None => None,
        };
        Ok(Self {
            metrics,
            alerts,
            metrics_server,
        })
    }

    /// Stop optional services without changing the already-determined command
    /// result. Alert draining is bounded; a stuck endpoint is counted and the
    /// worker is aborted rather than delaying process shutdown indefinitely.
    pub async fn shutdown(mut self) {
        if let Some(server) = self.metrics_server.take() {
            server.abort();
        }
        self.alerts.close_and_drain().await;
    }
}

impl Default for AlertHookConfig {
    fn default() -> Self {
        Self {
            queue_capacity: 32,
            delivery_timeout: Duration::from_secs(2),
        }
    }
}

/// Result of one best-effort enqueue attempt. Neither outcome is an error to
/// the caller: alerting must never delay or fail a trading operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertEnqueueOutcome {
    Disabled,
    Enqueued,
    Dropped,
}

/// External delivery adapter. Implementations must not put credentials into
/// [`Alert`]; credentials belong solely in the adapter's private state.
#[async_trait]
pub trait AlertDelivery: Send + Sync + 'static {
    /// Return the HTTP-like status code. Non-2xx responses are counted and
    /// discarded by the worker.
    async fn deliver(&self, alert: Alert) -> Result<u16, AlertDeliveryError>;
}

/// HTTP POST adapter for an optional alert hook. The URL is private adapter
/// configuration, never part of an [`Alert`] or event. Query strings and
/// fragments are rejected so a token cannot accidentally be copied into an
/// observable configuration/log value.
pub struct HttpAlertDelivery {
    client: reqwest::Client,
    url: reqwest::Url,
}

impl HttpAlertDelivery {
    pub fn new(url: &str) -> Result<Self, AlertDeliveryError> {
        let url = reqwest::Url::parse(url).map_err(|_| AlertDeliveryError)?;
        // Remote hooks must use TLS.  The narrowly scoped HTTP exception is
        // only for a literal loopback test receiver; accepting a hostname
        // here would make the exception depend on DNS resolution.
        let loopback_http = url.scheme() == "http"
            && url
                .host_str()
                .and_then(|host| {
                    host.trim_start_matches('[')
                        .trim_end_matches(']')
                        .parse::<IpAddr>()
                        .ok()
                })
                .is_some_and(|address| {
                    address == IpAddr::V4(Ipv4Addr::LOCALHOST)
                        || address == IpAddr::V6(Ipv6Addr::LOCALHOST)
                });
        if !(url.scheme() == "https" || loopback_http)
            || url.query().is_some()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(AlertDeliveryError);
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| AlertDeliveryError)?;
        Ok(Self { client, url })
    }
}

#[async_trait]
impl AlertDelivery for HttpAlertDelivery {
    async fn deliver(&self, alert: Alert) -> Result<u16, AlertDeliveryError> {
        self.client
            .post(self.url.clone())
            .json(&alert)
            .send()
            .await
            .map(|response| response.status().as_u16())
            .map_err(|_| AlertDeliveryError)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlertDeliveryError;

impl fmt::Display for AlertDeliveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("alert delivery failed")
    }
}

impl std::error::Error for AlertDeliveryError {}

/// Best-effort asynchronous alert queue. [`Default`] is disabled; enabling
/// it starts an isolated worker. `try_send` never awaits and never returns a
/// delivery error, timeout, or HTTP status to the execution path.
#[derive(Clone)]
pub struct AlertHook {
    sender: Option<mpsc::Sender<Alert>>,
    worker: Option<Arc<Mutex<Option<JoinHandle<()>>>>>,
    drain_timeout: Duration,
    metrics: Arc<MetricsRegistry>,
}

impl Default for AlertHook {
    fn default() -> Self {
        Self::disabled(Arc::new(MetricsRegistry::default()))
    }
}

impl AlertHook {
    pub fn disabled(metrics: Arc<MetricsRegistry>) -> Self {
        Self {
            sender: None,
            worker: None,
            drain_timeout: Duration::ZERO,
            metrics,
        }
    }

    pub fn enabled(
        delivery: Arc<dyn AlertDelivery>,
        config: AlertHookConfig,
        metrics: Arc<MetricsRegistry>,
    ) -> Self {
        let capacity = config.queue_capacity.max(1);
        let (sender, mut receiver) = mpsc::channel(capacity);
        let worker_metrics = Arc::clone(&metrics);
        let worker = tokio::spawn(async move {
            while let Some(alert) = receiver.recv().await {
                let result = timeout(config.delivery_timeout, delivery.deliver(alert)).await;
                if !matches!(result, Ok(Ok(status)) if (200..300).contains(&status)) {
                    worker_metrics.increment(Metric::AlertDeliveryFailures);
                }
            }
        });
        // A terminal alert commonly arrives immediately before process exit.
        // Retain the worker so the owning runtime can close the channel and
        // give it a short, bounded chance to flush instead of letting Tokio
        // cancel it during runtime teardown.
        let drain_timeout = config
            .delivery_timeout
            .saturating_mul(2)
            .max(Duration::from_millis(10))
            .min(Duration::from_secs(5));
        Self {
            sender: Some(sender),
            worker: Some(Arc::new(Mutex::new(Some(worker)))),
            drain_timeout,
            metrics,
        }
    }

    /// Close the final sender and drain queued delivery work within a fixed
    /// bound. Call this only after journal observers (which hold sender
    /// clones) have been dropped. Delivery outcome never changes trade state
    /// or the command's exit code.
    pub async fn close_and_drain(&mut self) {
        self.sender.take();
        let handle = self.worker.as_ref().and_then(|worker| {
            worker
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
        });
        let Some(mut handle) = handle else {
            return;
        };
        if timeout(self.drain_timeout, &mut handle).await.is_err() {
            handle.abort();
            self.metrics.increment(Metric::AlertDeliveryFailures);
        }
    }

    /// Queue an alert without waiting for I/O. Queue saturation, a shut-down
    /// worker, and disabled delivery are observable only through metrics.
    pub fn try_send(&self, alert: Alert) -> AlertEnqueueOutcome {
        let Some(sender) = &self.sender else {
            return AlertEnqueueOutcome::Disabled;
        };
        match sender.try_send(alert) {
            Ok(()) => {
                self.metrics.increment(Metric::AlertsEnqueued);
                AlertEnqueueOutcome::Enqueued
            }
            Err(_) => {
                self.metrics.increment(Metric::AlertsDropped);
                AlertEnqueueOutcome::Dropped
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    fn event() -> ExecutionEventPayload {
        ExecutionEventPayload::RunStarted {
            run_id: Uuid::nil(),
            symbol: Symbol::from("BTC"),
            side: Side::Long,
            planned_slices: 3,
            mode: ExecutionMode::ReadOnly,
        }
    }

    fn two_phase_header() -> crate::journal::RunHeader {
        crate::journal::RunHeader {
            run_id: Uuid::nil().to_string(),
            network: "testnet".into(),
            agent: None,
            master: None,
            symbol: Symbol::from("BTC"),
            side: Side::Short,
            slices: 3,
            plan_hash: "test".into(),
            execution_fingerprint: Some(crate::journal::ExecutionPlanFingerprint {
                version: crate::journal::ExecutionPlanFingerprint::VERSION,
                symbol: "BTC".into(),
                side: "short".into(),
                request_mode: "target_sz".into(),
                request_value: "1".into(),
                per_slice: "0.333".into(),
                total_adjusted: "1".into(),
                total_requested: "1".into(),
                slices: 3,
                duration_ms: 1_000,
                slippage_bps: "25".into(),
                max_notional_usd: "1000".into(),
                max_book_age_ms: 2_000,
                settle_retries: 25,
                child_algo: "market".into(),
                follow_poll_secs: 1,
                follow_repost_secs: 5,
                follow_threshold_bps: "5".into(),
                network: "testnet".into(),
                agent: None,
                master: None,
                position_mode: Some("target_sz".into()),
                initial_position_szi: Some("-1".into()),
                target_position_szi: Some("1".into()),
                position_requested_value: Some("1".into()),
                position_reference_price: None,
                position_phases: vec![
                    crate::journal::PositionPhaseFingerprint {
                        kind: "close_to_flat".into(),
                        side: "long".into(),
                        size: "1".into(),
                        reduce_only: true,
                    },
                    crate::journal::PositionPhaseFingerprint {
                        kind: "open_from_flat".into(),
                        side: "long".into(),
                        size: "1".into(),
                        reduce_only: false,
                    },
                ],
                reduce_only: false,
                absolute_deadline_unix_ms: Some(2_000),
            }),
            started_at_unix_ms: 1_000,
            execution_deadline_unix_ms: Some(2_000),
        }
    }

    #[test]
    fn zero_crossing_run_started_counts_every_planned_phase_slice() {
        let mut projection = JournalEventProjector::new(ExecutionMode::Live);
        let events = projection.project(&JournalRecord::Header(two_phase_header()));
        assert!(matches!(
            events.first(),
            Some(ExecutionEventPayload::RunStarted {
                planned_slices: 6,
                ..
            })
        ));
    }

    #[test]
    fn jsonl_schema_is_versioned_and_sequence_is_monotonic() {
        let mut writer = JsonlEventWriter::new(Vec::new());
        writer.emit(event()).unwrap();
        writer
            .emit(ExecutionEventPayload::FinalReport {
                outcome: RunOutcome::Completed,
                completed_slices: 3,
            })
            .unwrap();
        let lines: Vec<ExecutionEvent> = String::from_utf8(writer.into_inner())
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].schema_version, EXECUTION_EVENT_SCHEMA_VERSION);
        assert_eq!((lines[0].sequence, lines[1].sequence), (1, 2));
        assert_eq!(lines[0].payload, event());
    }

    #[test]
    fn payload_schema_has_no_unstructured_or_secret_bearing_fields() {
        // This compile-time-shaped event vocabulary deliberately offers no
        // `String`, `Value`, map, signing key, or signature field. The JSON
        // additionally demonstrates the only accepted public fields.
        let json = serde_json::to_value(ExecutionEvent::new(1, event())).unwrap();
        let encoded = json.to_string();
        for forbidden in ["secret", "private_key", "signature", "request", "response"] {
            assert!(
                !encoded.contains(forbidden),
                "unexpected field: {forbidden}"
            );
        }
        assert_eq!(json["payload"]["kind"], "run_started");
    }

    #[test]
    fn prometheus_snapshot_has_only_fixed_cardinality_labels() {
        let registry = MetricsRegistry::default();
        registry.increment(Metric::ExecutionEvents);
        registry.increment(Metric::AlertsDropped);
        let text = registry.prometheus_text();
        assert!(text.contains("hype_twap_execution_events_total 1"));
        assert!(text.contains("hype_twap_alerts_dropped_total 1"));
        assert!(text.contains("state=\"idle\""));
        assert!(!text.contains("symbol"));
        assert!(!text.contains("run_id"));
        assert!(!text.contains("cloid"));
    }

    #[test]
    fn default_hook_is_disabled_and_side_effect_free() {
        let hook = AlertHook::default();
        assert_eq!(hook.try_send(alert()), AlertEnqueueOutcome::Disabled);
        assert_eq!(hook.metrics.snapshot().alerts_enqueued, 0);
    }

    struct FailingDelivery;
    #[async_trait]
    impl AlertDelivery for FailingDelivery {
        async fn deliver(&self, _: Alert) -> Result<u16, AlertDeliveryError> {
            Err(AlertDeliveryError)
        }
    }

    struct BlockingDelivery(AtomicBool);
    #[async_trait]
    impl AlertDelivery for BlockingDelivery {
        async fn deliver(&self, _: Alert) -> Result<u16, AlertDeliveryError> {
            self.0.store(true, Ordering::SeqCst);
            std::future::pending().await
        }
    }

    fn alert() -> Alert {
        Alert {
            stage: ExecutionStage::Submission,
            reason: FailureReason::Timeout,
        }
    }

    struct StatusDelivery(u16);
    #[async_trait]
    impl AlertDelivery for StatusDelivery {
        async fn deliver(&self, _: Alert) -> Result<u16, AlertDeliveryError> {
            Ok(self.0)
        }
    }

    struct ImmediateDelivery(AtomicBool);
    #[async_trait]
    impl AlertDelivery for ImmediateDelivery {
        async fn deliver(&self, _: Alert) -> Result<u16, AlertDeliveryError> {
            self.0.store(true, Ordering::SeqCst);
            Ok(204)
        }
    }

    #[tokio::test]
    async fn close_and_drain_flushes_a_terminal_alert_without_a_caller_sleep() {
        let metrics = Arc::new(MetricsRegistry::default());
        let delivered = Arc::new(ImmediateDelivery(AtomicBool::new(false)));
        let mut hook = AlertHook::enabled(
            delivered.clone(),
            AlertHookConfig::default(),
            Arc::clone(&metrics),
        );
        assert_eq!(hook.try_send(alert()), AlertEnqueueOutcome::Enqueued);
        hook.close_and_drain().await;
        assert!(delivered.0.load(Ordering::SeqCst));
        assert_eq!(metrics.snapshot().alert_delivery_failures, 0);
    }

    #[tokio::test]
    async fn hook_failures_are_isolated_from_enqueue_callers() {
        let metrics = Arc::new(MetricsRegistry::default());
        let hook = AlertHook::enabled(
            Arc::new(FailingDelivery),
            AlertHookConfig {
                delivery_timeout: Duration::from_millis(10),
                ..AlertHookConfig::default()
            },
            Arc::clone(&metrics),
        );
        assert_eq!(hook.try_send(alert()), AlertEnqueueOutcome::Enqueued);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(metrics.snapshot().alert_delivery_failures, 1);
    }

    #[tokio::test]
    async fn full_alert_queue_is_dropped_without_waiting() {
        let metrics = Arc::new(MetricsRegistry::default());
        let blocking = Arc::new(BlockingDelivery(AtomicBool::new(false)));
        let hook = AlertHook::enabled(
            blocking.clone(),
            AlertHookConfig {
                queue_capacity: 1,
                delivery_timeout: Duration::from_secs(60),
            },
            Arc::clone(&metrics),
        );
        assert_eq!(hook.try_send(alert()), AlertEnqueueOutcome::Enqueued);
        for _ in 0..20 {
            if blocking.0.load(Ordering::SeqCst) {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(blocking.0.load(Ordering::SeqCst));
        assert_eq!(hook.try_send(alert()), AlertEnqueueOutcome::Enqueued);
        assert_eq!(hook.try_send(alert()), AlertEnqueueOutcome::Dropped);
        assert_eq!(metrics.snapshot().alerts_dropped, 1);
    }

    #[tokio::test]
    async fn timeout_and_5xx_are_counted_by_isolated_worker() {
        let metrics = Arc::new(MetricsRegistry::default());
        let hook = AlertHook::enabled(
            Arc::new(StatusDelivery(503)),
            AlertHookConfig::default(),
            Arc::clone(&metrics),
        );
        assert_eq!(hook.try_send(alert()), AlertEnqueueOutcome::Enqueued);
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(metrics.snapshot().alert_delivery_failures, 1);

        let timed_out_metrics = Arc::new(MetricsRegistry::default());
        let hook = AlertHook::enabled(
            Arc::new(BlockingDelivery(AtomicBool::new(false))),
            AlertHookConfig {
                delivery_timeout: Duration::from_millis(1),
                ..AlertHookConfig::default()
            },
            Arc::clone(&timed_out_metrics),
        );
        assert_eq!(hook.try_send(alert()), AlertEnqueueOutcome::Enqueued);
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(timed_out_metrics.snapshot().alert_delivery_failures, 1);
    }

    #[test]
    fn metrics_cover_required_execution_gauges_without_user_labels() {
        let metrics = MetricsRegistry::default();
        metrics.observe_event(&event());
        metrics.observe_event(&ExecutionEventPayload::Fill {
            slice_index: 1,
            filled_size: Decimal::new(25, 1),
            filled_notional: Decimal::new(125, 0),
        });
        metrics.observe_event(&ExecutionEventPayload::CapNear {
            remaining_notional: Decimal::new(5, 0),
        });
        metrics.observe_event(&ExecutionEventPayload::Reconciliation {
            outcome: ReconciliationOutcome::Unresolved,
            unresolved_orders: 1,
        });
        metrics.observe_event(&ExecutionEventPayload::RunStopped {
            reason: StopReason::CapReached,
        });
        let text = metrics.prometheus_text();
        for expected in [
            "hype_twap_run_state{state=\"stopped\"} 1",
            "hype_twap_filled_size 2.5",
            "hype_twap_filled_notional_usd 125",
            "hype_twap_cap_remaining_notional_usd 5",
            "hype_twap_unresolved_orders 1",
            "hype_twap_exit_reason{reason=\"cap_reached\"} 1",
        ] {
            assert!(text.contains(expected), "missing {expected} in {text}");
        }
        assert!(!text.contains("run_id"));
    }

    #[tokio::test]
    async fn metrics_http_is_loopback_by_default_and_serves_metrics() {
        let metrics = Arc::new(MetricsRegistry::default());
        let denied =
            start_metrics_server("0.0.0.0:0".parse().unwrap(), false, Arc::clone(&metrics)).await;
        assert_eq!(denied.unwrap_err().kind(), io::ErrorKind::PermissionDenied);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let server = start_metrics_server(addr, false, Arc::clone(&metrics))
            .await
            .unwrap();
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("hype_twap_execution_events_total"));
        server.abort();
    }

    struct FailingWriter;
    impl Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("nope"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn event_write_failure_is_observable_but_not_a_trading_error() {
        let metrics = Arc::new(MetricsRegistry::default());
        let mut events = ObservedEventWriter::new(FailingWriter, Arc::clone(&metrics));
        assert!(events.try_emit(event()).is_err());
        assert_eq!(metrics.snapshot().event_write_failures, 1);
        assert_eq!(metrics.snapshot().execution_events, 1);
    }

    #[test]
    fn alert_hook_requires_https_except_for_literal_loopback_http() {
        assert!(
            HttpAlertDelivery::new("https://hooks.example.invalid/alert?token=secret").is_err()
        );
        assert!(HttpAlertDelivery::new("https://token@hooks.example.invalid/alert").is_err());
        assert!(
            HttpAlertDelivery::new("http://127.0.0.1@evil.example/alert").is_err(),
            "userinfo must not smuggle a remote host through the loopback exception"
        );
        assert!(
            HttpAlertDelivery::new("http://hooks.example.invalid/services/path-secret").is_err()
        );
        assert!(HttpAlertDelivery::new("http://localhost/alert").is_err());
        assert!(HttpAlertDelivery::new("http://127.0.0.1/alert").is_ok());
        assert!(HttpAlertDelivery::new("http://127.0.0.2/alert").is_err());
        assert!(HttpAlertDelivery::new("http://[::1]/alert").is_ok());
        assert!(
            HttpAlertDelivery::new("https://hooks.example.invalid/services/path-secret").is_ok()
        );
    }

    #[tokio::test]
    async fn runtime_defaults_to_no_listener_and_disabled_hook() {
        let runtime = ObservabilityRuntime::start(&ObservabilityConfig::default())
            .await
            .unwrap();
        assert!(runtime.metrics_server.is_none());
        assert_eq!(
            runtime.alerts.try_send(alert()),
            AlertEnqueueOutcome::Disabled
        );
    }

    #[tokio::test]
    async fn runtime_rejects_external_metrics_without_opt_in() {
        let config = ObservabilityConfig {
            metrics_bind: Some("0.0.0.0:0".parse().unwrap()),
            ..ObservabilityConfig::default()
        };
        assert!(ObservabilityRuntime::start(&config).await.is_err());
    }

    #[test]
    fn journal_projection_preserves_durable_ambiguous_send_order() {
        let cloid = Cloid::from_uuid(Uuid::nil());
        let mut projection = JournalEventProjector::new(ExecutionMode::Live);
        let prepared = projection.project(&JournalRecord::Prepared {
            slice_idx: 1,
            cloid,
            nonce: Some(1),
            symbol: Symbol::from("BTC"),
            side: Side::Long,
            tif: None,
            px: "100".into(),
            sz: "2".into(),
        });
        let submitted = projection.project(&JournalRecord::SubmittedUnknown {
            slice_idx: 1,
            cloid,
        });
        let terminal = projection.project(&JournalRecord::Terminal {
            slice_idx: 1,
            cloid,
            status: "filled".into(),
            filled_sz: "2".into(),
            avg_px: Some("101".into()),
        });
        assert!(matches!(
            prepared.as_slice(),
            [ExecutionEventPayload::SlicePrepared { .. }]
        ));
        assert!(matches!(
            submitted.as_slice(),
            [ExecutionEventPayload::SliceSubmitted { .. }]
        ));
        assert!(matches!(
            terminal.as_slice(),
            [
                ExecutionEventPayload::SliceTerminal { .. },
                ExecutionEventPayload::Fill {
                    filled_notional,
                    ..
                },
                ExecutionEventPayload::SliceCompleted { .. }
            ] if *filled_notional == Decimal::new(202, 0)
        ));
        let metrics = MetricsRegistry::default();
        for event in prepared.iter().chain(&submitted).chain(&terminal) {
            metrics.observe_event(event);
        }
        assert_eq!(metrics.snapshot().gauges.api_errors, 1);
    }

    #[test]
    fn resume_failure_and_sigterm_cleanup_project_to_closed_stop_events() {
        let mut projection = JournalEventProjector::new(ExecutionMode::Live);
        let final_events = projection.project(&JournalRecord::FinalReport {
            completed: false,
            filled_total: "0".into(),
            outcome_unknown_cloids: vec![Cloid::from_uuid(Uuid::nil())],
            note: "never exported".into(),
            whole_run: None,
        });
        let cleanup = projection.project(&JournalRecord::Abandoned {
            note: "never exported".into(),
        });
        assert!(matches!(
            final_events.as_slice(),
            [
                ExecutionEventPayload::Reconciliation {
                    outcome: ReconciliationOutcome::Unresolved,
                    unresolved_orders: 1
                },
                ExecutionEventPayload::RunStopped {
                    reason: StopReason::ReconciliationFailed
                },
                ExecutionEventPayload::FinalReport {
                    outcome: RunOutcome::Incomplete,
                    ..
                }
            ]
        ));
        let metrics = MetricsRegistry::default();
        for event in &final_events {
            metrics.observe_event(event);
        }
        assert_eq!(metrics.snapshot().gauges.reconciliation_errors, 1);
        assert!(matches!(
            cleanup.as_slice(),
            [
                ExecutionEventPayload::RunStopped {
                    reason: StopReason::Abort
                },
                ExecutionEventPayload::FinalReport {
                    outcome: RunOutcome::Aborted,
                    completed_slices: 0,
                }
            ]
        ));
    }

    #[test]
    fn final_report_classifies_cap_and_emits_stop_before_final() {
        let mut projection = JournalEventProjector::new(ExecutionMode::Live);
        let events = projection.project(&JournalRecord::FinalReport {
            completed: false,
            filled_total: "0".into(),
            outcome_unknown_cloids: vec![],
            note: "notional cap reached before next order".into(),
            whole_run: None,
        });
        assert!(matches!(
            events.as_slice(),
            [
                ExecutionEventPayload::RunStopped {
                    reason: StopReason::CapReached
                },
                ExecutionEventPayload::FinalReport {
                    outcome: RunOutcome::Aborted,
                    ..
                }
            ]
        ));
        let metrics = MetricsRegistry::default();
        for event in &events {
            metrics.observe_event(event);
        }
        assert!(metrics
            .prometheus_text()
            .contains("hype_twap_exit_reason{reason=\"cap_reached\"} 1"));
    }

    #[test]
    fn abandoned_projects_stop_then_final_with_accumulated_completed_slices() {
        let mut projection = JournalEventProjector::new(ExecutionMode::Live);
        let cloid = Cloid::from_uuid(Uuid::nil());
        projection.project(&JournalRecord::Terminal {
            slice_idx: 4,
            cloid,
            status: "filled".into(),
            filled_sz: "1".into(),
            avg_px: Some("50".into()),
        });
        let events = projection.project(&JournalRecord::Abandoned {
            note: "operator abandoned after reconciliation".into(),
        });
        assert!(matches!(
            events.as_slice(),
            [
                ExecutionEventPayload::RunStopped {
                    reason: StopReason::Abort
                },
                ExecutionEventPayload::FinalReport {
                    outcome: RunOutcome::Aborted,
                    completed_slices: 1,
                }
            ]
        ));
    }

    #[test]
    fn exchange_abort_projects_failure_alert_and_api_error() {
        let mut projection = JournalEventProjector::new(ExecutionMode::Live);
        let events = projection.project(&JournalRecord::FinalReport {
            completed: false,
            filled_total: "0".into(),
            outcome_unknown_cloids: vec![],
            note: "exchange rejected child order".into(),
            whole_run: None,
        });
        assert!(matches!(
            events.as_slice(),
            [
                ExecutionEventPayload::ExecutionFailed {
                    stage: ExecutionStage::Submission,
                    reason: FailureReason::ExchangeRejected,
                },
                ExecutionEventPayload::RunStopped {
                    reason: StopReason::Abort,
                },
                ExecutionEventPayload::FinalReport {
                    outcome: RunOutcome::Aborted,
                    ..
                }
            ]
        ));
        assert!(matches!(
            JournalEventObserver::alert_for(&events[0]),
            Some(Alert {
                stage: ExecutionStage::Submission,
                reason: FailureReason::ExchangeRejected,
            })
        ));
        let metrics = MetricsRegistry::default();
        for event in &events {
            metrics.observe_event(event);
        }
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.execution_failures, 1);
        assert_eq!(snapshot.gauges.api_errors, 1);
    }

    #[test]
    fn shutdown_request_projects_as_interrupted_not_abort() {
        let mut projection = JournalEventProjector::new(ExecutionMode::Live);
        let events = projection.project(&JournalRecord::FinalReport {
            completed: false,
            filled_total: "0".into(),
            outcome_unknown_cloids: vec![],
            note: "shutdown requested; stopped before slice 2/3".into(),
            whole_run: None,
        });
        assert!(events.iter().any(|event| matches!(
            event,
            ExecutionEventPayload::RunStopped {
                reason: StopReason::Interrupted,
            }
        )));
        assert!(!events.iter().any(|event| matches!(
            event,
            ExecutionEventPayload::RunStopped {
                reason: StopReason::Abort,
            }
        )));
    }

    #[test]
    fn deferred_position_phase_does_not_emit_a_premature_run_terminal() {
        let mut projection = JournalEventProjector::new(ExecutionMode::Live);
        let first_phase_cloid = Cloid::from_uuid(Uuid::now_v7());
        projection.project(&JournalRecord::Terminal {
            slice_idx: 1,
            cloid: first_phase_cloid,
            status: "filled".into(),
            filled_sz: "1".into(),
            avg_px: Some("50".into()),
        });
        let events = projection.project(&JournalRecord::FinalReport {
            completed: false,
            filled_total: "1".into(),
            outcome_unknown_cloids: vec![],
            note: "phase completed; final position verification deferred".into(),
            whole_run: Some(crate::journal::WholeRunSummary {
                requested_total: None,
                adjusted_total: None,
                accounted_notional: "50".into(),
                cap_remaining: Some("950".into()),
                trusted_vwap: Some("50".into()),
                logical_elapsed_ms: 1,
                unresolved_cloids: 0,
            }),
        });
        assert_eq!(
            events,
            vec![ExecutionEventPayload::CapRemaining {
                remaining_notional: Decimal::from(950)
            }]
        );
        assert!(!events.iter().any(|event| matches!(
            event,
            ExecutionEventPayload::RunStopped { .. } | ExecutionEventPayload::FinalReport { .. }
        )));

        let second_phase_cloid = Cloid::from_uuid(Uuid::now_v7());
        projection.project(&JournalRecord::Terminal {
            slice_idx: 1,
            cloid: second_phase_cloid,
            status: "filled".into(),
            filled_sz: "1".into(),
            avg_px: Some("50".into()),
        });
        let finished = projection.project(&JournalRecord::FinalReport {
            completed: true,
            filled_total: "2".into(),
            outcome_unknown_cloids: vec![],
            note: "completed".into(),
            whole_run: None,
        });
        assert!(finished.iter().any(|event| matches!(
            event,
            ExecutionEventPayload::FinalReport {
                outcome: RunOutcome::Completed,
                completed_slices: 2,
            }
        )));
    }

    #[test]
    fn durable_fill_projects_cap_remaining_and_one_cap_near_event() {
        let cloid = Cloid::from_uuid(Uuid::nil());
        let mut projection = JournalEventProjector::new(ExecutionMode::Live);
        projection.max_notional = Some(Decimal::from(100));
        projection.project(&JournalRecord::Prepared {
            slice_idx: 1,
            cloid,
            nonce: Some(1),
            symbol: Symbol::from("BTC"),
            side: Side::Long,
            tif: None,
            px: "100".into(),
            sz: "1".into(),
        });
        let terminal = JournalRecord::Terminal {
            slice_idx: 1,
            cloid,
            status: "filled".into(),
            filled_sz: "0.95".into(),
            avg_px: None,
        };
        let first = projection.project(&terminal);
        assert!(first.iter().any(|event| matches!(
            event,
            ExecutionEventPayload::Fill {
                filled_size,
                filled_notional,
                ..
            } if *filled_size == Decimal::new(95, 2)
                && *filled_notional == Decimal::from(95)
        )));
        assert!(first.iter().any(|event| matches!(
            event,
            ExecutionEventPayload::CapRemaining { remaining_notional }
                if *remaining_notional == Decimal::from(5)
        )));
        assert_eq!(
            first
                .iter()
                .filter(|event| matches!(event, ExecutionEventPayload::CapNear { .. }))
                .count(),
            1
        );
        let repeated = projection.project(&terminal);
        assert!(!repeated
            .iter()
            .any(|event| matches!(event, ExecutionEventPayload::CapNear { .. })));
    }

    #[test]
    fn reconciliation_failure_is_not_mislabeled_as_completed() {
        let mut projection = JournalEventProjector::new(ExecutionMode::Live);
        let events = projection.project(&JournalRecord::FinalReport {
            completed: false,
            filled_total: "0".into(),
            outcome_unknown_cloids: vec![Cloid::from_uuid(Uuid::nil())],
            note: "resume reconciliation failed before completion".into(),
            whole_run: None,
        });
        assert!(matches!(
            events.first(),
            Some(ExecutionEventPayload::Reconciliation {
                outcome: ReconciliationOutcome::Failed,
                unresolved_orders: 1
            })
        ));
    }

    #[test]
    fn sidecar_sequence_continues_when_resume_reopens_the_same_file() {
        let path = std::env::temp_dir().join(format!("hype-twap-events-{}.jsonl", Uuid::now_v7()));
        let metrics = Arc::new(MetricsRegistry::default());
        let mut first = open_event_log(&path, Arc::clone(&metrics)).unwrap();
        first.try_emit(event()).unwrap();
        drop(first);
        let mut resumed = open_event_log(&path, metrics).unwrap();
        resumed.try_emit(ExecutionEventPayload::RunResumed).unwrap();
        drop(resumed);
        let events: Vec<ExecutionEvent> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn sidecar_resume_separates_a_torn_tail_and_continues_from_valid_sequence() {
        let path =
            std::env::temp_dir().join(format!("hype-twap-events-torn-{}.jsonl", Uuid::now_v7()));
        let first = serde_json::to_string(&ExecutionEvent::new(7, event())).unwrap();
        // Simulate a crash in the middle of the next JSON object: no trailing
        // newline and deliberately invalid JSON at EOF.
        std::fs::write(&path, format!("{first}\n{{\"schema_version\":1")).unwrap();

        let metrics = Arc::new(MetricsRegistry::default());
        let mut resumed = open_event_log(&path, metrics).unwrap();
        let emitted = resumed.try_emit(ExecutionEventPayload::RunResumed).unwrap();
        assert_eq!(emitted.sequence, 8);
        drop(resumed);

        let lines: Vec<String> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(
            lines.len(),
            3,
            "the resumed event must not join the torn tail"
        );
        assert_eq!(
            serde_json::from_str::<ExecutionEvent>(&lines[0])
                .unwrap()
                .sequence,
            7
        );
        assert!(serde_json::from_str::<ExecutionEvent>(&lines[1]).is_err());
        let resumed_event: ExecutionEvent = serde_json::from_str(&lines[2]).unwrap();
        assert_eq!(resumed_event.sequence, 8);
        assert_eq!(resumed_event.payload, ExecutionEventPayload::RunResumed);
        std::fs::remove_file(path).unwrap();
    }
}
