# 運用ガイド

実際に資金を動かす前後の手順をまとめます。設計の背景は [DESIGN.md](DESIGN.md) を参照してください。

## 事前準備

### 1. Agent (API ウォレット) の登録

Hyperliquid の UI で API ウォレットを作成し、マスターアカウントに登録します。
本ツールが使うのは **Agent の秘密鍵**であり、マスターアカウントの鍵ではありません。

登録済みかどうかは以下で確認できます (読み取り専用の照会です)。

```bash
curl -s -X POST https://api.hyperliquid.xyz/info \
  -H 'Content-Type: application/json' \
  -d '{"type":"userRole","user":"<agent のアドレス>"}'
```

`{"role":"agent","data":{"user":"0x<master>"}}` が返れば登録済みです。
`{"role":"user"}` や `{"role":"missing"}` が返る場合は、まだ Agent として登録されていません
(マスターのアドレスを渡している可能性もあります)。

本ツールも起動時に同じ照会を行い、`agent` 以外なら何も送信せずに停止します。

### 2. 秘密鍵の受け渡し

秘密鍵は環境変数 `HL_AGENT_PK` からのみ読み込みます。コマンドライン引数では受け取りません。
シェル履歴や `ps` 出力に残さないため、パスワードマネージャ経由での注入を推奨します。

```bash
# pass を使う例
export HL_AGENT_PK=$(pass show hyperliquid/agent-pk)
```

`export` を直接タイプする場合、シェルによっては履歴に残ります。
`HISTCONTROL=ignorespace` を設定した上で先頭にスペースを入れる、あるいは上記のように
コマンド置換を使ってください。

任意で `HL_AGENT_ADDRESS` を設定しておくと、秘密鍵から導出したアドレスと照合され、
鍵の取り違えを起動時に検出できます。

### 3. 名目額上限の決定 (`--max-notional-usd`, Issue #3)

`--read-only false` (本番実行) では `--max-notional-usd` の指定が**必須**です
(0.1.0 からの破壊的変更)。1 スライスが超えてはならない最大 USD 名目額を、
想定する `--usd` / `--size` と `--slices` から逆算して設定してください。
上限は各スライス送信の直前に、そのスライスの実際の指値で再検証されます。

同様に `--slippage-bps` が 1000 bps を超える場合は `--allow-high-slippage` の
明示が必要です (10000 bps 以上は override 不可で無条件拒否)。詳細は
[USAGE.md の risk envelope 節](USAGE.md#risk-envelope-issue-3) を参照してください。

`HL_INFO_URL` / `HL_EXCHANGE_URL` を設定した状態で本番実行すると、既定では
起動を拒否します。テスト目的で意図的に上書きする場合のみ
`--allow-custom-endpoints` を指定してください (https:// のみ許可)。

### 4. 証拠金の確認

証拠金の判定は**取引所の応答を真値**とする方針です。事前チェックは行いません。

過去に `/info clearinghouseState` の `withdrawable` が `$0` を示していても、
UI 上は利用可能額があり実際に発注が通った、という事例があります。API の単一フィールドから
UI の「Available to Trade」を復元することはできません。

したがって残高は **Hyperliquid の UI で確認**してください。証拠金が本当に足りない場合は、
発注時に取引所が `Insufficient margin` を返し、本ツールが即座に停止します。

## 初回実行の手順

### ステップ 1: ドライランで計画を確認

```bash
hype-twap --symbol HYPE --side long --usd 1500 --duration 30m --slices 10
```

確認するポイント:

- `resolved symbol` の `sz_decimals` が想定どおりか
- `Rounded per-slice` の行で、1 スライスの量と合計が意図した範囲か
- `rounding dropped` の警告が出ている場合、その残余が許容できる大きさか
- 各スライスの想定価格が現在の板と整合しているか

### ステップ 2: 少額で本番実行

**初回は必ず少額から始めてください。** testnet での実発注検証は未実施であり、
Agent 署名注文に対する `orderStatus` の実挙動が唯一の未検証点です。

```bash
export HL_AGENT_PK=$(pass show hyperliquid/agent-pk)
hype-twap --symbol HYPE --side long --usd 50 --duration 5m --slices 2 \
  --max-notional-usd 60 --read-only false
```

失敗する場合も「安全側に停止する」設計ですが、実際の資金で挙動を確認してから
本来の金額に移行してください。

### ステップ 3: 本来の金額で実行

少額実行で問題がなければ、目的の条件で実行します。

```bash
hype-twap --symbol HYPE --side long --usd 1500 --duration 30m \
  --trigger-price 40 --trigger-when above --start-after 2h \
  --max-notional-usd 2000 --read-only false
```

## 実行中の監視

### ログ

既定では `info` レベルで、スライスごとの発注・約定が標準エラー出力に流れます。
詳細を見る場合は `RUST_LOG=debug` を設定します。

長時間の実行ではログをファイルに残しておくと、中断時の突き合わせが楽になります。

```bash
hype-twap ... --read-only false 2>&1 | tee twap-$(date +%Y%m%d-%H%M%S).log
```

### 中断したい場合

`Ctrl-C` でプロセスを終了します。IOC 注文は約定しなければ自動的に取り消されるため、
板に注文が残り続けることは通常ありません。ただし、**中断した時点までの約定は残ります**。

再実行する前に、必ず Hyperliquid 上の建玉と約定履歴を確認してください。
状態の永続化がないため、そのまま再実行すると同じ量を重ねて執行することになります。

## トラブルシューティング

### `unknown symbol '...' — not in the HL perp universe`

銘柄名が Hyperliquid の perp ユニバースに存在しません。大文字小文字を含め正確な名称を
指定してください。HIP-3 の `dex:SYMBOL` 形式は本ツールでは未対応です。

### `agent wallet is not registered` 系のエラーで起動しない

`userRole` 照会が `agent` 以外を返しています。マスターのアドレスを Agent として
渡していないか、API ウォレットの有効期限が切れていないかを確認してください。

### `HL_AGENT_ADDRESS` の不一致で起動しない

環境変数のアドレスと、秘密鍵から導出したアドレスが一致していません。
`HL_AGENT_ADDRESS` には **Agent のアドレス**を設定してください (マスターではありません)。
どちらが正しいか不明な場合は、`HL_AGENT_ADDRESS` を未設定にすれば照合はスキップされます。

### `per-slice notional $X is below the $10 minimum`

1 スライスあたりの名目金額が Hyperliquid の最低額を下回っています。
`--usd` / `--size` を増やすか、`--slices` を減らしてください。

### `live mode requires --max-notional-usd` で起動しない

`--read-only false` (本番実行) には `--max-notional-usd` の指定が必須です
(Issue #3、0.1.0 からの破壊的変更)。想定する名目額に見合った上限を指定してください。

### `--slippage-bps ... exceeds the warn threshold ...` で起動しない

`--slippage-bps` が 1000 bps を超えています。意図的な設定であれば
`--allow-high-slippage` を追加してください。10000 bps 以上は無条件で拒否され、
override はできません — タイプミスの可能性を疑ってください。

### `live mode + custom endpoint override (...) is rejected by default` で起動しない

`HL_INFO_URL` / `HL_EXCHANGE_URL` を設定した状態で本番実行しています。
意図的なテスト目的であれば `--allow-custom-endpoints` を追加してください
(https:// の URL のみ許可されます)。本番運用でこれらの環境変数を設定する
状況は通常ありません — 環境変数の設定ミスの可能性を疑ってください。

### 実行中に `insufficient margin` で停止した

取引所が証拠金不足と判断しました。入金するか、`--usd` / `--size` を減らして再実行してください。
再実行前に、それまでに約定した分を必ず確認してください。

### `book stale` で停止する

板のスナップショットが `--max-book-age-ms` (既定 3000ms) より古い状態が続いています。
ネットワークが不安定か、ローカルのシステム時刻が Hyperliquid のサーバー時刻より
進みすぎている可能性があります。NTP による時刻同期を確認してください。

一時的な回避として `--max-book-age-ms` を大きくすることもできますが、
古い板で価格を計算することになるため推奨しません。

### 「結果が不明」で停止した

`/exchange` への送信結果が確定できず、`orderStatus` による照合も失敗した状態です。
**推測で再実行しないでください。** Hyperliquid 上で約定履歴を確認し、実際に何枚約定したかを
把握してから、残量に対して再実行してください。

### トリガーが発火しない

`--trigger-when` の方向が意図と逆になっていないか確認してください。
`above` は「mid が閾値以上になったら」、`below` は「mid が閾値以下になったら」発火します。
起動時のログの `Trigger:` 行に条件が明示されます。

タイムアウトを設けたい場合は `--start-after` を併記してください (OR 条件で先勝ちです)。

## 既知の制約

- **状態の永続化がありません。** プロセスが途中で終了した場合、再開はできません。
  約定済みの分はそのまま残り、残りは単に執行されません。復旧は手動です —
  再実行する前に約定を確認しないと、重複して執行することになります
- **システム時刻に依存します。** nonce と板の鮮度チェックは、ある程度正確なシステム時刻を
  前提としています。NTP を動かしてください (板のタイムスタンプがローカル時刻より未来の場合は
  新鮮として扱うため、軽度のずれは許容されます)
- **HL のエラー文字列を部分一致で判定しています。** Hyperliquid が拒否メッセージの文言を
  変更する可能性があります。文言が変わっても実行は停止しますが、分類が汎用的な
  「取引所が拒否」という表現にフォールバックします
- **1 プロセス 1 銘柄です。** ポートフォリオ的な制御、既存ポジションの考慮
  (`reduce_only` は常に未設定)、HIP-3 の `dex:SYMBOL` 形式には対応していません
- **テイカー専用です。** すべてのスライスがスプレッドを越え、テイカー手数料を支払います
  (メイカー化は P1 として [issue #1](https://github.com/howlrs/hype-trigger-twap/issues/1) に起票済み)
- **testnet での実発注検証は未実施です。** Agent 署名注文に対する `orderStatus` の
  実挙動が唯一の未検証点です。初回は少額から始めてください

## 今後の予定 (P1)

**ベスト bid/ask 追従 (passive post-only)** — テイカー手数料とスリッページを削減するため、
ベスト bid (ロング) / ベスト ask (ショート) に ALO (post-only) 指値を置いて追従するモードを
追加します。実装方針と受け入れ条件は
[issue #1](https://github.com/howlrs/hype-trigger-twap/issues/1) に記載しています。

当面スコープ外: WebSocket による約定取得、実行の再開・永続化、複数銘柄の同時執行、
既存ポジションの考慮。
