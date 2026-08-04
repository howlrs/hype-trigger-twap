#!/usr/bin/env python3
"""Generate known EIP-712 signing vectors from hyperliquid-python-sdk.

Cross-check fixture for the Rust `Eip712AgentSigner` (Issue #2, `expiresAfter`).

Provenance:
    - Pinned SDK version: hyperliquid-python-sdk==0.23.0 (see `pip show
      hyperliquid-python-sdk` in the venv below; confirmed via `pip freeze`).
    - Exact invocation used to regenerate `tests/fixtures/signing/known_vectors.json`:

        python3 scripts/gen_signing_vectors.py > /tmp/generated_vectors.json
        diff /tmp/generated_vectors.json tests/fixtures/signing/known_vectors.json

      (should print no diff; the script's stdout is the full, ordered vector list).

    - How to (re)create the venv used to run this script:

        python3 -m venv .venv
        source .venv/bin/activate
        pip install hyperliquid-python-sdk==0.23.0 eth-account msgpack

      This mirrors the minimal set of packages this script actually imports
      (`eth_account`, `hyperliquid.utils.signing`, `hyperliquid.utils.types`);
      `hyperliquid-python-sdk` itself pulls in `eth-account` and `msgpack` as
      transitive dependencies, but pinning all three explicitly keeps the
      venv reproducible even if that changes upstream.

The first 10 vectors (`dummy_*`, `order_eth_*`, `order_with_cloid_*`,
`dummy_with_vault_*`, `schedule_cancel_*`) are the original cross-check set,
ported byte-for-byte (same emit() calls, same inputs, same order) from the
parent repository's `diff-old-new/scripts/gen_signing_vectors.py` — they must
keep regenerating identically to `tests/fixtures/signing/known_vectors.json`'s
first 10 entries. Do not edit those `emit()` calls.

The next 7 vectors (`*_expires_after_*`, `order_with_cloid_vault_expires_after_*`,
`dummy_expires_after_zero_mainnet`) are new, added for Issue #2's `expiresAfter`
signing support. They cover: the simplest `expires_after` case (dummy action,
both networks), the actual order shape `run_twap` signs (IOC order, both
networks), cloid + vault + expires_after combined (catches any ordering bug
between the vault-flag byte and the expires-flag byte in `action_hash`'s
tail), and the `expires_after=0` edge case (must still emit the flag byte +
8 zero bytes, distinct from `expires_after=None` which omits the flag
entirely).

Source vectors mirror tests/signing_test.py from the SDK master branch.
"""
import json
import sys
import eth_account
from hyperliquid.utils.signing import (
    sign_l1_action,
    order_request_to_order_wire,
    order_wires_to_order_action,
    float_to_int_for_hashing,
)
from hyperliquid.utils.types import Cloid

# Same private key as the SDK's signing_test.py uses.
PK = "0x0123456789012345678901234567890123456789012345678901234567890123"
wallet = eth_account.Account.from_key(PK)

VECTORS = []


def emit(name, action, nonce, vault, expires, is_mainnet):
    sig = sign_l1_action(wallet, action, vault, nonce, expires, is_mainnet)
    VECTORS.append(
        {
            "name": name,
            "action": action,
            "nonce": nonce,
            "vault_address": vault,
            "expires_after": expires,
            "is_mainnet": is_mainnet,
            "expected_r": sig["r"],
            "expected_s": sig["s"],
            "expected_v": sig["v"],
            "expected_address": wallet.address.lower(),
        }
    )


# --- Original 10 vectors (unchanged from diff-old-new/scripts/gen_signing_vectors.py) ---

# Vector 1: dummy action
dummy_action = {"type": "dummy", "num": float_to_int_for_hashing(1000)}
emit("dummy_mainnet", dummy_action, 0, None, None, True)
emit("dummy_testnet", dummy_action, 0, None, None, False)

# Vector 2: order
order_request = {
    "coin": "ETH",
    "is_buy": True,
    "sz": 100,
    "limit_px": 100,
    "reduce_only": False,
    "order_type": {"limit": {"tif": "Gtc"}},
    "cloid": None,
}
order_action = order_wires_to_order_action(
    [order_request_to_order_wire(order_request, 1)]
)
emit("order_eth_mainnet", order_action, 0, None, None, True)
emit("order_eth_testnet", order_action, 0, None, None, False)

# Vector 3: order with cloid
order_request_c = {
    "coin": "ETH",
    "is_buy": True,
    "sz": 100,
    "limit_px": 100,
    "reduce_only": False,
    "order_type": {"limit": {"tif": "Gtc"}},
    "cloid": Cloid.from_str("0x00000000000000000000000000000001"),
}
order_action_c = order_wires_to_order_action(
    [order_request_to_order_wire(order_request_c, 1)]
)
emit("order_with_cloid_mainnet", order_action_c, 0, None, None, True)
emit("order_with_cloid_testnet", order_action_c, 0, None, None, False)

# Vector 4: dummy with vault
VAULT = "0x1719884eb866cb12b2287399b15f7db5e7d775ea"
emit("dummy_with_vault_mainnet", dummy_action, 0, VAULT, None, True)
emit("dummy_with_vault_testnet", dummy_action, 0, VAULT, None, False)

# Vector 5: scheduleCancel (basic, no time)
schedule_cancel = {"type": "scheduleCancel"}
emit("schedule_cancel_mainnet", schedule_cancel, 0, None, None, True)
emit("schedule_cancel_testnet", schedule_cancel, 0, None, None, False)

# --- New 7 vectors: expiresAfter coverage (Issue #2) ---

# Vector 6: dummy action + expires_after (simplest case, both networks)
emit("dummy_expires_after_mainnet", dummy_action, 0, None, 1700000000000, True)
emit("dummy_expires_after_testnet", dummy_action, 0, None, 1700000000000, False)

# Vector 7: order + expires_after, the actual shape run_twap signs (IOC order)
order_request_ioc = {
    "coin": "ETH",
    "is_buy": True,
    "sz": 100,
    "limit_px": 100,
    "reduce_only": False,
    "order_type": {"limit": {"tif": "Ioc"}},
    "cloid": None,
}
order_action_ioc = order_wires_to_order_action(
    [order_request_to_order_wire(order_request_ioc, 1)]
)
emit(
    "order_expires_after_mainnet",
    order_action_ioc,
    5,
    None,
    1700000000000,
    True,
)
emit(
    "order_expires_after_testnet",
    order_action_ioc,
    5,
    None,
    1700000000000,
    False,
)

# Vector 8: order with cloid + vault + expires_after combined — catches any
# ordering bug between the vault-flag byte and the expires-flag byte in
# action_hash's tail.
order_request_cve = {
    "coin": "ETH",
    "is_buy": False,
    "sz": 50,
    "limit_px": 2500,
    "reduce_only": False,
    "order_type": {"limit": {"tif": "Ioc"}},
    "cloid": Cloid.from_str("0x00000000000000000000000000000002"),
}
order_action_cve = order_wires_to_order_action(
    [order_request_to_order_wire(order_request_cve, 1)]
)
emit(
    "order_with_cloid_vault_expires_after_mainnet",
    order_action_cve,
    9,
    VAULT,
    1800000000001,
    True,
)
emit(
    "order_with_cloid_vault_expires_after_testnet",
    order_action_cve,
    9,
    VAULT,
    1800000000001,
    False,
)

# Vector 9: expires_after = 0 edge case — must still emit the flag byte + 8
# zero bytes, distinct from expires_after = None (which omits the flag
# entirely).
emit("dummy_expires_after_zero_mainnet", dummy_action, 1, None, 0, True)

json.dump(VECTORS, sys.stdout, indent=2, ensure_ascii=False)
sys.stdout.write("\n")
