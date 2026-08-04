//! Live API conformance smoke test (Issue #7).
//!
//! `ORDER_STATUS_VOCABULARY` in `src/client.rs` is a hand-maintained table of
//! every official Hyperliquid `orderStatus` status string. It can silently
//! drift out of date if HL adds a new status: the fail-closed default means a
//! drifted vocabulary does not crash anything, it just quietly stops treating
//! new terminal statuses as terminal (hard-stopping runs that should have
//! completed normally). This test is the guard against that drift.
//!
//! It hits the REAL Hyperliquid testnet `/info` endpoint, so it is `#[ignore]`d
//! by default (no network access in normal `cargo test` / CI runs, matching
//! every other test in this suite). Run it manually:
//!
//! ```bash
//! cargo test --test status_vocabulary_conformance -- --ignored --nocapture
//! ```
//!
//! What it checks:
//! - `/info orderStatus` for a bogus oid returns the documented
//!   `{"status":"unknownOid"}` shape — proves the wire shape this client
//!   parses has not changed.
//! - `/info meta` is reachable and returns a non-empty universe — proves the
//!   endpoint is up and the base response shape is intact.
//!
//! It deliberately does NOT try to fetch a live order's real status (that
//! would need a funded account and a resting order, which this smoke test
//! must not depend on). Vocabulary drift itself — HL adding a new status
//! string — cannot be detected by an automated request; see
//! docs/DEVELOPMENT.md "orderStatus ステータス表の更新手順" for the manual
//! procedure this test complements.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use hype_trigger_twap::client::{HlClient, HlConfig, Network};
use hype_trigger_twap::types::{Address, OrderId};

#[tokio::test]
#[ignore = "hits the real Hyperliquid testnet API; run manually with --ignored"]
async fn live_order_status_shape_matches_what_this_client_parses() {
    let client = HlClient::new(HlConfig::new(Network::Testnet), None)
        .expect("client construction must not require a signer for /info reads");

    // A bogus oid on a well-formed but almost certainly untouched address.
    // HL is expected to answer unknownOid — if the wire shape has changed,
    // `fetch_order_status` (and therefore `parse_order_status`) will fail to
    // parse it and this test will fail loudly instead of silently drifting.
    //
    // MUST be a full 40-hex-char (20-byte) address — HL's `/info` rejects
    // the request body itself (HTTP 422, before it ever reaches orderStatus
    // logic) for anything shorter, which this test caught once already.
    let bogus_user = Address::new("0x000000000000000000000000000000000000dead");
    let result = client.fetch_order_status(&bogus_user, OrderId(1)).await;

    match result {
        Ok(None) => {
            // Expected: HL confirms it does not know this oid.
        }
        Ok(Some(fill)) => {
            // Extremely unlikely (would mean oid 1 exists under this
            // address on testnet), but not itself a shape failure — assert
            // the vocabulary still recognises whatever status came back.
            assert!(
                fill.is_known_status(),
                "live orderStatus returned status '{}' which is NOT in \
                 ORDER_STATUS_VOCABULARY — HL vocabulary has drifted, \
                 update src/client.rs (see docs/DEVELOPMENT.md)",
                fill.status
            );
        }
        Err(e) => panic!(
            "orderStatus request/parse failed against the live API: {e}. \
             Either the network is down or HL's response shape has changed \
             in a way parse_order_status no longer understands."
        ),
    }
}

#[tokio::test]
#[ignore = "hits the real Hyperliquid testnet API; run manually with --ignored"]
async fn live_meta_endpoint_is_reachable_and_returns_a_nonempty_universe() {
    let client = HlClient::new(HlConfig::new(Network::Testnet), None)
        .expect("client construction must not require a signer for /info reads");
    let meta = client
        .fetch_meta()
        .await
        .expect("meta request/parse failed against the live API");
    assert!(
        !meta.universe.is_empty(),
        "live meta returned an empty universe — endpoint or response shape issue"
    );
}
