//! The seam between the TWAP loop and Hyperliquid (T6).
//!
//! `run_twap` commits real money, so it must be testable without a network.
//! Every HL call the loop makes goes through the `HlApi` trait; `HlClient`
//! implements it for production and `ScriptedApi` (test-only) implements it by
//! replaying a canned response list while recording the exact call sequence.
//!
//! The trait is deliberately narrow — only the operations execution needs —
//! so a fake stays cheap to write and impossible to under-specify.

use async_trait::async_trait;
use rust_decimal::Decimal;

use crate::client::{HlClient, OrderStatusFill, PlaceOutcome, UserFill};
use crate::errors::HlError;
use crate::types::{
    Address, CancelIntent, Cloid, OrderBook, OrderId, OrderIntent, SignedPerpPosition, Symbol,
};

/// Every Hyperliquid operation the slice loop performs.
#[async_trait]
pub trait HlApi: Send + Sync {
    /// `/info l2Book` — one top-of-book snapshot. Idempotent, retried inside.
    async fn fetch_l2_book(&self, symbol: &Symbol) -> Result<OrderBook, HlError>;

    /// `/info clearinghouseState` — the current signed perp position held by
    /// the master account. A valid state that omits `symbol` yields `szi = 0`.
    ///
    /// The default keeps existing narrow test wrappers source-compatible. It
    /// is fail-closed, so a wrapper must opt in explicitly before a new
    /// position-aware flow can use it.
    async fn fetch_perp_position(
        &self,
        _user: &Address,
        _symbol: &Symbol,
    ) -> Result<SignedPerpPosition, HlError> {
        Err(HlError::InvalidResponse(
            "HlApi implementation does not support clearinghouseState".into(),
        ))
    }

    /// `/exchange order` — sent EXACTLY ONCE (W1).
    ///
    /// An `Err(HlError::Network(_))` means the outcome is UNKNOWN, not that the
    /// order was not placed. The returned `u64` is the nonce that was signed,
    /// which lets a caller (and a test) prove a resend used fresh material.
    ///
    /// `expires_after_ms` (Issue #2) is the run-level `ExecutionDeadline`'s
    /// wall-clock Unix ms expiry — the SAME value must reach both the signed
    /// action hash and the `/exchange` body's `expiresAfter` field. Every
    /// caller of this trait method already re-checked the deadline before
    /// calling, per `src/twap.rs`'s `ExecutionDeadline::check`.
    async fn place_order_once(
        &self,
        intent: &OrderIntent,
        asset: u32,
        expires_after_ms: u64,
    ) -> Result<(u64, PlaceOutcome), HlError>;

    /// `/exchange cancelByCloid` — sent exactly once; failure is non-fatal.
    async fn cancel_by_cloid(&self, intent: &CancelIntent, asset: u32) -> Result<(), HlError>;

    /// `/info orderStatus` keyed on the exchange oid. `user` must be MASTER (F1).
    async fn fetch_order_status(
        &self,
        user: &Address,
        oid: OrderId,
    ) -> Result<Option<OrderStatusFill>, HlError>;

    /// `/info orderStatus` keyed on the cloid — the W1 reconciliation key.
    async fn fetch_order_status_by_cloid(
        &self,
        user: &Address,
        cloid: Cloid,
    ) -> Result<Option<OrderStatusFill>, HlError>;

    /// Official non-aggregated `userFillsByTime` ledger.
    async fn fetch_user_fills_by_time(
        &self,
        _user: &Address,
        _start_time_ms: u64,
        _end_time_ms: Option<u64>,
    ) -> Result<Vec<UserFill>, HlError> {
        Err(HlError::InvalidResponse(
            "HlApi implementation does not support userFillsByTime".into(),
        ))
    }
}

#[async_trait]
impl HlApi for HlClient {
    async fn fetch_l2_book(&self, symbol: &Symbol) -> Result<OrderBook, HlError> {
        HlClient::fetch_l2_book(self, symbol).await
    }

    async fn fetch_perp_position(
        &self,
        user: &Address,
        symbol: &Symbol,
    ) -> Result<SignedPerpPosition, HlError> {
        HlClient::fetch_perp_position(self, user, symbol).await
    }

    async fn place_order_once(
        &self,
        intent: &OrderIntent,
        asset: u32,
        expires_after_ms: u64,
    ) -> Result<(u64, PlaceOutcome), HlError> {
        HlClient::place_order_once(self, intent, asset, expires_after_ms).await
    }

    async fn cancel_by_cloid(&self, intent: &CancelIntent, asset: u32) -> Result<(), HlError> {
        HlClient::cancel_by_cloid(self, intent, asset).await
    }

    async fn fetch_order_status(
        &self,
        user: &Address,
        oid: OrderId,
    ) -> Result<Option<OrderStatusFill>, HlError> {
        HlClient::fetch_order_status(self, user, oid).await
    }

    async fn fetch_order_status_by_cloid(
        &self,
        user: &Address,
        cloid: Cloid,
    ) -> Result<Option<OrderStatusFill>, HlError> {
        HlClient::fetch_order_status_by_cloid(self, user, cloid).await
    }

    async fn fetch_user_fills_by_time(
        &self,
        user: &Address,
        start_time_ms: u64,
        end_time_ms: Option<u64>,
    ) -> Result<Vec<UserFill>, HlError> {
        HlClient::fetch_user_fills_by_time(self, user, start_time_ms, end_time_ms).await
    }
}

// === test double ===

/// One recorded interaction with the fake, in call order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    Book {
        symbol: String,
    },
    Position {
        user: String,
        symbol: String,
    },
    Place {
        sz: Decimal,
        px: Decimal,
        reduce_only: bool,
        cloid: Cloid,
        nonce: u64,
        /// The `expiresAfter` value this place was signed and sent with
        /// (Issue #2) — recorded so a test can assert every place used the
        /// SAME run-level expiry, including resends.
        expires_after_ms: u64,
    },
    Cancel {
        cloid: Cloid,
    },
    StatusByOid {
        user: String,
        oid: OrderId,
    },
    StatusByCloid {
        user: String,
        cloid: Cloid,
    },
    UserFillsByTime {
        user: String,
        start_time_ms: u64,
        end_time_ms: Option<u64>,
    },
}

impl Call {
    /// True for the calls that put money at risk. Tests assert on these to pin
    /// "no order was sent outside the window" style invariants.
    pub fn is_place(&self) -> bool {
        matches!(self, Call::Place { .. })
    }
}

/// A scripted `HlApi` for loop-level tests (T6).
///
/// Each queue is drained in order; running a queue dry is a panic rather than a
/// silent default, because a test that consumes more responses than it scripted
/// is asserting against behaviour it never described.
pub struct ScriptedApi {
    books: std::sync::Mutex<std::collections::VecDeque<Result<OrderBook, HlError>>>,
    positions: std::sync::Mutex<std::collections::VecDeque<Result<SignedPerpPosition, HlError>>>,
    places: std::sync::Mutex<std::collections::VecDeque<Result<PlaceOutcome, HlError>>>,
    cancels: std::sync::Mutex<std::collections::VecDeque<Result<(), HlError>>>,
    statuses:
        std::sync::Mutex<std::collections::VecDeque<Result<Option<OrderStatusFill>, HlError>>>,
    fills: std::sync::Mutex<std::collections::VecDeque<Result<Vec<UserFill>, HlError>>>,
    calls: std::sync::Mutex<Vec<Call>>,
    nonce: std::sync::atomic::AtomicU64,
    /// Reused when the book queue is exhausted, so a test only has to script
    /// the snapshots it actually cares about.
    default_book: std::sync::Mutex<Option<OrderBook>>,
}

impl Default for ScriptedApi {
    fn default() -> Self {
        Self::new()
    }
}

impl ScriptedApi {
    pub fn new() -> Self {
        Self {
            books: std::sync::Mutex::new(std::collections::VecDeque::new()),
            positions: std::sync::Mutex::new(std::collections::VecDeque::new()),
            places: std::sync::Mutex::new(std::collections::VecDeque::new()),
            cancels: std::sync::Mutex::new(std::collections::VecDeque::new()),
            statuses: std::sync::Mutex::new(std::collections::VecDeque::new()),
            fills: std::sync::Mutex::new(std::collections::VecDeque::new()),
            calls: std::sync::Mutex::new(Vec::new()),
            nonce: std::sync::atomic::AtomicU64::new(1),
            default_book: std::sync::Mutex::new(None),
        }
    }

    /// Book returned once every scripted book is consumed.
    pub fn with_default_book(self, book: OrderBook) -> Self {
        *lock(&self.default_book) = Some(book);
        self
    }

    pub fn push_book(self, book: Result<OrderBook, HlError>) -> Self {
        lock(&self.books).push_back(book);
        self
    }

    /// Queue a clearinghouse position response. Unlike books there is no
    /// default: an unscripted account-state read must fail closed in a test.
    pub fn push_position(self, position: Result<SignedPerpPosition, HlError>) -> Self {
        lock(&self.positions).push_back(position);
        self
    }

    pub fn push_place(self, outcome: Result<PlaceOutcome, HlError>) -> Self {
        lock(&self.places).push_back(outcome);
        self
    }

    pub fn push_cancel(self, r: Result<(), HlError>) -> Self {
        lock(&self.cancels).push_back(r);
        self
    }

    pub fn push_status(self, s: Result<Option<OrderStatusFill>, HlError>) -> Self {
        lock(&self.statuses).push_back(s);
        self
    }
    pub fn push_fills(self, fills: Result<Vec<UserFill>, HlError>) -> Self {
        lock(&self.fills).push_back(fills);
        self
    }

    /// The full call log, in order.
    pub fn calls(&self) -> Vec<Call> {
        lock(&self.calls).clone()
    }

    pub fn place_calls(&self) -> Vec<Call> {
        lock(&self.calls)
            .iter()
            .filter(|c| c.is_place())
            .cloned()
            .collect()
    }

    pub fn place_count(&self) -> usize {
        lock(&self.calls).iter().filter(|c| c.is_place()).count()
    }

    fn record(&self, c: Call) {
        lock(&self.calls).push(c);
    }
}

/// Lock helper: a poisoned mutex in a test double means a prior assertion
/// already panicked, so recovering the inner value keeps the ORIGINAL failure
/// as the reported one instead of masking it with a poison error.
fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[async_trait]
impl HlApi for ScriptedApi {
    async fn fetch_l2_book(&self, symbol: &Symbol) -> Result<OrderBook, HlError> {
        self.record(Call::Book {
            symbol: symbol.as_str().to_string(),
        });
        if let Some(next) = lock(&self.books).pop_front() {
            return next;
        }
        match lock(&self.default_book).clone() {
            Some(b) => Ok(b),
            None => Err(HlError::InvalidResponse(
                "ScriptedApi: book queue exhausted and no default_book set".into(),
            )),
        }
    }

    async fn fetch_perp_position(
        &self,
        user: &Address,
        symbol: &Symbol,
    ) -> Result<SignedPerpPosition, HlError> {
        self.record(Call::Position {
            user: user.as_str().to_string(),
            symbol: symbol.as_str().to_string(),
        });
        match lock(&self.positions).pop_front() {
            Some(r) => r,
            None => Err(HlError::InvalidResponse(
                "ScriptedApi: position queue exhausted".into(),
            )),
        }
    }

    async fn place_order_once(
        &self,
        intent: &OrderIntent,
        _asset: u32,
        expires_after_ms: u64,
    ) -> Result<(u64, PlaceOutcome), HlError> {
        let nonce = self.nonce.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        self.record(Call::Place {
            sz: intent.sz,
            px: intent.px,
            reduce_only: intent.reduce_only,
            cloid: intent.cloid,
            nonce,
            expires_after_ms,
        });
        match lock(&self.places).pop_front() {
            Some(r) => r.map(|o| (nonce, o)),
            None => Err(HlError::InvalidResponse(
                "ScriptedApi: place queue exhausted".into(),
            )),
        }
    }

    async fn cancel_by_cloid(&self, intent: &CancelIntent, _asset: u32) -> Result<(), HlError> {
        self.record(Call::Cancel {
            cloid: intent.by_cloid,
        });
        lock(&self.cancels).pop_front().unwrap_or(Ok(()))
    }

    async fn fetch_order_status(
        &self,
        user: &Address,
        oid: OrderId,
    ) -> Result<Option<OrderStatusFill>, HlError> {
        self.record(Call::StatusByOid {
            user: user.as_str().to_string(),
            oid,
        });
        match lock(&self.statuses).pop_front() {
            Some(r) => r,
            None => Err(HlError::InvalidResponse(
                "ScriptedApi: status queue exhausted".into(),
            )),
        }
    }

    async fn fetch_order_status_by_cloid(
        &self,
        user: &Address,
        cloid: Cloid,
    ) -> Result<Option<OrderStatusFill>, HlError> {
        self.record(Call::StatusByCloid {
            user: user.as_str().to_string(),
            cloid,
        });
        match lock(&self.statuses).pop_front() {
            Some(r) => r,
            None => Err(HlError::InvalidResponse(
                "ScriptedApi: status queue exhausted".into(),
            )),
        }
    }

    async fn fetch_user_fills_by_time(
        &self,
        user: &Address,
        start_time_ms: u64,
        end_time_ms: Option<u64>,
    ) -> Result<Vec<UserFill>, HlError> {
        self.record(Call::UserFillsByTime {
            user: user.as_str().to_string(),
            start_time_ms,
            end_time_ms,
        });
        lock(&self.fills).pop_front().unwrap_or_else(|| {
            Err(HlError::InvalidResponse(
                "ScriptedApi: fills queue exhausted".into(),
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use rust_decimal_macros::dec;

    #[tokio::test]
    async fn scripted_position_returns_queued_value_and_records_master_and_symbol() {
        let master = Address::new("0x00000000000000000000000000000000000000aa");
        let symbol = Symbol::new("HYPE");
        let api = ScriptedApi::new().push_position(Ok(SignedPerpPosition {
            symbol: symbol.clone(),
            szi: dec!(-1.25),
        }));

        let position = api.fetch_perp_position(&master, &symbol).await.unwrap();
        assert_eq!(position.szi, dec!(-1.25));
        assert_eq!(
            api.calls(),
            vec![Call::Position {
                user: master.as_str().to_string(),
                symbol: "HYPE".into(),
            }]
        );
    }

    #[tokio::test]
    async fn scripted_unscripted_position_fails_closed_after_recording_call() {
        let api = ScriptedApi::new();
        let err = api
            .fetch_perp_position(&Address::new("master"), &Symbol::new("HYPE"))
            .await
            .unwrap_err();
        assert!(
            matches!(err, HlError::InvalidResponse(message) if message.contains("queue exhausted"))
        );
        assert!(matches!(api.calls().as_slice(), [Call::Position { .. }]));
    }
}
