# hype-trigger-twap

Trigger-gated TWAP execution for Hyperliquid perpetuals. A single Rust binary
(`hype-twap`) — no server, no WebSocket, no Python, no database.

It waits for a price and/or time trigger, then works a target quantity into the
market as evenly-spaced IOC (taker) slices, catching up whenever a slice
under-fills.

**Read-only is the default.** You must pass `--read-only false` before a single
order can be sent.

> 日本語のドキュメントは [`docs/`](docs/README.md) にあります
> ([使い方](docs/USAGE.md) / [仕組み](docs/DESIGN.md) /
> [運用ガイド](docs/OPERATIONS.md) / [開発](docs/DEVELOPMENT.md))。

## Install

```bash
cargo build --release
./target/release/hype-twap --help
```

## Usage

Dry run (the default) — prints the orders it *would* place, from the live book:

```bash
hype-twap --symbol HYPE --side long --usd 1500 --duration 30m
```

Go live, starting immediately:

```bash
export HL_AGENT_PK=0x<64 hex>
hype-twap --symbol HYPE --side long --usd 1500 --duration 30m --read-only false
```

Wait for HYPE to reach $40 before starting, but give up waiting after 2 hours
and start anyway:

```bash
hype-twap --symbol HYPE --side long --size 50 --duration 1h \
  --trigger-price 40 --trigger-when above --start-after 2h --read-only false
```

Sell into a falling market, in 20 slices over 2 hours, on testnet:

```bash
hype-twap --symbol ETH --side short --usd 5000 --duration 2h --slices 20 \
  --trigger-price 3000 --trigger-when below --network testnet --read-only false
```

## Flags

| Flag | Type / values | Default | Meaning |
|---|---|---|---|
| `--symbol` | string | *required* | Perp symbol. Validated against `/info meta`; an unknown symbol aborts before anything is sent. |
| `--side` | `long` \| `short` | *required* | Trade direction. |
| `--size` | decimal (coin) | XOR `--usd` | Quantity in coin units. |
| `--usd` | decimal (USD) | XOR `--size` | Notional, converted to coins at the trigger-time mid and then **fixed**. See the caveat below. |
| `--duration` | humantime (`30m`, `2h`) | *required* | Total execution window. |
| `--slices` | u32 | `10` | Slice count; interval = `duration / slices`. |
| `--trigger-price` | decimal | none | Price threshold. Requires `--trigger-when`. |
| `--trigger-when` | `above` \| `below` | none | `above`: fire when `mid >= price`. `below`: fire when `mid <= price`. Never inferred — omitting it with `--trigger-price` is an error. |
| `--start-after` | humantime | none | Also fire after this much time. OR'd with the price trigger. |
| `--read-only` | `true` \| `false` | **`true`** | `true` signs nothing and sends nothing. |
| `--network` | `mainnet` \| `testnet` | `mainnet` | Sets the API URLs *and* the EIP-712 `Agent.source` domain together. |
| `--slippage-bps` | decimal | `20` | Cushion on the IOC limit price. |
| `--max-book-age-ms` | u64 | `3000` | Reject a book snapshot older than this. `0` disables ONLY the max-age check — every book (trigger polls, pre-flight, each slice) still has to pass semantic validation (matching symbol, positive prices/sizes, uncrossed and correctly-ordered levels) and a fixed 2s future-timestamp tolerance, unconditionally. |
| `--trigger-poll-secs` | u64 | `2` | Poll interval while waiting for the trigger. |
| `--expire-after` | humantime | none | Terminate the wait, placing **nothing**, if no trigger condition fires within this duration. See "Trigger semantics" below. |
| `--child-algo` | `market` \| `passive` | `market` | Per-slice order algorithm. `market` (default, unchanged behaviour) sends an IOC taker limit. `passive` sends a post-only (ALO) limit resting at the best bid/ask instead. See "Child-order algorithms" below. |

## Environment variables

| Variable | Required | Meaning |
|---|---|---|
| `HL_AGENT_PK` | only when `--read-only false` | `0x` + 64 hex. **Never accepted as a flag** so it cannot land in shell history or `ps` output. Held in a `secrecy::SecretString` and never logged — not even in error messages or `Debug` output. |
| `HL_AGENT_ADDRESS` | optional | The **Agent** (API wallet) address — *not* your master account. If set, it is checked against the address derived from `HL_AGENT_PK` and the process refuses to start on a mismatch. |
| `HL_MASTER_ADDRESS` | optional | The **master** account your agent belongs to. Live mode discovers this automatically (see below), so you never *have* to set it; if you do, it is cross-checked against what Hyperliquid reports and a mismatch aborts startup. |
| `HL_INFO_URL` | optional | Override the `/info` endpoint. |
| `HL_EXCHANGE_URL` | optional | Override the `/exchange` endpoint. |
| `RUST_LOG` | optional | Log filter; defaults to `info`. |

`--help` lists all of these too.

## Agent registration and the `userRole` probe

At startup in live mode (never in read-only, which sends nothing), the tool
calls `/info userRole` with the address derived from `HL_AGENT_PK`. This does
two jobs:

1. **Fail fast on an unregistered key.** Hyperliquid refuses signed actions from
   a key that is not an authorized API wallet. Catching that at startup beats
   discovering it when the first order explodes.
2. **Resolve the master account.** HL books an agent's orders under its *master*,
   so `/info orderStatus` has to be queried with the master address. Querying
   with the agent address returns `unknownOid` for orders that genuinely exist —
   and the fill-recovery path would read that as "nothing filled" and over-order
   on every later slice.

If the probe reports anything other than `agent`, startup aborts with the role
it did see and nothing is sent.

## Trigger semantics

The price and time triggers are **OR'd, first one wins**:

- Only `--trigger-price` / `--trigger-when`: waits indefinitely for the price.
- Only `--start-after`: pure delayed start, no polling.
- Both: whichever condition is met first starts the run.
- Neither: starts immediately.

The fired condition is logged so it's unambiguous which one won.

While polling, a transport error (or an empty book — mid unavailable) is retried
inside the client and then tolerated — the loop simply tries again on the next
poll. Consecutive failures are tracked by TIME, not count: `--wait-network-grace`
(default 30m) is how long a failure streak may run, timed from the first
failure, before the wait hard-stops with a message naming how long it was blind
and the last error. The streak resets on a single successful poll. The wait
phase holds no position and no open order, so it can afford to ride out an
ordinary network blip instead of exiting after a handful of failed polls — a
persistently blind trigger still never sits silent forever, it just gets a
generous, configurable budget first. While waiting, an info-level heartbeat is
logged every 5 minutes so `RUST_LOG=info` (the default) never goes silent for
days; for a price wait it includes the current mid and deviation from the
threshold, and for a time-only wait (`--start-after` with no price condition)
it reports only elapsed/remaining time and never touches the network.

### `--expire-after`: bounded waits that give up without ordering

`--expire-after <duration>` adds an upper bound on the *wait* phase itself,
distinct from `--start-after`. `--start-after` is a fallback **start** — when
it elapses, the run begins anyway, price or no price. `--expire-after` is the
opposite: if the period elapses with no trigger condition having fired, the
process terminates having placed **nothing** — no TWAP ever starts. This is
useful for "only enter if the breakout actually happens, don't sit there
forever" setups where an unmet trigger should mean "abandon the plan," not
"do it anyway."

- Unspecified (the default): waits indefinitely, exactly as before —
  behavior is unchanged.
- On expiry: stdout prints exactly `EXPIRED: no trigger fired within <dur>`
  and the process exits with **code 3** (`0` = completed, `1` = aborted,
  `2` = usage error, `3` = expired). No `TwapReport` is printed, because the
  TWAP never started.
- **Same-tick priority: the trigger always wins.** Each iteration of the wait
  loop evaluates the time and price trigger conditions FIRST, then checks
  expiry. If the expiry deadline has been reached on the same tick a trigger
  condition is also satisfied, the trigger fires normally — expiry only ever
  terminates the wait when neither trigger condition fired that iteration.
- Clock basis: the same monotonic clock as `--start-after`, measured from the
  same instant the wait began. Does not advance while the process is
  suspended.
- Validation (fail-fast at startup, before any network call):
  - `--expire-after 0s` is rejected.
  - Combined with `--start-after`, `--expire-after` must be strictly greater
    than `--start-after` — otherwise `--start-after` would always fire first
    and the expiry could never be reached.
  - `--expire-after` with **no** trigger configured at all (neither
    `--trigger-price` nor `--start-after` — i.e. immediate start) is
    rejected: expiry is meaningless when the run starts immediately.

## Sizing, rounding, and the `--usd` caveat

> **`--usd` fixes the coin quantity at trigger time.** "Trigger-time mid" has
> exactly one meaning: for a price trigger, it is the *same already-validated
> book snapshot* that satisfied the trigger condition, re-used as-is — never
> re-fetched. For an immediate start or a time-only trigger (no price
> condition), it is the first pre-flight snapshot, fetched fresh right before
> sizing. Either way the USD figure is converted to coins exactly once, and
> that quantity is held constant for the rest of the run. If price moves during
> the window, the *executed notional will drift away from the number you typed*.
> Use `--size` if you need an exact coin quantity.

Before the first slice the tool prints the rounded plan, e.g.:

```
Rounded per-slice: 5 × 10 = 50 (~$1500 at mid 30) [requested $1500 → 50 HYPE at mid 30]
```

- Sizes round **down** to the symbol's `szDecimals`, so a slice can never
  overshoot its target.
- Prices snap to HL's grid (≤ 5 significant digits, ≤ `6 - szDecimals`
  fractional digits) in the direction that keeps the order marketable: **long
  rounds up, short rounds down.**
- The run stops before sending anything if a per-slice order would round to zero
  or fall below HL's ~$10 minimum notional.

During the run, a slice that falls under the $10 floor is **skipped and carried
into the next slice** rather than rejected. Because slice targets are
cumulative, the carry is automatic. If the *final* slice is still under the
floor, the residual is genuinely unexecutable and is reported as a warning.

The floor is checked against the **actual limit price of the order**, not the
mid, with 1% of headroom. This matters for shorts, whose taker limit sits *below*
the mid: a size worth exactly $10.00 at the mid can be worth $9.96 at the price
that reaches HL, which comes back as a fatal `MinTradeNtl` rejection. Gating on
the real price turns that hard stop into a cheap skip-and-carry.

### Rounding is always reported

Because per-slice sizes round down, the *adjusted* total can be smaller than what
you asked for — sometimes much smaller on a coarse `szDecimals`. Requesting 10.5
where the exchange only accepts whole units, over 3 slices, adjusts down to 9.

Filling all 9 is "complete" against that adjusted target, so the final report
never prints a bare `complete` when a shortfall exists. It says
`complete (against the adjusted target)` and adds:

```
NOTE:            rounding dropped 1.5 of requested 10.5 at pre-flight
```

The note is printed on every outcome — a partial fill or an abort does not make
the pre-flight shortfall any less real.

## Child-order algorithms

`--child-algo market` (the default) is unchanged from earlier releases: every
slice sends an IOC taker limit at `mid +/- slippage-bps`, which either fills
immediately or is cancelled by the exchange.

`--child-algo passive` trades taker fees and slippage for patience: each slice
places a post-only (**ALO**) limit resting *at the touch* — the best bid for a
long, the best ask for a short — with no slippage cushion, and then waits the
**full slice interval** rather than resolving immediately. The price is snapped
to HL's grid with the exact same rounding rules as the market algorithm.

What happens at each slice boundary:

1. If the previous slice's ALO is still resting (fully or partially unfilled),
   it is **cancelled**, and the true filled quantity is then read back from
   `orderStatus` — never assumed from what was resting a moment before. This
   closes a cancel/late-fill race: an order can fill in the split second
   between the cancel request and HL processing it, and only `orderStatus`
   after the cancel is trusted as ground truth.
2. Whatever filled (fully, partially, or not at all) is credited, and the
   shortfall is carried into the next slice's size via the same catch-up
   sizing market mode already uses — a partial fill is never lost or
   double-ordered.
3. A **new** ALO is placed for the current slice's (possibly caught-up) size,
   at the then-current touch.

At most **one** passive child order rests on the book at any time — a
placement is never sent while a prior one is still unresolved. This is a
structural invariant, not just a convention: it is what prevents a
cancel/place race from ever producing two live orders for the same slice's
quantity.

An ALO can be **rejected** by the exchange (e.g. `badAloPxRejected`) if the
touch moves between the snapshot and the send, which would otherwise turn the
order into a taker fill — the opposite of what post-only means. This is
treated as a **normal outcome**, not an error: the slice is skipped as a
zero-fill and its size is carried into the next slice's catch-up, exactly like
a below-minimum-notional skip. The run never aborts because of an ALO
rejection.

Passive mode still respects `--duration` exactly as market mode does: once the
run's `ExecutionDeadline` has passed, no *new* quote is placed. A resting order
is always cancelled and settled during final cleanup — on normal completion,
on hitting the duration deadline, and on `SIGINT`/`SIGTERM` — so a passive run
never leaves an order resting on the book after the process exits.

Re-quoting only happens at slice boundaries. If the touch moves mid-slice, the
resting order is left alone until the next boundary — there is no intra-slice
chasing in this release.

## Error handling

The distinction that matters most:

- **Transport failures on `/info`** (connection errors, HTTP 5xx, HTTP 429)
  retry with exponential backoff — 1s, 2s, 4s, then give up. These are pure
  reads, so retrying is free.
- **Transport failures on `/exchange`** are *not* retried. See below — the nonce
  makes an order non-idempotent, so a blind resend is unsafe.
- **Exchange rejections** (a top-level `{"status":"err"}` or a per-order
  `{"error": ...}`) are **never retried**. The run stops immediately. Retrying a
  rejected order risks duplicate fills, and a rejection means the exchange has
  already made its decision.

### Ambiguous order sends (`/exchange` reconciliation)

An order is signed against a nonce that Hyperliquid burns the moment it receives
the body. So if a `POST /exchange` fails *after* HL got it — a timeout, a dropped
response — resending the same signed body can only ever come back as a stale
nonce, while the original order may well have filled. The naive "retry 3 times"
policy would report a hard failure on an order that actually executed.

The tool therefore sends each `/exchange` request **exactly once** and recovers
using the **cloid** as an idempotency key (we choose it before signing, so it
survives a lost response):

1. On an ambiguous failure, query `/info orderStatus` for that cloid, as the
   master account.
2. **HL has the order** → adopt its state. No resend; the position is real and
   is counted once.
3. **HL returns `unknownOid`** → the order never landed, so re-sign with a
   **fresh nonce** and send again (bounded to 2 resends, each preceded by
   another reconciliation).
4. **Reconciliation itself keeps failing** → stop, and say plainly that the
   outcome is unknown. Guessing either way risks a duplicate fill, which cannot
   be undone. Check your fills on Hyperliquid before re-running.

A failed *cancel* is non-fatal — the `orderStatus` query that follows it is what
establishes the truth.

Only a **terminal** order status (`filled`, `canceled`, `rejected`, …) is ever
accepted as a final fill count. A still-`open` order can fill again in the next
millisecond, so adopting its count would under-report the fill and make every
later slice over-order.

Common rejections are classified into an actionable message (insufficient
margin, below minimum notional); anything unrecognized still stops the run and
reports HL's raw text.

If an IOC order unexpectedly rests, the tool cancels it and queries
`/info orderStatus` to learn what actually filled — waiting for a terminal
status, and crediting the fill at HL's reported `avgPx` rather than at the limit
price. If it *cannot* determine the filled amount, it stops rather than guess:
assuming zero would over-order on the next slice, and over-fills cannot be
undone.

### The execution window is hard

No slice is placed once `--duration` has elapsed — including the last one. Delay
accumulates through retries, stale-book refetches and fill recovery, and the
final slice is also the catch-up slice carrying every earlier shortfall, so
exempting it would let the *largest* order of the run fire the *furthest* outside
the window you asked for. A run that hits the deadline early stops and reports
the shortfall; a normal run reaches its last slice inside the window anyway.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Completed. A partial fill that ran to the end of its window also exits `0`, with a warning in the report. |
| `1` | Aborted — exchange rejection, persistent stale book, unrecoverable fill ambiguity, or a startup/validation failure. |

## Known limitations

- **Crash/restart recovery requires `--resume`.** A live run journals every
  order intent (fsynced, before the network send) to
  `$XDG_STATE_HOME/hype-twap` (or `~/.local/state/hype-twap`, or
  `--state-dir`). If the process dies mid-run, restarting the SAME command
  refuses to start until you either pass `--resume <run-id>` (reconciles
  every in-flight order via `orderStatus`, then continues — no
  double-placement) or `--abandon-incomplete-run` (force-reconciles and
  stops, without continuing). See docs/OPERATIONS.md for the runbook.
  SIGINT/SIGTERM are handled the same way: in-flight orders are reconciled
  and confirmed resting orders are cancelled before the process exits.
- **Wall-clock dependent.** Nonces and book-freshness checks assume a
  reasonably accurate system clock; run NTP. (A book timestamp *ahead* of local
  time is treated as fresh, so mild skew is tolerated.)
- **Timing flags use a monotonic clock, not wall-clock.** `--start-after`,
  `--duration`, and `--expire-after` are all timed off `tokio::time::Instant`
  (`CLOCK_MONOTONIC` on Linux), which does **not** advance while the system is
  suspended. On a laptop that sleeps, a run started with `--start-after 2h`
  will begin 2 hours of *awake* time after launch — if the machine suspends
  for an hour in between, the actual wall-clock start is delayed by that hour.
- **HL error strings are matched by substring.** Hyperliquid can reword its
  rejection messages at any time; a reworded message still stops the run, it
  just falls back to the generic "exchange rejected" wording.
- **One symbol per process.** No portfolio logic, no existing-position
  awareness (`reduce_only` is never set), no HIP-3 `dex:SYMBOL` prefixes.
- **Taker only.** Every slice crosses the spread and pays taker fees.
- **Single-host single-writer only.** A local advisory lock (keyed by
  `network + agent address`) makes a second live process for the same agent
  on the SAME host fail fast, before any order. It has no visibility across
  hosts — use one dedicated API wallet per trading process; see
  docs/OPERATIONS.md for the multi-process/multi-host safety boundary.

## Roadmap

**P1: passive best bid/ask following.** Post ALO (post-only) child orders at the
touch and re-quote as the book moves, falling back to a taker sweep only when a
slice is running out of time. This trades fill certainty for maker rebates and
is the single biggest cost improvement available to this tool.

Also out of scope for now: WebSocket fills, multi-symbol execution, and
existing-position awareness. (Run resume/persistence — previously listed
here — shipped: see "Known limitations" above and docs/OPERATIONS.md.)

## Development

```bash
cargo test                                  # unit + integration
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

`tests/signing_cross_check.rs` verifies the EIP-712 / msgpack signing path
against 10 vectors generated by the Hyperliquid Python SDK. If those fail, the
signing code is wrong and **no order it produces should be trusted** — the
msgpack field order in `src/eip712.rs` is load-bearing and must not be reordered
without regenerating the fixture.

The slice loop talks to Hyperliquid through the `HlApi` trait (`src/api.rs`), so
`run_twap` — the part that actually commits money — is tested end to end against
a scripted fake under virtual time, with no network and no clock-watching. Those
tests live in `twap::loop_tests` and pin the sequencing behaviour: the window
cut-off, fill accounting, min-notional carry, and the `/exchange` reconciliation
described above.

No test touches the network; `/info` and `/exchange` are mocked with `mockito`.

## License

Apache-2.0. See [LICENSE](LICENSE).
