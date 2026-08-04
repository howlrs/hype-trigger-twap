//! Trigger-gated TWAP execution for Hyperliquid perps.
//!
//! The binary lives in `main.rs`; this library target exists so integration
//! tests (notably the python-sdk signing cross-check) can exercise the same
//! modules the binary uses.

pub mod api;
pub mod client;
pub mod eip712;
pub mod errors;
pub mod format;
pub mod risk;
pub mod signer;
pub mod trigger;
pub mod twap;
pub mod types;
