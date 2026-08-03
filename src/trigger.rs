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

use crate::client::HlClient;
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
}

impl TriggerConfig {
    /// True when neither condition is set — start immediately.
    pub fn is_immediate(&self) -> bool {
        self.price.is_none() && self.start_after.is_none()
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
    /// Price condition met; carries the mid that satisfied it.
    Price {
        when: TriggerWhen,
        threshold: Decimal,
        mid: Decimal,
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
/// Uses `tokio::time` throughout so tests can drive it with
/// `tokio::time::pause()`.
pub async fn wait_for_trigger(
    client: &HlClient,
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
            match client.fetch_l2_book(symbol).await {
                Ok(book) => match book.mid() {
                    Some(mid) => {
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
                            });
                        }
                    }
                    None => {
                        consecutive_failures += 1;
                        tracing::warn!(
                            symbol = %symbol,
                            consecutive_failures,
                            "trigger poll: empty book side"
                        );
                    }
                },
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
}
