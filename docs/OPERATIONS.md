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
(0.1.0 からの破壊的変更)。これは1スライスではなく、**同じ論理run全体の
累積 USD 名目額**の上限です。`--resume` 前の約定も累積に含まれます。
各注文の送信直前に、既約定額 + catch-up を反映した実注文数量 × 現在の指値を
再検証し、上限を超える注文は `/exchange` へ送る前に停止します。残余上限に
収まるよう注文数量を自動縮小はせず、runをhard-stopします。

`--resume` 時に取引所の terminal 応答へ `avg_px` が記録されていない場合、
Long は指値 (`Prepared.px`) を安全側の上限として使います。ALO maker は sideを
問わずresting指値が正確な約定価格なので、journalへ永続化した `Prepared.tif`
が `Alo` の場合だけ同じfallbackを使えます。Short IOC/GTCの売り指値は約定価格の
下限にしかならないため、価格不明の正数約定は推測せずfail-closedで再開を
停止します。旧journalはTIF不明として同じ安全側の扱いです。

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

### ステップ 2: funded testnet で Issue #16 を完了する

**mainnet live はまだ許可されていません。** 2026-09-04 に testnet の read-only
`meta` / unknown-oid `orderStatus` conformance は通過しましたが、funded account を
使う market/passive の実発注、ALO 拒否文言、cancel の `expiresAfter: null` は未検証です。
まず funded testnet account で、ごく少額の market smoke を実施します。

```bash
export HL_AGENT_PK=$(pass show hyperliquid/agent-pk)
hype-twap --symbol HYPE --side long --usd 50 --duration 5m --slices 2 \
  --network testnet --max-notional-usd 60 --read-only false
```

続いて Issue #16 の checklist に従い、passive の境界 cancel → settle → requote、
終了時 resting cancel、ALO 拒否文言、cancel の `expiresAfter: null` を同じ testnet
account で確認し、結果をこの文書へ記録してください。失敗する場合は mainnet へ
進まず、API との乖離を修正します。

### ステップ 3: mainnet 移行判定

Issue #16 の **全 checklist が実 API で完了し、その結果が文書化されるまで、
mainnet で `--read-only false` を実行しないでください。** 現在はこの gate が未完了のため、
mainnet live 用の実行コマンドは掲載しません。read-only で計画確認を続けるか、
明示的な `--network testnet` でのみ検証してください。

```bash
hype-twap --symbol HYPE --side long --usd 1500 --duration 30m --network testnet
```

## 実行中の監視

### ログ

既定では `info` レベルで、スライスごとの発注・約定が標準エラー出力に流れます。
詳細を見る場合は `RUST_LOG=debug` を設定します。

長時間の実行ではログをファイルに残しておくと、中断時の突き合わせが楽になります。

```bash
hype-twap ... --network testnet --read-only false 2>&1 | tee twap-$(date +%Y%m%d-%H%M%S).log
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
position-aware / zero-crossing 実行では、close 後の exact-zero 確認、open phase、
最終建玉確認、`FinalReport` 永続化まで同じ単一の猶予時間に含まれます。
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

> **重要:** 約定済み `Terminal` の削除・減額や、古いバックアップへの
> 巻き戻しは、再開時の累積名目額を過少評価させます。必ず Hyperliquid 側の
> 約定履歴と照合し、元ファイルを退避してから作業してください。

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
`--resume <run-id>` と `--master-address <master>`（または同じ値の
`HL_MASTER_ADDRESS`）を追加して再実行します。再開時は master も journal と
外部 API 呼び出し前に照合するため、明示が必須です。

```bash
hype-twap --symbol HYPE --side long --usd 1500 --duration 30m \
  --network testnet --max-notional-usd 5000 --read-only false --master-address 0x... \
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
  --network testnet --max-notional-usd 5000 --read-only false --master-address 0x... \
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
- **`<run-id>` は正しいが、実行条件が元の run と異なる場合**:
  `--resume` 限定のチェックです。run の未解決 cloid の `orderStatus` 照合を
  終えたあと (照合は安全のため常に先に行われ、条件の食い違いで
  スキップされることはありません)、journal の version 付き typed
  execution fingerprint と照合します。対象は request mode/value、sizing、
  duration、絶対deadline、symbol/side、slippage、cap、book freshness、
  settle retries、child algorithm/follow設定、network、agent/master、position
  phase/reduce-only です。相違field名を示して新規注文前に停止します。この場合も
  `orderStatus` 照合結果はjournalに残るので、**元と同じ条件で `--resume` を
  やり直す**か、
  続行するつもりがないなら `--abandon-incomplete-run` を使ってください。
  パラメータを意図的に変えて残りを続行する手段は用意していません —
  途中からスケジュールを変えて再解釈させることは事故のもとだからです。
  なお `--abandon-incomplete-run` は continuation 用 fingerprint チェックを行いません
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

## 観測イベント、metrics、alert hook

ジャーナル (`journal.jsonl`) が執行・resume・会計の唯一の正本です。観測用の
JSONL、Prometheus metrics、外部 alert はすべて best-effort であり、書込み・
DNS/TLS/connect/read timeout・5xx・hook 停止によって place/cancel/reconciliation
または終了コードが変わることはありません。

`src/observability.rs` のイベント契約は `schema_version: 1` の JSON Lines です。
同一 event stream 内で `sequence` は 1 から単調増加します。破壊的な変更は既存
version を変更せず、新しい schema version を導入してください。`run_started`、
`preflight_completed`、`slice_prepared/submitted/acknowledged/terminal`、`fill`、
`cap_near`、`reconciliation`、`execution_failed`、`run_stopped`、`final_report`、
`pair_leg_abnormal` が
閉じた語彙です。payload に秘密鍵、署名、認証 header、token、生の request/response、
完全な query URL、任意の error string を足してはいけません。read-only は event の
`mode: "read_only"` で live と区別します。read-only simulation で実際のJSONLを
確認する場合は `--event-jsonl /明示/path/events.jsonl` を指定します。この明示ファイル
以外のstate directoryやjournalは作られません。liveでは同フラグを使わず、run directory
内のsidecarを使用します。
`cap_near` はdurable terminal accounting後の残capがrun上限の10%以下になった時に
runごとに一度だけ発火します。各terminal後の `cap_remaining` と最終reportの値は
journal replayの保守的なaccounted notionalを使い、価格不明を実約定VWAPとは扱いません。
event をファイルへ保存する場合も `journal.jsonl` と別の sidecar にし、event
file の create/write/flush 失敗は記録・監視対象に留めて、正本 journal の fsync
や取引の成否へ伝播させません。

Prometheus は固定カーディナリティだけを公開します。run 状態、累積 filled size /
notional、cap 残額、API/reconciliation error、未解決注文、終了 reason と alert
drop/delivery failure を含みます。symbol、address、cloid、run_id、URL、error text は
label に禁止です。HTTP exposition は loopback (`127.0.0.1`/`::1`) bind を既定にし、
外部 interface への bind は明示的な opt-in を必要とします。外部公開する場合は
firewall/認証付き reverse proxy で保護し、metrics をインターネットへ直接出さないで
ください。

alert hook は既定 off です。有効化する実装では bounded queue と短い delivery timeout
を使います。queue 満杯は `hype_twap_alerts_dropped_total`、timeout/5xx は
`hype_twap_alert_delivery_failures_total` で検知し、トレード処理を待たせません。推奨
alert 条件は abort、`unresolved_orders > 0`、reconciliation error、`cap_reached`、
deadline、`pair_leg_abnormal` と watchdog の `ALERT:` ログです。

`dn-pair.sh` は `HL_ALERT_HOOK_URL` が設定されている場合、その値だけを
sanitizer の後に watchdog と各 leg の隔離環境へ private export します。hook は
argv、sanitizer の中間環境、manifest、event、launcher/leg log には書き込みません。
watchdog は片脚異常時に固定の `pair_leg_abnormal` JSON payload を curl で best-effort
送信します。remote URL は **HTTPS 必須**です。テスト受信機だけは literal loopback の
`http://127.0.0.1` / `http://[::1]` を許可します。userinfo、query、fragment は使えません。
送信は同時1件、connect 1 秒・全体 2 秒、redirect 無効です。curl 不在、URL 不正、timeout、
5xx、queue busy は警告だけで、watchdog の SIGTERM・exit 判定を変更しません。片脚異常の
containment として SIGTERM を送った後に両脚が消滅した場合は、異常を失わないよう watchdog
は非ゼロ終了します。両脚が自然に終了した場合は 0 です。

実行バイナリでは `--metrics-bind 127.0.0.1:9464`（または
`HL_METRICS_BIND`）で `/metrics` を有効化できます。non-loopback bind は
`--allow-external-metrics`（または `HL_ALLOW_EXTERNAL_METRICS=true`）を明示しない
限り拒否されます。alert hook は `HL_ALERT_HOOK_URL` のみで設定します。remote URL は HTTPS
のみで、literal loopback の `http://127.0.0.1` / `http://[::1]` だけがテスト用例外です。
userinfo、query、fragment は使えず、redirect は追跡しません。hook や metrics listener の
開始に失敗した場合は警告して disabled に落とすだけで、注文・cancel・resume の結果や
exit code は変えません。live run の sidecar は run directory 内の `events.jsonl` です。

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
  `HL_AGENT_PK` として環境変数で渡します)。この受け渡しでは秘密鍵を argv
  や `/proc/<pid>/cmdline` に載せません。2つの値が同一文字列の場合
`dn-pair.sh` は起動前に abort します。

各 leg の子プロセス環境は最小化されます。常に渡るのは `PATH`、`HOME`、`TMPDIR`
だけで、live の場合に限り該当 leg の `HL_AGENT_PK` と、設定されていれば
`HL_AGENT_ADDRESS` が追加されます。operator が明示設定した非空の値だけ、固定
allowlist の `SSL_CERT_FILE`、`SSL_CERT_DIR`、`HTTPS_PROXY` / `HTTP_PROXY` /
`ALL_PROXY` / `NO_PROXY`（および lowercase 版）、`RUST_LOG`、`RUST_BACKTRACE` も
leg に渡されます。任意の環境変数名を指定する仕組みはありません。generic
`HL_AGENT_PK` / `HL_AGENT_ADDRESS`、source の `*_LEG1` / `*_LEG2`、allowlist 外の値は
leg に継承されません。例外として `HL_ALERT_HOOK_URL` は allowlist とは別の private
値で、設定時にのみ sanitizer 後の各leg と watchdog に復元されます。

### live 実行前の極小 notional プローブを必須とする

本番の notional で立ち上げる前に、funded testnet で `--leg1-usd` / `--leg2-usd` を
最小 notional (例: $15〜$20 程度、per-slice が $10 の最小名目額を
上回る額) に絞った**プローブ運用**を必ず行ってください。極小 mainnet
プローブは過去に実バグ (orderStatus の avgPx 欠落による計上不備、
resume の二重執行など) をフルサイズ投入前に複数回捕捉した実績が
ありますが、現在は Issue #16 が未完了なので mainnet probe 自体を行ってはいけません。
全 checklist の完了を記録して gate を解除した後も、最初は極小額から始めます。

### `dn-pair.sh` の使用例

`dn-pair.sh` は **read-only が既定**です。本番実行は必ず `--live`、各脚の
明示 cap、別々の agent key を指定します。run ごとに mode 0700 の専用
directory と versioned `manifest.json` を作成し、PID だけでなく Linux の
starttime と executable identity を記録します。manifest やログに秘密鍵は入りません。
`--log-dir` は絶対パスで、symlink ではなく、起動ユーザー所有かつ mode 0700 の
root directory だけを受け付けます。

```bash
export HL_AGENT_PK_LEG1=$(pass show hyperliquid/agent-pk-eth)
export HL_AGENT_PK_LEG2=$(pass show hyperliquid/agent-pk-btc)

scripts/dn-pair.sh \
  --leg1-symbol ETH --leg1-side long  --leg1-usd 1000 \
  --leg2-symbol BTC --leg2-side short --leg2-usd 1000 \
  --duration 30m --slices 10 \
  --leg1-network testnet --leg2-network testnet \
  --leg1-child-algo follow --leg2-child-algo follow \
  --leg1-max-notional-usd 1200 --leg2-max-notional-usd 1200 \
  --live
```

同方向の脚、または既定の USD hedge ratio (1.0、許容差 100 bps) を外れる
構成は開始前に拒否されます。例外は監査対象の `--allow-same-side` /
`--allow-hedge-imbalance` に限られます。watchdog grace は既定 0 秒です。
`/proc/<pid>/stat` starttime と `/proc/<pid>/exe` を確認できない場合は
fail-closed で signal を送らず停止します。共通 start 公開後に片脚異常を検出した
launcher は、watchdog を先に止めず、verified な脚へ SIGTERM を送って最大10秒
待機・identity再確認します。残存脚があれば manifest は `start_failed_partial` のまま
非ゼロ終了し、watchdog を残して alert/containment を継続します。operator は建玉を
確認してください。

主なオプションは `--leg1-child-algo` / `--leg2-child-algo` (既定 `follow`)、
各脚の `--legN-max-notional-usd` (live では必須)、`--log-dir`、
`--watchdog-grace`、`--pair-barrier-timeout` (秒、既定30) です。
各脚はさらに `--legN-network` / `--legN-state-dir` / `--legN-trigger-price` /
`--legN-trigger-when` / `--legN-start-after` / `--legN-expire-after` /
`--legN-slippage-bps` / `--legN-max-book-age-ms` / `--legN-follow-poll-secs` /
`--legN-follow-repost-secs` / `--legN-follow-threshold-bps` /
`--legN-wait-network-grace` を独立指定できます。
脚別 `--legN-slippage-bps` は 0〜1000 bps を受け付けます。単体 CLI の
`--allow-high-slippage` に相当する危険域 override は pair launcher では公開しません。
`--read-only true` が既定で、鍵を使わないリハーサルです。標準出力の
`READ-ONLY / DRY-RUN` banner を確認し、実発注は明示した `--live` だけで行います。
旧 `--read-only false` は 0.1.x の移行期間だけ warning 付きで `--live` と同じ
扱いを維持し、次の breaking release (0.2.0) で削除します。新しい runbook や
automation では使用しないでください。unsafe override と非zero watchdog grace は
manifest に加えて `launcher.log` にも明示されます。

各実行は `<log-dir>/<run-id>/` (0700) を作り、そこに `manifest.json`、各脚の
ログ、`leg1.ready.json`/`leg2.ready.json`、`start.json` を保存します。manifest は
version 1 の正本で、PIDだけでなく `/proc/<pid>/stat` starttime と executable を
記録します。各Rust子は signer/agent/lock/meta/master/trigger/sizing/journal の
preflight後に ready を atomic に公開します。launcher は **両方の run_id とPIDが
一致する ready** を待ってから、共通の未来 Unix-ms を持つ `start.json` を atomic
公開します。timeout・ready不正・片脚死ではstartを公開せず、最初に起動した脚から
identity確認付きでrollbackします。これによりbarrier前の発注はありません。
watchdog は共通 start を公開する**前**に起動ログと PID identity の両方で health
check されます。watchdog 起動/health check 失敗時も start は公開されず、ready 済みの
両脚へ identity 確認付き rollback を行います。

pair run directory の `lifecycle.lock` は launcher の manifest 更新と `status` /
`stop` / `recover` を run 単位で直列化します。競合した操作は待機せず fail-fast
します。lock は launcher 終了時に解放され、watchdog・脚プロセスへは継承されません。
live leg は durable journal 作成後・barrier直前に ready file へ自分の正確な
`journal_run_id` を公開します。launcher は state directory 内のその一意IDと Header の
symbol/side を検証して manifest に保存し、shared state root の全走査や「最初の一致」を
行いません。read-only leg は journal を作らず空です。これにより同じ state root・同じ
symbol/side の同時 pair run でも mapping を混同しません。

### watchdog の意味論 (PID ベース監視)

`dn-watchdog.sh` は **PID ベースで監視**します
(`pgrep -cx` のようなプロセス名カウントは同一ホスト上の無関係な
`hype-twap` プロセスと干渉するため使用していません)。

- 1秒ごとに `kill -0`、`/proc/<pid>/stat` の starttime、`/proc/<pid>/exe` を
  3点一致で確認します。いずれかを読めない/一致しない場合は fail-closed で
  signal を送りません。
- **片方だけが生存している状態が `--grace` 秒 (既定0秒) 継続したら**、
  生存している方に SIGTERM を送ります。grace は自然完走時の両脚の
  終了タイミングのズレを吸収するためのもので、その間は生存側が発注を
  続けます (乖離の拡大は高々数スライス分の notional)。異常死への反応を
  速めたい場合は `--grace` を短くしてください。`hype-twap` は SIGTERM で
  resting 注文の cancel/settle まで行う graceful shutdown を実装済み
  なので、裸ポジションのまま放置されることを防ぎます。
- SIGTERM後も生存する場合は30秒ごとにalertを記録します。watchdog自身は
  identity未検証のPIDへKILLを送りません。

### 停止後の状態: ポジションは残る

プロセスを停止 (自然終了・SIGTERM いずれも) してもポジションそのものは
残ります。単なる `--usd` は常に**新規に注文する notional**であり、現在
建玉を考慮しません。解消・目標 exposure には position mode を使います。

```bash
# 読み取り専用: master の現在建玉、delta、phase を表示する
hype-twap --symbol HYPE --flatten --master-address 0x... --duration 5m --slices 5
hype-twap --symbol HYPE --target-sz -10 --master-address 0x... --duration 10m --slices 10 --json
# target-usd は preflight mid で一度だけ size 化し、以後再計算しない
hype-twap --symbol HYPE --target-usd 1000 --master-address 0x... --duration 10m --slices 10
```

`--flatten` は `clearinghouseState` の対象 perpetual symbol 一つだけを読み、
long なら short、short なら long の**最大 current size**を全 child order の
`reduce_only=true` で送ります。zero、取得失敗、symbol/精度不正は注文しません。
live flatten は、preflight が表示する `FLATTEN CONFIRMATION` token を同じ内容で
`--confirm-flatten` に渡すまで注文しません。token は network/master/symbol/
initial size/close side/max size/cap/algo/deadline に束縛されます。agent key は
live でのみ使い、read-only は `--master-address` または `HL_MASTER_ADDRESS` の
公開アドレスだけを使います。

token は再実行間で変わらない `--flatten-deadline-unix-ms` を指定して生成します。
read-only の同じ plan/deadline で token を確認し、live invocation に同じ deadline
と token を渡してください。この deadline は journal Header と全 order wire の
`expiresAfter`、ローカル期限に同一値で適用されます。

`--flatten --resume` でも元の `--confirm-flatten` token が必須です。未解決注文の
reconciliation/cancel は安全確定のため先に実行され得ますが、新規child注文は、
journal fingerprintの初期建玉・最大close量とHeaderの元deadlineから同じtokenを
再構築して一致確認するまで送信されません。部分約定後の小さい残量から別tokenを
作り直すことはありません。元のdeadlineとtokenを同じresumeコマンドへ渡してください。

token の直前には `FLATTEN PREFLIGHT` として、token に束縛される `network`、
`master`、`symbol`、`initial_szi`、`close_side`、`max_close_size`、
`max_notional_usd`、`child_algo`、`execution_deadline_unix_ms` を固定順で表示します。
秘密鍵・agent key・endpoint は表示しません。これは read-only の token 準備でも
同じです。`--json` は position plan の機械可読 stdout を維持するため、この人間向け
表示を出しません。

`--target-sz` / `--target-usd` は `target - current` だけを実行します。既存建玉を
減らす phase は reduce-only です。符号を跨ぐ target は close-to-flat が terminal、
未確定 cloid なし、最新 position が厳密に zero と確認できるまで open phase を
開始してはいけません。外部約定や reduce-only rejection を成功と推測せず、状態を
再取得して判断してください。失敗・中断時は journal を `--resume` で先に
reconcile し、手作業で反対注文を送らないでください。

### `--max-notional-usd` は総額判定であることへの注意

`--usd` を指定した場合、`hype-twap` の `--max-notional-usd` は
**per-slice ではなく総額 (執行全体の目標 notional) に対する判定**です。
そのため各脚の `--legN-max-notional-usd` は `--legN-usd` を安全に上回る
値として**明示指定**してください。launcherはlive modeのcap省略を拒否し、
自動補完は行いません。

### 停止方法

manifestを正本にする運用CLIを使ってください。`status` はhuman表示または
`--json` の機械可読表示、`stop` はPID+starttime+exe一致のプロセスだけへ
TERMを送り、指定timeout後も identity一致で生存するものは強制killせず
`stop_partial` として残します。
`recover` は死んだ/stale runを診断するだけで、spawn・発注・signal・manifest書換を
一切行いません。

```bash
scripts/dn-pair.sh status --run-id <run-id> --log-dir /absolute/path/to/pairs
scripts/dn-pair.sh status --run-id <run-id> --log-dir /absolute/path/to/pairs --json
scripts/dn-pair.sh stop --run-id <run-id> --log-dir /absolute/path/to/pairs --timeout 30
scripts/dn-pair.sh recover --run-id <run-id> --log-dir /absolute/path/to/pairs
```

`--run-id` は必須で、英数字から始まる `[A-Za-z0-9._-]` のみを受け付けます。
CLIは`<log-dir>/<run-id>/manifest.json`を自力で解決し、manifest_version=1かつ
内部のrun_id完全一致でなければfail-closedします。任意のmanifest pathは受け付けません。
manifestには各脚で明示した`state_dir`も保存されます。`recover`はこれを読むだけで
`hype-twap-runs verify/inspect` と同じvalidated replayにより、journal run id を
`VALIDATED-SUCCESS` / `VALIDATED-INCOMPLETE` / `VALIDATED-ABANDONED` / corruptとして
表示します。CLI不在・検証不能・壊れた/未終端journalは残存riskとして警告するので、exchangeの建玉と
注文を確認してから手動hedgeまたは通常の`--resume`手順を選んでください。

`pkill -x hype-twap` のような**プロセス名ベースの一括停止は非推奨**
です。同一ホスト上で動いている無関係な `hype-twap` プロセス
(別の運用・別のペア) まで巻き込んで停止させてしまいます。

`stop` はwatchdogも同じidentity検証で SIGTERM を送りますが、自動 SIGKILL はしません。
全記録 process が absent と再確認できた場合にだけ manifest を `stopped` にします。生存または identity mismatch が
残れば `stop_partial` と residual-risk を表示して非ゼロ終了します。`pkill -x` や裸のPIDへの
直接signalは、無関係な運用またはPID再利用を巻き込むため使用しません。

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

### cancel後の `orderStatus` が `open` / `unknownOid` のままになる

既知のresting注文をcancelした後のInfo index遅延です。待機予算は
`--settle-retries N` または `HL_SETTLE_RETRIES=N`（既定25、正の整数）で調整できます。
この値はcancel/settleだけに効き、ambiguous placeの再送回数は増やしません。
予算を使い切り、cancel acknowledgementが確認済みの場合だけ、cancel開始10秒前からの
`userFillsByTime`を照会し、oid/cloid/symbol/side/数量が一致するfillを採用します。
ledgerも確定根拠を返せなければ推測でzero扱いせずhard-stopするため、journalを検査して
`--resume`してください。

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

条件が成立しないまま待機を打ち切り、何も発注せず終了するタイムアウトを
設けたい場合は `--expire-after` を指定してください。`--start-after` は逆に、
指定時間が経過した時点で価格条件が未成立でも実行を**開始する**フォールバックです。
両方を併用する場合は `expire_after > start_after` となる値が必要です。

## 既知の制約

- **crash/restart の再開は `--resume` が前提です。** ジャーナルは注文送信の
  「前」に intent を fsync するため二重発注は防げますが、自動での続行は
  行いません — 「クラッシュ・再起動時の手順」節の通り、`--resume` または
  `--abandon-incomplete-run` を明示的に指定する必要があります
- **システム時刻に依存します。** nonce と板の鮮度チェックは、ある程度正確なシステム時刻を
  前提としています。NTP を動かしてください (板のタイムスタンプがローカル時刻より未来の場合は
  新鮮として扱うため、軽度のずれは許容されます)
- **trigger待機は単調クロック、executionは永続化したwall-clock deadlineです。**
  `--start-after` / `--expire-after` の待機は `tokio::time::Instant` を使います。
  execution開始時には `--duration` から絶対Unix-ms期限をjournalへ固定し、全注文の
  `expiresAfter` と `--resume` が同じ期限を引き継ぎます。サスペンドや再開によって
  元の論理execution windowが延長されることはありません。resume時の残りが元の
  1 slice interval 未満でも、1ms以上あれば残量をその短いwindowへ圧縮した最終
  continuationを許可しますが、各book retry・place・resend直前に同じ絶対deadlineを
  再確認します。境界ちょうど／境界後は reconciliation/cancel のみで、新規book取得・
  発注は行いません
- **HL のエラー文字列を部分一致で判定しています。** Hyperliquid が拒否メッセージの文言を
  変更する可能性があります。文言が変わっても実行は停止しますが、分類が汎用的な
  「取引所が拒否」という表現にフォールバックします
- **1 プロセス 1 銘柄です。** `--flatten` / `--target-sz` / `--target-usd` は
  その1つのstandard perpetualについて現在建玉を考慮し、解消区間を
  `reduce_only` にします。portfolio最適化とHIP-3 `dex:SYMBOL` position modeは
  対象外です
- **既定はテイカーですが、メイカー系モードも実装済みです。** `--child-algo market`
  (既定) はすべてのスライスがスプレッドを越え、テイカー手数料を支払います。
  `--child-algo passive` はベスト bid/ask に ALO (post-only) 指値を置きますが、
  スライス中の再クオートは行いません。`--child-algo follow` は passive に
  スライス中の板追従再クオートを加えたものです (`--follow-*` フラグで調整)。
  いずれもタイムアウト時のテイカー切り替えは行いません — 未約定分は次の
  スライスへ持ち越されるのみです
  ([issue #1](https://github.com/howlrs/hype-trigger-twap/issues/1))
- **testnet conformance は一部のみ確認済みです。** 2026-09-04 に公開 `meta` と
  unknown-oid `orderStatus` の応答shapeを実APIで確認済みです。funded account が必要な
  market/passive smoke、ALO拒否文言、cancel `expiresAfter: null` は未実施なので、
  mainnet live の前に Issue #16 の手順を完了し、初回は少額から始めてください
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

当面スコープ外: WebSocket による約定取得、複数銘柄のportfolio最適化、
funding/PnL最適化、leverageの自動変更。
(実行の再開・永続化は対応済みです — 「クラッシュ・再起動時の手順」を参照してください)
