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

/// Issue #1 Finding 2: `is_alo_reject()` (`src/twap.rs`) classifies an ALO
/// (post-only) place-time rejection via an EXACT-match allow-list
/// (`ALO_REJECT_EXACT`) rather than a loose substring match, to stay
/// fail-closed against unrelated fatal rejections. That allow-list is
/// pinned against wording this repo has observed (`badAloPxRejected` /
/// "Post only order would have immediately matched") but HL's wire wording
/// can change over time, exactly like `ORDER_STATUS_VOCABULARY` can drift —
/// see `live_order_status_shape_matches_what_this_client_parses` above for
/// the sibling conformance check on that table.
///
/// This is a MANUAL hook (`#[ignore]`d, same pattern as the two tests
/// above) for a human to run when actually verifying ALO-reject wording
/// against the live API: place a deliberately-crossing ALO limit order on
/// testnet (a long ALO priced at or above the current ask, or a short ALO
/// at or below the current bid) and confirm the resulting `HlError::Exchange`
/// message/code is still recognised by `is_alo_reject()`.
///
/// It does NOT attempt this automatically — doing so needs a funded testnet
/// account, a live signer, and a real crossing price at run time (none of
/// which this smoke-test binary has access to, and none of which should be
/// a silent prerequisite for `cargo test`). It exists so this drift-check
/// is discoverable and runnable, not to assert anything by itself.
#[tokio::test]
#[ignore = "manual only: requires a funded testnet account + signer + a live crossing price; \
            not runnable unattended. See this test's doc comment for the procedure."]
async fn live_alo_rejection_wording_matches_what_is_alo_reject_recognises() {
    // Deliberately not implemented as an automated assertion — see the
    // doc comment above for why. This body documents the manual procedure
    // as executable-looking scaffolding so the next person who runs
    // `cargo test -- --ignored --list` sees exactly what to do:
    //
    // 1. Fund a testnet account and export HL_AGENT_PK / HL_AGENT_ADDRESS.
    // 2. Run `hype-twap --child-algo passive --read-only false` with a
    //    limit price deliberately crossing the current touch (e.g. a long
    //    ALO priced above the current best ask).
    // 3. Capture the resulting HlError::Exchange { code, message } from the
    //    logs.
    // 4. Confirm `hype_trigger_twap::twap`'s `is_alo_reject` (currently
    //    private; temporarily made `pub(crate)` or exercised via a local
    //    patch for this manual check) still returns `true` for that exact
    //    code/message pair. If it does not, HL's wording has drifted and
    //    `ALO_REJECT_EXACT` in src/twap.rs needs a new entry.
    panic!(
        "this is a manual procedure, not an automated check — see the doc comment; \
         do not run this in CI"
    );
}
