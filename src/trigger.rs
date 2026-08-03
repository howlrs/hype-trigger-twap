//! Trigger wait loop (§7).
//!
//! Two independent conditions, whichever fires first (OR):
//! - price: `mid >= trigger` (above) or `mid <= trigger` (below)
//! - time:  `--start-after` elapsed
//!
//! With neither configured, the trigger fires immediately.
//!
//! Poll failures are tolerated because the loop is a patrol, not a critical
//! path: a transport error (already retried inside the client) advances a
//! consecutive-failure counter and the loop continues. `MAX_CONSECUTIVE_POLL_FAILURES`
//! in a row is a hard stop — a persistently blind trigger must not silently
//! sit forever.

use std::time::Duration;

use rust_decimal::Decimal;

use crate::api::HlApi;
use crate::client::ValidatedMarketSnapshot;
use crate::errors::HlError;
use crate::format::human;
use crate::types::Symbol;

/// Consecutive poll failures tolerated before hard-stopping (§7).
pub const MAX_CONSECUTIVE_POLL_FAILURES: u32 = 5;

/// Direction of a price trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerWhen {
    /// Fire when `mid >= price`.
    Above,
    /// Fire when `mid <= price`.
    Below,
}

impl std::fmt::Display for TriggerWhen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TriggerWhen::Above => write!(f, "above"),
            TriggerWhen::Below => write!(f, "below"),
        }
    }
}

/// Fully-resolved trigger configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TriggerConfig {
    pub price: Option<(TriggerWhen, Decimal)>,
    pub start_after: Option<Duration>,
    pub poll_interval: Duration,
    /// Same freshness gate pre-flight/slices use (`--max-book-age-ms`),
    /// applied to every book polled while waiting on a price trigger
    /// (Issue #6). `0` disables the max-age check only — see
    /// [`crate::client::ValidatedMarketSnapshot`] for what still applies.
    pub max_book_age_ms: u64,
}

impl TriggerConfig {
    /// True when neither condition is set — start immediately.
    pub fn is_immediate(&self) -> bool {
        self.price.is_none() && self.start_after.is_none()
    }

    /// True when only the time condition is set — the wait loop (and startup)
    /// must never touch `l2Book` in this case (Issue #6).
    pub fn is_time_only(&self) -> bool {
        self.price.is_none() && self.start_after.is_some()
    }

    /// Human-readable description for the startup log (§4 step 5).
    pub fn describe(&self) -> String {
        match (self.price, self.start_after) {
            (Some((when, px)), Some(d)) => format!(
                "Trigger: price {when} {} OR after {} (whichever comes first)",
                human(px),
                humantime::format_duration(d)
            ),
            (Some((when, px)), None) => format!("Trigger: price {when} {}", human(px)),
            (None, Some(d)) => format!("Trigger: after {}", humantime::format_duration(d)),
            (None, None) => "Trigger: immediate".to_string(),
        }
    }
}

/// Why the trigger fired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriggerReason {
    Immediate,
    /// Price condition met; carries the validated snapshot that satisfied it.
    ///
    /// This is the ONLY snapshot a caller should use for `--usd` sizing: it
    /// is re-used as-is, not re-fetched, so the size fixed by the trigger and
    /// the size the run executes always agree on the same market moment
    /// (Issue #6 reproduction: a stale crossing snapshot firing the trigger
    /// while a fresh, non-crossing one silently re-priced the size).
    Price {
        when: TriggerWhen,
        threshold: Decimal,
        mid: Decimal,
        snapshot: ValidatedMarketSnapshot,
    },
    /// `--start-after` elapsed.
    Elapsed {
        after: Duration,
    },
}

impl std::fmt::Display for TriggerReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TriggerReason::Immediate => write!(f, "immediate (no trigger configured)"),
            TriggerReason::Price {
                when,
                threshold,
                mid,
                snapshot: _,
            } => write!(
                f,
                "price {when} {} (mid={})",
                human(*threshold),
                human(*mid)
            ),
            TriggerReason::Elapsed { after } => {
                write!(f, "elapsed {}", humantime::format_duration(*after))
            }
        }
    }
}

/// Pure predicate: does `mid` satisfy the price condition?
pub fn price_condition_met(when: TriggerWhen, threshold: Decimal, mid: Decimal) -> bool {
    match when {
        TriggerWhen::Above => mid >= threshold,
        TriggerWhen::Below => mid <= threshold,
    }
}

/// Block until the trigger fires (§7).
///
/// Takes the `HlApi` seam (not a concrete client) so it can be driven by
/// `ScriptedApi` under virtual time in tests exactly like the slice loop
/// (Issue #6) — production still passes a `&HlClient`, which coerces to
/// `&dyn HlApi`.
///
/// Every polled book goes through [`crate::client::ValidatedMarketSnapshot`]
/// — the same freshness AND semantic validation (coin match, positive
/// levels, uncrossed, ordered, future-skew) pre-flight/slices apply — so a
/// stale or malformed snapshot can never satisfy the price condition
/// (Issue #6 reproduction case).
///
/// Uses `tokio::time` throughout so tests can drive it with
/// `tokio::time::pause()`.
pub async fn wait_for_trigger(
    client: &dyn HlApi,
    symbol: &Symbol,
    cfg: &TriggerConfig,
) -> Result<TriggerReason, HlError> {
    if cfg.is_immediate() {
        return Ok(TriggerReason::Immediate);
    }

    let deadline = cfg.start_after.map(|d| tokio::time::Instant::now() + d);
    let mut consecutive_failures = 0u32;

    loop {
        // Time condition — checked first so a zero/elapsed deadline fires
        // without needing a successful poll.
        if let (Some(dl), Some(after)) = (deadline, cfg.start_after) {
            if tokio::time::Instant::now() >= dl {
                return Ok(TriggerReason::Elapsed { after });
            }
        }

        // Price condition.
        if let Some((when, threshold)) = cfg.price {
            let validated = match client.fetch_l2_book(symbol).await {
                Ok(book) => ValidatedMarketSnapshot::validate(&book, symbol, cfg.max_book_age_ms),
                Err(e) => Err(e),
            };
            match validated {
                Ok(snapshot) => {
                    let mid = snapshot.mid;
                    consecutive_failures = 0;
                    tracing::debug!(
                        symbol = %symbol,
                        mid = %human(mid),
                        threshold = %human(threshold),
                        when = %when,
                        "trigger poll"
                    );
                    if price_condition_met(when, threshold, mid) {
                        return Ok(TriggerReason::Price {
                            when,
                            threshold,
                            mid,
                            snapshot,
                        });
                    }
                }
                Err(e) => {
                    consecutive_failures += 1;
                    tracing::warn!(
                        symbol = %symbol,
                        consecutive_failures,
                        error = %e,
                        "trigger poll failed"
                    );
                }
            }
            if consecutive_failures >= MAX_CONSECUTIVE_POLL_FAILURES {
                return Err(HlError::Network(format!(
                    "trigger poll failed {consecutive_failures} times consecutively; aborting"
                )));
            }
        }

        // Sleep until the next poll, but never past the time deadline.
        let mut sleep_for = cfg.poll_interval;
        if let Some(dl) = deadline {
            let remaining = dl.saturating_duration_since(tokio::time::Instant::now());
            if remaining < sleep_for {
                sleep_for = remaining;
            }
        }
        tokio::time::sleep(sleep_for).await;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use rust_decimal_macros::dec;

    fn cfg(price: Option<(TriggerWhen, Decimal)>, start_after: Option<Duration>) -> TriggerConfig {
        TriggerConfig {
            price,
            start_after,
            poll_interval: Duration::from_secs(2),
            max_book_age_ms: 3000,
        }
    }

    // === pure predicate ===

    #[test]
    fn above_fires_at_or_over_threshold() {
        assert!(price_condition_met(TriggerWhen::Above, dec!(40), dec!(40)));
        assert!(price_condition_met(TriggerWhen::Above, dec!(40), dec!(41)));
        assert!(!price_condition_met(
            TriggerWhen::Above,
            dec!(40),
            dec!(39.99)
        ));
    }

    #[test]
    fn below_fires_at_or_under_threshold() {
        assert!(price_condition_met(TriggerWhen::Below, dec!(40), dec!(40)));
        assert!(price_condition_met(TriggerWhen::Below, dec!(40), dec!(39)));
        assert!(!price_condition_met(
            TriggerWhen::Below,
            dec!(40),
            dec!(40.01)
        ));
    }

    // === config description ===

    #[test]
    fn immediate_when_nothing_configured() {
        let c = cfg(None, None);
        assert!(c.is_immediate());
        assert_eq!(c.describe(), "Trigger: immediate");
    }

    #[test]
    fn describe_mentions_or_when_both_set() {
        let c = cfg(
            Some((TriggerWhen::Above, dec!(40.5))),
            Some(Duration::from_secs(600)),
        );
        assert!(!c.is_immediate());
        let d = c.describe();
        assert!(d.contains("price above 40.5"), "{d}");
        assert!(d.contains("OR after"), "{d}");
        assert!(d.contains("whichever comes first"), "{d}");
    }

    #[test]
    fn describe_price_only_and_time_only() {
        assert_eq!(
            cfg(Some((TriggerWhen::Below, dec!(30))), None).describe(),
            "Trigger: price below 30"
        );
        assert_eq!(
            cfg(None, Some(Duration::from_secs(60))).describe(),
            "Trigger: after 1m"
        );
    }

    // === wait loop (deterministic virtual time) ===

    #[tokio::test(start_paused = true)]
    async fn immediate_returns_without_polling() {
        // No client calls are possible here — a bogus URL proves no HTTP.
        let client = crate::client::HlClient::new(
            crate::client::HlConfig::new(crate::client::Network::Testnet)
                .with_overrides(Some("http://127.0.0.1:1/info".into()), None),
            None,
        )
        .unwrap();
        let reason = wait_for_trigger(&client, &Symbol::new("HYPE"), &cfg(None, None))
            .await
            .unwrap();
        assert_eq!(reason, TriggerReason::Immediate);
    }

    #[tokio::test(start_paused = true)]
    async fn time_only_trigger_fires_after_deadline_without_network() {
        // start_after with no price trigger must never touch the network.
        let client = crate::client::HlClient::new(
            crate::client::HlConfig::new(crate::client::Network::Testnet)
                .with_overrides(Some("http://127.0.0.1:1/info".into()), None),
            None,
        )
        .unwrap();
        let start = tokio::time::Instant::now();
        let reason = wait_for_trigger(
            &client,
            &Symbol::new("HYPE"),
            &cfg(None, Some(Duration::from_secs(300))),
        )
        .await
        .unwrap();
        assert_eq!(
            reason,
            TriggerReason::Elapsed {
                after: Duration::from_secs(300)
            }
        );
        // Virtual clock advanced by the full deadline, no more.
        assert_eq!(start.elapsed(), Duration::from_secs(300));
    }

    #[tokio::test(start_paused = true)]
    async fn zero_start_after_fires_on_first_check() {
        let client = crate::client::HlClient::new(
            crate::client::HlConfig::new(crate::client::Network::Testnet)
                .with_overrides(Some("http://127.0.0.1:1/info".into()), None),
            None,
        )
        .unwrap();
        let reason = wait_for_trigger(
            &client,
            &Symbol::new("HYPE"),
            &cfg(None, Some(Duration::ZERO)),
        )
        .await
        .unwrap();
        assert!(matches!(reason, TriggerReason::Elapsed { .. }));
    }

    // === virtual-time tests on the `HlApi` seam (Issue #6) ===
    //
    // These exercise `wait_for_trigger` through `ScriptedApi`, the same test
    // double the slice loop uses — proving the trigger and the loop now share
    // one seam instead of the trigger being pinned to a concrete `HlClient`.

    fn book(coin: &str, bid: Decimal, ask: Decimal, time_ms: i64) -> crate::types::OrderBook {
        crate::types::OrderBook {
            coin: coin.to_string(),
            bids: vec![crate::types::BookLevel {
                px: bid,
                sz: dec!(10),
                n: 1,
            }],
            asks: vec![crate::types::BookLevel {
                px: ask,
                sz: dec!(10),
                n: 1,
            }],
            time_ms,
        }
    }

    /// Issue #6 reproduction: a STALE snapshot that already crosses the
    /// threshold must NOT fire the trigger; only a later snapshot that passes
    /// validation may.
    #[tokio::test(start_paused = true)]
    async fn stale_crossing_snapshot_does_not_fire_then_fresh_noncrossing_keeps_waiting() {
        let now = chrono::Utc::now().timestamp_millis();
        let api = crate::api::ScriptedApi::new()
            // Poll 1: stale (1 hour old) but crosses "above 40" at mid=41.
            .push_book(Ok(book("HYPE", dec!(40.99), dec!(41.01), now - 3_600_000)))
            // Poll 2: fresh, and genuinely non-crossing (mid=39) — must also
            // not fire.
            .push_book(Ok(book("HYPE", dec!(38.99), dec!(39.01), now)))
            // Poll 3: fresh and crossing — this is the only poll allowed to
            // fire the trigger.
            .with_default_book(book("HYPE", dec!(40.99), dec!(41.01), now));

        let reason = wait_for_trigger(
            &api,
            &Symbol::new("HYPE"),
            &cfg(Some((TriggerWhen::Above, dec!(40))), None),
        )
        .await
        .unwrap();

        match reason {
            TriggerReason::Price { mid, snapshot, .. } => {
                assert_eq!(mid, dec!(41.0));
                assert_eq!(snapshot.mid, dec!(41.0));
            }
            other => panic!("expected a price trigger, got {other:?}"),
        }
        // Exactly 3 polls: the stale-but-crossing one was rejected, the
        // fresh-but-non-crossing one correctly did not satisfy the
        // condition, and only the third (fresh + crossing) fired.
        assert_eq!(api.calls().len(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn wrong_coin_response_never_fires_the_trigger() {
        let now = chrono::Utc::now().timestamp_millis();
        let api = crate::api::ScriptedApi::new()
            .push_book(Ok(book("BTC", dec!(99999), dec!(100001), now)))
            .with_default_book(book("BTC", dec!(99999), dec!(100001), now));

        let result = tokio::time::timeout(
            Duration::from_secs(60),
            wait_for_trigger(
                &api,
                &Symbol::new("HYPE"),
                &cfg(Some((TriggerWhen::Above, dec!(40))), None),
            ),
        )
        .await
        .unwrap();

        // A persistently wrong-coin response must hard-stop (fail closed),
        // never silently satisfy the trigger with the wrong asset's price.
        assert!(result.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn crossed_book_never_fires_the_trigger() {
        let now = chrono::Utc::now().timestamp_millis();
        // best_bid > best_ask: crossed. Would "cross" 40 if taken at face
        // value, but must be rejected as malformed instead.
        let crossed = book("HYPE", dec!(41.0), dec!(40.5), now);
        let api = crate::api::ScriptedApi::new()
            .push_book(Ok(crossed.clone()))
            .with_default_book(crossed);

        let result = tokio::time::timeout(
            Duration::from_secs(60),
            wait_for_trigger(
                &api,
                &Symbol::new("HYPE"),
                &cfg(Some((TriggerWhen::Above, dec!(40))), None),
            ),
        )
        .await
        .unwrap();

        assert!(result.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn future_timestamp_beyond_skew_tolerance_never_fires_the_trigger() {
        let now = chrono::Utc::now().timestamp_millis();
        let future = book(
            "HYPE",
            dec!(40.99),
            dec!(41.01),
            now + crate::client::MAX_FUTURE_SKEW_MS + 5_000,
        );
        let api = crate::api::ScriptedApi::new()
            .push_book(Ok(future.clone()))
            .with_default_book(future);

        let result = tokio::time::timeout(
            Duration::from_secs(60),
            wait_for_trigger(
                &api,
                &Symbol::new("HYPE"),
                &cfg(Some((TriggerWhen::Above, dec!(40))), None),
            ),
        )
        .await
        .unwrap();

        assert!(result.is_err());
    }

    /// The trigger's virtual-time tests run on the SAME `&dyn HlApi` seam
    /// production uses (`ScriptedApi` here, `HlClient` in `main.rs`) — no
    /// separate test-only code path.
    #[tokio::test(start_paused = true)]
    async fn time_only_trigger_still_makes_zero_book_calls_through_the_hlapi_seam() {
        let api = crate::api::ScriptedApi::new(); // no books scripted at all
        let reason = wait_for_trigger(
            &api,
            &Symbol::new("HYPE"),
            &cfg(None, Some(Duration::from_secs(120))),
        )
        .await
        .unwrap();
        assert_eq!(
            reason,
            TriggerReason::Elapsed {
                after: Duration::from_secs(120)
            }
        );
        assert_eq!(api.calls().len(), 0);
    }
}
