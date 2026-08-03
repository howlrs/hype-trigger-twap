# 開発

## ビルドとテスト

```bash
cargo build --release
cargo test                                  # 単体 + 結合テスト (197 件)
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

この 4 つがすべてクリーンであることが、変更をコミットする際の最低条件です。

**ネットワークに触れるテストはありません。** `/info` と `/exchange` は `mockito` でモックし、
スライスループは後述のトレイトシーム経由でスクリプト化した偽実装に対して検証しています。

## モジュール構成

| ファイル | 行数 | 役割 |
|---|---|---|
| `src/main.rs` | 801 | CLI 定義 (clap)、起動シーケンス、事前検証、終了コード |
| `src/twap.rs` | 2048 | スライスループ、サイジング、キャッチアップ、約定照合、レポート |
| `src/client.rs` | 1151 | Hyperliquid REST クライアント (`/info`, `/exchange`)、応答解析、再試行方針 |
| `src/eip712.rs` | 370 | **移植物** — EIP-712 型定義、msgpack パック、action_hash |
| `src/trigger.rs` | 336 | 価格・時間トリガーの待機ループ |
| `src/format.rs` | 359 | 価格・数量の丸め (szDecimals、有効数字、方向制御)、テイカー指値の算出 |
| `src/api.rs` | 292 | `HlApi` トレイト (テストシーム) と `ScriptedApi` (テスト用偽実装) |
| `src/types.rs` | 287 | `Side` / `Tif` / `Cloid` / `Symbol` / `OrderBook` などのドメイン型 |
| `src/signer.rs` | 266 | **移植物** — `Eip712AgentSigner` (alloy による署名) |
| `src/errors.rs` | 129 | `HlError` と `RejectionKind` (拒否メッセージの分類) |
| `src/lib.rs` | 15 | 結合テストから内部モジュールを参照するためのライブラリターゲット |

テストの内訳:

| ターゲット | 件数 | 内容 |
|---|---|---|
| `src/lib.rs` (単体) | 138 | 純関数の算術、丸め、応答解析、署名、`run_twap` のループレベルテスト |
| `src/main.rs` (単体) | 21 | CLI 引数の検証、`--help` の内容、起動シーケンス |
| `tests/exchange_parse.rs` | 16 | mockito による `/exchange` 応答の解析と再試行方針 |
| `tests/reconcile_and_probe.rs` | 19 | 曖昧な送信の照合、`userRole` 照会、トリガーの end-to-end |
| `tests/signing_cross_check.rs` | 3 | Python SDK 由来の 10 ベクタに対する署名検証 |

## 署名コアの扱い (重要)

`src/eip712.rs` と `src/signer.rs` は、親リポジトリ `diff-old-new/executor` からの**そのままの移植**です。
インポートパスの書き換え、`MockSigner` の削除、秘密鍵を伏せるためのエラーメッセージと `Debug` 実装の
変更以外、ロジックには一切手を入れていません。

`tests/signing_cross_check.rs` は Hyperliquid Python SDK が生成した 10 件のベクタに対して
署名パスを検証します (フィクスチャは `tests/fixtures/signing/known_vectors.json`、
親リポジトリとバイト単位で同一)。

> **このテストが失敗した場合、署名コードが壊れています。生成された注文を一切信用しないでください。**

特に注意が必要な点:

- **msgpack のフィールド順序が意味を持ちます。** `src/eip712.rs` の構造体定義を並べ替えると
  action_hash が変わり、署名が無効になります。並べ替える場合はフィクスチャの再生成が必須です
- **`rmp_serde::to_vec_named` を使う必要があります。** 既定の `to_vec` は配列形式を出力するため、
  Python SDK の辞書パックと一致しません
- **価格・数量の末尾ゼロは除去します。** `Decimal::normalize()` を通さないと、
  数値として同一でも Hyperliquid に拒否されることがあります (実績あり)

## テストシーム (`HlApi`)

資金をコミットする `run_twap` は、Hyperliquid への呼び出しを `HlApi` トレイト (`src/api.rs`) 経由で
行います。これにより、ネットワークも実時刻も使わずに、シーケンス全体を検証できます。

```rust
pub trait HlApi {
    async fn fetch_l2_book(&self, ...) -> Result<OrderBook, HlError>;
    async fn place_order_once(&self, ...) -> Result<..., HlError>;
    async fn cancel_by_cloid(&self, ...) -> Result<(), HlError>;
    async fn fetch_order_status(&self, ...) -> Result<Option<OrderStatusFill>, HlError>;
    async fn fetch_order_status_by_cloid(&self, ...) -> Result<Option<OrderStatusFill>, HlError>;
}
```

`ScriptedApi` は応答をキューとして与え、呼び出し列を記録する偽実装です。
`#[tokio::test(start_paused = true)]` と組み合わせることで、仮想時刻上で長時間の実行を
即座に再現できます。

これらのテストは `twap::loop_tests` にあり、次の挙動を固定しています。

- 実行ウィンドウの打ち切り (最終スライスも例外にしない)
- 約定の計上が 1 回だけであること (二重計上の防止)
- 最低名目金額割れの skip と次スライスへの繰越
- `/exchange` の曖昧な送信に対する照合と再送

### 修正を入れるときの指針

`run_twap` とその周辺は、緑のテストをすり抜けたバグが実際に複数見つかった領域です。
挙動を変える修正を入れる場合は、**まず失敗するテストを書いてから**修正してください。

修正が実効的かどうかは、修正を一時的に戻して該当テストが落ちることを確認する
(ミューテーション検証) と確実です。

## 設計上の不変条件

変更時に壊してはいけない性質です。

1. **ドライランは何も送らない** — `--read-only true` (既定) のとき、署名・`/exchange` への POST・
   `orderStatus` 照会のいずれにも到達しないこと
2. **秘密鍵は出力されない** — ログ、エラーメッセージ、`Debug` 出力のいずれにも `HL_AGENT_PK` の
   値が現れないこと
3. **取引所の拒否は再試行しない** — 拒否は取引所の確定判断であり、再送は二重約定を招く
4. **`/exchange` は 1 回だけ送信する** — 再送は必ず新しい nonce での再署名を伴うこと
5. **数量は切り捨てる** — 目標量を超過しないこと
6. **終端ステータスのみを約定量として採用する** — `open` のまま計上しないこと
7. **`--duration` 経過後は発注しない** — 最終スライスも例外にしないこと

## 参考

- 設計の背景と理由: [DESIGN.md](DESIGN.md)
- 運用手順とトラブルシューティング: [OPERATIONS.md](OPERATIONS.md)
- 親リポジトリ (署名コアと TWAP アルゴリズムの出自): `diff-old-new/executor`
