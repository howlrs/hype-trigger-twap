//! Minimal Hyperliquid REST client (§5).
//!
//! Only the endpoints this binary needs: `/info meta`, `/info l2Book`,
//! `/info orderStatus`, `/exchange order`, `/exchange cancelByCloid`.
//!
//! Wire shapes and response parsing are lifted from
//! `diff-old-new/executor/crates/executor-hl/{wire.rs,hl_client.rs}`.
//!
//! Retry policy (§5) — this is the load-bearing difference from the source:
//! - `/info` reads (meta, l2Book, orderStatus, userRole) are idempotent, so
//!   transport failures (reqwest error, HTTP 5xx, HTTP 429) retry with
//!   exponential backoff 1s / 2s / 4s, up to 3 retries.
//! - `/exchange` writes (order, cancelByCloid) are NOT idempotent: the nonce is
//!   consumed the moment HL receives the body, so a blind resend of an
//!   already-signed body can only ever be rejected while the original may have
//!   filled. They are therefore sent EXACTLY ONCE (W1). Recovery from an
//!   ambiguous transport failure is the caller's job, via `orderStatus`
//!   reconciliation keyed on the cloid.
//! - Exchange rejections (top-level `status:"err"`, or a per-order
//!   `{"error": ...}`) are NEVER retried. They hard-stop the run.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::errors::HlError;
use crate::signer::Signer;
use crate::types::{BookLevel, CancelIntent, OrderBook, OrderId, OrderIntent, Symbol};

/// Backoff schedule for transport errors (§5): 3 retries max.
const RETRY_BACKOFF: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];

const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Which HL network to talk to. Drives both the base URLs and the EIP-712
/// `Agent.source` field ("a" mainnet / "b" testnet) — these MUST stay in sync,
/// so they are derived from the same value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    Mainnet,
    Testnet,
}

impl Network {
    pub fn is_mainnet(self) -> bool {
        matches!(self, Network::Mainnet)
    }

    pub fn base_url(self) -> &'static str {
        match self {
            Network::Mainnet => "https://api.hyperliquid.xyz",
            Network::Testnet => "https://api.hyperliquid-testnet.xyz",
        }
    }
}

impl std::fmt::Display for Network {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Network::Mainnet => write!(f, "mainnet"),
            Network::Testnet => write!(f, "testnet"),
        }
    }
}

/// Endpoint configuration. `HL_INFO_URL` / `HL_EXCHANGE_URL` override the
/// derived defaults (used by the mockito tests).
#[derive(Debug, Clone)]
pub struct HlConfig {
    pub info_url: String,
    pub exchange_url: String,
    pub network: Network,
}

impl HlConfig {
    pub fn new(network: Network) -> Self {
        let base = network.base_url();
        Self {
            info_url: format!("{base}/info"),
            exchange_url: format!("{base}/exchange"),
            network,
        }
    }

    pub fn with_overrides(mut self, info: Option<String>, exchange: Option<String>) -> Self {
        if let Some(i) = info {
            self.info_url = i;
        }
        if let Some(e) = exchange {
            self.exchange_url = e;
        }
        self
    }
}

// === wire types (mirrors of the HL JSON shapes) ===

/// HL `l2Book` response.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WireL2Book {
    pub coin: String,
    /// ms epoch of the snapshot.
    pub time: i64,
    /// Two arrays: \[bids, asks\]. Bids descending, asks ascending.
    pub levels: Vec<Vec<WireBookLevel>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WireBookLevel {
    #[serde(with = "rust_decimal::serde::str")]
    pub px: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub sz: Decimal,
    pub n: u32,
}

impl WireL2Book {
    /// Map into the domain `OrderBook`.
    ///
    /// HL guarantees `levels` is `[bids, asks]`; a malformed response with
    /// fewer than two arrays yields empty sides so callers see "no quotes"
    /// rather than panicking.
    pub fn to_orderbook(&self) -> OrderBook {
        let map = |v: &Vec<WireBookLevel>| -> Vec<BookLevel> {
            v.iter()
                .map(|l| BookLevel {
                    px: l.px,
                    sz: l.sz,
                    n: l.n,
                })
                .collect()
        };
        OrderBook {
            bids: self.levels.first().map(map).unwrap_or_default(),
            asks: self.levels.get(1).map(map).unwrap_or_default(),
            time_ms: self.time,
        }
    }
}

/// HL `meta` response (perp universe).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WireMeta {
    pub universe: Vec<WireUniverseEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WireUniverseEntry {
    pub name: String,
    pub sz_decimals: u32,
    #[serde(default)]
    pub max_leverage: u32,
    #[serde(default)]
    pub only_isolated: bool,
}

/// Resolved per-symbol metadata: universe index + size precision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssetMeta {
    pub asset_index: u32,
    pub sz_decimals: u32,
}

impl WireMeta {
    /// Resolve a symbol to its asset index and `szDecimals`.
    ///
    /// Mirrors `executor-hl::meta::MetaCache::resolve` — unknown symbols are a
    /// hard error so a typo can never reach `/exchange`.
    pub fn resolve(&self, symbol: &Symbol) -> Result<AssetMeta, HlError> {
        self.universe
            .iter()
            .enumerate()
            .find(|(_, e)| e.name == symbol.as_str())
            .map(|(idx, e)| AssetMeta {
                asset_index: idx as u32,
                sz_decimals: e.sz_decimals,
            })
            .ok_or_else(|| HlError::UnknownSymbol(symbol.clone()))
    }
}

/// HL `userRole` response (F1).
///
/// Wire shapes, mirroring `executor-hl::wire::WireUserRole`:
/// - `{"role":"user"}`
/// - `{"role":"agent","data":{"user":"0x<master>"}}`
/// - `{"role":"vault"}` / `"subAccount"` / `"missing"`
///
/// Forward compatibility: an untagged enum with an `Unknown(Value)` fallback so
/// a role HL adds later does NOT crash the client. `#[serde(other)]` is not
/// usable because it requires externally-tagged representation, while the
/// `agent` variant's `data` field requires internal tagging.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum WireUserRole {
    Tagged(WireUserRoleTagged),
    /// Raw JSON, captured when `role` is a value we do not know yet.
    Unknown(serde_json::Value),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum WireUserRoleTagged {
    User,
    Agent { data: WireAgentData },
    Vault,
    SubAccount,
    Missing,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WireAgentData {
    pub user: String,
}

/// Domain form of `userRole` (F1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    /// The queried address is a registered agent of `master`.
    Agent {
        master: crate::types::Address,
    },
    /// A plain user account (NOT an agent) — signed actions will be refused.
    User,
    Vault,
    SubAccount,
    /// HL does not know this address at all.
    Missing,
    /// A role HL added after this client was written.
    Unknown(String),
}

impl Role {
    /// Operator-facing label used in the fail-fast message.
    pub fn label(&self) -> String {
        match self {
            Role::Agent { master } => format!("agent (master {master})"),
            Role::User => "user".into(),
            Role::Vault => "vault".into(),
            Role::SubAccount => "subAccount".into(),
            Role::Missing => "missing (address unknown to HL)".into(),
            Role::Unknown(raw) => format!("unknown ({raw})"),
        }
    }
}

impl From<WireUserRole> for Role {
    fn from(w: WireUserRole) -> Self {
        match w {
            WireUserRole::Tagged(WireUserRoleTagged::Agent { data }) => Role::Agent {
                master: crate::types::Address::new(data.user.trim().to_ascii_lowercase()),
            },
            WireUserRole::Tagged(WireUserRoleTagged::User) => Role::User,
            WireUserRole::Tagged(WireUserRoleTagged::Vault) => Role::Vault,
            WireUserRole::Tagged(WireUserRoleTagged::SubAccount) => Role::SubAccount,
            WireUserRole::Tagged(WireUserRoleTagged::Missing) => Role::Missing,
            WireUserRole::Unknown(v) => Role::Unknown(v.to_string()),
        }
    }
}

/// Outcome of a single order placement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaceOutcome {
    /// `{"filled": {"oid", "totalSz", "avgPx"}}`
    Filled {
        oid: OrderId,
        total_sz: Decimal,
        avg_px: Decimal,
    },
    /// `{"resting": {"oid"}}` — unexpected for IOC, handled by the caller.
    Resting { oid: OrderId },
}

/// Fill information recovered from `/info orderStatus`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderStatusFill {
    /// Cumulative filled size (`origSz - sz` for a resting/partially filled
    /// order, `origSz` once it is fully filled).
    pub filled_sz: Decimal,
    pub avg_px: Option<Decimal>,
    /// Raw HL status string ("filled", "open", "canceled", ...).
    pub status: String,
}

/// HL order statuses that mean "this order will never fill again" (T3).
///
/// Only a terminal status may be adopted as the FINAL filled quantity. A
/// non-terminal status ("open", "triggered", or anything HL adds later) is a
/// snapshot of an order that can still fill in the next millisecond — treating
/// it as final under-counts the fill and makes every later slice over-order.
/// Written with the single-L American spelling; `is_terminal` normalises the
/// doubled-L variant before comparing, because HL is not consistent about it.
const TERMINAL_ORDER_STATUSES: &[&str] = &[
    "filled",
    "canceled",
    "marginCanceled",
    "rejected",
    "reduceOnlyCanceled",
    "liquidatedCanceled",
    "vaultWithdrawalCanceled",
    "openInterestCapCanceled",
    "selfTradeCanceled",
    "siblingFilledCanceled",
    "delistedCanceled",
    "scheduledCancel",
];

impl OrderStatusFill {
    /// True when the status guarantees the fill count can no longer change.
    ///
    /// Matching is case-insensitive and spelling-normalised ("cancelled" ==
    /// "canceled"). Both are safety measures in the same direction: a wording
    /// change on HL's side must not silently reclassify a SETTLED order as
    /// still-live, which would hard-stop an otherwise healthy run. The reverse
    /// error — treating a live order as settled — is guarded by keeping this
    /// list explicit, so an unrecognised status is always non-terminal.
    pub fn is_terminal(&self) -> bool {
        let got = normalise_status(&self.status);
        TERMINAL_ORDER_STATUSES
            .iter()
            .any(|t| normalise_status(t) == got)
    }
}

/// Lowercase and collapse the "cancelled"/"canceled" spelling split.
fn normalise_status(s: &str) -> String {
    s.to_ascii_lowercase().replace("cancelled", "canceled")
}

/// Minimal HL REST client.
pub struct HlClient {
    config: HlConfig,
    http: reqwest::Client,
    /// `None` in read-only mode — no signing, and `/exchange` is never called.
    signer: Option<Box<dyn Signer>>,
    /// Monotonic nonce: `max(last + 1, now_ms)` (§5).
    last_nonce: AtomicU64,
}

impl HlClient {
    pub fn new(config: HlConfig, signer: Option<Box<dyn Signer>>) -> Result<Self, HlError> {
        let http = reqwest::Client::builder()
            .pool_idle_timeout(Some(Duration::from_secs(60)))
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|e| HlError::InvalidConfig(format!("build http client: {e}")))?;
        Ok(Self {
            config,
            http,
            signer,
            last_nonce: AtomicU64::new(0),
        })
    }

    pub fn config(&self) -> &HlConfig {
        &self.config
    }

    /// Next nonce: `max(last + 1, now_ms)`. Monotonic even if two actions land
    /// inside the same millisecond.
    fn next_nonce(&self) -> u64 {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or_default();
        loop {
            let last = self.last_nonce.load(Ordering::Acquire);
            let next = now_ms.max(last + 1);
            if self
                .last_nonce
                .compare_exchange(last, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return next;
            }
        }
    }

    /// POST with the §5 retry policy. Only transport failures are retried;
    /// a 2xx response (even one carrying an exchange rejection in its body)
    /// returns immediately for the caller to parse.
    async fn post_with_retry(
        &self,
        url: &str,
        body: &serde_json::Value,
    ) -> Result<String, HlError> {
        let mut attempt = 0usize;
        loop {
            let outcome = self.post_once(url, body).await;
            match outcome {
                Ok(text) => return Ok(text),
                Err(e) => {
                    // Only Network (transport) errors are retryable.
                    let retryable = matches!(e, HlError::Network(_));
                    if !retryable || attempt >= RETRY_BACKOFF.len() {
                        return Err(e);
                    }
                    let wait = RETRY_BACKOFF[attempt];
                    tracing::warn!(
                        url = %url,
                        attempt = attempt + 1,
                        backoff_ms = wait.as_millis() as u64,
                        error = %e,
                        "transport error; retrying"
                    );
                    tokio::time::sleep(wait).await;
                    attempt += 1;
                }
            }
        }
    }

    /// One POST. Maps reqwest errors, 5xx and 429 into `HlError::Network`
    /// (retryable); other non-2xx into `HlError::Exchange` (fatal).
    async fn post_once(&self, url: &str, body: &serde_json::Value) -> Result<String, HlError> {
        let resp = self
            .http
            .post(url)
            .json(body)
            .send()
            .await
            .map_err(|e| HlError::Network(e.to_string()))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| HlError::Network(e.to_string()))?;
        if status.is_success() {
            return Ok(text);
        }
        if status.is_server_error() || status.as_u16() == 429 {
            return Err(HlError::Network(format!("HTTP {status}: {text}")));
        }
        // 4xx other than 429: the request itself is wrong. Retrying cannot help.
        Err(HlError::Exchange {
            code: Some(format!("http_{}", status.as_u16())),
            message: text,
        })
    }

    // === /info ===

    /// `{"type":"meta"}` → perp universe.
    pub async fn fetch_meta(&self) -> Result<WireMeta, HlError> {
        let body = serde_json::json!({"type": "meta"});
        let text = self.post_with_retry(&self.config.info_url, &body).await?;
        serde_json::from_str(&text).map_err(|e| HlError::InvalidResponse(format!("meta: {e}")))
    }

    /// `{"type":"l2Book","coin":SYM}` → top-of-book snapshot.
    pub async fn fetch_l2_book(&self, symbol: &Symbol) -> Result<OrderBook, HlError> {
        let body = serde_json::json!({"type": "l2Book", "coin": symbol.as_str()});
        let text = self.post_with_retry(&self.config.info_url, &body).await?;
        let wire: WireL2Book = serde_json::from_str(&text)
            .map_err(|e| HlError::InvalidResponse(format!("l2Book: {e}")))?;
        Ok(wire.to_orderbook())
    }

    /// `{"type":"userRole","user":addr}` → the address's HL role (F1).
    ///
    /// Used at live-mode startup to discover the MASTER account behind the
    /// agent key. HL books an agent's orders under the master, so `orderStatus`
    /// must be queried with the master address — querying with the agent
    /// address returns `unknownOid` for orders that really exist, which the
    /// recovery path would misread.
    pub async fn fetch_user_role(&self, user: &crate::types::Address) -> Result<Role, HlError> {
        let body = serde_json::json!({"type": "userRole", "user": user.as_str()});
        let text = self.post_with_retry(&self.config.info_url, &body).await?;
        parse_user_role(&text)
    }

    /// `{"type":"orderStatus","user":addr,"oid":<oid|cloid>}`.
    ///
    /// §5 note: HL's `orderStatus` takes `user` plus an `oid` that may be
    /// either the numeric exchange oid OR the 0x-prefixed cloid.
    ///
    /// F1: `user` MUST be the MASTER address (resolved by the `userRole` probe
    /// at startup), not the agent — HL books agent orders under the master.
    ///
    /// Returns `Ok(None)` when HL reports the order is unknown.
    pub async fn fetch_order_status(
        &self,
        user: &crate::types::Address,
        oid: OrderId,
    ) -> Result<Option<OrderStatusFill>, HlError> {
        self.order_status_body(user, serde_json::json!(oid.0)).await
    }

    /// `orderStatus` keyed on the CLOID instead of the exchange oid (W1).
    ///
    /// This is the idempotency key for `/exchange` reconciliation: when a place
    /// POST fails in transit we never learned an oid, but we chose the cloid
    /// ourselves before signing, so it is the only handle we have on the order
    /// that may or may not exist.
    pub async fn fetch_order_status_by_cloid(
        &self,
        user: &crate::types::Address,
        cloid: crate::types::Cloid,
    ) -> Result<Option<OrderStatusFill>, HlError> {
        self.order_status_body(user, serde_json::json!(cloid.to_hex_string()))
            .await
    }

    async fn order_status_body(
        &self,
        user: &crate::types::Address,
        oid: serde_json::Value,
    ) -> Result<Option<OrderStatusFill>, HlError> {
        let body = serde_json::json!({
            "type": "orderStatus",
            "user": user.as_str(),
            "oid": oid,
        });
        let text = self.post_with_retry(&self.config.info_url, &body).await?;
        parse_order_status(&text)
    }

    // === /exchange ===

    fn signer(&self) -> Result<&dyn Signer, HlError> {
        self.signer
            .as_deref()
            .ok_or_else(|| HlError::InvalidConfig("no signer configured (read-only mode)".into()))
    }

    /// Sign an action with a fresh nonce and POST it to `/exchange` EXACTLY
    /// ONCE (W1).
    ///
    /// There is deliberately no transport retry here. The nonce is burned the
    /// instant HL receives the body, so a resend of the same signed body can
    /// only be rejected as a stale nonce — while the original may already have
    /// filled. An `Err(HlError::Network(_))` from this call therefore means
    /// "outcome unknown", not "did not happen", and the caller must reconcile
    /// via `orderStatus` before deciding anything.
    ///
    /// Returns the nonce alongside the body text so callers can prove a resend
    /// used fresh nonce material.
    async fn post_exchange_once(
        &self,
        action: &serde_json::Value,
    ) -> Result<(u64, String), HlError> {
        let nonce = self.next_nonce();
        let sig = self.signer()?.sign_l1(action, nonce, None).await?;
        let body = serde_json::json!({
            "action": action,
            "nonce": nonce,
            "signature": sig,
            "vaultAddress": serde_json::Value::Null,
        });
        let text = self.post_once(&self.config.exchange_url, &body).await?;
        Ok((nonce, text))
    }

    /// Place a single IOC order. Returns the parsed per-order outcome.
    ///
    /// Sent exactly once (W1) — see `post_exchange_once`. A per-order
    /// `{"error": msg}` or a top-level `{"status":"err"}` is returned as
    /// `HlError::Exchange` — the caller hard-stops (§5).
    pub async fn place_order(
        &self,
        intent: &OrderIntent,
        asset: u32,
    ) -> Result<PlaceOutcome, HlError> {
        self.place_order_once(intent, asset).await.map(|(_, o)| o)
    }

    /// `place_order`, additionally reporting the nonce that was signed.
    pub async fn place_order_once(
        &self,
        intent: &OrderIntent,
        asset: u32,
    ) -> Result<(u64, PlaceOutcome), HlError> {
        let wire = crate::eip712::order_intent_to_wire(intent, asset);
        let action = crate::eip712::OrderAction {
            action_type: "order".into(),
            orders: vec![wire],
            grouping: "na".into(),
        };
        let action_value = serde_json::to_value(&action)
            .map_err(|e| HlError::ActionFormat(format!("order serialize: {e}")))?;
        let (nonce, text) = self.post_exchange_once(&action_value).await?;
        parse_place_response(&text).map(|o| (nonce, o))
    }

    /// Cancel one order by cloid. Returns `Ok(())` on `"success"`.
    ///
    /// Also sent exactly once (W1). A failed cancel is non-fatal: the caller's
    /// `orderStatus` reconciliation is what establishes the truth.
    pub async fn cancel_by_cloid(&self, intent: &CancelIntent, asset: u32) -> Result<(), HlError> {
        let wire = crate::eip712::cancel_intent_to_wire(intent, asset);
        let action = crate::eip712::CancelByCloidAction {
            action_type: "cancelByCloid".into(),
            cancels: vec![wire],
        };
        let action_value = serde_json::to_value(&action)
            .map_err(|e| HlError::ActionFormat(format!("cancel serialize: {e}")))?;
        let (_, text) = self.post_exchange_once(&action_value).await?;
        parse_cancel_response(&text)
    }
}

/// Render a `serde_json::Value` as a human-readable error string.
/// Strings are emitted verbatim; objects/arrays as compact JSON so the
/// diagnostic detail survives.
fn json_to_err_string(v: &serde_json::Value) -> String {
    if let Some(s) = v.as_str() {
        s.to_string()
    } else {
        v.to_string()
    }
}

/// Reject a top-level `{"status":"err","response":msg}` envelope.
fn check_top_level_err(v: &serde_json::Value) -> Result<(), HlError> {
    if v.get("status").and_then(|s| s.as_str()) == Some("err") {
        let msg = v
            .get("response")
            .map(json_to_err_string)
            .unwrap_or_else(|| "(no msg)".into());
        return Err(HlError::Exchange {
            code: Some("top_level_err".into()),
            message: msg,
        });
    }
    Ok(())
}

/// Extract the single element of `response.data.statuses`.
fn single_status(v: &serde_json::Value) -> Result<serde_json::Value, HlError> {
    let statuses = v
        .pointer("/response/data/statuses")
        .and_then(|s| s.as_array())
        .ok_or_else(|| HlError::InvalidResponse("statuses missing".into()))?;
    if statuses.len() != 1 {
        return Err(HlError::InvalidResponse(format!(
            "expected exactly 1 status, got {}",
            statuses.len()
        )));
    }
    Ok(statuses[0].clone())
}

fn decimal_field(v: &serde_json::Value, key: &str) -> Result<Decimal, HlError> {
    let raw = v
        .get(key)
        .ok_or_else(|| HlError::InvalidResponse(format!("{key} missing")))?;
    let s = match raw {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    s.parse::<Decimal>()
        .map_err(|e| HlError::InvalidResponse(format!("{key} not a decimal ({s}): {e}")))
}

/// Parse a single-order `/exchange` place response (§5).
pub fn parse_place_response(text: &str) -> Result<PlaceOutcome, HlError> {
    let v: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| HlError::InvalidResponse(format!("parse exchange json: {e}")))?;
    check_top_level_err(&v)?;
    let status = single_status(&v)?;

    if let Some(filled) = status.get("filled") {
        let oid = filled
            .get("oid")
            .and_then(|o| o.as_u64())
            .ok_or_else(|| HlError::InvalidResponse("filled.oid missing".into()))?;
        return Ok(PlaceOutcome::Filled {
            oid: OrderId(oid),
            total_sz: decimal_field(filled, "totalSz")?,
            avg_px: decimal_field(filled, "avgPx")?,
        });
    }
    if let Some(resting) = status.get("resting") {
        let oid = resting
            .get("oid")
            .and_then(|o| o.as_u64())
            .ok_or_else(|| HlError::InvalidResponse("resting.oid missing".into()))?;
        return Ok(PlaceOutcome::Resting { oid: OrderId(oid) });
    }
    if let Some(err) = status.get("error") {
        // Per-order rejection: fatal, never retried (§5).
        return Err(HlError::Exchange {
            code: Some("order_error".into()),
            message: json_to_err_string(err),
        });
    }
    Err(HlError::InvalidResponse(format!(
        "unknown place status shape: {status}"
    )))
}

/// Parse a single-cancel `/exchange` response. HL returns the bare string
/// `"success"` (NOT an object, unlike order responses).
pub fn parse_cancel_response(text: &str) -> Result<(), HlError> {
    let v: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| HlError::InvalidResponse(format!("parse cancel json: {e}")))?;
    check_top_level_err(&v)?;
    let status = single_status(&v)?;

    if status.as_str() == Some("success") {
        return Ok(());
    }
    if let Some(err) = status.get("error") {
        return Err(HlError::Exchange {
            code: Some("cancel_error".into()),
            message: json_to_err_string(err),
        });
    }
    Err(HlError::InvalidResponse(format!(
        "unknown cancel status shape: {status}"
    )))
}

/// Parse an `/info orderStatus` response.
///
/// Shape: `{"status":"order","order":{"order":{...},"status":"filled",...}}`
/// or `{"status":"unknownOid"}` when HL does not know the id.
pub fn parse_order_status(text: &str) -> Result<Option<OrderStatusFill>, HlError> {
    let v: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| HlError::InvalidResponse(format!("parse orderStatus json: {e}")))?;

    match v.get("status").and_then(|s| s.as_str()) {
        Some("unknownOid") => return Ok(None),
        Some("order") => {}
        Some(other) => {
            return Err(HlError::InvalidResponse(format!(
                "orderStatus: unexpected status {other}"
            )))
        }
        None => {
            return Err(HlError::InvalidResponse(
                "orderStatus: status missing".into(),
            ))
        }
    }

    let wrapper = v
        .get("order")
        .ok_or_else(|| HlError::InvalidResponse("orderStatus: order missing".into()))?;
    let inner = wrapper
        .get("order")
        .ok_or_else(|| HlError::InvalidResponse("orderStatus: order.order missing".into()))?;

    // `origSz` is the original size; `sz` is the UNFILLED remainder.
    let orig_sz = decimal_field(inner, "origSz")?;
    let remaining = decimal_field(inner, "sz")?;
    let filled_sz = (orig_sz - remaining).max(Decimal::ZERO);

    let status = wrapper
        .get("status")
        .and_then(|s| s.as_str())
        .unwrap_or("unknown")
        .to_string();

    // avgPx is not always present (HL omits it for never-filled orders).
    let avg_px = inner
        .get("avgPx")
        .and_then(|p| p.as_str())
        .and_then(|s| s.parse::<Decimal>().ok());

    Ok(Some(OrderStatusFill {
        filled_sz,
        avg_px,
        status,
    }))
}

/// Parse an `/info userRole` response into the domain `Role` (F1).
pub fn parse_user_role(text: &str) -> Result<Role, HlError> {
    let wire: WireUserRole = serde_json::from_str(text)
        .map_err(|e| HlError::InvalidResponse(format!("userRole: {e}")))?;
    Ok(wire.into())
}

/// True if `book.time_ms` is within `max_age` of now (§3 / §8).
///
/// `max_age_ms == 0` disables the check. A NEGATIVE age (HL's clock running
/// ahead of ours) counts as fresh — clock skew must not stall execution.
pub fn is_book_fresh(book: &OrderBook, max_age_ms: u64) -> bool {
    if max_age_ms == 0 {
        return true;
    }
    let now_ms = chrono::Utc::now().timestamp_millis();
    let age = now_ms - book.time_ms;
    age <= max_age_ms as i64
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use rust_decimal_macros::dec;

    const META_BODY: &str = r#"{"universe":[
        {"name":"BTC","szDecimals":5,"maxLeverage":40,"onlyIsolated":false},
        {"name":"ETH","szDecimals":4,"maxLeverage":25,"onlyIsolated":false},
        {"name":"HYPE","szDecimals":2,"maxLeverage":10,"onlyIsolated":false}
    ]}"#;

    #[test]
    fn meta_resolves_index_and_sz_decimals() {
        let meta: WireMeta = serde_json::from_str(META_BODY).unwrap();
        let btc = meta.resolve(&Symbol::new("BTC")).unwrap();
        assert_eq!(btc.asset_index, 0);
        assert_eq!(btc.sz_decimals, 5);
        let hype = meta.resolve(&Symbol::new("HYPE")).unwrap();
        assert_eq!(hype.asset_index, 2);
        assert_eq!(hype.sz_decimals, 2);
    }

    #[test]
    fn meta_unknown_symbol_is_hard_error() {
        let meta: WireMeta = serde_json::from_str(META_BODY).unwrap();
        let err = meta.resolve(&Symbol::new("NOPE")).unwrap_err();
        assert!(matches!(err, HlError::UnknownSymbol(_)));
    }

    #[test]
    fn l2_book_maps_bids_and_asks() {
        let body = r#"{"coin":"HYPE","time":1700000000000,"levels":[
            [{"px":"38.10","sz":"100","n":2},{"px":"38.09","sz":"50","n":1}],
            [{"px":"38.12","sz":"80","n":3}]
        ]}"#;
        let wire: WireL2Book = serde_json::from_str(body).unwrap();
        let book = wire.to_orderbook();
        assert_eq!(book.best_bid(), Some(dec!(38.10)));
        assert_eq!(book.best_ask(), Some(dec!(38.12)));
        assert_eq!(book.mid(), Some(dec!(38.11)));
        assert_eq!(book.time_ms, 1700000000000);
    }

    #[test]
    fn l2_book_malformed_levels_yields_empty_sides() {
        let body = r#"{"coin":"HYPE","time":1,"levels":[]}"#;
        let wire: WireL2Book = serde_json::from_str(body).unwrap();
        let book = wire.to_orderbook();
        assert!(book.bids.is_empty() && book.asks.is_empty());
        assert_eq!(book.mid(), None);
    }

    // === place response parsing ===

    #[test]
    fn place_filled_parses_size_and_price() {
        let body = r#"{"status":"ok","response":{"type":"order","data":{"statuses":[
            {"filled":{"oid":67890,"totalSz":"0.001","avgPx":"2000.5"}}]}}}"#;
        match parse_place_response(body).unwrap() {
            PlaceOutcome::Filled {
                oid,
                total_sz,
                avg_px,
            } => {
                assert_eq!(oid, OrderId(67890));
                assert_eq!(total_sz, dec!(0.001));
                assert_eq!(avg_px, dec!(2000.5));
            }
            other => panic!("expected Filled, got {other:?}"),
        }
    }

    #[test]
    fn place_resting_parses_oid() {
        let body = r#"{"status":"ok","response":{"type":"order","data":{"statuses":[
            {"resting":{"oid":12345}}]}}}"#;
        assert_eq!(
            parse_place_response(body).unwrap(),
            PlaceOutcome::Resting {
                oid: OrderId(12345)
            }
        );
    }

    #[test]
    fn place_per_order_error_is_exchange_error_not_retryable() {
        let body = r#"{"status":"ok","response":{"type":"order","data":{"statuses":[
            {"error":"MinTradeNtl"}]}}}"#;
        match parse_place_response(body).unwrap_err() {
            HlError::Exchange { code, message } => {
                assert_eq!(code.as_deref(), Some("order_error"));
                assert_eq!(message, "MinTradeNtl");
            }
            other => panic!("expected Exchange, got {other:?}"),
        }
    }

    #[test]
    fn place_top_level_err_is_exchange_error() {
        let body = r#"{"status":"err","response":"Insufficient margin"}"#;
        match parse_place_response(body).unwrap_err() {
            HlError::Exchange { code, message } => {
                assert_eq!(code.as_deref(), Some("top_level_err"));
                assert!(message.contains("Insufficient margin"));
            }
            other => panic!("expected Exchange, got {other:?}"),
        }
    }

    #[test]
    fn place_unknown_shape_is_invalid_response() {
        let body = r#"{"status":"ok","response":{"type":"order","data":{"statuses":[
            {"weirdNewStatus":{}}]}}}"#;
        assert!(matches!(
            parse_place_response(body).unwrap_err(),
            HlError::InvalidResponse(_)
        ));
    }

    // === cancel response parsing ===

    #[test]
    fn cancel_success_string_is_ok() {
        let body =
            r#"{"status":"ok","response":{"type":"cancel","data":{"statuses":["success"]}}}"#;
        assert!(parse_cancel_response(body).is_ok());
    }

    #[test]
    fn cancel_error_object_is_exchange_error() {
        let body = r#"{"status":"ok","response":{"type":"cancel","data":{"statuses":[
            {"error":"Order was never placed, already canceled, or filled"}]}}}"#;
        match parse_cancel_response(body).unwrap_err() {
            HlError::Exchange { message, .. } => assert!(message.contains("already canceled")),
            other => panic!("expected Exchange, got {other:?}"),
        }
    }

    // === orderStatus parsing ===

    #[test]
    fn order_status_partial_fill_computes_filled_size() {
        let body = r#"{"status":"order","order":{"order":{
            "coin":"HYPE","side":"B","limitPx":"38.2","sz":"3.0","origSz":"10.0",
            "oid":999,"timestamp":1700000000000,"avgPx":"38.15"},
            "status":"open","statusTimestamp":1700000000001}}"#;
        let got = parse_order_status(body).unwrap().unwrap();
        assert_eq!(got.filled_sz, dec!(7.0));
        assert_eq!(got.avg_px, Some(dec!(38.15)));
        assert_eq!(got.status, "open");
    }

    #[test]
    fn order_status_fully_filled_reports_orig_size() {
        let body = r#"{"status":"order","order":{"order":{
            "coin":"HYPE","side":"B","limitPx":"38.2","sz":"0.0","origSz":"10.0",
            "oid":999,"timestamp":1700000000000,"avgPx":"38.15"},
            "status":"filled","statusTimestamp":1700000000001}}"#;
        let got = parse_order_status(body).unwrap().unwrap();
        assert_eq!(got.filled_sz, dec!(10.0));
        assert_eq!(got.status, "filled");
    }

    #[test]
    fn order_status_unknown_oid_returns_none() {
        assert_eq!(
            parse_order_status(r#"{"status":"unknownOid"}"#).unwrap(),
            None
        );
    }

    #[test]
    fn order_status_missing_avg_px_is_none() {
        let body = r#"{"status":"order","order":{"order":{
            "coin":"HYPE","side":"B","limitPx":"38.2","sz":"10.0","origSz":"10.0",
            "oid":999,"timestamp":1700000000000},
            "status":"open","statusTimestamp":1}}"#;
        let got = parse_order_status(body).unwrap().unwrap();
        assert_eq!(got.filled_sz, dec!(0));
        assert_eq!(got.avg_px, None);
    }

    // === T3: terminal vs non-terminal order status ===

    fn fill(status: &str) -> OrderStatusFill {
        OrderStatusFill {
            filled_sz: dec!(1),
            avg_px: None,
            status: status.into(),
        }
    }

    #[test]
    fn terminal_statuses_are_safe_to_adopt_as_final() {
        for s in [
            "filled",
            "canceled",
            "marginCanceled",
            "rejected",
            "reduceOnlyCanceled",
            "openInterestCapCanceled",
        ] {
            assert!(fill(s).is_terminal(), "'{s}' should be terminal");
        }
    }

    #[test]
    fn open_status_is_not_terminal() {
        // The T3 bug in one assertion: an "open" order is still live and can
        // fill again, so its fill count must never be adopted as final.
        assert!(!fill("open").is_terminal());
        assert!(!fill("triggered").is_terminal());
    }

    #[test]
    fn unknown_status_is_treated_as_non_terminal() {
        // Fail safe: a status HL adds later is NOT assumed settled. Guessing
        // "terminal" would under-count a fill and over-order every later slice.
        assert!(!fill("someFutureStatus").is_terminal());
        assert!(!fill("unknown").is_terminal());
    }

    #[test]
    fn terminal_match_is_case_insensitive() {
        // A casing change in HL's wording must not silently reclassify a
        // settled order as still-live (which would hard-stop a healthy run).
        assert!(fill("Filled").is_terminal());
        assert!(fill("CANCELED").is_terminal());
        assert!(fill("marginCancelled").is_terminal());
    }

    // === F1: userRole parsing ===

    #[test]
    fn user_role_agent_carries_the_master_address() {
        let r = parse_user_role(
            r#"{"role":"agent","data":{"user":"0x00000000000000000000000000000000000000aa"}}"#,
        )
        .unwrap();
        match r {
            Role::Agent { master } => assert_eq!(
                master.as_str(),
                "0x00000000000000000000000000000000000000aa"
            ),
            other => panic!("expected Agent, got {other:?}"),
        }
    }

    #[test]
    fn user_role_non_agent_variants_parse() {
        assert_eq!(parse_user_role(r#"{"role":"user"}"#).unwrap(), Role::User);
        assert_eq!(parse_user_role(r#"{"role":"vault"}"#).unwrap(), Role::Vault);
        assert_eq!(
            parse_user_role(r#"{"role":"subAccount"}"#).unwrap(),
            Role::SubAccount
        );
        assert_eq!(
            parse_user_role(r#"{"role":"missing"}"#).unwrap(),
            Role::Missing
        );
    }

    #[test]
    fn user_role_unknown_role_degrades_instead_of_failing() {
        // Forward compatibility: a new HL role must not crash the client, but
        // it also must not be mistaken for a registered agent.
        let r = parse_user_role(r#"{"role":"marketMaker","data":{"tier":1}}"#).unwrap();
        assert!(matches!(r, Role::Unknown(_)), "got {r:?}");
        assert!(!matches!(r, Role::Agent { .. }));
    }

    #[test]
    fn every_role_has_an_operator_facing_label() {
        for r in [
            Role::Agent {
                master: crate::types::Address::new("0xaa"),
            },
            Role::User,
            Role::Vault,
            Role::SubAccount,
            Role::Missing,
            Role::Unknown("{}".into()),
        ] {
            assert!(!r.label().is_empty(), "{r:?} has no label");
        }
    }

    // === book freshness ===

    #[test]
    fn book_fresh_when_recent() {
        let book = OrderBook {
            time_ms: chrono::Utc::now().timestamp_millis(),
            ..Default::default()
        };
        assert!(is_book_fresh(&book, 3000));
    }

    #[test]
    fn book_stale_when_old() {
        let book = OrderBook {
            time_ms: chrono::Utc::now().timestamp_millis() - 10_000,
            ..Default::default()
        };
        assert!(!is_book_fresh(&book, 3000));
    }

    #[test]
    fn book_freshness_check_disabled_with_zero() {
        let book = OrderBook {
            time_ms: 0,
            ..Default::default()
        };
        assert!(is_book_fresh(&book, 0));
    }

    #[test]
    fn negative_age_counts_as_fresh() {
        // HL server clock ahead of ours — must NOT be treated as stale.
        let book = OrderBook {
            time_ms: chrono::Utc::now().timestamp_millis() + 5_000,
            ..Default::default()
        };
        assert!(is_book_fresh(&book, 3000));
    }

    // === nonce ===

    #[test]
    fn nonce_is_strictly_monotonic() {
        let client = HlClient::new(HlConfig::new(Network::Testnet), None).unwrap();
        let mut prev = 0u64;
        for _ in 0..1000 {
            let n = client.next_nonce();
            assert!(n > prev, "nonce {n} not > {prev}");
            prev = n;
        }
    }

    #[test]
    fn network_urls_and_source_stay_in_sync() {
        let m = HlConfig::new(Network::Mainnet);
        assert!(m.info_url.contains("api.hyperliquid.xyz"));
        assert!(m.network.is_mainnet());
        let t = HlConfig::new(Network::Testnet);
        assert!(t.info_url.contains("testnet"));
        assert!(!t.network.is_mainnet());
    }

    #[test]
    fn exchange_call_without_signer_is_config_error() {
        let client = HlClient::new(HlConfig::new(Network::Testnet), None).unwrap();
        assert!(matches!(client.signer(), Err(HlError::InvalidConfig(_))));
    }
}
