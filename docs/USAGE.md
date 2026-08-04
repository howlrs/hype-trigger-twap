# 使い方

## インストール

```bash
cargo build --release
./target/release/hype-twap --help
```

Rust 1.91 以降が必要です。ビルド成果物は `target/release/hype-twap` に生成されます。

## コマンド例

### ドライラン (既定)

実際には発注せず、ライブの板から計算した「送信するはずだった注文」を表示します。

```bash
hype-twap --symbol HYPE --side long --usd 1500 --duration 30m
```

### 本番実行 (即時開始)

`--read-only false` (本番実行) では `--max-notional-usd` が **必須**です (Issue #3、
0.1.0 からの破壊的変更)。指定しないと起動時に拒否されます。詳細は
[「risk envelope (Issue #3)」](#risk-envelope-issue-3) を参照してください。

```bash
export HL_AGENT_PK=0x<64桁の16進数>
hype-twap --symbol HYPE --side long --usd 1500 --duration 30m \
  --max-notional-usd 2000 --read-only false
```

### 価格トリガー + タイムアウト

HYPE が $40 に到達したら開始。ただし 2 時間待っても到達しなければその時点で開始します。

```bash
hype-twap --symbol HYPE --side long --size 50 --duration 1h \
  --trigger-price 40 --trigger-when above --start-after 2h \
  --max-notional-usd 3000 --read-only false
```

### ショート (testnet、20 スライス / 2 時間)

```bash
hype-twap --symbol ETH --side short --usd 5000 --duration 2h --slices 20 \
  --trigger-price 3000 --trigger-when below --network testnet \
  --max-notional-usd 6000 --read-only false
```

## フラグ一覧

| フラグ | 型 / 値 | 既定値 | 意味 |
|---|---|---|---|
| `--symbol` | 文字列 | **必須** | 対象銘柄。`/info meta` で検証し、未知の銘柄なら何も送信せずに停止します |
| `--side` | `long` \| `short` | **必須** | 売買方向 |
| `--size` | 数値 (枚数) | `--usd` と排他 | 枚数で指定 |
| `--usd` | 数値 (USD) | `--size` と排他 | 名目金額で指定。トリガー発火時の mid で枚数に換算し、以降は固定 (後述の注意を参照) |
| `--duration` | 期間 (`30m`, `2h`) | **必須** | 執行ウィンドウ全体の長さ |
| `--slices` | 整数 | `10` | スライス数。1 スライスの間隔 = `duration / slices` |
| `--trigger-price` | 数値 | なし | 価格トリガーの閾値。`--trigger-when` の併記が必須です |
| `--trigger-when` | `above` \| `below` | なし | `above`: mid が閾値以上で発火 / `below`: mid が閾値以下で発火。**自動推定はしません** |
| `--start-after` | 期間 | なし | 指定時間の経過で発火。価格トリガーとは OR 条件 (先に成立した方が勝ち) |
| `--read-only` | `true` \| `false` | **`true`** | `true` は署名も送信も行いません |
| `--network` | `mainnet` \| `testnet` | `mainnet` | API エンドポイントと EIP-712 の `Agent.source` を同時に切り替えます (不整合が起きない設計) |
| `--slippage-bps` | 数値 | `20` | IOC 指値に乗せるスリッページ余裕 (ベーシスポイント)。**10000 bps 以上、または非正の指値になる値は無条件で拒否** (override 不可)。**1000 bps 超は `--allow-high-slippage` が必須** |
| `--allow-high-slippage` | フラグ | `false` | **unsafe override**。`--slippage-bps` が 1000 bps を超える場合に必須。10000 bps 以上の無条件拒否には効果なし |
| `--max-notional-usd` | 数値 (USD) | なし | `--read-only false` (本番) では**必須** (Issue #3、破壊的変更)。この上限は二段階で検証されます: (1) 事前検証 — `--usd` は要求額そのもの、`--size` は保守的な指値で概算した名目額を、実行開始前に上限と比較します。(2) 各スライス送信の直前 — そのスライスが実際に署名する指値 × スライス数量の名目額を、同じ上限と比較します。いずれの検証も上限超過なら `/exchange` への送信より前に停止します。`--read-only` では不要 (何も送信しないため) |
| `--allow-custom-endpoints` | フラグ | `false` | **unsafe override**。本番実行時に `HL_INFO_URL` / `HL_EXCHANGE_URL` の上書きを許可します。指定しても **https:// 以外の URL は拒否**されます (ローカルホストの mock サーバーを使うテスト経路のみ例外) |
| `--max-book-age-ms` | 整数 | `3000` | この時間より古い板スナップショットを拒否します。`0` は**鮮度チェックのみ**を無効化するもので、銘柄一致・正値・非交差・並び順といった意味検証と、未来方向 2秒固定の許容 (future-skew) は `0` でも常に適用されます |
| `--trigger-poll-secs` | 整数 | `2` | トリガー待ち中の板ポーリング間隔 (秒) |
| `--wait-network-grace` | 期間 (`30m`, `1h`) | `30m` | トリガー待ち中の連続ポーリング失敗 (通信エラーまたは空板) を許容する継続時間。最初の失敗時刻からの経過で判定し、1 回でも成功すればリセットします。`0` は不可 |
| `--expire-after` | 期間 | なし | 期間内にどのトリガーも発火しなければ、何も発注せずに終了します (exit code 3)。`--start-after` (フォールバック**開始**) とは異なり、こちらは打ち切り。同一 tick ではトリガーが優先。`0` は不可。`--start-after` 併用時は `expire_after > start_after` が必須。トリガー未指定 (即時開始) との併用は不可 |
| `--child-algo` | `market` \| `passive` | `market` | スライスごとの子注文アルゴリズム。`market` (既定・従来と同一挙動) は IOC 成行相当の指値を送信します。`passive` は板の反対側を跨がない ALO (post-only) 指値をベスト bid/ask に置きます。詳細は後述の「passive (post-only) モード」を参照 |

`--size` と `--usd` はどちらか一方が必須です。両方指定または両方省略はエラーになります。

## 環境変数

| 変数 | 必須 | 意味 |
|---|---|---|
| `HL_AGENT_PK` | `--read-only false` のときのみ | Agent (API ウォレット) の秘密鍵。`0x` + 64 桁の16進数。**フラグでは受け取りません** — シェル履歴や `ps` 出力に残らないためです。`secrecy::SecretString` で保持し、エラーメッセージや `Debug` 出力を含め一切ログに出しません |
| `HL_AGENT_ADDRESS` | 任意 | **Agent (API ウォレット) のアドレス** — マスターアカウントではありません。設定した場合は `HL_AGENT_PK` から導出したアドレスと照合し、不一致なら起動を中止します |
| `HL_MASTER_ADDRESS` | 任意 | Agent が属するマスターアカウント。本番実行時は `userRole` 照会で自動解決されるため設定不要です。設定した場合は Hyperliquid の応答と照合し、不一致なら起動を中止します |
| `HL_INFO_URL` | 任意 | `/info` エンドポイントの上書き (テスト用)。**本番実行では既定で拒否**され、`--allow-custom-endpoints` (かつ https://) が必要です (Issue #3) |
| `HL_EXCHANGE_URL` | 任意 | `/exchange` エンドポイントの上書き (テスト用)。`HL_INFO_URL` と同じ本番時の制限を受けます |
| `RUST_LOG` | 任意 | ログフィルタ。既定は `info` |

`--help` にも同じ内容が表示されます。

## 出力の読み方

### 起動時

```
=== READ-ONLY MODE: NO ORDERS ARE SENT ===
INFO hype_twap: resolved symbol symbol=HYPE asset_index=159 sz_decimals=2 network=mainnet
INFO hype_twap: initial mid symbol=HYPE mid=54.3265
Trigger: immediate
```

`Trigger:` 行にトリガー条件が明示されます。価格と時間の両方を指定した場合は
`Trigger: price above 40 OR after 2h (whichever comes first)` のように表示されます。

### 発火時とサイジング

```
Triggered: price above 40 (mid=40.12)
Rounded per-slice: 0.61 × 3 = 1.83 (~$99.42 at mid 54.3265) [requested $100 → 1.8407... HYPE at mid 54.3265]
WARN hype_twap: rounding dropped a residual below one slice tick dropped=0.0107...
```

丸めによって要求量に届かない残余がある場合は警告が出ます。この残余は最終レポートにも再掲されます。

### 各スライス

```
[READ-ONLY] would place: slice 1/3 long 0.61 HYPE @ 54.436 (IOC, cloid 0x019fc856..., mid 54.3265)
```

本番実行時は実際の約定量と価格が表示されます。

### 最終レポート

```
=== TWAP report ===
mode:            READ-ONLY (no orders were sent)
symbol/side:     HYPE long
target:          requested 1.8407... / adjusted 1.83
filled:          1.83
avg price:       54.4216...
slices:          3 executed / 0 skipped
elapsed:         30s
status:          complete
exit code:       0
```

- `target` の `requested` と `adjusted` が異なる場合、丸めで落ちた分が存在します
- 丸め落ちがある場合は `status` が `complete (against the adjusted target)` となり、
  `NOTE: rounding dropped ... at pre-flight` の行が追加されます
- 部分約定で終わった場合は `WARNING: partial fill — X of Y unexecuted` が出ます

## passive (post-only) モード

`--child-algo passive` (既定は `market`) は、各スライスで IOC 成行相当の指値の代わりに
ALO (Add Liquidity Only / post-only) 指値を発注します。taker 手数料とスリッページを
削減する代わりに、約定するかどうかは板の動き次第になります。

### 挙動

1. スライス境界ごとに l2Book を取得し、**Long はベスト bid、Short はベスト ask** に
   ALO 指値を置きます。price は market モードと**全く同じ** szDecimals / 5 有効数字の
   丸めロジックを再利用します (別ロジックは実装していません)。slippage は乗せません
   (post-only の意味上、taker になる方向へ寄せる理由がないため)。
2. market モードと異なり、そのスライスの**インターバル全体**を待ちます
   (即座に IOC の結果を確定させる market モードとは対照的です)。
3. 次のスライス境界に到達した時点で未約定残 (部分約定含む) があれば、
   `cancelByCloid` → `orderStatus(cloid)` の順で**確定約定量を取得してから**
   次のスライスの新しい指値を出します。この cancel→確定の手順は、v0.1 で IOC が
   意図せず resting した場合の回収ロジック (`recover_resting_fill` /
   `poll_terminal_status`) をそのまま再利用しています。**cancel 送信後に約定が
   すり抜けて着地する race** が起きても、`orderStatus` が返す真の約定量だけを
   信頼するため、過大計上・過小計上のいずれも起きません。
4. **同時に resting できる子注文は常に 1 件まで**という不変条件を構造的に
   維持します (Option 型で 1 件しか保持しない設計)。これは姉妹リポジトリの
   実装で経験した「cancel と place の競合で target を超過する」不具合
   (PR-D10) を構造的に防ぐためのものです。

### ALO 拒否は正常系

`badAloPxRejected` など、板を跨いで taker になってしまう ALO 指値は取引所に
拒否されます。**これはエラーではなく正常な結果として扱われます** — post-only の
意味論上、拒否されるのは「跨がずに置けなかった」だけであり、そのスライスは
ゼロ約定として skip され、不足分は次のスライスの catch-up サイジングに
自動的に繰り越されます。実行を中断することはありません。

### 執行ウィンドウ・シャットダウンとの関係

`--duration` によるハードカットオフ (`ExecutionDeadline`) は passive モードでも
同様に適用されます — 期限を過ぎたら**新規の指値は一切出しません**。ただし
resting 中の注文がある場合、それを cancel して確定させる cleanup は期限後でも
必ず実行されます。これは以下の**すべての終了経路**で保証されます:

- 正常終了 (全スライス完了)
- `--duration` 経過による中断
- `SIGINT` / `SIGTERM` によるシャットダウン

いずれの経路でも、resting のまま板に残る注文が発生しないことが受け入れ条件です。

### スコープ外

スライス**内**でのリアルタイム追従 (touch が動いた瞬間に即 cancel → 再quote)
は本バージョンでは実装していません。再quote はスライス境界でのみ行われます。

## risk envelope (Issue #3)

本番実行 (`--read-only false`) には損失上限のガードレールがあります。すべて
`/exchange` への最初の送信より前 (ネットワークアクセスより前) に検証され、
違反すると exit code 非ゼロで即座に停止します。ポリシーの定数は `src/risk.rs`
に一元化されており、CLI 検証とスライスループの双方がここを参照します
(重複した magic number は存在しません)。

- **スリッページ上限**: `--slippage-bps` が **10000 bps 以上**、または計算後の
  指値が **非正**になる場合は無条件で拒否されます (override 不可)。
  **1000 bps 超**は `--allow-high-slippage` の明示が必須です。
- **名目額上限**: 本番実行には `--max-notional-usd` が**必須**です
  (0.1.0 からの破壊的変更)。`--usd` は要求額そのもの、`--size` は
  トリガー時点の板から計算した保守的な指値で概算した名目額として、事前に
  検証されます。さらに**各スライス送信の直前にも**、そのスライスの数量 ×
  実際に署名する指値の名目額を、同じ上限と比較して再検証します —
  執行中に板が動いても、スライス単位の上限超過は常に検出されます。
- **エンドポイント上書きの制限**: 本番実行で `HL_INFO_URL` / `HL_EXCHANGE_URL`
  を設定していると、既定で起動を拒否します。`--allow-custom-endpoints` を
  指定した場合のみ上書きを許可しますが、その場合も **https:// の URL のみ**
  受け付けます (ローカルホストの `http://127.0.0.1` / `http://localhost` は
  自動テスト用の例外として許可されますが、実運用でこの例外が使われることは
  想定されていません)。
- **送信前サマリ**: 実行開始前に一度だけ、解決済みの network/エンドポイント・
  symbol・side・target・slippage・名目額上限をまとめた1行 (`Pre-send summary: ...`)
  が出力されます。

`--read-only` (既定) ではこれらのガードは適用されません — 何も送信しないため、
`--max-notional-usd` も不要です。ただしスリッページ上限とエンドポイント制限は
`--read-only` でも一部評価されます (`--slippage-bps` の妥当性検証はモード共通)。

## 終了コード

| コード | 意味 |
|---|---|
| `0` | 完了。ウィンドウを使い切った上での部分約定もこのコードで、レポートに警告が出ます |
| `1` | 中止。取引所の拒否、板の継続的な鮮度切れ、約定量の確定不能、起動時検証の失敗など |
| `2` | 引数エラー (clap による検証。`--size` と `--usd` の同時指定、`--trigger-when` の欠落など) |
| `3` | 期限切れ。`--expire-after` 指定時、期間内にどのトリガーも発火しなかった場合。何も発注していません。標準出力に `EXPIRED: no trigger fired within <dur>` が出ます |

## `--usd` 指定の注意

> `--usd` は**トリガー発火時点の mid で枚数を確定し、以降は固定**します。
> 「トリガー発火時点の mid」の意味は一つだけです。価格トリガーの場合、トリガー条件を
> 満たした検証済みスナップショットをそのまま再利用します (再取得はしません)。即時開始
> または時間のみのトリガー (価格条件なし) の場合は、サイジング直前に新規取得した
> pre-flight スナップショットを使います。いずれの経路でも USD → 枚数の換算は一度きりです。
> 執行中に価格が動いた場合、実際に約定する名目金額は指定した USD 額から乖離します。
> 正確な枚数が必要な場合は `--size` を使ってください。

これは「USD 換算型の固定数量 TWAP」であり、価格下落時に買い増す DCA (ドルコスト平均法) とは
異なる挙動です。多くの取引所のアルゴ発注ツールも同じ方式を採っています。
