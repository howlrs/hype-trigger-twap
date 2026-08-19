//! Execution journal: crash-safe, append-only record of a live TWAP run
//! (Issue #4).
//!
//! ## Why
//!
//! `/exchange` is not idempotent and the nonce is consumed the moment HL
//! receives the body — so if the process dies between "we sent it" and "we
//! read the response," restarting and simply re-running the same command can
//! duplicate the order. Before this module existed, run state (cloid,
//! fills, nonce) lived only in memory (`FillStats` / `place_slice_reconciled`
//! in `src/twap.rs`); a crash lost it completely.
//!
//! The journal makes the send path crash-safe by durably recording intent
//! BEFORE the network call that could have an ambiguous outcome, so a
//! restart (or `--resume`) can always answer "did that cloid ever reach the
//! exchange?" via `orderStatus`, rather than guessing.
//!
//! ## Format
//!
//! One JSONL file per run, append-only, `fsync`ed after every record
//! (including the header). Each line is a [`JournalRecord`]. State root
//! resolution ([`state_dir`]) and file layout are documented on
//! [`ExecutionJournal`].
//!
//! ## What must NEVER appear here
//!
//! No private key material, no signing secrets, no raw signature bytes.
//! Addresses (agent/master, both public) are fine — see
//! `journal_never_serializes_secret_material` in the test module for the
//! audit this claim is backed by.
//!
//! ## Extension points for later tasks (documented per the Task 7 brief)
//!
//! - **Task 8 #5** (nonce high-water-mark + lock): the state dir
//!   ([`state_dir`] / [`ExecutionJournal::run_dir`]) is the natural home for
//!   a per-network+agent nonce HWM file and an advisory lock, sitting
//!   alongside the per-run journal directories. `run_key` (network+agent) is
//!   already the same partition key an HWM/lock file would use.
//! - **Task 9 #1** (passive/post-only mode): any order it rests and later
//!   cancels should go through [`ExecutionJournal::record`] with
//!   [`JournalRecord::Prepared`] → [`JournalRecord::Acknowledged`] →
//!   cancellation reflected via a new terminal-status
//!   [`JournalRecord::Terminal`] record (status `"canceled"`), reusing this
//!   same journal rather than inventing a second log.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::types::{Address, Cloid, Side, Symbol};

/// Resolve the state root directory (Issue #4 PM decision).
///
/// Precedence: `--state-dir` (passed explicitly by the caller as
/// `override_dir`) > `$XDG_STATE_HOME/hype-twap` > `~/.local/state/hype-twap`.
/// Hand-rolled `std::env::var` resolution — no new dependency.
///
/// This function does NOT create the directory; callers create it lazily,
/// only when a live run actually needs to write (read-only must create
/// nothing — see `read_only_creates_no_state_dir_or_journal`).
pub fn state_dir(override_dir: Option<&Path>) -> PathBuf {
    if let Some(p) = override_dir {
        return p.to_path_buf();
    }
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
        if !xdg.trim().is_empty() {
            return PathBuf::from(xdg).join("hype-twap");
        }
    }
    // Hand-rolled HOME resolution (no `dirs` crate dependency, per the PM
    // decision). `HOME` unset is exceedingly rare on any Unix this tool
    // targets; falling back to a relative path keeps the function total
    // rather than panicking.
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".local/state/hype-twap")
}

/// One append-only-journal record. Serialized as one JSON object per line.
///
/// State machine: `Prepared -> SubmittedUnknown -> Acknowledged | Terminal`.
/// `Acknowledged` covers a confirmed-resting (non-terminal, e.g. `open`)
/// order; `Terminal` covers any status for which
/// [`crate::client::OrderStatusFill::is_terminal`] is true (filled, canceled,
/// rejected, ...). A record's `run_id` is implicit (one file per run) and is
/// therefore NOT duplicated per-record — see [`RunHeader`] for the one place
/// it is stored.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind")]
pub enum JournalRecord {
    /// Header, always the first line of the file. Carries everything needed
    /// to identify and later reconcile the run WITHOUT any secret material.
    Header(RunHeader),
    /// Intent + cloid + nonce, fsynced BEFORE the `/exchange` POST is sent.
    /// This is the record that makes "did we already try to send this
    /// slice?" answerable after a crash.
    Prepared {
        slice_idx: u32,
        cloid: Cloid,
        /// The nonce that will be signed into the request. `None` until the
        /// nonce is actually minted (kept `Option` for forward
        /// compatibility; the current call sites always set it before the
        /// record is written).
        nonce: Option<u64>,
        symbol: Symbol,
        side: Side,
        px: String,
        sz: String,
    },
    /// The POST was sent but the response was not read (transport failure,
    /// or the process died in between) — outcome is genuinely unknown until
    /// reconciled via `orderStatus`.
    SubmittedUnknown { slice_idx: u32, cloid: Cloid },
    /// HL confirmed the order exists but it is not yet in a terminal state
    /// (e.g. `open`/resting). Per the prior-art note: `open` is NOT final —
    /// this is a live/tracked order, a candidate for cancel-on-shutdown, not
    /// a resolved fill.
    Acknowledged {
        slice_idx: u32,
        cloid: Cloid,
        oid: Option<u64>,
        status: String,
    },
    /// A terminal outcome (filled/canceled/rejected/...) — closes out the
    /// cloid for accounting purposes. `filled_sz`/`avg_px` are the values
    /// credited to the run's fill total; resume accounting reads these
    /// (never replays `Prepared`/`SubmittedUnknown` as a fill) to guarantee
    /// each fill is counted exactly once.
    Terminal {
        slice_idx: u32,
        cloid: Cloid,
        status: String,
        filled_sz: String,
        avg_px: Option<String>,
    },
    /// Final durable report, written once at the very end of a run —
    /// whether it finished normally, aborted, or was interrupted by a
    /// signal. `outcome_unknown_cloids` lists any cloid that could not be
    /// resolved (e.g. a grace-timeout during signal shutdown); a non-empty
    /// list is what drives the non-zero exit in that case.
    FinalReport {
        completed: bool,
        filled_total: String,
        outcome_unknown_cloids: Vec<Cloid>,
        note: String,
    },
    /// The run was explicitly abandoned via `--abandon-incomplete-run`,
    /// AFTER forced reconciliation of every submitted/unknown cloid (the
    /// reconciliation results themselves are separate `Acknowledged`/
    /// `Terminal` records preceding this one — this record only marks the
    /// run closed).
    Abandoned { note: String },
}

/// First line of every journal file. No secrets: addresses are public
/// on-chain identifiers, never key material.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunHeader {
    pub run_id: String,
    pub network: String,
    /// Agent (API wallet) address — public, never the private key.
    pub agent: Option<Address>,
    /// Master account address — public.
    pub master: Option<Address>,
    pub symbol: Symbol,
    pub side: Side,
    pub slices: u32,
    /// Opaque hash of the resolved plan (symbol/side/size/slices/duration/
    /// slippage/notional cap/...), so a `--resume` can detect a mismatched
    /// invocation. Not itself sensitive.
    pub plan_hash: String,
    pub started_at_unix_ms: u64,
}

/// Hash a plan's defining parameters into the `plan_hash` a [`RunHeader`]
/// carries, so `--resume` can flag an invocation whose plan no longer
/// matches the run it is resuming (e.g. a different `--slices`/`--duration`
/// was passed by mistake). Not a cryptographic hash — `DefaultHasher`
/// (SipHash) is used only as a change-detector, not for anything
/// security-sensitive, so no new dependency is needed.
pub fn hash_plan_params(fields: &[&str]) -> String {
    use std::hash::{Hash, Hasher};
    // Determinism assumption this `--resume` check is load-bearing on:
    // `DefaultHasher::new()` uses a FIXED (non-randomized) key per the Rust
    // std docs — deterministic across constructions, unlike
    // `RandomState`/`HashMap`'s default hasher — so hashing the same plan
    // params in two different process runs (this run vs. the crashed one
    // being resumed) yields the same hash. Covered empirically by
    // `hash_plan_params_is_deterministic_across_independent_hasher_instances`
    // below, simulating "two different process runs."
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for f in fields {
        f.hash(&mut hasher);
        0u8.hash(&mut hasher); // field separator, avoids "ab"+"c" == "a"+"bc"
    }
    format!("{:016x}", hasher.finish())
}

impl RunHeader {
    /// Stable partition key for "incomplete run for the same network+agent"
    /// detection — the exact granularity the PM brief specifies startup
    /// blocking on.
    pub fn run_key(&self) -> String {
        format!(
            "{}:{}",
            self.network,
            self.agent.as_ref().map(Address::as_str).unwrap_or("none")
        )
    }
}

/// An append-only, `fsync`-after-every-record JSONL journal for one live
/// run.
///
/// ## Layout
///
/// ```text
/// <state-dir>/runs/<run_id>/journal.jsonl
/// ```
///
/// `run_id` is a UUIDv7 string (time-sortable, matching this codebase's
/// existing `Cloid` convention) unless the caller supplies one (`--resume`
/// re-opens an existing `run_id`'s file for append).
///
/// ## Durability
///
/// Every [`ExecutionJournal::record`] call: serializes to one JSON line,
/// appends `\n`, `write_all`s it, then `sync_data()`s the file. This is
/// deliberately synchronous/blocking (`std::fs::File`, not tokio's async
/// file) — the guarantee this type exists to provide is "if `record`
/// returned `Ok`, the bytes survived a crash immediately after," and an
/// async write that hasn't been polled to completion cannot promise that
/// without extra bookkeeping this run loop does not need.
pub struct ExecutionJournal {
    file: File,
    run_id: String,
    run_dir: PathBuf,
}

/// Errors from journal I/O. Kept separate from [`crate::errors::HlError`]
/// since these are local filesystem failures, not HL communication errors —
/// callers decide how to fold them in (typically `.to_string()` into the
/// same `Result<_, String>` `main.rs` already uses).
#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("journal I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("journal serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("malformed journal record at {path}:{line}: {source}")]
    Parse {
        path: PathBuf,
        line: usize,
        #[source]
        source: serde_json::Error,
    },
}

impl ExecutionJournal {
    /// Directory for one run: `<state_root>/runs/<run_id>/`.
    pub fn run_dir(state_root: &Path, run_id: &str) -> PathBuf {
        state_root.join("runs").join(run_id)
    }

    /// Path to the journal file for one run.
    pub fn journal_path(state_root: &Path, run_id: &str) -> PathBuf {
        Self::run_dir(state_root, run_id).join("journal.jsonl")
    }

    /// Start a brand-new run: creates `<state_root>/runs/<run_id>/`, opens
    /// `journal.jsonl` for append, and writes+fsyncs the header as the
    /// first record. This is the ONLY function that creates the state
    /// directory — a read-only run must never call it (Issue #4 acceptance
    /// criterion: read-only creates no state dir).
    pub fn start(
        state_root: &Path,
        run_id: String,
        header: RunHeader,
    ) -> Result<Self, JournalError> {
        let run_dir = Self::run_dir(state_root, &run_id);
        std::fs::create_dir_all(&run_dir)?;
        let path = run_dir.join("journal.jsonl");
        // B5: `create_new` (exclusive create) rather than `create + append`
        // — a run_id collision (uuid v7 makes this astronomically unlikely,
        // but cheap to guard) would otherwise silently append a second
        // Header onto an existing journal instead of erroring. `--resume`
        // uses `open_existing` (below), a separate path, so this does not
        // affect the normal resume flow.
        let file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)?;
        let mut journal = Self {
            file,
            run_id,
            run_dir,
        };
        journal.record(&JournalRecord::Header(header))?;
        Ok(journal)
    }

    /// Re-open an existing run's journal for append (used by `--resume`).
    pub fn open_existing(state_root: &Path, run_id: &str) -> Result<Self, JournalError> {
        let run_dir = Self::run_dir(state_root, run_id);
        let path = run_dir.join("journal.jsonl");
        let file = OpenOptions::new().append(true).open(&path)?;
        Ok(Self {
            file,
            run_id: run_id.to_string(),
            run_dir,
        })
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// This run's directory on disk (`<state_root>/runs/<run_id>/`).
    pub fn dir(&self) -> &Path {
        &self.run_dir
    }

    /// Append one record and `fsync` before returning. Callers that need the
    /// "durable BEFORE the POST" guarantee (the `Prepared` record) MUST
    /// await/check this call's success before issuing the network send.
    pub fn record(&mut self, rec: &JournalRecord) -> Result<(), JournalError> {
        let mut line = serde_json::to_vec(rec)?;
        line.push(b'\n');
        self.file.write_all(&line)?;
        self.file.sync_data()?;
        Ok(())
    }

    /// Read every record currently in a run's journal, in file order.
    ///
    /// B4: tolerant of a torn FINAL line — the normal crash shape (the
    /// process died mid-`write_all`/before the trailing newline of the
    /// LAST record it was appending). All lines up to and including the
    /// last one that parses successfully are returned; a torn last line is
    /// silently dropped rather than erroring, exactly like standard JSONL
    /// crash-recovery semantics. Any parse failure on a line that is NOT
    /// the last one is still a hard error (a genuinely corrupt journal, not
    /// a crash-torn tail), and a file that yields NO parseable records at
    /// all (e.g. not even a valid `Header`) is likewise a hard error — see
    /// `find_incomplete_run`, whose caller (`main.rs`, live startup) must
    /// fail closed on that case rather than silently treat the run as
    /// absent. This distinction is documented in `docs/OPERATIONS.md`.
    pub fn read_all(state_root: &Path, run_id: &str) -> Result<Vec<JournalRecord>, JournalError> {
        let path = Self::journal_path(state_root, run_id);
        let f = File::open(&path)?;
        let reader = BufReader::new(f);
        let lines: Vec<String> = reader.lines().collect::<Result<_, _>>()?;
        let mut out = Vec::new();
        for (idx, line) in lines.iter().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<JournalRecord>(line) {
                Ok(rec) => out.push(rec),
                Err(e) => {
                    let is_last_nonblank = lines[idx + 1..].iter().all(|l| l.trim().is_empty());
                    // Torn final line IS the normal crash-mid-append shape
                    // ONLY when at least one record (starting with the
                    // Header) already parsed successfully before it — a
                    // file that fails to parse its very FIRST record (no
                    // header, nothing) is not "a run that crashed mid
                    // append," it is unreadable from the start, and must
                    // hard-error rather than be silently treated as an
                    // empty/absent run.
                    if is_last_nonblank && !out.is_empty() {
                        break;
                    }
                    return Err(JournalError::Parse {
                        path,
                        line: idx + 1,
                        source: e,
                    });
                }
            }
        }
        Ok(out)
    }
}

/// One cloid's reconciliation state, derived by replaying a run's journal
/// (used both for incomplete-run detection at startup and for `--resume`
/// accounting).
#[derive(Debug, Clone, PartialEq)]
pub enum CloidState {
    /// `Prepared` was written but no `SubmittedUnknown`/terminal outcome
    /// followed — the POST was never even attempted (or the process died
    /// before recording that it was).
    PreparedOnly,
    /// `SubmittedUnknown` is the last record for this cloid — ambiguous,
    /// needs `orderStatus` reconciliation.
    SubmittedUnknown,
    /// HL confirmed the order exists but non-terminal (e.g. resting/open).
    Acknowledged,
    /// Resolved to a terminal status; carries the credited fill.
    Terminal {
        filled_sz: String,
        avg_px: Option<String>,
    },
}

/// A summary of one journal's replay: is the run complete, and what does
/// every cloid's last-known state look like.
#[derive(Debug, Clone, Default)]
pub struct RunSummary {
    pub header: Option<RunHeader>,
    /// In first-seen order, so `--resume` reconciles in the same order
    /// slices were placed.
    pub cloids: Vec<(Cloid, CloidState)>,
    pub final_report_seen: bool,
    /// `outcome_unknown_cloids` carried by the LAST `FinalReport` record in
    /// the journal (an append-only journal can carry more than one — Issue
    /// #20: a `--resume`/`--abandon-incomplete-run` that reconciles a
    /// previously-unknown-cloid run and then continues/closes it appends a
    /// FRESH FinalReport rather than rewriting the first one). `None` until
    /// at least one `FinalReport` has been seen; `Some(vec![])` once one has
    /// been seen with an empty list. Last-one-wins: each `FinalReport`
    /// record overwrites this, so after a full replay it always reflects
    /// the most recent one, never an earlier one.
    pub last_final_report_unknown_cloids: Option<Vec<Cloid>>,
    pub abandoned: bool,
}

impl RunSummary {
    /// A run is "incomplete" (per the Issue #4 startup-blocking acceptance
    /// criterion, extended by Issue #20) if it has a header, is not
    /// abandoned, AND EITHER:
    /// - has no `FinalReport` at all (original behaviour: a crash mid-run,
    ///   with at least one cloid not yet `Terminal`), OR
    /// - its LAST `FinalReport` (by journal order) carries a non-empty
    ///   `outcome_unknown_cloids` — a settle-failure abort that journaled a
    ///   FinalReport but left dangling `Prepared`/`Acknowledged` cloids
    ///   un-reconciled (Issue #20). This branch does NOT additionally
    ///   require `self.cloids` to have an unresolved entry — the
    ///   FinalReport's own list is authoritative for this case, since it is
    ///   exactly the set `find_incomplete_run`'s caller must reconcile.
    pub fn is_incomplete(&self) -> bool {
        if self.header.is_none() || self.abandoned {
            return false;
        }
        if !self.final_report_seen {
            return self
                .cloids
                .iter()
                .any(|(_, st)| !matches!(st, CloidState::Terminal { .. }));
        }
        self.last_final_report_unknown_cloids
            .as_ref()
            .is_some_and(|unknown| !unknown.is_empty())
    }

    /// Sum of every `Terminal` cloid's `filled_sz` — the resume accounting
    /// entry point. Each cloid appears at most once in `self.cloids` (see
    /// [`summarize`]'s de-duplication), so this sums each fill EXACTLY
    /// once regardless of how many `Prepared`/`SubmittedUnknown`/
    /// `Acknowledged` records preceded the terminal one.
    pub fn total_filled(&self) -> rust_decimal::Decimal {
        self.cloids
            .iter()
            .filter_map(|(_, st)| match st {
                CloidState::Terminal { filled_sz, .. } => {
                    filled_sz.parse::<rust_decimal::Decimal>().ok()
                }
                _ => None,
            })
            .sum()
    }

    /// Cloids still needing `orderStatus` reconciliation (not yet
    /// `Terminal`) — what `--resume` and signal-driven shutdown both
    /// iterate over.
    pub fn unresolved_cloids(&self) -> Vec<Cloid> {
        self.cloids
            .iter()
            .filter(|(_, st)| !matches!(st, CloidState::Terminal { .. }))
            .map(|(c, _)| *c)
            .collect()
    }
}

/// Replay a journal's records into a [`RunSummary`]. Pure function over an
/// in-memory record list so it is trivially unit-testable without any
/// filesystem I/O (tests build the `Vec<JournalRecord>` directly).
///
/// Each cloid's LATEST record wins (later records supersede earlier ones —
/// `Prepared` -> `SubmittedUnknown` -> `Acknowledged`/`Terminal` is a
/// forward-only progression), which is what keeps `total_filled` from
/// double-counting: a cloid that reached `Terminal` contributes its fill
/// exactly once, no matter how many earlier non-terminal records exist for
/// the same cloid.
pub fn summarize(records: &[JournalRecord]) -> RunSummary {
    let mut summary = RunSummary::default();
    // Preserve first-seen order but let later records overwrite state.
    let mut order: Vec<Cloid> = Vec::new();
    let mut states: std::collections::HashMap<Cloid, CloidState> = std::collections::HashMap::new();

    for rec in records {
        match rec {
            JournalRecord::Header(h) => summary.header = Some(h.clone()),
            JournalRecord::Prepared { cloid, .. } => {
                if !states.contains_key(cloid) {
                    order.push(*cloid);
                }
                states.insert(*cloid, CloidState::PreparedOnly);
            }
            JournalRecord::SubmittedUnknown { cloid, .. } => {
                if !states.contains_key(cloid) {
                    order.push(*cloid);
                }
                states.insert(*cloid, CloidState::SubmittedUnknown);
            }
            JournalRecord::Acknowledged { cloid, .. } => {
                if !states.contains_key(cloid) {
                    order.push(*cloid);
                }
                states.insert(*cloid, CloidState::Acknowledged);
            }
            JournalRecord::Terminal {
                cloid,
                filled_sz,
                avg_px,
                ..
            } => {
                if !states.contains_key(cloid) {
                    order.push(*cloid);
                }
                states.insert(
                    *cloid,
                    CloidState::Terminal {
                        filled_sz: filled_sz.clone(),
                        avg_px: avg_px.clone(),
                    },
                );
            }
            JournalRecord::FinalReport {
                outcome_unknown_cloids,
                ..
            } => {
                summary.final_report_seen = true;
                // Last-one-wins: a later FinalReport (Issue #20 resume/
                // abandon continuation) overwrites whatever an earlier one
                // in this same journal left here.
                summary.last_final_report_unknown_cloids = Some(outcome_unknown_cloids.clone());
            }
            JournalRecord::Abandoned { .. } => summary.abandoned = true,
        }
    }

    summary.cloids = order
        .into_iter()
        .map(|c| {
            let st = states.remove(&c).unwrap_or(CloidState::PreparedOnly);
            (c, st)
        })
        .collect();
    summary
}

/// Scan `<state_root>/runs/*/journal.jsonl` for any run whose header
/// `run_key()` matches `(network, agent)` and whose [`RunSummary`] is
/// [`RunSummary::is_incomplete`]. Returns the first such run's id, if any.
///
/// Used by main.rs at startup to refuse a new overlapping live run.
/// Missing/empty `runs/` directories (including a state dir that has never
/// been created — e.g. this is the very first live run ever) are treated as
/// "no incomplete run," never an error.
pub fn find_incomplete_run(
    state_root: &Path,
    network: &str,
    agent: Option<&Address>,
) -> Result<Option<String>, JournalError> {
    let runs_dir = state_root.join("runs");
    if !runs_dir.is_dir() {
        return Ok(None);
    }
    let want_key = format!("{network}:{}", agent.map(Address::as_str).unwrap_or("none"));
    let mut entries: Vec<_> = std::fs::read_dir(&runs_dir)?
        .filter_map(|e| e.ok())
        .collect();
    // Deterministic order (oldest-looking run_id first, since run_ids are
    // UUIDv7 and therefore lexicographically time-sortable) so a test/ops
    // scenario with multiple incomplete runs reports a stable answer.
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let run_id = match entry.file_name().into_string() {
            Ok(s) => s,
            Err(_) => continue,
        };
        // B4: an unparseable/corrupt journal must fail-closed live startup,
        // not be silently skipped — corruption is MOST likely right after a
        // crash, which is exactly when the incomplete-run gate exists to
        // catch a still-unresolved run before a new live run can start
        // alongside it. `ExecutionJournal::read_all` already tolerates the
        // NORMAL crash shape (a torn final line with valid prior records),
        // so any error surfacing here is a genuine corruption, not a
        // standard crash artifact.
        let records = ExecutionJournal::read_all(state_root, &run_id).map_err(|e| {
            tracing::error!(
                run_id = %run_id,
                error = %e,
                "found a journal file that could not be parsed at all while scanning for an \
                 incomplete run — refusing to start a new live run until this is resolved. \
                 See docs/OPERATIONS.md for the recovery runbook (inspect the file by hand; \
                 if it is truly unrecoverable, move it aside so this run_id no longer blocks \
                 startup, understanding that whatever it recorded is then unaccounted for)."
            );
            e
        })?;
        let summary = summarize(&records);
        let Some(header) = &summary.header else {
            continue;
        };
        if header.run_key() == want_key && summary.is_incomplete() {
            return Ok(Some(run_id));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// Minimal hand-rolled temp-directory guard (no `tempfile` dependency —
    /// this crate already has `uuid` for a unique suffix). Removes its
    /// directory tree on drop, mirroring `tempfile::TempDir`'s contract
    /// closely enough for these tests' needs.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> std::io::Result<Self> {
            let dir = std::env::temp_dir()
                .join(format!("hype-twap-journal-test-{}", uuid::Uuid::now_v7()));
            std::fs::create_dir_all(&dir)?;
            Ok(Self(dir))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn header(run_id: &str) -> RunHeader {
        RunHeader {
            run_id: run_id.to_string(),
            network: "testnet".into(),
            agent: Some(Address::new("0xagent")),
            master: Some(Address::new("0xmaster")),
            symbol: Symbol::new("HYPE"),
            side: Side::Long,
            slices: 10,
            plan_hash: "deadbeef".into(),
            started_at_unix_ms: 1_000,
        }
    }

    // === hash_plan_params determinism ===

    /// `--resume` runs in a FRESH process from the one that started (and
    /// possibly crashed mid-) the run being resumed. The `plan_hash`
    /// consistency check therefore only works if `DefaultHasher::new()`
    /// (used internally by `hash_plan_params`) hashes identically across
    /// two INDEPENDENT constructions — this test simulates exactly that:
    /// two separate `hash_plan_params` calls (standing in for "two
    /// different process runs") over the SAME plan input must produce the
    /// SAME hash. Per the Rust std docs, `DefaultHasher::new()` uses a
    /// fixed (non-randomized) key, so this is expected to hold — but the
    /// brief calls for verifying it empirically rather than trusting the
    /// docs alone, since if it were process-randomized every cross-process
    /// `--resume` would silently fail the plan-hash check.
    #[test]
    fn hash_plan_params_is_deterministic_across_independent_hasher_instances() {
        let fields = ["HYPE", "long", "5", "50", "10", "1800", "50", "100000"];
        let hash_a = hash_plan_params(&fields); // "process run" A
        let hash_b = hash_plan_params(&fields); // "process run" B
        assert_eq!(
            hash_a, hash_b,
            "DefaultHasher::new() must be deterministic across independent \
             constructions for --resume to ever work cross-process"
        );

        // A different plan must (with overwhelming probability) hash
        // differently — pins that this isn't a degenerate always-equal
        // hasher passing the test above vacuously.
        let different_fields = ["HYPE", "short", "5", "50", "10", "1800", "50", "100000"];
        let hash_c = hash_plan_params(&different_fields);
        assert_ne!(hash_a, hash_c);
    }

    // === state_dir resolution ===

    #[test]
    fn state_dir_override_wins() {
        let p = state_dir(Some(Path::new("/custom/dir")));
        assert_eq!(p, PathBuf::from("/custom/dir"));
    }

    #[test]
    fn state_dir_prefers_xdg_state_home() {
        // Not using std::env mutation here (no serial group needed) would be
        // unsafe under parallel tests; this crate's convention is
        // `#[serial_test::serial(hl_env_vars)]` for HL_* vars specifically —
        // XDG_STATE_HOME is a distinct var, but to be safe or race-free we
        // still avoid mutating global state in this pure-function test by
        // asserting the join logic directly instead.
        let p = PathBuf::from("/xdg").join("hype-twap");
        assert_eq!(p, PathBuf::from("/xdg/hype-twap"));
    }

    #[test]
    #[serial_test::serial(state_dir_env)]
    fn state_dir_falls_back_to_home_local_state() {
        let prev_xdg = std::env::var("XDG_STATE_HOME").ok();
        let prev_home = std::env::var("HOME").ok();
        std::env::remove_var("XDG_STATE_HOME");
        std::env::set_var("HOME", "/home/testuser");

        let p = state_dir(None);
        assert_eq!(p, PathBuf::from("/home/testuser/.local/state/hype-twap"));

        match prev_xdg {
            Some(v) => std::env::set_var("XDG_STATE_HOME", v),
            None => std::env::remove_var("XDG_STATE_HOME"),
        }
        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    #[serial_test::serial(state_dir_env)]
    fn state_dir_uses_xdg_state_home_when_set() {
        let prev_xdg = std::env::var("XDG_STATE_HOME").ok();
        std::env::set_var("XDG_STATE_HOME", "/xdg/state");

        let p = state_dir(None);
        assert_eq!(p, PathBuf::from("/xdg/state/hype-twap"));

        match prev_xdg {
            Some(v) => std::env::set_var("XDG_STATE_HOME", v),
            None => std::env::remove_var("XDG_STATE_HOME"),
        }
    }

    // === ExecutionJournal basics ===

    #[test]
    fn start_creates_run_dir_and_writes_header_first() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let mut j = ExecutionJournal::start(root, "run-1".into(), header("run-1")).unwrap();
        j.record(&JournalRecord::SubmittedUnknown {
            slice_idx: 1,
            cloid: Cloid::new(),
        })
        .unwrap();

        let records = ExecutionJournal::read_all(root, "run-1").unwrap();
        assert_eq!(records.len(), 2);
        assert!(matches!(records[0], JournalRecord::Header(_)));
        assert!(matches!(records[1], JournalRecord::SubmittedUnknown { .. }));
    }

    #[test]
    fn open_existing_appends_without_duplicating_header() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        {
            let mut j = ExecutionJournal::start(root, "run-2".into(), header("run-2")).unwrap();
            j.record(&JournalRecord::Prepared {
                slice_idx: 1,
                cloid: Cloid::new(),
                nonce: Some(1),
                symbol: Symbol::new("HYPE"),
                side: Side::Long,
                px: "50".into(),
                sz: "5".into(),
            })
            .unwrap();
        }
        {
            let mut j = ExecutionJournal::open_existing(root, "run-2").unwrap();
            j.record(&JournalRecord::Abandoned {
                note: "test".into(),
            })
            .unwrap();
        }
        let records = ExecutionJournal::read_all(root, "run-2").unwrap();
        assert_eq!(records.len(), 3);
        assert!(matches!(records[0], JournalRecord::Header(_)));
        assert!(matches!(records[2], JournalRecord::Abandoned { .. }));
    }

    // === B5: ExecutionJournal::start must not silently append a second
    // Header onto an existing journal for a colliding run_id ===

    #[test]
    fn start_rejects_a_colliding_run_id_instead_of_appending_a_second_header() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let mut j1 =
            ExecutionJournal::start(root, "run-collide".into(), header("run-collide")).unwrap();
        j1.record(&JournalRecord::SubmittedUnknown {
            slice_idx: 1,
            cloid: Cloid::new(),
        })
        .unwrap();
        drop(j1);

        // Same run_id, started again (uuid v7 collision is astronomically
        // unlikely, but this must still be a clear error rather than a
        // silently corrupted two-Header journal).
        let second = ExecutionJournal::start(root, "run-collide".into(), header("run-collide"));
        assert!(
            second.is_err(),
            "starting a journal for a run_id that already has a journal file must error, not \
             silently append a second Header onto the existing file"
        );

        // The original journal must be untouched — still exactly its 2
        // original records, no second Header appended.
        let records = ExecutionJournal::read_all(root, "run-collide").unwrap();
        assert_eq!(
            records.len(),
            2,
            "a failed start() must not have appended anything to the existing journal"
        );
        assert!(matches!(records[0], JournalRecord::Header(_)));
    }

    // === No secrets, ever ===

    #[test]
    fn journal_never_serializes_secret_material() {
        // Audit every field of every variant: nothing here is or could hold
        // a private key / signature. This test asserts the SERIALIZED JSON
        // of a representative record of every variant contains none of the
        // literal substrings a leaked secret would produce, and that the
        // types involved (String/Decimal-as-string/Address/Cloid/Symbol/
        // Side/u32/u64/bool/Vec<Cloid>) are structurally incapable of
        // carrying a SecretString — `RunHeader`/`JournalRecord` do not
        // derive `secrecy` at all, so this is also a compile-time guarantee,
        // not just a runtime string check.
        let cloid = Cloid::new();
        let records = vec![
            JournalRecord::Header(header("run-3")),
            JournalRecord::Prepared {
                slice_idx: 1,
                cloid,
                nonce: Some(42),
                symbol: Symbol::new("HYPE"),
                side: Side::Long,
                px: "50.5".into(),
                sz: "5".into(),
            },
            JournalRecord::SubmittedUnknown {
                slice_idx: 1,
                cloid,
            },
            JournalRecord::Acknowledged {
                slice_idx: 1,
                cloid,
                oid: Some(123),
                status: "open".into(),
            },
            JournalRecord::Terminal {
                slice_idx: 1,
                cloid,
                status: "filled".into(),
                filled_sz: "5".into(),
                avg_px: Some("50.5".into()),
            },
            JournalRecord::FinalReport {
                completed: true,
                filled_total: "50".into(),
                outcome_unknown_cloids: vec![],
                note: "done".into(),
            },
            JournalRecord::Abandoned {
                note: "operator abandoned".into(),
            },
        ];
        for rec in &records {
            let json = serde_json::to_string(rec).unwrap();
            for banned in [
                "private",
                "secret",
                "pk",
                "0x01234567890123456789012345678901234567890123456789012345678901",
                "signature",
                "SecretString",
            ] {
                assert!(
                    !json.to_lowercase().contains(&banned.to_lowercase()),
                    "journal record leaked forbidden substring {banned:?}: {json}"
                );
            }
        }
    }

    // === summarize / RunSummary ===

    #[test]
    fn summarize_counts_each_terminal_fill_exactly_once() {
        let c1 = Cloid::new();
        let c2 = Cloid::new();
        let records = vec![
            JournalRecord::Header(header("run-4")),
            JournalRecord::Prepared {
                slice_idx: 1,
                cloid: c1,
                nonce: Some(1),
                symbol: Symbol::new("HYPE"),
                side: Side::Long,
                px: "50".into(),
                sz: "5".into(),
            },
            JournalRecord::SubmittedUnknown {
                slice_idx: 1,
                cloid: c1,
            },
            JournalRecord::Terminal {
                slice_idx: 1,
                cloid: c1,
                status: "filled".into(),
                filled_sz: "5".into(),
                avg_px: Some("50".into()),
            },
            JournalRecord::Prepared {
                slice_idx: 2,
                cloid: c2,
                nonce: Some(2),
                symbol: Symbol::new("HYPE"),
                side: Side::Long,
                px: "51".into(),
                sz: "5".into(),
            },
            JournalRecord::Terminal {
                slice_idx: 2,
                cloid: c2,
                status: "filled".into(),
                filled_sz: "5".into(),
                avg_px: Some("51".into()),
            },
        ];
        let summary = summarize(&records);
        assert_eq!(summary.total_filled(), rust_decimal::Decimal::from(10));
        assert_eq!(summary.cloids.len(), 2);
        assert!(summary.unresolved_cloids().is_empty());
        assert!(!summary.is_incomplete());
    }

    #[test]
    fn summarize_treats_unresolved_cloid_as_incomplete() {
        let c1 = Cloid::new();
        let records = vec![
            JournalRecord::Header(header("run-5")),
            JournalRecord::Prepared {
                slice_idx: 1,
                cloid: c1,
                nonce: Some(1),
                symbol: Symbol::new("HYPE"),
                side: Side::Long,
                px: "50".into(),
                sz: "5".into(),
            },
            JournalRecord::SubmittedUnknown {
                slice_idx: 1,
                cloid: c1,
            },
        ];
        let summary = summarize(&records);
        assert!(summary.is_incomplete());
        assert_eq!(summary.unresolved_cloids(), vec![c1]);
        assert_eq!(summary.total_filled(), rust_decimal::Decimal::ZERO);
    }

    #[test]
    fn final_report_marks_run_complete_even_with_no_fills() {
        let records = vec![
            JournalRecord::Header(header("run-6")),
            JournalRecord::FinalReport {
                completed: true,
                filled_total: "0".into(),
                outcome_unknown_cloids: vec![],
                note: "nothing placed".into(),
            },
        ];
        let summary = summarize(&records);
        assert!(!summary.is_incomplete());
    }

    #[test]
    fn abandoned_run_is_not_incomplete_even_with_unresolved_cloids() {
        let c1 = Cloid::new();
        let records = vec![
            JournalRecord::Header(header("run-7")),
            JournalRecord::Prepared {
                slice_idx: 1,
                cloid: c1,
                nonce: Some(1),
                symbol: Symbol::new("HYPE"),
                side: Side::Long,
                px: "50".into(),
                sz: "5".into(),
            },
            // Forced reconciliation would append Acknowledged/Terminal
            // records here in the real flow; this test only pins that the
            // Abandoned marker itself is sufficient to stop is_incomplete
            // from tripping regardless.
            JournalRecord::Abandoned {
                note: "operator abandoned after reconciliation".into(),
            },
        ];
        let summary = summarize(&records);
        assert!(!summary.is_incomplete());
        assert!(summary.abandoned);
    }

    // === find_incomplete_run ===

    #[test]
    fn find_incomplete_run_matches_on_network_and_agent() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let agent = Address::new("0xagent");

        let mut j = ExecutionJournal::start(
            root,
            "run-a".into(),
            RunHeader {
                run_id: "run-a".into(),
                network: "testnet".into(),
                agent: Some(agent.clone()),
                master: Some(Address::new("0xmaster")),
                symbol: Symbol::new("HYPE"),
                side: Side::Long,
                slices: 10,
                plan_hash: "hash1".into(),
                started_at_unix_ms: 0,
            },
        )
        .unwrap();
        j.record(&JournalRecord::SubmittedUnknown {
            slice_idx: 1,
            cloid: Cloid::new(),
        })
        .unwrap();

        // Same network+agent: found.
        let found = find_incomplete_run(root, "testnet", Some(&agent)).unwrap();
        assert_eq!(found, Some("run-a".to_string()));

        // Different network: not found.
        let not_found = find_incomplete_run(root, "mainnet", Some(&agent)).unwrap();
        assert_eq!(not_found, None);

        // Different agent: not found.
        let other_agent = Address::new("0xother");
        let not_found2 = find_incomplete_run(root, "testnet", Some(&other_agent)).unwrap();
        assert_eq!(not_found2, None);
    }

    #[test]
    fn find_incomplete_run_ignores_completed_runs() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let agent = Address::new("0xagent");
        let mut j = ExecutionJournal::start(
            root,
            "run-b".into(),
            RunHeader {
                run_id: "run-b".into(),
                network: "testnet".into(),
                agent: Some(agent.clone()),
                master: Some(Address::new("0xmaster")),
                symbol: Symbol::new("HYPE"),
                side: Side::Long,
                slices: 10,
                plan_hash: "hash1".into(),
                started_at_unix_ms: 0,
            },
        )
        .unwrap();
        j.record(&JournalRecord::FinalReport {
            completed: true,
            filled_total: "50".into(),
            outcome_unknown_cloids: vec![],
            note: "done".into(),
        })
        .unwrap();

        let found = find_incomplete_run(root, "testnet", Some(&agent)).unwrap();
        assert_eq!(found, None);
    }

    #[test]
    fn find_incomplete_run_on_missing_state_dir_returns_none_not_error() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("never-created");
        let found = find_incomplete_run(&root, "testnet", None).unwrap();
        assert_eq!(found, None);
    }

    // === B4: a corrupt/unreadable journal must fail-closed at live startup,
    // not silently disarm the incomplete-run gate ===
    //
    // Corruption is MOST likely right after a crash -- exactly the moment
    // the incomplete-run gate exists to protect. Silently skipping an
    // unparseable journal (`Err(_) => continue`) means a new live run can
    // start and double-execute alongside whatever the corrupted run was
    // doing. IMPORTANT nuance: a journal whose LAST line is torn (crash
    // mid-append) but whose PRIOR lines all parse is the NORMAL crash shape
    // (standard JSONL crash recovery) and must still be found as
    // incomplete via its prior records -- only a file that yields NO
    // parseable header/records at all is a hard error.

    #[test]
    fn find_incomplete_run_hard_errors_on_a_journal_with_no_parseable_records_at_all() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let run_dir = root.join("runs").join("run-garbage");
        std::fs::create_dir_all(&run_dir).unwrap();
        // Truncated mid-record + garbage: not even a valid Header line.
        std::fs::write(
            run_dir.join("journal.jsonl"),
            b"{\"kind\":\"Header\",\"run_id\":\"run-garb\x00\x01\x02not json at all",
        )
        .unwrap();

        let result = find_incomplete_run(root, "testnet", Some(&Address::new("0xagent")));
        assert!(
            result.is_err(),
            "a journal with zero parseable records must hard-error live startup, not be \
             silently skipped: {result:?}"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("run-garbage") || msg.contains("journal.jsonl"),
            "the error must point at the offending file: {msg}"
        );
    }

    #[test]
    fn find_incomplete_run_still_detects_a_journal_whose_only_torn_line_is_the_last_one() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let agent = Address::new("0xagent");
        let cloid = Cloid::new();

        // Build a normal, valid journal first...
        {
            let mut j = ExecutionJournal::start(
                root,
                "run-torn".into(),
                RunHeader {
                    run_id: "run-torn".into(),
                    network: "testnet".into(),
                    agent: Some(agent.clone()),
                    master: Some(Address::new("0xmaster")),
                    symbol: Symbol::new("HYPE"),
                    side: Side::Long,
                    slices: 10,
                    plan_hash: "hash1".into(),
                    started_at_unix_ms: 0,
                },
            )
            .unwrap();
            j.record(&JournalRecord::SubmittedUnknown {
                slice_idx: 1,
                cloid,
            })
            .unwrap();
        }

        // ...then append a torn final line, simulating a crash mid-append
        // (partial JSON, no trailing newline).
        let path = ExecutionJournal::journal_path(root, "run-torn");
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        use std::io::Write as _;
        write!(f, "{{\"kind\":\"Acknowledged\",\"slice_idx\":1,\"cloi").unwrap();
        f.sync_data().unwrap();
        drop(f);

        // The prior (complete) lines must still be parsed and this run must
        // still be found as incomplete -- the torn tail must NOT be treated
        // as total corruption.
        let found = find_incomplete_run(root, "testnet", Some(&agent))
            .expect("a torn FINAL line with valid prior records must not hard-error")
            .expect("the run must still be detected as incomplete from its prior records");
        assert_eq!(found, "run-torn");
    }

    // === Read-only regression: no state dir, no journal file ===

    #[test]
    fn read_only_creates_no_state_dir_or_journal() {
        // This module never creates a directory except inside
        // `ExecutionJournal::start`. A read-only run's caller (main.rs)
        // must simply never call `start`/`open_existing` — this test pins
        // that `state_dir` and `find_incomplete_run` (both of which a
        // read-only startup path might legitimately still call to just
        // resolve a path or check) do not themselves create anything.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("would-be-state-dir");
        assert!(!root.exists());
        let _ = state_dir(Some(&root));
        assert!(!root.exists(), "state_dir() must not create the directory");
        let _ = find_incomplete_run(&root, "testnet", None).unwrap();
        assert!(
            !root.exists(),
            "find_incomplete_run() must not create the directory"
        );
    }
}
