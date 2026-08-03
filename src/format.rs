//! Price / size rounding to the Hyperliquid tick grid (§6).
//!
//! New logic — the reference `executor` crates never rounded explicitly, they
//! forwarded book prices verbatim (book prices are already on-grid) and sizes
//! straight from the algorithm.
//!
//! HL perp rules:
//! - `sz`: at most `szDecimals` fractional digits. We always round DOWN so a
//!   slice never exceeds its target (over-fill is unrecoverable, under-fill
//!   carries to the next slice).
//! - `px`: at most 5 significant digits AND at most `6 - szDecimals`
//!   fractional digits. Integer prices are exempt from the significant-digit
//!   cap (HL accepts e.g. `123456`).
//! - Direction (Gemini review): a taker price must stay marketable after
//!   rounding, so LONG rounds UP (ceil) and SHORT rounds DOWN (floor).
//!   Rounding a long's limit down could push it below the ask and turn an IOC
//!   into a no-op.
//!
//! Wire serialization is `eip712::decimal_to_hl_wire` (`Decimal::normalize`),
//! which strips trailing zeros — `2387.0` must go out as `2387`.

use rust_decimal::prelude::ToPrimitive;
use rust_decimal::{Decimal, RoundingStrategy};

use crate::types::Side;

/// HL perp maximum significant digits for a price.
const MAX_PX_SIGNIFICANT_DIGITS: u32 = 5;
/// HL perp fractional-digit budget: `MAX_PX_DECIMALS_BASE - sz_decimals`.
const MAX_PX_DECIMALS_BASE: i64 = 6;

/// Round a size DOWN to `sz_decimals` fractional digits.
///
/// Always truncates toward zero, so the caller can never overshoot its target.
pub fn round_size(sz: Decimal, sz_decimals: u32) -> Decimal {
    sz.round_dp_with_strategy(sz_decimals, RoundingStrategy::ToZero)
}

/// Round a price onto the HL grid, in the direction that keeps a taker order
/// marketable for `side` (long → ceil, short → floor).
///
/// Applies the fractional-digit cap first, then the significant-digit cap.
/// Both use the same direction so the result can only move away from the
/// touch (more aggressive), never toward it.
pub fn round_price(px: Decimal, sz_decimals: u32, side: Side) -> Decimal {
    if px <= Decimal::ZERO {
        return px;
    }
    let strategy = match side {
        // Long buys: round the limit UP so it still crosses the ask.
        Side::Long => RoundingStrategy::ToPositiveInfinity,
        // Short sells: round the limit DOWN so it still crosses the bid.
        Side::Short => RoundingStrategy::ToNegativeInfinity,
    };

    // 1. Fractional-digit cap: 6 - szDecimals for perps (clamped at >= 0).
    let max_decimals = (MAX_PX_DECIMALS_BASE - i64::from(sz_decimals)).max(0) as u32;
    let px = px.round_dp_with_strategy(max_decimals, strategy);

    // 2. Significant-digit cap: at most 5. Integer prices are exempt — HL
    //    accepts 6-digit whole numbers like 123456.
    round_to_significant_digits(px, MAX_PX_SIGNIFICANT_DIGITS, strategy)
}

/// Round `px` to at most `sig` significant digits using `strategy`.
///
/// Prices whose integer part already has >= `sig` digits are returned
/// unchanged (they are integers at this point, and HL exempts integer prices
/// from the significant-digit rule).
fn round_to_significant_digits(px: Decimal, sig: u32, strategy: RoundingStrategy) -> Decimal {
    if px <= Decimal::ZERO {
        return px;
    }
    let int_digits = integer_digit_count(px);
    if int_digits >= sig {
        // The significant-digit budget is fully consumed by the integer part;
        // drop the fraction entirely (integer prices are exempt from the cap).
        return px.round_dp_with_strategy(0, strategy);
    }
    // Remaining budget goes to the fraction. For px < 1 the leading zeros
    // after the point are NOT significant, so the budget shifts right by the
    // number of leading zeros.
    let allowed_decimals = if px < Decimal::ONE {
        sig + leading_fraction_zeros(px)
    } else {
        sig - int_digits
    };
    px.round_dp_with_strategy(allowed_decimals, strategy)
}

/// Number of digits in the integer part of a positive decimal.
/// `0.5 -> 0`, `9 -> 1`, `1234 -> 4`.
fn integer_digit_count(px: Decimal) -> u32 {
    let trunc = px.trunc();
    if trunc.is_zero() {
        return 0;
    }
    // `trunc` is a whole number; its Display form has no fraction or sign
    // (px > 0 is guaranteed by the caller).
    trunc.normalize().to_string().len() as u32
}

/// For `px < 1`, the count of zeros between the decimal point and the first
/// significant digit. `0.5 -> 0`, `0.0123 -> 1`.
fn leading_fraction_zeros(px: Decimal) -> u32 {
    let mut zeros = 0u32;
    let mut scaled = px;
    // Cap the loop: rust_decimal has 28 significant digits total.
    while scaled < Decimal::ONE && zeros < 28 {
        scaled *= Decimal::TEN;
        if scaled >= Decimal::ONE {
            break;
        }
        zeros += 1;
    }
    zeros
}

/// Taker limit price with a slippage cushion, then snapped to the HL grid.
///
/// `long  = best_ask * (1 + bps/1e4)` rounded up
/// `short = best_bid * (1 - bps/1e4)` rounded down
///
/// Logic copied from `executor-algo::algorithm::taker_limit_price`; the
/// grid snapping is the new part.
pub fn taker_limit_price(
    best_bid: Decimal,
    best_ask: Decimal,
    side: Side,
    slippage_bps: Decimal,
    sz_decimals: u32,
) -> Decimal {
    let bps = slippage_bps / Decimal::from(10_000);
    let raw = match side {
        Side::Long => best_ask + best_ask * bps,
        Side::Short => best_bid - best_bid * bps,
    };
    round_price(raw, sz_decimals, side)
}

/// Notional value in USD of `sz` coins at `px`.
pub fn notional(sz: Decimal, px: Decimal) -> Decimal {
    sz * px
}

/// Format a Decimal for human-facing logs (trailing zeros stripped).
pub fn human(d: Decimal) -> String {
    d.normalize().to_string()
}

/// Convert a Decimal to f64 for coarse display only (never for arithmetic
/// that reaches the wire).
pub fn approx_f64(d: Decimal) -> f64 {
    d.to_f64().unwrap_or(f64::NAN)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::eip712::decimal_to_hl_wire;
    use rust_decimal_macros::dec;

    // === size rounding ===

    #[test]
    fn round_size_truncates_never_rounds_up() {
        assert_eq!(round_size(dec!(1.23456), 2), dec!(1.23));
        assert_eq!(round_size(dec!(1.23999), 2), dec!(1.23));
        assert_eq!(round_size(dec!(1.999), 0), dec!(1));
        assert_eq!(round_size(dec!(0.00009), 4), dec!(0));
    }

    #[test]
    fn round_size_exact_value_unchanged() {
        assert_eq!(round_size(dec!(1.5), 4), dec!(1.5));
        assert_eq!(round_size(dec!(10), 2), dec!(10));
    }

    // === price rounding direction ===

    #[test]
    fn round_price_long_rounds_up_short_rounds_down() {
        // szDecimals=2 → max 4 decimals; 5 sig digits binds first here.
        let px = dec!(2387.14159);
        let long = round_price(px, 2, Side::Long);
        let short = round_price(px, 2, Side::Short);
        assert_eq!(long, dec!(2387.2), "long must ceil");
        assert_eq!(short, dec!(2387.1), "short must floor");
        assert!(long >= px);
        assert!(short <= px);
    }

    #[test]
    fn round_price_direction_holds_for_low_priced_coin() {
        // HYPE-like: szDecimals=2 → 4 decimal budget, but 5 sig digits caps
        // a sub-100 price at 3 decimals.
        let px = dec!(38.123456);
        assert_eq!(round_price(px, 2, Side::Long), dec!(38.124));
        assert_eq!(round_price(px, 2, Side::Short), dec!(38.123));
    }

    // === 5-significant-digit rule ===

    #[test]
    fn five_significant_digits_binds_before_decimal_budget() {
        // szDecimals=0 → decimal budget is 6, but 5 sig digits allows only 1
        // fractional digit at this magnitude.
        assert_eq!(round_price(dec!(1234.5678), 0, Side::Short), dec!(1234.5));
        assert_eq!(round_price(dec!(1234.5678), 0, Side::Long), dec!(1234.6));
    }

    #[test]
    fn decimal_budget_binds_before_significant_digits() {
        // szDecimals=5 → only 1 fractional digit allowed, even though 5 sig
        // digits would permit 3 at this magnitude.
        assert_eq!(round_price(dec!(12.3456), 5, Side::Short), dec!(12.3));
        assert_eq!(round_price(dec!(12.3456), 5, Side::Long), dec!(12.4));
    }

    #[test]
    fn integer_prices_are_exempt_from_significant_digit_cap() {
        // 6-digit integer price (e.g. BTC at 123456) must survive intact.
        assert_eq!(round_price(dec!(123456), 5, Side::Long), dec!(123456));
        assert_eq!(round_price(dec!(123456), 5, Side::Short), dec!(123456));
    }

    #[test]
    fn high_priced_coin_drops_fraction_when_int_part_fills_budget() {
        // BTC-like: 6 integer digits already exceed the 5-sig budget, so the
        // fraction is dropped in the marketable direction.
        assert_eq!(round_price(dec!(104321.7), 5, Side::Long), dec!(104322));
        assert_eq!(round_price(dec!(104321.7), 5, Side::Short), dec!(104321));
    }

    #[test]
    fn exactly_five_integer_digits_drops_fraction() {
        assert_eq!(round_price(dec!(12345.6), 2, Side::Long), dec!(12346));
        assert_eq!(round_price(dec!(12345.6), 2, Side::Short), dec!(12345));
    }

    #[test]
    fn sub_one_price_keeps_five_significant_digits() {
        // szDecimals=0 → 6 decimal budget. Leading zeros are not significant,
        // so 0.0123456 keeps 5 sig digits = 6 decimals here.
        assert_eq!(
            round_price(dec!(0.01234567), 0, Side::Short),
            dec!(0.012345)
        );
        assert_eq!(round_price(dec!(0.01234567), 0, Side::Long), dec!(0.012346));
    }

    #[test]
    fn sub_one_price_respects_decimal_budget_first() {
        // szDecimals=3 → only 3 decimals allowed regardless of sig digits.
        assert_eq!(round_price(dec!(0.123456), 3, Side::Short), dec!(0.123));
        assert_eq!(round_price(dec!(0.123456), 3, Side::Long), dec!(0.124));
    }

    #[test]
    fn already_on_grid_price_is_unchanged_in_both_directions() {
        let px = dec!(2387.1);
        assert_eq!(round_price(px, 2, Side::Long), px);
        assert_eq!(round_price(px, 2, Side::Short), px);
    }

    // === wire normalization interaction ===

    #[test]
    fn rounded_price_wire_form_strips_trailing_zeros() {
        // dec!(2387.0) rounds to itself but must serialize as "2387".
        let px = round_price(dec!(2387.0), 2, Side::Long);
        assert_eq!(decimal_to_hl_wire(px), "2387");
        // And a value that rounds to a whole number likewise.
        let px2 = round_price(dec!(12345.6), 2, Side::Long);
        assert_eq!(decimal_to_hl_wire(px2), "12346");
    }

    #[test]
    fn rounded_size_wire_form_strips_trailing_zeros() {
        let sz = round_size(dec!(0.0050), 4);
        assert_eq!(decimal_to_hl_wire(sz), "0.005");
    }

    // === taker price ===

    #[test]
    fn taker_long_adds_slippage_above_ask() {
        // 50 bps on ask=100 → 100.5, already on grid.
        let px = taker_limit_price(dec!(99), dec!(100), Side::Long, dec!(50), 2);
        assert_eq!(px, dec!(100.5));
        assert!(px > dec!(100), "long taker must be above the ask");
    }

    #[test]
    fn taker_short_subtracts_slippage_below_bid() {
        let px = taker_limit_price(dec!(100), dec!(101), Side::Short, dec!(50), 2);
        assert_eq!(px, dec!(99.5));
        assert!(px < dec!(100), "short taker must be below the bid");
    }

    #[test]
    fn taker_zero_slippage_lands_on_the_touch() {
        assert_eq!(
            taker_limit_price(dec!(99), dec!(100), Side::Long, dec!(0), 2),
            dec!(100)
        );
        assert_eq!(
            taker_limit_price(dec!(99), dec!(100), Side::Short, dec!(0), 2),
            dec!(99)
        );
    }

    #[test]
    fn taker_price_stays_marketable_after_rounding() {
        // ask = 2387.13, +20bps = 2391.90426. At this magnitude the 5-sig-digit
        // cap leaves only one fractional digit, so the long limit ceils to
        // 2392.0 — still comfortably above the ask.
        let ask = dec!(2387.13);
        let px = taker_limit_price(dec!(2387.10), ask, Side::Long, dec!(20), 2);
        assert!(px >= ask, "long taker {px} fell below ask {ask}");
        assert_eq!(px, dec!(2392.0));

        // bid = 2387.10, -20bps = 2382.3258 → floor to 2382.3, below the bid.
        let bid = dec!(2387.10);
        let px = taker_limit_price(bid, ask, Side::Short, dec!(20), 2);
        assert!(px <= bid, "short taker {px} rose above bid {bid}");
        assert_eq!(px, dec!(2382.3));
    }

    // === helpers ===

    #[test]
    fn notional_multiplies() {
        assert_eq!(notional(dec!(2), dec!(50)), dec!(100));
    }

    #[test]
    fn human_strips_trailing_zeros() {
        assert_eq!(human(dec!(1.500)), "1.5");
        assert_eq!(human(dec!(10.0)), "10");
    }

    #[test]
    fn integer_digit_count_basics() {
        assert_eq!(integer_digit_count(dec!(0.5)), 0);
        assert_eq!(integer_digit_count(dec!(9)), 1);
        assert_eq!(integer_digit_count(dec!(1234)), 4);
        assert_eq!(integer_digit_count(dec!(104321.7)), 6);
    }

    #[test]
    fn leading_fraction_zeros_basics() {
        assert_eq!(leading_fraction_zeros(dec!(0.5)), 0);
        assert_eq!(leading_fraction_zeros(dec!(0.05)), 1);
        assert_eq!(leading_fraction_zeros(dec!(0.0123)), 1);
        assert_eq!(leading_fraction_zeros(dec!(0.00123)), 2);
    }
}
