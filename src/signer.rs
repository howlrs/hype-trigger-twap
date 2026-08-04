//! EIP-712 signer for HL L1 actions.
//!
//! Copied from `diff-old-new/executor/crates/executor-hl/src/signer.rs`;
//! `MockSigner` was dropped (this binary either signs for real or runs
//! read-only with no signer at all). Import paths rewritten to `crate::types`.
//!
//! The private key is held as a `secrecy::SecretString` until it is parsed
//! once in `from_secret`; the resulting `PrivateKeySigner` keeps it in a
//! `k256::SecretKey` that zeroizes on drop. The key is NEVER logged, and
//! `Eip712AgentSigner` deliberately has no `Debug` impl.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::errors::HlError;
use crate::types::Address;

use crate::eip712::{action_hash, build_agent, l1_domain};
use crate::eip712::{CancelByCloidAction, DummyAction, OrderAction, ScheduleCancelAction};
use alloy::primitives::Address as AlloyAddress;
use alloy::signers::SignerSync;
use alloy::sol_types::SolStruct;
use secrecy::{ExposeSecret, SecretString};

/// Hyperliquid action JSON value (typed-data preimage). Opaque at this layer.
pub type Action = serde_json::Value;

/// EIP-712 signature in HL wire format (`{r,s,v}` hex strings).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Signature {
    pub r: String,
    pub s: String,
    pub v: u8,
}

#[async_trait]
pub trait Signer: Send + Sync {
    /// Address that this signer signs for.
    fn address(&self) -> Address;

    /// Sign an L1 action with a specific nonce.
    /// `vault` allows trading on behalf of a vault/subaccount; pass `None`
    /// for direct master/agent action.
    /// `expires_after` (Issue #2) is the wall-clock Unix ms the action
    /// expires at; it MUST be the same value the caller puts in the
    /// `/exchange` request body's `expiresAfter` field, or the signature will
    /// not match what HL re-derives from the body. `None` omits the expires
    /// flag from the signed hash entirely (matches HL python-sdk semantics).
    async fn sign_l1(
        &self,
        action: &Action,
        nonce: u64,
        vault: Option<&Address>,
        expires_after: Option<u64>,
    ) -> Result<Signature, HlError>;
}

/// Real EIP-712 signer for HL L1 actions.
///
/// Holds an `alloy_signer_local::PrivateKeySigner` constructed from a
/// secret hex string. The secret is consumed once via `from_secret`; the
/// resulting `PrivateKeySigner` retains the key in its internal `k256`
/// `SecretKey` which zeroizes on drop.
pub struct Eip712AgentSigner {
    inner: alloy::signers::local::PrivateKeySigner,
    is_mainnet: bool,
}

/// Hand-written so the key material can never reach a log line. Only the
/// public address and the network are shown; `#[derive(Debug)]` is deliberately
/// NOT used here.
impl std::fmt::Debug for Eip712AgentSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Eip712AgentSigner")
            .field("address", &format_args!("{:#x}", self.inner.address()))
            .field("is_mainnet", &self.is_mainnet)
            .field("key", &"<redacted>")
            .finish()
    }
}

impl Eip712AgentSigner {
    /// Construct from an `0x`-prefixed 64-hex private key.
    ///
    /// The parse error from alloy is deliberately NOT included verbatim for
    /// key-shaped input — it can echo the malformed key. We emit a fixed
    /// message instead so a mistyped PK never reaches the logs.
    pub fn from_secret(pk: SecretString, is_mainnet: bool) -> Result<Self, HlError> {
        let s = pk.expose_secret().trim();
        let inner: alloy::signers::local::PrivateKeySigner = s.parse().map_err(|_| {
            HlError::InvalidConfig(
                "agent PK parse failed: expected 0x-prefixed 64-hex secp256k1 key \
                 (value redacted)"
                    .into(),
            )
        })?;
        Ok(Self { inner, is_mainnet })
    }
}

#[async_trait]
impl Signer for Eip712AgentSigner {
    fn address(&self) -> Address {
        // Lowercase 0x... 40-hex form, matching HL wire convention.
        Address::new(format!("{:#x}", self.inner.address()))
    }

    async fn sign_l1(
        &self,
        action: &Action,
        nonce: u64,
        vault: Option<&Address>,
        expires_after: Option<u64>,
    ) -> Result<Signature, HlError> {
        // Convert crate::types::Address (String wrapper) to alloy 20-byte
        // typed Address for action_hash. Parse failures surface as
        // ActionFormat (dynamic input data, not config).
        let vault_alloy = vault
            .map(|a| {
                a.as_str()
                    .parse::<AlloyAddress>()
                    .map_err(|e| HlError::ActionFormat(format!("vault address parse: {e}")))
            })
            .transpose()?;

        let hash = dispatch_and_hash(action, nonce, vault_alloy.as_ref(), expires_after)?;
        let agent = build_agent(hash, self.is_mainnet);
        let signing_hash = agent.eip712_signing_hash(&l1_domain());

        // alloy 2.0.4: PrivateKeySigner implements SignerSync::sign_hash_sync(&B256).
        let raw_sig = self
            .inner
            .sign_hash_sync(&signing_hash)
            .map_err(|e| HlError::Signature(format!("sign_hash: {e}")))?;

        // alloy 2.0.4 Signature: r() / s() → U256, v() → bool (parity:
        // true = 28, false = 27). HL wants v ∈ {27, 28}.
        let r_u256 = raw_sig.r();
        let s_u256 = raw_sig.s();
        let v_byte: u8 = if raw_sig.v() { 28 } else { 27 };

        Ok(Signature {
            r: format!("0x{:064x}", r_u256),
            s: format!("0x{:064x}", s_u256),
            v: v_byte,
        })
    }
}

/// Dispatch an action JSON to the correct strongly-typed struct, then
/// compute action_hash. Currently supports the action types exercised by
/// the cross-check fixture (dummy / order / scheduleCancel / cancelByCloid).
/// New action types must be added here AND have a matching struct in eip712.rs.
///
/// Uses `serde::Deserialize::deserialize(action)` (no clone) — `&serde_json::Value`
/// implements `Deserializer` directly, so an `action.clone()` would be wasted.
fn dispatch_and_hash(
    action: &Action,
    nonce: u64,
    vault: Option<&AlloyAddress>,
    expires_after: Option<u64>,
) -> Result<alloy::primitives::B256, HlError> {
    use serde::Deserialize;

    let kind = action
        .get("type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| HlError::ActionFormat("action.type missing or not string".into()))?;

    match kind {
        "dummy" => {
            let typed = DummyAction::deserialize(action)
                .map_err(|e| HlError::ActionFormat(format!("dummy decode: {e}")))?;
            action_hash(&typed, nonce, vault, expires_after)
                .map_err(|e| HlError::ActionFormat(format!("dummy msgpack: {e}")))
        }
        "order" => {
            let typed = OrderAction::deserialize(action)
                .map_err(|e| HlError::ActionFormat(format!("order decode: {e}")))?;
            action_hash(&typed, nonce, vault, expires_after)
                .map_err(|e| HlError::ActionFormat(format!("order msgpack: {e}")))
        }
        "scheduleCancel" => {
            let typed = ScheduleCancelAction::deserialize(action)
                .map_err(|e| HlError::ActionFormat(format!("scheduleCancel decode: {e}")))?;
            action_hash(&typed, nonce, vault, expires_after)
                .map_err(|e| HlError::ActionFormat(format!("scheduleCancel msgpack: {e}")))
        }
        "cancelByCloid" => {
            let typed = CancelByCloidAction::deserialize(action)
                .map_err(|e| HlError::ActionFormat(format!("cancelByCloid decode: {e}")))?;
            action_hash(&typed, nonce, vault, expires_after)
                .map_err(|e| HlError::ActionFormat(format!("cancelByCloid msgpack: {e}")))
        }
        other => Err(HlError::ActionFormat(format!(
            "unsupported action type for Eip712AgentSigner: {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use serde_json::json;

    const TEST_PK: &str = "0x0123456789012345678901234567890123456789012345678901234567890123";

    #[tokio::test]
    async fn from_secret_rejects_malformed_key_without_echoing_it() {
        let err = Eip712AgentSigner::from_secret(SecretString::new("0xdeadbeef".into()), true)
            .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("value redacted"), "msg was: {msg}");
        assert!(
            !msg.contains("deadbeef"),
            "error message must not echo the key material: {msg}"
        );
    }

    #[tokio::test]
    async fn debug_impl_never_leaks_key_material() {
        let s = Eip712AgentSigner::from_secret(SecretString::new(TEST_PK.into()), true).unwrap();
        let dbg = format!("{s:?}");
        assert!(dbg.contains("<redacted>"), "{dbg}");
        assert!(
            !dbg.contains("0123456789012345678901234567890123456789"),
            "Debug leaked the private key: {dbg}"
        );
    }

    #[tokio::test]
    async fn address_is_lowercase_0x_40_hex() {
        let s = Eip712AgentSigner::from_secret(SecretString::new(TEST_PK.into()), true).unwrap();
        let a = s.address();
        assert!(a.as_str().starts_with("0x"));
        assert_eq!(a.as_str().len(), 42);
        assert_eq!(a.as_str(), a.as_str().to_lowercase());
    }

    #[tokio::test]
    async fn mainnet_and_testnet_signatures_differ_for_same_action() {
        // Agent.source is "a" vs "b", so the signing hash — and thus the
        // signature — must differ.
        let action = json!({"type": "dummy", "num": 100000000000i64});
        let m = Eip712AgentSigner::from_secret(SecretString::new(TEST_PK.into()), true).unwrap();
        let t = Eip712AgentSigner::from_secret(SecretString::new(TEST_PK.into()), false).unwrap();
        let sm = m.sign_l1(&action, 0, None, None).await.unwrap();
        let st = t.sign_l1(&action, 0, None, None).await.unwrap();
        assert_ne!(sm, st);
    }

    #[tokio::test]
    async fn unsupported_action_type_is_rejected() {
        let s = Eip712AgentSigner::from_secret(SecretString::new(TEST_PK.into()), true).unwrap();
        let err = s
            .sign_l1(&json!({"type": "withdraw3"}), 1, None, None)
            .await
            .unwrap_err();
        assert!(matches!(err, HlError::ActionFormat(_)));
    }

    #[tokio::test]
    async fn v_is_27_or_28() {
        let s = Eip712AgentSigner::from_secret(SecretString::new(TEST_PK.into()), true).unwrap();
        for nonce in 0..8u64 {
            let sig = s
                .sign_l1(&json!({"type": "dummy", "num": 1i64}), nonce, None, None)
                .await
                .unwrap();
            assert!(sig.v == 27 || sig.v == 28, "v was {}", sig.v);
        }
    }
}
