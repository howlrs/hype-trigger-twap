//! End-to-end `/info` + `/exchange` tests against a mockito server.
//!
//! No real network, no real key beyond the well-known test PK from the
//! signing fixture. Covers §5: wire shapes, response parsing, the
//! transport-retry policy, and the "exchange rejections are never retried"
//! rule.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use hype_trigger_twap::client::{HlClient, HlConfig, Network, PlaceOutcome};
use hype_trigger_twap::errors::{HlError, RejectionKind};
use hype_trigger_twap::signer::Eip712AgentSigner;
use hype_trigger_twap::types::{CancelIntent, Cloid, OrderId, OrderIntent, Side, Symbol, Tif};
use rust_decimal_macros::dec;
use secrecy::SecretString;

const TEST_PK: &str = "0x0123456789012345678901234567890123456789012345678901234567890123";

const META_BODY: &str = r#"{"universe":[
    {"name":"BTC","szDecimals":5,"maxLeverage":40,"onlyIsolated":false},
    {"name":"ETH","szDecimals":4,"maxLeverage":25,"onlyIsolated":false},
    {"name":"HYPE","szDecimals":2,"maxLeverage":10,"onlyIsolated":false}
]}"#;

/// Client pointed at the mock server, with a real signer (testnet domain).
fn make_client(server: &mockito::ServerGuard) -> HlClient {
    let signer = Eip712AgentSigner::from_secret(SecretString::new(TEST_PK.into()), false).unwrap();
    let config = HlConfig::new(Network::Testnet).with_overrides(
        Some(format!("{}/info", server.url())),
        Some(format!("{}/exchange", server.url())),
    );
    HlClient::new(config, Some(Box::new(signer))).unwrap()
}

/// Read-only client: no signer at all.
fn make_read_only_client(server: &mockito::ServerGuard) -> HlClient {
    let config = HlConfig::new(Network::Testnet).with_overrides(
        Some(format!("{}/info", server.url())),
        Some(format!("{}/exchange", server.url())),
    );
    HlClient::new(config, None).unwrap()
}

fn order_intent() -> OrderIntent {
    OrderIntent {
        cloid: Cloid::new(),
        symbol: Symbol::new("ETH"),
        side: Side::Long,
        px: dec!(2000),
        sz: dec!(0.001),
        tif: Tif::Ioc,
        reduce_only: false,
    }
}

// === /info meta ===

#[tokio::test]
async fn meta_resolves_symbol_to_asset_and_precision() {
    let mut server = mockito::Server::new_async().await;
    let _m = server
        .mock("POST", "/info")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(META_BODY)
        .create_async()
        .await;

    let client = make_read_only_client(&server);
    let meta = client.fetch_meta().await.unwrap();
    let hype = meta.resolve(&Symbol::new("HYPE")).unwrap();
    assert_eq!(hype.asset_index, 2);
    assert_eq!(hype.sz_decimals, 2);
}

#[tokio::test]
async fn meta_unknown_symbol_aborts_before_any_exchange_call() {
    let mut server = mockito::Server::new_async().await;
    let _m = server
        .mock("POST", "/info")
        .with_status(200)
        .with_body(META_BODY)
        .create_async()
        .await;
    // Deliberately NO /exchange mock: any call there would 501 and fail loudly.

    let client = make_read_only_client(&server);
    let meta = client.fetch_meta().await.unwrap();
    let err = meta.resolve(&Symbol::new("NOTACOIN")).unwrap_err();
    assert!(matches!(err, HlError::UnknownSymbol(_)));
}

// === /info l2Book ===

#[tokio::test]
async fn l2_book_parses_touch_and_timestamp() {
    let mut server = mockito::Server::new_async().await;
    let _m = server
        .mock("POST", "/info")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{"coin":"HYPE","time":1700000000000,"levels":[
                [{"px":"38.10","sz":"120.5","n":3}],
                [{"px":"38.14","sz":"90.0","n":2}]]}"#,
        )
        .create_async()
        .await;

    let client = make_read_only_client(&server);
    let book = client.fetch_l2_book(&Symbol::new("HYPE")).await.unwrap();
    assert_eq!(book.best_bid(), Some(dec!(38.10)));
    assert_eq!(book.best_ask(), Some(dec!(38.14)));
    assert_eq!(book.mid(), Some(dec!(38.12)));
    assert_eq!(book.time_ms, 1700000000000);
}

// === /exchange place ===

#[tokio::test]
async fn place_filled_response_parses() {
    let mut server = mockito::Server::new_async().await;
    let _m = server
        .mock("POST", "/exchange")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{"status":"ok","response":{"type":"order","data":{"statuses":[
                {"filled":{"oid":67890,"totalSz":"0.001","avgPx":"2000.5"}}]}}}"#,
        )
        .create_async()
        .await;

    let client = make_client(&server);
    let out = client
        .place_order(&order_intent(), 1, 1_700_000_000_000)
        .await
        .unwrap();
    assert_eq!(
        out,
        PlaceOutcome::Filled {
            oid: OrderId(67890),
            total_sz: dec!(0.001),
            avg_px: dec!(2000.5),
        }
    );
}

/// Issue #2: the `/exchange` body's `expiresAfter` field must carry the exact
/// same value that was used in the SIGNED action hash — this is the
/// acceptance criterion "`/exchange` body の `expiresAfter` と署名に使った値
/// が一致する". `Matcher::PartialJsonString` asserts the outgoing body
/// contains this field with this exact value; a separate cross-check
/// (`tests/signing_cross_check.rs`) proves the msgpack/hash side is correct
/// against Hyperliquid python-sdk vectors — this test proves the CLIENT
/// wires the two together, not just that each is independently plausible.
#[tokio::test]
async fn place_request_body_carries_the_same_expires_after_used_for_signing() {
    let mut server = mockito::Server::new_async().await;
    let m = server
        .mock("POST", "/exchange")
        .match_body(mockito::Matcher::PartialJsonString(
            r#"{"expiresAfter": 1700000000000}"#.to_string(),
        ))
        .with_status(200)
        .with_body(
            r#"{"status":"ok","response":{"type":"order","data":{"statuses":[
                {"filled":{"oid":1,"totalSz":"0.001","avgPx":"2000.5"}}]}}}"#,
        )
        .expect(1)
        .create_async()
        .await;

    let client = make_client(&server);
    client
        .place_order(&order_intent(), 1, 1_700_000_000_000)
        .await
        .unwrap();

    // mockito rejects the request (500, and the mock's expect(1) fails) if
    // the body didn't carry the matching expiresAfter — this assertion is
    // the proof the mock actually matched, not a false pass.
    m.assert_async().await;
}

/// A `cancelByCloid` is NOT subject to the execution deadline (Issue #2 PM
/// decision: cancels remain allowed past the deadline), so it must be signed
/// with `expiresAfter: null` — cancelling should never itself be able to
/// expire.
#[tokio::test]
async fn cancel_request_body_has_no_expires_after() {
    let mut server = mockito::Server::new_async().await;
    let m = server
        .mock("POST", "/exchange")
        .match_body(mockito::Matcher::PartialJsonString(
            r#"{"expiresAfter": null}"#.to_string(),
        ))
        .with_status(200)
        .with_body(
            r#"{"status":"ok","response":{"type":"cancel","data":{"statuses":["success"]}}}"#,
        )
        .expect(1)
        .create_async()
        .await;

    let client = make_client(&server);
    let intent = CancelIntent {
        symbol: Symbol::new("ETH"),
        by_cloid: Cloid::new(),
    };
    client.cancel_by_cloid(&intent, 1).await.unwrap();
    m.assert_async().await;
}

#[tokio::test]
async fn place_resting_response_parses() {
    let mut server = mockito::Server::new_async().await;
    let _m = server
        .mock("POST", "/exchange")
        .with_status(200)
        .with_body(
            r#"{"status":"ok","response":{"type":"order","data":{"statuses":[
                {"resting":{"oid":12345}}]}}}"#,
        )
        .create_async()
        .await;

    let client = make_client(&server);
    let out = client
        .place_order(&order_intent(), 1, 1_700_000_000_000)
        .await
        .unwrap();
    assert_eq!(
        out,
        PlaceOutcome::Resting {
            oid: OrderId(12345)
        }
    );
}

#[tokio::test]
async fn place_per_order_error_is_fatal_and_never_retried() {
    let mut server = mockito::Server::new_async().await;
    // expect(1): a retry would make this assertion fail.
    let m = server
        .mock("POST", "/exchange")
        .with_status(200)
        .with_body(
            r#"{"status":"ok","response":{"type":"order","data":{"statuses":[
                {"error":"Order has zero size. asset=1 MinTradeNtl"}]}}}"#,
        )
        .expect(1)
        .create_async()
        .await;

    let client = make_client(&server);
    let err = client
        .place_order(&order_intent(), 1, 1_700_000_000_000)
        .await
        .unwrap_err();
    match err {
        HlError::Exchange { code, message } => {
            assert_eq!(code.as_deref(), Some("order_error"));
            assert_eq!(
                RejectionKind::classify(&message),
                RejectionKind::MinNotional
            );
        }
        other => panic!("expected Exchange, got {other:?}"),
    }
    m.assert_async().await; // exactly one request — no retry
}

#[tokio::test]
async fn place_top_level_err_is_fatal_and_never_retried() {
    let mut server = mockito::Server::new_async().await;
    let m = server
        .mock("POST", "/exchange")
        .with_status(200)
        .with_body(r#"{"status":"err","response":"Insufficient margin to place order"}"#)
        .expect(1)
        .create_async()
        .await;

    let client = make_client(&server);
    let err = client
        .place_order(&order_intent(), 1, 1_700_000_000_000)
        .await
        .unwrap_err();
    match err {
        HlError::Exchange { code, message } => {
            assert_eq!(code.as_deref(), Some("top_level_err"));
            assert_eq!(
                RejectionKind::classify(&message),
                RejectionKind::InsufficientMargin
            );
        }
        other => panic!("expected Exchange, got {other:?}"),
    }
    m.assert_async().await;
}

#[tokio::test]
async fn read_only_client_cannot_place_an_order() {
    let server = mockito::Server::new_async().await;
    // No /exchange mock: proving we never get that far.
    let client = make_read_only_client(&server);
    let err = client
        .place_order(&order_intent(), 1, 1_700_000_000_000)
        .await
        .unwrap_err();
    assert!(matches!(err, HlError::InvalidConfig(_)));
}

// === /exchange cancel ===

#[tokio::test]
async fn cancel_success_string_parses() {
    let mut server = mockito::Server::new_async().await;
    let _m = server
        .mock("POST", "/exchange")
        .with_status(200)
        .with_body(
            r#"{"status":"ok","response":{"type":"cancel","data":{"statuses":["success"]}}}"#,
        )
        .create_async()
        .await;

    let client = make_client(&server);
    let intent = CancelIntent {
        symbol: Symbol::new("ETH"),
        by_cloid: Cloid::new(),
    };
    client.cancel_by_cloid(&intent, 1).await.unwrap();
}

#[tokio::test]
async fn cancel_error_object_is_exchange_error() {
    let mut server = mockito::Server::new_async().await;
    let _m = server
        .mock("POST", "/exchange")
        .with_status(200)
        .with_body(
            r#"{"status":"ok","response":{"type":"cancel","data":{"statuses":[
                {"error":"Order was never placed, already canceled, or filled"}]}}}"#,
        )
        .create_async()
        .await;

    let client = make_client(&server);
    let intent = CancelIntent {
        symbol: Symbol::new("ETH"),
        by_cloid: Cloid::new(),
    };
    let err = client.cancel_by_cloid(&intent, 1).await.unwrap_err();
    assert!(matches!(err, HlError::Exchange { .. }));
}

// === /info orderStatus ===

#[tokio::test]
async fn order_status_recovers_partial_fill_size() {
    let mut server = mockito::Server::new_async().await;
    let _m = server
        .mock("POST", "/info")
        .with_status(200)
        .with_body(
            r#"{"status":"order","order":{"order":{
                "coin":"HYPE","side":"B","limitPx":"38.2","sz":"2.5","origSz":"10.0",
                "oid":999,"timestamp":1700000000000,"avgPx":"38.15"},
                "status":"open","statusTimestamp":1700000000001}}"#,
        )
        .create_async()
        .await;

    let client = make_client(&server);
    let user = hype_trigger_twap::types::Address::new("0x0000000000000000000000000000000000000001");
    let got = client
        .fetch_order_status(&user, OrderId(999))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.filled_sz, dec!(7.5));
    assert_eq!(got.avg_px, Some(dec!(38.15)));
}

#[tokio::test]
async fn order_status_unknown_oid_is_none_not_an_error() {
    let mut server = mockito::Server::new_async().await;
    let _m = server
        .mock("POST", "/info")
        .with_status(200)
        .with_body(r#"{"status":"unknownOid"}"#)
        .create_async()
        .await;

    let client = make_client(&server);
    let user = hype_trigger_twap::types::Address::new("0x0000000000000000000000000000000000000001");
    assert!(client
        .fetch_order_status(&user, OrderId(999))
        .await
        .unwrap()
        .is_none());
}

// === transport retry policy (§5) ===

#[tokio::test]
async fn transport_5xx_is_retried_then_succeeds() {
    let mut server = mockito::Server::new_async().await;
    // First two attempts 503, third succeeds. Mockito matches in order.
    let fail = server
        .mock("POST", "/info")
        .with_status(503)
        .with_body("upstream unavailable")
        .expect(2)
        .create_async()
        .await;
    let ok = server
        .mock("POST", "/info")
        .with_status(200)
        .with_body(META_BODY)
        .expect(1)
        .create_async()
        .await;

    let client = make_read_only_client(&server);
    // Backoff is 1s + 2s of real time here; well inside the test timeout.
    let meta = tokio::time::timeout(Duration::from_secs(20), client.fetch_meta())
        .await
        .expect("retry sequence timed out")
        .expect("should succeed on the third attempt");
    assert_eq!(meta.universe.len(), 3);
    fail.assert_async().await;
    ok.assert_async().await;
}

#[tokio::test]
async fn transport_429_is_retried() {
    let mut server = mockito::Server::new_async().await;
    let fail = server
        .mock("POST", "/info")
        .with_status(429)
        .with_body("rate limited")
        .expect(1)
        .create_async()
        .await;
    let ok = server
        .mock("POST", "/info")
        .with_status(200)
        .with_body(META_BODY)
        .expect(1)
        .create_async()
        .await;

    let client = make_read_only_client(&server);
    let meta = tokio::time::timeout(Duration::from_secs(20), client.fetch_meta())
        .await
        .expect("retry timed out")
        .unwrap();
    assert_eq!(meta.universe.len(), 3);
    fail.assert_async().await;
    ok.assert_async().await;
}

#[tokio::test]
async fn transport_errors_give_up_after_the_retry_budget() {
    let mut server = mockito::Server::new_async().await;
    // Always 503: 1 initial + 3 retries = 4 requests, then Err.
    let m = server
        .mock("POST", "/info")
        .with_status(503)
        .with_body("still down")
        .expect(4)
        .create_async()
        .await;

    let client = make_read_only_client(&server);
    let err = tokio::time::timeout(Duration::from_secs(30), client.fetch_meta())
        .await
        .expect("retry sequence timed out")
        .unwrap_err();
    assert!(matches!(err, HlError::Network(_)), "got {err:?}");
    m.assert_async().await;
}

#[tokio::test]
async fn client_error_4xx_is_not_retried() {
    let mut server = mockito::Server::new_async().await;
    // 400 is our fault, not a transport blip — retrying cannot help.
    let m = server
        .mock("POST", "/info")
        .with_status(400)
        .with_body("bad request")
        .expect(1)
        .create_async()
        .await;

    let client = make_read_only_client(&server);
    let err = client.fetch_meta().await.unwrap_err();
    assert!(matches!(err, HlError::Exchange { .. }), "got {err:?}");
    m.assert_async().await;
}
