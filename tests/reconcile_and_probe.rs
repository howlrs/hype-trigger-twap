//! Mockito coverage for the review fixes that live at the HTTP boundary:
//! W1 (`/exchange` idempotency), F1 (`userRole` probe), F2 (pre-flight
//! freshness) and F4 (the trigger wait loop end-to-end).
//!
//! No real network and no real key beyond the well-known test PK.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use hype_trigger_twap::client::{HlClient, HlConfig, Network, PlaceOutcome, Role};
use hype_trigger_twap::errors::HlError;
use hype_trigger_twap::signer::Eip712AgentSigner;
use hype_trigger_twap::trigger::{
    wait_for_trigger, TriggerConfig, TriggerOutcome, TriggerReason, TriggerWhen,
};
use hype_trigger_twap::twap::fetch_fresh_book;
use hype_trigger_twap::types::{Address, Cloid, OrderId, OrderIntent, Side, Symbol, Tif};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use secrecy::SecretString;

const TEST_PK: &str = "0x0123456789012345678901234567890123456789012345678901234567890123";
const AGENT: &str = "0x00000000000000000000000000000000000000bb";
const MASTER: &str = "0x00000000000000000000000000000000000000aa";

fn make_client(server: &mockito::ServerGuard) -> HlClient {
    let signer = Eip712AgentSigner::from_secret(SecretString::new(TEST_PK.into()), false).unwrap();
    let config = HlConfig::new(Network::Testnet).with_overrides(
        Some(format!("{}/info", server.url())),
        Some(format!("{}/exchange", server.url())),
    );
    HlClient::new(config, Some(Box::new(signer))).unwrap()
}

fn read_only_client(server: &mockito::ServerGuard) -> HlClient {
    let config = HlConfig::new(Network::Testnet).with_overrides(
        Some(format!("{}/info", server.url())),
        Some(format!("{}/exchange", server.url())),
    );
    HlClient::new(config, None).unwrap()
}

fn intent() -> OrderIntent {
    OrderIntent {
        cloid: Cloid::new(),
        symbol: Symbol::new("HYPE"),
        side: Side::Long,
        px: dec!(50),
        sz: dec!(1),
        tif: Tif::Ioc,
        reduce_only: false,
    }
}

/// An l2Book body stamped `now`, so the freshness gate sees it as current.
fn fresh_book_body(bid: &str, ask: &str) -> String {
    book_body_at(bid, ask, chrono::Utc::now().timestamp_millis())
}

fn book_body_at(bid: &str, ask: &str, time_ms: i64) -> String {
    format!(
        r#"{{"coin":"HYPE","time":{time_ms},"levels":[
            [{{"px":"{bid}","sz":"100","n":1}}],
            [{{"px":"{ask}","sz":"100","n":1}}]]}}"#
    )
}

// === F1: userRole probe ===

#[tokio::test]
async fn f1_user_role_agent_resolves_the_master_address() {
    let mut server = mockito::Server::new_async().await;
    let _m = server
        .mock("POST", "/info")
        .with_status(200)
        .with_body(format!(
            r#"{{"role":"agent","data":{{"user":"{MASTER}"}}}}"#
        ))
        .create_async()
        .await;

    let client = read_only_client(&server);
    let role = client.fetch_user_role(&Address::new(AGENT)).await.unwrap();
    assert_eq!(
        role,
        Role::Agent {
            master: Address::new(MASTER)
        }
    );
}

#[tokio::test]
async fn f1_user_role_master_is_normalised_to_lowercase() {
    // HL may answer with a checksummed (mixed-case) address, but every
    // comparison downstream is a lowercase string compare.
    let mut server = mockito::Server::new_async().await;
    let _m = server
        .mock("POST", "/info")
        .with_status(200)
        .with_body(
            r#"{"role":"agent","data":{"user":"0x00000000000000000000000000000000000000AA"}}"#,
        )
        .create_async()
        .await;

    let client = read_only_client(&server);
    match client.fetch_user_role(&Address::new(AGENT)).await.unwrap() {
        Role::Agent { master } => assert_eq!(master.as_str(), MASTER),
        other => panic!("expected Agent, got {other:?}"),
    }
}

#[tokio::test]
async fn f1_user_role_plain_user_is_not_an_agent() {
    // A key that is NOT registered as an API wallet. HL refuses its signed
    // actions, so startup must fail here rather than at the first order.
    let mut server = mockito::Server::new_async().await;
    let _m = server
        .mock("POST", "/info")
        .with_status(200)
        .with_body(r#"{"role":"user"}"#)
        .create_async()
        .await;

    let client = read_only_client(&server);
    let role = client.fetch_user_role(&Address::new(AGENT)).await.unwrap();
    assert_eq!(role, Role::User);
    assert!(!matches!(role, Role::Agent { .. }));
    assert!(role.label().contains("user"));
}

#[tokio::test]
async fn f1_user_role_unknown_variant_does_not_crash_the_client() {
    // Forward compatibility: a role HL adds later must degrade to Unknown, not
    // a parse error that takes the process down.
    let mut server = mockito::Server::new_async().await;
    let _m = server
        .mock("POST", "/info")
        .with_status(200)
        .with_body(r#"{"role":"marketMaker","data":{"tier":3}}"#)
        .create_async()
        .await;

    let client = read_only_client(&server);
    let role = client.fetch_user_role(&Address::new(AGENT)).await.unwrap();
    assert!(matches!(role, Role::Unknown(_)), "got {role:?}");
    assert!(!matches!(role, Role::Agent { .. }), "must not trade");
}

#[tokio::test]
async fn f1_order_status_sends_the_master_as_user() {
    // The load-bearing assertion of F1: the request body's `user` must be the
    // MASTER. With the agent address HL answers `unknownOid` for orders that
    // genuinely exist, and the recovery path would read that as "no fill".
    let mut server = mockito::Server::new_async().await;
    let m = server
        .mock("POST", "/info")
        .match_body(mockito::Matcher::PartialJson(serde_json::json!({
            "type": "orderStatus",
            "user": MASTER,
        })))
        .with_status(200)
        .with_body(
            r#"{"status":"order","order":{"order":{
                "coin":"HYPE","side":"B","limitPx":"50","sz":"0","origSz":"1",
                "oid":999,"timestamp":1,"avgPx":"49.9"},
                "status":"filled","statusTimestamp":2}}"#,
        )
        .expect(1)
        .create_async()
        .await;

    let client = make_client(&server);
    let got = client
        .fetch_order_status(&Address::new(MASTER), OrderId(999))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.filled_sz, dec!(1));
    assert!(got.is_terminal());
    m.assert_async().await;
}

#[tokio::test]
async fn f1_order_status_by_cloid_sends_the_hex_cloid() {
    let cloid = Cloid::new();
    let mut server = mockito::Server::new_async().await;
    let m = server
        .mock("POST", "/info")
        .match_body(mockito::Matcher::PartialJson(serde_json::json!({
            "type": "orderStatus",
            "user": MASTER,
            "oid": cloid.to_hex_string(),
        })))
        .with_status(200)
        .with_body(r#"{"status":"unknownOid"}"#)
        .expect(1)
        .create_async()
        .await;

    let client = make_client(&server);
    assert!(client
        .fetch_order_status_by_cloid(&Address::new(MASTER), cloid)
        .await
        .unwrap()
        .is_none());
    m.assert_async().await;
}

// === W1: /exchange is sent exactly once ===

#[tokio::test]
async fn w1_exchange_transport_failure_is_never_blind_retried() {
    // The core of W1. A 503 from /exchange used to trigger up to 3 resends of
    // the SAME signed body — which HL can only reject as a stale nonce, while
    // the original may have filled. There must be exactly one request.
    let mut server = mockito::Server::new_async().await;
    let m = server
        .mock("POST", "/exchange")
        .with_status(503)
        .with_body("gateway timeout")
        .expect(1)
        .create_async()
        .await;

    let client = make_client(&server);
    let err = client
        .place_order(&intent(), 2, 1_700_000_000_000)
        .await
        .unwrap_err();
    assert!(matches!(err, HlError::Network(_)), "got {err:?}");
    m.assert_async().await;
}

#[tokio::test]
async fn w1_cancel_transport_failure_is_also_sent_once_only() {
    let mut server = mockito::Server::new_async().await;
    let m = server
        .mock("POST", "/exchange")
        .with_status(503)
        .with_body("down")
        .expect(1)
        .create_async()
        .await;

    let client = make_client(&server);
    let cancel = hype_trigger_twap::types::CancelIntent {
        symbol: Symbol::new("HYPE"),
        by_cloid: Cloid::new(),
    };
    assert!(client.cancel_by_cloid(&cancel, 2).await.is_err());
    m.assert_async().await;
}

#[tokio::test]
async fn w1_info_reads_keep_their_retry_because_they_are_idempotent() {
    // The send-once rule applies to /exchange only. /info is a pure read, so
    // dropping its retry would make the tool fragile for no safety gain.
    let mut server = mockito::Server::new_async().await;
    let fail = server
        .mock("POST", "/info")
        .with_status(503)
        .expect(1)
        .create_async()
        .await;
    let ok = server
        .mock("POST", "/info")
        .with_status(200)
        .with_body(
            r#"{"role":"agent","data":{"user":"0x00000000000000000000000000000000000000aa"}}"#,
        )
        .expect(1)
        .create_async()
        .await;

    let client = read_only_client(&server);
    let role = tokio::time::timeout(
        Duration::from_secs(20),
        client.fetch_user_role(&Address::new(AGENT)),
    )
    .await
    .expect("retry timed out")
    .unwrap();
    assert!(matches!(role, Role::Agent { .. }));
    fail.assert_async().await;
    ok.assert_async().await;
}

#[tokio::test]
async fn w1_place_reports_the_nonce_so_a_resend_can_be_proven_fresh() {
    // The reconcile path re-signs rather than replaying the old body. Exposing
    // the nonce is what lets both the caller and a test verify that.
    let mut server = mockito::Server::new_async().await;
    let _m = server
        .mock("POST", "/exchange")
        .with_status(200)
        .with_body(
            r#"{"status":"ok","response":{"type":"order","data":{"statuses":[
                {"filled":{"oid":1,"totalSz":"1","avgPx":"50"}}]}}}"#,
        )
        .expect(2)
        .create_async()
        .await;

    let client = make_client(&server);
    let (n1, o1) = client
        .place_order_once(&intent(), 2, 1_700_000_000_000)
        .await
        .unwrap();
    let (n2, _) = client
        .place_order_once(&intent(), 2, 1_700_000_000_000)
        .await
        .unwrap();
    assert!(matches!(o1, PlaceOutcome::Filled { .. }));
    assert!(
        n2 > n1,
        "each send must consume a fresh nonce ({n1} -> {n2})"
    );
}

// === F2: the pre-flight book goes through the freshness gate ===

#[tokio::test]
async fn f2_stale_pre_flight_book_is_retried_then_hard_stops() {
    // The pre-flight mid fixes the coin quantity for the WHOLE run, so it must
    // not be the one price allowed to skip the staleness check. A book an hour
    // old is retried and then refused.
    let stale = chrono::Utc::now().timestamp_millis() - 3_600_000;
    let mut server = mockito::Server::new_async().await;
    // 1 initial attempt + 3 retries.
    let m = server
        .mock("POST", "/info")
        .with_status(200)
        .with_body(book_body_at("49.9", "50.1", stale))
        .expect(4)
        .create_async()
        .await;

    let client = read_only_client(&server);
    let err = tokio::time::timeout(
        Duration::from_secs(30),
        fetch_fresh_book(&client, &Symbol::new("HYPE"), 3000, None),
    )
    .await
    .expect("stale retry timed out")
    .unwrap_err();

    match err {
        HlError::InvalidResponse(msg) => {
            assert!(msg.contains("still invalid"), "{msg}");
            assert!(msg.contains("stale"), "{msg}");
        }
        other => panic!("expected InvalidResponse, got {other:?}"),
    }
    m.assert_async().await;
}

#[tokio::test]
async fn f2_fresh_pre_flight_book_passes_the_gate_on_the_first_try() {
    let mut server = mockito::Server::new_async().await;
    let m = server
        .mock("POST", "/info")
        .with_status(200)
        .with_body(fresh_book_body("49.9", "50.1"))
        .expect(1)
        .create_async()
        .await;

    let client = read_only_client(&server);
    let book = fetch_fresh_book(&client, &Symbol::new("HYPE"), 3000, None)
        .await
        .unwrap();
    assert_eq!(book.mid, dec!(50.0));
    m.assert_async().await;
}

#[tokio::test]
async fn f2_stale_book_is_accepted_when_the_gate_is_disabled() {
    // `--max-book-age-ms 0` disables the check, and that must hold on the
    // pre-flight path exactly as it does inside the loop.
    let stale = chrono::Utc::now().timestamp_millis() - 3_600_000;
    let mut server = mockito::Server::new_async().await;
    let m = server
        .mock("POST", "/info")
        .with_status(200)
        .with_body(book_body_at("49.9", "50.1", stale))
        .expect(1)
        .create_async()
        .await;

    let client = read_only_client(&server);
    let book = fetch_fresh_book(&client, &Symbol::new("HYPE"), 0, None)
        .await
        .unwrap();
    assert_eq!(book.mid, dec!(50.0));
    m.assert_async().await;
}

// === F4: wait_for_trigger, end to end over HTTP ===

fn trigger_cfg(
    price: Option<(TriggerWhen, Decimal)>,
    start_after: Option<Duration>,
) -> TriggerConfig {
    TriggerConfig {
        price,
        start_after,
        // Short so the test finishes in real time without being flaky.
        poll_interval: Duration::from_millis(20),
        max_book_age_ms: 3000,
        // Real (unpaused) clock in this file — keep short so a persistent
        // failure test finishes quickly instead of waiting out 30m of real
        // time.
        wait_network_grace: Duration::from_millis(200),
        expire_after: None,
    }
}

#[tokio::test]
async fn f4_above_trigger_fires_once_the_polled_mid_crosses_up() {
    let mut server = mockito::Server::new_async().await;
    // Two polls below the threshold, then one at/over it.
    let _low = server
        .mock("POST", "/info")
        .with_status(200)
        .with_body(fresh_book_body("38.00", "38.02"))
        .expect(2)
        .create_async()
        .await;
    let _high = server
        .mock("POST", "/info")
        .with_status(200)
        .with_body(fresh_book_body("40.10", "40.30"))
        .create_async()
        .await;

    let client = read_only_client(&server);
    let reason = tokio::time::timeout(
        Duration::from_secs(10),
        wait_for_trigger(
            &client,
            &Symbol::new("HYPE"),
            &trigger_cfg(Some((TriggerWhen::Above, dec!(40))), None),
        ),
    )
    .await
    .expect("trigger wait timed out")
    .unwrap();

    match reason {
        TriggerOutcome::Fired(TriggerReason::Price {
            when,
            threshold,
            mid,
            snapshot,
        }) => {
            assert_eq!(when, TriggerWhen::Above);
            assert_eq!(threshold, dec!(40));
            assert_eq!(mid, dec!(40.20));
            assert!(mid >= threshold);
            // The trigger-firing snapshot IS the mid that satisfied the
            // condition — this is the single "trigger-time mid" a caller
            // (main.rs's --usd sizing) reuses without re-fetching (Issue #6).
            assert_eq!(snapshot.mid, mid);
        }
        other => panic!("expected a price trigger, got {other:?}"),
    }
}

#[tokio::test]
async fn f4_below_trigger_fires_once_the_polled_mid_crosses_down() {
    let mut server = mockito::Server::new_async().await;
    let _high = server
        .mock("POST", "/info")
        .with_status(200)
        .with_body(fresh_book_body("41.00", "41.02"))
        .expect(2)
        .create_async()
        .await;
    let _low = server
        .mock("POST", "/info")
        .with_status(200)
        .with_body(fresh_book_body("29.90", "30.10"))
        .create_async()
        .await;

    let client = read_only_client(&server);
    let reason = tokio::time::timeout(
        Duration::from_secs(10),
        wait_for_trigger(
            &client,
            &Symbol::new("HYPE"),
            &trigger_cfg(Some((TriggerWhen::Below, dec!(30))), None),
        ),
    )
    .await
    .expect("trigger wait timed out")
    .unwrap();

    match reason {
        TriggerOutcome::Fired(TriggerReason::Price { when, mid, .. }) => {
            assert_eq!(when, TriggerWhen::Below);
            assert_eq!(mid, dec!(30.00));
        }
        other => panic!("expected a price trigger, got {other:?}"),
    }
}

#[tokio::test]
async fn f4_price_wins_when_it_crosses_before_the_time_deadline() {
    // Both conditions armed. The price is already over the threshold on the
    // first poll, and the time deadline is far away, so the reported reason
    // must be Price — the log has to be unambiguous about which one won.
    let mut server = mockito::Server::new_async().await;
    let _m = server
        .mock("POST", "/info")
        .with_status(200)
        .with_body(fresh_book_body("40.10", "40.30"))
        .create_async()
        .await;

    let client = read_only_client(&server);
    let reason = tokio::time::timeout(
        Duration::from_secs(10),
        wait_for_trigger(
            &client,
            &Symbol::new("HYPE"),
            &trigger_cfg(
                Some((TriggerWhen::Above, dec!(40))),
                Some(Duration::from_secs(3600)),
            ),
        ),
    )
    .await
    .expect("trigger wait timed out")
    .unwrap();

    assert!(
        matches!(reason, TriggerOutcome::Fired(TriggerReason::Price { .. })),
        "price must win, got {reason:?}"
    );
}

#[tokio::test]
async fn f4_time_wins_when_the_price_never_crosses() {
    // Same pairing, opposite outcome: the price never reaches the threshold, so
    // the elapsed deadline is what fires.
    let mut server = mockito::Server::new_async().await;
    let _m = server
        .mock("POST", "/info")
        .with_status(200)
        .with_body(fresh_book_body("10.00", "10.02"))
        .create_async()
        .await;

    let client = read_only_client(&server);
    let after = Duration::from_millis(200);
    let reason = tokio::time::timeout(
        Duration::from_secs(10),
        wait_for_trigger(
            &client,
            &Symbol::new("HYPE"),
            &trigger_cfg(Some((TriggerWhen::Above, dec!(999))), Some(after)),
        ),
    )
    .await
    .expect("trigger wait timed out")
    .unwrap();

    assert_eq!(
        reason,
        TriggerOutcome::Fired(TriggerReason::Elapsed { after })
    );
}

#[tokio::test]
async fn f4_persistent_poll_failures_hard_stop_the_wait_once_grace_is_exceeded() {
    // A persistently blind trigger must not sit silent forever (Issue #9:
    // time-budgeted, not count-based). `trigger_cfg`'s grace is 200ms with a
    // 20ms poll interval, so several 400s in a row exceed it.
    let mut server = mockito::Server::new_async().await;
    // Each poll is a 400 (fatal, not retried inside the client), so one
    // response == one consecutive failure. `.expect(N)` here is a MINIMUM,
    // not exact — the grace is time-based, so the exact poll count needed to
    // exceed it depends on real scheduling jitter around the 20ms interval.
    let m = server
        .mock("POST", "/info")
        .with_status(400)
        .with_body("bad request")
        .expect_at_least(5)
        .create_async()
        .await;

    let client = read_only_client(&server);
    let err = tokio::time::timeout(
        Duration::from_secs(20),
        wait_for_trigger(
            &client,
            &Symbol::new("HYPE"),
            &trigger_cfg(Some((TriggerWhen::Above, dec!(40))), None),
        ),
    )
    .await
    .expect("poll-failure abort timed out")
    .unwrap_err();

    match err {
        HlError::Network(msg) => {
            assert!(msg.contains("blind for"), "{msg}");
            assert!(msg.contains("grace"), "{msg}");
            assert!(msg.contains("last error"), "{msg}");
            assert!(msg.contains("aborting"), "{msg}");
        }
        other => panic!("expected Network, got {other:?}"),
    }
    m.assert_async().await;
}

#[tokio::test]
async fn f4_a_recovered_poll_resets_the_failure_streak() {
    // Failures must be CONSECUTIVE. A couple of failures, one success, then a
    // fire — the streak resets on the success rather than accumulating to a
    // false hard stop.
    let mut server = mockito::Server::new_async().await;
    let _fail = server
        .mock("POST", "/info")
        .with_status(400)
        .expect(2)
        .create_async()
        .await;
    let _ok = server
        .mock("POST", "/info")
        .with_status(200)
        .with_body(fresh_book_body("40.10", "40.30"))
        .create_async()
        .await;

    let client = read_only_client(&server);
    let reason = tokio::time::timeout(
        Duration::from_secs(20),
        wait_for_trigger(
            &client,
            &Symbol::new("HYPE"),
            &trigger_cfg(Some((TriggerWhen::Above, dec!(40))), None),
        ),
    )
    .await
    .expect("trigger wait timed out")
    .unwrap();

    assert!(
        matches!(reason, TriggerOutcome::Fired(TriggerReason::Price { .. })),
        "{reason:?}"
    );
}
