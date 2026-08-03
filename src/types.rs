//! Minimal domain types, inlined from `executor-core`
//! (`types.rs` / `cloid.rs` / `symbol.rs`) with only the parts this binary
//! needs. Semantics are unchanged from the source crate.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

/// Ethereum-style address. Wire format: lowercase 0x-prefixed 40 hex chars.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Address(pub String);

impl Address {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Hyperliquid coin symbol (e.g. `BTC`, `HYPE`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Symbol(pub String);

impl Symbol {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<&str> for Symbol {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

/// Trade direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Long,
    Short,
}

impl Side {
    pub fn as_signed_unit(self) -> Decimal {
        match self {
            Side::Long => Decimal::ONE,
            Side::Short => Decimal::NEGATIVE_ONE,
        }
    }
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Side::Long => write!(f, "long"),
            Side::Short => write!(f, "short"),
        }
    }
}

/// Time-in-force flag (matches Hyperliquid wire values).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tif {
    /// Add liquidity only (post-only). Cancelled if it would match immediately.
    Alo,
    /// Immediate-or-cancel. Unfilled remainder is cancelled.
    Ioc,
    /// Good til cancelled.
    Gtc,
}

/// Hyperliquid-side order id (returned by the exchange after a successful place).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OrderId(pub u64);

impl fmt::Display for OrderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Hyperliquid Client Order ID.
///
/// Wire format: 32 lowercase hex chars prefixed with `0x` (per HL exchange API).
/// Internally a uuid v7 so ids are time-sortable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub struct Cloid(Uuid);

impl Cloid {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    pub fn from_uuid(u: Uuid) -> Self {
        Self(u)
    }

    /// Hex-encoded with `0x` prefix (the HL wire format).
    pub fn to_hex_string(&self) -> String {
        format!("0x{}", self.0.simple())
    }

    pub fn uuid(&self) -> Uuid {
        self.0
    }
}

impl Default for Cloid {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for Cloid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex_string())
    }
}

impl From<Cloid> for String {
    fn from(c: Cloid) -> Self {
        c.to_hex_string()
    }
}

impl TryFrom<String> for Cloid {
    type Error = uuid::Error;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let trimmed = value.trim_start_matches("0x");
        Uuid::parse_str(trimmed).map(Self)
    }
}

/// One order to be sent to HL.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderIntent {
    pub cloid: Cloid,
    pub symbol: Symbol,
    pub side: Side,
    pub px: Decimal,
    pub sz: Decimal,
    pub tif: Tif,
    pub reduce_only: bool,
}

/// One cancel request (cloid-based only — this binary never cancels by oid).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelIntent {
    pub symbol: Symbol,
    pub by_cloid: Cloid,
}

/// One price level of the L2 book.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookLevel {
    pub px: Decimal,
    pub sz: Decimal,
    pub n: u32,
}

/// Top-of-book snapshot. `bids` descending, `asks` ascending.
#[derive(Debug, Clone, Default)]
pub struct OrderBook {
    /// The `coin` field HL echoed back in the response — carried through so a
    /// caller can check it against the symbol it requested (Issue #6).
    pub coin: String,
    pub bids: Vec<BookLevel>,
    pub asks: Vec<BookLevel>,
    /// HL server timestamp of the snapshot (ms epoch).
    pub time_ms: i64,
}

impl OrderBook {
    pub fn best_bid(&self) -> Option<Decimal> {
        self.bids.first().map(|l| l.px)
    }

    pub fn best_ask(&self) -> Option<Decimal> {
        self.asks.first().map(|l| l.px)
    }

    /// `(best_bid + best_ask) / 2`. `None` if either side is empty.
    pub fn mid(&self) -> Option<Decimal> {
        match (self.best_bid(), self.best_ask()) {
            (Some(b), Some(a)) => Some((b + a) / Decimal::TWO),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn side_signed_unit() {
        assert_eq!(Side::Long.as_signed_unit(), Decimal::ONE);
        assert_eq!(Side::Short.as_signed_unit(), Decimal::NEGATIVE_ONE);
    }

    #[test]
    fn tif_serializes_pascalcase() {
        assert_eq!(serde_json::to_string(&Tif::Alo).unwrap(), "\"Alo\"");
        assert_eq!(serde_json::to_string(&Tif::Ioc).unwrap(), "\"Ioc\"");
        assert_eq!(serde_json::to_string(&Tif::Gtc).unwrap(), "\"Gtc\"");
    }

    #[test]
    fn cloid_hex_format_is_0x_prefixed_32_chars() {
        let c = Cloid::new();
        let s = c.to_hex_string();
        assert!(s.starts_with("0x"));
        assert_eq!(s.len(), 34); // "0x" + 32 hex chars
    }

    #[test]
    fn cloid_is_unique() {
        assert_ne!(Cloid::new(), Cloid::new());
    }

    #[test]
    fn cloid_serde_roundtrip() {
        let c = Cloid::new();
        let json = serde_json::to_string(&c).unwrap();
        let back: Cloid = serde_json::from_str(&json).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn book_mid_is_average_of_touch() {
        let book = OrderBook {
            coin: "HYPE".to_string(),
            bids: vec![BookLevel {
                px: dec!(99),
                sz: dec!(1),
                n: 1,
            }],
            asks: vec![BookLevel {
                px: dec!(101),
                sz: dec!(1),
                n: 1,
            }],
            time_ms: 0,
        };
        assert_eq!(book.mid(), Some(dec!(100)));
        assert_eq!(book.best_bid(), Some(dec!(99)));
        assert_eq!(book.best_ask(), Some(dec!(101)));
    }

    #[test]
    fn book_mid_none_when_one_side_empty() {
        let book = OrderBook {
            coin: "HYPE".to_string(),
            bids: vec![BookLevel {
                px: dec!(99),
                sz: dec!(1),
                n: 1,
            }],
            asks: vec![],
            time_ms: 0,
        };
        assert_eq!(book.mid(), None);
    }
}
