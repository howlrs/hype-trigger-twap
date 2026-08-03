//! Errors for HL communication and execution.
//!
//! Inlined from `executor-hl/src/errors.rs`, trimmed to the variants this
//! binary can actually produce (no WS, no rate limiter, no nonce window).

use thiserror::Error;

#[derive(Debug, Error)]
pub enum HlError {
    /// Transport-level failure: reqwest error, HTTP 5xx, HTTP 429.
    /// This is the ONLY class that is retried (§5).
    #[error("network error: {0}")]
    Network(String),

    #[error("signature error: {0}")]
    Signature(String),

    #[error("invalid response: {0}")]
    InvalidResponse(String),

    #[error("invalid config: {0}")]
    InvalidConfig(String),

    #[error("invalid action format: {0}")]
    ActionFormat(String),

    #[error("unknown symbol (not in HL universe): {0}")]
    UnknownSymbol(crate::types::Symbol),

    /// Exchange rejected the action. NEVER retried — hard stop (§5).
    #[error("hl exchange error: {code:?} {message}")]
    Exchange {
        code: Option<String>,
        message: String,
    },

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Human-readable classification of an exchange rejection message (§5).
///
/// Substring matching is case-insensitive. Classification is best-effort:
/// an unclassified rejection still hard-stops, it just reports the raw
/// message. HL wording changes over time — see README "known limitations".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectionKind {
    /// "Insufficient margin ..." — account cannot support the order.
    InsufficientMargin,
    /// "MinTradeNtl" / "... minimum ..." — order notional below HL's floor.
    MinNotional,
    /// Anything else. Still fatal, just not categorized.
    Unclassified,
}

impl RejectionKind {
    pub fn classify(message: &str) -> Self {
        let m = message.to_ascii_lowercase();
        if m.contains("insufficient margin") {
            Self::InsufficientMargin
        } else if m.contains("mintradentl") || m.contains("minimum") {
            Self::MinNotional
        } else {
            Self::Unclassified
        }
    }

    /// Operator-facing explanation appended to the abort message.
    pub fn advice(self) -> &'static str {
        match self {
            Self::InsufficientMargin => {
                "insufficient margin: fund the account or reduce --usd/--size"
            }
            Self::MinNotional => {
                "order below HL minimum notional: increase --usd/--size or reduce --slices"
            }
            Self::Unclassified => "exchange rejected the order (not retried)",
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn classify_insufficient_margin_case_insensitive() {
        assert_eq!(
            RejectionKind::classify("Insufficient margin to place order"),
            RejectionKind::InsufficientMargin
        );
        assert_eq!(
            RejectionKind::classify("INSUFFICIENT MARGIN"),
            RejectionKind::InsufficientMargin
        );
    }

    #[test]
    fn classify_min_notional_variants() {
        assert_eq!(
            RejectionKind::classify("MinTradeNtl"),
            RejectionKind::MinNotional
        );
        assert_eq!(
            RejectionKind::classify("Order value below minimum of $10"),
            RejectionKind::MinNotional
        );
    }

    #[test]
    fn classify_unknown_falls_back_to_unclassified() {
        assert_eq!(
            RejectionKind::classify("Post only order would have immediately matched"),
            RejectionKind::Unclassified
        );
    }

    #[test]
    fn every_kind_has_advice() {
        for k in [
            RejectionKind::InsufficientMargin,
            RejectionKind::MinNotional,
            RejectionKind::Unclassified,
        ] {
            assert!(!k.advice().is_empty());
        }
    }
}
