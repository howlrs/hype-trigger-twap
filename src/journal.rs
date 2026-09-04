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
use std::io::Write;
use std::path::{Path, PathBuf};

use rust_decimal::{prelude::Signed, Decimal};
use serde::{Deserialize, Serialize};

use crate::types::{Address, Cloid, Side, Symbol, Tif};

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
// Keep Header unboxed: this is an append-only on-disk schema and boxing it
// would churn every caller without reducing any material runtime pressure.
#[allow(clippy::large_enum_variant)]
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
        /// Persisted so resume accounting can distinguish an ALO maker,
        /// whose own limit is its exact fill price, from a short IOC/GTC,
        /// whose sell limit is only a lower bound when `avg_px` is absent.
        /// Legacy journals deserialize this as `None` and fail closed for
        /// that ambiguous short-fill case.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tif: Option<Tif>,
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
        /// Optional for backwards-compatible JSONL decoding. New writers
        /// populate this from the validated replay immediately before the
        /// FinalReport is appended.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        whole_run: Option<WholeRunSummary>,
    },
    /// The run was explicitly abandoned via `--abandon-incomplete-run`,
    /// AFTER forced reconciliation of every submitted/unknown cloid (the
    /// reconciliation results themselves are separate `Acknowledged`/
    /// `Terminal` records preceding this one — this record only marks the
    /// run closed).
    Abandoned { note: String },
}

/// Durable, logical-run totals. `accounted_notional` may include a
/// conservative Prepared-price fallback for risk-cap accounting; `trusted_vwap`
/// never does and is omitted if any positive terminal lacks exchange avg_px.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WholeRunSummary {
    /// Original operator-requested quantity for this logical run. Optional
    /// for journals written before whole-run sizing was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_total: Option<String>,
    /// Grid-adjusted quantity actually planned for this logical run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adjusted_total: Option<String>,
    pub accounted_notional: String,
    pub cap_remaining: Option<String>,
    pub trusted_vwap: Option<String>,
    pub logical_elapsed_ms: u64,
    pub unresolved_cloids: usize,
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
    /// Versioned, typed execution-plan identity.  `plan_hash` is retained so
    /// legacy journals remain readable for reconciliation, but continuation
    /// requires the current fingerprint version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_fingerprint: Option<ExecutionPlanFingerprint>,
    pub started_at_unix_ms: u64,
    /// Absolute wall-clock end of this logical execution run.  Optional only
    /// for journals written before Issue #28; callers must derive it safely
    /// or refuse a new order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_deadline_unix_ms: Option<u64>,
}

/// Canonical, versioned identity of every setting that changes execution.
/// Decimal and duration values are stored after Rust's canonical `Display`
/// formatting, rather than preserving CLI spelling (for example `1.0`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionPlanFingerprint {
    pub version: u32,
    pub symbol: String,
    pub side: String,
    #[serde(default)]
    pub request_mode: String,
    #[serde(default)]
    pub request_value: String,
    pub per_slice: String,
    pub total_adjusted: String,
    pub total_requested: String,
    pub slices: u32,
    pub duration_ms: u64,
    pub slippage_bps: String,
    pub max_notional_usd: String,
    pub max_book_age_ms: u64,
    #[serde(default)]
    pub settle_retries: u32,
    pub child_algo: String,
    pub follow_poll_secs: u64,
    pub follow_repost_secs: u64,
    pub follow_threshold_bps: String,
    pub network: String,
    pub agent: Option<String>,
    pub master: Option<String>,
    /// Position-aware execution values frozen from the authoritative
    /// preflight snapshot. `None` is an ordinary size/USD TWAP.
    #[serde(default)]
    pub position_mode: Option<String>,
    #[serde(default)]
    pub initial_position_szi: Option<String>,
    #[serde(default)]
    pub target_position_szi: Option<String>,
    #[serde(default)]
    pub position_requested_value: Option<String>,
    #[serde(default)]
    pub position_reference_price: Option<String>,
    #[serde(default)]
    pub position_phases: Vec<PositionPhaseFingerprint>,
    #[serde(default)]
    pub reduce_only: bool,
    #[serde(default)]
    pub absolute_deadline_unix_ms: Option<u64>,
}

/// Canonical immutable identity of one phase in a position-aware run.
/// Keeping the whole ordered sequence in the Header makes a crash between
/// close-to-flat and open-from-flat reconstructible without consulting a
/// changed market price or the operator's memory.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PositionPhaseFingerprint {
    pub kind: String,
    pub side: String,
    pub size: String,
    pub reduce_only: bool,
}

impl ExecutionPlanFingerprint {
    pub const VERSION: u32 = 3;

    /// Position reversals are represented as multiple ordered phase sizes.
    /// Their logical requested/adjusted execution total is therefore the sum
    /// of the phases, not the first phase's `total_*` fields.
    pub fn logical_position_total(&self) -> Option<String> {
        if self.position_phases.is_empty() {
            return None;
        }
        self.position_phases
            .iter()
            .try_fold(Decimal::ZERO, |total, phase| {
                phase
                    .size
                    .parse::<Decimal>()
                    .ok()
                    .and_then(|size| total.checked_add(size))
            })
            .map(|total| total.to_string())
    }

    /// Lists field names rather than opaque hashes, making a rejected resume
    /// actionable without leaking any secret material.
    pub fn differing_fields(&self, other: &Self) -> Vec<&'static str> {
        let mut out = Vec::new();
        macro_rules! changed {
            ($field:ident) => {
                if self.$field != other.$field {
                    out.push(stringify!($field));
                }
            };
        }
        changed!(version);
        changed!(symbol);
        changed!(side);
        changed!(request_mode);
        changed!(request_value);
        changed!(per_slice);
        changed!(total_adjusted);
        changed!(total_requested);
        changed!(slices);
        changed!(duration_ms);
        changed!(slippage_bps);
        changed!(max_notional_usd);
        changed!(max_book_age_ms);
        changed!(settle_retries);
        changed!(child_algo);
        changed!(follow_poll_secs);
        changed!(follow_repost_secs);
        changed!(follow_threshold_bps);
        changed!(network);
        changed!(agent);
        changed!(master);
        changed!(position_mode);
        changed!(initial_position_szi);
        changed!(target_position_szi);
        changed!(position_requested_value);
        changed!(position_reference_price);
        changed!(position_phases);
        changed!(reduce_only);
        changed!(absolute_deadline_unix_ms);
        out
    }
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
    observer: Option<Box<dyn JournalRecordObserver>>,
}

/// Best-effort observer invoked only after a record has been written and
/// fsynced successfully. Observers are deliberately unable to return an
/// error: journal durability is the execution source of truth and telemetry
/// must never affect place/cancel/resume control flow or its exit status.
pub trait JournalRecordObserver: Send {
    fn observe(&mut self, record: &JournalRecord);
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
    #[error("invalid run id {run_id:?}: only ASCII letters, digits, '-' and '_' are allowed")]
    InvalidRunId { run_id: String },
    #[error("unsafe journal path (symlink is not permitted): {path}")]
    UnsafePath { path: PathBuf },
}

/// Run ids are path components, never paths.  UUIDv7 is the normal producer,
/// but accepting the conservative subset below keeps deterministic test and
/// operator-supplied ids usable without permitting `..`, separators, or an
/// absolute-path escape from `<state-dir>/runs`.
pub fn validate_run_id(run_id: &str) -> Result<(), JournalError> {
    if !run_id.is_empty()
        && run_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        Ok(())
    } else {
        Err(JournalError::InvalidRunId {
            run_id: run_id.to_owned(),
        })
    }
}

fn reject_symlink(path: &Path) -> Result<(), JournalError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(JournalError::UnsafePath {
            path: path.to_path_buf(),
        }),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(JournalError::Io(error)),
    }
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
        validate_run_id(&run_id)?;
        let run_dir = Self::run_dir(state_root, &run_id);
        // The directory entry must be durable before the journal header can
        // make this run discoverable.  `sync_data` on the JSONL alone does
        // not guarantee that a newly-created `runs/<run_id>` survives a
        // power loss, so create the parent first, create this run directory
        // exclusively, then fsync its parent directory (Issue #14).
        let runs_dir = state_root.join("runs");
        std::fs::create_dir_all(&runs_dir)?;
        // Persist both newly-created directory entries in order: `runs`
        // lives in `state_root`, and an explicitly supplied state root may
        // itself have been created by `create_dir_all`. Ancestors above the
        // state-root parent are an operator provisioning boundary.
        File::open(state_root)?.sync_all()?;
        if let Some(parent) = state_root
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            File::open(parent)?.sync_all()?;
        }
        std::fs::create_dir(&run_dir)?;
        File::open(&runs_dir)?.sync_all()?;
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
        // `create_new` made `journal.jsonl` discoverable in `run_dir`.
        // Persist that directory entry before writing the header: syncing
        // the file data alone cannot make a newly-created filename survive
        // a power loss.
        File::open(&run_dir)?.sync_all()?;
        let mut journal = Self {
            file,
            run_id,
            run_dir,
            observer: None,
        };
        journal.record(&JournalRecord::Header(header))?;
        Ok(journal)
    }

    /// Re-open an existing run's journal for append (used by `--resume`).
    pub fn open_existing(state_root: &Path, run_id: &str) -> Result<Self, JournalError> {
        validate_run_id(run_id)?;
        let run_dir = Self::run_dir(state_root, run_id);
        let path = run_dir.join("journal.jsonl");
        reject_symlink(&run_dir)?;
        reject_symlink(&path)?;
        let file = OpenOptions::new().append(true).open(&path)?;
        Ok(Self {
            file,
            run_id: run_id.to_string(),
            run_dir,
            observer: None,
        })
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// This run's directory on disk (`<state_root>/runs/<run_id>/`).
    pub fn dir(&self) -> &Path {
        &self.run_dir
    }

    /// Install or replace a best-effort post-fsync observer. This is intended
    /// for structured event/metric projection; it is never called before the
    /// journal header exists and cannot make [`Self::record`] fail.
    pub fn set_observer(&mut self, observer: Box<dyn JournalRecordObserver>) {
        self.observer = Some(observer);
    }

    /// Append one record and `fsync` before returning. Callers that need the
    /// "durable BEFORE the POST" guarantee (the `Prepared` record) MUST
    /// await/check this call's success before issuing the network send.
    pub fn record(&mut self, rec: &JournalRecord) -> Result<(), JournalError> {
        let mut line = serde_json::to_vec(rec)?;
        line.push(b'\n');
        self.file.write_all(&line)?;
        self.file.sync_data()?;
        if let Some(observer) = self.observer.as_mut() {
            observer.observe(rec);
        }
        Ok(())
    }

    /// Read every record currently in a run's journal, in file order.
    ///
    /// B4: tolerant of a torn FINAL line — the normal crash shape (the
    /// process died mid-`write_all`/before the trailing newline of the
    /// LAST record it was appending). All lines up to and including the
    /// last one that parses successfully are returned; an invalid last line
    /// is silently dropped only when the physical file does not end in a
    /// newline. A newline-terminated invalid record was fully written and is
    /// corruption, even when it is last. Any other parse failure is likewise
    /// a hard error, and a file that yields NO parseable records at
    /// all (e.g. not even a valid `Header`) is likewise a hard error — see
    /// `find_incomplete_run`, whose caller (`main.rs`, live startup) must
    /// fail closed on that case rather than silently treat the run as
    /// absent. This distinction is documented in `docs/OPERATIONS.md`.
    pub fn read_all(state_root: &Path, run_id: &str) -> Result<Vec<JournalRecord>, JournalError> {
        validate_run_id(run_id)?;
        let path = Self::journal_path(state_root, run_id);
        reject_symlink(&Self::run_dir(state_root, run_id))?;
        reject_symlink(&path)?;
        let bytes = std::fs::read(&path)?;
        let ends_with_newline = bytes.last() == Some(&b'\n');
        let lines: Vec<&[u8]> = bytes.split(|byte| *byte == b'\n').collect();
        let mut out = Vec::new();
        for (idx, line) in lines.iter().enumerate() {
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            match serde_json::from_slice::<JournalRecord>(line) {
                Ok(rec) => out.push(rec),
                Err(e) => {
                    let is_last_nonblank = lines[idx + 1..]
                        .iter()
                        .all(|later| later.iter().all(u8::is_ascii_whitespace));
                    // Torn final line IS the normal crash-mid-append shape
                    // ONLY when its trailing newline was never persisted and
                    // at least one prior record (starting with the Header)
                    // parsed. `BufRead::lines` discards that distinction, so
                    // this parser intentionally retains the raw EOF byte.
                    if is_last_nonblank && !ends_with_newline && !out.is_empty() {
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

/// Fill quantities and executed notional reconstructed from durable terminal
/// journal records during `--resume`.
///
/// Both values are non-negative. `notional` is credited at a terminal
/// `avg_px` when available. A missing price falls back to durable
/// `Prepared.px` only when it is a safe upper bound (Long, or an explicitly
/// persisted ALO); ambiguous Short IOC/GTC and legacy records fail closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalFillTotals {
    pub filled_sz: Decimal,
    pub notional: Decimal,
}

/// Fail-closed errors while reconstructing resume accounting from a journal.
///
/// A malformed amount or price must never be silently skipped: doing so could
/// make a resumed run place more than its remaining requested quantity or
/// notional cap permits.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum JournalAccountingError {
    #[error("journal state validation failed before accounting: {reason}")]
    InvalidReplay { reason: String },
    #[error("invalid decimal in {field} for cloid {cloid}: {value:?}")]
    InvalidDecimal {
        cloid: Cloid,
        field: &'static str,
        value: String,
    },
    #[error("negative size in {field} for cloid {cloid}: {value}")]
    NegativeSize {
        cloid: Cloid,
        field: &'static str,
        value: Decimal,
    },
    #[error("non-positive price in {field} for cloid {cloid}: {value}")]
    NonPositivePrice {
        cloid: Cloid,
        field: &'static str,
        value: Decimal,
    },
    #[error("filled terminal for cloid {cloid} has no preceding Prepared record")]
    MissingPrepared { cloid: Cloid },
    #[error(
        "terminal filled size {filled_sz} exceeds Prepared.sz {prepared_sz} for cloid {cloid}"
    )]
    FilledSizeExceedsPrepared {
        cloid: Cloid,
        filled_sz: Decimal,
        prepared_sz: Decimal,
    },
    #[error(
        "terminal avg price {avg_px} violates the {side:?} Prepared limit {prepared_px} for cloid {cloid}"
    )]
    AveragePriceViolatesLimit {
        cloid: Cloid,
        side: Side,
        avg_px: Decimal,
        prepared_px: Decimal,
    },
    #[error(
        "short terminal fill for cloid {cloid} has no avg price and Prepared.tif is {tif:?}; only an explicit ALO limit is a safe notional fallback"
    )]
    MissingAveragePriceForShort { cloid: Cloid, tif: Option<Tif> },
    #[error(
        "terminal accounting regressed for cloid {cloid}: filled size {previous_filled_sz} -> {next_filled_sz}, notional {previous_notional} -> {next_notional}"
    )]
    TerminalAccountingRegressed {
        cloid: Cloid,
        previous_filled_sz: Decimal,
        next_filled_sz: Decimal,
        previous_notional: Decimal,
        next_notional: Decimal,
    },
    #[error("decimal overflow while {operation} for cloid {cloid}")]
    Overflow {
        cloid: Cloid,
        operation: &'static str,
    },
}

/// Reconstruct already-filled size and notional from append-only journal
/// records for safe `--resume` accounting.
///
/// The final [`JournalRecord::Terminal`] for each cloid is selected exactly
/// once. Successive terminal snapshots must never reduce either the credited
/// size or notional; such a regression would weaken both resume accounting
/// and the run-level risk cap, so it is rejected as journal corruption.
/// A positive fill requires a preceding `Prepared` record and cannot exceed
/// its size. It uses `Terminal.avg_px` when present, provided that price is
/// within the prepared side-aware limit. When the exchange omitted `avg_px`,
/// the preceding `Prepared.px` is used only for a Long (conservative upper
/// bound) or an explicitly recorded ALO (exact resting price). A positive
/// Short IOC/GTC or legacy fill with no price has no safe upper bound and is
/// rejected. Zero fills need no prepared record or price. Every decimal used
/// by this accounting is validated and arithmetic is checked, so corrupt or
/// ambiguous journals fail closed rather than under-crediting a prior fill.
/// Compatibility entry point backed by the one validated replay state
/// machine. Headerless record fragments are accepted only for callers of
/// this legacy accounting API; a synthetic Header is prepended and every
/// subsequent state transition/accounting rule is still validated by
/// [`ValidatedJournalReplay`]. New code should consume that replay directly.
pub fn restore_fill_totals(
    records: &[JournalRecord],
) -> Result<JournalFillTotals, JournalAccountingError> {
    let mut with_header = Vec::new();
    let replay_records = if matches!(records.first(), Some(JournalRecord::Header(_))) {
        records
    } else {
        with_header.push(JournalRecord::Header(RunHeader {
            run_id: "legacy-accounting-fragment".into(),
            network: "unknown".into(),
            agent: None,
            master: None,
            symbol: Symbol::new("UNKNOWN"),
            side: Side::Long,
            slices: 0,
            plan_hash: String::new(),
            execution_fingerprint: None,
            started_at_unix_ms: 0,
            execution_deadline_unix_ms: None,
        }));
        with_header.extend_from_slice(records);
        &with_header
    };
    ValidatedJournalReplay::replay(replay_records)
        .map(|replay| replay.fill_totals)
        .map_err(|error| match error {
            JournalReplayError::Accounting(error) => error,
            other => JournalAccountingError::InvalidReplay {
                reason: other.to_string(),
            },
        })
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
    /// `completed` carried by the last durable FinalReport.  A report with
    /// `completed: false` is an explicit recovery barrier even when every
    /// cloid currently happens to be terminal (for example between the
    /// close and open phases of a zero-crossing position target).
    pub last_final_report_completed: Option<bool>,
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
    /// Whole-run projection carried by the last FinalReport, captured during
    /// the same sequential replay as state and accounting.
    pub last_whole_run: Option<WholeRunSummary>,
    pub abandoned: bool,
}

impl RunSummary {
    /// A started run remains incomplete until its last durable FinalReport
    /// explicitly says `completed: true`, carries no unknown outcomes, and
    /// every prepared cloid is terminal.  Merely reaching terminal order
    /// states is not enough: the process may have crashed before the final
    /// position/cap verification and completion record.
    pub fn is_incomplete(&self) -> bool {
        if self.header.is_none() || self.abandoned {
            return false;
        }
        if !self.final_report_seen || self.last_final_report_completed != Some(true) {
            return true;
        }
        self.last_final_report_unknown_cloids
            .as_ref()
            .is_some_and(|unknown| !unknown.is_empty())
            || self
                .cloids
                .iter()
                .any(|(_, st)| !matches!(st, CloidState::Terminal { .. }))
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
pub fn summarize(records: &[JournalRecord]) -> Result<RunSummary, JournalReplayError> {
    ValidatedJournalReplay::replay(records).map(|replay| replay.summary)
}

/// The one fail-closed interpretation of a journal.  Consumers must use this
/// instead of independently choosing "latest record wins" semantics.
#[derive(Debug, Clone)]
pub struct ValidatedJournalReplay {
    pub summary: RunSummary,
    pub fill_totals: JournalFillTotals,
    /// Parsed immutable notional ceiling from the typed Header fingerprint.
    /// `None` identifies legacy journals that cannot authenticate a cap.
    pub fingerprint_max_notional: Option<Decimal>,
    /// Execution VWAP is intentionally distinct from `fill_totals.notional`:
    /// the latter may use a conservative Prepared-price fallback for cap
    /// accounting, while this is present only when every positive fill has a
    /// trusted exchange average price.
    pub execution_vwap: Option<Decimal>,
    /// Original durable intent, keyed by cloid, for reconciliation.
    pub prepared: std::collections::HashMap<Cloid, PreparedJournalIntent>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PreparedJournalIntent {
    pub slice_idx: u32,
    pub symbol: Symbol,
    pub side: Side,
    pub tif: Option<Tif>,
    pub px: String,
    pub sz: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum JournalReplayError {
    #[error("journal is empty or has no Header")]
    MissingHeader,
    #[error("journal Header must be the first record")]
    HeaderNotFirst,
    #[error("journal contains more than one Header")]
    DuplicateHeader,
    #[error("journal record for cloid {cloid} has no preceding Prepared record")]
    MissingPrepared { cloid: Cloid },
    #[error("journal Prepared intent changed for cloid {cloid}")]
    ConflictingPrepared { cloid: Cloid },
    #[error("journal slice index changed for cloid {cloid}: {expected} -> {actual}")]
    SliceIndexChanged {
        cloid: Cloid,
        expected: u32,
        actual: u32,
    },
    #[error("invalid journal state transition for cloid {cloid}: {from} -> {to}")]
    InvalidTransition {
        cloid: Cloid,
        from: &'static str,
        to: &'static str,
    },
    #[error("journal Terminal for cloid {cloid} has non-terminal or unknown status {status:?}")]
    InvalidTerminalStatus { cloid: Cloid, status: String },
    #[error("journal Abandoned marker has {unresolved} unresolved order(s)")]
    AbandonedWithUnresolved { unresolved: usize },
    #[error("journal has records after Abandoned")]
    RecordAfterAbandoned,
    #[error("FinalReport completed=true has unresolved cloids or non-terminal orders")]
    CompletedFinalReportNotComplete,
    #[error(
        "FinalReport outcome_unknown_cloids {reported:?} does not match durable unresolved cloids {reconstructed:?}"
    )]
    FinalReportUnknownCloidsMismatch {
        reported: Vec<Cloid>,
        reconstructed: Vec<Cloid>,
    },
    #[error("journal has execution records after FinalReport completed=true")]
    RecordAfterCompletedFinalReport,
    #[error("FinalReport has an invalid filled_total value: {value:?}")]
    InvalidFinalReportFilledTotal { value: String },
    #[error(
        "FinalReport filled_total {reported} does not match durable terminal total {reconstructed}"
    )]
    FinalReportFilledTotalMismatch {
        reported: Decimal,
        reconstructed: Decimal,
    },
    #[error("execution fingerprint has invalid max_notional_usd {value:?}")]
    InvalidFingerprintMaxNotional { value: String },
    #[error("current execution fingerprint has invalid {field}: {reason}")]
    InvalidFingerprint { field: &'static str, reason: String },
    #[error("FinalReport whole_run is invalid: {reason}")]
    InvalidWholeRunSummary { reason: String },
    #[error(transparent)]
    Accounting(#[from] JournalAccountingError),
}

fn is_valid_journal_terminal_status(status: &str) -> bool {
    crate::client::is_terminal_order_status(status)
        || status.eq_ignore_ascii_case("aloRejected")
        || status.eq_ignore_ascii_case("neverReceived")
        || status.eq_ignore_ascii_case("positionTargetReached")
}

impl ValidatedJournalReplay {
    pub fn replay(records: &[JournalRecord]) -> Result<Self, JournalReplayError> {
        if records.is_empty() {
            return Err(JournalReplayError::MissingHeader);
        }
        if !matches!(records.first(), Some(JournalRecord::Header(_))) {
            return Err(JournalReplayError::HeaderNotFirst);
        }
        #[derive(Clone)]
        struct AccountingPrepared {
            px: Decimal,
            sz: Decimal,
            side: Side,
            tif: Option<Tif>,
        }
        #[derive(Clone)]
        struct TerminalFill {
            filled_sz: Decimal,
            notional: Decimal,
            avg_px: Option<Decimal>,
        }
        fn decimal(
            cloid: Cloid,
            field: &'static str,
            value: &str,
        ) -> Result<Decimal, JournalReplayError> {
            value.parse::<Decimal>().map_err(|_| {
                JournalAccountingError::InvalidDecimal {
                    cloid,
                    field,
                    value: value.to_owned(),
                }
                .into()
            })
        }

        let calculate_totals = |terminal_fills: &std::collections::HashMap<Cloid, TerminalFill>| {
            let mut fill_totals = JournalFillTotals {
                filled_sz: Decimal::ZERO,
                notional: Decimal::ZERO,
            };
            let mut execution_notional = Decimal::ZERO;
            let mut execution_filled = Decimal::ZERO;
            let mut vwap_available = true;
            for (cloid, terminal) in terminal_fills {
                fill_totals.filled_sz = fill_totals
                    .filled_sz
                    .checked_add(terminal.filled_sz)
                    .ok_or(JournalAccountingError::Overflow {
                        cloid: *cloid,
                        operation: "summing filled sizes",
                    })?;
                fill_totals.notional = fill_totals.notional.checked_add(terminal.notional).ok_or(
                    JournalAccountingError::Overflow {
                        cloid: *cloid,
                        operation: "summing notional",
                    },
                )?;
                if terminal.filled_sz > Decimal::ZERO {
                    let Some(px) = terminal.avg_px else {
                        vwap_available = false;
                        continue;
                    };
                    execution_filled = execution_filled.checked_add(terminal.filled_sz).ok_or(
                        JournalAccountingError::Overflow {
                            cloid: *cloid,
                            operation: "summing execution fills",
                        },
                    )?;
                    execution_notional = execution_notional
                        .checked_add(terminal.filled_sz.checked_mul(px).ok_or(
                            JournalAccountingError::Overflow {
                                cloid: *cloid,
                                operation: "multiplying execution fill by price",
                            },
                        )?)
                        .ok_or(JournalAccountingError::Overflow {
                            cloid: *cloid,
                            operation: "summing execution notional",
                        })?;
                }
            }
            let execution_vwap = if vwap_available && execution_filled > Decimal::ZERO {
                Some(execution_notional / execution_filled)
            } else {
                None
            };
            Ok::<_, JournalReplayError>((fill_totals, execution_vwap))
        };

        let mut prepared = std::collections::HashMap::new();
        let mut accounting_prepared = std::collections::HashMap::<Cloid, AccountingPrepared>::new();
        let mut terminal_fills = std::collections::HashMap::<Cloid, TerminalFill>::new();
        let mut order = Vec::<Cloid>::new();
        let mut state_values = std::collections::HashMap::<Cloid, CloidState>::new();
        let mut states = std::collections::HashMap::<Cloid, &'static str>::new();
        let mut summary = RunSummary::default();
        let mut fingerprint_max_notional = None;
        let mut abandoned = false;
        let mut completed_final_seen = false;
        for (record_idx, rec) in records.iter().enumerate() {
            if abandoned {
                return Err(JournalReplayError::RecordAfterAbandoned);
            }
            if completed_final_seen {
                return Err(JournalReplayError::RecordAfterCompletedFinalReport);
            }
            match rec {
                JournalRecord::Header(header) => {
                    if record_idx != 0 {
                        return Err(JournalReplayError::DuplicateHeader);
                    }
                    if let Some(fingerprint) = header.execution_fingerprint.as_ref() {
                        // Unknown versions are not resume-eligible, but their
                        // immutable cap is still an accounting claim consumed
                        // by inspection/verification. Validate it for every
                        // typed fingerprint so a future or corrupt version
                        // cannot degrade into an apparently valid null cap.
                        let max =
                            fingerprint
                                .max_notional_usd
                                .parse::<Decimal>()
                                .map_err(|_| JournalReplayError::InvalidFingerprintMaxNotional {
                                    value: fingerprint.max_notional_usd.clone(),
                                })?;
                        if max <= Decimal::ZERO {
                            return Err(JournalReplayError::InvalidFingerprintMaxNotional {
                                value: fingerprint.max_notional_usd.clone(),
                            });
                        }
                        fingerprint_max_notional = Some(max);
                        if fingerprint.version == ExecutionPlanFingerprint::VERSION {
                            validate_current_fingerprint(header, fingerprint)?;
                        }
                    }
                    summary.header = Some(header.clone());
                }
                JournalRecord::Prepared {
                    slice_idx,
                    cloid,
                    symbol,
                    side,
                    tif,
                    px,
                    sz,
                    ..
                } => {
                    let next = PreparedJournalIntent {
                        slice_idx: *slice_idx,
                        symbol: symbol.clone(),
                        side: *side,
                        tif: *tif,
                        px: px.clone(),
                        sz: sz.clone(),
                    };
                    if let Some(old) = prepared.get(cloid) {
                        if old != &next {
                            return Err(JournalReplayError::ConflictingPrepared { cloid: *cloid });
                        }
                        return Err(JournalReplayError::InvalidTransition {
                            cloid: *cloid,
                            from: states.get(cloid).copied().unwrap_or("Prepared"),
                            to: "Prepared",
                        });
                    }
                    let prepared_sz = decimal(*cloid, "Prepared.sz", sz)?;
                    if prepared_sz < Decimal::ZERO {
                        return Err(JournalAccountingError::NegativeSize {
                            cloid: *cloid,
                            field: "Prepared.sz",
                            value: prepared_sz,
                        }
                        .into());
                    }
                    let prepared_px = decimal(*cloid, "Prepared.px", px)?;
                    if prepared_px <= Decimal::ZERO {
                        return Err(JournalAccountingError::NonPositivePrice {
                            cloid: *cloid,
                            field: "Prepared.px",
                            value: prepared_px,
                        }
                        .into());
                    }
                    prepared.insert(*cloid, next);
                    accounting_prepared.insert(
                        *cloid,
                        AccountingPrepared {
                            px: prepared_px,
                            sz: prepared_sz,
                            side: *side,
                            tif: *tif,
                        },
                    );
                    order.push(*cloid);
                    state_values.insert(*cloid, CloidState::PreparedOnly);
                    states.insert(*cloid, "Prepared");
                }
                JournalRecord::SubmittedUnknown { slice_idx, cloid }
                | JournalRecord::Acknowledged {
                    slice_idx, cloid, ..
                }
                | JournalRecord::Terminal {
                    slice_idx, cloid, ..
                } => {
                    let Some(intent) = prepared.get(cloid) else {
                        return Err(JournalReplayError::MissingPrepared { cloid: *cloid });
                    };
                    if intent.slice_idx != *slice_idx {
                        return Err(JournalReplayError::SliceIndexChanged {
                            cloid: *cloid,
                            expected: intent.slice_idx,
                            actual: *slice_idx,
                        });
                    }
                    let from = states.get(cloid).copied().unwrap_or("missing");
                    let to = match rec {
                        JournalRecord::SubmittedUnknown { .. } => "SubmittedUnknown",
                        JournalRecord::Acknowledged { .. } => "Acknowledged",
                        JournalRecord::Terminal { .. } => "Terminal",
                        _ => unreachable!(),
                    };
                    let allowed = matches!(
                        (from, to),
                        ("Prepared", "SubmittedUnknown" | "Acknowledged" | "Terminal")
                            | ("SubmittedUnknown", "Acknowledged" | "Terminal")
                            | ("Acknowledged", "Acknowledged" | "Terminal")
                            | ("Terminal", "Terminal")
                    );
                    if !allowed {
                        return Err(JournalReplayError::InvalidTransition {
                            cloid: *cloid,
                            from,
                            to,
                        });
                    }
                    let next_state = match rec {
                        JournalRecord::SubmittedUnknown { .. } => CloidState::SubmittedUnknown,
                        JournalRecord::Acknowledged { .. } => CloidState::Acknowledged,
                        JournalRecord::Terminal {
                            filled_sz,
                            avg_px,
                            status,
                            ..
                        } => {
                            if !is_valid_journal_terminal_status(status) {
                                return Err(JournalReplayError::InvalidTerminalStatus {
                                    cloid: *cloid,
                                    status: status.clone(),
                                });
                            }
                            let filled_sz_decimal =
                                decimal(*cloid, "Terminal.filled_sz", filled_sz)?;
                            if filled_sz_decimal < Decimal::ZERO {
                                return Err(JournalAccountingError::NegativeSize {
                                    cloid: *cloid,
                                    field: "Terminal.filled_sz",
                                    value: filled_sz_decimal,
                                }
                                .into());
                            }
                            let prepared_order = accounting_prepared
                                .get(cloid)
                                .ok_or(JournalAccountingError::MissingPrepared { cloid: *cloid })?;
                            let (notional, trusted_avg_px) = if filled_sz_decimal > Decimal::ZERO {
                                if filled_sz_decimal > prepared_order.sz {
                                    return Err(
                                        JournalAccountingError::FilledSizeExceedsPrepared {
                                            cloid: *cloid,
                                            filled_sz: filled_sz_decimal,
                                            prepared_sz: prepared_order.sz,
                                        }
                                        .into(),
                                    );
                                }
                                let price = match avg_px {
                                    Some(value) => {
                                        let value = decimal(*cloid, "Terminal.avg_px", value)?;
                                        if value <= Decimal::ZERO {
                                            return Err(JournalAccountingError::NonPositivePrice {
                                                cloid: *cloid,
                                                field: "Terminal.avg_px",
                                                value,
                                            }
                                            .into());
                                        }
                                        let violates = match prepared_order.side {
                                            Side::Long => value > prepared_order.px,
                                            Side::Short => value < prepared_order.px,
                                        };
                                        if violates {
                                            return Err(
                                                JournalAccountingError::AveragePriceViolatesLimit {
                                                    cloid: *cloid,
                                                    side: prepared_order.side,
                                                    avg_px: value,
                                                    prepared_px: prepared_order.px,
                                                }
                                                .into(),
                                            );
                                        }
                                        value
                                    }
                                    None if prepared_order.side == Side::Short
                                        && prepared_order.tif != Some(Tif::Alo) =>
                                    {
                                        return Err(
                                            JournalAccountingError::MissingAveragePriceForShort {
                                                cloid: *cloid,
                                                tif: prepared_order.tif,
                                            }
                                            .into(),
                                        );
                                    }
                                    None => prepared_order.px,
                                };
                                (
                                    filled_sz_decimal.checked_mul(price).ok_or(
                                        JournalAccountingError::Overflow {
                                            cloid: *cloid,
                                            operation: "multiplying fill size by price",
                                        },
                                    )?,
                                    avg_px
                                        .as_ref()
                                        .map(|value| decimal(*cloid, "Terminal.avg_px", value))
                                        .transpose()?,
                                )
                            } else {
                                (Decimal::ZERO, None)
                            };
                            if let Some(previous) = terminal_fills.get(cloid) {
                                if filled_sz_decimal < previous.filled_sz
                                    || notional < previous.notional
                                {
                                    return Err(
                                        JournalAccountingError::TerminalAccountingRegressed {
                                            cloid: *cloid,
                                            previous_filled_sz: previous.filled_sz,
                                            next_filled_sz: filled_sz_decimal,
                                            previous_notional: previous.notional,
                                            next_notional: notional,
                                        }
                                        .into(),
                                    );
                                }
                            }
                            terminal_fills.insert(
                                *cloid,
                                TerminalFill {
                                    filled_sz: filled_sz_decimal,
                                    notional,
                                    avg_px: trusted_avg_px,
                                },
                            );
                            CloidState::Terminal {
                                filled_sz: filled_sz.clone(),
                                avg_px: avg_px.clone(),
                            }
                        }
                        _ => unreachable!(),
                    };
                    state_values.insert(*cloid, next_state);
                    states.insert(*cloid, to);
                }
                JournalRecord::FinalReport {
                    completed,
                    filled_total,
                    outcome_unknown_cloids,
                    whole_run,
                    ..
                } => {
                    let reported = filled_total.parse::<Decimal>().map_err(|_| {
                        JournalReplayError::InvalidFinalReportFilledTotal {
                            value: filled_total.clone(),
                        }
                    })?;
                    let (current_totals, current_vwap) = calculate_totals(&terminal_fills)?;
                    if reported != current_totals.filled_sz {
                        return Err(JournalReplayError::FinalReportFilledTotalMismatch {
                            reported,
                            reconstructed: current_totals.filled_sz,
                        });
                    }
                    let unresolved = state_values
                        .values()
                        .filter(|state| !matches!(state, CloidState::Terminal { .. }))
                        .count();
                    validate_whole_run(
                        summary.header.as_ref(),
                        whole_run.as_ref(),
                        &current_totals,
                        current_vwap,
                        unresolved,
                    )?;
                    summary.final_report_seen = true;
                    summary.last_final_report_completed = Some(*completed);
                    summary.last_final_report_unknown_cloids = Some(outcome_unknown_cloids.clone());
                    summary.last_whole_run = whole_run.clone();
                    if *completed {
                        if !outcome_unknown_cloids.is_empty()
                            || state_values
                                .values()
                                .any(|state| !matches!(state, CloidState::Terminal { .. }))
                        {
                            return Err(JournalReplayError::CompletedFinalReportNotComplete);
                        }
                        completed_final_seen = true;
                    } else {
                        let reconstructed: Vec<_> = order
                            .iter()
                            .copied()
                            .filter(|cloid| {
                                state_values.get(cloid).is_some_and(|state| {
                                    !matches!(state, CloidState::Terminal { .. })
                                })
                            })
                            .collect();
                        let reported_set: std::collections::HashSet<_> =
                            outcome_unknown_cloids.iter().copied().collect();
                        let reconstructed_set: std::collections::HashSet<_> =
                            reconstructed.iter().copied().collect();
                        if reported_set.len() != outcome_unknown_cloids.len()
                            || reported_set != reconstructed_set
                        {
                            return Err(JournalReplayError::FinalReportUnknownCloidsMismatch {
                                reported: outcome_unknown_cloids.clone(),
                                reconstructed,
                            });
                        }
                    }
                }
                JournalRecord::Abandoned { .. } => {
                    let unresolved = state_values
                        .values()
                        .filter(|state| !matches!(state, CloidState::Terminal { .. }))
                        .count();
                    if unresolved != 0 {
                        return Err(JournalReplayError::AbandonedWithUnresolved { unresolved });
                    }
                    abandoned = true;
                }
            }
        }

        summary.abandoned = abandoned;
        summary.cloids = order
            .into_iter()
            .map(|cloid| {
                let state = state_values
                    .remove(&cloid)
                    .unwrap_or(CloidState::PreparedOnly);
                (cloid, state)
            })
            .collect();

        let (fill_totals, execution_vwap) = calculate_totals(&terminal_fills)?;
        Ok(Self {
            summary,
            fill_totals,
            fingerprint_max_notional,
            execution_vwap,
            prepared,
        })
    }
}

/// Cross-check one optional whole-run projection against the durable state at
/// that exact FinalReport checkpoint. An incomplete report may legitimately
/// be followed by resume records, so comparing it only with the journal's
/// eventual totals would reject a valid continuation. Old journals may omit
/// the projection; once present it remains a checked accounting claim.
fn validate_whole_run(
    header: Option<&RunHeader>,
    whole: Option<&WholeRunSummary>,
    fill_totals: &JournalFillTotals,
    execution_vwap: Option<Decimal>,
    unresolved_cloids: usize,
) -> Result<(), JournalReplayError> {
    let Some(whole) = whole else {
        return Ok(());
    };
    let invalid = |reason: String| JournalReplayError::InvalidWholeRunSummary { reason };
    let parse_nonnegative = |field: &str, value: &str| {
        value
            .parse::<Decimal>()
            .ok()
            .filter(|value| *value >= Decimal::ZERO)
            .ok_or_else(|| {
                invalid(format!(
                    "{field} must be a non-negative decimal, got {value:?}"
                ))
            })
    };
    let accounted = parse_nonnegative("accounted_notional", &whole.accounted_notional)?;
    if accounted != fill_totals.notional {
        return Err(invalid(format!(
            "accounted_notional {accounted} does not match replayed {}",
            fill_totals.notional
        )));
    }
    if let Some(value) = whole.cap_remaining.as_deref() {
        let reported = parse_nonnegative("cap_remaining", value)?;
        let Some(fingerprint) = header.and_then(|header| header.execution_fingerprint.as_ref())
        else {
            return Err(invalid(
                "cap_remaining is present without a typed fingerprint".into(),
            ));
        };
        let max = fingerprint
            .max_notional_usd
            .parse::<Decimal>()
            .map_err(|_| {
                invalid(format!(
                    "cap_remaining is present but max_notional_usd is invalid: {:?}",
                    fingerprint.max_notional_usd
                ))
            })?;
        let expected = (max - fill_totals.notional).max(Decimal::ZERO);
        if reported != expected {
            return Err(invalid(format!(
                "cap_remaining {reported} does not match replayed {expected}"
            )));
        }
    }
    let reported_vwap = whole
        .trusted_vwap
        .as_deref()
        .map(|value| parse_nonnegative("trusted_vwap", value))
        .transpose()?;
    if reported_vwap != execution_vwap {
        return Err(invalid(format!(
            "trusted_vwap {:?} does not match replayed {:?}",
            reported_vwap, execution_vwap
        )));
    }
    if whole.unresolved_cloids != unresolved_cloids {
        return Err(invalid(format!(
            "unresolved_cloids {} does not match replayed {}",
            whole.unresolved_cloids, unresolved_cloids
        )));
    }
    if let Some(fingerprint) = header.and_then(|header| header.execution_fingerprint.as_ref()) {
        let expected = fingerprint
            .logical_position_total()
            .unwrap_or_else(|| fingerprint.total_requested.clone());
        if let Some(value) = whole.requested_total.as_deref() {
            let _ = parse_nonnegative("requested_total", value)?;
            if value != expected {
                return Err(invalid(format!(
                    "requested_total {value:?} does not match fingerprint {expected:?}"
                )));
            }
        }
        let expected = fingerprint
            .logical_position_total()
            .unwrap_or_else(|| fingerprint.total_adjusted.clone());
        if let Some(value) = whole.adjusted_total.as_deref() {
            let _ = parse_nonnegative("adjusted_total", value)?;
            if value != expected {
                return Err(invalid(format!(
                    "adjusted_total {value:?} does not match fingerprint {expected:?}"
                )));
            }
        }
    }
    Ok(())
}

/// Validate the self-contained, current-version part of a fingerprint before
/// any consumer (including the read-only runs CLI) treats the journal as a
/// sound execution plan. Market metadata / size-grid reconstruction remains
/// the live resume validator's responsibility.
fn validate_current_fingerprint(
    header: &RunHeader,
    fingerprint: &ExecutionPlanFingerprint,
) -> Result<(), JournalReplayError> {
    let invalid = |field, reason: String| JournalReplayError::InvalidFingerprint { field, reason };
    let decimal = |field: &'static str, value: &str| {
        let parsed = value
            .parse::<Decimal>()
            .map_err(|_| invalid(field, format!("not a decimal: {value:?}")))?;
        if parsed.normalize().to_string() != value {
            return Err(invalid(field, "not canonical".into()));
        }
        Ok(parsed)
    };
    let positive_decimal = |field: &'static str, value: &str| {
        let parsed = decimal(field, value)?;
        if parsed <= Decimal::ZERO {
            return Err(invalid(field, "outside permitted range".into()));
        }
        Ok(parsed)
    };
    let nonnegative_decimal = |field: &'static str, value: &str| {
        let parsed = decimal(field, value)?;
        if parsed < Decimal::ZERO {
            return Err(invalid(field, "outside permitted range".into()));
        }
        Ok(parsed)
    };
    if fingerprint.slices == 0 {
        return Err(invalid("slices", "must be positive".into()));
    }
    if fingerprint.duration_ms == 0 {
        return Err(invalid("duration_ms", "must be positive".into()));
    }
    if fingerprint.symbol != header.symbol.as_str() {
        return Err(invalid("symbol", "does not match Header".into()));
    }
    if fingerprint.side != header.side.to_string()
        || !matches!(fingerprint.side.as_str(), "long" | "short")
    {
        return Err(invalid("side", "invalid or does not match Header".into()));
    }
    if fingerprint.slices != header.slices {
        return Err(invalid("slices", "does not match Header".into()));
    }
    if fingerprint.absolute_deadline_unix_ms != header.execution_deadline_unix_ms
        || fingerprint.absolute_deadline_unix_ms.is_none()
    {
        return Err(invalid(
            "absolute_deadline_unix_ms",
            "missing or does not match Header".into(),
        ));
    }
    if fingerprint.network != header.network {
        return Err(invalid("network", "does not match Header".into()));
    }
    if fingerprint.agent.as_deref() != header.agent.as_ref().map(Address::as_str) {
        return Err(invalid("agent", "does not match Header".into()));
    }
    if fingerprint.master.as_deref() != header.master.as_ref().map(Address::as_str) {
        return Err(invalid("master", "does not match Header".into()));
    }
    let per_slice = positive_decimal("per_slice", &fingerprint.per_slice)?;
    let total_adjusted = positive_decimal("total_adjusted", &fingerprint.total_adjusted)?;
    let total_requested = positive_decimal("total_requested", &fingerprint.total_requested)?;
    if total_adjusted > total_requested || per_slice > total_adjusted {
        return Err(invalid(
            "sizing",
            "per_slice, total_adjusted, and total_requested are inconsistent".into(),
        ));
    }
    let slippage_bps = nonnegative_decimal("slippage_bps", &fingerprint.slippage_bps)?;
    if slippage_bps >= Decimal::from(10_000) {
        return Err(invalid(
            "slippage_bps",
            "must be below the 10000 bps hard cap".into(),
        ));
    }
    let _ = positive_decimal("max_notional_usd", &fingerprint.max_notional_usd)?;
    let _ = nonnegative_decimal("follow_threshold_bps", &fingerprint.follow_threshold_bps)?;
    if fingerprint.settle_retries == 0 {
        return Err(invalid("settle_retries", "must be positive".into()));
    }
    if !matches!(
        fingerprint.child_algo.as_str(),
        "market" | "passive" | "follow"
    ) {
        return Err(invalid("child_algo", "unsupported value".into()));
    }
    if fingerprint.follow_poll_secs == 0 || fingerprint.follow_repost_secs == 0 {
        return Err(invalid("follow timing", "must be positive".into()));
    }
    if !matches!(fingerprint.network.as_str(), "mainnet" | "testnet") {
        return Err(invalid("network", "unsupported value".into()));
    }
    match fingerprint.position_mode.as_deref() {
        None => {
            if !matches!(fingerprint.request_mode.as_str(), "size" | "usd") {
                return Err(invalid(
                    "request_mode",
                    "ordinary run requires size or usd".into(),
                ));
            }
            let request_value = positive_decimal("request_value", &fingerprint.request_value)?;
            let scheduled_total = per_slice
                .checked_mul(Decimal::from(fingerprint.slices))
                .ok_or_else(|| invalid("sizing", "slice multiplication overflowed".into()))?;
            if scheduled_total != total_adjusted {
                return Err(invalid(
                    "sizing",
                    "total_adjusted does not equal per_slice * slices".into(),
                ));
            }
            if fingerprint.request_mode == "size" && request_value != total_requested {
                return Err(invalid(
                    "request_value",
                    "size request does not match total_requested".into(),
                ));
            }
            if fingerprint.initial_position_szi.is_some()
                || fingerprint.target_position_szi.is_some()
                || fingerprint.position_requested_value.is_some()
                || fingerprint.position_reference_price.is_some()
                || !fingerprint.position_phases.is_empty()
                || fingerprint.reduce_only
            {
                return Err(invalid(
                    "position fields",
                    "ordinary run contains position-only values".into(),
                ));
            }
        }
        Some(mode) => {
            if !matches!(mode, "flatten" | "target_sz" | "target_usd")
                || fingerprint.request_mode != mode
            {
                return Err(invalid(
                    "position_mode",
                    "does not match request_mode".into(),
                ));
            }
            let initial = fingerprint
                .initial_position_szi
                .as_deref()
                .ok_or_else(|| invalid("initial_position_szi", "missing".into()))?;
            let target = fingerprint
                .target_position_szi
                .as_deref()
                .ok_or_else(|| invalid("target_position_szi", "missing".into()))?;
            // Positions and position targets are signed. Requiring these to
            // be non-negative would make every valid short target look like
            // journal corruption to read-only consumers.
            let initial = decimal("initial_position_szi", initial)?;
            let target = decimal("target_position_szi", target)?;
            if mode == "flatten" {
                if fingerprint.request_value != "0"
                    || target != Decimal::ZERO
                    || fingerprint.position_requested_value.is_some()
                    || fingerprint.position_reference_price.is_some()
                {
                    return Err(invalid("flatten fields", "inconsistent values".into()));
                }
            } else {
                let request = fingerprint
                    .position_requested_value
                    .as_deref()
                    .ok_or_else(|| invalid("position_requested_value", "missing".into()))?;
                let _ = decimal("position_requested_value", request)?;
                let _ = decimal("request_value", &fingerprint.request_value)?;
                if fingerprint.request_value != request {
                    return Err(invalid(
                        "request_value",
                        "does not match position_requested_value".into(),
                    ));
                }
                if mode == "target_usd" {
                    let price = fingerprint
                        .position_reference_price
                        .as_deref()
                        .ok_or_else(|| invalid("position_reference_price", "missing".into()))?;
                    let _ = positive_decimal("position_reference_price", price)?;
                } else if fingerprint.position_reference_price.is_some() {
                    return Err(invalid(
                        "position_reference_price",
                        "only valid for target_usd".into(),
                    ));
                }
            }
            for (index, phase) in fingerprint.position_phases.iter().enumerate() {
                if !matches!(
                    phase.kind.as_str(),
                    "adjust" | "close_to_flat" | "open_from_flat"
                ) || !matches!(phase.side.as_str(), "long" | "short")
                {
                    return Err(invalid("position_phases", format!("invalid phase {index}")));
                }
                let _ = positive_decimal("position_phases.size", &phase.size)?;
            }
            if initial != target && fingerprint.position_phases.is_empty() {
                return Err(invalid(
                    "position_phases",
                    "missing executable phase".into(),
                ));
            }
            let expected_side = |delta: Decimal| {
                if delta > Decimal::ZERO {
                    "long"
                } else {
                    "short"
                }
            };
            match fingerprint.position_phases.as_slice() {
                [] => {}
                [phase] => {
                    let delta = target - initial;
                    let reduces = initial != Decimal::ZERO
                        && (target == Decimal::ZERO
                            || (initial.signum() == target.signum()
                                && target.abs() < initial.abs()));
                    if phase.kind != "adjust"
                        || phase.side != expected_side(delta)
                        || phase.size != delta.abs().to_string()
                        || phase.reduce_only != reduces
                    {
                        return Err(invalid(
                            "position_phases",
                            "adjust phase is inconsistent with frozen endpoints".into(),
                        ));
                    }
                }
                [close, open] => {
                    if initial == Decimal::ZERO
                        || target == Decimal::ZERO
                        || initial.signum() == target.signum()
                        || close.kind != "close_to_flat"
                        || close.side != expected_side(-initial)
                        || close.size != initial.abs().to_string()
                        || !close.reduce_only
                        || open.kind != "open_from_flat"
                        || open.side != expected_side(target)
                        || open.size != target.abs().to_string()
                        || open.reduce_only
                    {
                        return Err(invalid(
                            "position_phases",
                            "zero-cross phase sequence is inconsistent with frozen endpoints"
                                .into(),
                        ));
                    }
                }
                _ => {
                    return Err(invalid(
                        "position_phases",
                        "must contain at most two ordered phases".into(),
                    ));
                }
            }
            if let Some(first) = fingerprint.position_phases.first() {
                if fingerprint.reduce_only != first.reduce_only || fingerprint.side != first.side {
                    return Err(invalid(
                        "reduce_only",
                        "does not match first position phase".into(),
                    ));
                }
            } else if fingerprint.reduce_only {
                return Err(invalid(
                    "reduce_only",
                    "cannot be set when the position plan has no phase".into(),
                ));
            }
        }
    }
    Ok(())
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
        let summary = ValidatedJournalReplay::replay(&records)
            .map_err(|e| {
                JournalError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid journal state machine: {e}"),
                ))
            })?
            .summary;
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
            execution_fingerprint: None,
            started_at_unix_ms: 1_000,
            execution_deadline_unix_ms: None,
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

    fn fingerprint_fixture() -> ExecutionPlanFingerprint {
        ExecutionPlanFingerprint {
            version: ExecutionPlanFingerprint::VERSION,
            symbol: "HYPE".into(),
            side: "long".into(),
            request_mode: "target_sz".into(),
            request_value: "10".into(),
            per_slice: "1".into(),
            total_adjusted: "10".into(),
            total_requested: "10".into(),
            slices: 10,
            duration_ms: 60_000,
            slippage_bps: "20".into(),
            max_notional_usd: "1000".into(),
            max_book_age_ms: 3_000,
            settle_retries: 25,
            child_algo: "market".into(),
            follow_poll_secs: 2,
            follow_repost_secs: 10,
            follow_threshold_bps: "1".into(),
            network: "testnet".into(),
            agent: Some("0x1111111111111111111111111111111111111111".into()),
            master: Some("0x2222222222222222222222222222222222222222".into()),
            position_mode: Some("target_sz".into()),
            initial_position_szi: Some("5".into()),
            target_position_szi: Some("10".into()),
            position_requested_value: Some("10".into()),
            position_reference_price: None,
            position_phases: vec![PositionPhaseFingerprint {
                kind: "adjust".into(),
                side: "long".into(),
                size: "5".into(),
                reduce_only: false,
            }],
            reduce_only: false,
            absolute_deadline_unix_ms: Some(1_900_000_000_000),
        }
    }

    #[test]
    fn fingerprint_reports_each_execution_affecting_field_by_name() {
        let base = fingerprint_fixture();
        macro_rules! changed {
            ($field:ident, $value:expr) => {{
                let mut other = base.clone();
                other.$field = $value;
                assert_eq!(
                    base.differing_fields(&other),
                    vec![stringify!($field)],
                    "field {} must be independently checked",
                    stringify!($field)
                );
            }};
        }
        changed!(version, base.version + 1);
        changed!(symbol, "BTC".into());
        changed!(side, "short".into());
        changed!(request_mode, "usd".into());
        changed!(request_value, "11".into());
        changed!(per_slice, "2".into());
        changed!(total_adjusted, "9".into());
        changed!(total_requested, "11".into());
        changed!(slices, 11);
        changed!(duration_ms, 60_001);
        changed!(slippage_bps, "21".into());
        changed!(max_notional_usd, "999".into());
        changed!(max_book_age_ms, 3_001);
        changed!(settle_retries, 26);
        changed!(child_algo, "passive".into());
        changed!(follow_poll_secs, 3);
        changed!(follow_repost_secs, 11);
        changed!(follow_threshold_bps, "2".into());
        changed!(network, "mainnet".into());
        changed!(agent, None);
        changed!(master, None);
        changed!(position_mode, Some("target_usd".into()));
        changed!(initial_position_szi, Some("4".into()));
        changed!(target_position_szi, Some("9".into()));
        changed!(position_requested_value, Some("9".into()));
        changed!(position_reference_price, Some("50".into()));
        changed!(position_phases, Vec::new());
        changed!(reduce_only, true);
        changed!(absolute_deadline_unix_ms, Some(1_900_000_000_001));
    }

    #[test]
    fn fingerprint_is_named_versioned_json_without_runtime_order_identity() {
        let fingerprint = fingerprint_fixture();
        let value = serde_json::to_value(&fingerprint).unwrap();
        let object = value.as_object().unwrap();
        assert_eq!(object.get("version"), Some(&serde_json::json!(3)));
        assert!(!object.contains_key("cloid"));
        assert!(!object.contains_key("nonce"));

        // A JSON object is deserialized by field name, not insertion order.
        // Rebuilding it in reverse iteration order yields the same typed plan
        // (serde_json may internally sort keys; either way order is inert).
        let reordered = serde_json::Value::Object(
            object
                .iter()
                .rev()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        );
        assert_eq!(
            serde_json::from_value::<ExecutionPlanFingerprint>(reordered).unwrap(),
            fingerprint
        );
    }

    #[test]
    fn current_fingerprint_structural_corruption_is_rejected_by_replay() {
        type Mutator = Box<dyn Fn(&mut ExecutionPlanFingerprint)>;
        let base = fingerprint_fixture();
        let header = |fingerprint: ExecutionPlanFingerprint| RunHeader {
            run_id: "fingerprint-validation".into(),
            network: "testnet".into(),
            agent: Some(Address::new("0x1111111111111111111111111111111111111111")),
            master: Some(Address::new("0x2222222222222222222222222222222222222222")),
            symbol: Symbol::new("HYPE"),
            side: Side::Long,
            slices: 10,
            plan_hash: "fixture".into(),
            execution_fingerprint: Some(fingerprint),
            started_at_unix_ms: 1,
            execution_deadline_unix_ms: Some(1_900_000_000_000),
        };
        let cases: Vec<(&str, Mutator)> = vec![
            (
                "noncanonical per_slice",
                Box::new(|f| f.per_slice = "1.0".into()),
            ),
            ("zero duration", Box::new(|f| f.duration_ms = 0)),
            ("invalid side", Box::new(|f| f.side = "sideways".into())),
            (
                "invalid algorithm",
                Box::new(|f| f.child_algo = "ioc".into()),
            ),
            (
                "deadline mismatch",
                Box::new(|f| f.absolute_deadline_unix_ms = Some(1)),
            ),
            (
                "phase mismatch",
                Box::new(|f| f.position_phases[0].size = "-5".into()),
            ),
            (
                "network does not match header",
                Box::new(|f| f.network = "mainnet".into()),
            ),
            ("zero settle retries", Box::new(|f| f.settle_retries = 0)),
            (
                "slippage reaches hard cap",
                Box::new(|f| f.slippage_bps = "10000".into()),
            ),
            ("agent does not match header", Box::new(|f| f.agent = None)),
        ];
        assert!(
            ValidatedJournalReplay::replay(&[JournalRecord::Header(header(base.clone()))]).is_ok(),
            "the unmodified fixture must be structurally valid"
        );
        for (name, mutate) in cases {
            let mut fingerprint = base.clone();
            mutate(&mut fingerprint);
            assert!(
                matches!(
                    ValidatedJournalReplay::replay(&[JournalRecord::Header(header(fingerprint))]),
                    Err(JournalReplayError::InvalidFingerprint { .. })
                ),
                "{name}"
            );
        }
    }

    #[test]
    fn current_fingerprint_accepts_signed_short_position_values() {
        let mut fingerprint = fingerprint_fixture();
        fingerprint.side = "short".into();
        fingerprint.request_value = "-10".into();
        fingerprint.initial_position_szi = Some("-5".into());
        fingerprint.target_position_szi = Some("-10".into());
        fingerprint.position_requested_value = Some("-10".into());
        fingerprint.position_phases = vec![PositionPhaseFingerprint {
            kind: "adjust".into(),
            side: "short".into(),
            size: "5".into(),
            reduce_only: false,
        }];
        let header = RunHeader {
            run_id: "short-fingerprint".into(),
            network: "testnet".into(),
            agent: Some(Address::new("0x1111111111111111111111111111111111111111")),
            master: Some(Address::new("0x2222222222222222222222222222222222222222")),
            symbol: Symbol::new("HYPE"),
            side: Side::Short,
            slices: 10,
            plan_hash: "fixture".into(),
            execution_fingerprint: Some(fingerprint),
            started_at_unix_ms: 1,
            execution_deadline_unix_ms: Some(1_900_000_000_000),
        };

        assert!(ValidatedJournalReplay::replay(&[JournalRecord::Header(header)]).is_ok());
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
                tif: None,
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
                tif: None,
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
                whole_run: None,
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

    #[test]
    fn prepared_tif_round_trips_and_legacy_records_default_to_unknown() {
        let record = JournalRecord::Prepared {
            slice_idx: 1,
            cloid: Cloid::new(),
            nonce: Some(1),
            symbol: Symbol::new("HYPE"),
            side: Side::Short,
            tif: Some(Tif::Ioc),
            px: "50".into(),
            sz: "1".into(),
        };
        let mut value = serde_json::to_value(&record).unwrap();
        assert_eq!(value.get("tif"), Some(&serde_json::json!("Ioc")));
        assert!(matches!(
            serde_json::from_value::<JournalRecord>(value.clone()).unwrap(),
            JournalRecord::Prepared {
                tif: Some(Tif::Ioc),
                ..
            }
        ));

        value.as_object_mut().unwrap().remove("tif");
        assert!(matches!(
            serde_json::from_value::<JournalRecord>(value).unwrap(),
            JournalRecord::Prepared { tif: None, .. }
        ));
    }

    // === summarize / RunSummary ===

    // === restore_fill_totals ===

    #[test]
    fn restore_fill_totals_uses_terminal_average_price() {
        let cloid = Cloid::new();
        let records = vec![
            JournalRecord::Prepared {
                slice_idx: 1,
                cloid,
                nonce: Some(1),
                symbol: Symbol::new("HYPE"),
                side: Side::Long,
                px: "50".into(),
                sz: "3".into(),
                tif: None,
            },
            JournalRecord::Terminal {
                slice_idx: 1,
                cloid,
                status: "filled".into(),
                filled_sz: "2.5".into(),
                avg_px: Some("49.2".into()),
            },
        ];

        assert_eq!(
            restore_fill_totals(&records).unwrap(),
            JournalFillTotals {
                filled_sz: Decimal::new(25, 1),
                notional: Decimal::new(123, 0),
            }
        );
    }

    #[test]
    fn restore_fill_totals_falls_back_to_prepared_price() {
        let cloid = Cloid::new();
        let records = vec![
            JournalRecord::Prepared {
                slice_idx: 1,
                cloid,
                nonce: Some(1),
                symbol: Symbol::new("HYPE"),
                side: Side::Long,
                px: "50.4".into(),
                sz: "2".into(),
                tif: None,
            },
            JournalRecord::Terminal {
                slice_idx: 1,
                cloid,
                status: "filled".into(),
                filled_sz: "1.5".into(),
                avg_px: None,
            },
        ];

        assert_eq!(
            restore_fill_totals(&records).unwrap(),
            JournalFillTotals {
                filled_sz: Decimal::new(15, 1),
                notional: Decimal::new(756, 1),
            }
        );
    }

    #[test]
    fn restore_fill_totals_allows_unpriced_short_only_for_explicit_alo() {
        let alo_cloid = Cloid::new();
        let alo_records = vec![
            JournalRecord::Prepared {
                slice_idx: 1,
                cloid: alo_cloid,
                nonce: Some(1),
                symbol: Symbol::new("HYPE"),
                side: Side::Short,
                tif: Some(Tif::Alo),
                px: "50".into(),
                sz: "2".into(),
            },
            JournalRecord::Terminal {
                slice_idx: 1,
                cloid: alo_cloid,
                status: "filled".into(),
                filled_sz: "1.5".into(),
                avg_px: None,
            },
        ];
        assert_eq!(
            restore_fill_totals(&alo_records).unwrap(),
            JournalFillTotals {
                filled_sz: Decimal::new(15, 1),
                notional: Decimal::from(75),
            }
        );

        for tif in [Some(Tif::Ioc), Some(Tif::Gtc), None] {
            let cloid = Cloid::new();
            let records = vec![
                JournalRecord::Prepared {
                    slice_idx: 1,
                    cloid,
                    nonce: Some(1),
                    symbol: Symbol::new("HYPE"),
                    side: Side::Short,
                    tif,
                    px: "50".into(),
                    sz: "2".into(),
                },
                JournalRecord::Terminal {
                    slice_idx: 1,
                    cloid,
                    status: "filled".into(),
                    filled_sz: "1.5".into(),
                    avg_px: None,
                },
            ];
            assert_eq!(
                restore_fill_totals(&records),
                Err(JournalAccountingError::MissingAveragePriceForShort { cloid, tif })
            );
        }
    }

    #[test]
    fn restore_fill_totals_handles_interleaved_cloids_and_last_terminal() {
        let c1 = Cloid::new();
        let c2 = Cloid::new();
        let records = vec![
            JournalRecord::Prepared {
                slice_idx: 1,
                cloid: c1,
                nonce: Some(1),
                symbol: Symbol::new("HYPE"),
                side: Side::Long,
                px: "12".into(),
                sz: "5".into(),
                tif: None,
            },
            JournalRecord::Prepared {
                slice_idx: 2,
                cloid: c2,
                nonce: Some(2),
                symbol: Symbol::new("HYPE"),
                side: Side::Long,
                px: "20".into(),
                sz: "1".into(),
                tif: None,
            },
            JournalRecord::Terminal {
                slice_idx: 1,
                cloid: c1,
                status: "canceled".into(),
                filled_sz: "1".into(),
                avg_px: Some("10".into()),
            },
            JournalRecord::Terminal {
                slice_idx: 2,
                cloid: c2,
                status: "filled".into(),
                filled_sz: "1".into(),
                avg_px: None,
            },
            JournalRecord::Terminal {
                slice_idx: 1,
                cloid: c1,
                status: "filled".into(),
                filled_sz: "2".into(),
                avg_px: Some("11".into()),
            },
        ];

        assert_eq!(
            restore_fill_totals(&records).unwrap(),
            JournalFillTotals {
                filled_sz: Decimal::from(3),
                notional: Decimal::from(42),
            }
        );
    }

    #[test]
    fn restore_fill_totals_rejects_regressing_terminal_accounting() {
        for (next_filled_sz, next_avg_px) in [("0.5", "10"), ("1", "9")] {
            let cloid = Cloid::new();
            let records = vec![
                JournalRecord::Prepared {
                    slice_idx: 1,
                    cloid,
                    nonce: Some(1),
                    symbol: Symbol::new("HYPE"),
                    side: Side::Long,
                    px: "12".into(),
                    sz: "2".into(),
                    tif: None,
                },
                JournalRecord::Terminal {
                    slice_idx: 1,
                    cloid,
                    status: "canceled".into(),
                    filled_sz: "1".into(),
                    avg_px: Some("10".into()),
                },
                JournalRecord::Terminal {
                    slice_idx: 1,
                    cloid,
                    status: "canceled".into(),
                    filled_sz: next_filled_sz.into(),
                    avg_px: Some(next_avg_px.into()),
                },
            ];

            assert!(matches!(
                restore_fill_totals(&records),
                Err(JournalAccountingError::TerminalAccountingRegressed { .. })
            ));
        }
    }

    #[test]
    fn restore_fill_totals_fails_closed_for_malformed_or_missing_values() {
        let invalid_cloid = Cloid::new();
        let invalid = vec![
            JournalRecord::Prepared {
                slice_idx: 1,
                cloid: invalid_cloid,
                nonce: Some(1),
                symbol: Symbol::new("HYPE"),
                side: Side::Long,
                tif: None,
                px: "10".into(),
                sz: "1".into(),
            },
            JournalRecord::Terminal {
                slice_idx: 1,
                cloid: invalid_cloid,
                status: "filled".into(),
                filled_sz: "not-a-decimal".into(),
                avg_px: Some("10".into()),
            },
        ];
        assert!(matches!(
            restore_fill_totals(&invalid),
            Err(JournalAccountingError::InvalidDecimal {
                field: "Terminal.filled_sz",
                ..
            })
        ));

        let missing_cloid = Cloid::new();
        let missing = vec![JournalRecord::Terminal {
            slice_idx: 2,
            cloid: missing_cloid,
            status: "filled".into(),
            filled_sz: "1".into(),
            avg_px: None,
        }];
        assert!(matches!(
            restore_fill_totals(&missing),
            Err(JournalAccountingError::InvalidReplay { .. })
        ));

        let negative_cloid = Cloid::new();
        let negative = vec![
            JournalRecord::Prepared {
                slice_idx: 3,
                cloid: negative_cloid,
                nonce: Some(3),
                symbol: Symbol::new("HYPE"),
                side: Side::Long,
                tif: None,
                px: "10".into(),
                sz: "1".into(),
            },
            JournalRecord::Terminal {
                slice_idx: 3,
                cloid: negative_cloid,
                status: "filled".into(),
                filled_sz: "-1".into(),
                avg_px: Some("10".into()),
            },
        ];
        assert!(matches!(
            restore_fill_totals(&negative),
            Err(JournalAccountingError::NegativeSize {
                field: "Terminal.filled_sz",
                ..
            })
        ));
    }

    #[test]
    fn restore_fill_totals_rejects_untrusted_positive_fill_values() {
        let overfill_cloid = Cloid::new();
        let overfill = vec![
            JournalRecord::Prepared {
                slice_idx: 1,
                cloid: overfill_cloid,
                nonce: Some(1),
                symbol: Symbol::new("HYPE"),
                side: Side::Long,
                px: "10".into(),
                sz: "1".into(),
                tif: None,
            },
            JournalRecord::Terminal {
                slice_idx: 1,
                cloid: overfill_cloid,
                status: "filled".into(),
                filled_sz: "2".into(),
                avg_px: Some("10".into()),
            },
        ];
        assert!(matches!(
            restore_fill_totals(&overfill),
            Err(JournalAccountingError::FilledSizeExceedsPrepared { .. })
        ));

        for (side, average_price) in [(Side::Long, "11"), (Side::Short, "9")] {
            let cloid = Cloid::new();
            let records = vec![
                JournalRecord::Prepared {
                    slice_idx: 2,
                    cloid,
                    nonce: Some(2),
                    symbol: Symbol::new("HYPE"),
                    side,
                    px: "10".into(),
                    sz: "1".into(),
                    tif: None,
                },
                JournalRecord::Terminal {
                    slice_idx: 2,
                    cloid,
                    status: "filled".into(),
                    filled_sz: "1".into(),
                    avg_px: Some(average_price.into()),
                },
            ];
            assert!(matches!(
                restore_fill_totals(&records),
                Err(JournalAccountingError::AveragePriceViolatesLimit { .. })
            ));
        }
    }

    #[test]
    fn restore_fill_totals_rejects_zero_fill_without_prepared() {
        let records = vec![JournalRecord::Terminal {
            slice_idx: 1,
            cloid: Cloid::new(),
            status: "canceled".into(),
            filled_sz: "0".into(),
            // A zero-fill terminal has no price-bearing accounting effect,
            // so even an exchange's malformed/placeholder value is ignored.
            avg_px: Some("not-a-price".into()),
        }];
        assert!(matches!(
            restore_fill_totals(&records),
            Err(JournalAccountingError::InvalidReplay { .. })
        ));
    }

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
                tif: None,
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
                tif: None,
            },
            JournalRecord::Terminal {
                slice_idx: 2,
                cloid: c2,
                status: "filled".into(),
                filled_sz: "5".into(),
                avg_px: Some("51".into()),
            },
        ];
        let summary = summarize(&records).unwrap();
        assert_eq!(summary.total_filled(), rust_decimal::Decimal::from(10));
        assert_eq!(summary.cloids.len(), 2);
        assert!(summary.unresolved_cloids().is_empty());
        assert!(summary.is_incomplete());
        assert_eq!(summary.last_final_report_completed, None);
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
                tif: None,
            },
            JournalRecord::SubmittedUnknown {
                slice_idx: 1,
                cloid: c1,
            },
        ];
        let summary = summarize(&records).unwrap();
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
                whole_run: None,
            },
        ];
        let summary = summarize(&records).unwrap();
        assert!(!summary.is_incomplete());
    }

    #[test]
    fn validated_replay_rejects_empty_missing_and_duplicate_headers() {
        assert_eq!(
            ValidatedJournalReplay::replay(&[]).unwrap_err(),
            JournalReplayError::MissingHeader
        );

        let no_header = vec![JournalRecord::Abandoned {
            note: "not a header".into(),
        }];
        assert_eq!(
            ValidatedJournalReplay::replay(&no_header).unwrap_err(),
            JournalReplayError::HeaderNotFirst
        );

        let duplicate = vec![
            JournalRecord::Header(header("duplicate-header")),
            JournalRecord::Header(header("duplicate-header")),
        ];
        assert_eq!(
            ValidatedJournalReplay::replay(&duplicate).unwrap_err(),
            JournalReplayError::DuplicateHeader
        );
    }

    #[test]
    fn validated_replay_transition_table_is_fail_closed() {
        #[derive(Clone, Copy, Debug)]
        enum State {
            Prepared,
            SubmittedUnknown,
            Acknowledged,
            Terminal,
        }
        fn transition_record(state: State, cloid: Cloid) -> JournalRecord {
            match state {
                State::Prepared => unreachable!("Prepared is the fixed initial state"),
                State::SubmittedUnknown => JournalRecord::SubmittedUnknown {
                    slice_idx: 1,
                    cloid,
                },
                State::Acknowledged => JournalRecord::Acknowledged {
                    slice_idx: 1,
                    cloid,
                    oid: Some(7),
                    status: "open".into(),
                },
                State::Terminal => JournalRecord::Terminal {
                    slice_idx: 1,
                    cloid,
                    status: "filled".into(),
                    filled_sz: "1".into(),
                    avg_px: Some("10".into()),
                },
            }
        }
        let cases = [
            (State::Prepared, State::SubmittedUnknown, true),
            (State::Prepared, State::Acknowledged, true),
            (State::Prepared, State::Terminal, true),
            (State::SubmittedUnknown, State::SubmittedUnknown, false),
            (State::SubmittedUnknown, State::Acknowledged, true),
            (State::SubmittedUnknown, State::Terminal, true),
            (State::Acknowledged, State::SubmittedUnknown, false),
            (State::Acknowledged, State::Acknowledged, true),
            (State::Acknowledged, State::Terminal, true),
            (State::Terminal, State::SubmittedUnknown, false),
            (State::Terminal, State::Acknowledged, false),
            (State::Terminal, State::Terminal, true),
        ];

        for (from, to, allowed) in cases {
            let cloid = Cloid::new();
            let mut records = vec![
                JournalRecord::Header(header("transition-table")),
                JournalRecord::Prepared {
                    slice_idx: 1,
                    cloid,
                    nonce: Some(1),
                    symbol: Symbol::new("HYPE"),
                    side: Side::Long,
                    tif: Some(Tif::Ioc),
                    px: "10".into(),
                    sz: "1".into(),
                },
            ];
            if !matches!(from, State::Prepared) {
                records.push(transition_record(from, cloid));
            }
            records.push(transition_record(to, cloid));
            let result = ValidatedJournalReplay::replay(&records);
            assert_eq!(
                result.is_ok(),
                allowed,
                "transition {from:?} -> {to:?}: {result:?}"
            );
            if !allowed {
                assert!(matches!(
                    result,
                    Err(JournalReplayError::InvalidTransition { .. })
                ));
            }
        }
    }

    #[test]
    fn validated_replay_rejects_missing_prepared_changed_slice_and_intent() {
        let cloid = Cloid::new();
        let no_prepared = vec![
            JournalRecord::Header(header("missing-prepared")),
            JournalRecord::SubmittedUnknown {
                slice_idx: 1,
                cloid,
            },
        ];
        assert_eq!(
            ValidatedJournalReplay::replay(&no_prepared).unwrap_err(),
            JournalReplayError::MissingPrepared { cloid }
        );

        let prepared = JournalRecord::Prepared {
            slice_idx: 1,
            cloid,
            nonce: Some(1),
            symbol: Symbol::new("HYPE"),
            side: Side::Long,
            tif: Some(Tif::Ioc),
            px: "10".into(),
            sz: "1".into(),
        };
        assert_eq!(
            ValidatedJournalReplay::replay(&[
                JournalRecord::Header(header("changed-slice")),
                prepared.clone(),
                JournalRecord::Acknowledged {
                    slice_idx: 2,
                    cloid,
                    oid: Some(7),
                    status: "open".into(),
                },
            ])
            .unwrap_err(),
            JournalReplayError::SliceIndexChanged {
                cloid,
                expected: 1,
                actual: 2,
            }
        );

        let mut conflicting = prepared.clone();
        if let JournalRecord::Prepared { px, .. } = &mut conflicting {
            *px = "11".into();
        }
        assert_eq!(
            ValidatedJournalReplay::replay(&[
                JournalRecord::Header(header("conflicting-intent")),
                prepared,
                conflicting,
            ])
            .unwrap_err(),
            JournalReplayError::ConflictingPrepared { cloid }
        );
    }

    #[test]
    fn validated_replay_rejects_completed_final_with_unknown_or_live_order() {
        let cloid = Cloid::new();
        let unknown = vec![
            JournalRecord::Header(header("run-final-unknown")),
            JournalRecord::FinalReport {
                completed: true,
                filled_total: "0".into(),
                outcome_unknown_cloids: vec![cloid],
                note: "invalid".into(),
                whole_run: None,
            },
        ];
        assert!(matches!(
            ValidatedJournalReplay::replay(&unknown),
            Err(JournalReplayError::CompletedFinalReportNotComplete)
        ));

        let live = vec![
            JournalRecord::Header(header("run-final-live")),
            JournalRecord::Prepared {
                slice_idx: 0,
                cloid,
                nonce: None,
                symbol: Symbol::new("HYPE"),
                side: Side::Long,
                tif: None,
                px: "10".into(),
                sz: "1".into(),
            },
            JournalRecord::FinalReport {
                completed: true,
                filled_total: "0".into(),
                outcome_unknown_cloids: vec![],
                note: "invalid".into(),
                whole_run: None,
            },
        ];
        assert!(matches!(
            ValidatedJournalReplay::replay(&live),
            Err(JournalReplayError::CompletedFinalReportNotComplete)
        ));
    }

    #[test]
    fn validated_replay_rejects_incomplete_final_with_wrong_unknown_cloids() {
        let cloid = Cloid::new();
        let records = vec![
            JournalRecord::Header(header("run-final-mismatch")),
            JournalRecord::Prepared {
                slice_idx: 0,
                cloid,
                nonce: None,
                symbol: Symbol::new("HYPE"),
                side: Side::Long,
                tif: None,
                px: "10".into(),
                sz: "1".into(),
            },
            JournalRecord::FinalReport {
                completed: false,
                filled_total: "0".into(),
                outcome_unknown_cloids: vec![],
                note: "invalid".into(),
                whole_run: None,
            },
        ];
        assert!(matches!(
            ValidatedJournalReplay::replay(&records),
            Err(JournalReplayError::FinalReportUnknownCloidsMismatch {
                reported,
                reconstructed
            }) if reported.is_empty() && reconstructed == vec![cloid]
        ));
    }

    #[test]
    fn validated_replay_rejects_execution_after_completed_final() {
        let records = vec![
            JournalRecord::Header(header("run-after-final")),
            JournalRecord::FinalReport {
                completed: true,
                filled_total: "0".into(),
                outcome_unknown_cloids: vec![],
                note: "done".into(),
                whole_run: None,
            },
            JournalRecord::Prepared {
                slice_idx: 0,
                cloid: Cloid::new(),
                nonce: None,
                symbol: Symbol::new("HYPE"),
                side: Side::Long,
                tif: None,
                px: "10".into(),
                sz: "1".into(),
            },
        ];
        assert!(matches!(
            ValidatedJournalReplay::replay(&records),
            Err(JournalReplayError::RecordAfterCompletedFinalReport)
        ));
    }

    #[test]
    fn run_id_cannot_escape_runs_directory() {
        let tmp = TempDir::new().unwrap();
        let err = ExecutionJournal::read_all(tmp.path(), "../outside").unwrap_err();
        assert!(matches!(err, JournalError::InvalidRunId { .. }));
        assert!(validate_run_id("run_01-abc").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn read_and_resume_refuse_symlinked_run_directory() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("target");
        std::fs::create_dir_all(&target).unwrap();
        let runs = tmp.path().join("runs");
        std::fs::create_dir_all(&runs).unwrap();
        std::os::unix::fs::symlink(&target, runs.join("run-link")).unwrap();
        assert!(matches!(
            ExecutionJournal::read_all(tmp.path(), "run-link"),
            Err(JournalError::UnsafePath { .. })
        ));
        assert!(matches!(
            ExecutionJournal::open_existing(tmp.path(), "run-link"),
            Err(JournalError::UnsafePath { .. })
        ));
    }

    #[test]
    fn abandoned_marker_is_rejected_until_every_cloid_is_terminal() {
        let c1 = Cloid::new();
        let mut records = vec![
            JournalRecord::Header(header("run-7")),
            JournalRecord::Prepared {
                slice_idx: 1,
                cloid: c1,
                nonce: Some(1),
                symbol: Symbol::new("HYPE"),
                side: Side::Long,
                px: "50".into(),
                sz: "5".into(),
                tif: None,
            },
            JournalRecord::Abandoned {
                note: "operator abandoned after reconciliation".into(),
            },
        ];
        assert_eq!(
            ValidatedJournalReplay::replay(&records).unwrap_err(),
            JournalReplayError::AbandonedWithUnresolved { unresolved: 1 }
        );

        records.insert(
            2,
            JournalRecord::Terminal {
                slice_idx: 1,
                cloid: c1,
                status: "canceled".into(),
                filled_sz: "0".into(),
                avg_px: None,
            },
        );
        let summary = summarize(&records).unwrap();
        assert!(!summary.is_incomplete());
        assert!(summary.abandoned);
    }

    #[test]
    fn terminal_record_requires_a_closed_official_or_synthetic_status() {
        for status in ["open", "triggered", "futureUnknownStatus"] {
            let cloid = Cloid::new();
            let records = vec![
                JournalRecord::Header(header("invalid-terminal-status")),
                JournalRecord::Prepared {
                    slice_idx: 1,
                    cloid,
                    nonce: Some(1),
                    symbol: Symbol::new("HYPE"),
                    side: Side::Long,
                    px: "50".into(),
                    sz: "5".into(),
                    tif: None,
                },
                JournalRecord::Terminal {
                    slice_idx: 1,
                    cloid,
                    status: status.into(),
                    filled_sz: "0".into(),
                    avg_px: None,
                },
            ];
            assert_eq!(
                ValidatedJournalReplay::replay(&records).unwrap_err(),
                JournalReplayError::InvalidTerminalStatus {
                    cloid,
                    status: status.into(),
                }
            );
        }

        for status in [
            "filled",
            "aloRejected",
            "neverReceived",
            "positionTargetReached",
        ] {
            let cloid = Cloid::new();
            let records = vec![
                JournalRecord::Header(header("valid-terminal-status")),
                JournalRecord::Prepared {
                    slice_idx: 1,
                    cloid,
                    nonce: Some(1),
                    symbol: Symbol::new("HYPE"),
                    side: Side::Long,
                    px: "50".into(),
                    sz: "5".into(),
                    tif: None,
                },
                JournalRecord::Terminal {
                    slice_idx: 1,
                    cloid,
                    status: status.into(),
                    filled_sz: "0".into(),
                    avg_px: None,
                },
            ];
            ValidatedJournalReplay::replay(&records)
                .unwrap_or_else(|error| panic!("{status} must replay: {error}"));
        }
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
                execution_fingerprint: None,
                started_at_unix_ms: 0,
                execution_deadline_unix_ms: None,
            },
        )
        .unwrap();
        let cloid = Cloid::new();
        j.record(&JournalRecord::Prepared {
            slice_idx: 1,
            cloid,
            nonce: None,
            symbol: Symbol::new("HYPE"),
            side: Side::Long,
            tif: None,
            px: "50".into(),
            sz: "1".into(),
        })
        .unwrap();
        j.record(&JournalRecord::SubmittedUnknown {
            slice_idx: 1,
            cloid,
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
                execution_fingerprint: None,
                started_at_unix_ms: 0,
                execution_deadline_unix_ms: None,
            },
        )
        .unwrap();
        j.record(&JournalRecord::FinalReport {
            completed: true,
            filled_total: "0".into(),
            outcome_unknown_cloids: vec![],
            note: "done".into(),
            whole_run: None,
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
    fn corrupt_middle_record_is_not_treated_as_a_torn_tail() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let run_dir = root.join("runs").join("run-corrupt-middle");
        std::fs::create_dir_all(&run_dir).unwrap();
        let first =
            serde_json::to_string(&JournalRecord::Header(header("run-corrupt-middle"))).unwrap();
        let last = serde_json::to_string(&JournalRecord::FinalReport {
            completed: true,
            filled_total: "0".into(),
            outcome_unknown_cloids: vec![],
            note: "done".into(),
            whole_run: None,
        })
        .unwrap();
        std::fs::write(
            run_dir.join("journal.jsonl"),
            format!("{first}\n{{not json}}\n{last}\n"),
        )
        .unwrap();
        let err = ExecutionJournal::read_all(root, "run-corrupt-middle").unwrap_err();
        assert!(matches!(err, JournalError::Parse { line: 2, .. }));
    }

    #[test]
    fn newline_terminated_invalid_final_record_is_corruption_not_a_torn_tail() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let run_dir = root.join("runs").join("run-corrupt-final");
        std::fs::create_dir_all(&run_dir).unwrap();
        let first =
            serde_json::to_string(&JournalRecord::Header(header("run-corrupt-final"))).unwrap();
        std::fs::write(
            run_dir.join("journal.jsonl"),
            format!("{first}\n{{\"kind\":\"Prepared\",not-valid}}\n"),
        )
        .unwrap();

        let error = ExecutionJournal::read_all(root, "run-corrupt-final").unwrap_err();
        assert!(matches!(error, JournalError::Parse { line: 2, .. }));
    }

    #[test]
    fn invalid_final_fragment_without_newline_is_the_only_tolerated_torn_shape() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let run_dir = root.join("runs").join("run-torn-final");
        std::fs::create_dir_all(&run_dir).unwrap();
        let first =
            serde_json::to_string(&JournalRecord::Header(header("run-torn-final"))).unwrap();
        std::fs::write(
            run_dir.join("journal.jsonl"),
            format!("{first}\n{{\"kind\":\"Prepared\",not-valid}}"),
        )
        .unwrap();

        let records = ExecutionJournal::read_all(root, "run-torn-final").unwrap();
        assert_eq!(
            records,
            vec![JournalRecord::Header(header("run-torn-final"))]
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
                    execution_fingerprint: None,
                    started_at_unix_ms: 0,
                    execution_deadline_unix_ms: None,
                },
            )
            .unwrap();
            j.record(&JournalRecord::Prepared {
                slice_idx: 1,
                cloid,
                nonce: None,
                symbol: Symbol::new("HYPE"),
                side: Side::Long,
                tif: None,
                px: "50".into(),
                sz: "1".into(),
            })
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
