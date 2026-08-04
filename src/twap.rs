//! TWAP slice loop (§8).
//!
//! Slice arithmetic is factored into pure functions (`per_slice_size`,
//! `slice_order_size`, `SliceDecision`) so the catch-up / remainder /
//! min-notional behaviour is unit-testable without any I/O.
//!
//! Key invariants (§8):
//! - `target_at_slice(i) = per_slice * i`, except the final slice which uses
//!   `total_adjusted` so rounding remainder is absorbed exactly once.
//! - `order_sz = round_down(target_at_slice - filled_so_far)` — a partially
//!   filled earlier slice is caught up, never double-ordered.
//! - A slice whose notional is under `MIN_NOTIONAL_USD` is SKIPPED and its
//!   quantity carries into the next slice (the target is cumulative, so the
//!   carry is automatic). On the final slice this is a warning, not an error:
//!   the residual is simply unexecutable.

use std::time::Duration;

use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use crate::api::HlApi;
use crate::client::{OrderStatusFill, PlaceOutcome, ValidatedFill, ValidatedMarketSnapshot};
use crate::errors::{HlError, RejectionKind};
use crate::format::{human, round_size, taker_limit_price};
use crate::journal::{ExecutionJournal, JournalRecord};
use crate::risk::RiskEnvelope;
use crate::types::{Address, CancelIntent, Cloid, OrderIntent, Side, Symbol, Tif};

/// HL's practical minimum order notional in USD (§8).
pub const MIN_NOTIONAL_USD: Decimal = dec!(10);

/// Safety margin applied to the min-notional gate (T1).
///
/// The gate is evaluated on the price we are about to sign, but HL evaluates
/// the rejection on its own book at receipt time. A notional sitting exactly on
/// $10.00 is one tick of adverse movement away from `MinTradeNtl`, which is a
/// FATAL rejection that stops the whole run. Requiring 1% of headroom converts
/// that hard stop into a cheap skip-and-carry.
const MIN_NOTIONAL_MARGIN: Decimal = dec!(1.01);

/// The notional a slice must clear to be sent (T1).
pub fn min_notional_gate() -> Decimal {
    MIN_NOTIONAL_USD * MIN_NOTIONAL_MARGIN
}

/// Stale-book retries before hard-stopping a slice (§8).
const STALE_BOOK_RETRIES: u32 = 3;
const STALE_BOOK_RETRY_INTERVAL: Duration = Duration::from_secs(1);

/// Retries when recovering a resting order's fill via `orderStatus` (§8, T3).
///
/// This is UNRELATED to the W1 unknownOid resend policy below — a resting
/// order is known to exist (HL gave us its oid), so this loop is purely
/// "keep asking until the status is terminal," with no safe-resend decision
/// involved.
const ORDER_STATUS_RETRIES: u32 = 3;
const ORDER_STATUS_RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// W1 unknownOid safe-resend policy (Issue #7, PM-decided, binding).
///
/// HL's `orderStatus` carries no documented read-after-write guarantee
/// immediately after `/exchange` (see docs/DEVELOPMENT.md). A single
/// `unknownOid` observation is therefore NOT proof the order never landed —
/// it may simply not be visible yet. Treating the first `unknownOid` as
/// definitive (the pre-Issue-#7 behaviour: 3 retries × 500ms = 1.5s) risked a
/// resend racing the order's own propagation, which would double-place it.
///
/// The tightened policy requires BOTH:
/// - at least [`UNKNOWN_OID_MIN_CONSECUTIVE`] consecutive `unknownOid`
///   responses (no live/terminal observation in between — a single non-
///   unknownOid response resets the streak and aborts outcome-unknown), AND
/// - at least [`UNKNOWN_OID_MIN_WINDOW`] of wall-clock time elapsed between
///   the FIRST and LAST `unknownOid` observation in that streak.
///
/// Anything else — fewer than the minimum consecutive observations, a mixed
/// sequence (an `unknownOid` followed by a live/terminal response), or
/// exhausting the retry budget before the window closes — aborts
/// outcome-unknown (`Err`), which `run_twap` maps to a hard stop (exit 1).
const UNKNOWN_OID_MIN_CONSECUTIVE: u32 = 3;
const UNKNOWN_OID_MIN_WINDOW: Duration = Duration::from_secs(2);

/// Poll interval while accumulating the unknownOid streak.
///
/// `UNKNOWN_OID_MIN_CONSECUTIVE` observations span `MIN_CONSECUTIVE - 1`
/// intervals (the elapsed window is measured from the FIRST observation to
/// the LAST, not from the start of polling). With 3 observations spaced
/// 1100ms apart that is 2 × 1100ms = 2.2s, comfortably clearing the 2s
/// window rather than sitting exactly on the boundary.
const UNKNOWN_OID_POLL_INTERVAL: Duration = Duration::from_millis(1100);

/// Hard cap on how long `reconcile_by_cloid` may keep polling before giving
/// up even if the streak has not resolved either way. Generous relative to
/// `UNKNOWN_OID_MIN_WINDOW` so a healthy 2s-window resend is never starved,
/// while still bounding the worst case.
const UNKNOWN_OID_MAX_ATTEMPTS: u32 = 8;

pub const READ_ONLY_BANNER: &str = "=== READ-ONLY MODE: NO ORDERS ARE SENT ===";

/// The absolute execution-phase deadline for a TWAP run (Issue #2).
///
/// This is DELIBERATELY a different concept from the `--expire-after`
/// WAIT-phase cutoff in `src/trigger.rs` (Issue #8): that flag bounds how
/// long the tool will wait for a trigger to fire BEFORE any slice is placed
/// (and aborts with nothing sent if it elapses). `ExecutionDeadline` bounds
/// the slice loop itself, AFTER the trigger has already fired — it is the
/// enforcement mechanism behind the `--duration` hard-window invariant
/// (`docs/DESIGN.md` "執行ウィンドウは厳格"). They are constructed at
/// different times from different clocks-of-record and must never be
/// conflated.
///
/// Two clocks are tracked, for two different purposes:
/// - `monotonic`: a [`tokio::time::Instant`] used for every LOCAL decision —
///   "may I still place / resend?", "how much retry budget is left?". Never
///   affected by wall-clock adjustments (NTP step, DST, operator clock
///   changes), which is exactly why it is the one that gates local control
///   flow.
/// - `expires_after_ms`: the wall-clock Unix ms sent to Hyperliquid, both in
///   the signed action hash and the `/exchange` body's `expiresAfter` field.
///   This is the EXCHANGE-side half of the hard window — HL enforces it
///   independently of whatever our local clock thinks, which is the point:
///   two independent enforcers of the same invariant, on two different
///   clocks. See `docs/DESIGN.md` for the full local-vs-server responsibility
///   split and the clock-skew fail-closed rule this pairs with.
#[derive(Debug, Clone, Copy)]
pub struct ExecutionDeadline {
    monotonic: tokio::time::Instant,
    expires_after_ms: u64,
}

impl ExecutionDeadline {
    /// Construct from the run's monotonic start and its `--duration`.
    ///
    /// `expires_after_ms` is derived from the CURRENT wall clock plus the
    /// remaining monotonic duration at construction time (i.e. call this
    /// once, at `run_twap`'s start, not per-slice — `wall_now` and `start`
    /// must be read at (approximately) the same instant for the two clocks to
    /// stay in correspondence).
    pub fn new(start: tokio::time::Instant, duration: Duration, wall_now_ms: u64) -> Self {
        let monotonic = start + duration;
        let expires_after_ms = wall_now_ms.saturating_add(duration.as_millis() as u64);
        Self {
            monotonic,
            expires_after_ms,
        }
    }

    /// Construct directly from both clock values. Exposed for tests that need
    /// to control the wall-clock expiry independently of `Instant::now`
    /// (virtual time).
    #[cfg(test)]
    pub fn from_parts(monotonic: tokio::time::Instant, expires_after_ms: u64) -> Self {
        Self {
            monotonic,
            expires_after_ms,
        }
    }

    /// The wall-clock Unix ms to sign into every order this run sends
    /// (`expiresAfter`, both in the action hash and the `/exchange` body).
    /// Constant for the whole run — a resend reuses this exact value (PM
    /// decision), it does NOT get a fresh expiry.
    pub fn expires_after_ms(&self) -> u64 {
        self.expires_after_ms
    }

    /// Monotonic time remaining until the deadline. `Duration::ZERO` once it
    /// has passed (never negative).
    pub fn remaining(&self, now: tokio::time::Instant) -> Duration {
        self.monotonic.saturating_duration_since(now)
    }

    /// True once the monotonic deadline has passed.
    pub fn has_passed(&self, now: tokio::time::Instant) -> bool {
        now >= self.monotonic
    }

    /// Re-check immediately before a place OR a resend (Issue #2 PM
    /// decision). `Err` means: do not place, do not resend — status queries
    /// and cancels remain allowed, but this call site must stop here.
    pub fn check_before_send(&self, now: tokio::time::Instant) -> Result<(), HlError> {
        if self.has_passed(now) {
            Err(HlError::InvalidResponse(format!(
                "execution deadline elapsed ({}ms ago); refusing to place or resend \
                 (status queries and cancels remain allowed)",
                now.saturating_duration_since(self.monotonic).as_millis()
            )))
        } else {
            Ok(())
        }
    }
}

/// Current wall-clock time as Unix ms. Small wrapper so call sites don't
/// repeat the `SystemTime` dance and so tests can see the one place this is
/// read from.
pub fn wall_clock_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// Maximum tolerated |local wall clock − HL server clock| before a LIVE
/// preflight fails closed (Issue #2 PM decision). Only enforced for live
/// (non-read-only, non-paper) runs — see `docs/DESIGN.md` "クロックずれ" and
/// `docs/DEVELOPMENT.md` for the NTP dependency this implies operationally.
pub const MAX_CLOCK_SKEW_MS: i64 = 5_000;

/// Compare local wall-clock time against an `l2Book` response's server
/// timestamp (Issue #2). Called ONLY at live preflight, never in read-only /
/// paper mode — a dry run signs nothing, so a bad local clock cannot corrupt
/// an `expiresAfter` that is never sent.
///
/// `expiresAfter` is a wall-clock Unix ms value trusted by BOTH the local
/// `ExecutionDeadline` and Hyperliquid's own exchange-side enforcement of the
/// same field. If the operator's clock is skewed against HL's by more than
/// [`MAX_CLOCK_SKEW_MS`], the locally-computed `expiresAfter` could expire
/// far earlier or later than intended relative to HL's own clock — silently
/// shortening or (worse) extending the effective hard window. Failing closed
/// here, rather than silently trusting a skewed clock, is the whole point of
/// signing a wall-clock expiry at all.
pub fn check_clock_skew(local_wall_now_ms: i64, server_ts_ms: i64) -> Result<(), HlError> {
    let skew_ms = local_wall_now_ms - server_ts_ms;
    if skew_ms.abs() > MAX_CLOCK_SKEW_MS {
        return Err(HlError::InvalidConfig(format!(
            "local clock is skewed {skew_ms}ms from the Hyperliquid server clock \
             (tolerance ±{MAX_CLOCK_SKEW_MS}ms); refusing to proceed in live mode — \
             the signed expiresAfter would not mean what this run expects it to. \
             Check NTP sync on this host (see docs/DEVELOPMENT.md)."
        )));
    }
    Ok(())
}

/// Everything the loop needs, resolved before the first slice.
pub struct TwapPlan {
    pub symbol: Symbol,
    pub side: Side,
    pub asset_index: u32,
    pub sz_decimals: u32,
    /// Rounded size of every non-final slice.
    pub per_slice: Decimal,
    /// `per_slice * slices` — the size the run actually targets.
    pub total_adjusted: Decimal,
    /// The size originally requested (pre-rounding), for the report.
    pub total_requested: Decimal,
    pub slices: u32,
    pub duration: Duration,
    pub slippage_bps: Decimal,
    pub max_book_age_ms: u64,
    pub read_only: bool,
    /// Issue #3: the notional cap resolved before the run started —
    /// required in live mode, `Decimal::MAX` (effectively unbounded) in
    /// read-only. Re-checked before EVERY slice against that slice's actual
    /// order price, via [`RiskEnvelope::check_notional_cap`] — the SAME
    /// function (and constants module) the CLI pre-flight check uses, so the
    /// policy cannot drift between the two call sites.
    pub max_notional_usd: Decimal,
    /// Agent (API wallet) address — the key that signs. `None` in read-only.
    pub agent: Option<Address>,
    /// MASTER account address, resolved by the `userRole` probe at startup
    /// (F1). This is the `user` for every `orderStatus` query: HL books an
    /// agent's orders under its master, so querying with the agent address
    /// returns `unknownOid` for orders that really exist. `None` in read-only,
    /// where no order is ever placed and nothing needs recovering.
    pub master: Option<Address>,
}

impl TwapPlan {
    /// The address `orderStatus` must be queried as (F1): the MASTER.
    fn status_user(&self) -> Result<&Address, HlError> {
        self.master.as_ref().ok_or_else(|| {
            HlError::InvalidConfig(
                "orderStatus requires the master address (userRole probe did not run)".into(),
            )
        })
    }
}

/// Pre-flight sizing errors (§8).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PreflightError {
    #[error("per-slice size rounds to zero at szDecimals={sz_decimals} (total {total} / {slices} slices); increase --usd/--size or reduce --slices")]
    PerSliceZero {
        total: Decimal,
        slices: u32,
        sz_decimals: u32,
    },
    #[error("per-slice notional ${notional} is below the ${min} minimum; increase --usd/--size or reduce --slices")]
    PerSliceBelowMinNotional { notional: Decimal, min: Decimal },
    #[error("total size must be > 0, got {0}")]
    NonPositiveTotal(Decimal),
}

/// Result of pre-flight sizing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sizing {
    pub per_slice: Decimal,
    pub total_adjusted: Decimal,
}

/// Compute per-slice size and the adjusted total (§8 pre-flight).
///
/// `total_coin` is the requested quantity in coin units; `mid` is the
/// reference price used for the min-notional gate.
pub fn compute_sizing(
    total_coin: Decimal,
    slices: u32,
    sz_decimals: u32,
    mid: Decimal,
) -> Result<Sizing, PreflightError> {
    if total_coin <= Decimal::ZERO {
        return Err(PreflightError::NonPositiveTotal(total_coin));
    }
    let per_slice = round_size(total_coin / Decimal::from(slices), sz_decimals);
    if per_slice <= Decimal::ZERO {
        return Err(PreflightError::PerSliceZero {
            total: total_coin,
            slices,
            sz_decimals,
        });
    }
    let notional = per_slice * mid;
    if notional < MIN_NOTIONAL_USD {
        return Err(PreflightError::PerSliceBelowMinNotional {
            notional,
            min: MIN_NOTIONAL_USD,
        });
    }
    Ok(Sizing {
        per_slice,
        total_adjusted: per_slice * Decimal::from(slices),
    })
}

/// Convert a USD notional into a coin quantity at `mid` (§3 `--usd`).
pub fn usd_to_coin(usd: Decimal, mid: Decimal) -> Result<Decimal, PreflightError> {
    if mid <= Decimal::ZERO {
        return Err(PreflightError::NonPositiveTotal(mid));
    }
    Ok(usd / mid)
}

/// Cumulative target quantity after slice `i` (1-based).
///
/// The final slice targets `total_adjusted` exactly so the rounding remainder
/// is absorbed there and nowhere else.
pub fn target_at_slice(
    slice_idx: u32,
    slices: u32,
    per_slice: Decimal,
    total_adjusted: Decimal,
) -> Decimal {
    if slice_idx >= slices {
        total_adjusted
    } else {
        per_slice * Decimal::from(slice_idx)
    }
}

/// The size to order on slice `i`, after catching up for prior under-fills.
pub fn slice_order_size(
    slice_idx: u32,
    slices: u32,
    per_slice: Decimal,
    total_adjusted: Decimal,
    filled_so_far: Decimal,
    sz_decimals: u32,
) -> Decimal {
    let target = target_at_slice(slice_idx, slices, per_slice, total_adjusted);
    let raw = target - filled_so_far;
    if raw <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    round_size(raw, sz_decimals)
}

/// What the loop should do with one slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SliceDecision {
    /// Send an order of this size.
    Place(Decimal),
    /// Nothing due (already at or past target) — wait for the next deadline.
    SkipAhead,
    /// Below the min notional; carry into the next slice.
    SkipBelowMinNotional { sz: Decimal, notional: Decimal },
}

/// Decide a slice, combining catch-up sizing with the min-notional gate (§8).
///
/// T1: the gate is evaluated at `order_px` — the taker limit price we are about
/// to sign — NOT at the mid. For a SHORT the limit sits *below* the mid, so a
/// mid-based gate passes sizes whose real notional is under HL's floor; the
/// order then comes back `MinTradeNtl`, which is fatal and stops the run. The
/// price used for the gate must be the price that reaches the exchange.
pub fn decide_slice(
    slice_idx: u32,
    slices: u32,
    per_slice: Decimal,
    total_adjusted: Decimal,
    filled_so_far: Decimal,
    sz_decimals: u32,
    order_px: Decimal,
) -> SliceDecision {
    let sz = slice_order_size(
        slice_idx,
        slices,
        per_slice,
        total_adjusted,
        filled_so_far,
        sz_decimals,
    );
    if sz <= Decimal::ZERO {
        return SliceDecision::SkipAhead;
    }
    let notional = sz * order_px;
    if notional < min_notional_gate() {
        return SliceDecision::SkipBelowMinNotional { sz, notional };
    }
    SliceDecision::Place(sz)
}

/// Deadline for slice `i`: `start + duration * i / slices`.
///
/// Computed from the absolute start so per-slice scheduling error cannot
/// accumulate into drift.
pub fn slice_deadline(
    start: tokio::time::Instant,
    duration: Duration,
    slice_idx: u32,
    slices: u32,
) -> tokio::time::Instant {
    let total_nanos = duration.as_nanos();
    let share = total_nanos * u128::from(slice_idx) / u128::from(slices);
    start + Duration::from_nanos(share.min(u128::from(u64::MAX)) as u64)
}

/// Final execution report (§8).
#[derive(Debug, Clone)]
pub struct TwapReport {
    pub symbol: Symbol,
    pub side: Side,
    pub total_requested: Decimal,
    pub total_adjusted: Decimal,
    pub filled: Decimal,
    /// Size-weighted average fill price. `None` if nothing filled.
    pub avg_px: Option<Decimal>,
    pub slices_executed: u32,
    pub slices_skipped: u32,
    pub elapsed: Duration,
    pub abort_reason: Option<String>,
    pub read_only: bool,
}

impl TwapReport {
    /// Exit code: 1 only on an abort. A partial fill that ran to completion
    /// exits 0 with a warning (§8).
    pub fn exit_code(&self) -> i32 {
        if self.abort_reason.is_some() {
            1
        } else {
            0
        }
    }

    pub fn is_partial(&self) -> bool {
        self.filled < self.total_adjusted
    }

    /// Quantity the pre-flight rounding dropped before the run even started
    /// (T4): `total_requested - total_adjusted`.
    ///
    /// At `szDecimals=0` a requested 10.5 over 3 slices adjusts to 9 — filling
    /// all 9 is "complete" against the adjusted target while 14% of what the
    /// operator asked for was never in play. That gap is invisible unless the
    /// report states it, so `render` always does.
    pub fn rounding_dropped(&self) -> Option<Decimal> {
        let diff = self.total_requested - self.total_adjusted;
        if diff > Decimal::ZERO {
            Some(diff)
        } else {
            None
        }
    }

    /// Multi-line human-readable summary.
    pub fn render(&self) -> String {
        let mut s = String::new();
        s.push_str("=== TWAP report ===\n");
        if self.read_only {
            s.push_str("mode:            READ-ONLY (no orders were sent)\n");
        }
        s.push_str(&format!("symbol/side:     {} {}\n", self.symbol, self.side));
        s.push_str(&format!(
            "target:          requested {} / adjusted {}\n",
            human(self.total_requested),
            human(self.total_adjusted)
        ));
        s.push_str(&format!("filled:          {}\n", human(self.filled)));
        s.push_str(&format!(
            "avg price:       {}\n",
            self.avg_px.map(human).unwrap_or_else(|| "-".into())
        ));
        s.push_str(&format!(
            "slices:          {} executed / {} skipped\n",
            self.slices_executed, self.slices_skipped
        ));
        s.push_str(&format!(
            "elapsed:         {}\n",
            humantime::format_duration(Duration::from_secs(self.elapsed.as_secs()))
        ));
        match &self.abort_reason {
            Some(r) => s.push_str(&format!("ABORTED:         {r}\n")),
            None if self.is_partial() => s.push_str(&format!(
                "WARNING:         partial fill — {} of {} unexecuted\n",
                human(self.total_adjusted - self.filled),
                human(self.total_adjusted)
            )),
            // T4: "complete" is only ever true against the ADJUSTED target, so
            // it never stands alone when rounding shrank that target.
            None if self.rounding_dropped().is_some() => {
                s.push_str("status:          complete (against the adjusted target)\n")
            }
            None => s.push_str("status:          complete\n"),
        }
        // T4: printed on every outcome — an abort or a partial fill does not
        // make the pre-flight shortfall any less real.
        if let Some(dropped) = self.rounding_dropped() {
            s.push_str(&format!(
                "NOTE:            rounding dropped {} of requested {} at pre-flight\n",
                human(dropped),
                human(self.total_requested)
            ));
        }
        s.push_str(&format!("exit code:       {}\n", self.exit_code()));
        s
    }
}

/// Accumulator for fill statistics.
#[derive(Debug, Default, Clone)]
struct FillStats {
    filled: Decimal,
    /// Σ(px * sz), for the size-weighted average.
    notional: Decimal,
}

impl FillStats {
    fn add(&mut self, sz: Decimal, px: Decimal) {
        self.filled += sz;
        self.notional += sz * px;
    }

    fn avg_px(&self) -> Option<Decimal> {
        if self.filled > Decimal::ZERO {
            Some(self.notional / self.filled)
        } else {
            None
        }
    }
}

/// Fetch a book that passes full validation — freshness included — retrying a
/// stale or otherwise-invalid one (§8, Issue #6).
///
/// F2: this is the ONLY way a book should enter a sizing decision — pre-flight
/// and price-trigger polling included. Sizing off an unchecked snapshot can
/// fix the whole run's quantity against a price that is minutes old, from the
/// wrong coin, or from a crossed/malformed book. Validation is
/// [`ValidatedMarketSnapshot::validate`] — the same policy the trigger wait
/// loop uses — so a snapshot that clears this gate is fit to size an order or
/// satisfy a price trigger.
///
/// `deadline` (Issue #2) is `None` for callers outside the execution loop
/// (pre-flight, price-trigger polling — there is no `ExecutionDeadline` yet
/// at those points). Inside the slice loop it is `Some`, and the retry budget
/// is capped by whatever monotonic time remains: a book fetch must not be
/// allowed to burn the STALE_BOOK_RETRIES × STALE_BOOK_RETRY_INTERVAL budget
/// (up to several seconds) past a deadline that only had, say, 200ms left —
/// that is exactly the bug this issue exists to close (a slice that started
/// just inside the window but placed well outside it).
pub async fn fetch_fresh_book(
    client: &dyn HlApi,
    symbol: &Symbol,
    max_age_ms: u64,
    deadline: Option<&ExecutionDeadline>,
) -> Result<ValidatedMarketSnapshot, HlError> {
    let mut last_err: Option<HlError> = None;
    for attempt in 0..=STALE_BOOK_RETRIES {
        if let Some(dl) = deadline {
            if dl.has_passed(tokio::time::Instant::now()) {
                return Err(HlError::InvalidResponse(format!(
                    "execution deadline elapsed while fetching a fresh book (attempt {}); \
                     refusing to spend further retry budget",
                    attempt + 1
                )));
            }
        }
        // Issue #2 (Finding 2): a single fetch_l2_book call within one
        // attempt can otherwise run for up to the full HTTP_TIMEOUT past the
        // deadline before this loop notices (the has_passed check above only
        // runs BETWEEN attempts). When a deadline with a defined remaining
        // duration is in play, bound the in-flight call itself with
        // tokio::time::timeout. An elapsed timeout is treated as a deadline
        // abort (matching the style of the has_passed check above), not as
        // an ordinary fetch failure — it must NOT fall through to the
        // retry-and-sleep logic below, which could otherwise retry past the
        // deadline.
        let book = match deadline {
            Some(dl) => {
                let remaining = dl.remaining(tokio::time::Instant::now());
                match tokio::time::timeout(remaining, client.fetch_l2_book(symbol)).await {
                    Ok(result) => result?,
                    Err(_elapsed) => {
                        return Err(HlError::InvalidResponse(format!(
                            "execution deadline elapsed while awaiting an in-flight book fetch \
                             (attempt {}); refusing to await it to completion or retry past the \
                             deadline",
                            attempt + 1
                        )));
                    }
                }
            }
            None => client.fetch_l2_book(symbol).await?,
        };
        match ValidatedMarketSnapshot::validate(&book, symbol, max_age_ms) {
            Ok(snapshot) => return Ok(snapshot),
            Err(e) => {
                tracing::warn!(
                    symbol = %symbol,
                    max_age_ms,
                    attempt = attempt + 1,
                    error = %e,
                    "invalid or stale book; refetching"
                );
                last_err = Some(e);
            }
        }
        if attempt < STALE_BOOK_RETRIES {
            let sleep_for = match deadline {
                // Never sleep past the deadline: capping the sleep itself
                // (rather than only checking at the top of the next
                // iteration) means the NEXT attempt's deadline check fires
                // promptly instead of after a full unclipped retry interval.
                Some(dl) => {
                    STALE_BOOK_RETRY_INTERVAL.min(dl.remaining(tokio::time::Instant::now()))
                }
                None => STALE_BOOK_RETRY_INTERVAL,
            };
            if sleep_for.is_zero() {
                return Err(HlError::InvalidResponse(
                    "execution deadline elapsed; refusing to spend further retry budget on a book fetch"
                        .into(),
                ));
            }
            tokio::time::sleep(sleep_for).await;
        }
    }
    Err(HlError::InvalidResponse(format!(
        "book still invalid after {} retries: {}",
        STALE_BOOK_RETRIES,
        last_err.map(|e| e.to_string()).unwrap_or_default()
    )))
}

/// Ask HL for an order's status until it reports a TERMINAL one (T3).
///
/// A non-terminal status such as `"open"` means the cancel has not landed yet;
/// the order can still fill in the next millisecond. Returning that snapshot as
/// the final count under-states `filled_so_far`, and because every later slice
/// sizes off that number, the run then over-orders on EVERY remaining slice —
/// precisely the accident this recovery path exists to prevent. So only a
/// terminal status is adopted; a non-terminal one keeps retrying, and running
/// out of retries is a hard stop, deliberately the safe side.
///
/// Issue #7: also cross-checks the response's coin/side against `plan` —
/// oid is trivially correct (we queried by it), but a coin/side mismatch
/// means the response does not describe the order we placed.
async fn poll_terminal_status(
    client: &dyn HlApi,
    plan: &TwapPlan,
    user: &Address,
    oid: crate::types::OrderId,
) -> Result<OrderStatusFill, HlError> {
    let mut last_err: Option<String> = None;
    for attempt in 0..ORDER_STATUS_RETRIES {
        match client.fetch_order_status(user, oid).await {
            Ok(Some(st)) if st.is_terminal() => {
                st.cross_check(plan.symbol.as_str(), &plan.side, None)?;
                tracing::info!(
                    oid = %oid,
                    filled = %human(st.filled_sz),
                    status = %st.status,
                    "recovered resting order fill (terminal status)"
                );
                return Ok(st);
            }
            Ok(Some(st)) => {
                last_err = Some(format!(
                    "status '{}' is not terminal (filled {} so far)",
                    st.status,
                    human(st.filled_sz)
                ));
                tracing::warn!(
                    oid = %oid,
                    status = %st.status,
                    filled_so_far = %human(st.filled_sz),
                    attempt = attempt + 1,
                    "orderStatus not yet terminal; the order can still fill — retrying"
                );
            }
            Ok(None) => last_err = Some(format!("HL reports unknown oid {oid}")),
            Err(e) => last_err = Some(e.to_string()),
        }
        if attempt + 1 < ORDER_STATUS_RETRIES {
            tokio::time::sleep(ORDER_STATUS_RETRY_INTERVAL).await;
        }
    }
    Err(HlError::InvalidResponse(format!(
        "could not determine a terminal fill for oid {oid} after {ORDER_STATUS_RETRIES} attempts \
         ({}); stopping rather than risk over-ordering",
        last_err.unwrap_or_else(|| "no detail".into())
    )))
}

/// Recover the true filled quantity of a resting order (§8).
///
/// IOC should never rest, but if it does we cancel and then ask HL what
/// actually filled — assuming zero would over-order on the next slice
/// (over-fill is unrecoverable).
///
/// T5: returns the whole `OrderStatusFill`, not just the size, so the caller
/// can attribute the fill at HL's reported `avgPx` instead of at the limit
/// price (the worst price the order could possibly have got).
async fn recover_resting_fill(
    client: &dyn HlApi,
    plan: &TwapPlan,
    cloid: Cloid,
    oid: crate::types::OrderId,
) -> Result<OrderStatusFill, HlError> {
    tracing::warn!(oid = %oid, cloid = %cloid, "IOC order rested unexpectedly; cancelling");
    let cancel = CancelIntent {
        symbol: plan.symbol.clone(),
        by_cloid: cloid,
    };
    // A cancel failure is not fatal by itself — the order may have filled in
    // the interim. orderStatus below is what decides.
    if let Err(e) = client.cancel_by_cloid(&cancel, plan.asset_index).await {
        tracing::warn!(error = %e, "cancelByCloid failed; querying orderStatus anyway");
    }

    // F1: orderStatus must be queried as the MASTER, not the agent.
    let user = plan.status_user()?;
    poll_terminal_status(client, plan, user, oid).await
}

/// How many times a place may be re-signed and re-sent after an AMBIGUOUS
/// transport failure (W1). Each attempt is preceded by an `orderStatus`
/// reconciliation, so a resend only happens once HL has told us the order is
/// genuinely absent.
const PLACE_RESEND_LIMIT: u32 = 2;

/// Delay before reconciling an ambiguous place, giving HL time to book the
/// order it may already have received.
const RECONCILE_DELAY: Duration = Duration::from_millis(500);

/// What a slice's order attempt finally resolved to: the size to credit and
/// the price to credit it at.
///
/// A zero size is a legitimate outcome (an IOC that rested and was cancelled
/// without trading), so there is no separate "nothing happened" variant — every
/// resolved slice flows through one accounting path.
#[derive(Debug)]
struct SliceOutcome {
    sz: Decimal,
    px: Decimal,
}

/// Place one slice, resolving any transport ambiguity via cloid reconciliation
/// (W1).
///
/// The problem this solves: `/exchange` is not idempotent. The nonce is
/// consumed the moment HL receives the body, so the old behaviour — sign once,
/// blind-resend the same body up to three times — could only ever produce a
/// stale-nonce rejection on the retry, while the ORIGINAL order might have
/// filled. The run would then hard-stop without knowing it held a position.
///
/// The fix keys recovery on the cloid, which we chose before signing and which
/// therefore survives a lost response:
/// - send exactly once;
/// - on an ambiguous failure, ask HL about the cloid;
/// - if HL knows the order, adopt its (terminal) state — no resend;
/// - if HL returns `unknownOid`, the order never landed, so re-sign with a
///   FRESH nonce and send again (bounded);
/// - if reconciliation itself keeps failing, hard-stop with the ambiguity
///   stated plainly. Guessing here risks a double fill, which cannot be undone.
///
/// `deadline` (Issue #2): re-checked immediately before EVERY place attempt
/// in this function — the initial send and any resend after a safe-resend
/// reconciliation. Once it has passed, no further place/resend happens; the
/// reconciliation that is already in flight (status polling via
/// `reconcile_by_cloid` / `recover_resting_fill`) still runs to completion,
/// since status queries and cancels remain allowed past the deadline (PM
/// decision) — only a NEW place or resend is forbidden.
async fn place_slice_reconciled(
    client: &dyn HlApi,
    plan: &TwapPlan,
    intent: &OrderIntent,
    deadline: &ExecutionDeadline,
    slice_idx: u32,
    journal: Option<&mut ExecutionJournal>,
) -> Result<SliceOutcome, HlError> {
    deadline.check_before_send(tokio::time::Instant::now())?;

    // Issue #4: durably record intent+cloid BEFORE the send that could have
    // an ambiguous outcome. `nonce` is not known until `place_order_once`
    // mints it (the trait signs internally), so this Prepared record leaves
    // `nonce: None` — the crash window this closes is "did we even attempt
    // to send this cloid," which does not require the nonce value itself.
    // fsync happens inside `ExecutionJournal::record`; a failure to journal
    // aborts the slice rather than risk sending an order this run cannot
    // later prove it attempted.
    let mut journal = journal;
    if let Some(j) = journal.as_deref_mut() {
        j.record(&JournalRecord::Prepared {
            slice_idx,
            cloid: intent.cloid,
            nonce: None,
            symbol: intent.symbol.clone(),
            side: intent.side,
            px: intent.px.to_string(),
            sz: intent.sz.to_string(),
        })
        .map_err(|e| HlError::InvalidResponse(format!("journal write (Prepared) failed: {e}")))?;
    }

    let mut attempt = 0u32;
    loop {
        let send_err = match client
            .place_order_once(intent, plan.asset_index, deadline.expires_after_ms())
            .await
        {
            Ok((nonce, outcome @ PlaceOutcome::Filled { .. })) => {
                // Issue #7: the exchange response is a trusted boundary —
                // overfill, non-positive avgPx, and a fill outside the
                // signed limit are all hard errors here, never credited.
                let vf = ValidatedFill::try_from_place(&outcome, intent)?;
                tracing::debug!(nonce, "place acknowledged");
                journal_terminal(
                    journal.as_deref_mut(),
                    slice_idx,
                    intent.cloid,
                    "filled",
                    vf.filled_sz,
                    Some(vf.avg_px.unwrap_or(intent.px)),
                )?;
                return Ok(SliceOutcome {
                    sz: vf.filled_sz,
                    px: vf.avg_px.unwrap_or(intent.px),
                });
            }
            Ok((_, PlaceOutcome::Resting { oid })) => {
                if let Some(j) = journal.as_deref_mut() {
                    j.record(&JournalRecord::Acknowledged {
                        slice_idx,
                        cloid: intent.cloid,
                        oid: Some(oid.0),
                        status: "resting".into(),
                    })
                    .map_err(|e| {
                        HlError::InvalidResponse(format!(
                            "journal write (Acknowledged) failed: {e}"
                        ))
                    })?;
                }
                let st = recover_resting_fill(client, plan, intent.cloid, oid).await?;
                let vf = ValidatedFill::try_from_status(&st, intent)?;
                // T5: credit at HL's realised average, not at our limit.
                journal_terminal(
                    journal.as_deref_mut(),
                    slice_idx,
                    intent.cloid,
                    &st.status,
                    vf.filled_sz,
                    vf.avg_px,
                )?;
                return Ok(SliceOutcome {
                    sz: vf.filled_sz,
                    px: vf.avg_px.unwrap_or(intent.px),
                });
            }
            // Exchange rejections are decisions, not ambiguity — propagate.
            Err(e @ HlError::Exchange { .. }) => return Err(e),
            // Transport failure: the order may or may not have landed.
            Err(e @ HlError::Network(_)) => e,
            Err(e) => return Err(e),
        };

        tracing::warn!(
            cloid = %intent.cloid,
            error = %send_err,
            attempt = attempt + 1,
            "place outcome UNKNOWN after transport failure; reconciling by cloid"
        );

        // Issue #4: the POST was sent but its response was never read — this
        // is the crash-injection window between "sent" and "response read."
        // Journal it as SubmittedUnknown BEFORE the reconciliation sleep/poll
        // below, so a process death here still leaves a durable record that
        // this cloid's outcome must be resolved via orderStatus on restart.
        if let Some(j) = journal.as_deref_mut() {
            j.record(&JournalRecord::SubmittedUnknown {
                slice_idx,
                cloid: intent.cloid,
            })
            .map_err(|e| {
                HlError::InvalidResponse(format!("journal write (SubmittedUnknown) failed: {e}"))
            })?;
        }

        // Give HL a moment to book an order it may already have accepted.
        tokio::time::sleep(RECONCILE_DELAY).await;

        let user = plan.status_user()?;
        match reconcile_by_cloid(client, plan, user, intent.cloid).await {
            // HL has it, and it is settled — adopt that as the truth.
            Ok(Some(st)) => {
                let vf = ValidatedFill::try_from_status(&st, intent)?;
                tracing::info!(
                    cloid = %intent.cloid,
                    filled = %human(vf.filled_sz),
                    status = %st.status,
                    "reconciled: HL had the order; no resend"
                );
                journal_terminal(
                    journal.as_deref_mut(),
                    slice_idx,
                    intent.cloid,
                    &st.status,
                    vf.filled_sz,
                    vf.avg_px,
                )?;
                return Ok(SliceOutcome {
                    sz: vf.filled_sz,
                    px: vf.avg_px.unwrap_or(intent.px),
                });
            }
            // HL never received it — safe to re-sign with a fresh nonce,
            // PROVIDED the execution deadline has not passed in the
            // meantime (Issue #2): reconciliation itself can take seconds
            // (RECONCILE_DELAY + the unknownOid streak window), so the
            // deadline must be re-checked here, immediately before the
            // resend, not just once at the top of this function.
            Ok(None) => {
                deadline.check_before_send(tokio::time::Instant::now())?;
                attempt += 1;
                if attempt > PLACE_RESEND_LIMIT {
                    return Err(HlError::Network(format!(
                        "place failed {attempt} times and HL never received the order \
                         (cloid {}); giving up: {send_err}",
                        intent.cloid
                    )));
                }
                tracing::warn!(
                    cloid = %intent.cloid,
                    attempt,
                    "reconciled: HL never received the order; re-signing with a fresh nonce"
                );
            }
            // We cannot establish what happened. Stop rather than risk a
            // double fill.
            Err(e) => {
                return Err(HlError::InvalidResponse(format!(
                    "place outcome UNKNOWN for cloid {} and reconciliation failed ({e}); \
                     the order may or may not be live. Stopping rather than risk a duplicate \
                     fill — check your fills on Hyperliquid before re-running. \
                     Original send error: {send_err}",
                    intent.cloid
                )));
            }
        }
    }
}

/// Write a [`JournalRecord::Terminal`] record if a journal is attached.
/// Small helper to keep the three call sites in `place_slice_reconciled`
/// (direct fill, resting→recovered fill, ambiguous→reconciled fill) from
/// repeating the same `if let Some(j) = ...` boilerplate and error mapping.
fn journal_terminal(
    journal: Option<&mut ExecutionJournal>,
    slice_idx: u32,
    cloid: Cloid,
    status: &str,
    filled_sz: Decimal,
    avg_px: Option<Decimal>,
) -> Result<(), HlError> {
    if let Some(j) = journal {
        j.record(&JournalRecord::Terminal {
            slice_idx,
            cloid,
            status: status.to_string(),
            filled_sz: filled_sz.to_string(),
            avg_px: avg_px.map(|p| p.to_string()),
        })
        .map_err(|e| HlError::InvalidResponse(format!("journal write (Terminal) failed: {e}")))?;
    }
    Ok(())
}

/// Ask HL whether it holds `cloid`, retrying until the answer is unambiguous
/// under the Issue #7 W1 safe-resend policy.
///
/// `Ok(Some(st))` — HL has the order in a TERMINAL state; adopt it, no resend.
/// `Ok(None)`     — the unknownOid streak cleared BOTH thresholds
///                  ([`UNKNOWN_OID_MIN_CONSECUTIVE`] consecutive observations
///                  spanning at least [`UNKNOWN_OID_MIN_WINDOW`]) — safe to
///                  resend with a fresh nonce.
/// `Err(_)`       — could not establish either. This covers every other
///                  case: HL has it but it is still open/non-terminal, a
///                  mixed sequence (an unknownOid streak broken by a live
///                  response resets the streak), or the streak never
///                  cleared both thresholds before the attempt budget ran
///                  out. None of these is a safe basis for a resend.
async fn reconcile_by_cloid(
    client: &dyn HlApi,
    plan: &TwapPlan,
    user: &Address,
    cloid: Cloid,
) -> Result<Option<OrderStatusFill>, HlError> {
    let mut last_err: Option<String> = None;
    // Consecutive unknownOid streak tracking (W1 tightened policy): reset to
    // `None` by ANY non-unknownOid, non-terminal observation, since a mixed
    // sequence is not a safe basis for a resend even if the streak had
    // already cleared the count threshold.
    let mut streak_start: Option<tokio::time::Instant> = None;
    let mut streak_count: u32 = 0;

    for attempt in 0..UNKNOWN_OID_MAX_ATTEMPTS {
        match client.fetch_order_status_by_cloid(user, cloid).await {
            Ok(Some(st)) if st.is_terminal() => {
                st.cross_check(plan.symbol.as_str(), &plan.side, Some(cloid))?;
                return Ok(Some(st));
            }
            Ok(Some(st)) => {
                // The order exists and is still working. A resend here would
                // duplicate it, so this is never a safe-resend basis — and it
                // breaks any unknownOid streak in progress (a mixed sequence
                // is exactly what the tightened policy refuses to trust).
                last_err = Some(format!("order is live but non-terminal ('{}')", st.status));
                streak_start = None;
                streak_count = 0;
            }
            Ok(None) => {
                let now = tokio::time::Instant::now();
                streak_count += 1;
                let start = *streak_start.get_or_insert(now);
                let elapsed = now.saturating_duration_since(start);
                tracing::debug!(
                    cloid = %cloid,
                    streak_count,
                    elapsed_ms = elapsed.as_millis() as u64,
                    "unknownOid observed; accumulating safe-resend streak"
                );
                if streak_count >= UNKNOWN_OID_MIN_CONSECUTIVE && elapsed >= UNKNOWN_OID_MIN_WINDOW
                {
                    return Ok(None);
                }
                last_err = Some(format!(
                    "unknownOid streak at {streak_count}/{UNKNOWN_OID_MIN_CONSECUTIVE} \
                     observations, {}ms/{}ms window",
                    elapsed.as_millis(),
                    UNKNOWN_OID_MIN_WINDOW.as_millis()
                ));
            }
            Err(e) => {
                last_err = Some(e.to_string());
                streak_start = None;
                streak_count = 0;
            }
        }
        if attempt + 1 < UNKNOWN_OID_MAX_ATTEMPTS {
            tokio::time::sleep(UNKNOWN_OID_POLL_INTERVAL).await;
        }
    }
    Err(HlError::InvalidResponse(format!(
        "reconciliation for cloid {cloid} could not establish a safe-resend basis \
         ({}); a resend here risks a duplicate fill — \
         check your fills on Hyperliquid before re-running \
         (see docs/DEVELOPMENT.md \"orderStatus の read-after-write に関する注意\" for the \
         propagation-window policy this enforces)",
        last_err.unwrap_or_else(|| "no detail".into())
    )))
}

/// Run the TWAP loop (§8).
///
/// Thin wrapper over [`run_twap_journaled`] with no journal and no shutdown
/// signal — preserves the exact behaviour every pre-Issue-#4 caller (and the
/// 40+ existing `loop_tests` in this file) already depends on.
pub async fn run_twap(client: &dyn HlApi, plan: &TwapPlan) -> TwapReport {
    run_twap_journaled(client, plan, None, None).await
}

/// Force-reconcile ONE unresolved cloid (`Prepared`/`SubmittedUnknown`/
/// `Acknowledged` per the journal replay) via `orderStatus`, appending the
/// resolved outcome to `journal` (Issue #4).
///
/// Reuses the SAME `orderStatus`-by-cloid policy (`reconcile_by_cloid`,
/// Issue #7's W1 unknownOid streak logic) that `place_slice_reconciled`
/// already uses for an ambiguous send inside a live run — `main.rs`'s
/// `--resume` and `--abandon-incomplete-run` paths call this instead of
/// reimplementing reconciliation.
///
/// Every cloid handed to this function is one this run never itself
/// observed placing (it is reconciling a PRIOR process's in-flight state),
/// so `slice_idx` is not tracked by the caller — `0` is used as a
/// placeholder in the journal record; it is meaningful only within a single
/// live `run_twap_journaled` call, not across a resume boundary.
///
/// - HL has the cloid, terminal → journaled `Terminal` with the credited
///   fill (possibly zero, e.g. `canceled`/`rejected`).
/// - HL never received it (unknownOid streak cleared) → journaled
///   `Terminal` with `filled_sz = 0` — the order genuinely never landed, so
///   it is safe to record as a zero-fill terminal outcome; a resume/abandon
///   NEVER resends on this codepath (only a live run's OWN
///   `place_slice_reconciled` may resend).
/// - Neither resolves within the reconciliation budget → the error is
///   propagated as-is; `main.rs` surfaces it and the run does not proceed
///   (leaving the cloid unresolved in the journal for a future attempt).
pub async fn reconcile_unresolved_cloid(
    client: &dyn HlApi,
    plan: &TwapPlan,
    cloid: Cloid,
    journal: &mut ExecutionJournal,
) -> Result<(), HlError> {
    let user = plan.status_user()?;
    match reconcile_by_cloid(client, plan, user, cloid).await {
        Ok(Some(st)) => {
            journal_terminal(Some(journal), 0, cloid, &st.status, st.filled_sz, st.avg_px)?;
            Ok(())
        }
        Ok(None) => {
            journal_terminal(
                Some(journal),
                0,
                cloid,
                "neverReceived",
                Decimal::ZERO,
                None,
            )?;
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Cooperative shutdown signal (Issue #4).
///
/// Backed by [`tokio::sync::watch`] rather than a real OS signal so both the
/// production SIGINT/SIGTERM handler (`main.rs`) and tests can drive it
/// identically — a test sets the watch value directly, no real signal
/// delivery required. `true` means "a shutdown has been requested; stop
/// scheduling new slices, reconcile/cancel in-flight work, and return."
#[derive(Clone)]
pub struct ShutdownSignal(tokio::sync::watch::Receiver<bool>);

impl ShutdownSignal {
    pub fn new(rx: tokio::sync::watch::Receiver<bool>) -> Self {
        Self(rx)
    }

    pub fn is_triggered(&self) -> bool {
        *self.0.borrow()
    }

    /// Wait until shutdown is requested. Used to race against `sleep_until`
    /// so an interrupt during the inter-slice pause is noticed immediately
    /// rather than only at the top of the next loop iteration.
    async fn wait(&mut self) {
        // `changed()` only returns Err if the sender was dropped without
        // ever sending — in that case there is nothing more to wait for, so
        // treat it the same as "never triggers" by parking forever; the
        // caller's `select!` biases on the other branch finishing normally.
        while !*self.0.borrow() {
            if self.0.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }
}

/// Run the TWAP loop with optional crash-safety journaling and cooperative
/// shutdown (Issue #4).
///
/// `journal`: when `Some`, every slice's Prepared/SubmittedUnknown/
/// Acknowledged/Terminal transitions are durably recorded (see
/// `src/journal.rs`). `None` reproduces the pre-Issue-#4 in-memory-only
/// behaviour exactly (used by `run_twap` and all pre-existing tests).
///
/// `shutdown`: when `Some` and triggered (SIGINT/SIGTERM in production, or a
/// test driving the underlying `watch` channel directly), the loop stops
/// scheduling NEW slices at the next opportunity — the top of a slice
/// iteration, or during the inter-slice sleep — lets any in-flight
/// `place_slice_reconciled` call for the CURRENT slice run to completion
/// (never abandoned mid-send, so no is-it-terminal ambiguity is created by
/// the shutdown itself beyond what a normal ambiguous send already produces),
/// then returns with `abort_reason` describing the interruption.
pub async fn run_twap_journaled(
    client: &dyn HlApi,
    plan: &TwapPlan,
    mut journal: Option<&mut ExecutionJournal>,
    mut shutdown: Option<ShutdownSignal>,
) -> TwapReport {
    let start = tokio::time::Instant::now();
    // Issue #2: the run-level ExecutionDeadline, constructed ONCE at the
    // very start of execution so `monotonic` and `expires_after_ms` are
    // read from the two clocks at (as close as possible to) the same
    // instant. Every place/resend for the ENTIRE run — including the final
    // slice — checks against this same value; a resend does NOT get a fresh
    // expiry (PM decision).
    let exec_deadline = ExecutionDeadline::new(start, plan.duration, wall_clock_now_ms());
    let mut stats = FillStats::default();
    let mut slices_executed = 0u32;
    let mut slices_skipped = 0u32;
    let mut abort_reason: Option<String> = None;

    for slice_idx in 1..=plan.slices {
        // Issue #4: stop scheduling NEW slices once shutdown has been
        // requested. Checked at the top of every iteration so an interrupt
        // noticed during the previous slice's inter-slice sleep (via the
        // `select!` below) or between slices takes effect before the next
        // network call is ever made.
        if let Some(sd) = &shutdown {
            if sd.is_triggered() {
                abort_reason = Some(format!(
                    "shutdown requested; stopped before slice {slice_idx}/{}",
                    plan.slices
                ));
                break;
            }
        }
        if plan.read_only {
            println!("{READ_ONLY_BANNER}");
        }

        let slice_end = slice_deadline(start, plan.duration, slice_idx, plan.slices);

        // Hard window cut-off (§8, T2): never place past the requested
        // duration — the final slice included.
        //
        // The old code exempted the last slice (`&& slice_idx < plan.slices`).
        // That is the worst possible exemption: delay accumulates through
        // retries, stale-book refetches and fill recovery, and the final slice
        // is also the catch-up slice that carries every earlier shortfall, so
        // the exemption let the LARGEST order fire the FURTHEST outside the
        // window the operator asked for. A normal run reaches its last slice
        // inside the window anyway, so removing it changes nothing there.
        //
        // Issue #2: this check is now backed by `exec_deadline`, the same
        // ExecutionDeadline re-checked immediately before the place call
        // below and before any ambiguous-send resend — so a slice that
        // passes THIS check but then loses time to a book-fetch retry or a
        // reconciliation round-trip is caught again right before it would
        // actually send.
        if exec_deadline.has_passed(tokio::time::Instant::now()) {
            abort_reason = Some(format!(
                "duration {} elapsed at slice {slice_idx}/{}",
                humantime::format_duration(plan.duration),
                plan.slices
            ));
            break;
        }

        let snapshot = match fetch_fresh_book(
            client,
            &plan.symbol,
            plan.max_book_age_ms,
            Some(&exec_deadline),
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                abort_reason = Some(format!("book fetch failed at slice {slice_idx}: {e}"));
                break;
            }
        };
        let (bid, ask) = (snapshot.best_bid, snapshot.best_ask);

        // T1: the price we are about to sign, computed BEFORE the gate so the
        // gate judges the notional that will actually reach HL.
        let px = taker_limit_price(bid, ask, plan.side, plan.slippage_bps, plan.sz_decimals);

        // Issue #3: non-positive limit price is rejected unconditionally,
        // no override — belt-and-braces guard, mirroring the CLI pre-flight
        // check with the SAME risk module.
        if let Err(e) =
            RiskEnvelope::validate_limit_price(px, plan.side, plan.slippage_bps, bid, ask)
        {
            abort_reason = Some(format!(
                "slice {slice_idx}: risk envelope rejected the computed limit price: {e}"
            ));
            break;
        }

        // Issue #3: re-check the notional cap before EACH slice using the
        // ACTUAL order px for that slice (book prices move between slices),
        // not the estimate used at CLI pre-flight time.
        let slice_notional_estimate = plan.per_slice * px;
        if let Err(e) =
            RiskEnvelope::check_notional_cap(slice_notional_estimate, plan.max_notional_usd)
        {
            abort_reason = Some(format!(
                "slice {slice_idx}: risk envelope rejected the notional: {e}"
            ));
            break;
        }

        let decision = decide_slice(
            slice_idx,
            plan.slices,
            plan.per_slice,
            plan.total_adjusted,
            stats.filled,
            plan.sz_decimals,
            px,
        );

        let order_sz = match decision {
            SliceDecision::Place(sz) => sz,
            SliceDecision::SkipAhead => {
                slices_skipped += 1;
                tracing::info!(slice = slice_idx, "slice skipped: already at target");
                sleep_until(slice_end).await;
                continue;
            }
            SliceDecision::SkipBelowMinNotional { sz, notional } => {
                slices_skipped += 1;
                if slice_idx == plan.slices {
                    tracing::warn!(
                        slice = slice_idx,
                        sz = %human(sz),
                        notional = %human(notional),
                        order_px = %human(px),
                        gate = %human(min_notional_gate()),
                        "FINAL slice below min notional — residual is unexecutable"
                    );
                } else {
                    tracing::info!(
                        slice = slice_idx,
                        sz = %human(sz),
                        notional = %human(notional),
                        order_px = %human(px),
                        gate = %human(min_notional_gate()),
                        "slice below min notional; carrying to next slice"
                    );
                }
                sleep_until(slice_end).await;
                continue;
            }
        };

        let cloid = Cloid::new();

        if plan.read_only {
            println!(
                "[READ-ONLY] would place: slice {}/{} {} {} {} @ {} (IOC, cloid {}, mid {})",
                slice_idx,
                plan.slices,
                plan.side,
                human(order_sz),
                plan.symbol,
                human(px),
                cloid,
                human(snapshot.mid)
            );
            // Assume a full fill so the dry run walks the same slice path.
            stats.add(order_sz, px);
            slices_executed += 1;
            sleep_until(slice_end).await;
            continue;
        }

        let intent = OrderIntent {
            cloid,
            symbol: plan.symbol.clone(),
            side: plan.side,
            px,
            sz: order_sz,
            tif: Tif::Ioc,
            reduce_only: false,
        };
        tracing::info!(
            slice = slice_idx,
            slices = plan.slices,
            sz = %human(order_sz),
            px = %human(px),
            cloid = %cloid,
            "placing IOC slice"
        );

        // Issue #4: once a place is in flight it runs to completion — the
        // journal's Prepared/SubmittedUnknown/Acknowledged/Terminal
        // sequence inside `place_slice_reconciled` is what makes THIS call
        // crash-safe; a shutdown signal arriving mid-call is handled by the
        // NEXT iteration's top-of-loop check (or by the caller's own grace
        // timeout racing this whole `run_twap_journaled` future), never by
        // cancelling the call itself — an abandoned in-flight place is
        // exactly the ambiguity this feature exists to prevent.
        match place_slice_reconciled(
            client,
            plan,
            &intent,
            &exec_deadline,
            slice_idx,
            journal.as_deref_mut(),
        )
        .await
        {
            // Every fill — direct, recovered from a resting order, or
            // reconciled after an ambiguous send — is credited here EXACTLY
            // ONCE (T3/T5). There is no second accounting path.
            Ok(SliceOutcome { sz, px: fill_px }) => {
                stats.add(sz, fill_px);
                slices_executed += 1;
                tracing::info!(
                    slice = slice_idx,
                    filled = %human(sz),
                    avg_px = %human(fill_px),
                    cumulative = %human(stats.filled),
                    "slice filled"
                );
            }
            // Exchange rejection: NEVER retried, hard stop (§5).
            Err(HlError::Exchange { code, message }) => {
                let kind = RejectionKind::classify(&message);
                abort_reason = Some(format!(
                    "slice {slice_idx} rejected by exchange [{}]: {message} — {}",
                    code.unwrap_or_else(|| "?".into()),
                    kind.advice()
                ));
                break;
            }
            Err(e) => {
                abort_reason = Some(format!("slice {slice_idx} failed: {e}"));
                break;
            }
        }

        if stats.filled >= plan.total_adjusted {
            tracing::info!(filled = %human(stats.filled), "target reached; finishing early");
            break;
        }

        // Issue #4: race the inter-slice sleep against the shutdown signal
        // so an interrupt during a (potentially long) pause is noticed
        // immediately rather than only at the top of the next iteration —
        // including the case where shutdown was ALREADY triggered by the
        // time this point is reached (e.g. it fired while the slice we just
        // completed was in flight): that must break here, not fall through
        // to a full inter-slice sleep first.
        match &mut shutdown {
            Some(sd) if sd.is_triggered() => {
                abort_reason = Some(format!(
                    "shutdown requested during inter-slice wait after slice {slice_idx}/{}",
                    plan.slices
                ));
                break;
            }
            Some(sd) => {
                let mut sd_wait = sd.clone();
                tokio::select! {
                    _ = sleep_until(slice_end) => {}
                    _ = sd_wait.wait() => {
                        abort_reason = Some(format!(
                            "shutdown requested during inter-slice wait after slice {slice_idx}/{}",
                            plan.slices
                        ));
                        break;
                    }
                }
            }
            None => sleep_until(slice_end).await,
        }
    }

    if let Some(j) = journal {
        let _ = j.record(&JournalRecord::FinalReport {
            completed: abort_reason.is_none(),
            filled_total: stats.filled.to_string(),
            outcome_unknown_cloids: Vec::new(),
            note: abort_reason.clone().unwrap_or_else(|| "completed".into()),
        });
    }

    TwapReport {
        symbol: plan.symbol.clone(),
        side: plan.side,
        total_requested: plan.total_requested,
        total_adjusted: plan.total_adjusted,
        filled: stats.filled,
        avg_px: stats.avg_px(),
        slices_executed,
        slices_skipped,
        elapsed: start.elapsed(),
        abort_reason,
        read_only: plan.read_only,
    }
}

async fn sleep_until(deadline: tokio::time::Instant) {
    let now = tokio::time::Instant::now();
    if deadline > now {
        tokio::time::sleep_until(deadline).await;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    // === pre-flight sizing ===

    #[test]
    fn sizing_rounds_per_slice_down_and_reports_adjusted_total() {
        // 10 / 3 = 3.3333... → 3.33 at szDecimals=2 → adjusted 9.99
        let s = compute_sizing(dec!(10), 3, 2, dec!(100)).unwrap();
        assert_eq!(s.per_slice, dec!(3.33));
        assert_eq!(s.total_adjusted, dec!(9.99));
    }

    #[test]
    fn sizing_exact_division_leaves_no_remainder() {
        let s = compute_sizing(dec!(10), 5, 2, dec!(100)).unwrap();
        assert_eq!(s.per_slice, dec!(2));
        assert_eq!(s.total_adjusted, dec!(10));
    }

    #[test]
    fn sizing_errors_when_per_slice_rounds_to_zero() {
        // 0.005 / 10 = 0.0005 → 0 at szDecimals=2
        let err = compute_sizing(dec!(0.005), 10, 2, dec!(100000)).unwrap_err();
        assert!(matches!(err, PreflightError::PerSliceZero { .. }));
    }

    #[test]
    fn sizing_errors_when_per_slice_below_min_notional() {
        // 1 coin / 10 slices = 0.1 @ $50 = $5 < $10
        let err = compute_sizing(dec!(1), 10, 2, dec!(50)).unwrap_err();
        match err {
            PreflightError::PerSliceBelowMinNotional { notional, min } => {
                assert_eq!(notional, dec!(5.0));
                assert_eq!(min, dec!(10));
            }
            other => panic!("expected min-notional error, got {other:?}"),
        }
    }

    #[test]
    fn sizing_at_exactly_min_notional_is_accepted() {
        // 2 coins / 10 = 0.2 @ $50 = exactly $10
        let s = compute_sizing(dec!(2), 10, 2, dec!(50)).unwrap();
        assert_eq!(s.per_slice, dec!(0.2));
    }

    #[test]
    fn sizing_rejects_non_positive_total() {
        assert!(matches!(
            compute_sizing(dec!(0), 10, 2, dec!(50)).unwrap_err(),
            PreflightError::NonPositiveTotal(_)
        ));
    }

    // === USD conversion ===

    #[test]
    fn usd_converts_to_coin_at_mid() {
        assert_eq!(usd_to_coin(dec!(1500), dec!(30)).unwrap(), dec!(50));
    }

    #[test]
    fn usd_conversion_rejects_non_positive_mid() {
        assert!(usd_to_coin(dec!(1500), dec!(0)).is_err());
    }

    #[test]
    fn usd_flow_end_to_end_matches_spec_example() {
        // --usd 1500 at mid 30 → 50 coins over 10 slices → 5 per slice.
        let coin = usd_to_coin(dec!(1500), dec!(30)).unwrap();
        let s = compute_sizing(coin, 10, 2, dec!(30)).unwrap();
        assert_eq!(s.per_slice, dec!(5));
        assert_eq!(s.total_adjusted, dec!(50));
    }

    // === target / catch-up ===

    #[test]
    fn target_is_linear_until_final_slice_absorbs_remainder() {
        let (per, total) = (dec!(3.33), dec!(9.99));
        assert_eq!(target_at_slice(1, 3, per, total), dec!(3.33));
        assert_eq!(target_at_slice(2, 3, per, total), dec!(6.66));
        // Final slice targets the adjusted total exactly.
        assert_eq!(target_at_slice(3, 3, per, total), dec!(9.99));
    }

    #[test]
    fn catch_up_orders_the_shortfall_after_a_partial_fill() {
        // Slice 1 targeted 5 but only 3 filled; slice 2 must order 7 (=10-3),
        // NOT 5 — the shortfall is caught up exactly once.
        let sz = slice_order_size(2, 10, dec!(5), dec!(50), dec!(3), 2);
        assert_eq!(sz, dec!(7));
    }

    #[test]
    fn catch_up_never_double_orders_when_fully_filled() {
        // Everything up to slice 2's target is already filled → order exactly
        // one slice worth.
        let sz = slice_order_size(3, 10, dec!(5), dec!(50), dec!(10), 2);
        assert_eq!(sz, dec!(5));
    }

    #[test]
    fn over_fill_yields_zero_not_a_negative_order() {
        // Filled 12 but slice-2 target is 10 → nothing to do.
        let sz = slice_order_size(2, 10, dec!(5), dec!(50), dec!(12), 2);
        assert_eq!(sz, Decimal::ZERO);
    }

    #[test]
    fn last_slice_absorbs_the_rounding_remainder() {
        // per_slice 3.33 × 3 = 9.99. After two full slices (6.66), the last
        // slice must order the 3.33 that completes total_adjusted.
        let sz = slice_order_size(3, 3, dec!(3.33), dec!(9.99), dec!(6.66), 2);
        assert_eq!(sz, dec!(3.33));
    }

    #[test]
    fn order_size_is_rounded_down_to_sz_decimals() {
        // Shortfall of 3.456789 must truncate, never round up.
        let sz = slice_order_size(1, 10, dec!(3.456789), dec!(34.56789), dec!(0), 2);
        assert_eq!(sz, dec!(3.45));
    }

    #[test]
    fn full_run_arithmetic_never_exceeds_total_adjusted() {
        // Walk all 10 slices assuming full fills; the cumulative total must
        // land exactly on total_adjusted and never overshoot en route.
        let (per, total, slices) = (dec!(3.33), dec!(33.30), 10u32);
        let mut filled = Decimal::ZERO;
        for i in 1..=slices {
            let sz = slice_order_size(i, slices, per, total, filled, 2);
            filled += sz;
            assert!(filled <= total, "slice {i} overshot: {filled} > {total}");
        }
        assert_eq!(filled, total);
    }

    #[test]
    fn run_with_partial_fills_still_converges_to_total() {
        // Every slice fills only half; the catch-up logic keeps pushing the
        // shortfall forward and the last slice requests the full remainder.
        let (per, total, slices) = (dec!(5), dec!(50), 10u32);
        let mut filled = Decimal::ZERO;
        let mut last_requested = Decimal::ZERO;
        let mut filled_before_last = Decimal::ZERO;
        for i in 1..=slices {
            let sz = slice_order_size(i, slices, per, total, filled, 2);
            if i == slices {
                filled_before_last = filled;
                last_requested = sz;
            }
            filled += sz / dec!(2);
            // The catch-up must never request more than the outstanding gap.
            assert!(filled <= total, "slice {i} overshot: {filled} > {total}");
        }
        // The final slice asks for the entire outstanding shortfall, modulo
        // the size-precision truncation (which can only ever under-request).
        let gap = total - filled_before_last;
        assert_eq!(last_requested, round_size(gap, 2));
        assert!(
            last_requested <= gap,
            "must never request more than the gap"
        );
        assert!(last_requested > per, "shortfall should have accumulated");
    }

    // === min-notional skip / carry ===

    #[test]
    fn slice_below_min_notional_is_skipped_and_carried() {
        // per-slice 0.1 @ $50 = $5 < $10 → skip.
        let d = decide_slice(1, 10, dec!(0.1), dec!(1), dec!(0), 2, dec!(50));
        match d {
            SliceDecision::SkipBelowMinNotional { sz, notional } => {
                assert_eq!(sz, dec!(0.1));
                assert_eq!(notional, dec!(5.0));
            }
            other => panic!("expected skip, got {other:?}"),
        }
        // Slice 2's target is cumulative → 0.2 = $10.00. That sits exactly on
        // the bare floor and so is now SKIPPED: the T1 margin demands headroom
        // (see `min_notional_gate_requires_headroom_over_the_bare_floor`).
        assert!(matches!(
            decide_slice(2, 10, dec!(0.1), dec!(1), dec!(0), 2, dec!(50)),
            SliceDecision::SkipBelowMinNotional { .. }
        ));
        // Slice 3 carries to 0.3 = $15, comfortably clear of the gate.
        assert_eq!(
            decide_slice(3, 10, dec!(0.1), dec!(1), dec!(0), 2, dec!(50)),
            SliceDecision::Place(dec!(0.3))
        );
    }

    // === T1: the gate uses the ORDER price, not the mid ===

    #[test]
    fn t1_short_slice_priced_below_mid_is_gated_on_the_order_price() {
        // The reviewed counter-example. bid=49.9 / ask=50.1 → mid=50.0.
        // A SHORT's taker limit sits BELOW the bid: 49.9 - 20bps = 49.80.
        // sz=0.2 → mid notional $10.00 (would have passed the old gate) but the
        // real order is 0.2 × 49.80 = $9.96, which HL rejects as MinTradeNtl —
        // a FATAL rejection that would stop the whole run.
        let (bid, ask) = (dec!(49.9), dec!(50.1));
        let mid = (bid + ask) / dec!(2);
        assert_eq!(mid, dec!(50.0));

        let px = taker_limit_price(bid, ask, Side::Short, dec!(20), 2);
        assert!(px < mid, "short limit {px} must sit below mid {mid}");
        assert_eq!(px, dec!(49.80));

        let sz = dec!(0.2);
        assert_eq!(sz * mid, dec!(10.00), "mid notional sits on the old gate");
        assert!(
            sz * px < MIN_NOTIONAL_USD,
            "real notional is under the floor"
        );

        // Gating on the order price skips (and carries) instead of placing a
        // doomed order.
        match decide_slice(1, 10, sz, sz * dec!(10), dec!(0), 2, px) {
            SliceDecision::SkipBelowMinNotional { sz: s, notional } => {
                assert_eq!(s, sz);
                assert_eq!(notional, dec!(9.960));
            }
            other => panic!("expected skip on the real order price, got {other:?}"),
        }
    }

    #[test]
    fn t1_long_slice_priced_above_mid_still_gates_on_the_order_price() {
        // The long case is the mirror image: the limit is ABOVE the mid, so the
        // order-price gate is strictly more permissive than the mid gate. It
        // must still be the order price that decides.
        let (bid, ask) = (dec!(49.9), dec!(50.1));
        let px = taker_limit_price(bid, ask, Side::Long, dec!(20), 2);
        assert!(px > dec!(50.0));
        // 0.21 × ~50.2 = ~$10.54 — over the margined gate.
        assert!(dec!(0.21) * px > min_notional_gate());
        assert_eq!(
            decide_slice(1, 10, dec!(0.21), dec!(2.1), dec!(0), 2, px),
            SliceDecision::Place(dec!(0.21))
        );
    }

    #[test]
    fn min_notional_gate_requires_headroom_over_the_bare_floor() {
        // A notional resting exactly on $10.00 is one adverse tick away from a
        // FATAL MinTradeNtl rejection. The margin converts that hard stop into
        // a skip-and-carry, so the gate must sit strictly above the floor.
        assert!(min_notional_gate() > MIN_NOTIONAL_USD);
        assert_eq!(min_notional_gate(), dec!(10.10));

        // Exactly $10.00 → skipped.
        assert!(matches!(
            decide_slice(1, 10, dec!(0.2), dec!(2), dec!(0), 2, dec!(50)),
            SliceDecision::SkipBelowMinNotional { .. }
        ));
        // $10.20 → placed.
        assert_eq!(
            decide_slice(1, 10, dec!(0.2), dec!(2), dec!(0), 2, dec!(51)),
            SliceDecision::Place(dec!(0.2))
        );
    }

    #[test]
    fn carry_accumulates_across_several_skipped_slices() {
        // $4 per slice: needs 3 slices to clear the $10 floor.
        let (per, total, mid) = (dec!(0.08), dec!(0.8), dec!(50));
        assert!(matches!(
            decide_slice(1, 10, per, total, dec!(0), 2, mid),
            SliceDecision::SkipBelowMinNotional { .. }
        ));
        assert!(matches!(
            decide_slice(2, 10, per, total, dec!(0), 2, mid),
            SliceDecision::SkipBelowMinNotional { .. }
        ));
        // Slice 3: 0.24 @ $50 = $12 ≥ $10 → place the accumulated carry.
        assert_eq!(
            decide_slice(3, 10, per, total, dec!(0), 2, mid),
            SliceDecision::Place(dec!(0.24))
        );
    }

    #[test]
    fn slice_at_target_skips_ahead() {
        assert_eq!(
            decide_slice(1, 10, dec!(5), dec!(50), dec!(5), 2, dec!(100)),
            SliceDecision::SkipAhead
        );
    }

    #[test]
    fn slice_above_min_notional_is_placed() {
        assert_eq!(
            decide_slice(1, 10, dec!(5), dec!(50), dec!(0), 2, dec!(100)),
            SliceDecision::Place(dec!(5))
        );
    }

    // === deadlines ===

    #[tokio::test(start_paused = true)]
    async fn deadlines_are_evenly_spaced_from_the_absolute_start() {
        let start = tokio::time::Instant::now();
        let dur = Duration::from_secs(30 * 60);
        let d1 = slice_deadline(start, dur, 1, 10);
        let d5 = slice_deadline(start, dur, 5, 10);
        let d10 = slice_deadline(start, dur, 10, 10);
        assert_eq!(d1 - start, Duration::from_secs(180));
        assert_eq!(d5 - start, Duration::from_secs(900));
        assert_eq!(d10 - start, dur);
    }

    #[tokio::test(start_paused = true)]
    async fn final_deadline_equals_the_full_duration_with_odd_slice_counts() {
        let start = tokio::time::Instant::now();
        let dur = Duration::from_secs(100);
        assert_eq!(slice_deadline(start, dur, 3, 3) - start, dur);
        assert_eq!(slice_deadline(start, dur, 7, 7) - start, dur);
    }

    // === report ===

    fn report(filled: Decimal, adjusted: Decimal, abort: Option<&str>) -> TwapReport {
        TwapReport {
            symbol: Symbol::new("HYPE"),
            side: Side::Long,
            total_requested: adjusted,
            total_adjusted: adjusted,
            filled,
            avg_px: Some(dec!(38.1)),
            slices_executed: 10,
            slices_skipped: 0,
            elapsed: Duration::from_secs(1800),
            abort_reason: abort.map(str::to_string),
            read_only: false,
        }
    }

    #[test]
    fn complete_run_exits_zero() {
        let r = report(dec!(50), dec!(50), None);
        assert_eq!(r.exit_code(), 0);
        assert!(!r.is_partial());
        assert!(r.render().contains("status:          complete"));
    }

    #[test]
    fn partial_but_unaborted_run_exits_zero_with_warning() {
        let r = report(dec!(30), dec!(50), None);
        assert_eq!(r.exit_code(), 0);
        assert!(r.is_partial());
        assert!(r.render().contains("WARNING"), "{}", r.render());
    }

    #[test]
    fn aborted_run_exits_one() {
        let r = report(dec!(30), dec!(50), Some("exchange rejected"));
        assert_eq!(r.exit_code(), 1);
        assert!(r.render().contains("ABORTED"));
    }

    // === T4: pre-flight rounding loss is always re-surfaced ===

    /// Report where the requested and adjusted totals differ.
    fn report_with_requested(
        requested: Decimal,
        adjusted: Decimal,
        filled: Decimal,
        abort: Option<&str>,
    ) -> TwapReport {
        TwapReport {
            total_requested: requested,
            ..report(filled, adjusted, abort)
        }
    }

    #[test]
    fn t4_rounding_loss_is_reported_even_when_the_adjusted_target_is_met() {
        // szDecimals=0, requested 10.5 over some slices → adjusted 8. Filling
        // all 8 is "complete" against the adjusted target, but 2.5 of what the
        // operator asked for (24%) never entered the market. Reporting a bare
        // "complete" here hides that entirely.
        let r = report_with_requested(dec!(10.5), dec!(8), dec!(8), None);
        assert!(!r.is_partial(), "filled == adjusted, so not a partial fill");
        assert_eq!(r.exit_code(), 0);
        assert_eq!(r.rounding_dropped(), Some(dec!(2.5)));

        let out = r.render();
        assert!(
            out.contains("NOTE:            rounding dropped 2.5 of requested 10.5 at pre-flight"),
            "{out}"
        );
        // "complete" must never appear unqualified when a shortfall exists.
        assert!(
            out.contains("complete (against the adjusted target)"),
            "{out}"
        );
        assert!(!out.contains("status:          complete\n"), "{out}");
    }

    #[test]
    fn t4_no_note_when_rounding_dropped_nothing() {
        let r = report_with_requested(dec!(50), dec!(50), dec!(50), None);
        assert_eq!(r.rounding_dropped(), None);
        let out = r.render();
        assert!(!out.contains("NOTE:"), "{out}");
        assert!(out.contains("status:          complete\n"), "{out}");
    }

    #[test]
    fn t4_note_survives_a_partial_fill_and_an_abort() {
        // A partial fill or an abort does not make the pre-flight shortfall any
        // less real, so the note is printed alongside either.
        let partial = report_with_requested(dec!(10.5), dec!(8), dec!(5), None).render();
        assert!(partial.contains("WARNING"), "{partial}");
        assert!(partial.contains("rounding dropped 2.5"), "{partial}");

        let aborted = report_with_requested(dec!(10.5), dec!(8), dec!(5), Some("boom")).render();
        assert!(aborted.contains("ABORTED"), "{aborted}");
        assert!(aborted.contains("rounding dropped 2.5"), "{aborted}");
    }

    #[test]
    fn read_only_report_is_labelled() {
        let mut r = report(dec!(50), dec!(50), None);
        r.read_only = true;
        assert!(r.render().contains("READ-ONLY"));
    }

    #[test]
    fn avg_px_is_size_weighted() {
        let mut s = FillStats::default();
        s.add(dec!(1), dec!(100));
        s.add(dec!(3), dec!(200));
        // (1*100 + 3*200) / 4 = 175
        assert_eq!(s.avg_px(), Some(dec!(175)));
    }

    #[test]
    fn avg_px_is_none_with_no_fills() {
        assert_eq!(FillStats::default().avg_px(), None);
    }

    #[test]
    fn read_only_banner_is_loud() {
        assert!(READ_ONLY_BANNER.contains("READ-ONLY"));
        assert!(READ_ONLY_BANNER.contains("NO ORDERS ARE SENT"));
    }
}

/// Loop-level tests driving `run_twap` through the `HlApi` seam (T6).
///
/// The pure slice arithmetic above is well covered, but it was the SEQUENCING
/// layer — the part that actually commits money — that carried T1, T2, T3 and
/// T5 past a green suite. These tests exercise `run_twap` itself, with virtual
/// time so a 30-minute window costs nothing.
#[cfg(test)]
mod loop_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::api::{Call, HlApi, ScriptedApi};
    use crate::client::OrderStatusFill;
    use crate::types::{BookLevel, OrderBook, OrderId};

    const MASTER: &str = "0x00000000000000000000000000000000000000aa";
    const AGENT: &str = "0x00000000000000000000000000000000000000bb";

    fn book_at(bid: Decimal, ask: Decimal) -> OrderBook {
        OrderBook {
            coin: "HYPE".to_string(),
            bids: vec![BookLevel {
                px: bid,
                sz: dec!(1000),
                n: 1,
            }],
            asks: vec![BookLevel {
                px: ask,
                sz: dec!(1000),
                n: 1,
            }],
            // `max_book_age_ms: 0` in these plans disables the freshness gate,
            // so the timestamp is irrelevant to the behaviour under test.
            time_ms: 0,
        }
    }

    /// 10 slices of 5 coins over 30 minutes, long, at ~$50.
    fn plan(read_only: bool) -> TwapPlan {
        TwapPlan {
            symbol: Symbol::new("HYPE"),
            side: Side::Long,
            asset_index: 2,
            sz_decimals: 2,
            per_slice: dec!(5),
            total_adjusted: dec!(50),
            total_requested: dec!(50),
            slices: 10,
            duration: Duration::from_secs(1800),
            slippage_bps: dec!(20),
            // Disabled: these tests pin sequencing, not freshness.
            max_book_age_ms: 0,
            read_only,
            // Generous default so pre-existing tests (small notionals, ~$5-500)
            // are unaffected; Issue #3 boundary tests override this explicitly.
            max_notional_usd: dec!(1_000_000),
            agent: Some(Address::new(AGENT)),
            master: if read_only {
                None
            } else {
                Some(Address::new(MASTER))
            },
        }
    }

    fn filled(sz: Decimal, px: Decimal) -> Result<PlaceOutcome, HlError> {
        Ok(PlaceOutcome::Filled {
            oid: OrderId(1),
            total_sz: sz,
            avg_px: px,
        })
    }

    /// Default oid/coin/side match `plan()`'s HYPE/Long order, so existing
    /// tests that don't care about the cross-check keep passing it for free.
    fn status(filled_sz: Decimal, avg_px: Option<Decimal>, st: &str) -> OrderStatusFill {
        status_full(filled_sz, avg_px, st, OrderId(77), None, "HYPE", "B")
    }

    #[allow(clippy::too_many_arguments)]
    fn status_full(
        filled_sz: Decimal,
        avg_px: Option<Decimal>,
        st: &str,
        oid: OrderId,
        cloid: Option<Cloid>,
        coin: &str,
        side: &str,
    ) -> OrderStatusFill {
        OrderStatusFill {
            filled_sz,
            avg_px,
            status: st.into(),
            oid,
            cloid,
            coin: coin.into(),
            side: side.into(),
        }
    }

    // === (d) happy path ===

    #[tokio::test(start_paused = true)]
    async fn d_ten_slice_happy_path_fills_exactly_the_adjusted_total() {
        let mut api = ScriptedApi::new().with_default_book(book_at(dec!(49.9), dec!(50.1)));
        for _ in 0..10 {
            api = api.push_place(filled(dec!(5), dec!(50)));
        }

        let report = run_twap(&api, &plan(false)).await;

        assert_eq!(report.filled, dec!(50));
        assert_eq!(report.total_adjusted, dec!(50));
        assert_eq!(report.slices_executed, 10);
        assert_eq!(report.slices_skipped, 0);
        assert_eq!(report.abort_reason, None);
        assert_eq!(report.exit_code(), 0);
        assert!(!report.is_partial());
        assert_eq!(api.place_count(), 10);

        // The cumulative total must never overshoot at ANY step, not just at
        // the end — an intermediate overshoot is an unrecoverable over-fill.
        let mut cumulative = Decimal::ZERO;
        for c in api.place_calls() {
            if let Call::Place { sz, .. } = c {
                cumulative += sz;
                assert!(
                    cumulative <= dec!(50),
                    "overshot mid-run: {cumulative} > 50"
                );
            }
        }
        assert_eq!(cumulative, dec!(50));
    }

    // === (a) T2: the duration cut-off exempts nothing ===

    #[tokio::test(start_paused = true)]
    async fn a_t2_no_order_is_placed_after_the_duration_elapses() {
        // Every place takes 5 minutes to come back (HL slow / retries / fill
        // recovery). With a 20-minute window and 10 slices, the clock runs out
        // partway through and the run must stop — including on the FINAL slice,
        // which the old code exempted.
        struct SlowApi {
            inner: ScriptedApi,
            start: tokio::time::Instant,
            /// Elapsed time at each place, to prove none happened late.
            place_times: std::sync::Mutex<Vec<Duration>>,
        }

        #[async_trait::async_trait]
        impl HlApi for SlowApi {
            async fn fetch_l2_book(&self, s: &Symbol) -> Result<OrderBook, HlError> {
                self.inner.fetch_l2_book(s).await
            }
            async fn place_order_once(
                &self,
                i: &OrderIntent,
                a: u32,
                e: u64,
            ) -> Result<(u64, PlaceOutcome), HlError> {
                self.place_times.lock().unwrap().push(self.start.elapsed());
                let r = self.inner.place_order_once(i, a, e).await;
                tokio::time::sleep(Duration::from_secs(300)).await;
                r
            }
            async fn cancel_by_cloid(&self, i: &CancelIntent, a: u32) -> Result<(), HlError> {
                self.inner.cancel_by_cloid(i, a).await
            }
            async fn fetch_order_status(
                &self,
                u: &Address,
                o: OrderId,
            ) -> Result<Option<OrderStatusFill>, HlError> {
                self.inner.fetch_order_status(u, o).await
            }
            async fn fetch_order_status_by_cloid(
                &self,
                u: &Address,
                c: Cloid,
            ) -> Result<Option<OrderStatusFill>, HlError> {
                self.inner.fetch_order_status_by_cloid(u, c).await
            }
        }

        let mut inner = ScriptedApi::new().with_default_book(book_at(dec!(49.9), dec!(50.1)));
        for _ in 0..10 {
            inner = inner.push_place(filled(dec!(5), dec!(50)));
        }
        let window = Duration::from_secs(1200);
        let api = SlowApi {
            inner,
            start: tokio::time::Instant::now(),
            place_times: std::sync::Mutex::new(Vec::new()),
        };

        let mut p = plan(false);
        p.duration = window;
        let report = run_twap(&api, &p).await;

        // The window ran out, so the run aborted rather than finishing.
        let reason = report.abort_reason.clone().expect("must abort on duration");
        assert!(reason.contains("elapsed"), "{reason}");
        assert_eq!(report.exit_code(), 1);

        // The pin: not one order was sent at or past the window edge.
        let times = api.place_times.lock().unwrap().clone();
        assert!(!times.is_empty(), "the run should have placed something");
        for (i, t) in times.iter().enumerate() {
            assert!(
                *t < window,
                "slice {} placed at {t:?}, outside the {window:?} window",
                i + 1
            );
        }
        // And it stopped short of the full slice count.
        assert!(
            times.len() < 10,
            "expected an early stop, placed {} slices",
            times.len()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_t2_final_slice_gets_no_exemption_from_the_window() {
        // Directly targets the removed `&& slice_idx < plan.slices` clause.
        //
        // Two slices over 10 minutes. Slice 1 is fine, but its place takes 15
        // minutes to resolve (retries / recovery), so by the time slice 2 — the
        // FINAL slice — comes up the window is long gone. Under the old code
        // that slice was exempt and fired anyway, and because it is also the
        // catch-up slice it would have carried the largest size of the run.
        struct SlowPlace {
            inner: ScriptedApi,
        }

        #[async_trait::async_trait]
        impl HlApi for SlowPlace {
            async fn fetch_l2_book(&self, s: &Symbol) -> Result<OrderBook, HlError> {
                self.inner.fetch_l2_book(s).await
            }
            async fn place_order_once(
                &self,
                i: &OrderIntent,
                a: u32,
                e: u64,
            ) -> Result<(u64, PlaceOutcome), HlError> {
                let r = self.inner.place_order_once(i, a, e).await;
                tokio::time::sleep(Duration::from_secs(900)).await;
                r
            }
            async fn cancel_by_cloid(&self, i: &CancelIntent, a: u32) -> Result<(), HlError> {
                self.inner.cancel_by_cloid(i, a).await
            }
            async fn fetch_order_status(
                &self,
                u: &Address,
                o: OrderId,
            ) -> Result<Option<OrderStatusFill>, HlError> {
                self.inner.fetch_order_status(u, o).await
            }
            async fn fetch_order_status_by_cloid(
                &self,
                u: &Address,
                c: Cloid,
            ) -> Result<Option<OrderStatusFill>, HlError> {
                self.inner.fetch_order_status_by_cloid(u, c).await
            }
        }

        let api = SlowPlace {
            inner: ScriptedApi::new()
                .with_default_book(book_at(dec!(49.9), dec!(50.1)))
                // Only slice 1 is fillable; slice 2 must never be attempted.
                .push_place(filled(dec!(2), dec!(50))),
        };

        let mut p = plan(false);
        p.slices = 2;
        p.per_slice = dec!(5);
        p.total_adjusted = dec!(10);
        p.total_requested = dec!(10);
        p.duration = Duration::from_secs(600);

        let report = run_twap(&api, &p).await;

        assert_eq!(
            api.inner.place_count(),
            1,
            "the FINAL slice must not be exempt from the window"
        );
        // Only slice 1's 2 coins were filled; the catch-up never fired.
        assert_eq!(report.filled, dec!(2));
        let reason = report
            .abort_reason
            .clone()
            .expect("running past the window must abort");
        assert!(reason.contains("elapsed"), "{reason}");
        assert!(reason.contains("slice 2/2"), "{reason}");
        assert_eq!(report.exit_code(), 1);
    }

    // === Issue #2: ExecutionDeadline ===

    #[test]
    fn execution_deadline_check_before_send_passes_before_the_deadline() {
        let now = std::time::Instant::now();
        let start = tokio::time::Instant::now();
        let dl = ExecutionDeadline::from_parts(start + Duration::from_secs(10), 1_700_000_000_000);
        assert!(dl.check_before_send(tokio::time::Instant::now()).is_ok());
        assert!(!dl.has_passed(tokio::time::Instant::now()));
        // remaining() should be close to 10s (paused-clock tests use exact
        // instants elsewhere; here we just sanity-check the ordering holds).
        let _ = now;
    }

    #[test]
    fn execution_deadline_check_before_send_fails_after_the_deadline() {
        let start = tokio::time::Instant::now();
        // A deadline already in the past relative to "now".
        let dl = ExecutionDeadline::from_parts(start, 1_700_000_000_000);
        // Force "now" to be after the deadline by using a later Instant.
        let later = start + Duration::from_millis(1);
        assert!(dl.has_passed(later));
        let err = dl.check_before_send(later).unwrap_err();
        assert!(
            format!("{err}").contains("deadline"),
            "error should mention the deadline: {err}"
        );
    }

    #[test]
    fn execution_deadline_expires_after_ms_is_wall_clock_start_plus_duration() {
        let start = tokio::time::Instant::now();
        let dl = ExecutionDeadline::new(start, Duration::from_secs(60), 1_700_000_000_000);
        assert_eq!(dl.expires_after_ms(), 1_700_000_060_000);
    }

    #[test]
    fn clock_skew_within_tolerance_is_accepted() {
        assert!(check_clock_skew(1_700_000_000_000, 1_700_000_004_000).is_ok());
        assert!(check_clock_skew(1_700_000_000_000, 1_699_999_996_000).is_ok());
        // Exactly at the tolerance boundary is still accepted (`>` not `>=`).
        assert!(check_clock_skew(1_700_000_000_000, 1_700_000_005_000).is_ok());
        assert!(check_clock_skew(1_700_000_000_000, 1_699_999_995_000).is_ok());
    }

    #[test]
    fn clock_skew_beyond_tolerance_fails_closed() {
        let err = check_clock_skew(1_700_000_000_000, 1_700_000_005_001).unwrap_err();
        assert!(matches!(err, HlError::InvalidConfig(_)));
        let msg = format!("{err}");
        assert!(msg.contains("skew"), "{msg}");
        assert!(msg.contains("NTP"), "{msg}");

        // Negative direction (local clock is ahead) fails closed too.
        let err2 = check_clock_skew(1_700_000_005_001, 1_700_000_000_000).unwrap_err();
        assert!(matches!(err2, HlError::InvalidConfig(_)));
    }

    // === Issue #2 acceptance criterion 1: a book-fetch retry that crosses
    // the deadline must place ZERO orders after the deadline ===

    #[tokio::test(start_paused = true)]
    async fn issue2_book_fetch_crossing_the_deadline_places_zero_orders_after_it() {
        // Every book fetch is slow enough that, combined with the retry
        // interval, the SECOND slice's book fetch starts before the deadline
        // but cannot possibly finish an attempt before it. The reproduction
        // from the issue body: "T+59.5s enters the final slice, then
        // fetch_fresh_book's retry/delay burns >1s past T+60s".
        struct SlowBookApi {
            inner: ScriptedApi,
            book_delay: Duration,
        }

        #[async_trait::async_trait]
        impl HlApi for SlowBookApi {
            async fn fetch_l2_book(&self, s: &Symbol) -> Result<OrderBook, HlError> {
                tokio::time::sleep(self.book_delay).await;
                self.inner.fetch_l2_book(s).await
            }
            async fn place_order_once(
                &self,
                i: &OrderIntent,
                a: u32,
                e: u64,
            ) -> Result<(u64, PlaceOutcome), HlError> {
                self.inner.place_order_once(i, a, e).await
            }
            async fn cancel_by_cloid(&self, i: &CancelIntent, a: u32) -> Result<(), HlError> {
                self.inner.cancel_by_cloid(i, a).await
            }
            async fn fetch_order_status(
                &self,
                u: &Address,
                o: OrderId,
            ) -> Result<Option<OrderStatusFill>, HlError> {
                self.inner.fetch_order_status(u, o).await
            }
            async fn fetch_order_status_by_cloid(
                &self,
                u: &Address,
                c: Cloid,
            ) -> Result<Option<OrderStatusFill>, HlError> {
                self.inner.fetch_order_status_by_cloid(u, c).await
            }
        }

        // 1 slice over a 10s window. The book fetch takes 30s — well past the
        // deadline — reproducing the issue body's scenario directly: the
        // slice enters (passes the initial hard-window check at T=0 < 10s),
        // but the subsequent book fetch alone blows past the deadline before
        // a place could ever be attempted.
        let inner = ScriptedApi::new()
            .with_default_book(book_at(dec!(49.9), dec!(50.1)))
            .push_place(filled(dec!(5), dec!(50)));
        let api = SlowBookApi {
            inner,
            book_delay: Duration::from_secs(30),
        };

        let mut p = plan(false);
        p.slices = 1;
        p.per_slice = dec!(5);
        p.total_adjusted = dec!(5);
        p.total_requested = dec!(5);
        p.duration = Duration::from_secs(10);

        let report = run_twap(&api, &p).await;

        // The load-bearing assertion: zero places occur when the book fetch
        // alone spans past the monotonic deadline.
        assert_eq!(
            api.inner.place_count(),
            0,
            "a book fetch that cannot complete before the deadline must never reach a place"
        );
        let reason = report
            .abort_reason
            .clone()
            .expect("a book fetch stuck past the deadline must abort, not silently succeed");
        assert!(
            reason.contains("book fetch failed") || reason.contains("elapsed"),
            "{reason}"
        );
        assert_eq!(report.exit_code(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn issue2_single_fetch_exceeding_remaining_deadline_is_timed_out_not_awaited_to_completion(
    ) {
        // A single fetch_l2_book call that would take far longer than the
        // remaining deadline must be cut off by fetch_fresh_book itself (via
        // a timeout wrapped around the in-flight call), not merely allowed
        // to run to completion and rejected afterwards by some OUTER caller
        // (e.g. place_slice_reconciled's check_before_send). This is proven
        // by calling fetch_fresh_book directly (no run_twap, no outer
        // deadline gate in the picture) with a fetch delay (30s) much larger
        // than the remaining deadline (200ms), and asserting:
        //   1. it returns an error (the fetch never got to complete),
        //   2. it returns promptly relative to the fetch delay — i.e. it did
        //      NOT await the full 30s before giving up.
        struct SlowBookApi {
            inner: ScriptedApi,
            book_delay: Duration,
        }

        #[async_trait::async_trait]
        impl HlApi for SlowBookApi {
            async fn fetch_l2_book(&self, s: &Symbol) -> Result<OrderBook, HlError> {
                tokio::time::sleep(self.book_delay).await;
                self.inner.fetch_l2_book(s).await
            }
            async fn place_order_once(
                &self,
                i: &OrderIntent,
                a: u32,
                e: u64,
            ) -> Result<(u64, PlaceOutcome), HlError> {
                self.inner.place_order_once(i, a, e).await
            }
            async fn cancel_by_cloid(&self, i: &CancelIntent, a: u32) -> Result<(), HlError> {
                self.inner.cancel_by_cloid(i, a).await
            }
            async fn fetch_order_status(
                &self,
                u: &Address,
                o: OrderId,
            ) -> Result<Option<OrderStatusFill>, HlError> {
                self.inner.fetch_order_status(u, o).await
            }
            async fn fetch_order_status_by_cloid(
                &self,
                u: &Address,
                c: Cloid,
            ) -> Result<Option<OrderStatusFill>, HlError> {
                self.inner.fetch_order_status_by_cloid(u, c).await
            }
        }

        let inner = ScriptedApi::new().with_default_book(book_at(dec!(49.9), dec!(50.1)));
        let api = SlowBookApi {
            inner,
            book_delay: Duration::from_secs(30),
        };

        let start = tokio::time::Instant::now();
        // Only 200ms remaining — far less than the 30s the fetch would take.
        let dl =
            ExecutionDeadline::from_parts(start + Duration::from_millis(200), 1_700_000_000_000);

        let before = tokio::time::Instant::now();
        let result = fetch_fresh_book(&api, &Symbol::new("ETH"), 60_000, Some(&dl)).await;
        let elapsed = tokio::time::Instant::now() - before;

        assert!(
            result.is_err(),
            "a fetch that cannot complete within the remaining deadline must error, not succeed"
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "fetch_fresh_book must be cut off by the deadline itself (~200ms), not await the \
             full 30s in-flight fetch to completion; elapsed = {elapsed:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn issue2_fetch_fresh_book_retry_budget_is_capped_by_remaining_deadline_time() {
        // Directly exercises fetch_fresh_book's deadline-aware retry cap
        // (not through run_twap): an invalid book is returned every time, so
        // fetch_fresh_book would normally retry STALE_BOOK_RETRIES times at
        // STALE_BOOK_RETRY_INTERVAL (1s) each = ~3s. With only 200ms left on
        // the deadline, it must give up almost immediately rather than
        // spending the full ~3s retry budget.
        let bad_book = OrderBook {
            coin: "WRONG".to_string(), // fails ValidatedMarketSnapshot::validate
            bids: vec![],
            asks: vec![],
            time_ms: 0,
        };
        let api = ScriptedApi::new().with_default_book(bad_book);

        let start = tokio::time::Instant::now();
        let dl =
            ExecutionDeadline::from_parts(start + Duration::from_millis(200), 1_700_000_000_000);

        let began = tokio::time::Instant::now();
        let err = fetch_fresh_book(&api, &Symbol::new("HYPE"), 0, Some(&dl))
            .await
            .unwrap_err();
        let took = began.elapsed();

        assert!(matches!(err, HlError::InvalidResponse(_)));
        // Must give up close to the 200ms budget, nowhere near the ~3s the
        // full unbudgeted retry schedule would take.
        assert!(
            took <= Duration::from_millis(500),
            "retry budget was not capped by the deadline: took {took:?}"
        );
    }

    // === Issue #2 acceptance criterion 2: an ambiguous-send reconciliation
    // that crosses the deadline must not attempt a second place; the
    // original reconciliation (status polling) still runs to completion ===

    #[tokio::test(start_paused = true)]
    async fn issue2_ambiguous_send_crossing_deadline_does_not_resend() {
        // Slice 1's place comes back as a transport failure (ambiguous). The
        // RECONCILE_DELAY (500ms) plus the unknownOid streak accumulation
        // (UNKNOWN_OID_MIN_CONSECUTIVE observations spanning
        // UNKNOWN_OID_MIN_WINDOW = 2s) takes long enough to cross a very
        // short deadline. Reconciliation must still run to completion
        // (status polling is always allowed) and correctly conclude
        // "HL never received it" — but the resend that would normally follow
        // must NOT happen, because by the time reconciliation resolves the
        // deadline has passed.
        let api = ScriptedApi::new()
            .with_default_book(book_at(dec!(49.9), dec!(50.1)))
            .push_place(Err(HlError::Network("connection reset".into())))
            // unknownOid streak: 3 consecutive observations spanning >= 2s
            // (poll interval 1100ms) resolves reconciliation as "safe to
            // resend" — except the deadline will have passed by then.
            .push_status(Ok(None))
            .push_status(Ok(None))
            .push_status(Ok(None));

        let plan_val = plan(false);
        let intent = OrderIntent {
            cloid: Cloid::new(),
            symbol: plan_val.symbol.clone(),
            side: plan_val.side,
            px: dec!(50),
            sz: dec!(5),
            tif: Tif::Ioc,
            reduce_only: false,
        };

        // Deadline passes well before reconciliation can complete (which
        // takes RECONCILE_DELAY 500ms + at least 2 * 1100ms = ~2.7s), but
        // after the INITIAL check_before_send at the top of
        // place_slice_reconciled (which must be allowed to attempt the first
        // send at all, matching "sent exactly once" W1 semantics).
        let start = tokio::time::Instant::now();
        let dl =
            ExecutionDeadline::from_parts(start + Duration::from_millis(800), 1_700_000_000_000);

        let err = place_slice_reconciled(&api, &plan_val, &intent, &dl, 1, None)
            .await
            .unwrap_err();

        // Reconciliation ran to completion (all 3 status polls were consumed
        // — proven by place_slice_reconciled not erroring out of
        // reconcile_by_cloid itself, but instead reaching the deadline
        // re-check AFTER `Ok(None)` was returned).
        assert_eq!(
            api.place_count(),
            1,
            "exactly the original send — no resend after the deadline passed"
        );
        let msg = format!("{err}");
        assert!(
            msg.contains("deadline"),
            "the resend must be refused specifically for having crossed the \
             deadline, not some other error: {msg}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn issue2_ambiguous_send_resend_still_happens_within_the_deadline() {
        // Control case for the test above: same scenario, but with a
        // deadline generous enough that reconciliation resolves safely
        // BEFORE it passes. The resend must still happen — Issue #2 must not
        // regress the Issue #7/W1 safe-resend policy for healthy runs.
        let api = ScriptedApi::new()
            .with_default_book(book_at(dec!(49.9), dec!(50.1)))
            .push_place(Err(HlError::Network("connection reset".into())))
            .push_status(Ok(None))
            .push_status(Ok(None))
            .push_status(Ok(None))
            .push_place(filled(dec!(5), dec!(50)));

        let plan_val = plan(false);
        let intent = OrderIntent {
            cloid: Cloid::new(),
            symbol: plan_val.symbol.clone(),
            side: plan_val.side,
            px: dec!(50),
            sz: dec!(5),
            tif: Tif::Ioc,
            reduce_only: false,
        };

        let start = tokio::time::Instant::now();
        let dl = ExecutionDeadline::from_parts(start + Duration::from_secs(60), 1_700_000_000_000);

        let outcome = place_slice_reconciled(&api, &plan_val, &intent, &dl, 1, None)
            .await
            .unwrap();

        assert_eq!(outcome.sz, dec!(5));
        assert_eq!(
            api.place_count(),
            2,
            "the original send plus exactly one resend"
        );
    }

    // === Issue #2 acceptance criterion 3: expiresAfter is threaded to every
    // place, including a resend, with the SAME run-level value ===

    #[tokio::test(start_paused = true)]
    async fn issue2_every_place_including_a_resend_uses_the_same_run_level_expires_after() {
        let api = ScriptedApi::new()
            .with_default_book(book_at(dec!(49.9), dec!(50.1)))
            .push_place(Err(HlError::Network("connection reset".into())))
            .push_status(Ok(None))
            .push_status(Ok(None))
            .push_status(Ok(None))
            .push_place(filled(dec!(5), dec!(50)));

        let plan_val = plan(false);
        let intent = OrderIntent {
            cloid: Cloid::new(),
            symbol: plan_val.symbol.clone(),
            side: plan_val.side,
            px: dec!(50),
            sz: dec!(5),
            tif: Tif::Ioc,
            reduce_only: false,
        };

        let start = tokio::time::Instant::now();
        let dl = ExecutionDeadline::from_parts(start + Duration::from_secs(60), 1_700_123_456_789);

        place_slice_reconciled(&api, &plan_val, &intent, &dl, 1, None)
            .await
            .unwrap();

        let places = api.place_calls();
        assert_eq!(places.len(), 2, "original send + 1 resend");
        for c in places {
            if let Call::Place {
                expires_after_ms, ..
            } = c
            {
                assert_eq!(
                    expires_after_ms, 1_700_123_456_789,
                    "a resend must reuse the SAME run-level expiry, not a fresh one"
                );
            }
        }
    }

    // === (b) T3/T5: resting-order recovery ===

    #[tokio::test(start_paused = true)]
    async fn b_t3_resting_partial_fill_is_credited_once_from_the_terminal_status() {
        // Slice 1 rests. The first orderStatus says "open" with 2 filled — that
        // is a LIVE order which can still fill, so it must NOT be adopted. The
        // second says "canceled" with 3 filled: terminal, and the value that
        // counts. Adopting the "open" 2 would under-count and make every later
        // slice over-order.
        let mut api = ScriptedApi::new()
            .with_default_book(book_at(dec!(49.9), dec!(50.1)))
            .push_place(Ok(PlaceOutcome::Resting { oid: OrderId(77) }))
            .push_status(Ok(Some(status(dec!(2), Some(dec!(49.5)), "open"))))
            .push_status(Ok(Some(status(dec!(3), Some(dec!(49.5)), "canceled"))));
        for _ in 0..9 {
            api = api.push_place(filled(dec!(5), dec!(50)));
        }

        let report = run_twap(&api, &plan(false)).await;

        // Credited exactly once, at the TERMINAL value (3), never the open 2.
        let places: Vec<Decimal> = api
            .place_calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::Place { sz, .. } => Some(sz),
                _ => None,
            })
            .collect();
        assert_eq!(places[0], dec!(5), "slice 1 ordered its full share");
        // Slice 2 catches up the 2-coin shortfall: target 10 - filled 3 = 7.
        assert_eq!(
            places[1],
            dec!(7),
            "slice 2 must catch up from the terminal fill of 3, not from 2"
        );

        // The status was queried as the MASTER (F1), not the agent.
        let status_users: Vec<String> = api
            .calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::StatusByOid { user, .. } | Call::StatusByCloid { user, .. } => Some(user),
                _ => None,
            })
            .collect();
        assert!(!status_users.is_empty());
        for u in status_users {
            assert_eq!(u, MASTER, "orderStatus must query the master (F1)");
        }
        assert_eq!(report.abort_reason, None);
    }

    #[tokio::test(start_paused = true)]
    async fn b_t3_never_terminal_status_hard_stops_rather_than_guess() {
        // orderStatus stays "open" for every retry. The fill count is genuinely
        // unknown, so the run must stop — guessing low over-orders on every
        // later slice, guessing high under-executes silently.
        let mut api = ScriptedApi::new()
            .with_default_book(book_at(dec!(49.9), dec!(50.1)))
            .push_place(Ok(PlaceOutcome::Resting { oid: OrderId(77) }));
        for _ in 0..ORDER_STATUS_RETRIES {
            api = api.push_status(Ok(Some(status(dec!(2), None, "open"))));
        }

        let report = run_twap(&api, &plan(false)).await;

        let reason = report.abort_reason.clone().expect("must hard-stop");
        assert!(reason.contains("terminal"), "{reason}");
        assert_eq!(report.exit_code(), 1);
        // Nothing was credited from the non-terminal snapshot.
        assert_eq!(report.filled, Decimal::ZERO);
        assert_eq!(api.place_count(), 1, "must not place further slices");
    }

    #[tokio::test(start_paused = true)]
    async fn b_t5_recovered_fill_is_priced_at_the_reported_avg_px_not_the_limit() {
        // T5: the recovered fill is credited at HL's realised avgPx (49.50),
        // not at our limit price (~50.2, the worst price it could have got).
        // Crediting the limit skews the avg-price report against us.
        let api = ScriptedApi::new()
            .with_default_book(book_at(dec!(49.9), dec!(50.1)))
            .push_place(Ok(PlaceOutcome::Resting { oid: OrderId(77) }))
            .push_status(Ok(Some(status(dec!(5), Some(dec!(49.5)), "filled"))));

        let mut p = plan(false);
        p.slices = 1;
        p.per_slice = dec!(5);
        p.total_adjusted = dec!(5);
        p.total_requested = dec!(5);

        let report = run_twap(&api, &p).await;

        assert_eq!(report.filled, dec!(5));
        assert_eq!(
            report.avg_px,
            Some(dec!(49.5)),
            "must use orderStatus avgPx, not the limit price"
        );
        let limit = taker_limit_price(dec!(49.9), dec!(50.1), Side::Long, dec!(20), 2);
        assert!(
            report.avg_px.unwrap() < limit,
            "avg {:?} should be better than the limit {limit}",
            report.avg_px
        );
    }

    #[tokio::test(start_paused = true)]
    async fn b_t5_missing_avg_px_falls_back_to_the_limit_price() {
        // HL omits avgPx for orders that never filled. With filled_sz 0 the
        // price is immaterial, but the code must not panic or credit nonsense.
        let api = ScriptedApi::new()
            .with_default_book(book_at(dec!(49.9), dec!(50.1)))
            .push_place(Ok(PlaceOutcome::Resting { oid: OrderId(77) }))
            .push_status(Ok(Some(status(dec!(0), None, "canceled"))));

        let mut p = plan(false);
        p.slices = 1;
        p.per_slice = dec!(5);
        p.total_adjusted = dec!(5);
        p.total_requested = dec!(5);

        let report = run_twap(&api, &p).await;
        assert_eq!(report.filled, Decimal::ZERO);
        assert_eq!(report.avg_px, None);
        assert_eq!(report.abort_reason, None, "a zero fill is not an abort");
    }

    // === (c) T1: a mid-run price drop pushes a slice under the floor ===

    #[tokio::test(start_paused = true)]
    async fn c_t1_slice_under_the_floor_is_skipped_and_carried_to_the_next() {
        // Slice 1 places 0.3 at ~$50 (~$15, clear of the gate). Then price
        // collapses to ~$25, so slice 2's 0.3 is worth only ~$7.5 — under the
        // gate. It must be SKIPPED (not sent and rejected), and because targets
        // are cumulative the quantity carries forward: slice 3 orders 0.6 at the
        // recovered price.
        let api = ScriptedApi::new()
            .push_book(Ok(book_at(dec!(49.9), dec!(50.1))))
            .push_place(filled(dec!(0.3), dec!(50)))
            .push_book(Ok(book_at(dec!(24.9), dec!(25.1)))) // crash
            .push_book(Ok(book_at(dec!(49.9), dec!(50.1)))) // recovery
            .push_place(filled(dec!(0.6), dec!(50)))
            .with_default_book(book_at(dec!(49.9), dec!(50.1)));

        let mut p = plan(false);
        p.slices = 3;
        p.per_slice = dec!(0.3);
        p.total_adjusted = dec!(0.9);
        p.total_requested = dec!(0.9);

        let report = run_twap(&api, &p).await;

        let sizes: Vec<Decimal> = api
            .place_calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::Place { sz, .. } => Some(sz),
                _ => None,
            })
            .collect();
        assert_eq!(
            sizes,
            vec![dec!(0.3), dec!(0.6)],
            "slice 2 must be skipped and its size carried into slice 3"
        );
        assert_eq!(report.slices_executed, 2);
        assert_eq!(report.slices_skipped, 1);
        assert_eq!(report.filled, dec!(0.9));
        assert_eq!(report.abort_reason, None);
    }

    #[tokio::test(start_paused = true)]
    async fn c_t1_short_slice_between_the_two_gates_is_not_sent() {
        // The counter-example from the review, driven through the whole loop.
        //
        // A SHORT's taker limit sits BELOW the mid, so there is a band of sizes
        // where the mid says "fine" and the price that actually reaches HL is
        // under the floor. This test sits squarely in that band, which is what
        // makes it able to tell the two gates apart — with a LONG the limit is
        // ABOVE the mid and both gates agree, so no long test can detect this.
        //
        // bid=40 / ask=60 → mid 50. With 500bps of slippage the short limit is
        // 40 - 5% = 38, a wide spread chosen to make the band easy to land in.
        // sz=0.26 is worth $13.00 at the mid — comfortably over the $10.10 gate
        // — but 0.26 × 38 = $9.88 at the price that actually reaches HL, which
        // comes back as a fatal MinTradeNtl and stops the entire run.
        let api = ScriptedApi::new().with_default_book(book_at(dec!(40), dec!(60)));
        // No place is scripted: if the loop tries to send, the fake errors and
        // the assertions below fail loudly.

        let mut p = plan(false);
        p.side = Side::Short;
        p.slippage_bps = dec!(500);
        p.slices = 1;
        p.per_slice = dec!(0.26);
        p.total_adjusted = dec!(0.26);
        p.total_requested = dec!(0.26);

        // Pin the arithmetic this test depends on, so a change to the price
        // rounding cannot silently move the case out of the discriminating band.
        let limit = taker_limit_price(dec!(40), dec!(60), Side::Short, dec!(500), 2);
        assert_eq!(limit, dec!(38));
        assert!(
            dec!(0.26) * dec!(50) > min_notional_gate(),
            "the mid gate must PASS, or this test proves nothing"
        );
        assert!(
            dec!(0.26) * limit < MIN_NOTIONAL_USD,
            "the real order price must be under HL's floor"
        );

        let report = run_twap(&api, &p).await;

        assert_eq!(
            api.place_count(),
            0,
            "a short slice worth $9.88 at its own limit price must not be sent, \
             even though it looks like $13.00 at the mid"
        );
        assert_eq!(report.slices_skipped, 1);
        assert_eq!(report.slices_executed, 0);
        assert_eq!(report.filled, Decimal::ZERO);
        // Skipping is not an abort — the residual is simply unexecutable.
        assert_eq!(report.abort_reason, None);
        assert_eq!(report.exit_code(), 0);
    }

    // === (e) read-only accounting ===

    #[tokio::test(start_paused = true)]
    async fn e_read_only_assumes_full_fills_and_sends_nothing() {
        let api = ScriptedApi::new().with_default_book(book_at(dec!(49.9), dec!(50.1)));
        // No places scripted at all: any attempt to send would error out.

        let report = run_twap(&api, &plan(true)).await;

        assert_eq!(api.place_count(), 0, "read-only must send NOTHING");
        assert!(
            !api.calls().iter().any(|c| matches!(
                c,
                Call::Cancel { .. } | Call::StatusByOid { .. } | Call::StatusByCloid { .. }
            )),
            "read-only must not touch the exchange or orderStatus"
        );
        // The dry run walks the same accounting path, assuming full fills.
        assert_eq!(report.filled, dec!(50));
        assert_eq!(report.slices_executed, 10);
        assert!(report.read_only);
        assert_eq!(report.exit_code(), 0);
        assert!(report.render().contains("READ-ONLY"));
    }

    // === (f) early exit once the target is reached ===

    #[tokio::test(start_paused = true)]
    async fn f_run_breaks_early_once_filled_reaches_the_adjusted_total() {
        // Single-slice plan whose one order fills exactly its (also the
        // run's) full target. The loop must stop immediately after slice 1
        // rather than attempt a slice 2 that does not exist.
        //
        // Issue #7: a fill can never legitimately exceed the intent's own
        // signed `sz`, so this no longer scripts a "slice fills the whole
        // 50-coin run target in one 5-coin order" scenario — that shape is
        // now (correctly) a hard-error overfill, covered separately by
        // `w7_overfill_relative_to_intent_hard_stops_before_next_slice`.
        let api = ScriptedApi::new()
            .with_default_book(book_at(dec!(49.9), dec!(50.1)))
            .push_place(filled(dec!(50), dec!(50)));

        let mut p = plan(false);
        p.slices = 1;
        p.per_slice = dec!(50);
        p.total_adjusted = dec!(50);
        p.total_requested = dec!(50);

        let report = run_twap(&api, &p).await;

        assert_eq!(api.place_count(), 1, "must break after the target is met");
        assert_eq!(report.filled, dec!(50));
        assert_eq!(report.slices_executed, 1);
        assert_eq!(report.abort_reason, None);
        assert!(!report.is_partial());
    }

    // === W1: ambiguous transport failure is reconciled by cloid ===

    #[tokio::test(start_paused = true)]
    async fn w1_ambiguous_send_that_actually_landed_is_credited_without_a_resend() {
        // The POST timed out AFTER HL received it. Reconciling by cloid finds a
        // terminal fill, so the order must be credited and NOT re-sent — a
        // resend here would double the position.
        let api = ScriptedApi::new()
            .with_default_book(book_at(dec!(49.9), dec!(50.1)))
            .push_place(Err(HlError::Network("operation timed out".into())))
            .push_status(Ok(Some(status(dec!(5), Some(dec!(50.05)), "filled"))));

        let mut p = plan(false);
        p.slices = 1;
        p.per_slice = dec!(5);
        p.total_adjusted = dec!(5);
        p.total_requested = dec!(5);

        let report = run_twap(&api, &p).await;

        assert_eq!(api.place_count(), 1, "must NOT resend an order HL received");
        assert_eq!(report.filled, dec!(5));
        assert_eq!(report.avg_px, Some(dec!(50.05)));
        assert_eq!(report.abort_reason, None);

        // Reconciliation used the cloid of the order we sent, as the master.
        let sent_cloid = match api.place_calls().first() {
            Some(Call::Place { cloid, .. }) => *cloid,
            other => panic!("expected a place call, got {other:?}"),
        };
        assert!(
            api.calls().iter().any(|c| matches!(
                c,
                Call::StatusByCloid { user, cloid } if user == MASTER && *cloid == sent_cloid
            )),
            "reconciliation must query the master by the sent cloid"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn w1_unknown_oid_means_hl_never_got_it_so_a_fresh_nonce_resend_is_safe() {
        // The POST failed BEFORE HL saw it. Issue #7's tightened policy
        // requires >= 3 CONSECUTIVE unknownOid observations spanning >= 2s
        // before a resend is considered safe — a single `unknownOid` is no
        // longer enough, since HL's orderStatus carries no documented
        // read-after-write guarantee. Once the streak clears both
        // thresholds, re-signing with a FRESH nonce is safe and required.
        let api = ScriptedApi::new()
            .with_default_book(book_at(dec!(49.9), dec!(50.1)))
            .push_place(Err(HlError::Network("connection refused".into())))
            .push_status(Ok(None)) // unknownOid #1
            .push_status(Ok(None)) // unknownOid #2
            .push_status(Ok(None)) // unknownOid #3 — streak + 2s window clear
            .push_place(filled(dec!(5), dec!(50)));

        let mut p = plan(false);
        p.slices = 1;
        p.per_slice = dec!(5);
        p.total_adjusted = dec!(5);
        p.total_requested = dec!(5);

        let report = run_twap(&api, &p).await;

        assert_eq!(report.filled, dec!(5));
        assert_eq!(report.abort_reason, None);

        let nonces: Vec<u64> = api
            .place_calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::Place { nonce, .. } => Some(nonce),
                _ => None,
            })
            .collect();
        assert_eq!(nonces.len(), 2, "should have resent exactly once");
        assert!(
            nonces[1] > nonces[0],
            "the resend must use a FRESH nonce ({:?}); reusing the signed body \
             could only ever be rejected as stale",
            nonces
        );
    }

    #[tokio::test(start_paused = true)]
    async fn w1_unresolvable_ambiguity_hard_stops_instead_of_guessing() {
        // The send failed and reconciliation cannot establish what happened.
        // The order may or may not be live, so the only safe move is to stop
        // and tell the operator to check their fills. Every reconciliation
        // attempt errors, so the streak never accumulates and the attempt
        // budget (`UNKNOWN_OID_MAX_ATTEMPTS`) is what ends the loop.
        let mut api = ScriptedApi::new()
            .with_default_book(book_at(dec!(49.9), dec!(50.1)))
            .push_place(Err(HlError::Network("timed out".into())));
        for _ in 0..UNKNOWN_OID_MAX_ATTEMPTS {
            api = api.push_status(Err(HlError::Network("info also down".into())));
        }

        let mut p = plan(false);
        p.slices = 1;
        p.per_slice = dec!(5);
        p.total_adjusted = dec!(5);
        p.total_requested = dec!(5);

        let report = run_twap(&api, &p).await;

        assert_eq!(api.place_count(), 1, "must not blind-resend");
        assert_eq!(report.filled, Decimal::ZERO);
        let reason = report
            .abort_reason
            .clone()
            .expect("must hard-stop on ambiguity");
        assert!(reason.contains("UNKNOWN"), "{reason}");
        assert!(reason.contains("check your fills"), "{reason}");
        assert_eq!(report.exit_code(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn w1_resend_is_bounded_and_gives_up_safely() {
        // HL keeps not receiving the order — each attempt clears a full
        // 3-consecutive/2s unknownOid streak, so the resend is judged safe
        // every time. The OUTER resend-attempt loop must still be bounded
        // rather than hammering the exchange forever.
        let mut api = ScriptedApi::new().with_default_book(book_at(dec!(49.9), dec!(50.1)));
        for _ in 0..(PLACE_RESEND_LIMIT + 1) {
            api = api
                .push_place(Err(HlError::Network("connection refused".into())))
                .push_status(Ok(None))
                .push_status(Ok(None))
                .push_status(Ok(None));
        }

        let mut p = plan(false);
        p.slices = 1;
        p.per_slice = dec!(5);
        p.total_adjusted = dec!(5);
        p.total_requested = dec!(5);

        let report = run_twap(&api, &p).await;

        assert_eq!(
            api.place_count() as u32,
            PLACE_RESEND_LIMIT + 1,
            "1 initial send + {PLACE_RESEND_LIMIT} resends, then stop"
        );
        assert_eq!(report.filled, Decimal::ZERO);
        assert!(report.abort_reason.is_some());
        assert_eq!(report.exit_code(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn w1_exchange_rejection_is_a_decision_not_ambiguity_and_never_reconciles() {
        // A rejection is HL's final answer. Reconciling or resending it would
        // be pointless at best and a duplicate order at worst.
        let api = ScriptedApi::new()
            .with_default_book(book_at(dec!(49.9), dec!(50.1)))
            .push_place(Err(HlError::Exchange {
                code: Some("order_error".into()),
                message: "Insufficient margin".into(),
            }));

        let report = run_twap(&api, &plan(false)).await;

        assert_eq!(api.place_count(), 1, "a rejection must never be resent");
        assert!(
            !api.calls()
                .iter()
                .any(|c| matches!(c, Call::StatusByCloid { .. })),
            "a rejection is unambiguous; no reconciliation should occur"
        );
        let reason = report.abort_reason.clone().expect("must abort");
        assert!(reason.contains("rejected by exchange"), "{reason}");
        assert!(reason.contains("insufficient margin"), "{reason}");
        assert_eq!(report.exit_code(), 1);
    }

    // === F1: a missing master is a config error, never a silent agent query ===

    #[tokio::test(start_paused = true)]
    async fn f1_recovery_without_a_resolved_master_is_a_hard_error() {
        let api = ScriptedApi::new()
            .with_default_book(book_at(dec!(49.9), dec!(50.1)))
            .push_place(Ok(PlaceOutcome::Resting { oid: OrderId(77) }));

        let mut p = plan(false);
        p.master = None; // probe never ran
        p.slices = 1;
        p.per_slice = dec!(5);
        p.total_adjusted = dec!(5);
        p.total_requested = dec!(5);

        let report = run_twap(&api, &p).await;

        let reason = report
            .abort_reason
            .clone()
            .expect("must abort without a master");
        assert!(reason.contains("master address"), "{reason}");
        assert_eq!(report.exit_code(), 1);
    }

    // === Issue #7: exchange responses are a trusted boundary ===

    /// A single-slice plan whose slice size matches the plan's `total_adjusted`,
    /// so any `PlaceOutcome::Filled` with `total_sz` above `per_slice` is a
    /// genuine overfill relative to the signed `OrderIntent.sz`.
    fn single_slice_plan() -> TwapPlan {
        let mut p = plan(false);
        p.slices = 1;
        p.per_slice = dec!(5);
        p.total_adjusted = dec!(5);
        p.total_requested = dec!(5);
        p
    }

    #[tokio::test(start_paused = true)]
    async fn w7_overfill_relative_to_intent_hard_stops_before_next_slice() {
        // HL reports totalSz greater than the sz we signed. This must never
        // be clamped or partially credited — it is a hard-stop, and no
        // further slice may be attempted.
        let api = ScriptedApi::new()
            .with_default_book(book_at(dec!(49.9), dec!(50.1)))
            .push_place(filled(dec!(5.01), dec!(50)));

        let report = run_twap(&api, &single_slice_plan()).await;

        assert_eq!(
            api.place_count(),
            1,
            "no further slice after the bad response"
        );
        assert_eq!(
            report.filled,
            Decimal::ZERO,
            "the bad fill must not be credited"
        );
        let reason = report.abort_reason.clone().expect("must hard-stop");
        assert!(reason.contains("exceeds intent size"), "{reason}");
        assert_eq!(report.exit_code(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn w7_zero_avg_px_on_a_nonzero_fill_hard_stops_before_next_slice() {
        let api = ScriptedApi::new()
            .with_default_book(book_at(dec!(49.9), dec!(50.1)))
            .push_place(filled(dec!(5), dec!(0)));

        let report = run_twap(&api, &single_slice_plan()).await;

        assert_eq!(
            api.place_count(),
            1,
            "no further slice after the bad response"
        );
        assert_eq!(report.filled, Decimal::ZERO);
        let reason = report.abort_reason.clone().expect("must hard-stop");
        assert!(reason.contains("not positive"), "{reason}");
        assert_eq!(report.exit_code(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn w7_negative_avg_px_hard_stops_before_next_slice() {
        let api = ScriptedApi::new()
            .with_default_book(book_at(dec!(49.9), dec!(50.1)))
            .push_place(filled(dec!(5), dec!(-1)));

        let report = run_twap(&api, &single_slice_plan()).await;

        assert_eq!(api.place_count(), 1);
        assert_eq!(report.filled, Decimal::ZERO);
        assert!(report.abort_reason.is_some());
        assert_eq!(report.exit_code(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn w7_long_fill_priced_above_the_limit_hard_stops_before_next_slice() {
        // plan() is Side::Long. taker_limit_price will be computed and
        // signed as intent.px; a reported avgPx above that limit means the
        // response does not describe an order we could have gotten.
        let mut p = single_slice_plan();
        p.slices = 3; // so a hard-stop-before-next-slice is observable
        p.per_slice = dec!(5);
        p.total_adjusted = dec!(15);
        p.total_requested = dec!(15);
        let limit = taker_limit_price(dec!(49.9), dec!(50.1), Side::Long, dec!(20), 2);

        let api = ScriptedApi::new()
            .with_default_book(book_at(dec!(49.9), dec!(50.1)))
            .push_place(filled(dec!(5), limit + dec!(1)));

        let report = run_twap(&api, &p).await;

        assert_eq!(
            api.place_count(),
            1,
            "must hard-stop before any further slice is attempted"
        );
        assert_eq!(report.filled, Decimal::ZERO);
        let reason = report.abort_reason.clone().expect("must hard-stop");
        assert!(reason.contains("long avgPx"), "{reason}");
        assert_eq!(report.exit_code(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn w7_short_fill_priced_below_the_limit_hard_stops_before_next_slice() {
        let mut p = single_slice_plan();
        p.side = Side::Short;
        p.slices = 3;
        p.per_slice = dec!(5);
        p.total_adjusted = dec!(15);
        p.total_requested = dec!(15);
        let limit = taker_limit_price(dec!(49.9), dec!(50.1), Side::Short, dec!(20), 2);

        let api = ScriptedApi::new()
            .with_default_book(book_at(dec!(49.9), dec!(50.1)))
            .push_place(filled(dec!(5), limit - dec!(1)));

        let report = run_twap(&api, &p).await;

        assert_eq!(api.place_count(), 1);
        assert_eq!(report.filled, Decimal::ZERO);
        let reason = report.abort_reason.clone().expect("must hard-stop");
        assert!(reason.contains("short avgPx"), "{reason}");
        assert_eq!(report.exit_code(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn w7_recovered_resting_fill_overfill_hard_stops_before_next_slice() {
        // The overfill check applies equally to the resting-order recovery
        // path (T3/T5), not just the direct Filled path.
        let api = ScriptedApi::new()
            .with_default_book(book_at(dec!(49.9), dec!(50.1)))
            .push_place(Ok(PlaceOutcome::Resting { oid: OrderId(77) }))
            .push_status(Ok(Some(status(dec!(5.5), Some(dec!(49.5)), "filled"))));

        let report = run_twap(&api, &single_slice_plan()).await;

        assert_eq!(
            api.place_count(),
            1,
            "no further slice after the bad response"
        );
        assert_eq!(report.filled, Decimal::ZERO);
        let reason = report.abort_reason.clone().expect("must hard-stop");
        assert!(reason.contains("exceeds intent size"), "{reason}");
        assert_eq!(report.exit_code(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn w7_ioc_cancel_rejected_zero_fill_settles_cleanly_no_resend_no_hard_stop() {
        // Acceptance criteria: iocCancelRejected with a zero fill is a
        // normal SETTLED outcome, not an error and not ambiguous. This
        // exercises it through the resting-order recovery path exactly as a
        // real IOC-rests-then-gets-cancelled sequence would.
        let api = ScriptedApi::new()
            .with_default_book(book_at(dec!(49.9), dec!(50.1)))
            .push_place(Ok(PlaceOutcome::Resting { oid: OrderId(77) }))
            .push_status(Ok(Some(status(dec!(0), None, "iocCancelRejected"))));

        let mut p = plan(false);
        p.slices = 1;
        p.per_slice = dec!(5);
        p.total_adjusted = dec!(5);
        p.total_requested = dec!(5);

        let report = run_twap(&api, &p).await;

        assert_eq!(report.filled, Decimal::ZERO);
        assert_eq!(
            report.abort_reason, None,
            "a zero-fill iocCancelRejected is settled, not an abort"
        );
        assert_eq!(report.exit_code(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn w7_remaining_greater_than_orig_sz_from_recovery_hard_stops_before_next_slice() {
        // The malformed orderStatus response (remaining > origSz) surfaces
        // as a parse-time InvalidResponse from `fetch_order_status` on every
        // retry attempt (a malformed response does not self-correct), which
        // must propagate as a hard stop with zero further slices — never
        // silently clamped to a zero fill (the pre-Issue-#7 behaviour).
        let malformed = || {
            Err(HlError::InvalidResponse(
                "orderStatus: remaining 15 exceeds origSz 10 (oid Some(77)) — \
                 malformed response, refusing to clamp"
                    .into(),
            ))
        };
        let mut api = ScriptedApi::new()
            .with_default_book(book_at(dec!(49.9), dec!(50.1)))
            .push_place(Ok(PlaceOutcome::Resting { oid: OrderId(77) }));
        for _ in 0..ORDER_STATUS_RETRIES {
            api = api.push_status(malformed());
        }

        let report = run_twap(&api, &single_slice_plan()).await;

        assert_eq!(
            api.place_count(),
            1,
            "no further slice after the bad response"
        );
        assert_eq!(report.filled, Decimal::ZERO);
        let reason = report.abort_reason.clone().expect("must hard-stop");
        assert!(reason.contains("exceeds origSz"), "{reason}");
        assert_eq!(report.exit_code(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn w7_unknown_status_from_recovery_is_fail_closed_no_further_placement() {
        // A status HL adds later (not in our vocabulary) must never be
        // adopted as terminal — the recovery loop keeps retrying, and
        // exhausting the retry budget without ever seeing a KNOWN terminal
        // status is a hard stop, never a guessed credit.
        let mut api = ScriptedApi::new()
            .with_default_book(book_at(dec!(49.9), dec!(50.1)))
            .push_place(Ok(PlaceOutcome::Resting { oid: OrderId(77) }));
        for _ in 0..ORDER_STATUS_RETRIES {
            api = api.push_status(Ok(Some(status(
                dec!(3),
                Some(dec!(49.5)),
                "someBrandNewStatusHlAddsLater",
            ))));
        }

        let report = run_twap(&api, &single_slice_plan()).await;

        assert_eq!(
            api.place_count(),
            1,
            "no further placement on an unknown status"
        );
        assert_eq!(report.filled, Decimal::ZERO);
        assert!(report.abort_reason.is_some());
        assert_eq!(report.exit_code(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn w7_delayed_visibility_unknown_then_terminal_causes_no_duplicate_place() {
        // Delayed-visibility test (Issue #7 acceptance criteria): the first
        // several `orderStatus(cloid)` calls after an ambiguous send return
        // unknownOid — not enough to clear the 3-consecutive/2s threshold —
        // and then a later call returns a terminal fill. The resend must NOT
        // fire, even though the early unknownOid observations looked like a
        // safe-resend candidate in progress.
        let api = ScriptedApi::new()
            .with_default_book(book_at(dec!(49.9), dec!(50.1)))
            .push_place(Err(HlError::Network("operation timed out".into())))
            .push_status(Ok(None)) // unknownOid #1
            .push_status(Ok(None)) // unknownOid #2 — streak in progress
            .push_status(Ok(Some(status(dec!(5), Some(dec!(50.05)), "filled"))));

        let report = run_twap(&api, &single_slice_plan()).await;

        assert_eq!(
            api.place_count(),
            1,
            "the order became visible before the safe-resend threshold cleared; \
             must NOT resend and must NOT duplicate the place"
        );
        assert_eq!(report.filled, dec!(5));
        assert_eq!(report.avg_px, Some(dec!(50.05)));
        assert_eq!(report.abort_reason, None);
        assert_eq!(report.exit_code(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn w7_unknown_oid_streak_broken_by_a_live_response_aborts_outcome_unknown() {
        // A mixed sequence — unknownOid, unknownOid, then a LIVE (non-
        // terminal) response — must reset the streak and never be treated
        // as a safe-resend basis, even though 2 consecutive unknownOid
        // observations had already been seen.
        let mut api = ScriptedApi::new()
            .with_default_book(book_at(dec!(49.9), dec!(50.1)))
            .push_place(Err(HlError::Network("timed out".into())))
            .push_status(Ok(None)) // unknownOid #1
            .push_status(Ok(None)) // unknownOid #2 — streak in progress
            .push_status(Ok(Some(status(dec!(1), Some(dec!(50)), "open")))); // live, non-terminal
        for _ in 0..(UNKNOWN_OID_MAX_ATTEMPTS - 3) {
            api = api.push_status(Ok(Some(status(dec!(1), Some(dec!(50)), "open"))));
        }

        let report = run_twap(&api, &single_slice_plan()).await;

        assert_eq!(api.place_count(), 1, "must not resend on a mixed sequence");
        assert_eq!(report.filled, Decimal::ZERO);
        let reason = report
            .abort_reason
            .clone()
            .expect("must abort outcome-unknown");
        assert!(reason.contains("check your fills"), "{reason}");
        assert_eq!(report.exit_code(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn w7_orderstatus_cross_check_rejects_a_coin_mismatch_and_hard_stops() {
        // The response claims a different coin than the one we queried
        // about — a defence against a proxy/cache serving the wrong id or a
        // client bug, not something HL itself would normally do.
        let mismatched = status_full(
            dec!(5),
            Some(dec!(49.9)),
            "filled",
            OrderId(77),
            None,
            "BTC", // plan is HYPE
            "B",
        );
        let api = ScriptedApi::new()
            .with_default_book(book_at(dec!(49.9), dec!(50.1)))
            .push_place(Ok(PlaceOutcome::Resting { oid: OrderId(77) }))
            .push_status(Ok(Some(mismatched)));

        let report = run_twap(&api, &single_slice_plan()).await;

        assert_eq!(
            api.place_count(),
            1,
            "no further slice after a cross-check failure"
        );
        assert_eq!(report.filled, Decimal::ZERO);
        let reason = report.abort_reason.clone().expect("must hard-stop");
        assert!(reason.contains("coin mismatch"), "{reason}");
        assert_eq!(report.exit_code(), 1);
    }

    // === Issue #3: per-slice notional cap re-check, long/short x usd/size ===
    //
    // Book fixed at bid=49.9 / ask=50.1 for all four scenarios below.
    // slippage_bps=20 (the plan() default) puts:
    //   long px  = ask * 1.002, rounded up   = 50.2002 -> 50.201 (szDecimals=2)
    //   short px = bid * 0.998, rounded down = 49.8002 -> 49.8  (szDecimals=2)
    // Each scenario sets `per_slice` (single slice) so that
    // `per_slice * order_px` sits just under/over `max_notional_usd`, then
    // asserts accept (order placed, no abort) vs reject (abort before any
    // `/exchange` call — `api.place_count() == 0`).
    //
    // "usd" vs "size" scenarios differ only in how the operator originally
    // specified the notional (main.rs resolves both into a coin `per_slice`
    // before the plan is built) — the loop's re-check logic itself does not
    // distinguish them, so all four exercise the identical `check_notional_cap`
    // call site with different `per_slice`/`max_notional_usd` pairs, matching
    // how each origin would concretely land on this boundary.

    fn plan_with_cap(side: Side, per_slice: Decimal, max_notional_usd: Decimal) -> TwapPlan {
        let mut p = plan(false);
        p.side = side;
        p.slices = 1;
        p.per_slice = per_slice;
        p.total_adjusted = per_slice;
        p.total_requested = per_slice;
        p.max_notional_usd = max_notional_usd;
        p
    }

    #[tokio::test(start_paused = true)]
    async fn notional_cap_long_usd_just_under_cap_is_accepted() {
        // long px = 50.201; per_slice sized so notional is just under $10000.
        let per_slice = dec!(199); // 199 * 50.201 = 9989.999
        let plan = plan_with_cap(Side::Long, per_slice, dec!(10000));
        let api = ScriptedApi::new()
            .with_default_book(book_at(dec!(49.9), dec!(50.1)))
            .push_place(filled(per_slice, dec!(50.201)));

        let report = run_twap(&api, &plan).await;

        assert_eq!(api.place_count(), 1, "order must be placed");
        assert!(report.abort_reason.is_none(), "{:?}", report.abort_reason);
        assert_eq!(report.filled, per_slice);
    }

    #[tokio::test(start_paused = true)]
    async fn notional_cap_long_usd_just_over_cap_is_rejected() {
        // long px = 50.201; per_slice sized so notional is just over $10000.
        let per_slice = dec!(200); // 200 * 50.201 = 10040.2
        let plan = plan_with_cap(Side::Long, per_slice, dec!(10000));
        let api = ScriptedApi::new().with_default_book(book_at(dec!(49.9), dec!(50.1)));

        let report = run_twap(&api, &plan).await;

        assert_eq!(
            api.place_count(),
            0,
            "notional cap must reject before any /exchange call"
        );
        let reason = report.abort_reason.clone().expect("must abort");
        assert!(reason.contains("notional"), "{reason}");
        assert_eq!(report.exit_code(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn notional_cap_long_size_just_under_cap_is_accepted() {
        // Same math as the usd case — "size"-origin plans reach the loop
        // through the identical TwapPlan shape, so this exercises the same
        // call site with a size-derived per_slice.
        let per_slice = dec!(99.5); // 99.5 * 50.201 = 4994.9995
        let plan = plan_with_cap(Side::Long, per_slice, dec!(5000));
        let api = ScriptedApi::new()
            .with_default_book(book_at(dec!(49.9), dec!(50.1)))
            .push_place(filled(per_slice, dec!(50.201)));

        let report = run_twap(&api, &plan).await;

        assert_eq!(api.place_count(), 1, "order must be placed");
        assert!(report.abort_reason.is_none(), "{:?}", report.abort_reason);
    }

    #[tokio::test(start_paused = true)]
    async fn notional_cap_long_size_just_over_cap_is_rejected() {
        let per_slice = dec!(100); // 100 * 50.201 = 5020.1
        let plan = plan_with_cap(Side::Long, per_slice, dec!(5000));
        let api = ScriptedApi::new().with_default_book(book_at(dec!(49.9), dec!(50.1)));

        let report = run_twap(&api, &plan).await;

        assert_eq!(api.place_count(), 0, "must reject before /exchange");
        let reason = report.abort_reason.expect("must abort");
        assert!(reason.contains("notional"), "{reason}");
    }

    #[tokio::test(start_paused = true)]
    async fn notional_cap_short_usd_just_under_cap_is_accepted() {
        // short px = 49.8; per_slice sized so notional is just under $10000.
        let per_slice = dec!(200); // 200 * 49.8 = 9960
        let plan = plan_with_cap(Side::Short, per_slice, dec!(10000));
        let api = ScriptedApi::new()
            .with_default_book(book_at(dec!(49.9), dec!(50.1)))
            .push_place(filled(per_slice, dec!(49.8)));

        let report = run_twap(&api, &plan).await;

        assert_eq!(api.place_count(), 1, "order must be placed");
        assert!(report.abort_reason.is_none(), "{:?}", report.abort_reason);
    }

    #[tokio::test(start_paused = true)]
    async fn notional_cap_short_usd_just_over_cap_is_rejected() {
        let per_slice = dec!(201); // 201 * 49.8 = 10009.8
        let plan = plan_with_cap(Side::Short, per_slice, dec!(10000));
        let api = ScriptedApi::new().with_default_book(book_at(dec!(49.9), dec!(50.1)));

        let report = run_twap(&api, &plan).await;

        assert_eq!(api.place_count(), 0, "must reject before /exchange");
        let reason = report.abort_reason.expect("must abort");
        assert!(reason.contains("notional"), "{reason}");
    }

    #[tokio::test(start_paused = true)]
    async fn notional_cap_short_size_just_under_cap_is_accepted() {
        let per_slice = dec!(100); // 100 * 49.8 = 4980
        let plan = plan_with_cap(Side::Short, per_slice, dec!(5000));
        let api = ScriptedApi::new()
            .with_default_book(book_at(dec!(49.9), dec!(50.1)))
            .push_place(filled(per_slice, dec!(49.8)));

        let report = run_twap(&api, &plan).await;

        assert_eq!(api.place_count(), 1, "order must be placed");
        assert!(report.abort_reason.is_none(), "{:?}", report.abort_reason);
    }

    #[tokio::test(start_paused = true)]
    async fn notional_cap_short_size_just_over_cap_is_rejected() {
        let per_slice = dec!(101); // 101 * 49.8 = 5029.8
        let plan = plan_with_cap(Side::Short, per_slice, dec!(5000));
        let api = ScriptedApi::new().with_default_book(book_at(dec!(49.9), dec!(50.1)));

        let report = run_twap(&api, &plan).await;

        assert_eq!(api.place_count(), 0, "must reject before /exchange");
        let reason = report.abort_reason.expect("must abort");
        assert!(reason.contains("notional"), "{reason}");
    }

    // === Issue #3: non-positive limit price rejected unconditionally ===
    //
    // slippage_bps at/above SLIPPAGE_HARD_CAP_BPS would already be rejected
    // at CLI pre-flight (src/main.rs / src/risk.rs), so this test drives the
    // slice-loop's OWN belt-and-braces check directly by constructing a plan
    // whose slippage, while under the CLI's hard cap, is high enough at this
    // specific bid to floor the short limit price to zero or below — proving
    // the loop does not blindly trust a slippage value that passed CLI
    // validation against a DIFFERENT (trigger-time) book.
    #[tokio::test(start_paused = true)]
    async fn non_positive_short_limit_price_hard_stops_before_any_exchange_call() {
        let mut plan = plan_with_cap(Side::Short, dec!(5), dec!(1_000_000));
        // bid=0.01: short px = 0.01 * (1 - bps/1e4). At slippage=9999bps
        // (just under the 10000 hard cap) this floors to <= 0.
        plan.slippage_bps = dec!(9999);
        let api = ScriptedApi::new().with_default_book(book_at(dec!(0.01), dec!(0.02)));

        let report = run_twap(&api, &plan).await;

        assert_eq!(
            api.place_count(),
            0,
            "non-positive limit price must reject before any /exchange call"
        );
        let reason = report.abort_reason.clone().expect("must abort");
        assert!(reason.contains("risk envelope"), "{reason}");
        assert_eq!(report.exit_code(), 1);
    }

    // === Issue #4: ExecutionJournal crash-injection + shutdown tests ===
    //
    // These drive `run_twap_journaled` directly (rather than the plain
    // `run_twap` wrapper used by every test above) with a real
    // `ExecutionJournal` backed by a throwaway temp directory, so each test
    // can replay the on-disk journal afterwards and assert exactly what a
    // restart would see. "Crash" is simulated by simply not continuing the
    // run — nothing here kills a real process; `ScriptedApi`'s queues are
    // used to stop exactly at the point under test (e.g. an empty places
    // queue makes the NEXT place attempt panic, so tests script exactly one
    // place and then inspect the journal as if the process had died right
    // after it).
    mod journal_tests {
        use super::*;
        use crate::journal::{summarize, CloidState, ExecutionJournal, JournalRecord, RunHeader};

        /// Hand-rolled temp dir (no `tempfile` dependency — mirrors the one
        /// in `src/journal.rs`'s own test module).
        struct TempDir(std::path::PathBuf);
        impl TempDir {
            fn new() -> Self {
                let dir = std::env::temp_dir().join(format!(
                    "hype-twap-twaprs-journal-test-{}",
                    uuid::Uuid::now_v7()
                ));
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

        fn test_header() -> RunHeader {
            RunHeader {
                run_id: "test-run".into(),
                network: "testnet".into(),
                agent: Some(Address::new(AGENT)),
                master: Some(Address::new(MASTER)),
                symbol: Symbol::new("HYPE"),
                side: Side::Long,
                slices: 10,
                plan_hash: "test-hash".into(),
                started_at_unix_ms: 0,
            }
        }

        // --- Crash injection 1: after Prepared fsynced, before POST sent ---
        //
        // We cannot literally stop `place_order_once` from being called once
        // `run_twap_journaled` reaches it (there is no hook between "Prepared
        // recorded" and "POST sent" other than the POST call itself), so this
        // test proves the INVARIANT the real crash window depends on: the
        // Prepared record for slice 1's cloid is durably on disk (fsynced)
        // strictly before ScriptedApi ever observes a Place call. If the
        // process had died at that exact instant, replaying the journal shows
        // a `PreparedOnly` cloid — the resume/incomplete-run-detection layer
        // (`find_incomplete_run`/`summarize`) sees this as "needs
        // reconciliation before any new send," never as "safe to assume
        // nothing happened."
        #[tokio::test(start_paused = true)]
        async fn crash_after_prepared_before_post_leaves_a_preparedonly_record_never_a_phantom_terminal(
        ) {
            let tmp = TempDir::new();
            let mut journal =
                ExecutionJournal::start(tmp.path(), "run-1".into(), test_header()).unwrap();

            // A single-slice plan isolates the one place under test — the
            // run reaches its target after slice 1 and stops there on its
            // own (`stats.filled >= plan.total_adjusted`), with no shutdown
            // signal needed to keep this test focused on the crash window.
            let mut single_slice_plan = plan(false);
            single_slice_plan.slices = 1;
            single_slice_plan.per_slice = dec!(5);
            single_slice_plan.total_adjusted = dec!(5);

            let api = ScriptedApi::new()
                .with_default_book(book_at(dec!(49.9), dec!(50.1)))
                .push_place(filled(dec!(5), dec!(50)));

            let _report =
                run_twap_journaled(&api, &single_slice_plan, Some(&mut journal), None).await;

            let records = ExecutionJournal::read_all(tmp.path(), "run-1").unwrap();
            // Slice 1's Prepared record exists and precedes any Terminal
            // record for the same cloid — proving durability-before-send.
            let prepared_idx = records
                .iter()
                .position(|r| matches!(r, JournalRecord::Prepared { slice_idx: 1, .. }))
                .expect("Prepared record for slice 1 must exist");
            let terminal_idx = records
                .iter()
                .position(|r| matches!(r, JournalRecord::Terminal { slice_idx: 1, .. }))
                .expect("Terminal record for slice 1 must exist (this run completed the send)");
            assert!(
                prepared_idx < terminal_idx,
                "Prepared must be written before Terminal: {records:?}"
            );

            // No double-place: a single call reached ScriptedApi.
            assert_eq!(api.place_count(), 1);
        }

        // --- Crash injection 2: after POST sent, before response read ---
        //
        // ScriptedApi's place queue is EMPTY, so `place_order_once` returns
        // `HlError::InvalidResponse("...queue exhausted")` immediately — a
        // stand-in for "the transport call itself failed/was interrupted,"
        // i.e. exactly the ambiguous case `place_slice_reconciled` treats as
        // a `Network` error requiring reconciliation. Because
        // `InvalidResponse` (not `Network`) is what an exhausted queue
        // produces, we instead script a genuine `HlError::Network` failure to
        // hit the real ambiguous-send branch and its `SubmittedUnknown`
        // journal write.
        #[tokio::test(start_paused = true)]
        async fn crash_after_post_sent_before_response_read_journals_submitted_unknown_and_resume_reconciles_without_a_resend(
        ) {
            let tmp = TempDir::new();
            let mut journal =
                ExecutionJournal::start(tmp.path(), "run-2".into(), test_header()).unwrap();

            let mut single_slice_plan = plan(false);
            single_slice_plan.slices = 1;
            single_slice_plan.per_slice = dec!(5);
            single_slice_plan.total_adjusted = dec!(5);

            // The send transport-fails (ambiguous outcome); reconciliation
            // then discovers HL actually HAD the order, terminal/filled — the
            // realistic "response was lost, but the exchange got it" crash
            // shape. No resend may occur.
            let api = ScriptedApi::new()
                .with_default_book(book_at(dec!(49.9), dec!(50.1)))
                .push_place(Err(HlError::Network("connection reset".into())))
                .push_status(Ok(Some(status(dec!(5), Some(dec!(50)), "filled"))));

            let report =
                run_twap_journaled(&api, &single_slice_plan, Some(&mut journal), None).await;

            // Exactly one place call — the ambiguous send is never resent
            // once reconciliation finds HL already had it.
            assert_eq!(api.place_count(), 1, "must never resend once reconciled");
            assert_eq!(report.filled, dec!(5));

            let records = ExecutionJournal::read_all(tmp.path(), "run-2").unwrap();
            assert!(
                records
                    .iter()
                    .any(|r| matches!(r, JournalRecord::SubmittedUnknown { slice_idx: 1, .. })),
                "the ambiguous send must be journaled as SubmittedUnknown: {records:?}"
            );
            let summary = summarize(&records);
            // Resume-safety: the cloid's LATEST state is Terminal (filled),
            // not stuck at SubmittedUnknown — a resume replay would see this
            // as already resolved and would not attempt to reconcile or
            // resend it again.
            assert_eq!(summary.cloids.len(), 1);
            assert!(matches!(
                &summary.cloids[0].1,
                CloidState::Terminal { filled_sz, .. } if filled_sz == "5"
            ));
        }

        // --- Crash injection 3: after resting order confirmed ---
        //
        // The IOC unexpectedly rests (`PlaceOutcome::Resting`); the run
        // journals `Acknowledged` for that cloid, then (per the existing
        // `recover_resting_fill` policy) cancels and polls `orderStatus`
        // until a terminal fill is known. This proves the Acknowledged state
        // is durably recorded and superseded by the eventual Terminal record
        // — a crash between those two points would leave a resumable
        // Acknowledged (cancel-on-shutdown candidate), never silently
        // dropped.
        #[tokio::test(start_paused = true)]
        async fn crash_after_resting_order_confirmed_journals_acknowledged_then_terminal() {
            let tmp = TempDir::new();
            let mut journal =
                ExecutionJournal::start(tmp.path(), "run-3".into(), test_header()).unwrap();

            let api = ScriptedApi::new()
                .with_default_book(book_at(dec!(49.9), dec!(50.1)))
                .push_place(Ok(PlaceOutcome::Resting { oid: OrderId(99) }))
                .push_cancel(Ok(()))
                .push_status(Ok(Some(status(dec!(5), Some(dec!(50)), "filled"))));

            let report = run_twap_journaled(&api, &plan(false), Some(&mut journal), None).await;

            assert_eq!(report.filled, dec!(5));

            let records = ExecutionJournal::read_all(tmp.path(), "run-3").unwrap();
            let ack_idx = records
                .iter()
                .position(|r| matches!(r, JournalRecord::Acknowledged { slice_idx: 1, .. }))
                .expect("Acknowledged record must exist for the resting order");
            let terminal_idx = records
                .iter()
                .position(|r| matches!(r, JournalRecord::Terminal { slice_idx: 1, .. }))
                .expect("Terminal record must exist once the resting order resolves");
            assert!(ack_idx < terminal_idx);
        }

        // --- Incomplete-run blocks new run ---

        #[test]
        fn incomplete_run_present_blocks_a_new_overlapping_live_run() {
            let tmp = TempDir::new();
            let mut journal =
                ExecutionJournal::start(tmp.path(), "run-incomplete".into(), test_header())
                    .unwrap();
            journal
                .record(&JournalRecord::Prepared {
                    slice_idx: 1,
                    cloid: Cloid::new(),
                    nonce: None,
                    symbol: Symbol::new("HYPE"),
                    side: Side::Long,
                    px: "50".into(),
                    sz: "5".into(),
                })
                .unwrap();
            journal
                .record(&JournalRecord::SubmittedUnknown {
                    slice_idx: 1,
                    cloid: Cloid::new(),
                })
                .unwrap();
            drop(journal); // simulate the crash: no FinalReport was ever written

            let found = crate::journal::find_incomplete_run(
                tmp.path(),
                "testnet",
                Some(&Address::new(AGENT)),
            )
            .unwrap();
            assert_eq!(
                found,
                Some("run-incomplete".to_string()),
                "an incomplete run for the same network+agent must be detected"
            );
        }

        // --- SIGINT/SIGTERM (simulated via ShutdownSignal) ---

        /// Pre-send: shutdown is ALREADY triggered before slice 1 is ever
        /// attempted (equivalent to a signal arriving in the gap between
        /// trigger-fire and the first slice iteration). No place must occur.
        #[tokio::test(start_paused = true)]
        async fn shutdown_pre_send_places_nothing_and_journals_no_prepared_record() {
            let tmp = TempDir::new();
            let mut journal =
                ExecutionJournal::start(tmp.path(), "run-presend".into(), test_header()).unwrap();
            let api = ScriptedApi::new().with_default_book(book_at(dec!(49.9), dec!(50.1)));

            let (tx, rx) = tokio::sync::watch::channel(true); // ALREADY triggered
            let _tx = tx; // keep sender alive for the duration of the run
            let shutdown = ShutdownSignal::new(rx);

            let report =
                run_twap_journaled(&api, &plan(false), Some(&mut journal), Some(shutdown)).await;

            assert_eq!(
                api.place_count(),
                0,
                "no slice may be placed after shutdown"
            );
            assert!(report.abort_reason.unwrap().contains("shutdown"));

            let records = ExecutionJournal::read_all(tmp.path(), "run-presend").unwrap();
            assert!(
                !records
                    .iter()
                    .any(|r| matches!(r, JournalRecord::Prepared { .. })),
                "no Prepared record should exist when shutdown fires before any slice: {records:?}"
            );
            let summary = summarize(&records);
            assert!(summary.final_report_seen);
            assert!(summary.cloids.is_empty());
        }

        /// Post-send (ambiguous): shutdown is signalled AFTER slice 1 has
        /// already been placed and journaled (during its inter-slice wait).
        /// Slice 1 must complete its own place/reconcile cycle (never
        /// abandoned mid-flight); slice 2 must never be attempted.
        #[tokio::test(start_paused = true)]
        async fn shutdown_post_send_finishes_the_inflight_slice_then_stops_before_the_next() {
            let tmp = TempDir::new();
            let mut journal =
                ExecutionJournal::start(tmp.path(), "run-postsend".into(), test_header()).unwrap();

            let api = ScriptedApi::new()
                .with_default_book(book_at(dec!(49.9), dec!(50.1)))
                .push_place(filled(dec!(5), dec!(50)));

            let (tx, rx) = tokio::sync::watch::channel(false);
            let shutdown = ShutdownSignal::new(rx);

            let plan_val = plan(false);
            let run_fut = run_twap_journaled(&api, &plan_val, Some(&mut journal), Some(shutdown));
            tokio::pin!(run_fut);

            // Drive the run just far enough to complete slice 1's place (a
            // handful of yields is enough under `start_paused`: ScriptedApi's
            // Filled path awaits no timer, so slice 1 resolves on the first
            // few polls), THEN signal shutdown — modelling a signal arriving
            // while the run is sitting in its inter-slice sleep, after slice
            // 1 is already journaled Terminal.
            for _ in 0..8 {
                tokio::select! {
                    biased;
                    _ = &mut run_fut => panic!("run must not finish before shutdown is signalled"),
                    _ = tokio::task::yield_now() => {}
                }
            }
            assert_eq!(
                api.place_count(),
                1,
                "slice 1 must already be placed before shutdown is signalled"
            );
            tx.send(true).unwrap();

            let report = run_fut.await;

            assert_eq!(
                api.place_count(),
                1,
                "slice 1 (already in flight when shutdown fired) must complete; slice 2 must not start"
            );
            assert_eq!(report.filled, dec!(5));
            assert!(report.abort_reason.unwrap().contains("shutdown"));

            let records = ExecutionJournal::read_all(tmp.path(), "run-postsend").unwrap();
            let summary = summarize(&records);
            assert_eq!(
                summary.cloids.len(),
                1,
                "only slice 1's cloid was ever touched"
            );
            assert!(matches!(
                &summary.cloids[0].1,
                CloidState::Terminal { filled_sz, .. } if filled_sz == "5"
            ));
        }

        /// Mid-reconcile: shutdown fires while an ambiguous send for slice 1
        /// is being reconciled. The reconciliation must still run to
        /// completion (per the PM decision: "reconciliation that is already
        /// in flight ... still runs to completion") rather than being
        /// abandoned, so the journal ends with a resolved Terminal state, not
        /// a dangling SubmittedUnknown.
        #[tokio::test(start_paused = true)]
        async fn shutdown_mid_reconcile_lets_the_inflight_reconciliation_finish() {
            let tmp = TempDir::new();
            let mut journal =
                ExecutionJournal::start(tmp.path(), "run-midreconcile".into(), test_header())
                    .unwrap();

            let api = ScriptedApi::new()
                .with_default_book(book_at(dec!(49.9), dec!(50.1)))
                .push_place(Err(HlError::Network("connection reset".into())))
                .push_status(Ok(Some(status(dec!(5), Some(dec!(50)), "filled"))));

            let (tx, rx) = tokio::sync::watch::channel(false);
            let shutdown = ShutdownSignal::new(rx);

            let plan_val = plan(false);
            let run_fut = run_twap_journaled(&api, &plan_val, Some(&mut journal), Some(shutdown));
            tokio::pin!(run_fut);

            // Drive the run past the initial ambiguous send (SubmittedUnknown
            // is journaled) and into the reconciliation delay/poll — i.e.
            // virtual time must advance past `RECONCILE_DELAY` — BEFORE
            // signalling shutdown. `place_slice_reconciled` has no
            // shutdown-awareness of its own (PM decision), so a signal
            // arriving here must not truncate the reconciliation already in
            // flight.
            tokio::time::advance(RECONCILE_DELAY + Duration::from_millis(50)).await;
            for _ in 0..8 {
                tokio::select! {
                    biased;
                    _ = &mut run_fut => panic!("run must not finish before shutdown is signalled"),
                    _ = tokio::task::yield_now() => {}
                }
            }
            let mid_flight_records =
                ExecutionJournal::read_all(tmp.path(), "run-midreconcile").unwrap();
            assert!(
                mid_flight_records
                    .iter()
                    .any(|r| matches!(r, JournalRecord::SubmittedUnknown { .. })),
                "reconciliation must already be in flight (SubmittedUnknown journaled) \
                 before shutdown fires: {mid_flight_records:?}"
            );
            tx.send(true).unwrap();

            let report = run_fut.await;

            assert_eq!(
                report.filled,
                dec!(5),
                "the in-flight reconciliation must finish"
            );
            let records = ExecutionJournal::read_all(tmp.path(), "run-midreconcile").unwrap();
            let summary = summarize(&records);
            assert!(
                summary.unresolved_cloids().is_empty(),
                "no cloid should be left unresolved: {records:?}"
            );
        }

        // --- Resume counts fills exactly once ---

        #[tokio::test(start_paused = true)]
        async fn resume_after_simulated_crash_counts_each_fill_exactly_once() {
            let tmp = TempDir::new();

            // "First process": completes slice 1 (filled 5), then "crashes"
            // (we simply stop driving it — the journal already has slice 1's
            // Terminal record on disk).
            let filled_sz_slice1 = {
                let mut journal =
                    ExecutionJournal::start(tmp.path(), "run-resume".into(), test_header())
                        .unwrap();
                let api = ScriptedApi::new()
                    .with_default_book(book_at(dec!(49.9), dec!(50.1)))
                    .push_place(filled(dec!(5), dec!(50)));

                // A single-slice plan models "the process completed exactly
                // one slice, then crashed" (simulated by simply not driving
                // the run any further — there is no second slice to run), so
                // only ONE Terminal record exists in the journal at "crash"
                // time.
                let mut one_slice_plan = plan(false);
                one_slice_plan.slices = 1;
                one_slice_plan.per_slice = dec!(5);
                one_slice_plan.total_adjusted = dec!(5);

                let report =
                    run_twap_journaled(&api, &one_slice_plan, Some(&mut journal), None).await;
                report.filled
            };
            assert_eq!(filled_sz_slice1, dec!(5));

            // "Resume": replay the journal to get the fills already credited
            // (this is what a real `--resume` accounting step does BEFORE
            // continuing the run), then continue the run for the remaining
            // slices with a FRESH ScriptedApi/journal-append session. The
            // total must be the sum of the resumed fill plus the newly
            // executed ones, with slice 1's fill counted exactly once (not
            // replayed again as a "new" fill in this second session).
            let resumed_records = ExecutionJournal::read_all(tmp.path(), "run-resume").unwrap();
            let resumed_summary = summarize(&resumed_records);
            let already_filled = resumed_summary.total_filled();
            assert_eq!(already_filled, dec!(5));

            // Continue: a plan sized for the REMAINING 45 (9 slices × 5) —
            // this models how `main.rs`/the resume path would shrink the
            // plan's remaining-target by `already_filled` before resuming
            // the loop, which is what guarantees slice 1's fill is never
            // re-requested.
            let mut resumed_plan = plan(false);
            resumed_plan.slices = 9;
            resumed_plan.total_adjusted = dec!(45);
            resumed_plan.per_slice = dec!(5);

            let mut journal2 = ExecutionJournal::open_existing(tmp.path(), "run-resume").unwrap();
            let api2 = ScriptedApi::new().with_default_book(book_at(dec!(49.9), dec!(50.1)));
            let mut api2 = api2;
            for _ in 0..9 {
                api2 = api2.push_place(filled(dec!(5), dec!(50)));
            }

            let report2 = run_twap_journaled(&api2, &resumed_plan, Some(&mut journal2), None).await;
            assert_eq!(report2.filled, dec!(45));

            let grand_total = already_filled + report2.filled;
            assert_eq!(
                grand_total,
                dec!(50),
                "total must equal the full target with slice 1's fill counted exactly once, \
                 not double-counted across the crash+resume boundary"
            );

            // Final on-disk check: exactly 10 distinct cloids ever appear
            // across the whole journal (1 from the first session + 9 from
            // the resumed session), each Terminal exactly once.
            let all_records = ExecutionJournal::read_all(tmp.path(), "run-resume").unwrap();
            let final_summary = summarize(&all_records);
            assert_eq!(final_summary.cloids.len(), 10);
            assert_eq!(final_summary.total_filled(), dec!(50));
        }
    }
}
