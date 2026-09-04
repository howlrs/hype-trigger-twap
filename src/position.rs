//! Position-aware execution planning.
//!
//! This module is deliberately pure: it turns one validated account snapshot
//! and one fixed target into an ordered set of phases.  It never reads the
//! exchange and never sends an order, which makes the zero-crossing and
//! reduce-only invariants straightforward to test.

use alloy::primitives::keccak256;
use rust_decimal::{Decimal, RoundingStrategy};
use serde::{Deserialize, Serialize};

use crate::types::{Address, Side, SignedPerpPosition, Symbol};

/// Why a phase exists.  A zero-crossing execution must complete and reconcile
/// `CloseToFlat` before it may begin `OpenFromFlat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PositionPhaseKind {
    Adjust,
    CloseToFlat,
    OpenFromFlat,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PositionPhase {
    pub kind: PositionPhaseKind,
    pub side: Side,
    #[serde(with = "rust_decimal::serde::str")]
    pub size: Decimal,
    pub reduce_only: bool,
}

/// A target fixed at preflight time.  `current_szi` and `target_szi` use HL's
/// signed-size convention: positive long, negative short, zero flat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PositionExecutionPlan {
    pub symbol: Symbol,
    #[serde(with = "rust_decimal::serde::str")]
    pub current_szi: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub target_szi: Decimal,
    pub phases: Vec<PositionPhase>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PositionPlanError {
    #[error("position snapshot symbol mismatch: requested {requested}, snapshot {actual}")]
    SymbolMismatch { requested: Symbol, actual: Symbol },
    #[error("current position {value} exceeds szDecimals={sz_decimals} precision")]
    CurrentPrecision { value: Decimal, sz_decimals: u32 },
    #[error("target USD conversion requires a positive finite price, got {0}")]
    InvalidPrice(Decimal),
    #[error("decimal overflow while computing position target")]
    Overflow,
    #[error("durable fill size must be non-negative, got {0}")]
    InvalidFill(Decimal),
}

/// Quantize without increasing absolute exposure.
pub fn quantize_toward_zero(value: Decimal, sz_decimals: u32) -> Decimal {
    value.round_dp_with_strategy(sz_decimals, RoundingStrategy::ToZero)
}

/// Apply one already-validated durable fill to a signed position. Buys add
/// exposure and sells subtract it. This is used by resume preflight to prove
/// that the exchange's current position equals the position implied by the
/// journal before any new child order can be constructed.
pub fn apply_signed_fill(
    current_szi: Decimal,
    side: Side,
    filled_sz: Decimal,
) -> Result<Decimal, PositionPlanError> {
    if filled_sz < Decimal::ZERO {
        return Err(PositionPlanError::InvalidFill(filled_sz));
    }
    match side {
        Side::Long => current_szi.checked_add(filled_sz),
        Side::Short => current_szi.checked_sub(filled_sz),
    }
    .ok_or(PositionPlanError::Overflow)
}

/// A position-target run is monotonic. If durable fills place the expected
/// exposure outside the closed interval between its frozen initial and target
/// positions, an external/manual action or corrupt journal has crossed the
/// target and resume must not auto-reverse to compensate.
pub fn is_between_frozen_endpoints(
    value: Decimal,
    initial_szi: Decimal,
    target_szi: Decimal,
) -> bool {
    value >= initial_szi.min(target_szi) && value <= initial_szi.max(target_szi)
}

impl PositionExecutionPlan {
    /// Build a plan for a signed target size.  The target is conservatively
    /// quantized toward zero; the exchange snapshot must already conform to
    /// the asset's size precision or planning fails closed.
    pub fn target_size(
        position: &SignedPerpPosition,
        requested_symbol: &Symbol,
        target_szi: Decimal,
        sz_decimals: u32,
    ) -> Result<Self, PositionPlanError> {
        if &position.symbol != requested_symbol {
            return Err(PositionPlanError::SymbolMismatch {
                requested: requested_symbol.clone(),
                actual: position.symbol.clone(),
            });
        }
        let current = quantize_toward_zero(position.szi, sz_decimals);
        if current != position.szi {
            return Err(PositionPlanError::CurrentPrecision {
                value: position.szi,
                sz_decimals,
            });
        }
        let target = quantize_toward_zero(target_szi, sz_decimals);
        let phases = phases_for(current, target)?;
        Ok(Self {
            symbol: requested_symbol.clone(),
            current_szi: current,
            target_szi: target,
            phases,
        })
    }

    /// Convert a signed USD exposure to a size exactly once using the
    /// validated preflight price, then freeze the resulting target size.
    pub fn target_usd(
        position: &SignedPerpPosition,
        requested_symbol: &Symbol,
        target_usd: Decimal,
        preflight_price: Decimal,
        sz_decimals: u32,
    ) -> Result<Self, PositionPlanError> {
        if preflight_price <= Decimal::ZERO {
            return Err(PositionPlanError::InvalidPrice(preflight_price));
        }
        let target_szi = target_usd
            .checked_div(preflight_price)
            .ok_or(PositionPlanError::Overflow)?;
        Self::target_size(position, requested_symbol, target_szi, sz_decimals)
    }

    pub fn flatten(
        position: &SignedPerpPosition,
        requested_symbol: &Symbol,
        sz_decimals: u32,
    ) -> Result<Self, PositionPlanError> {
        Self::target_size(position, requested_symbol, Decimal::ZERO, sz_decimals)
    }

    pub fn is_noop(&self) -> bool {
        self.phases.is_empty()
    }

    pub fn crosses_zero(&self) -> bool {
        self.phases.len() == 2
            && self.phases[0].kind == PositionPhaseKind::CloseToFlat
            && self.phases[1].kind == PositionPhaseKind::OpenFromFlat
    }
}

fn phases_for(current: Decimal, target: Decimal) -> Result<Vec<PositionPhase>, PositionPlanError> {
    if current == target {
        return Ok(Vec::new());
    }

    let current_sign = decimal_sign(current);
    let target_sign = decimal_sign(target);
    if current_sign != 0 && target_sign != 0 && current_sign != target_sign {
        return Ok(vec![
            PositionPhase {
                kind: PositionPhaseKind::CloseToFlat,
                side: side_for_delta(-current)?,
                size: current.abs(),
                reduce_only: true,
            },
            PositionPhase {
                kind: PositionPhaseKind::OpenFromFlat,
                side: side_for_delta(target)?,
                size: target.abs(),
                reduce_only: false,
            },
        ]);
    }

    let delta = target
        .checked_sub(current)
        .ok_or(PositionPlanError::Overflow)?;
    let reduces_existing = current_sign != 0
        && (target_sign == 0 || (current_sign == target_sign && target.abs() < current.abs()));
    Ok(vec![PositionPhase {
        kind: PositionPhaseKind::Adjust,
        side: side_for_delta(delta)?,
        size: delta.abs(),
        reduce_only: reduces_existing,
    }])
}

fn decimal_sign(value: Decimal) -> i8 {
    if value > Decimal::ZERO {
        1
    } else if value < Decimal::ZERO {
        -1
    } else {
        0
    }
}

fn side_for_delta(delta: Decimal) -> Result<Side, PositionPlanError> {
    if delta > Decimal::ZERO {
        Ok(Side::Long)
    } else if delta < Decimal::ZERO {
        Ok(Side::Short)
    } else {
        Err(PositionPlanError::Overflow)
    }
}

/// Public values shown to the operator before a live flatten.  The token is
/// bound to every field that could materially change what will be closed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlattenConfirmation {
    pub schema_version: u16,
    pub network: String,
    pub master: Address,
    pub symbol: Symbol,
    #[serde(with = "rust_decimal::serde::str")]
    pub initial_szi: Decimal,
    pub close_side: Side,
    #[serde(with = "rust_decimal::serde::str")]
    pub max_close_size: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub max_notional_usd: Decimal,
    pub child_algo: String,
    pub execution_deadline_unix_ms: u64,
}

impl FlattenConfirmation {
    pub const SCHEMA_VERSION: u16 = 1;

    /// A short operator-copyable digest.  The `flatten-v1-` prefix prevents a
    /// token from another confirmation workflow being accepted by accident.
    pub fn token(&self) -> Result<String, serde_json::Error> {
        let canonical = serde_json::to_vec(self)?;
        Ok(format!("flatten-v1-{}", hex::encode(keccak256(canonical))))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use rust_decimal_macros::dec;

    use super::*;

    fn pos(szi: Decimal) -> SignedPerpPosition {
        SignedPerpPosition {
            symbol: Symbol::new("HYPE"),
            szi,
        }
    }

    #[test]
    fn long_short_and_flat_adjustments_choose_reduce_only_correctly() {
        let cases = [
            (dec!(2), dec!(3), Side::Long, dec!(1), false),
            (dec!(2), dec!(1), Side::Short, dec!(1), true),
            (dec!(-2), dec!(-3), Side::Short, dec!(1), false),
            (dec!(-2), dec!(-1), Side::Long, dec!(1), true),
            (dec!(0), dec!(2), Side::Long, dec!(2), false),
            (dec!(2), dec!(0), Side::Short, dec!(2), true),
        ];
        for (current, target, side, size, reduce_only) in cases {
            let plan =
                PositionExecutionPlan::target_size(&pos(current), &Symbol::new("HYPE"), target, 2)
                    .unwrap();
            assert_eq!(plan.phases.len(), 1);
            assert_eq!(plan.phases[0].side, side);
            assert_eq!(plan.phases[0].size, size);
            assert_eq!(plan.phases[0].reduce_only, reduce_only);
        }
    }

    #[test]
    fn zero_crossing_is_strictly_close_then_open() {
        let plan = PositionExecutionPlan::target_size(
            &pos(dec!(2.5)),
            &Symbol::new("HYPE"),
            dec!(-1.25),
            2,
        )
        .unwrap();
        assert!(plan.crosses_zero());
        assert_eq!(plan.phases[0].side, Side::Short);
        assert_eq!(plan.phases[0].size, dec!(2.5));
        assert!(plan.phases[0].reduce_only);
        assert_eq!(plan.phases[1].side, Side::Short);
        assert_eq!(plan.phases[1].size, dec!(1.25));
        assert!(!plan.phases[1].reduce_only);
    }

    #[test]
    fn target_usd_is_fixed_and_quantized_toward_zero() {
        let long = PositionExecutionPlan::target_usd(
            &pos(Decimal::ZERO),
            &Symbol::new("HYPE"),
            dec!(100),
            dec!(30),
            2,
        )
        .unwrap();
        let short = PositionExecutionPlan::target_usd(
            &pos(Decimal::ZERO),
            &Symbol::new("HYPE"),
            dec!(-100),
            dec!(30),
            2,
        )
        .unwrap();
        assert_eq!(long.target_szi, dec!(3.33));
        assert_eq!(short.target_szi, dec!(-3.33));
        assert!(long.target_szi * dec!(30) <= dec!(100));
        assert!((short.target_szi * dec!(30)).abs() <= dec!(100));
    }

    #[test]
    fn flat_target_is_noop_and_malformed_snapshot_fails_closed() {
        let plan =
            PositionExecutionPlan::flatten(&pos(Decimal::ZERO), &Symbol::new("HYPE"), 2).unwrap();
        assert!(plan.is_noop());

        let err = PositionExecutionPlan::target_size(
            &pos(dec!(1.001)),
            &Symbol::new("HYPE"),
            Decimal::ZERO,
            2,
        )
        .unwrap_err();
        assert!(matches!(err, PositionPlanError::CurrentPrecision { .. }));
    }

    fn confirmation() -> FlattenConfirmation {
        FlattenConfirmation {
            schema_version: FlattenConfirmation::SCHEMA_VERSION,
            network: "mainnet".into(),
            master: Address::new("0x0000000000000000000000000000000000000001"),
            symbol: Symbol::new("HYPE"),
            initial_szi: dec!(2),
            close_side: Side::Short,
            max_close_size: dec!(2),
            max_notional_usd: dec!(200),
            child_algo: "market".into(),
            execution_deadline_unix_ms: 1_800_000_000_000,
        }
    }

    #[test]
    fn durable_fills_reconstruct_signed_position_without_auto_reversal() {
        let after_sell = apply_signed_fill(dec!(2), Side::Short, dec!(1.25)).unwrap();
        assert_eq!(after_sell, dec!(0.75));
        let after_buy = apply_signed_fill(after_sell, Side::Long, dec!(0.25)).unwrap();
        assert_eq!(after_buy, dec!(1));
        assert!(is_between_frozen_endpoints(dec!(0.75), dec!(2), dec!(-1)));
        assert!(!is_between_frozen_endpoints(dec!(-1.01), dec!(2), dec!(-1)));
        assert_eq!(
            apply_signed_fill(Decimal::ZERO, Side::Long, dec!(-0.1)),
            Err(PositionPlanError::InvalidFill(dec!(-0.1)))
        );
    }

    #[test]
    fn flatten_confirmation_is_deterministic_and_plan_bound() {
        let original = confirmation();
        assert_eq!(original.token().unwrap(), confirmation().token().unwrap());
        let mut changed = confirmation();
        changed.max_close_size = dec!(1.99);
        assert_ne!(original.token().unwrap(), changed.token().unwrap());
        assert!(original.token().unwrap().starts_with("flatten-v1-"));
    }
}
