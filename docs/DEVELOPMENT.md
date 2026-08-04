# 開発

## ビルドとテスト

```bash
cargo build --release
cargo test                                  # 単体 + 結合テスト (289 件)
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

この 4 つがすべてクリーンであることが、変更をコミットする際の最低条件です。

**通常のテストスイートはネットワークに触れません。** `/info` と `/exchange` は `mockito` でモックし、
スライスループは後述のトレイトシーム経由でスクリプト化した偽実装に対して検証しています。
唯一の例外が `tests/status_vocabulary_conformance.rs` の `#[ignore]` 付きスモークテストで、
実際の Hyperliquid API を叩きます (詳細は後述の「orderStatus ステータス表の更新手順」参照)。

## モジュール構成

| ファイル | 行数 | 役割 |
|---|---|---|
| `src/main.rs` | 1058 | CLI 定義 (clap)、起動シーケンス、事前検証 (Issue #2 のクロックずれ検証を含む)、終了コード |
| `src/twap.rs` | 3068 | スライスループ、サイジング、キャッチアップ、約定照合 (`ValidatedFill` 経由)、W1 unknownOid 安全再送ポリシー、`ExecutionDeadline` / `expiresAfter` / クロックずれ検証 (Issue #2)、レポート |
| `src/client.rs` | 2043 | Hyperliquid REST クライアント (`/info`, `/exchange`)、応答解析、再試行方針、`ValidatedMarketSnapshot` (板の検証境界)、`ORDER_STATUS_VOCABULARY` (status 語彙表)、`ValidatedFill` (約定の検証境界)、`expiresAfter` の署名・送信 (Issue #2) |
| `src/eip712.rs` | 370 | **移植物** — EIP-712 型定義、msgpack パック、action_hash (`expires_after` 引数を含む) |
| `src/trigger.rs` | 1041 | 価格・時間トリガーの待機ループ (`&dyn HlApi` シーム、`ValidatedMarketSnapshot` 検証込み) |
| `src/format.rs` | 359 | 価格・数量の丸め (szDecimals、有効数字、方向制御)、テイカー指値の算出 |
| `src/api.rs` | 306 | `HlApi` トレイト (テストシーム) と `ScriptedApi` (テスト用偽実装) |
| `src/types.rs` | 292 | `Side` / `Tif` / `Cloid` / `Symbol` / `OrderBook` などのドメイン型 |
| `src/signer.rs` | 274 | **移植物** — `Eip712AgentSigner` (alloy による署名、`expires_after` 引数を含む) |
| `src/errors.rs` | 129 | `HlError` と `RejectionKind` (拒否メッセージの分類) |
| `src/lib.rs` | 15 | 結合テストから内部モジュールを参照するためのライブラリターゲット |

テストの内訳 (Issue #2 で `ExecutionDeadline` / `expiresAfter` / クロックずれ検証のテストを追加):

| ターゲット | 件数 | 内容 |
|---|---|---|
| `src/lib.rs` (単体) | 216 | 純関数の算術、丸め、応答解析、署名、`run_twap` のループレベルテスト、`ValidatedMarketSnapshot` / `ValidatedFill` の検証、status 語彙の全件終端性テスト、トリガーの `ScriptedApi` テスト、`ExecutionDeadline` / クロックずれ検証の単体テスト、期限を跨ぐ仮想時刻テスト (Issue #2) |
| `src/main.rs` (単体) | 31 | CLI 引数の検証、`--help` の内容、起動シーケンス |
| `tests/exchange_parse.rs` | 18 | mockito による `/exchange` 応答の解析と再試行方針、`expiresAfter` の body/署名一致 (Issue #2) |
| `tests/reconcile_and_probe.rs` | 19 | 曖昧な送信の照合、`userRole` 照会、トリガーの end-to-end |
| `tests/signing_cross_check.rs` | 3 | Python SDK 由来の 17 ベクタ (元 10 + `expiresAfter` 系 7、Issue #2) に対する署名検証 |
| `tests/status_vocabulary_conformance.rs` | 2 | **`#[ignore]`** — 実 Hyperliquid API に対する orderStatus / meta の疎通・形状スモークテスト |

## 署名コアの扱い (重要)

`src/eip712.rs` と `src/signer.rs` は、親リポジトリ `diff-old-new/executor` からの**そのままの移植**です。
インポートパスの書き換え、`MockSigner` の削除、秘密鍵を伏せるためのエラーメッセージと `Debug` 実装の
変更以外、ロジックには一切手を入れていません。

`tests/signing_cross_check.rs` は Hyperliquid Python SDK が生成した 17 件のベクタに対して
署名パスを検証します。うち元の 10 件 (フィクスチャは `tests/fixtures/signing/known_vectors.json`
の先頭 10 件、親リポジトリとバイト単位で同一) は **絶対に手で編集しないこと**。
残り 7 件は Issue #2 (`expiresAfter` の署名) で追加したベクタで、同じく
`hyperliquid-python-sdk` の `sign_l1_action` を使って生成しています
(ローカルの venv に `pip install hyperliquid-python-sdk eth-account msgpack` した上で、
`scripts/gen_signing_vectors.py` と同じ流儀の生成スクリプトを実行 — dummy/order アクション ×
mainnet/testnet に加えて cloid + vault + expiresAfter を同時に使う組み合わせと
`expires_after=0` の境界値を含みます)。ベクタを追加する場合も、既存の 17 件を書き換えては
いけません — 新しいベクタを末尾に追記するだけにしてください。

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
8. **取引所応答は信頼境界として検証する** (Issue #7) — `filled` は `0 <= filled <= intent.sz` を
   満たし、`avgPx` は正の値でサイド別の指値内に収まること。`orderStatus` の `remaining` は
   `0 <= remaining <= origSz` を満たすこと。いずれかを満たさない応答は握りつぶさず即座に hard-stop
   すること (クランプ厳禁)
9. **`ExecutionDeadline` は送信の直前に再確認する** (Issue #2) — ループ先頭で一度確認するだけでは
   板取得や照合ポーリングの `await` 中に期限を越えるケースを取りこぼす。初回発注と resend の
   両方の直前で再確認すること。期限後も `orderStatus` 照会と cancel は許可し続けること
10. **`expiresAfter` は body と署名で必ず同じ値を使う** (Issue #2) — `/exchange` リクエストボディの
    `expiresAfter` フィールドと、署名対象の action hash に渡す `expires_after` 引数は、
    どのコードパスでも同一の値でなければならない。resend も run 開始時に決まった値を再利用し、
    新しい期限を発行しないこと

### NTP 依存とクロックずれの fail-closed ルール (Issue #2)

`expiresAfter` は wall-clock (Unix ms) を signed action hash に埋め込む値なので、実行ホストの
システムクロックが Hyperliquid のサーバー時刻から大きくずれていると、ローカルが意図した期限と
HL が実際に enforce する期限が一致しなくなります。これは「多少ずれても安全側に倒れる」類の
問題ではありません — ローカルの時計が遅れていれば意図より早く HL 側が失効させ、進んでいれば
意図より長く有効な注文を残してしまいます。

このツールは live モードの起動時に `/info l2Book` のサーバータイムスタンプとローカル時刻を
比較し、差が `MAX_CLOCK_SKEW_MS` (5000ms、`src/twap.rs`) を超えていれば **起動そのものを
拒否します** (fail-closed)。read-only では検証しません — 何も署名しないため実害がないからです。

**運用者への要求**: 実行ホストは NTP (chrony / systemd-timesyncd 等) でシステムクロックを
同期しておいてください。多くのクラウド VM は既定で有効です。自前のホストで動かす場合、
`timedatectl status` や `chronyc tracking` で同期状態を確認してから live モードを起動して
ください。クロックずれで起動が拒否された場合、まず NTP デーモンが動いているか、時刻同期の
オフセットが妥当かを確認してから再実行してください。

## orderStatus ステータス表の更新手順 (Issue #7)

`src/client.rs` の `ORDER_STATUS_VOCABULARY` (const配列 `(&str, bool)` のテーブル) が、
Hyperliquid が返しうる全 `orderStatus` 値を terminal (`true`) / non-terminal (`false`) に
分類した唯一の正とする一覧です。`OrderStatusFill::is_terminal()` はこの表を正規化
(小文字化・"cancelled"→"canceled" のスペル統一) して照合するだけで、表にない値は
**必ず non-terminal 扱いのまま fail-closed** になります (未知 status を terminal と
誤認すると、まだ約定しうる注文の約定量を確定値として採用してしまい、次スライス以降が
過剰発注になる — これが Issue #7 以前の実際のバグパターンでした)。

Hyperliquid が新しい status を追加した場合の更新手順:

1. 公式ドキュメント (https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint)
   で新しい status 文字列と、それが「注文が二度と約定しない」ことを意味するか
   ("terminal") を確認する。`*Rejected` (発注時点で拒否 = 生成されなかった) と
   `*Canceled` (取消済み) は基本的にすべて terminal。`open` / `triggered` のような
   「まだ生きている」状態のみ non-terminal
2. `src/client.rs` の `ORDER_STATUS_VOCABULARY` にエントリを追加する
   (`("newStatusName", true_or_false)`)
3. `client::tests::every_official_status_has_terminal_semantics` の該当リスト
   (`terminal` または `non_terminal` の配列) に同じ名前を追加する。このテストは
   `ORDER_STATUS_VOCABULARY.len()` とテスト内リストの合計件数を突き合わせるため、
   表に足してテストに足し忘れるとテスト自体が失敗する
4. `cargo test --lib client::` で確認し、`cargo test --all-targets` をフルで通す
5. 可能であれば `cargo test --test status_vocabulary_conformance -- --ignored --nocapture`
   を手動実行し、実 API との疎通が壊れていないことも確認する (このテストはドリフトを
   自動検出できるわけではない — 新 status を実際に踏むには本物の注文が必要なため。
   あくまで「この client が今のレスポンス形状をまだ正しくパースできるか」の疎通確認)

## orderStatus の read-after-write に関する注意 (Issue #7)

**Hyperliquid の `orderStatus` には、`/exchange` 送信直後の read-after-write 保証が
公式資料上どこにも明記されていません。** つまり `/exchange order` への POST が返ってきた
直後に同じ注文を `orderStatus` で照会しても、その注文がまだ HL 側で可視化されておらず
`unknownOid` が返る可能性がある、という前提を置く必要があります。

これは W1 (曖昧な送信の再送判定) にとって重要です。`/exchange` はべき等でないため、
POST がタイムアウト等で失敗した場合「実際に届いたが応答だけ失われた」のか
「本当に届いていない」のかを `orderStatus` で確認してから再送を判断しますが、
**`unknownOid` の 1 回の観測だけでは「本当に届いていない」ことの証明になりません**
(read-after-write 保証がない以上、たまたま可視化が遅れているだけかもしれない)。

そのため `src/twap.rs` の `reconcile_by_cloid` は単発の `unknownOid` では再送しません。
連続 `UNKNOWN_OID_MIN_CONSECUTIVE` (3) 回以上の `unknownOid` 観測が、
`UNKNOWN_OID_MIN_WINDOW` (2秒) 以上の壁時計時間にまたがって得られた場合のみ、
「HL は本当にこの注文を受け取っていない」と判断して新しい nonce での再送を許可します。
それ以外 — 観測回数不足、時間窓不足、途中で live/terminal な応答が混じった (streak が
リセットされる)、リトライ上限 (`UNKNOWN_OID_MAX_ATTEMPTS`) に達した — はすべて
outcome-unknown として hard-stop (exit code 1) します。二重約定は取り消せない一方、
hard-stop はオペレーターが Hyperliquid 上で実際の約定を確認してから再実行すれば
回復できるため、安全側に倒しています。

## 参考

- 設計の背景と理由: [DESIGN.md](DESIGN.md)
- 運用手順とトラブルシューティング: [OPERATIONS.md](OPERATIONS.md)
- 親リポジトリ (署名コアと TWAP アルゴリズムの出自): `diff-old-new/executor`
