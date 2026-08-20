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

`Ctrl-C` (SIGINT) または `SIGTERM` でプロセスに中断を要求します。
即座には終了しません — 以下の graceful shutdown 手順を踏みます。

1. 新しいスライスの発注を止める (次のスライスの送信直前でチェックされます)。
2. 送信済みで結果が未確定 (in-flight) の注文があれば `orderStatus` で照合する。
   本ツールが送信するのは常に IOC です。IOC は約定しなければ取引所側で
   即時キャンセルされる注文形式ですが、ごく稀に取引所からの応答が
   `resting` (板に乗った) として返ることがあり、その場合は本ツールが
   `cancelByCloid` を送ってから `orderStatus` で最終的な約定数量を
   確認します。この確認・キャンセル処理はシグナルの有無に関わらず
   既存の送信ロジックの一部として常に行われるため、シグナル到着時に
   in-flight だったスライスは、その処理が完了するまで待ってから
   次のステップに進みます。
3. 最終レポートをジャーナルに永続化してから終了する。

この一連の処理は `--shutdown-grace` (既定 60 秒) 以内に完了させます。
超過した場合は、未解決の注文を `outcome_unknown` としてジャーナルに記録した上で、
**非ゼロ終了コード**でプロセスを終了します — この場合は次項の crash/restart
手順に従ってください。

状態は `--state-dir` (既定 `$XDG_STATE_HOME/hype-twap` またはなければ
`~/.local/state/hype-twap`) 以下にジャーナルとして永続化されているため、
中断後の再実行は手動の建玉確認に頼らず `--resume` で安全に継続できます。
詳細は次項を参照してください。

## クラッシュ・再起動時の手順 (Issue #4)

**この節は本番実行 (`--read-only false`) にのみ関係します。** `--read-only`
(既定) はジャーナルを一切書き込まないため、再開の概念自体がありません。

### 何が起きているか

本番実行は、注文を送信する**前**に intent (symbol / side / 価格 / 数量 /
cloid) を `<state-dir>/runs/<run-id>/journal.jsonl` に `fsync` 付きで
追記してから `/exchange` へ POST します。応答を受け取ったら、その結果
(約定・resting・エラー) も追記されます。つまりプロセスが

- POST 送信前にクラッシュした場合 → ジャーナルには intent だけが残り、
  実際には送信されていません。
- POST 送信後・応答受信前にクラッシュした場合 → ジャーナルには
  「結果不明 (SubmittedUnknown)」として残ります。実際には HL が受理済みの
  可能性があります。
- resting 注文の確認後にクラッシュした場合 → ジャーナルには確認済みの
  状態が残ります。

いずれの場合も、**同じコマンドをそのまま再実行することはできません**。
同じ network + agent の組み合わせで未完了 (incomplete) のジャーナルが
見つかると、本ツールは新規の重複実行を拒否して起動時に停止します。

```text
an incomplete run (<run-id>) exists for this network+agent (state dir: ...);
refusing to start a new overlapping live run. Pass --resume <run-id> to
continue it, or --abandon-incomplete-run to force-reconcile and abandon it
(nothing further from that run will be executed).
```

### 破損したジャーナルファイルへの対応 (B4 / hardening)

未完了 run のスキャン (`find_incomplete_run`) は、`<state-dir>/runs/*/journal.jsonl`
を毎回読み直します。ここで二種類の「壊れたジャーナル」を区別します:

- **末尾の1行だけが途切れている場合 (通常のクラッシュ形状)**: `fsync` 前の
  最後の1レコード書き込み中にプロセスが死んだ、標準的な JSONL クラッシュの
  形です。それより前の行はすべて正常にパースされ、その run は通常どおり
  incomplete として検出されます。**エラーにはなりません** — 何もする必要は
  ありません。`--resume` / `--abandon-incomplete-run` で通常どおり続行して
  ください。
- **ファイルが1レコードもパースできない場合 (Header すら読めない)**:
  これは「クラッシュで途切れた」とは別の、真の破損です。この場合
  `find_incomplete_run` はスキップせず、起動時に **fail-closed でエラーに
  なります** — 破損に気づかず新しい live run を起動して、実は未解決のまま
  だった旧 run と並行して二重発注してしまう事故を防ぐためです。

破損エラーが出た場合の対応手順:

1. エラーメッセージに示された `<state-dir>/runs/<run-id>/journal.jsonl` を
   手で確認する (`cat` / `head`)。復旧可能な内容 (例えば単なる末尾破損に
   見えるが自動判定に引っかかった等) であれば手動で修復を試みる。
2. 本当に復旧不能と判断した場合は、そのファイル (またはディレクトリごと)
   を別の場所に退避し、この run_id が起動をブロックしなくなるようにする。
   ただし、その run が実際に何を送信済みだったかはこれ以降 **この
   ツールでは追跡できなくなる** — Hyperliquid 側の実際の約定/建玉を
   手動で確認すること。

### 手順 1: `--resume` で再開する (推奨)

エラーメッセージに表示された `<run-id>` を使い、**元と同じコマンドライン**に
`--resume <run-id>` を追加して再実行します。

```bash
hype-twap --symbol HYPE --side long --usd 1500 --duration 30m \
  --max-notional-usd 5000 --read-only false \
  --resume <run-id>
```

起動すると、そのジャーナルに記録されている未解決 (submitted/unknown) な
cloid をすべて `orderStatus` で照合してから、通常の実行フローを継続します。
すでにジャーナルに約定として記録されている分は再送されず、二重発注は
起こりません。

### 手順 2: 続行せず放棄する場合 — `--abandon-incomplete-run`

その run を再開せず打ち切りたい場合は `--abandon-incomplete-run` を指定します。
`<run-id>` は不要です — 対象は起動時に検出された、その network + agent の
未完了 run から自動的に特定されます。

```bash
hype-twap --symbol HYPE --side long --usd 1500 --duration 30m \
  --max-notional-usd 5000 --read-only false \
  --abandon-incomplete-run
```

**このフラグは照合を省略しません。** 内部的には `--resume` と同じ
`orderStatus` による強制照合を先に行い、結果をジャーナルに記録してから
run を `Abandoned` としてクローズします。「照合すらせず握りつぶす」手段は
意図的に用意していません。残りの未執行分量は実行されず、放棄したことを
示すメッセージが表示されて終了コード `0` で終わります。

### `--resume` と実行内容が食い違う場合

食い違いには 2 種類あり、それぞれ検出タイミングが異なります。

- **`<run-id>` 自体が一致しない場合**: `--resume <run-id>` に渡した ID が
  状態ディレクトリ上の未完了 run と一致しないと、`orderStatus` による
  照合を始める前に即座にエラーで停止します。エラーメッセージに表示された
  正しい `<run-id>` を使ってください。
- **`<run-id>` は正しいが、他のパラメータ (`--usd`/`--size`/`--duration`/
  `--slices`/`--slippage-bps`/`--max-notional-usd` など) が元の実行と
  異なる場合**: `--resume` 限定のチェックです。run の未解決 cloid の
  `orderStatus` 照合を終えたあと (照合は安全のため常に先に行われ、
  パラメータの食い違いでスキップされることはありません)、サイズ算出後に
  元の run のジャーナルへ記録された `plan_hash` と比較し、一致しなければ
  エラーで停止します。この場合も `orderStatus` 照合の結果はジャーナルに
  残ったままなので、**元と同じパラメータで `--resume` をやり直す**か、
  続行するつもりがないなら `--abandon-incomplete-run` を使ってください。
  パラメータを意図的に変えて残りを続行する手段は用意していません —
  途中からスケジュールを変えて再解釈させることは事故のもとだからです。
  なお `--abandon-incomplete-run` はこの `plan_hash` チェックを行いません
  — run を閉じるだけなので、渡した `--usd`/`--duration` 等がどんな値でも
  (照合完了後に) 正常に `Abandoned` としてクローズできます。

### 状態ディレクトリの場所を変える

複数の環境 (例: 複数ホスト、複数エージェント) で状態ディレクトリを
分離したい場合は `--state-dir <path>` を明示してください。
既定値は `$XDG_STATE_HOME/hype-twap`、`$XDG_STATE_HOME` 未設定なら
`~/.local/state/hype-twap` です。

未完了 run の検出は同一の状態ディレクトリ配下しかスキャンしないため、`--state-dir` で別パスを指定することは、そこに保存された run に対してこの保護を明示的にオプトアウトすることを意味します。

## 単一 writer ロックと nonce の運用境界 (Issue #5)

**この節も本番実行 (`--read-only false`) にのみ関係します。** `--read-only`
はロックも nonce の永続化も一切行わないため、以下のいずれも読み取り専用の
実行には影響しません。

### 何を保証し、何を保証しないか

本番起動時、`network + agent アドレス` をキーとする advisory file lock
(`flock` セマンティクス、`<state-dir>/locks/<key>.lock`) を、未完了 run の
検出・照合 ([Issue #4](#クラッシュ再起動時の手順-issue-4)) より**前**に
取得します。同じキーで 2 つ目の本番プロセスを起動すると、**注文を送信する前に**
起動が拒否されます。

```text
another live process already holds the writer lock for this network+agent
(lock file: ...). If that process is gone, its lock is released automatically
on process death (this is a real flock, not just the metadata file) — if you
are certain no other process for this network+agent is running, check the
metadata file for a stale PID before retrying. Nothing was sent by this
process.
```

同時にロック取得を試みてもプロセスは待機しません — 即座に失敗します
(fail-fast)。異なる agent、または `--read-only` の実行は影響を受けず、
並行して動作できます。

このロックには**明確な限界**があります:

1. **単一ホストのローカル `flock` です。** 複数ホスト (別サーバー、別コンテナ、
   NFS 越しなど) にまたがる二重起動は、このロックでは検知できません。
   ホストが異なれば OS レベルの `flock` は互いに見えないため、同じ
   `HL_AGENT_PK` を 2 台のホストで同時に走らせても、両方とも起動に成功して
   しまいます。
2. **したがって、本番運用の唯一の確実な境界は「1 trading process につき
   専用の API ウォレット (Agent) を割り当てること」です。** ホストや
   コンテナが複数あっても、各プロセスが異なる Agent 秘密鍵を使う限り、
   `network + agent` キーがそもそも重複しないため、ロックの有無に関係なく
   安全です。**同じ Agent 鍵を複数のホスト・複数のプロセスで共有する運用は
   行わないでください。**
3. nonce の高水位マーク (下記) も同じ理由で、単一ホスト内でのみ
   単調性を保証します。複数プロセスが同じ Agent の nonce 状態を共有する
   ような構成をもし将来許容するなら、**ローカルのファイルベース HWM を
   silent に上書きする形で共有してはいけません** — 専用の外部 nonce
   コーディネーター (複数プロセスから同時にアクセス可能な、単一の
   真値ソースとなる調停サービス) を別途導入し、明示的に置き換える設計が
   必須です。現在の実装はそのような外部コーディネーターを持たないため、
   複数プロセスでの nonce/HWM 共有は**サポート外**です。

### ロックのメタデータファイル (診断用)

ロックファイルと同じディレクトリに `<key>.meta.json` が書かれます。これは
**安全機構ではありません** — 実際の排他制御はあくまで `flock`
そのものが担っており、プロセスが (正常終了・クラッシュのいずれであれ)
消滅すれば OS が自動的にロックを解放します。メタデータファイルは、
「今どのプロセスがロックを保持しているか」を人間が調査するための
診断情報 (PID、開始時刻、plan summary) に過ぎません。

```json
{
  "pid": 12345,
  "started_at_unix_ms": 1735900000000,
  "run_id": null,
  "plan_summary": "HYPE Long usd=1500 slices=10 network=mainnet"
}
```

### stale lock の疑いがあるとき

ロック取得エラーが出たが、記録された PID のプロセスが実際には存在しない
(`ps -p <pid>` で見つからない) 場合、その古いロックはプロセス消滅時に
**すでに自動解放されています** — `flock` はファイルディスクリプタが
閉じられた時点で解放されるため、`.lock` ファイル自体は残っていても
排他状態は残りません。したがって「ロックが古いから手で消す」という操作は
通常不要です。

もしロック取得エラーが続く場合は、`.meta.json` の `pid` を確認し、
本当に別プロセスが生きているかどうかをまず疑ってください。それでも
解決しない場合 (例えば `.lock` ファイルの権限が壊れているなど) は
`.lock` / `.meta.json` を削除しても構いませんが、**その前に必ず**
[クラッシュ・再起動時の手順](#クラッシュ再起動時の手順-issue-4) の
`--resume` / `--abandon-incomplete-run` の手順で未完了ジャーナルを
先に照合・解決してください。ロックの取得はジャーナルの照合より前に
行われますが、ロックを取り除く操作そのものは照合を代行しません —
ロックが空いた状態で起動しても、`find_incomplete_run` による未完了検出は
引き続き働き、`--resume` か `--abandon-incomplete-run` を要求します。

### nonce の高水位マーク (HWM)

Hyperliquid への署名済みリクエストは nonce で追跡されます。本ツールは
プロセス内の `AtomicU64` に加えて、`<state-dir>/locks/<key>.nonce-hwm.json`
に永続化した高水位マークを保持し、次の nonce は常に
`max(現在時刻ms, HWM + 1)` として計算されます。これにより:

- **再起動をまたいで単調性が保たれます。** プロセスが再起動しても、
  以前のプロセスが最後に使った nonce より必ず大きい値から再開します。
- **システム時刻が後退しても単調性が保たれます。** NTP 補正などで
  ローカル時計が巻き戻っても、`HWM + 1` の下限がそれを吸収します。

HWM は nonce を発行するたびに `fsync` 付きで即座に永続化されます
(ジャーナルの「1 レコードごとに fsync」という方針と同じトレードオフです —
TWAP のスライス間隔は秒〜分単位のため、発行のたびに fsync してもスループット上の
問題にはなりません)。

### 手動での実死活検証 (kill テスト) について

自動テストでは実プロセスを kill する代わりに、モック API + テスト用の
シグナルチャネルでクラッシュ地点を再現しています (詳細は開発者向けドキュメント参照)。
運用担当者が手動で実プロセスの挙動を検証したい場合は、testnet 上で
**2 パターン**を分けて確認することを推奨します —
「シグナルによる graceful shutdown」と「本当のクラッシュ (SIGKILL)」は
挙動もジャーナルの終端状態も異なるため、混同しないでください。

**パターン A: graceful shutdown (`SIGTERM`/`Ctrl-C`)**

1. testnet かつ少額 (`--max-notional-usd` を小さく) で本番実行を開始する。
2. スライスが 1 〜 2 回発注されたところで `kill -SIGTERM <pid>` (または
   `Ctrl-C`) を送る。
3. プロセスが `--shutdown-grace` 以内に自発的に終了し、最終レポートと
   終了コードが表示されることを確認する。
4. `<state-dir>/runs/<run-id>/journal.jsonl` を `cat` し、最後のレコードが
   `FinalReport` で終わっていることを確認する — graceful shutdown は
   in-flight の注文を照合してから終わるため、`SubmittedUnknown` のまま
   宙ぶらりんの cloid は残らないはずです (もし残っていれば
   `--shutdown-grace` を超過したケースなので、次のパターン B と同じ手順で
   復旧してください)。
5. Hyperliquid 上の実際の約定・建玉と、ジャーナル上の `filled_sz` の合計が
   一致することを確認する。

**パターン B: 本当のクラッシュ (`kill -9` / SIGKILL)**

graceful shutdown を経由しないプロセス消滅 (電源断、OOM kill、`kill -9`
など) を再現します。

1. testnet かつ少額で本番実行を開始する。
2. スライスが 1 〜 2 回発注されたところで `kill -9 <pid>` を送る —
   シグナルハンドラは一切実行されず、プロセスは即座に消滅します。
3. `<state-dir>/runs/<run-id>/journal.jsonl` を `cat` し、`FinalReport` が
   **存在しない**こと (途中の `Prepared`/`SubmittedUnknown`/`Acknowledged`
   のいずれかで終わっていること) を確認する。
4. 同じコマンドをそのまま (フラグなしで) 再実行し、incomplete-run の
   エラーで正しく拒否されることを確認する。
5. `--resume <run-id>` で再実行し、未解決だった cloid が `orderStatus` で
   照合された上で残数量だけが執行されることを確認する。
6. Hyperliquid 上の実際の約定・建玉と、ジャーナル上の `filled_sz` の合計が
   一致することを確認する (パターン A の手順5と同じ最終確認)。

## デルタニュートラル2脚運用

`hype-twap` は「1プロセス=1銘柄」の設計を維持しますが、**脚ごとに専用の
agent (API) ウォレットを分けた複数プロセスの並行実行**はサポート対象の
運用パターンです。例えば ETH ロング × BTC ショートのような
デルタニュートラルペアを、2つの `hype-twap` プロセスで同時駆動できます。
`scripts/dn-pair.sh` はこのパターンをコード化したランチャーで、
`scripts/dn-watchdog.sh` と組み合わせて使います。

### 前提: agent ウォレットは脚ごとに専用のものを用意する

nonce の状態管理と単一 writer ロック (前節参照) は
**`network + agent アドレス`** をキーに行われます。2脚を同じ agent
ウォレットで動かすと、2つ目のプロセスが flock 競合により起動時点で
拒否されます (最悪の場合、1脚だけが片肺で走り続ける状態を招きます)。

- HL の agent ウォレット上限は **unnamed 1本 + named 3本** (マスター
  アカウントあたり)。2脚のデルタニュートラルであれば named を2本
  登録すれば足ります。
- 各脚の秘密鍵は `HL_AGENT_PK_LEG1` / `HL_AGENT_PK_LEG2` として
  `dn-pair.sh` に渡します (本ツール自体が読む環境変数は従来通り
  `HL_AGENT_PK` 1本のみで、`dn-pair.sh` が脚ごとに子プロセスへ
  `HL_AGENT_PK` として再エクスポートします)。2つの値が同一文字列の場合
  `dn-pair.sh` は起動前に abort します。

### live 実行前の極小 notional プローブを必須とする

本番の notional で立ち上げる前に、`--leg1-usd` / `--leg2-usd` を
最小 notional (例: $15〜$20 程度、per-slice が $10 の最小名目額を
上回る額) に絞った**プローブ運用**を必ず行ってください。極小 mainnet
プローブは過去に実バグ (orderStatus の avgPx 欠落による計上不備、
resume の二重執行など) をフルサイズ投入前に複数回捕捉した実績が
あります。フルサイズで初めて気づくのは手遅れです。

### `dn-pair.sh` の使用例

```bash
export HL_AGENT_PK_LEG1=$(pass show hyperliquid/agent-pk-eth)
export HL_AGENT_PK_LEG2=$(pass show hyperliquid/agent-pk-btc)

scripts/dn-pair.sh \
  --leg1-symbol ETH --leg1-side long  --leg1-usd 1000 \
  --leg2-symbol BTC --leg2-side short --leg2-usd 1000 \
  --duration 30m --slices 10 \
  --child-algo follow \
  --read-only false
```

主なオプション (すべて `--leg1-*`/`--leg2-*` は必須、他は任意):

- `--child-algo` (既定値 `follow`)
- `--max-notional-usd` (既定値: 各脚の `--usd` の1.2倍を自動計算。
  下記「`--max-notional-usd` は総額判定」を参照)
- `--log-dir` (既定値 `~/.local/state/hype-twap/logs`)
- `--read-only` (既定値 `false`。`true` ならリハーサルモードで鍵不要)

各脚は `setsid`+`nohup` で detach 起動され、ログは
`<log-dir>/dn-<symbol>-<side>.log`、PID は
`<log-dir>/dn-<symbol>-<side>.pid` に記録されます。起動後10秒で両PIDの
生存確認を行い、片方が死んでいればもう片方に SIGTERM を送ってから
異常終了します (裸ポジション防止)。両脚が健在なら
`scripts/dn-watchdog.sh` を続けて起動します。

### watchdog の意味論 (PID ベース監視)

`dn-watchdog.sh` は **PID ベースで監視**します
(`pgrep -cx` のようなプロセス名カウントは同一ホスト上の無関係な
`hype-twap` プロセスと干渉するため使用していません)。

- 5秒ごとに `kill -0` で両 PID の生存を確認します。可能であれば
  `/proc/<pid>/comm` が `hype-twap` であることも確認し、PID 再利用
  による誤爆を防ぎます。
- **片方だけが生存している状態が `--grace` 秒 (既定90秒) 継続したら**、
  生存している方に SIGTERM を送ります。`hype-twap` は SIGTERM で
  resting 注文の cancel/settle まで行う graceful shutdown を実装済み
  なので、裸ポジションのまま放置されることを防ぎます。
- SIGTERM 送信後、最大180秒待って生存していれば exit 1 で終了します。

### 停止後の状態: ポジションは残る

プロセスを停止 (自然終了・SIGTERM いずれも) してもポジションそのものは
残ります。フラット化する場合は以下の手順を踏んでください。

1. `/info clearinghouseState` で各脚の建玉数量を確認する。
2. 逆サイドの TWAP (本ツールを `--side` を逆にして再実行する、または
   手動成行) で解消する。

本ツールは `reduce_only` を使わない設計です。フラット化の数量は
**手動で厳密に建玉数量へ合わせる必要があります**(意図せず追加の
ポジションを積んでしまうリスクに注意)。

### `--max-notional-usd` は総額判定であることへの注意

`--usd` を指定した場合、`hype-twap` の `--max-notional-usd` は
**per-slice ではなく総額 (執行全体の目標 notional) に対する判定**です。
そのため `--max-notional-usd` には `--usd` そのものより大きい値を
設定する必要があります。`dn-pair.sh` は明示指定がなければ各脚の
`--usd` の1.2倍を自動計算しますが、意図と異なる場合は
`--max-notional-usd` を明示的に指定してください。

### 停止方法

`dn-pair.sh` 実行後に表示される (またはログディレクトリの `.pid`
ファイルに記録された) PID へ `kill -TERM` してください。

```bash
kill -TERM <leg1-pid> <leg2-pid>
```

`pkill -x hype-twap` のような**プロセス名ベースの一括停止は非推奨**
です。同一ホスト上で動いている無関係な `hype-twap` プロセス
(別の運用・別のペア) まで巻き込んで停止させてしまいます。

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

- **crash/restart の再開は `--resume` が前提です。** ジャーナルは注文送信の
  「前」に intent を fsync するため二重発注は防げますが、自動での続行は
  行いません — 「クラッシュ・再起動時の手順」節の通り、`--resume` または
  `--abandon-incomplete-run` を明示的に指定する必要があります
- **システム時刻に依存します。** nonce と板の鮮度チェックは、ある程度正確なシステム時刻を
  前提としています。NTP を動かしてください (板のタイムスタンプがローカル時刻より未来の場合は
  新鮮として扱うため、軽度のずれは許容されます)
- **タイミング系フラグは wall-clock ではなく単調クロックです。** `--start-after` /
  `--duration` / `--expire-after` はいずれも `tokio::time::Instant` (Linux では
  `CLOCK_MONOTONIC`) を基準に計測しており、システムサスペンド中は時刻が進みません。
  サスペンドするラップトップ等で運用すると、たとえば `--start-after 2h` は「起動後、
  実際に稼働していた時間で 2 時間後」に開始します — 途中で 1 時間サスペンドすれば、
  実際の開始時刻もその分だけ後ろ倒しになります
- **HL のエラー文字列を部分一致で判定しています。** Hyperliquid が拒否メッセージの文言を
  変更する可能性があります。文言が変わっても実行は停止しますが、分類が汎用的な
  「取引所が拒否」という表現にフォールバックします
- **1 プロセス 1 銘柄です。** ポートフォリオ的な制御、既存ポジションの考慮
  (`reduce_only` は常に未設定)、HIP-3 の `dex:SYMBOL` 形式には対応していません
- **既定はテイカーですが、メイカー系モードも実装済みです。** `--child-algo market`
  (既定) はすべてのスライスがスプレッドを越え、テイカー手数料を支払います。
  `--child-algo passive` はベスト bid/ask に ALO (post-only) 指値を置きますが、
  スライス中の再クオートは行いません。`--child-algo follow` は passive に
  スライス中の板追従再クオートを加えたものです (`--follow-*` フラグで調整)。
  いずれもタイムアウト時のテイカー切り替えは行いません — 未約定分は次の
  スライスへ持ち越されるのみです
  ([issue #1](https://github.com/howlrs/hype-trigger-twap/issues/1))
- **testnet での実発注検証は未実施です。** Agent 署名注文に対する `orderStatus` の
  実挙動が唯一の未検証点です。初回は少額から始めてください
- **単一ホスト内の単一 writer のみ保証します。** 同一ホスト・同一
  `network + agent` の二重起動は起動時ロックで検知しますが、複数ホストに
  またがる二重起動は検知できません。「単一 writer ロックと nonce の運用境界」
  節を参照し、trading process ごとに専用の API ウォレットを割り当ててください

## 今後の予定

**対応済み: ベスト bid/ask 追従 (passive post-only / follow)** — `--child-algo passive` で
ベスト bid (ロング) / ベスト ask (ショート) に ALO (post-only) 指値を置いて
テイカー手数料とスリッページを削減できます (境界のみの再クオート)。
`--child-algo follow` はさらにスライス中も板をポーリングし、touch が
`--follow-threshold-bps` 以上離れたら cancel→新 touch へ再掲示して追従します
(`--follow-poll-secs` / `--follow-repost-secs` で頻度制御)。
タイムアウト時のテイカー切り替えフォールバックは未実装です。詳細は README の
「Child-order algorithms」節、実装方針は
[issue #1](https://github.com/howlrs/hype-trigger-twap/issues/1) を参照してください。

今後の候補: タイムアウト時のテイカー切り替えフォールバック。

当面スコープ外: WebSocket による約定取得、複数銘柄の同時執行、既存ポジションの考慮。
(実行の再開・永続化は対応済みです — 「クラッシュ・再起動時の手順」を参照してください)
