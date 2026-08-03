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

```bash
export HL_AGENT_PK=0x<64桁の16進数>
hype-twap --symbol HYPE --side long --usd 1500 --duration 30m --read-only false
```

### 価格トリガー + タイムアウト

HYPE が $40 に到達したら開始。ただし 2 時間待っても到達しなければその時点で開始します。

```bash
hype-twap --symbol HYPE --side long --size 50 --duration 1h \
  --trigger-price 40 --trigger-when above --start-after 2h --read-only false
```

### ショート (testnet、20 スライス / 2 時間)

```bash
hype-twap --symbol ETH --side short --usd 5000 --duration 2h --slices 20 \
  --trigger-price 3000 --trigger-when below --network testnet --read-only false
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
| `--slippage-bps` | 数値 | `20` | IOC 指値に乗せるスリッページ余裕 (ベーシスポイント) |
| `--max-book-age-ms` | 整数 | `3000` | この時間より古い板スナップショットを拒否します。`0` は**鮮度チェックのみ**を無効化するもので、銘柄一致・正値・非交差・並び順といった意味検証と、未来方向 2秒固定の許容 (future-skew) は `0` でも常に適用されます |
| `--trigger-poll-secs` | 整数 | `2` | トリガー待ち中の板ポーリング間隔 (秒) |
| `--wait-network-grace` | 期間 (`30m`, `1h`) | `30m` | トリガー待ち中の連続ポーリング失敗 (通信エラーまたは空板) を許容する継続時間。最初の失敗時刻からの経過で判定し、1 回でも成功すればリセットします。`0` は不可 |
| `--expire-after` | 期間 | なし | 期間内にどのトリガーも発火しなければ、何も発注せずに終了します (exit code 3)。`--start-after` (フォールバック**開始**) とは異なり、こちらは打ち切り。同一 tick ではトリガーが優先。`0` は不可。`--start-after` 併用時は `expire_after > start_after` が必須。トリガー未指定 (即時開始) との併用は不可 |

`--size` と `--usd` はどちらか一方が必須です。両方指定または両方省略はエラーになります。

## 環境変数

| 変数 | 必須 | 意味 |
|---|---|---|
| `HL_AGENT_PK` | `--read-only false` のときのみ | Agent (API ウォレット) の秘密鍵。`0x` + 64 桁の16進数。**フラグでは受け取りません** — シェル履歴や `ps` 出力に残らないためです。`secrecy::SecretString` で保持し、エラーメッセージや `Debug` 出力を含め一切ログに出しません |
| `HL_AGENT_ADDRESS` | 任意 | **Agent (API ウォレット) のアドレス** — マスターアカウントではありません。設定した場合は `HL_AGENT_PK` から導出したアドレスと照合し、不一致なら起動を中止します |
| `HL_MASTER_ADDRESS` | 任意 | Agent が属するマスターアカウント。本番実行時は `userRole` 照会で自動解決されるため設定不要です。設定した場合は Hyperliquid の応答と照合し、不一致なら起動を中止します |
| `HL_INFO_URL` | 任意 | `/info` エンドポイントの上書き (テスト用) |
| `HL_EXCHANGE_URL` | 任意 | `/exchange` エンドポイントの上書き (テスト用) |
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
