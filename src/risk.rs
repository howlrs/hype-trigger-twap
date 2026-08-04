//! Live-execution risk envelope (Issue #3).
//!
//! Before Issue #3, `--slippage-bps` rejected only negative values (no upper
//! bound — a fat-fingered `--slippage-bps 20000` on a long side would sign a
//! limit price at 3x the best ask), `--usd`/`--size` had no maximum notional
//! cap, and `HL_INFO_URL`/`HL_EXCHANGE_URL` could be overridden even in live
//! mode. This module is the SINGLE place the risk policy constants and the
//! validation logic that enforces them live — both `main.rs`'s CLI validation
//! and `twap.rs`'s per-slice loop import from here, so the two can never drift
//! apart into duplicated (and possibly inconsistent) magic numbers.
//!
//! Everything in [`RiskEnvelope`] is constructed BEFORE any network call, so
//! a bad risk configuration fails fast and unconditionally before the first
//! `/exchange` (and, for the endpoint checks, before the first `/info` too).

use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use crate::client::Network;

/// Slippage at or above this many basis points is rejected unconditionally —
/// no flag can override it. At this cushion a long's limit price is already
/// double the best ask (100% above), which is not a "slippage cushion" in any
/// meaningful sense; a short's limit price would be non-positive or close to
/// it. See [`taker_limit_price`](crate::format::taker_limit_price) for the
/// price formula this bounds.
pub const SLIPPAGE_HARD_CAP_BPS: Decimal = dec!(10000);

/// Slippage above this many basis points requires `--allow-high-slippage`.
/// Below this threshold (the default operating range) no extra flag is
/// needed. This is a WARN-and-require-opt-in threshold, not a hard reject —
/// see [`SLIPPAGE_HARD_CAP_BPS`] for the unconditional cutoff.
pub const SLIPPAGE_WARN_THRESHOLD_BPS: Decimal = dec!(1000);

/// Errors produced while validating the risk envelope. All of these must be
/// caught BEFORE any network access — see [`RiskEnvelope::validate_slippage`]
/// and friends.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RiskError {
    #[error(
        "--slippage-bps {bps} is >= the hard cap of {cap} bps; this is rejected unconditionally, no override exists"
    )]
    SlippageHardCapExceeded { bps: Decimal, cap: Decimal },

    #[error(
        "--slippage-bps {bps} exceeds the warn threshold of {threshold} bps; pass --allow-high-slippage to proceed anyway"
    )]
    SlippageWarnThresholdExceeded { bps: Decimal, threshold: Decimal },

    #[error("--slippage-bps must be >= 0, got {0}")]
    NegativeSlippage(Decimal),

    #[error(
        "computed limit price {px} is non-positive for side {side} at slippage {bps}bps (bid={bid}, ask={ask}); refusing to proceed"
    )]
    NonPositiveLimitPrice {
        px: Decimal,
        side: crate::types::Side,
        bps: Decimal,
        bid: Decimal,
        ask: Decimal,
    },

    #[error("live mode requires --max-notional-usd (breaking change, see docs/USAGE.md)")]
    MaxNotionalRequired,

    #[error("--max-notional-usd must be > 0, got {0}")]
    NonPositiveMaxNotional(Decimal),

    #[error("requested notional ${requested} exceeds --max-notional-usd ${cap}")]
    NotionalCapExceeded { requested: Decimal, cap: Decimal },

    #[error(
        "live mode + custom endpoint override ({url}) is rejected by default; pass --allow-custom-endpoints (and use https://) to override"
    )]
    CustomEndpointRejected { url: String },

    #[error("--allow-custom-endpoints requires an https:// URL, got {url}")]
    CustomEndpointNotHttps { url: String },
}

/// Everything needed to judge whether a slippage/price configuration is safe,
/// resolved ONCE before any network access.
#[derive(Debug, Clone, Copy)]
pub struct RiskEnvelope {
    pub slippage_bps: Decimal,
    pub allow_high_slippage: bool,
    /// `None` in read-only mode (no cap is required there). `Some` in live
    /// mode — enforced as required by
    /// [`RiskEnvelope::validate_max_notional_required`].
    pub max_notional_usd: Option<Decimal>,
}

impl RiskEnvelope {
    /// §1: slippage bounds. Applies REGARDLESS of read-only vs live — a
    /// nonsensical slippage cushion is worth rejecting even in a dry run, so
    /// operators see the mistake before flipping `--read-only false`.
    ///
    /// - `< 0` → rejected (pre-existing check, now centralized here too).
    /// - `>= SLIPPAGE_HARD_CAP_BPS` → rejected unconditionally, no override.
    /// - `> SLIPPAGE_WARN_THRESHOLD_BPS` → rejected unless `allow_high_slippage`.
    pub fn validate_slippage(
        slippage_bps: Decimal,
        allow_high_slippage: bool,
    ) -> Result<(), RiskError> {
        if slippage_bps < Decimal::ZERO {
            return Err(RiskError::NegativeSlippage(slippage_bps));
        }
        if slippage_bps >= SLIPPAGE_HARD_CAP_BPS {
            return Err(RiskError::SlippageHardCapExceeded {
                bps: slippage_bps,
                cap: SLIPPAGE_HARD_CAP_BPS,
            });
        }
        if slippage_bps > SLIPPAGE_WARN_THRESHOLD_BPS && !allow_high_slippage {
            return Err(RiskError::SlippageWarnThresholdExceeded {
                bps: slippage_bps,
                threshold: SLIPPAGE_WARN_THRESHOLD_BPS,
            });
        }
        Ok(())
    }

    /// §1 cont'd: a non-positive limit price is rejected unconditionally, no
    /// override — this is the failure mode a short-side slippage above 10000
    /// bps would otherwise produce (and which `validate_slippage` alone
    /// already blocks via the hard cap, but this is checked independently as
    /// a defence-in-depth belt-and-braces guard against any other path that
    /// might compute a non-positive price).
    pub fn validate_limit_price(
        px: Decimal,
        side: crate::types::Side,
        bps: Decimal,
        bid: Decimal,
        ask: Decimal,
    ) -> Result<(), RiskError> {
        if px <= Decimal::ZERO {
            return Err(RiskError::NonPositiveLimitPrice {
                px,
                side,
                bps,
                bid,
                ask,
            });
        }
        Ok(())
    }

    /// §2: live mode requires `--max-notional-usd` (breaking change, PM
    /// decision, documented in docs/USAGE.md and docs/OPERATIONS.md).
    /// Read-only mode does not require this — it places nothing.
    pub fn validate_max_notional_required(
        read_only: bool,
        max_notional_usd: Option<Decimal>,
    ) -> Result<Decimal, RiskError> {
        if read_only {
            // Not required in read-only, but if given it must still be sane.
            if let Some(cap) = max_notional_usd {
                if cap <= Decimal::ZERO {
                    return Err(RiskError::NonPositiveMaxNotional(cap));
                }
            }
            return Ok(max_notional_usd.unwrap_or(Decimal::MAX));
        }
        let cap = max_notional_usd.ok_or(RiskError::MaxNotionalRequired)?;
        if cap <= Decimal::ZERO {
            return Err(RiskError::NonPositiveMaxNotional(cap));
        }
        Ok(cap)
    }

    /// §2 cont'd: check a requested/estimated notional against the cap.
    /// Used both at CLI-validation time (against `--usd`, or a freshly
    /// computed conservative limit price × `--size`) and again before EACH
    /// slice in the TWAP loop (against the actual order px for that slice).
    pub fn check_notional_cap(requested: Decimal, cap: Decimal) -> Result<(), RiskError> {
        if requested > cap {
            return Err(RiskError::NotionalCapExceeded { requested, cap });
        }
        Ok(())
    }

    /// §3: live + a custom `HL_INFO_URL`/`HL_EXCHANGE_URL` override is
    /// rejected by default. `allow_custom_endpoints` opts in, but even then
    /// the URL must be `https://` — plaintext http in live mode is refused
    /// unconditionally — UNLESS the host is loopback (`127.0.0.1` /
    /// `localhost`), which is exempted from the https requirement so a live
    /// integration test can point at a local mock HTTP server (mockito has
    /// no TLS support) without weakening the guarantee for any real, remote
    /// endpoint. A loopback URL can never be the genuine Hyperliquid API, so
    /// this carve-out cannot be used to smuggle a plaintext MITM-able
    /// override of a real endpoint into live trading.
    ///
    /// `read_only` mocks are UNAFFECTED: this check is only invoked for live
    /// mode (the caller in `main.rs` gates on `!read_only` before calling
    /// this), so the existing mockito test seam (setting `HL_INFO_URL` /
    /// `HL_EXCHANGE_URL` to a local `http://127.0.0.1:PORT` mock) continues
    /// to work unchanged for read-only and any live test that does not
    /// exercise this specific rejection path.
    pub fn validate_endpoint_override(
        url: &str,
        allow_custom_endpoints: bool,
    ) -> Result<(), RiskError> {
        if !allow_custom_endpoints {
            return Err(RiskError::CustomEndpointRejected { url: url.into() });
        }
        if !url.starts_with("https://") && !is_loopback_url(url) {
            return Err(RiskError::CustomEndpointNotHttps { url: url.into() });
        }
        Ok(())
    }
}

/// True when `url`'s host is loopback (`http://127.0.0.1...` or
/// `http://localhost...`), the narrow carve-out documented on
/// [`RiskEnvelope::validate_endpoint_override`]. Deliberately conservative —
/// only matches an explicit `http://` scheme on those two exact hosts, not
/// e.g. `127.0.0.1.evil.example.com` or any other bypass shape.
fn is_loopback_url(url: &str) -> bool {
    for prefix in ["http://127.0.0.1", "http://localhost"] {
        if let Some(rest) = url.strip_prefix(prefix) {
            // Must be followed by end-of-string, `:` (port), or `/` (path) —
            // not by more hostname characters.
            if rest.is_empty() || rest.starts_with(':') || rest.starts_with('/') {
                return true;
            }
        }
    }
    false
}

/// One-shot pre-send summary line (§4), printed once before execution begins:
/// resolved network/origin, symbol, side, target, slippage, notional cap.
#[allow(clippy::too_many_arguments)]
pub fn pre_send_summary(
    network: Network,
    info_url: &str,
    exchange_url: &str,
    symbol: &str,
    side: crate::types::Side,
    target_desc: &str,
    slippage_bps: Decimal,
    max_notional_usd: Option<Decimal>,
) -> String {
    let cap_desc = match max_notional_usd {
        Some(cap) => format!("${}", crate::format::human(cap)),
        None => "unbounded (read-only)".to_string(),
    };
    format!(
        "Pre-send summary: network={network} info={info_url} exchange={exchange_url} \
         symbol={symbol} side={side} target={target_desc} slippage={slippage_bps}bps \
         max_notional_usd={cap_desc}"
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::types::Side;

    // === slippage bounds ===

    #[test]
    fn slippage_negative_is_rejected() {
        let err = RiskEnvelope::validate_slippage(dec!(-1), false).unwrap_err();
        assert!(matches!(err, RiskError::NegativeSlippage(_)));
    }

    #[test]
    fn slippage_zero_is_accepted() {
        RiskEnvelope::validate_slippage(dec!(0), false).unwrap();
    }

    #[test]
    fn slippage_at_warn_threshold_is_accepted_without_override() {
        // Exactly 1000 bps: threshold is "> 1000", so 1000 itself is fine.
        RiskEnvelope::validate_slippage(SLIPPAGE_WARN_THRESHOLD_BPS, false).unwrap();
    }

    #[test]
    fn slippage_just_over_warn_threshold_requires_override() {
        let just_over = SLIPPAGE_WARN_THRESHOLD_BPS + dec!(1);
        let err = RiskEnvelope::validate_slippage(just_over, false).unwrap_err();
        assert!(matches!(
            err,
            RiskError::SlippageWarnThresholdExceeded { .. }
        ));
        RiskEnvelope::validate_slippage(just_over, true).unwrap();
    }

    #[test]
    fn slippage_just_under_hard_cap_is_accepted_with_override() {
        let just_under = SLIPPAGE_HARD_CAP_BPS - dec!(1);
        RiskEnvelope::validate_slippage(just_under, true).unwrap();
    }

    #[test]
    fn slippage_at_hard_cap_is_rejected_unconditionally() {
        let err = RiskEnvelope::validate_slippage(SLIPPAGE_HARD_CAP_BPS, true).unwrap_err();
        assert!(matches!(err, RiskError::SlippageHardCapExceeded { .. }));
    }

    #[test]
    fn slippage_over_hard_cap_is_rejected_unconditionally() {
        let over = SLIPPAGE_HARD_CAP_BPS + dec!(1);
        let err = RiskEnvelope::validate_slippage(over, true).unwrap_err();
        assert!(matches!(err, RiskError::SlippageHardCapExceeded { .. }));
    }

    // === limit price ===

    #[test]
    fn zero_limit_price_is_rejected() {
        let err = RiskEnvelope::validate_limit_price(
            dec!(0),
            Side::Short,
            dec!(12000),
            dec!(100),
            dec!(101),
        )
        .unwrap_err();
        assert!(matches!(err, RiskError::NonPositiveLimitPrice { .. }));
    }

    #[test]
    fn negative_limit_price_is_rejected() {
        let err = RiskEnvelope::validate_limit_price(
            dec!(-5),
            Side::Short,
            dec!(15000),
            dec!(100),
            dec!(101),
        )
        .unwrap_err();
        assert!(matches!(err, RiskError::NonPositiveLimitPrice { .. }));
    }

    #[test]
    fn positive_limit_price_is_accepted() {
        RiskEnvelope::validate_limit_price(dec!(100), Side::Long, dec!(20), dec!(99), dec!(100))
            .unwrap();
    }

    // === max notional required (live) ===

    #[test]
    fn read_only_does_not_require_max_notional() {
        let cap = RiskEnvelope::validate_max_notional_required(true, None).unwrap();
        assert_eq!(cap, Decimal::MAX);
    }

    #[test]
    fn live_without_max_notional_is_rejected() {
        let err = RiskEnvelope::validate_max_notional_required(false, None).unwrap_err();
        assert!(matches!(err, RiskError::MaxNotionalRequired));
    }

    #[test]
    fn live_with_max_notional_is_accepted() {
        let cap = RiskEnvelope::validate_max_notional_required(false, Some(dec!(5000))).unwrap();
        assert_eq!(cap, dec!(5000));
    }

    #[test]
    fn live_with_non_positive_max_notional_is_rejected() {
        let err = RiskEnvelope::validate_max_notional_required(false, Some(dec!(0))).unwrap_err();
        assert!(matches!(err, RiskError::NonPositiveMaxNotional(_)));
    }

    // === notional cap boundary tests (long/short x usd/size) ===
    //
    // §1000 target price convention used throughout: mid = 100, so a small
    // slippage keeps prices near 100. Cap is fixed at $10_000 for all four
    // scenarios; the "requested" side varies.

    const CAP: Decimal = dec!(10000);

    #[test]
    fn long_usd_just_under_cap_is_accepted() {
        let requested = CAP - dec!(0.01);
        RiskEnvelope::check_notional_cap(requested, CAP).unwrap();
    }

    #[test]
    fn long_usd_just_over_cap_is_rejected() {
        let requested = CAP + dec!(0.01);
        let err = RiskEnvelope::check_notional_cap(requested, CAP).unwrap_err();
        assert!(matches!(err, RiskError::NotionalCapExceeded { .. }));
    }

    #[test]
    fn long_size_just_under_cap_is_accepted() {
        // size 99.9 at conservative long limit price 100.01 => 9990.999
        let sz = dec!(99.9);
        let conservative_px = dec!(100.01);
        let requested = sz * conservative_px;
        assert!(requested < CAP, "{requested}");
        RiskEnvelope::check_notional_cap(requested, CAP).unwrap();
    }

    #[test]
    fn long_size_just_over_cap_is_rejected() {
        let sz = dec!(100.01);
        let conservative_px = dec!(100.01);
        let requested = sz * conservative_px;
        assert!(requested > CAP);
        let err = RiskEnvelope::check_notional_cap(requested, CAP).unwrap_err();
        assert!(matches!(err, RiskError::NotionalCapExceeded { .. }));
    }

    #[test]
    fn short_usd_just_under_cap_is_accepted() {
        let requested = CAP - dec!(0.01);
        RiskEnvelope::check_notional_cap(requested, CAP).unwrap();
    }

    #[test]
    fn short_usd_just_over_cap_is_rejected() {
        let requested = CAP + dec!(0.01);
        let err = RiskEnvelope::check_notional_cap(requested, CAP).unwrap_err();
        assert!(matches!(err, RiskError::NotionalCapExceeded { .. }));
    }

    #[test]
    fn short_size_just_under_cap_is_accepted() {
        // Short's conservative limit sits BELOW mid, but for a notional cap
        // check the conservative direction is the same as long: use the
        // WORST-CASE (highest) price the size could execute at, which for a
        // short taker limit computed with slippage below the bid is still
        // its OWN limit price (there is no upside beyond it for a taker
        // IOC), so px here is short's own limit.
        let sz = dec!(99.9);
        let conservative_px = dec!(100.01);
        let requested = sz * conservative_px;
        assert!(requested < CAP, "{requested}");
        RiskEnvelope::check_notional_cap(requested, CAP).unwrap();
    }

    #[test]
    fn short_size_just_over_cap_is_rejected() {
        let sz = dec!(100.01);
        let conservative_px = dec!(100.01);
        let requested = sz * conservative_px;
        assert!(requested > CAP);
        let err = RiskEnvelope::check_notional_cap(requested, CAP).unwrap_err();
        assert!(matches!(err, RiskError::NotionalCapExceeded { .. }));
    }

    // === endpoint override ===

    #[test]
    fn custom_endpoint_rejected_by_default() {
        let err = RiskEnvelope::validate_endpoint_override("http://127.0.0.1:1234/info", false)
            .unwrap_err();
        assert!(matches!(err, RiskError::CustomEndpointRejected { .. }));
    }

    #[test]
    fn custom_endpoint_http_rejected_even_with_override() {
        let err =
            RiskEnvelope::validate_endpoint_override("http://example.com/info", true).unwrap_err();
        assert!(matches!(err, RiskError::CustomEndpointNotHttps { .. }));
    }

    #[test]
    fn custom_endpoint_https_accepted_with_override() {
        RiskEnvelope::validate_endpoint_override("https://example.com/info", true).unwrap();
    }

    #[test]
    fn custom_endpoint_loopback_http_accepted_with_override() {
        // Test/dev carve-out: mockito has no TLS support, so a live
        // integration test pointing at a local mock server must still work
        // once --allow-custom-endpoints is passed.
        RiskEnvelope::validate_endpoint_override("http://127.0.0.1:12345/info", true).unwrap();
        RiskEnvelope::validate_endpoint_override("http://localhost:12345/info", true).unwrap();
    }

    #[test]
    fn custom_endpoint_loopback_http_still_rejected_without_override() {
        let err = RiskEnvelope::validate_endpoint_override("http://127.0.0.1:12345/info", false)
            .unwrap_err();
        assert!(matches!(err, RiskError::CustomEndpointRejected { .. }));
    }

    #[test]
    fn custom_endpoint_loopback_lookalike_host_is_not_exempted() {
        // A hostname that merely starts with the loopback string must NOT be
        // treated as loopback (guards against a bypass like
        // "127.0.0.1.evil.example.com").
        let err = RiskEnvelope::validate_endpoint_override(
            "http://127.0.0.1.evil.example.com/info",
            true,
        )
        .unwrap_err();
        assert!(matches!(err, RiskError::CustomEndpointNotHttps { .. }));
    }

    // === pre-send summary ===

    #[test]
    fn pre_send_summary_includes_all_required_fields() {
        let s = pre_send_summary(
            Network::Mainnet,
            "https://api.hyperliquid.xyz/info",
            "https://api.hyperliquid.xyz/exchange",
            "HYPE",
            Side::Long,
            "$1500 -> 50 HYPE",
            dec!(20),
            Some(dec!(5000)),
        );
        assert!(s.contains("mainnet"), "{s}");
        assert!(s.contains("HYPE"), "{s}");
        assert!(s.contains("long"), "{s}");
        assert!(s.contains("1500"), "{s}");
        assert!(s.contains("20bps"), "{s}");
        assert!(s.contains("5000"), "{s}");
    }

    #[test]
    fn pre_send_summary_read_only_shows_unbounded() {
        let s = pre_send_summary(
            Network::Testnet,
            "https://api.hyperliquid-testnet.xyz/info",
            "https://api.hyperliquid-testnet.xyz/exchange",
            "HYPE",
            Side::Short,
            "10 HYPE",
            dec!(20),
            None,
        );
        assert!(s.contains("unbounded"), "{s}");
    }
}
