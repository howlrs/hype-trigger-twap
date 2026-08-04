//! `hype-twap` — trigger-gated TWAP execution for Hyperliquid perps.
//!
//! Startup sequence (§4):
//! 1. validate args
//! 2. `/info meta` → asset index + szDecimals (unknown symbol aborts before
//!    any order can be sent)
//! 3. build the signer (skipped in read-only) and verify HL_AGENT_ADDRESS
//! 4. `/info l2Book` → mid
//! 5. log the trigger condition
//! 6. wait for the trigger → pre-flight sizing → TWAP loop

use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, ValueEnum};
use rust_decimal::Decimal;
use secrecy::SecretString;

use hype_trigger_twap::client::{HlClient, HlConfig, Network, Role, ValidatedMarketSnapshot};
use hype_trigger_twap::errors::HlError;
use hype_trigger_twap::format::human;
use hype_trigger_twap::risk::{pre_send_summary, RiskEnvelope};
use hype_trigger_twap::signer::{Eip712AgentSigner, Signer};
use hype_trigger_twap::trigger::{
    wait_for_trigger, TriggerConfig, TriggerOutcome, TriggerReason, TriggerWhen,
};
use hype_trigger_twap::twap::{
    check_clock_skew, compute_sizing, fetch_fresh_book, usd_to_coin, wall_clock_now_ms, ChildAlgo,
    TwapPlan, MIN_NOTIONAL_USD, READ_ONLY_BANNER,
};
use hype_trigger_twap::types::{Address, Side, Symbol};

/// F3: `long_about = None` makes clap drop the struct's doc comment, so the
/// environment contract would otherwise be invisible to `--help`. These are the
/// variables that decide whether the tool can trade at all, so they belong in
/// the help output rather than only in the README.
const ENV_HELP: &str = "\
ENVIRONMENT VARIABLES:
  HL_AGENT_PK         Required with `--read-only false`. The AGENT (API wallet)
                      private key, `0x` + 64 hex. Accepted ONLY from the
                      environment — never as a flag — so it cannot reach shell
                      history or `ps` output. Never logged, not even on error.

  HL_AGENT_ADDRESS    Optional. The AGENT (API wallet) address — NOT the master
                      account. If set, it is checked against the address derived
                      from HL_AGENT_PK and startup fails on a mismatch.

  HL_MASTER_ADDRESS   Optional. The MASTER account the agent belongs to. Live
                      mode discovers this automatically via the HL `userRole`
                      probe; if you also set it here, the two must agree or
                      startup fails.

  HL_INFO_URL         Optional. Override the /info endpoint (testing). In LIVE
                      mode this is rejected by default (Issue #3) unless
                      --allow-custom-endpoints is also passed, and even then
                      only an https:// URL is accepted.
  HL_EXCHANGE_URL     Optional. Override the /exchange endpoint (testing).
                      Same live-mode restriction as HL_INFO_URL above.
  RUST_LOG            Optional. Log filter; defaults to `info`.
";

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum SideArg {
    Long,
    Short,
}

impl From<SideArg> for Side {
    fn from(s: SideArg) -> Self {
        match s {
            SideArg::Long => Side::Long,
            SideArg::Short => Side::Short,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum WhenArg {
    Above,
    Below,
}

impl From<WhenArg> for TriggerWhen {
    fn from(w: WhenArg) -> Self {
        match w {
            WhenArg::Above => TriggerWhen::Above,
            WhenArg::Below => TriggerWhen::Below,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum NetworkArg {
    Mainnet,
    Testnet,
}

impl From<NetworkArg> for Network {
    fn from(n: NetworkArg) -> Self {
        match n {
            NetworkArg::Mainnet => Network::Mainnet,
            NetworkArg::Testnet => Network::Testnet,
        }
    }
}

/// Child-order algorithm for each slice (Issue #1).
///
/// `market` (default): unchanged pre-Issue-#1 behaviour — an IOC taker limit
/// at `mid +/- slippage-bps`.
///
/// `passive`: a post-only (ALO) limit resting at the best bid (long) / best
/// ask (short), waiting the full slice interval. See docs/DESIGN.md for the
/// cancel-then-settle reconciliation this mode performs at each slice
/// boundary and the in-flight-order cap that bounds it to at most one
/// resting child order at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
enum ChildAlgoArg {
    #[default]
    Market,
    Passive,
}

impl From<ChildAlgoArg> for ChildAlgo {
    fn from(a: ChildAlgoArg) -> Self {
        match a {
            ChildAlgoArg::Market => ChildAlgo::Market,
            ChildAlgoArg::Passive => ChildAlgo::Passive,
        }
    }
}

/// Trigger-gated TWAP for Hyperliquid perps.
///
/// Waits for a price and/or time trigger, then works the requested quantity
/// into the market as evenly-spaced IOC (taker) slices.
///
/// The private key is read ONLY from the HL_AGENT_PK environment variable —
/// never from a flag — so it cannot land in shell history or a process list.
#[derive(Debug, Parser)]
#[command(name = "hype-twap", version, about, long_about = None, after_help = ENV_HELP)]
struct Cli {
    /// Perp symbol, e.g. HYPE. Validated against /info meta before anything
    /// is sent; an unknown symbol aborts immediately.
    #[arg(long)]
    symbol: String,

    /// Trade direction.
    #[arg(long, value_enum)]
    side: SideArg,

    /// Quantity in coin units. Mutually exclusive with --usd.
    #[arg(long, conflicts_with = "usd")]
    size: Option<Decimal>,

    /// Notional in USD. Converted to a coin quantity at the mid observed when
    /// the trigger fires, and FIXED from then on — if price moves during the
    /// window the executed notional will drift from this number. Mutually
    /// exclusive with --size.
    #[arg(long)]
    usd: Option<Decimal>,

    /// Execution window, e.g. 30m, 2h.
    #[arg(long, value_parser = parse_duration)]
    duration: Duration,

    /// Number of slices; interval = duration / slices.
    #[arg(long, default_value_t = 10)]
    slices: u32,

    /// Price trigger threshold. Requires --trigger-when.
    #[arg(long, requires = "trigger_when")]
    trigger_price: Option<Decimal>,

    /// Fire when mid rises to/above (above) or falls to/below (below) the
    /// trigger price. Required with --trigger-price; never inferred.
    #[arg(long, value_enum, requires = "trigger_price")]
    trigger_when: Option<WhenArg>,

    /// Also fire after this much time elapses. Combined with --trigger-price
    /// the two are OR'd — whichever comes first wins. With neither set the
    /// run starts immediately.
    #[arg(long, value_parser = parse_duration)]
    start_after: Option<Duration>,

    /// Dry run. true (the DEFAULT) signs nothing and sends no orders; each
    /// slice prints the order it would have placed from the live book.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    read_only: bool,

    /// Selects both the API endpoints and the EIP-712 Agent.source domain
    /// ("a" mainnet / "b" testnet) — the two can never disagree.
    #[arg(long, value_enum, default_value = "mainnet")]
    network: NetworkArg,

    /// Slippage cushion for the IOC limit price, in basis points. Rejected
    /// unconditionally at or above 10000 bps or if it produces a non-positive
    /// limit price; above 1000 bps requires --allow-high-slippage (Issue #3).
    #[arg(long, default_value = "20")]
    slippage_bps: Decimal,

    /// Unsafe override: allow --slippage-bps above the 1000 bps warn
    /// threshold. Has NO effect on the unconditional >= 10000 bps hard cap or
    /// the non-positive-limit-price rejection — neither can be overridden.
    #[arg(long, default_value_t = false)]
    allow_high_slippage: bool,

    /// REQUIRED in live mode (`--read-only false`): the maximum USD notional
    /// any single slice may target. `--usd` is checked as the requested
    /// notional; `--size` is checked via a freshly computed conservative
    /// limit price. Re-checked before EVERY slice against that slice's
    /// actual order price. Not required in read-only mode (Issue #3,
    /// breaking change for live users — see docs/USAGE.md).
    #[arg(long)]
    max_notional_usd: Option<Decimal>,

    /// Unsafe override: allow HL_INFO_URL / HL_EXCHANGE_URL to be overridden
    /// in LIVE mode. Requires the override URL(s) to be https://. Read-only
    /// mode and tests are unaffected by this flag — the restriction it lifts
    /// only ever applies to live mode (Issue #3).
    #[arg(long, default_value_t = false)]
    allow_custom_endpoints: bool,

    /// Reject an l2Book snapshot older than this many ms. 0 disables the
    /// check. A negative age (HL clock ahead of ours) counts as fresh.
    #[arg(long, default_value_t = 3000)]
    max_book_age_ms: u64,

    /// Seconds between l2Book polls while waiting for the trigger.
    #[arg(long, default_value_t = 2)]
    trigger_poll_secs: u64,

    /// How long a consecutive trigger-poll failure streak (network error or
    /// empty book) may run before the wait hard-stops. Timed from the first
    /// failure, not a retry count; resets on any single successful poll. The
    /// wait phase holds no position, so it can afford to ride out ordinary
    /// network blips instead of exiting after a handful of failed polls.
    #[arg(long, value_parser = parse_duration, default_value = "30m")]
    wait_network_grace: Duration,

    /// Terminate the wait, placing NOTHING, if no trigger condition fires
    /// within this duration (Issue #8). Not a fallback start like
    /// `--start-after` — an unmet expiry means the run ends without ever
    /// starting the TWAP. Unspecified = wait indefinitely (unchanged
    /// default). If a trigger condition and the expiry are both satisfied
    /// on the same evaluated tick, the trigger wins. Exits with code 3 and
    /// prints `EXPIRED: no trigger fired within <dur>` on expiry.
    #[arg(long, value_parser = parse_duration)]
    expire_after: Option<Duration>,

    /// Root directory for run-state persistence (Issue #4). Defaults to
    /// `$XDG_STATE_HOME/hype-twap` if set, else `~/.local/state/hype-twap`.
    /// A read-only run never touches this — no directory is created, no
    /// journal is written. Live runs write `<dir>/runs/<run-id>/journal.jsonl`.
    #[arg(long)]
    state_dir: Option<std::path::PathBuf>,

    /// Resume a specific incomplete run by its run id (Issue #4). Every
    /// submitted/unknown cloid recorded in that run's journal is reconciled
    /// via `orderStatus` BEFORE the run continues; fills already credited in
    /// the journal are never re-requested. Mutually exclusive with starting
    /// a brand-new run when an incomplete run already exists for the same
    /// network+agent — one of `--resume` or `--abandon-incomplete-run` is
    /// required in that situation.
    #[arg(long, conflicts_with = "abandon_incomplete_run")]
    resume: Option<String>,

    /// Explicit confirmation to abandon an incomplete run for this
    /// network+agent WITHOUT resuming it (Issue #4). Every submitted/unknown
    /// cloid is still force-reconciled via `orderStatus` first — this flag
    /// never skips reconciliation, it only accepts that whatever remainder
    /// was not yet executed will NOT be continued.
    #[arg(long, default_value_t = false)]
    abandon_incomplete_run: bool,

    /// How long SIGINT/SIGTERM shutdown may spend reconciling in-flight
    /// orders and cancelling confirmed resting ones before giving up
    /// (Issue #4). Exceeding this exits non-zero with any still-unresolved
    /// cloid journaled as `outcome_unknown`.
    #[arg(long, value_parser = parse_duration, default_value = "60s")]
    shutdown_grace: Duration,

    /// Child-order algorithm for each slice (Issue #1). `market` (default)
    /// reproduces pre-Issue-#1 behaviour exactly: an IOC taker limit at
    /// mid +/- slippage-bps. `passive` places a post-only (ALO) limit at the
    /// best bid (long) / best ask (short) and waits the full slice interval;
    /// see README/docs/DESIGN.md for the semantics.
    #[arg(long, value_enum, default_value = "market")]
    child_algo: ChildAlgoArg,
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    humantime::parse_duration(s).map_err(|e| format!("invalid duration '{s}': {e}"))
}

impl Cli {
    /// §4 step 1: argument validation that clap cannot express.
    fn validate(&self) -> Result<(), String> {
        if self.size.is_none() && self.usd.is_none() {
            return Err("exactly one of --size or --usd is required".into());
        }
        if let Some(sz) = self.size {
            if sz <= Decimal::ZERO {
                return Err(format!("--size must be > 0, got {sz}"));
            }
        }
        if let Some(usd) = self.usd {
            if usd <= Decimal::ZERO {
                return Err(format!("--usd must be > 0, got {usd}"));
            }
        }
        if self.duration.is_zero() {
            return Err("--duration must be > 0".into());
        }
        if self.slices == 0 {
            return Err("--slices must be > 0".into());
        }
        // Issue #3: slippage bounds are enforced by the single risk-policy
        // module (src/risk.rs) — CLI validation and the twap.rs slice loop
        // both call into RiskEnvelope so the bounds can never drift apart.
        RiskEnvelope::validate_slippage(self.slippage_bps, self.allow_high_slippage)
            .map_err(|e| e.to_string())?;
        // Issue #3: live mode requires --max-notional-usd (breaking change,
        // documented in docs/USAGE.md / docs/OPERATIONS.md).
        RiskEnvelope::validate_max_notional_required(self.read_only, self.max_notional_usd)
            .map_err(|e| e.to_string())?;
        if self.trigger_price.is_some() != self.trigger_when.is_some() {
            return Err("--trigger-price and --trigger-when must be given together".into());
        }
        if let Some(px) = self.trigger_price {
            if px <= Decimal::ZERO {
                return Err(format!("--trigger-price must be > 0, got {px}"));
            }
        }
        if self.trigger_poll_secs == 0 {
            return Err("--trigger-poll-secs must be > 0".into());
        }
        if self.wait_network_grace.is_zero() {
            return Err("--wait-network-grace must be > 0".into());
        }
        if let Some(expire_after) = self.expire_after {
            if expire_after.is_zero() {
                return Err("--expire-after must be > 0".into());
            }
            if let Some(start_after) = self.start_after {
                if expire_after <= start_after {
                    return Err(
                        "--expire-after must be greater than --start-after (start would always fire first, making expiry unreachable)".into(),
                    );
                }
            }
            if self.trigger_price.is_none() && self.start_after.is_none() {
                return Err(
                    "--expire-after requires a trigger (--trigger-price or --start-after); it is meaningless with immediate start".into(),
                );
            }
        }
        Ok(())
    }

    fn trigger_config(&self) -> TriggerConfig {
        TriggerConfig {
            price: match (self.trigger_price, self.trigger_when) {
                (Some(px), Some(w)) => Some((w.into(), px)),
                _ => None,
            },
            start_after: self.start_after,
            poll_interval: Duration::from_secs(self.trigger_poll_secs),
            max_book_age_ms: self.max_book_age_ms,
            wait_network_grace: self.wait_network_grace,
            expire_after: self.expire_after,
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    match run().await {
        Ok(code) => code,
        Err(e) => {
            tracing::error!("{e}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<ExitCode, String> {
    run_with_cli(Cli::parse()).await
}

/// The body of `run()`, taking an already-parsed [`Cli`] rather than reading
/// `std::env::args()` itself. This split exists purely as a test seam: it
/// lets `#[cfg(test)]` drive a full pre-wait/post-trigger `run()` execution
/// against a mock HTTP server (via `Cli::try_parse_from` + `HL_INFO_URL`/
/// `HL_EXCHANGE_URL` env overrides, both of which `run()` already reads),
/// without needing `Cli::parse()` to read real process argv. `run()` itself
/// is unchanged in every other respect — same validation, same behavior.
async fn run_with_cli(cli: Cli) -> Result<ExitCode, String> {
    cli.validate()?;

    let symbol = Symbol::new(&cli.symbol);
    let side: Side = cli.side.into();
    let network: Network = cli.network.into();

    if cli.read_only {
        println!("{READ_ONLY_BANNER}");
    } else {
        tracing::warn!("LIVE MODE: orders WILL be sent to {network}");
    }

    // Issue #3: the RiskEnvelope is resolved HERE, immediately after CLI
    // validation and BEFORE any network access (including the endpoint
    // override check right below, and everything that follows). This is the
    // single point in the whole run where the risk policy is fixed for good —
    // later tasks that need to observe or extend the resolved risk config
    // (e.g. a run journal, or a passive/post-only build mode) should hook in
    // at this construction point rather than re-deriving it elsewhere.
    let risk = RiskEnvelope {
        slippage_bps: cli.slippage_bps,
        allow_high_slippage: cli.allow_high_slippage,
        max_notional_usd: cli.max_notional_usd,
    };

    // Issue #3: live + a custom HL_INFO_URL/HL_EXCHANGE_URL override is
    // rejected by default. This MUST run before any network access — it sits
    // ahead of the signer/client construction below, which is itself already
    // ahead of the first network call. Read-only is UNAFFECTED (the check
    // below is gated on `!cli.read_only`), which is what keeps the existing
    // mockito-based read-only test seam working unchanged.
    if !cli.read_only {
        for url in [
            std::env::var("HL_INFO_URL").ok(),
            std::env::var("HL_EXCHANGE_URL").ok(),
        ]
        .into_iter()
        .flatten()
        {
            RiskEnvelope::validate_endpoint_override(&url, cli.allow_custom_endpoints)
                .map_err(|e| e.to_string())?;
        }
    }

    // §4 step 3 (partial): build the signer before any network call so a bad
    // key fails fast. Read-only never touches the key at all.
    let signer: Option<Box<dyn Signer>> = if cli.read_only {
        None
    } else {
        let pk = std::env::var("HL_AGENT_PK").map_err(|_| {
            "HL_AGENT_PK is required when --read-only false (0x + 64 hex, env var only)".to_string()
        })?;
        let s = Eip712AgentSigner::from_secret(SecretString::new(pk.into()), network.is_mainnet())
            .map_err(|e| e.to_string())?;
        let derived = s.address();
        // HL_AGENT_ADDRESS is the AGENT's address (the API wallet), NOT the
        // master account. A mismatch means the wrong key is loaded.
        if let Ok(expected) = std::env::var("HL_AGENT_ADDRESS") {
            let expected_norm = expected.trim().to_lowercase();
            if expected_norm != derived.as_str() {
                return Err(format!(
                    "HL_AGENT_ADDRESS mismatch: env says {expected_norm}, key derives {derived}. \
                     HL_AGENT_ADDRESS must be the AGENT (API wallet) address, not the master account."
                ));
            }
            tracing::info!(agent = %derived, "agent address verified against HL_AGENT_ADDRESS");
        } else {
            tracing::info!(agent = %derived, "agent address (HL_AGENT_ADDRESS not set; unverified)");
        }
        Some(Box::new(s))
    };
    let agent_address = signer.as_ref().map(|s| s.address());

    // Issue #4: state-dir resolution and incomplete-run detection. Read-only
    // never creates a directory or touches the journal at all — this whole
    // block is gated on `!cli.read_only`, mirroring the endpoint-override
    // gate above. Live mode checks BEFORE any network call (fetch_meta is
    // the very next one below), so a blocked startup never wastes a request.
    let resolved_state_dir = hype_trigger_twap::journal::state_dir(cli.state_dir.as_deref());

    // Issue #5: single-writer advisory lock, keyed by network+agent_address.
    // Taken HERE — immediately after `resolved_state_dir` is known and
    // strictly BEFORE the incomplete-run detection block below — so that two
    // concurrent live processes for the same network+agent can never both
    // run reconciliation/resume concurrently: the acceptance criterion
    // "stale lock recovery does not skip reconcile" depends on this
    // lock-then-reconcile ordering (see `src/lock.rs`'s test
    // `stale_lock_scenario_does_not_bypass_incomplete_run_reconciliation`
    // in this file's test module for the regression this guards).
    //
    // Read-only is completely unaffected (no lock file, no lock directory,
    // nothing written) — gated on `!cli.read_only` exactly like every other
    // live-only block in this function, preserving the "read-only creates
    // nothing" invariant `read_only_creates_no_state_dir_or_journal_file`
    // already covers for the journal.
    //
    // The lock is held for the rest of this function's scope (and the
    // entire live run, since `_process_lock` is not dropped until
    // `run_with_cli` returns) by keeping the guard bound in an outer `let`;
    // Task 9 #1 (passive/post-only) runs entirely inside this same
    // `run_with_cli` body and needs no changes here — one live process still
    // equals one lock holder regardless of order style.
    let _process_lock = if !cli.read_only {
        let agent = agent_address.as_ref().ok_or_else(|| {
            "internal error: live mode must have an agent address by this point".to_string()
        })?;
        let lock_key = hype_trigger_twap::lock::lock_key(&network.to_string(), agent);
        let plan_summary = format!(
            "{symbol} {side:?} usd={} slices={} network={network}",
            cli.usd.map(|d| d.to_string()).unwrap_or_default(),
            cli.slices
        );
        let metadata = hype_trigger_twap::lock::LockMetadata::new(plan_summary);
        Some(
            hype_trigger_twap::lock::ProcessLock::acquire(
                &resolved_state_dir,
                &lock_key,
                &metadata,
            )
            .map_err(|e| e.to_string())?,
        )
    } else {
        None
    };

    if !cli.read_only {
        let incomplete = hype_trigger_twap::journal::find_incomplete_run(
            &resolved_state_dir,
            network.to_string().as_str(),
            agent_address.as_ref(),
        )
        .map_err(|e| format!("checking for an incomplete prior run failed: {e}"))?;
        if let Some(incomplete_run_id) = incomplete {
            let is_the_one_being_resumed =
                cli.resume.as_deref() == Some(incomplete_run_id.as_str());
            if !is_the_one_being_resumed && !cli.abandon_incomplete_run {
                return Err(format!(
                    "an incomplete run ({incomplete_run_id}) exists for this network+agent \
                     (state dir: {}); refusing to start a new overlapping live run. \
                     Pass --resume {incomplete_run_id} to continue it, or \
                     --abandon-incomplete-run to force-reconcile and abandon it \
                     (nothing further from that run will be executed).",
                    resolved_state_dir.display()
                ));
            }
            if let Some(requested) = &cli.resume {
                if requested != &incomplete_run_id {
                    return Err(format!(
                        "--resume {requested} does not match the incomplete run on disk \
                         ({incomplete_run_id}); pass the correct run id, or \
                         --abandon-incomplete-run to abandon {incomplete_run_id} instead."
                    ));
                }
            }
        } else if cli.resume.is_some() {
            return Err(format!(
                "--resume {} was given but no incomplete run exists for this network+agent \
                 (state dir: {})",
                cli.resume.as_deref().unwrap_or_default(),
                resolved_state_dir.display()
            ));
        }
    }

    let config = HlConfig::new(network).with_overrides(
        std::env::var("HL_INFO_URL").ok(),
        std::env::var("HL_EXCHANGE_URL").ok(),
    );
    let client = HlClient::new(config, signer).map_err(|e| e.to_string())?;

    // Issue #5: seed the durable nonce high-water mark, right after
    // `HlClient::new` and strictly BEFORE the first `/exchange` call could
    // ever happen (the first network call at all is `fetch_meta` directly
    // below, which never mints a nonce, but seeding here — before ANY
    // network call — keeps the ordering simple to audit). Read-only never
    // seeds anything: it has no signer and never calls `next_nonce`, and
    // seeding would touch disk under the state dir, violating the
    // read-only-creates-nothing invariant this whole function's live-only
    // blocks already preserve.
    if !cli.read_only {
        if let Some(agent) = agent_address.as_ref() {
            let key = hype_trigger_twap::lock::lock_key(&network.to_string(), agent);
            let hwm = hype_trigger_twap::lock::NonceHwm::load(&resolved_state_dir, &key)
                .map_err(|e| format!("failed to load nonce high-water mark: {e}"))?;
            client.seed_nonce(hwm);
        }
    }

    // §4 step 2: resolve the symbol. Unknown → abort with zero orders sent.
    let meta = client.fetch_meta().await.map_err(|e| e.to_string())?;
    let asset = meta.resolve(&symbol).map_err(|e| match e {
        HlError::UnknownSymbol(s) => format!(
            "unknown symbol '{s}' — not in the HL perp universe ({} symbols); nothing was sent",
            meta.universe.len()
        ),
        other => other.to_string(),
    })?;
    tracing::info!(
        symbol = %symbol,
        asset_index = asset.asset_index,
        sz_decimals = asset.sz_decimals,
        network = %network,
        "resolved symbol"
    );

    // §4 step 3 (rest), F1: resolve the MASTER account behind the agent key.
    //
    // HL books an agent's orders under its master, so every orderStatus query
    // must use the master address — with the agent address HL answers
    // `unknownOid` for orders that genuinely exist, and the fill-recovery path
    // would read that as "nothing filled". The probe doubles as a registration
    // check: an unregistered key is refused here, at startup, instead of
    // exploding on the first order.
    //
    // Read-only never probes: it places nothing, so it needs no master, and the
    // mode's contract is that it makes no calls a dry run does not require.
    let master: Option<Address> = match agent_address.as_ref() {
        None => None,
        Some(agent) => {
            let role = client
                .fetch_user_role(agent)
                .await
                .map_err(|e| format!("userRole probe for agent {agent} failed: {e}"))?;
            let master = match role {
                Role::Agent { master } => master,
                other => {
                    return Err(format!(
                        "agent {agent} is not registered with Hyperliquid as an Agent \
                         (role = {}). Authorize this address as an API wallet on the master \
                         account first, or set HL_AGENT_PK to a registered agent key. \
                         Nothing was sent.",
                        other.label()
                    ))
                }
            };
            // If the operator also declared the master, the two must agree —
            // a mismatch means the key belongs to a different account than
            // they think.
            if let Ok(declared) = std::env::var("HL_MASTER_ADDRESS") {
                let declared = declared.trim().to_ascii_lowercase();
                if !declared.is_empty() && declared != master.as_str().to_ascii_lowercase() {
                    return Err(format!(
                        "HL_MASTER_ADDRESS mismatch: env says {declared}, but HL reports agent \
                         {agent} belongs to master {master}. Nothing was sent."
                    ));
                }
            }
            tracing::info!(
                agent = %agent,
                master = %master,
                "userRole probe: agent registered; orderStatus will query the master"
            );
            Some(master)
        }
    };

    // Issue #4: resolve `--resume`/`--abandon-incomplete-run` reconciliation
    // HERE — immediately after `master` is known and strictly BEFORE the
    // trigger wait / any l2Book call below. Both flags force-reconcile every
    // submitted/unknown cloid from the PRIOR run via `orderStatus`; this
    // must happen before this process risks placing anything of its own
    // (resume) or before it declares itself done (abandon) — waiting until
    // after the trigger fires would let a long wait (or an immediate
    // trigger's sizing/pre-flight calls) run first, which is wrong for
    // abandon (nothing should happen at all) and unnecessarily late for
    // resume. Only `symbol`/`side`/`master` are needed for reconciliation
    // (`orderStatus` cross-check + the master-address query), so a minimal
    // plan fragment is built here rather than waiting for the full `TwapPlan`
    // (which needs sizing that has not happened yet).
    //
    // Ordering note: the `--resume` plan_hash consistency check (below, near
    // where the full `TwapPlan` and journal are opened) intentionally runs
    // AFTER this reconciliation, not before. Reconciliation itself never
    // risks a duplicate fill (it only queries `orderStatus`, never places),
    // so there is no safety reason to gate it on the plan matching — and
    // gating it the other way round would mean a `--resume` with a
    // mismatched plan leaves the prior run's cloids permanently unresolved
    // every time the operator retries with the wrong parameters. Running
    // reconciliation unconditionally first means the run's on-disk state
    // only ever gets MORE resolved, never less, regardless of which flags
    // the operator got wrong.
    if !cli.read_only && (cli.resume.is_some() || cli.abandon_incomplete_run) {
        let reconcile_plan = TwapPlan {
            symbol: symbol.clone(),
            side,
            asset_index: 0,
            sz_decimals: 0,
            per_slice: Decimal::ZERO,
            total_adjusted: Decimal::ZERO,
            total_requested: Decimal::ZERO,
            slices: 1,
            duration: Duration::ZERO,
            slippage_bps: cli.slippage_bps,
            max_book_age_ms: cli.max_book_age_ms,
            read_only: false,
            max_notional_usd: Decimal::MAX,
            agent: agent_address.clone(),
            master: master.clone(),
            child_algo: cli.child_algo.into(),
        };

        if let Some(resume_id) = &cli.resume {
            let mut j = hype_trigger_twap::journal::ExecutionJournal::open_existing(
                &resolved_state_dir,
                resume_id,
            )
            .map_err(|e| format!("--resume {resume_id}: failed to open journal: {e}"))?;
            reconcile_incomplete_run(&client, &reconcile_plan, &mut j)
                .await
                .map_err(|e| format!("--resume {resume_id}: reconciliation failed: {e}"))?;
        } else if cli.abandon_incomplete_run {
            let incomplete_id = hype_trigger_twap::journal::find_incomplete_run(
                &resolved_state_dir,
                network.to_string().as_str(),
                agent_address.as_ref(),
            )
            .map_err(|e| e.to_string())?
            .ok_or_else(|| {
                "--abandon-incomplete-run given but no incomplete run was found".to_string()
            })?;
            let mut j = hype_trigger_twap::journal::ExecutionJournal::open_existing(
                &resolved_state_dir,
                &incomplete_id,
            )
            .map_err(|e| format!("--abandon-incomplete-run: failed to open journal: {e}"))?;
            reconcile_incomplete_run(&client, &reconcile_plan, &mut j)
                .await
                .map_err(|e| format!("--abandon-incomplete-run: reconciliation failed: {e}"))?;
            j.record(&hype_trigger_twap::journal::JournalRecord::Abandoned {
                note: format!(
                    "operator passed --abandon-incomplete-run; run {incomplete_id} force-\
                     reconciled and closed without continuing"
                ),
            })
            .map_err(|e| e.to_string())?;
            println!(
                "Abandoned incomplete run {incomplete_id} after forced reconciliation; \
                 nothing further will be executed for it."
            );
            return Ok(ExitCode::SUCCESS);
        }
    }

    // §4 step 5 (moved ahead of step 4: the gate below needs to know whether
    // this run is time-only before it may touch l2Book at all).
    let trigger_cfg = cli.trigger_config();
    println!("{}", trigger_cfg.describe());

    // §4 step 4: initial mid, for the startup log.
    //
    // A time-only trigger (`--start-after` with no `--trigger-price`) must
    // NEVER call l2Book before its deadline (Issue #6) — there is nothing to
    // log a price for yet, and the pinned test
    // `time_only_trigger_fires_after_deadline_without_network` in
    // `trigger.rs` enforces the same contract on the wait loop itself.
    if !trigger_cfg.is_time_only() {
        let book = client
            .fetch_l2_book(&symbol)
            .await
            .map_err(|e| format!("initial l2Book: {e}"))?;
        let snapshot = ValidatedMarketSnapshot::validate(&book, &symbol, 0)
            .map_err(|e| format!("initial l2Book: {e}"))?;
        tracing::info!(symbol = %symbol, mid = %human(snapshot.mid), "initial mid");
    }

    // §4 step 6a: wait.
    let outcome = wait_for_trigger(&client, &symbol, &trigger_cfg)
        .await
        .map_err(|e| format!("trigger wait: {e}"))?;
    let reason = match outcome {
        TriggerOutcome::Fired(reason) => reason,
        TriggerOutcome::Expired(dur) => {
            // Issue #8: nothing was placed, no TwapReport — just the EXPIRED
            // line and exit code 3.
            println!(
                "EXPIRED: no trigger fired within {}",
                humantime::format_duration(dur)
            );
            return Ok(ExitCode::from(3));
        }
    };
    println!("Triggered: {reason}");

    // §8 pre-flight, F2: size against a mid that passed the SAME freshness gate
    // the slice loop uses. This snapshot fixes the coin quantity for the entire
    // run (and, with --usd, the notional too), so it is the single most
    // consequential price the tool reads — it must not be allowed to be the one
    // price that skips the staleness check.
    //
    // If the trigger fired on a price condition, the ALREADY-VALIDATED
    // snapshot that satisfied it is reused as-is rather than re-fetched: that
    // is the one and only meaning of "trigger-time mid" (Issue #6). Re-fetching
    // here would let a fresh, no-longer-crossing snapshot silently size an
    // order the trigger snapshot never actually justified.
    let snapshot = match &reason {
        TriggerReason::Price { snapshot, .. } => snapshot.clone(),
        TriggerReason::Immediate | TriggerReason::Elapsed { .. } => {
            fetch_fresh_book(&client, &symbol, cli.max_book_age_ms, None)
                .await
                .map_err(|e| format!("pre-flight l2Book: {e}"))?
        }
    };

    // Issue #2 (Finding 3): live-preflight clock-skew check, relocated to
    // execution entry. `expiresAfter` is a wall-clock Unix ms value trusted
    // by both the local `ExecutionDeadline` and Hyperliquid's own
    // exchange-side enforcement of the same field — if this host's clock is
    // skewed against HL's, the two enforcers disagree about when the run's
    // orders actually expire. Read-only / paper mode signs nothing, so a bad
    // local clock there is harmless; this check is therefore live-only (see
    // docs/DESIGN.md "クロックずれ").
    //
    // This check MUST run here — after the trigger has fired and a snapshot
    // is already in hand — rather than pre-wait: a pre-wait check would
    // require its own dedicated l2Book call, which a time-only trigger
    // (`--start-after` with no `--trigger-price`) must never make before its
    // deadline (Issue #6/#8; see `time_only_trigger_fires_after_deadline_without_network`
    // in `trigger.rs`). Reading `server_ts_ms` off the snapshot ALREADY
    // obtained above (whichever branch produced it) applies the same check
    // uniformly to both trigger modes, exactly once, with no extra l2Book
    // call of its own — it fails closed before ANY place happens.
    if !cli.read_only {
        check_clock_skew(wall_clock_now_ms() as i64, snapshot.server_ts_ms)
            .map_err(|e| e.to_string())?;
    }

    let mid = snapshot.mid;

    let (total_coin, requested_desc) = match (cli.size, cli.usd) {
        (Some(sz), _) => (sz, format!("{} {symbol}", human(sz))),
        (_, Some(usd)) => {
            let coin = usd_to_coin(usd, mid).map_err(|e| e.to_string())?;
            (
                coin,
                format!(
                    "${} → {} {symbol} at mid {}",
                    human(usd),
                    human(coin),
                    human(mid)
                ),
            )
        }
        (None, None) => return Err("no size specified".into()),
    };

    let sizing = compute_sizing(total_coin, cli.slices, asset.sz_decimals, mid)
        .map_err(|e| e.to_string())?;
    println!(
        "Rounded per-slice: {} × {} = {} (~${} at mid {}) [requested {}]",
        human(sizing.per_slice),
        cli.slices,
        human(sizing.total_adjusted),
        human((sizing.total_adjusted * mid).round_dp(2)),
        human(mid),
        requested_desc
    );
    if sizing.total_adjusted < total_coin {
        tracing::warn!(
            dropped = %human(total_coin - sizing.total_adjusted),
            "rounding dropped a residual below one slice tick"
        );
    }
    tracing::info!(min_notional_usd = %human(MIN_NOTIONAL_USD), "per-slice min-notional gate");

    // Issue #3: resolve the notional cap (required in live, Decimal::MAX —
    // effectively unbounded — in read-only) and pre-flight-check the
    // requested notional against it, BEFORE any `/exchange` call. `--usd` is
    // checked as the requested notional directly; `--size` is checked via a
    // freshly computed CONSERVATIVE limit price (the same taker_limit_price
    // formula the slice loop uses, evaluated at this snapshot's touch) so a
    // size-denominated request cannot dodge the cap by omitting price
    // entirely. The slice loop re-checks this same cap before EVERY slice
    // against that slice's actual order price (src/twap.rs) — both call
    // sites share the one risk module (src/risk.rs), never duplicated
    // constants.
    let max_notional_usd =
        RiskEnvelope::validate_max_notional_required(cli.read_only, risk.max_notional_usd)
            .map_err(|e| e.to_string())?;
    let preflight_notional = match (cli.size, cli.usd) {
        (Some(_), _) => {
            let conservative_px = hype_trigger_twap::format::taker_limit_price(
                snapshot.best_bid,
                snapshot.best_ask,
                side,
                risk.slippage_bps,
                asset.sz_decimals,
            );
            RiskEnvelope::validate_limit_price(
                conservative_px,
                side,
                risk.slippage_bps,
                snapshot.best_bid,
                snapshot.best_ask,
            )
            .map_err(|e| e.to_string())?;
            total_coin * conservative_px
        }
        (_, Some(usd)) => usd,
        (None, None) => return Err("no size specified".into()),
    };
    RiskEnvelope::check_notional_cap(preflight_notional, max_notional_usd)
        .map_err(|e| e.to_string())?;

    // Issue #3: one-shot pre-send summary, printed once before execution
    // begins.
    println!(
        "{}",
        pre_send_summary(
            network,
            &client.config().info_url,
            &client.config().exchange_url,
            symbol.as_str(),
            side,
            &requested_desc,
            risk.slippage_bps,
            if cli.read_only {
                None
            } else {
                Some(max_notional_usd)
            },
        )
    );

    // §8: the loop.
    let original_plan = TwapPlan {
        symbol: symbol.clone(),
        side,
        asset_index: asset.asset_index,
        sz_decimals: asset.sz_decimals,
        per_slice: sizing.per_slice,
        total_adjusted: sizing.total_adjusted,
        total_requested: total_coin,
        slices: cli.slices,
        duration: cli.duration,
        slippage_bps: risk.slippage_bps,
        max_book_age_ms: cli.max_book_age_ms,
        read_only: cli.read_only,
        max_notional_usd,
        agent: agent_address.clone(),
        master: master.clone(),
        child_algo: cli.child_algo.into(),
    };

    // Issue #4: this plan's hash — computed identically whether starting a
    // fresh run (stored in its new header) or resuming one (compared
    // against the STORED header's hash below). Any drift means the operator
    // passed different sizing/timing/risk parameters on the resume than the
    // original invocation used, which is exactly the mismatch a `--resume`
    // safety check exists to catch — resuming a 30-minute/10-slice run as
    // if it were a 5-minute/1-slice run would silently reinterpret the
    // remaining schedule.
    let plan_hash = hype_trigger_twap::journal::hash_plan_params(&[
        original_plan.symbol.as_str(),
        &original_plan.side.to_string(),
        &original_plan.per_slice.to_string(),
        &original_plan.total_adjusted.to_string(),
        &original_plan.slices.to_string(),
        &original_plan.duration.as_secs().to_string(),
        &original_plan.slippage_bps.to_string(),
        &original_plan.max_notional_usd.to_string(),
    ]);

    // Issue #4: open (or resume) the journal for a LIVE run only — read-only
    // never creates the state dir or a journal file (mirrors the incomplete-
    // run-detection gate above). `--abandon-incomplete-run` already returned
    // above (right after reconciliation, before the trigger wait), so only
    // two cases remain here: `--resume` re-opens the ALREADY-reconciled
    // run's journal for append and continues it, or a brand-new run starts
    // a fresh journal.
    // Issue #4 Finding 1 fix: the amount already credited (via prior
    // Terminal journal records) by the run being resumed, replayed from the
    // ALREADY-reconciled journal below. `None` for a brand-new run (no prior
    // fills to subtract). Used after the journal is opened to compute a
    // continuation plan that targets only the remainder, so a `--resume`
    // never re-executes the full original plan on top of fills the prior
    // process already made.
    let mut already_filled: Option<Decimal> = None;

    let mut journal = if cli.read_only {
        None
    } else if let Some(resume_id) = &cli.resume {
        let records =
            hype_trigger_twap::journal::ExecutionJournal::read_all(&resolved_state_dir, resume_id)
                .map_err(|e| format!("--resume {resume_id}: failed to read journal: {e}"))?;
        let stored_hash = records.iter().find_map(|r| match r {
            hype_trigger_twap::journal::JournalRecord::Header(h) => Some(h.plan_hash.clone()),
            _ => None,
        });
        if let Some(stored_hash) = stored_hash {
            if stored_hash != plan_hash {
                return Err(format!(
                    "--resume {resume_id}: this invocation's plan does not match the run being \
                     resumed (stored plan_hash {stored_hash}, this invocation computes \
                     {plan_hash}). Re-run --resume with the EXACT SAME --symbol/--side/--usd or \
                     --size/--duration/--slices/--slippage-bps/--max-notional-usd the original \
                     run used, or pass --abandon-incomplete-run instead if you intend to change \
                     them."
                ));
            }
        }
        // Issue #4 Finding 1 fix: `records` here reflects the journal AFTER
        // forced reconciliation ran above (reconciliation reopened/appended
        // to the SAME file via `reconcile_incomplete_run`, which fsyncs
        // every record it writes) — so this replay already includes every
        // cloid's resolved Terminal outcome, not just what the prior
        // (crashed) process itself observed.
        already_filled = Some(hype_trigger_twap::journal::summarize(&records).total_filled());
        Some(
            hype_trigger_twap::journal::ExecutionJournal::open_existing(
                &resolved_state_dir,
                resume_id,
            )
            .map_err(|e| format!("--resume {resume_id}: failed to re-open journal: {e}"))?,
        )
    } else {
        let run_id = uuid::Uuid::now_v7().to_string();
        let header = hype_trigger_twap::journal::RunHeader {
            run_id: run_id.clone(),
            network: network.to_string(),
            agent: agent_address.clone(),
            master: master.clone(),
            symbol: original_plan.symbol.clone(),
            side: original_plan.side,
            slices: original_plan.slices,
            plan_hash,
            started_at_unix_ms: hype_trigger_twap::twap::wall_clock_now_ms(),
        };
        Some(
            hype_trigger_twap::journal::ExecutionJournal::start(
                &resolved_state_dir,
                run_id,
                header,
            )
            .map_err(|e| format!("failed to start execution journal: {e}"))?,
        )
    };

    // Issue #4 Finding 1 fix: `--resume` must continue for the REMAINDER of
    // the original plan, never re-execute it from scratch — a slice already
    // journaled Terminal (a real fill) is excluded from
    // `unresolved_cloids()`, so the forced reconciliation above does not by
    // itself prevent double-placing it; only this continuation-plan
    // computation does.
    //
    // Captured before `original_plan` is potentially moved into `plan`
    // below (the no-resume arm), so the final report (after the run
    // completes) can still be rendered against the ORIGINAL target.
    let original_total_adjusted = original_plan.total_adjusted;
    let original_total_requested = original_plan.total_requested;
    //
    // Continuation-plan computation: `remaining = original_total_adjusted -
    // already_filled`. If the
    // remainder cannot clear the same per-slice min-notional gate the
    // original plan was sized against (using the same trigger-time `mid`),
    // there is nothing left this run can legally place: skip straight to a
    // durable FinalReport and exit 0 without ever placing a new order. This
    // mirrors `PreflightError::PerSliceBelowMinNotional`'s gate (same
    // `MIN_NOTIONAL_USD` constant, no new threshold invented).
    let plan = match already_filled {
        None => original_plan,
        Some(filled) => {
            let remaining = original_plan.total_adjusted - filled;
            if remaining <= Decimal::ZERO || remaining * mid < MIN_NOTIONAL_USD {
                tracing::info!(
                    already_filled = %human(filled),
                    original_total = %human(original_plan.total_adjusted),
                    remaining = %human(remaining.max(Decimal::ZERO)),
                    "--resume: remainder is already complete (or unplaceable below min notional); \
                     nothing further will be executed"
                );
                if let Some(j) = journal.as_mut() {
                    let _ = j.record(&hype_trigger_twap::journal::JournalRecord::FinalReport {
                        completed: true,
                        filled_total: filled.to_string(),
                        outcome_unknown_cloids: Vec::new(),
                        note: "resume: prior fills already satisfy (or leave an unplaceable \
                               remainder of) the original plan; nothing further executed"
                            .to_string(),
                    });
                }
                let report = hype_trigger_twap::twap::TwapReport {
                    symbol: original_plan.symbol.clone(),
                    side: original_plan.side,
                    total_requested: original_plan.total_requested,
                    total_adjusted: original_plan.total_adjusted,
                    filled,
                    avg_px: None,
                    slices_executed: 0,
                    slices_skipped: 0,
                    elapsed: Duration::ZERO,
                    abort_reason: None,
                    read_only: cli.read_only,
                };
                print!("{}", report.render());
                return Ok(ExitCode::SUCCESS);
            }
            // Continuation plan: SAME per-slice size as the original plan
            // (preserving the schedule's granularity), slices = ceil(
            // remaining / per_slice) so the last slice absorbs whatever
            // remainder is smaller than a full per-slice size — the same
            // "final slice carries the residual" convention
            // `target_at_slice`/`slice_order_size` already use for a normal
            // (non-resumed) run. `total_adjusted` is set to exactly
            // `remaining` so `run_twap_journaled`'s own early-finish check
            // (`stats.filled >= plan.total_adjusted`) and min-notional gating
            // on the final slice both apply to the true remainder, not the
            // original total.
            let per_slice = original_plan.per_slice;
            let continuation_slices: u32 = {
                let whole = (remaining / per_slice).floor();
                let has_remainder = whole * per_slice < remaining;
                let n = whole.to_string().parse::<u32>().unwrap_or(0) + u32::from(has_remainder);
                n.max(1)
            };
            tracing::info!(
                already_filled = %human(filled),
                original_total = %human(original_plan.total_adjusted),
                remaining = %human(remaining),
                continuation_slices,
                "--resume: continuing with a plan scoped to the remainder only"
            );
            TwapPlan {
                symbol: original_plan.symbol.clone(),
                side: original_plan.side,
                asset_index: original_plan.asset_index,
                sz_decimals: original_plan.sz_decimals,
                per_slice,
                total_adjusted: remaining,
                total_requested: original_plan.total_requested,
                slices: continuation_slices,
                duration: original_plan.duration,
                slippage_bps: original_plan.slippage_bps,
                max_book_age_ms: original_plan.max_book_age_ms,
                read_only: original_plan.read_only,
                max_notional_usd: original_plan.max_notional_usd,
                agent: original_plan.agent.clone(),
                master: original_plan.master.clone(),
                child_algo: original_plan.child_algo,
            }
        }
    };

    // Issue #4: SIGINT/SIGTERM cooperative shutdown. A tokio::sync::watch
    // channel is the shutdown token both the real signal task and (in
    // src/twap.rs's tests) a test harness can drive identically. The signal
    // task itself only flips the watch value — all the actual
    // stop-scheduling / reconcile / cancel / report behaviour lives in
    // `run_twap_journaled`, so there is exactly one code path for both a
    // normal end-of-run and a signal-interrupted one.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let shutdown_signal = hype_trigger_twap::twap::ShutdownSignal::new(shutdown_rx);
    let signal_task = if cli.read_only {
        None
    } else {
        Some(tokio::spawn(async move {
            let mut sigterm =
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to install SIGTERM handler");
                        return;
                    }
                };
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    tracing::warn!("SIGINT received; requesting graceful shutdown");
                }
                _ = sigterm.recv() => {
                    tracing::warn!("SIGTERM received; requesting graceful shutdown");
                }
            }
            let _ = shutdown_tx.send(true);
        }))
    };

    let run_fut = hype_trigger_twap::twap::run_twap_journaled(
        &client,
        &plan,
        journal.as_mut(),
        Some(shutdown_signal),
    );

    // Issue #4: bound the WHOLE run (not just the shutdown-triggered tail)
    // by `--shutdown-grace` ONLY once a signal has actually fired — a
    // healthy run with no signal must never be timed out by this. The
    // simplest correct way to express "grace only applies after shutdown"
    // without duplicating run_twap_journaled's internal reconcile/cancel
    // logic is: race the run against the grace timer, but only START the
    // grace timer once the signal task has completed (i.e. a signal fired).
    let report = if let Some(task) = signal_task {
        tokio::select! {
            report = run_fut => report,
            _ = async {
                let _ = task.await;
                tokio::time::sleep(cli.shutdown_grace).await;
            } => {
                tracing::error!(
                    grace = ?cli.shutdown_grace,
                    "shutdown grace period exceeded; giving up with outcome_unknown"
                );
                if let Some(j) = journal.as_mut() {
                    // Finding 2 fix: derive both fields from the ACTUAL
                    // journal state at this moment, via `grace_timeout_report_fields`
                    // (unit-tested directly below), rather than hardcoding
                    // zero/empty, which silently discarded every real fill
                    // and every genuinely-still-open cloid this run needed
                    // to flag as outcome_unknown.
                    let (filled_total, outcome_unknown_cloids) =
                        grace_timeout_report_fields(&resolved_state_dir, j.run_id());
                    let _ = j.record(&hype_trigger_twap::journal::JournalRecord::FinalReport {
                        completed: false,
                        filled_total: filled_total.to_string(),
                        outcome_unknown_cloids,
                        note: format!(
                            "shutdown grace period ({:?}) exceeded before reconciliation finished",
                            cli.shutdown_grace
                        ),
                    });
                }
                return Ok(ExitCode::FAILURE);
            }
        }
    } else {
        run_fut.await
    };

    // Issue #4 Finding 1 fix: a resumed run's `report` only reflects fills
    // this process itself placed (the continuation plan, scoped to
    // `remaining`) — the prior process's already-journaled fills must be
    // folded back in so the PRINTED report (and its `total_adjusted`) read
    // against the ORIGINAL plan the operator asked for, not the truncated
    // continuation. The journal itself is already the source of truth for
    // exactly-once accounting (`RunSummary::total_filled` sums each
    // Terminal cloid once, whether journaled by this process or a prior
    // one) — this only affects the human-readable summary.
    let report = match already_filled {
        Some(prior) => hype_trigger_twap::twap::TwapReport {
            total_adjusted: original_total_adjusted,
            total_requested: original_total_requested,
            filled: prior + report.filled,
            ..report
        },
        None => report,
    };
    print!("{}", report.render());

    if report.exit_code() == 0 {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::FAILURE)
    }
}

/// Finding 2 fix: the `(filled_total, outcome_unknown_cloids)` a
/// grace-timeout `FinalReport` should carry, derived by replaying THIS
/// run's own journal at the moment the grace period expired — rather than
/// the pre-fix hardcoded `(0, [])`, which discarded every real fill and
/// every genuinely-still-open cloid.
///
/// A read failure (journal file vanished, malformed, ...) falls back to
/// `(0, [])` with a warning rather than panicking — the grace-timeout path
/// is already the "giving up" branch; best-effort accounting here is still
/// strictly better than the pre-fix behaviour, which was ALWAYS `(0, [])`.
fn grace_timeout_report_fields(
    state_root: &std::path::Path,
    run_id: &str,
) -> (Decimal, Vec<hype_trigger_twap::types::Cloid>) {
    hype_trigger_twap::journal::ExecutionJournal::read_all(state_root, run_id)
        .map(|records| {
            let summary = hype_trigger_twap::journal::summarize(&records);
            (summary.total_filled(), summary.unresolved_cloids())
        })
        .unwrap_or_else(|e| {
            tracing::warn!(
                error = %e,
                "grace-timeout FinalReport: failed to replay this run's own journal for \
                 accounting; falling back to zero/empty"
            );
            (Decimal::ZERO, Vec::new())
        })
}

/// Issue #4: force-reconcile every submitted/unknown cloid in an incomplete
/// run's journal via `orderStatus`, appending the resolved
/// `Acknowledged`/`Terminal` records. Used by both `--resume` (which then
/// continues the run) and `--abandon-incomplete-run` (which reconciles then
/// marks the run `Abandoned` without continuing it) — reconciliation itself
/// is identical either way, only what happens AFTER it differs.
///
/// Reuses [`hype_trigger_twap::twap::reconcile_unresolved_cloid`], which
/// wraps the same `orderStatus`-by-cloid policy `place_slice_reconciled`
/// already uses for an ambiguous send (Issue #7's `reconcile_by_cloid`) —
/// there is exactly one orderStatus reconciliation policy in this codebase,
/// not a second one for the resume path.
async fn reconcile_incomplete_run(
    client: &HlClient,
    plan: &TwapPlan,
    journal: &mut hype_trigger_twap::journal::ExecutionJournal,
) -> Result<(), String> {
    let records = hype_trigger_twap::journal::ExecutionJournal::read_all(
        journal
            .dir()
            .parent()
            .and_then(|p| p.parent())
            .ok_or_else(|| {
                "malformed journal run directory (expected <state_dir>/runs/<run_id>)".to_string()
            })?,
        journal.run_id(),
    )
    .map_err(|e| e.to_string())?;
    let summary = hype_trigger_twap::journal::summarize(&records);

    // Finding 3 fix: recover the TRUE original `slice_idx` for each
    // unresolved cloid from its own `Prepared` record (joined by cloid) in
    // the SAME prior journal, rather than writing a `0` placeholder. Every
    // cloid this function ever reconciles was, by construction, placed by
    // this same run's own `place_slice_reconciled` (which always journals
    // `Prepared` before the network send — see src/twap.rs), so the join
    // always succeeds for any cloid actually reachable via
    // `unresolved_cloids()`; the `unwrap_or(0)` fallback exists only for a
    // theoretically malformed/hand-edited journal missing its own Prepared
    // record; see `docs` note in this function's caller-facing report if
    // that ever fires in practice.
    let slice_idx_by_cloid: std::collections::HashMap<hype_trigger_twap::types::Cloid, u32> =
        records
            .iter()
            .filter_map(|r| match r {
                hype_trigger_twap::journal::JournalRecord::Prepared {
                    slice_idx, cloid, ..
                } => Some((*cloid, *slice_idx)),
                _ => None,
            })
            .collect();

    // A2: `reconcile_unresolved_cloid` validates every fill it credits
    // (`ValidatedFill::try_from_status`, bounds: 0<=filled<=intent.sz,
    // avg_px>0 when filled>0) against the ORIGINAL Prepared intent for its
    // cloid — the same trusted-boundary treatment every other fill in this
    // codebase gets (Issue #7). Build that map alongside slice_idx's, from
    // the SAME `Prepared` records (every cloid reconciled here was, by
    // construction, journaled with one — see the note above).
    let prepared_by_cloid: std::collections::HashMap<
        hype_trigger_twap::types::Cloid,
        hype_trigger_twap::twap::PreparedIntent,
    > = records
        .iter()
        .filter_map(|r| match r {
            hype_trigger_twap::journal::JournalRecord::Prepared {
                cloid,
                symbol,
                side,
                px,
                sz,
                ..
            } => {
                let px: Decimal = px.parse().ok()?;
                let sz: Decimal = sz.parse().ok()?;
                Some((
                    *cloid,
                    hype_trigger_twap::twap::PreparedIntent {
                        symbol: symbol.clone(),
                        side: *side,
                        px,
                        sz,
                    },
                ))
            }
            _ => None,
        })
        .collect();

    for cloid in summary.unresolved_cloids() {
        let slice_idx = slice_idx_by_cloid.get(&cloid).copied().unwrap_or(0);
        let prepared = prepared_by_cloid.get(&cloid).ok_or_else(|| {
            format!(
                "cannot reconcile cloid {cloid}: no Prepared record found in this run's own \
                 journal — the journal is malformed (every cloid reachable via \
                 unresolved_cloids() must have been journaled with a Prepared record before \
                 the send that made it unresolved)"
            )
        })?;
        hype_trigger_twap::twap::reconcile_unresolved_cloid(
            client, plan, cloid, slice_idx, prepared, journal,
        )
        .await
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use clap::CommandFactory;

    fn base_args() -> Vec<&'static str> {
        vec![
            "hype-twap",
            "--symbol",
            "HYPE",
            "--side",
            "long",
            "--usd",
            "1500",
            "--duration",
            "30m",
        ]
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    // === F3: --help documents the environment contract ===

    #[test]
    fn help_output_documents_every_environment_variable() {
        // `long_about = None` makes clap discard the struct doc comment, so
        // without `after_help` the variables that decide whether the tool can
        // trade at all would appear nowhere in `--help`.
        let help = Cli::command().render_help().to_string();
        for var in [
            "HL_AGENT_PK",
            "HL_AGENT_ADDRESS",
            "HL_MASTER_ADDRESS",
            "HL_INFO_URL",
            "HL_EXCHANGE_URL",
        ] {
            assert!(help.contains(var), "--help must mention {var}\n{help}");
        }
        assert!(help.contains("ENVIRONMENT VARIABLES"), "{help}");
    }

    #[test]
    fn help_says_agent_address_is_the_agent_not_the_master() {
        // The single most dangerous confusion in this tool's configuration:
        // pointing HL_AGENT_ADDRESS at the master account silently describes
        // the wrong wallet. The help must call it out explicitly.
        let help = Cli::command().render_help().to_string();
        assert!(
            help.contains("AGENT (API wallet) address — NOT the master"),
            "{help}"
        );
        // And HL_MASTER_ADDRESS must be documented as optional / auto-probed.
        assert!(help.contains("userRole"), "{help}");
    }

    #[test]
    fn help_never_suggests_passing_the_key_as_a_flag() {
        let help = Cli::command().render_help().to_string();
        assert!(help.contains("never as a flag"), "{help}");
        assert!(
            !help.contains("--hl-agent-pk"),
            "there must be no PK flag, not even in the help text"
        );
    }

    #[test]
    fn parses_the_spec_example_invocation() {
        let cli = Cli::try_parse_from([
            "hype-twap",
            "--symbol",
            "HYPE",
            "--side",
            "long",
            "--usd",
            "1500",
            "--duration",
            "30m",
            "--max-notional-usd",
            "5000",
            "--read-only",
            "false",
        ])
        .unwrap();
        assert_eq!(cli.symbol, "HYPE");
        assert_eq!(cli.side, SideArg::Long);
        assert_eq!(cli.usd, Some(Decimal::from(1500)));
        assert_eq!(cli.duration, Duration::from_secs(1800));
        assert!(!cli.read_only);
        cli.validate().unwrap();
    }

    #[test]
    fn read_only_defaults_to_true() {
        let cli = Cli::try_parse_from(base_args()).unwrap();
        assert!(cli.read_only, "read-only MUST default to true (§3)");
    }

    #[test]
    fn defaults_match_the_spec_table() {
        let cli = Cli::try_parse_from(base_args()).unwrap();
        assert_eq!(cli.slices, 10);
        assert_eq!(cli.network, NetworkArg::Mainnet);
        assert_eq!(cli.slippage_bps, Decimal::from(20));
        assert_eq!(cli.max_book_age_ms, 3000);
        assert_eq!(cli.trigger_poll_secs, 2);
        assert_eq!(cli.wait_network_grace, Duration::from_secs(30 * 60));
    }

    // === Issue #1: --child-algo ===

    #[test]
    fn child_algo_defaults_to_market_and_maps_to_the_market_variant() {
        let cli = Cli::try_parse_from(base_args()).unwrap();
        assert_eq!(cli.child_algo, ChildAlgoArg::Market);
        assert_eq!(ChildAlgo::from(cli.child_algo), ChildAlgo::Market);
    }

    #[test]
    fn child_algo_passive_parses_and_maps_to_the_passive_variant() {
        let mut args = base_args();
        args.push("--child-algo");
        args.push("passive");
        let cli = Cli::try_parse_from(args).unwrap();
        assert_eq!(cli.child_algo, ChildAlgoArg::Passive);
        assert_eq!(ChildAlgo::from(cli.child_algo), ChildAlgo::Passive);
    }

    #[test]
    fn child_algo_rejects_an_unknown_value() {
        let mut args = base_args();
        args.push("--child-algo");
        args.push("aggressive");
        assert!(Cli::try_parse_from(args).is_err());
    }

    #[test]
    fn size_and_usd_are_mutually_exclusive() {
        let r = Cli::try_parse_from([
            "hype-twap",
            "--symbol",
            "HYPE",
            "--side",
            "long",
            "--usd",
            "1500",
            "--size",
            "10",
            "--duration",
            "30m",
        ]);
        assert!(r.is_err(), "--size and --usd must conflict");
    }

    #[test]
    fn one_of_size_or_usd_is_required() {
        let cli = Cli::try_parse_from([
            "hype-twap",
            "--symbol",
            "HYPE",
            "--side",
            "long",
            "--duration",
            "30m",
        ])
        .unwrap();
        let err = cli.validate().unwrap_err();
        assert!(err.contains("--size or --usd"), "{err}");
    }

    #[test]
    fn trigger_price_requires_trigger_when() {
        let r = Cli::try_parse_from([
            "hype-twap",
            "--symbol",
            "HYPE",
            "--side",
            "long",
            "--usd",
            "1500",
            "--duration",
            "30m",
            "--trigger-price",
            "40",
        ]);
        assert!(
            r.is_err(),
            "--trigger-price alone must be rejected (fail-fast, no inference)"
        );
    }

    #[test]
    fn trigger_when_requires_trigger_price() {
        let r = Cli::try_parse_from([
            "hype-twap",
            "--symbol",
            "HYPE",
            "--side",
            "long",
            "--usd",
            "1500",
            "--duration",
            "30m",
            "--trigger-when",
            "above",
        ]);
        assert!(r.is_err());
    }

    #[test]
    fn trigger_pair_parses_and_builds_config() {
        let cli = Cli::try_parse_from([
            "hype-twap",
            "--symbol",
            "HYPE",
            "--side",
            "long",
            "--usd",
            "1500",
            "--duration",
            "30m",
            "--trigger-price",
            "40.5",
            "--trigger-when",
            "above",
            "--start-after",
            "10m",
        ])
        .unwrap();
        cli.validate().unwrap();
        let cfg = cli.trigger_config();
        assert_eq!(
            cfg.price,
            Some((TriggerWhen::Above, "40.5".parse().unwrap()))
        );
        assert_eq!(cfg.start_after, Some(Duration::from_secs(600)));
        assert!(cfg.describe().contains("OR after"));
    }

    #[test]
    fn no_trigger_flags_means_immediate() {
        let cli = Cli::try_parse_from(base_args()).unwrap();
        assert!(cli.trigger_config().is_immediate());
    }

    #[test]
    fn zero_slices_is_rejected() {
        let cli = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain(["--slices", "0"])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert!(cli.validate().unwrap_err().contains("--slices"));
    }

    #[test]
    fn zero_duration_is_rejected() {
        let cli = Cli::try_parse_from([
            "hype-twap",
            "--symbol",
            "HYPE",
            "--side",
            "long",
            "--usd",
            "1500",
            "--duration",
            "0s",
        ])
        .unwrap();
        assert!(cli.validate().unwrap_err().contains("--duration"));
    }

    #[test]
    fn non_positive_sizes_are_rejected() {
        let cli = Cli::try_parse_from([
            "hype-twap",
            "--symbol",
            "HYPE",
            "--side",
            "long",
            "--usd",
            "0",
            "--duration",
            "30m",
        ])
        .unwrap();
        assert!(cli.validate().unwrap_err().contains("--usd"));

        // `--size=-1` (attached form): clap would treat a bare `-1` as a flag,
        // so the attached form is what actually reaches the validator.
        let cli = Cli::try_parse_from([
            "hype-twap",
            "--symbol",
            "HYPE",
            "--side",
            "short",
            "--size=-1",
            "--duration",
            "30m",
        ])
        .unwrap();
        assert!(cli.validate().unwrap_err().contains("--size"));
    }

    #[test]
    fn negative_slippage_is_rejected() {
        let cli = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain(["--slippage-bps=-1"])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert!(cli.validate().unwrap_err().contains("--slippage-bps"));
    }

    // === Issue #3: risk envelope CLI wiring ===

    #[test]
    fn slippage_at_hard_cap_is_rejected_even_with_allow_high_slippage() {
        let cli = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain(["--slippage-bps", "10000", "--allow-high-slippage"])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let err = cli.validate().unwrap_err();
        assert!(err.contains("hard cap"), "{err}");
    }

    #[test]
    fn slippage_over_warn_threshold_is_rejected_without_the_flag() {
        let cli = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain(["--slippage-bps", "1001"])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let err = cli.validate().unwrap_err();
        assert!(err.contains("--allow-high-slippage"), "{err}");
    }

    #[test]
    fn slippage_over_warn_threshold_is_accepted_with_the_flag() {
        let cli = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain(["--slippage-bps", "1001", "--allow-high-slippage"])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        cli.validate().unwrap();
    }

    #[test]
    fn live_without_max_notional_usd_is_rejected() {
        let cli = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain(["--read-only", "false"])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let err = cli.validate().unwrap_err();
        assert!(err.contains("--max-notional-usd"), "{err}");
    }

    #[test]
    fn live_with_max_notional_usd_is_accepted() {
        let cli = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain(["--read-only", "false", "--max-notional-usd", "5000"])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        cli.validate().unwrap();
    }

    #[test]
    fn read_only_does_not_require_max_notional_usd() {
        // Default read-only (true), no --max-notional-usd given at all.
        let cli = Cli::try_parse_from(base_args()).unwrap();
        cli.validate().unwrap();
    }

    #[test]
    fn allow_custom_endpoints_defaults_to_false() {
        let cli = Cli::try_parse_from(base_args()).unwrap();
        assert!(!cli.allow_custom_endpoints);
    }

    #[test]
    fn allow_high_slippage_defaults_to_false() {
        let cli = Cli::try_parse_from(base_args()).unwrap();
        assert!(!cli.allow_high_slippage);
    }

    #[test]
    fn zero_poll_interval_is_rejected() {
        let cli = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain(["--trigger-poll-secs", "0"])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert!(cli.validate().unwrap_err().contains("--trigger-poll-secs"));
    }

    #[test]
    fn zero_wait_network_grace_is_rejected() {
        let cli = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain(["--wait-network-grace", "0s"])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert!(cli.validate().unwrap_err().contains("--wait-network-grace"));
    }

    #[test]
    fn wait_network_grace_parses_and_wires_into_trigger_config() {
        let cli = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain(["--wait-network-grace", "45m"])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        cli.validate().unwrap();
        assert_eq!(cli.wait_network_grace, Duration::from_secs(45 * 60));
        assert_eq!(
            cli.trigger_config().wait_network_grace,
            Duration::from_secs(45 * 60)
        );
    }

    #[test]
    fn humantime_durations_parse() {
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_secs(1800));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert!(parse_duration("banana").is_err());
    }

    #[test]
    fn network_arg_maps_to_matching_urls_and_domain() {
        let n: Network = NetworkArg::Testnet.into();
        assert!(!n.is_mainnet());
        assert!(HlConfig::new(n).exchange_url.contains("testnet"));
        let n: Network = NetworkArg::Mainnet.into();
        assert!(n.is_mainnet());
        assert!(HlConfig::new(n)
            .exchange_url
            .contains("api.hyperliquid.xyz"));
    }

    #[test]
    fn pk_is_not_accepted_as_a_flag() {
        // The key must come from the environment only — never argv.
        let r = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain([
                    "--hl-agent-pk",
                    "0x0123456789012345678901234567890123456789012345678901234567890123",
                ])
                .collect::<Vec<_>>(),
        );
        assert!(r.is_err(), "there must be no PK flag");
    }

    // === Issue #8: --expire-after ===

    #[test]
    fn zero_expire_after_is_rejected() {
        let cli = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain([
                    "--trigger-price",
                    "40",
                    "--trigger-when",
                    "above",
                    "--expire-after",
                    "0s",
                ])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert!(cli.validate().unwrap_err().contains("--expire-after"));
    }

    #[test]
    fn expire_after_equal_to_start_after_is_rejected() {
        let cli = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain(["--start-after", "10m", "--expire-after", "10m"])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let err = cli.validate().unwrap_err();
        assert!(err.contains("--expire-after"), "{err}");
        assert!(err.contains("--start-after"), "{err}");
    }

    #[test]
    fn expire_after_less_than_start_after_is_rejected() {
        let cli = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain(["--start-after", "10m", "--expire-after", "5m"])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let err = cli.validate().unwrap_err();
        assert!(err.contains("--expire-after"), "{err}");
        assert!(err.contains("--start-after"), "{err}");
    }

    #[test]
    fn expire_after_with_no_trigger_at_all_is_rejected() {
        let cli = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain(["--expire-after", "1h"])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let err = cli.validate().unwrap_err();
        assert!(err.contains("--expire-after"), "{err}");
    }

    #[test]
    fn expire_after_with_price_trigger_is_accepted() {
        let cli = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain([
                    "--trigger-price",
                    "40",
                    "--trigger-when",
                    "above",
                    "--expire-after",
                    "1h",
                ])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        cli.validate().unwrap();
        assert_eq!(
            cli.trigger_config().expire_after,
            Some(Duration::from_secs(3600))
        );
    }

    #[test]
    fn expire_after_greater_than_start_after_is_accepted() {
        let cli = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain(["--start-after", "5m", "--expire-after", "10m"])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        cli.validate().unwrap();
        assert_eq!(
            cli.trigger_config().expire_after,
            Some(Duration::from_secs(600))
        );
        assert_eq!(
            cli.trigger_config().start_after,
            Some(Duration::from_secs(300))
        );
    }

    #[test]
    fn expire_after_unset_defaults_to_none_and_does_not_change_immediate_or_time_only() {
        let cli = Cli::try_parse_from(base_args()).unwrap();
        cli.validate().unwrap();
        assert_eq!(cli.trigger_config().expire_after, None);
        assert!(cli.trigger_config().is_immediate());
    }

    #[test]
    fn describe_includes_expiry_wording_when_flag_is_set() {
        let cli = Cli::try_parse_from(
            base_args()
                .into_iter()
                .chain([
                    "--trigger-price",
                    "40",
                    "--trigger-when",
                    "above",
                    "--start-after",
                    "2h",
                    "--expire-after",
                    "4h",
                ])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        cli.validate().unwrap();
        let d = cli.trigger_config().describe();
        assert_eq!(
            d,
            "Trigger: price above 40 OR after 2h (whichever comes first); EXPIRES after 4h (no order if not fired)"
        );
    }

    // === Finding 3 (Issue #2 relocation): clock-skew check moved from
    // pre-wait to execution entry ===

    const TEST_PK: &str = "0x0123456789012345678901234567890123456789012345678901234567890123";
    // The address TEST_PK actually derives to (same key used by
    // tests/signing_cross_check.rs's vectors — see expected_address there).
    const AGENT: &str = "0x14791697260e4c9a71f18484c9f997b308e59325";
    const MASTER: &str = "0x00000000000000000000000000000000000000aa";

    fn book_body_at(coin: &str, bid: &str, ask: &str, time_ms: i64) -> String {
        format!(
            r#"{{"coin":"{coin}","time":{time_ms},"levels":[
                [{{"px":"{bid}","sz":"100","n":1}}],
                [{{"px":"{ask}","sz":"100","n":1}}]]}}"#
        )
    }

    /// Regression guard for the invariant Finding 3 protects: a live
    /// time-only trigger (`--start-after`, no `--trigger-price`) must make
    /// ZERO network calls before its deadline elapses. This was true before
    /// commit 9986b46 (`wait_for_trigger` itself never touches the network
    /// for a time-only wait — see `time_only_trigger_fires_after_deadline_without_network`
    /// in `trigger.rs`) but 9986b46 broke it end-to-end by adding a
    /// dedicated pre-wait `l2Book` call in `main.rs::run()` purely for the
    /// clock-skew check. This test exercises `run_with_cli` (the `Cli`-
    /// injectable body of `run()`) against a mock server that has NO route
    /// registered for `/info` at all — any network call made before the
    /// wait completes would fail loudly (mockito 501s an unmatched request),
    /// which would surface as an error well before the deadline. Instead,
    /// the run must reach the deadline (virtual time elapses the full
    /// `--start-after`), THEN make its post-trigger `l2Book` call (which the
    /// mock now serves) for sizing/clock-skew, and finally fail on
    /// `--size`/exchange being absent from this minimal fixture — the
    /// precise failure mode after that point does not matter; what matters
    /// is that virtual time advanced the full wait duration first, proving
    /// no network call happened during the wait itself.
    // Real time, not `start_paused`: this test drives `run_with_cli` against
    // a real (localhost) mockito HTTP server, and mixing tokio's virtual-time
    // pause with real socket I/O is unreliable (the mock server's own
    // accept/response loop runs on real time regardless of the test's
    // virtual clock). A short, real --start-after keeps the test fast while
    // still proving the ordering.
    // `#[serial]`: this test mutates process-global env vars (HL_INFO_URL,
    // HL_EXCHANGE_URL) that `run_with_cli` reads mid-flight over several real
    // seconds. `cargo test` runs test fns concurrently on separate threads
    // within one process, and env vars are process-wide — without
    // serialization this races against the other env-mutating test below
    // (`issue2_live_skew_beyond_tolerance_...`), letting one test's
    // in-flight `run_with_cli` pick up the other's HL_INFO_URL/HL_EXCHANGE_URL
    // mid-run and hit the wrong mockito server (observed as a spurious
    // mockito 501 on an unmatched route, e.g. a "userRole probe" landing on
    // a server that never registered that route).
    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn issue2_live_time_only_trigger_makes_no_network_call_before_its_deadline() {
        let mut server = mockito::Server::new_async().await;
        // meta: needed at startup, before the wait begins — this IS allowed
        // (it happens at §4 step 2, well before the trigger wait).
        let _meta = server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({"type": "meta"})))
            .with_status(200)
            .with_body(r#"{"universe":[{"name":"HYPE","szDecimals":2,"maxLeverage":10,"onlyIsolated":false}]}"#)
            .create_async()
            .await;
        // l2Book: only served AFTER the trigger fires (post-trigger pre-flight
        // fetch + clock-skew read). If this were hit DURING the wait, the
        // virtual clock would not have advanced the full --start-after
        // duration by the time the run errors out below.
        let now_ms = chrono::Utc::now().timestamp_millis();
        let _book = server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "l2Book"}),
            ))
            .with_status(200)
            .with_body(book_body_at("HYPE", "49.9", "50.1", now_ms))
            .create_async()
            .await;

        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));

        let cli = Cli::try_parse_from([
            "hype-twap",
            "--symbol",
            "HYPE",
            "--side",
            "long",
            "--usd",
            "1500",
            "--duration",
            "5m",
            "--start-after",
            "2s",
            "--read-only",
            "true",
        ])
        .unwrap();

        let start = std::time::Instant::now();
        // This test only cares about the ordering up to (and just past) the
        // trigger firing; once triggered, the run continues into sizing and
        // the slice loop, which — with no `/exchange` mock registered here —
        // eventually retries a going-stale book for several seconds before
        // aborting. Bound the whole call so the test does not pay for that
        // unrelated retry storm.
        let _ = tokio::time::timeout(Duration::from_secs(10), run_with_cli(cli)).await;
        let elapsed = start.elapsed();

        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");

        // The load-bearing assertion: real time advanced by (at least) the
        // full 2s --start-after wait. If a pre-wait skew check like the one
        // 9986b46 added were still present, its l2Book call would be served
        // by the mock immediately at T=0 (the mock has no artificial delay),
        // making `elapsed` far short of 2s.
        assert!(
            elapsed >= Duration::from_secs(2),
            "a time-only trigger must not resolve before its --start-after deadline; \
             elapsed = {elapsed:?} (expected >= 2s) — a network call before the wait \
             would let this finish early"
        );
    }

    /// Adapts the "skew > 5s fails closed" behavior to assert it now fires at
    /// EXECUTION ENTRY (after "Triggered:", using the trigger's own
    /// snapshot) rather than pre-wait. Uses an immediate trigger (no wait) so
    /// the ONLY l2Book call in the whole run is the one that both fires the
    /// trigger (well, immediate doesn't need one) and doubles as the
    /// post-trigger pre-flight snapshot the skew check reads — proving the
    /// relocated check reads `server_ts_ms` off that snapshot rather than
    /// issuing a second dedicated l2Book call.
    // `#[serial]`: see the comment on
    // `issue2_live_time_only_trigger_makes_no_network_call_before_its_deadline`
    // above — same process-global env var race (this test additionally sets
    // HL_AGENT_PK / HL_AGENT_ADDRESS).
    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn issue2_live_skew_beyond_tolerance_fails_closed_at_execution_entry_not_prewait() {
        let mut server = mockito::Server::new_async().await;
        let _meta = server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({"type": "meta"})))
            .with_status(200)
            .with_body(r#"{"universe":[{"name":"HYPE","szDecimals":2,"maxLeverage":10,"onlyIsolated":false}]}"#)
            .create_async()
            .await;
        let _role = server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "userRole"}),
            ))
            .with_status(200)
            .with_body(format!(
                r#"{{"role":"agent","data":{{"user":"{MASTER}"}}}}"#
            ))
            .create_async()
            .await;
        // A time-only trigger (`--start-after`) is used so `wait_for_trigger`
        // itself makes NO l2Book call (Issue #6/#8 contract) — the ONLY
        // l2Book call in the whole run is the post-trigger pre-flight fetch
        // (`fetch_fresh_book(..., None)` at main.rs, `TriggerReason::Elapsed`
        // branch), which per the Finding 3 relocation is also the source of
        // `server_ts_ms` for the skew check. This proves the relocated check
        // reads the ALREADY-obtained snapshot rather than issuing a second,
        // dedicated l2Book call. The response is stamped far in the PAST
        // (skew < -5s) rather than the future, so it fails only the
        // clock-skew check and not `ValidatedMarketSnapshot`'s own
        // unconditional `MAX_FUTURE_SKEW_MS` (2s) future-skew check;
        // `--max-book-age-ms 0` disables the (unrelated) staleness half of
        // that same validation so an old timestamp does not fail for the
        // wrong reason.
        let now_ms = chrono::Utc::now().timestamp_millis();
        let skewed_ms = now_ms - 10_000; // 10s in the past: beyond MAX_CLOCK_SKEW_MS (5s)
        let _book = server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "l2Book"}),
            ))
            .with_status(200)
            .with_body(book_body_at("HYPE", "39.9", "40.1", skewed_ms))
            .expect(1)
            .create_async()
            .await;

        std::env::set_var("HL_AGENT_PK", TEST_PK);
        std::env::set_var("HL_AGENT_ADDRESS", AGENT);
        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));

        let cli = Cli::try_parse_from([
            "hype-twap",
            "--symbol",
            "HYPE",
            "--side",
            "long",
            "--usd",
            "1500",
            "--duration",
            "5m",
            "--start-after",
            "1s",
            "--max-book-age-ms",
            "0",
            "--max-notional-usd",
            "1000000",
            // Issue #3: live + a custom endpoint override is rejected by
            // default; this test's mock server is loopback-only (mockito has
            // no TLS support), which the loopback carve-out on
            // validate_endpoint_override permits once this flag is passed.
            "--allow-custom-endpoints",
            "--read-only",
            "false",
        ])
        .unwrap();

        let result = run_with_cli(cli).await;

        std::env::remove_var("HL_AGENT_PK");
        std::env::remove_var("HL_AGENT_ADDRESS");
        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");

        let err = result.expect_err("a >5s clock skew must fail the run closed");
        assert!(err.contains("skew"), "{err}");

        // Exactly one l2Book call: the trigger-poll snapshot doubles as the
        // skew-check source. A pre-wait-style implementation would need TWO
        // l2Book calls (one for the old pre-wait skew preflight, one for the
        // trigger poll); the relocated implementation needs only one.
        _book.assert_async().await;
    }

    /// Issue #3: live + a custom `HL_INFO_URL`/`HL_EXCHANGE_URL` override
    /// (without `--allow-custom-endpoints`) must be rejected before ANY
    /// network call — not even the `/info meta` call that normally happens
    /// first at startup. The mock server below registers NO routes at all,
    /// so any network call would surface as a mockito 501 rather than the
    /// expected endpoint-override error, proving the rejection happens
    /// strictly before the first request.
    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn issue3_live_custom_endpoint_without_override_flag_rejected_before_any_network_call() {
        let server = mockito::Server::new_async().await;
        // No mocks registered — a network call here would 501.

        std::env::set_var("HL_AGENT_PK", TEST_PK);
        std::env::set_var("HL_AGENT_ADDRESS", AGENT);
        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));

        let cli = Cli::try_parse_from([
            "hype-twap",
            "--symbol",
            "HYPE",
            "--side",
            "long",
            "--usd",
            "1500",
            "--duration",
            "5m",
            "--max-notional-usd",
            "5000",
            "--read-only",
            "false",
            // Deliberately NOT passing --allow-custom-endpoints.
        ])
        .unwrap();

        let result = run_with_cli(cli).await;

        std::env::remove_var("HL_AGENT_PK");
        std::env::remove_var("HL_AGENT_ADDRESS");
        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");

        let err = result.expect_err("a live custom endpoint override must be rejected");
        assert!(err.contains("--allow-custom-endpoints"), "{err}");
    }

    // === Issue #4: state-dir / journal / incomplete-run tests ===

    /// Hand-rolled temp-dir guard (no `tempfile` dependency, matching
    /// `src/journal.rs`'s own test module).
    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new() -> Self {
            let dir =
                std::env::temp_dir().join(format!("hype-twap-main-test-{}", uuid::Uuid::now_v7()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// **Finding 2 regression test.** Before the fix, a grace-timeout
    /// `FinalReport` hardcoded `filled_total: "0"` and
    /// `outcome_unknown_cloids: []` regardless of what the run had actually
    /// done — silently discarding real fills and hiding genuinely-unresolved
    /// cloids from the operator. This test seeds a journal with a prior
    /// Terminal fill (slice 1, 3 HYPE) AND a still-unresolved
    /// SubmittedUnknown cloid (slice 2) — i.e. exactly the state the journal
    /// would be in if the grace timer fired while slice 2 was ambiguous —
    /// then asserts `grace_timeout_report_fields` (the function the
    /// grace-timeout branch in `run_with_cli` calls) reports the REAL
    /// filled total and the REAL unresolved cloid, not zero/empty.
    #[test]
    fn grace_timeout_report_fields_reflect_actual_journal_state_not_zero_or_empty() {
        let tmp = TempDir::new();
        let state_dir = tmp.path().join("state");
        let filled_cloid = hype_trigger_twap::types::Cloid::new();
        let unresolved_cloid = hype_trigger_twap::types::Cloid::new();

        let mut j = hype_trigger_twap::journal::ExecutionJournal::start(
            &state_dir,
            "run-grace-timeout".into(),
            hype_trigger_twap::journal::RunHeader {
                run_id: "run-grace-timeout".into(),
                network: "testnet".into(),
                agent: Some(hype_trigger_twap::types::Address::new(AGENT)),
                master: Some(hype_trigger_twap::types::Address::new(MASTER)),
                symbol: hype_trigger_twap::types::Symbol::new("HYPE"),
                side: hype_trigger_twap::types::Side::Long,
                slices: 2,
                plan_hash: "irrelevant".into(),
                started_at_unix_ms: 0,
            },
        )
        .unwrap();
        j.record(&hype_trigger_twap::journal::JournalRecord::Prepared {
            slice_idx: 1,
            cloid: filled_cloid,
            nonce: None,
            symbol: hype_trigger_twap::types::Symbol::new("HYPE"),
            side: hype_trigger_twap::types::Side::Long,
            px: "50".into(),
            sz: "3".into(),
        })
        .unwrap();
        j.record(&hype_trigger_twap::journal::JournalRecord::Terminal {
            slice_idx: 1,
            cloid: filled_cloid,
            status: "filled".into(),
            filled_sz: "3".into(),
            avg_px: Some("50".into()),
        })
        .unwrap();
        j.record(&hype_trigger_twap::journal::JournalRecord::Prepared {
            slice_idx: 2,
            cloid: unresolved_cloid,
            nonce: None,
            symbol: hype_trigger_twap::types::Symbol::new("HYPE"),
            side: hype_trigger_twap::types::Side::Long,
            px: "50".into(),
            sz: "2".into(),
        })
        .unwrap();
        j.record(
            &hype_trigger_twap::journal::JournalRecord::SubmittedUnknown {
                slice_idx: 2,
                cloid: unresolved_cloid,
            },
        )
        .unwrap();
        // No FinalReport: this is the still-open state a grace-timeout
        // would fire against.

        let (filled_total, outcome_unknown_cloids) =
            grace_timeout_report_fields(&state_dir, "run-grace-timeout");

        assert_eq!(
            filled_total,
            rust_decimal::Decimal::from(3),
            "must reflect the ACTUAL prior fill (3), not the hardcoded 0"
        );
        assert_eq!(
            outcome_unknown_cloids,
            vec![unresolved_cloid],
            "must list the ACTUAL still-unresolved cloid, not an empty list"
        );
    }

    fn live_cli(extra: &[&str], state_dir: &std::path::Path) -> Vec<String> {
        let mut args: Vec<String> = vec![
            "hype-twap".into(),
            "--symbol".into(),
            "HYPE".into(),
            "--side".into(),
            "long".into(),
            "--usd".into(),
            "50".into(),
            "--duration".into(),
            "2s".into(),
            "--slices".into(),
            "1".into(),
            "--max-notional-usd".into(),
            "1000000".into(),
            "--allow-custom-endpoints".into(),
            "--read-only".into(),
            "false".into(),
            "--state-dir".into(),
            state_dir.display().to_string(),
        ];
        args.extend(extra.iter().map(|s| s.to_string()));
        args
    }

    /// Registers the full mock set a complete one-slice live run needs: meta,
    /// userRole, l2Book (served for both the startup snapshot and the
    /// pre-flight/slice-loop fetch — `.expect_at_least(1)` since exactly how
    /// many times it is called is an implementation detail this test does
    /// not pin), and one filled `/exchange` response.
    async fn mock_full_live_run(server: &mut mockito::ServerGuard) {
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({"type": "meta"})))
            .with_status(200)
            .with_body(r#"{"universe":[{"name":"HYPE","szDecimals":2,"maxLeverage":10,"onlyIsolated":false}]}"#)
            .create_async()
            .await;
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "userRole"}),
            ))
            .with_status(200)
            .with_body(format!(
                r#"{{"role":"agent","data":{{"user":"{MASTER}"}}}}"#
            ))
            .create_async()
            .await;
        let now_ms = chrono::Utc::now().timestamp_millis();
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "l2Book"}),
            ))
            .with_status(200)
            .with_body(book_body_at("HYPE", "49.9", "50.1", now_ms))
            .expect_at_least(1)
            .create_async()
            .await;
        server
            .mock("POST", "/exchange")
            .with_status(200)
            .with_body(
                r#"{"status":"ok","response":{"type":"order","data":{"statuses":[
                    {"filled":{"oid":1,"totalSz":"1","avgPx":"50"}}]}}}"#,
            )
            .create_async()
            .await;
    }

    /// Read-only regression (Issue #4 acceptance criterion): a read-only run
    /// must create NEITHER the state directory NOR a journal file, even when
    /// `--state-dir` points at a path that does not exist yet.
    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn read_only_creates_no_state_dir_or_journal_file() {
        let tmp = TempDir::new();
        let state_dir = tmp.path().join("would-be-state-dir");
        assert!(!state_dir.exists());

        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({"type": "meta"})))
            .with_status(200)
            .with_body(r#"{"universe":[{"name":"HYPE","szDecimals":2,"maxLeverage":10,"onlyIsolated":false}]}"#)
            .create_async()
            .await;
        let now_ms = chrono::Utc::now().timestamp_millis();
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "l2Book"}),
            ))
            .with_status(200)
            .with_body(book_body_at("HYPE", "49.9", "50.1", now_ms))
            .create_async()
            .await;

        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));

        let cli = Cli::try_parse_from([
            "hype-twap",
            "--symbol",
            "HYPE",
            "--side",
            "long",
            "--usd",
            "50",
            "--duration",
            "2s",
            "--slices",
            "1",
            "--state-dir",
            &state_dir.display().to_string(),
            "--read-only",
            "true",
        ])
        .unwrap();

        let _ = run_with_cli(cli).await;

        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");

        assert!(
            !state_dir.exists(),
            "a read-only run must never create the state directory"
        );
    }

    /// A complete one-slice LIVE run creates the state dir and a journal
    /// file containing a Header record and a FinalReport (completed: true).
    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn live_run_creates_state_dir_and_a_completed_journal() {
        let tmp = TempDir::new();
        let state_dir = tmp.path().join("state");

        let mut server = mockito::Server::new_async().await;
        mock_full_live_run(&mut server).await;

        std::env::set_var("HL_AGENT_PK", TEST_PK);
        std::env::set_var("HL_AGENT_ADDRESS", AGENT);
        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));

        let args = live_cli(&[], &state_dir);
        let cli = Cli::try_parse_from(&args).unwrap();
        let result = run_with_cli(cli).await;

        std::env::remove_var("HL_AGENT_PK");
        std::env::remove_var("HL_AGENT_ADDRESS");
        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");

        result.expect("a fully-mocked one-slice live run must succeed");
        assert!(state_dir.exists(), "a live run must create the state dir");

        let runs_dir = state_dir.join("runs");
        let run_ids: Vec<_> = std::fs::read_dir(&runs_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().into_string().unwrap())
            .collect();
        assert_eq!(run_ids.len(), 1, "exactly one run directory: {run_ids:?}");

        let records =
            hype_trigger_twap::journal::ExecutionJournal::read_all(&state_dir, &run_ids[0])
                .unwrap();
        assert!(
            matches!(
                records[0],
                hype_trigger_twap::journal::JournalRecord::Header(_)
            ),
            "the first record must be the run header: {records:?}"
        );
        assert!(
            records.iter().any(|r| matches!(
                r,
                hype_trigger_twap::journal::JournalRecord::FinalReport {
                    completed: true,
                    ..
                }
            )),
            "a completed run must end with a completed FinalReport: {records:?}"
        );
    }

    /// Issue #4 acceptance criterion: an incomplete run for the same
    /// network+agent must block a brand-new overlapping live run (no
    /// `--resume`, no `--abandon-incomplete-run`).
    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn incomplete_run_blocks_a_new_live_run_without_resume_or_abandon() {
        let tmp = TempDir::new();
        let state_dir = tmp.path().join("state");

        // Seed an incomplete run directly via the journal API (no
        // FinalReport, one unresolved SubmittedUnknown cloid) — simulating a
        // prior process that crashed mid-run.
        {
            let mut j = hype_trigger_twap::journal::ExecutionJournal::start(
                &state_dir,
                "prior-incomplete-run".into(),
                hype_trigger_twap::journal::RunHeader {
                    run_id: "prior-incomplete-run".into(),
                    network: "testnet".into(),
                    agent: Some(hype_trigger_twap::types::Address::new(AGENT)),
                    master: Some(hype_trigger_twap::types::Address::new(MASTER)),
                    symbol: hype_trigger_twap::types::Symbol::new("HYPE"),
                    side: hype_trigger_twap::types::Side::Long,
                    slices: 1,
                    plan_hash: "irrelevant".into(),
                    started_at_unix_ms: 0,
                },
            )
            .unwrap();
            j.record(
                &hype_trigger_twap::journal::JournalRecord::SubmittedUnknown {
                    slice_idx: 1,
                    cloid: hype_trigger_twap::types::Cloid::new(),
                },
            )
            .unwrap();
        }

        let server = mockito::Server::new_async().await; // no mocks: must never be reached

        std::env::set_var("HL_AGENT_PK", TEST_PK);
        std::env::set_var("HL_AGENT_ADDRESS", AGENT);
        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));

        let args = live_cli(&["--network", "testnet"], &state_dir);
        let cli = Cli::try_parse_from(&args).unwrap();
        let result = run_with_cli(cli).await;

        std::env::remove_var("HL_AGENT_PK");
        std::env::remove_var("HL_AGENT_ADDRESS");
        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");

        let err = result.expect_err("a new overlapping live run must be refused");
        assert!(err.contains("incomplete run"), "{err}");
        assert!(err.contains("prior-incomplete-run"), "{err}");
    }

    /// Issue #5 acceptance criterion 1: a second live process for the SAME
    /// network+agent must fail BEFORE any order is placed. Simulated here by
    /// pre-acquiring the lock directly (standing in for "another process
    /// already holds it") and then calling `run_with_cli` — no `/exchange`
    /// mock is registered, so any attempt to place an order would panic the
    /// mock server, proving the failure happens before send.
    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn second_live_process_for_the_same_agent_fails_before_any_order() {
        let tmp = TempDir::new();
        let state_dir = tmp.path().join("state");

        let key = hype_trigger_twap::lock::lock_key(
            "testnet",
            &hype_trigger_twap::types::Address::new(AGENT),
        );
        let held_lock = hype_trigger_twap::lock::ProcessLock::acquire(
            &state_dir,
            &key,
            &hype_trigger_twap::lock::LockMetadata::new("first holder, still running"),
        )
        .expect("first acquire (simulating the already-running process) must succeed");

        // No mocks registered at all: /info and /exchange must never be hit.
        let server = mockito::Server::new_async().await;

        std::env::set_var("HL_AGENT_PK", TEST_PK);
        std::env::set_var("HL_AGENT_ADDRESS", AGENT);
        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));

        let args = live_cli(&["--network", "testnet"], &state_dir);
        let cli = Cli::try_parse_from(&args).unwrap();
        let result = run_with_cli(cli).await;

        std::env::remove_var("HL_AGENT_PK");
        std::env::remove_var("HL_AGENT_ADDRESS");
        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");

        let err = result.expect_err("a second live process for the same network+agent must fail");
        assert!(
            err.contains("already holds the writer lock"),
            "expected a lock-contention error, got: {err}"
        );

        drop(held_lock);
    }

    /// Issue #5 acceptance criterion 2 (part 1): a DIFFERENT agent must run
    /// concurrently unaffected by another agent's held lock.
    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn different_agent_runs_concurrently_unaffected_by_another_agents_lock() {
        const OTHER_AGENT: &str = "0x00000000000000000000000000000000000bad";
        let tmp = TempDir::new();
        let state_dir = tmp.path().join("state");

        let other_key = hype_trigger_twap::lock::lock_key(
            "testnet",
            &hype_trigger_twap::types::Address::new(OTHER_AGENT),
        );
        let _held_by_other = hype_trigger_twap::lock::ProcessLock::acquire(
            &state_dir,
            &other_key,
            &hype_trigger_twap::lock::LockMetadata::new("a different agent's run"),
        )
        .unwrap();

        let mut server = mockito::Server::new_async().await;
        mock_full_live_run(&mut server).await;

        std::env::set_var("HL_AGENT_PK", TEST_PK);
        std::env::set_var("HL_AGENT_ADDRESS", AGENT);
        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));

        let args = live_cli(&["--network", "testnet"], &state_dir);
        let cli = Cli::try_parse_from(&args).unwrap();
        let result = run_with_cli(cli).await;

        std::env::remove_var("HL_AGENT_PK");
        std::env::remove_var("HL_AGENT_ADDRESS");
        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");

        result.expect(
            "a different agent's live run must succeed even while another agent's lock is held",
        );
    }

    /// Issue #5 acceptance criterion 2 (part 2): a read-only process must run
    /// concurrently unaffected by a held live lock for the SAME agent, and
    /// must not itself touch the lock/state dir at all.
    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn read_only_runs_concurrently_unaffected_by_a_held_live_lock_for_the_same_agent() {
        let tmp = TempDir::new();
        let state_dir = tmp.path().join("state");

        let key = hype_trigger_twap::lock::lock_key(
            "testnet",
            &hype_trigger_twap::types::Address::new(AGENT),
        );
        let _held = hype_trigger_twap::lock::ProcessLock::acquire(
            &state_dir,
            &key,
            &hype_trigger_twap::lock::LockMetadata::new("a live run for the same agent"),
        )
        .unwrap();

        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({"type": "meta"})))
            .with_status(200)
            .with_body(r#"{"universe":[{"name":"HYPE","szDecimals":2,"maxLeverage":10,"onlyIsolated":false}]}"#)
            .create_async()
            .await;
        let now_ms = chrono::Utc::now().timestamp_millis();
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "l2Book"}),
            ))
            .with_status(200)
            .with_body(book_body_at("HYPE", "49.9", "50.1", now_ms))
            .create_async()
            .await;

        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));

        let cli = Cli::try_parse_from([
            "hype-twap",
            "--symbol",
            "HYPE",
            "--side",
            "long",
            "--usd",
            "50",
            "--duration",
            "2s",
            "--slices",
            "1",
            "--network",
            "testnet",
            "--state-dir",
            &state_dir.display().to_string(),
            "--read-only",
            "true",
        ])
        .unwrap();

        let result = run_with_cli(cli).await;

        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");

        result.expect(
            "a read-only run must succeed even while a live lock is held for the same network+agent",
        );
    }

    /// Issue #5 acceptance criterion 3: even in a "stale lock" scenario (the
    /// held lock's owner is simulated as gone by dropping it before the new
    /// process starts, so acquisition succeeds), the incomplete-run
    /// reconciliation flow that Task 7/#4 already implemented is NOT
    /// skipped — lock acquisition happens strictly BEFORE incomplete-run
    /// detection in `run_with_cli`, so a stale lock's recovery can never
    /// shortcut past reconciliation. This test proves the ordering by
    /// combining both fixtures: a prior incomplete run (unresolved
    /// `SubmittedUnknown` cloid) AND a lock that has just been released
    /// (simulating the crash that left both behind) — the resuming process
    /// must still be forced through reconciliation (verified by the
    /// resolved cloid ending up Terminal in the journal), not just allowed
    /// to barrel past it because the lock happened to be free.
    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn stale_lock_recovery_does_not_skip_incomplete_run_reconciliation() {
        let tmp = TempDir::new();
        let state_dir = tmp.path().join("state");
        let prior_cloid = hype_trigger_twap::types::Cloid::new();

        // Simulate the crashed process: it held the lock, then died (flock
        // auto-releases on process death — modeled here by simply dropping
        // the guard) leaving an incomplete run behind.
        {
            let key = hype_trigger_twap::lock::lock_key(
                "testnet",
                &hype_trigger_twap::types::Address::new(AGENT),
            );
            let crashed_holder = hype_trigger_twap::lock::ProcessLock::acquire(
                &state_dir,
                &key,
                &hype_trigger_twap::lock::LockMetadata::new("the crashed process"),
            )
            .unwrap();
            drop(crashed_holder); // simulates process death releasing the flock
        }

        std::env::set_var("HL_AGENT_PK", TEST_PK);
        std::env::set_var("HL_AGENT_ADDRESS", AGENT);

        // Same plan_hash-matching trick as `resume_reconciles_the_incomplete_
        // cloid_before_continuing`: derive the real plan_hash via a throwaway
        // probe run rather than hand-computing DefaultHasher output.
        let probe_state_dir = tmp.path().join("probe-state");
        {
            let mut probe_server = mockito::Server::new_async().await;
            mock_full_live_run(&mut probe_server).await;
            std::env::set_var("HL_INFO_URL", format!("{}/info", probe_server.url()));
            std::env::set_var(
                "HL_EXCHANGE_URL",
                format!("{}/exchange", probe_server.url()),
            );
            let probe_args = live_cli(&["--network", "testnet"], &probe_state_dir);
            let probe_cli = Cli::try_parse_from(&probe_args).unwrap();
            run_with_cli(probe_cli)
                .await
                .expect("probe run must succeed to derive a real plan_hash");
            std::env::remove_var("HL_INFO_URL");
            std::env::remove_var("HL_EXCHANGE_URL");
        }
        let probe_run_id = std::fs::read_dir(probe_state_dir.join("runs"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .file_name()
            .into_string()
            .unwrap();
        let probe_records =
            hype_trigger_twap::journal::ExecutionJournal::read_all(&probe_state_dir, &probe_run_id)
                .unwrap();
        let plan_hash = match &probe_records[0] {
            hype_trigger_twap::journal::JournalRecord::Header(h) => h.plan_hash.clone(),
            other => panic!("expected Header, got {other:?}"),
        };

        // Seed the incomplete run this process will --resume, with the same
        // plan_hash derived above.
        {
            let mut j = hype_trigger_twap::journal::ExecutionJournal::start(
                &state_dir,
                "stale-lock-incomplete-run".into(),
                hype_trigger_twap::journal::RunHeader {
                    run_id: "stale-lock-incomplete-run".into(),
                    network: "testnet".into(),
                    agent: Some(hype_trigger_twap::types::Address::new(AGENT)),
                    master: Some(hype_trigger_twap::types::Address::new(MASTER)),
                    symbol: hype_trigger_twap::types::Symbol::new("HYPE"),
                    side: hype_trigger_twap::types::Side::Long,
                    slices: 1,
                    plan_hash,
                    started_at_unix_ms: 0,
                },
            )
            .unwrap();
            j.record(&hype_trigger_twap::journal::JournalRecord::Prepared {
                slice_idx: 1,
                cloid: prior_cloid,
                nonce: None,
                symbol: hype_trigger_twap::types::Symbol::new("HYPE"),
                side: hype_trigger_twap::types::Side::Long,
                px: "50".into(),
                sz: "1".into(),
            })
            .unwrap();
            j.record(
                &hype_trigger_twap::journal::JournalRecord::SubmittedUnknown {
                    slice_idx: 1,
                    cloid: prior_cloid,
                },
            )
            .unwrap();
        }

        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({"type": "meta"})))
            .with_status(200)
            .with_body(r#"{"universe":[{"name":"HYPE","szDecimals":2,"maxLeverage":10,"onlyIsolated":false}]}"#)
            .create_async()
            .await;
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "userRole"}),
            ))
            .with_status(200)
            .with_body(format!(
                r#"{{"role":"agent","data":{{"user":"{MASTER}"}}}}"#
            ))
            .create_async()
            .await;
        // Reconciliation queries orderStatus for the unresolved cloid: report
        // it as filled, so the reconciled Terminal record is unambiguous.
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "orderStatus", "oid": prior_cloid.to_hex_string()}),
            ))
            .with_status(200)
            .with_body(format!(
                r#"{{"status":"order","order":{{"order":{{"oid":42,"coin":"HYPE","side":"B","cloid":"{prior_cloid}","origSz":"1","sz":"0","avgPx":"50"}},"status":"filled","statusTimestamp":0}}}}"#
            ))
            .create_async()
            .await;
        let now_ms = chrono::Utc::now().timestamp_millis();
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "l2Book"}),
            ))
            .with_status(200)
            .with_body(book_body_at("HYPE", "49.9", "50.1", now_ms))
            .expect_at_least(0)
            .create_async()
            .await;
        server
            .mock("POST", "/exchange")
            .with_status(200)
            .with_body(
                r#"{"status":"ok","response":{"type":"order","data":{"statuses":[
                    {"filled":{"oid":2,"totalSz":"1","avgPx":"50"}}]}}}"#,
            )
            .expect_at_least(0)
            .create_async()
            .await;

        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));

        let mut args = live_cli(&["--network", "testnet"], &state_dir);
        args.push("--resume".into());
        args.push("stale-lock-incomplete-run".into());
        let cli = Cli::try_parse_from(&args).unwrap();
        let result = run_with_cli(cli).await;

        std::env::remove_var("HL_AGENT_PK");
        std::env::remove_var("HL_AGENT_ADDRESS");
        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");

        result.expect("resuming after a stale-lock scenario must succeed");

        let records = hype_trigger_twap::journal::ExecutionJournal::read_all(
            &state_dir,
            "stale-lock-incomplete-run",
        )
        .unwrap();
        assert!(
            records.iter().any(|r| matches!(
                r,
                hype_trigger_twap::journal::JournalRecord::Terminal { cloid, .. }
                    if *cloid == prior_cloid
            )),
            "reconciliation must have resolved the prior unresolved cloid to Terminal, \
             proving the stale-lock recovery path did not skip it: {records:?}"
        );
    }

    /// `--resume <run-id>` on an incomplete run force-reconciles its
    /// unresolved cloid via `orderStatus`, then CONTINUES the run for the
    /// remaining plan — proving the resumed run's own journal ends up with
    /// the reconciled cloid resolved to Terminal, not left dangling.
    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn resume_reconciles_the_incomplete_cloid_before_continuing() {
        let tmp = TempDir::new();
        let state_dir = tmp.path().join("state");
        let prior_cloid = hype_trigger_twap::types::Cloid::new();

        std::env::set_var("HL_AGENT_PK", TEST_PK);
        std::env::set_var("HL_AGENT_ADDRESS", AGENT);

        // Issue #4's --resume verifies the stored plan_hash against a
        // freshly recomputed one before resuming, so the fixture seeded
        // below must carry the SAME hash `live_cli`'s resolved TwapPlan will
        // compute at runtime. Rather than hand-reconstruct rust_decimal's
        // exact string formatting (fragile / easy to drift from the real
        // formatter), derive it from a real probe run: start a fresh live
        // run with the SAME `live_cli` args against a throwaway state dir,
        // let it write its own header, then read the plan_hash back out.
        let probe_state_dir = tmp.path().join("probe-state");
        {
            let mut probe_server = mockito::Server::new_async().await;
            mock_full_live_run(&mut probe_server).await;
            std::env::set_var("HL_INFO_URL", format!("{}/info", probe_server.url()));
            std::env::set_var(
                "HL_EXCHANGE_URL",
                format!("{}/exchange", probe_server.url()),
            );
            let probe_args = live_cli(&["--network", "testnet"], &probe_state_dir);
            run_with_cli(Cli::try_parse_from(&probe_args).unwrap())
                .await
                .expect("probe run to derive plan_hash must succeed");
        }
        let probe_run_id = std::fs::read_dir(probe_state_dir.join("runs"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .file_name()
            .into_string()
            .unwrap();
        let probe_records =
            hype_trigger_twap::journal::ExecutionJournal::read_all(&probe_state_dir, &probe_run_id)
                .unwrap();
        let matching_plan_hash = probe_records
            .iter()
            .find_map(|r| match r {
                hype_trigger_twap::journal::JournalRecord::Header(h) => Some(h.plan_hash.clone()),
                _ => None,
            })
            .unwrap();

        {
            let mut j = hype_trigger_twap::journal::ExecutionJournal::start(
                &state_dir,
                "run-to-resume".into(),
                hype_trigger_twap::journal::RunHeader {
                    run_id: "run-to-resume".into(),
                    network: "testnet".into(),
                    agent: Some(hype_trigger_twap::types::Address::new(AGENT)),
                    master: Some(hype_trigger_twap::types::Address::new(MASTER)),
                    symbol: hype_trigger_twap::types::Symbol::new("HYPE"),
                    side: hype_trigger_twap::types::Side::Long,
                    slices: 1,
                    plan_hash: matching_plan_hash,
                    started_at_unix_ms: 0,
                },
            )
            .unwrap();
            j.record(&hype_trigger_twap::journal::JournalRecord::Prepared {
                slice_idx: 1,
                cloid: prior_cloid,
                nonce: None,
                symbol: hype_trigger_twap::types::Symbol::new("HYPE"),
                side: hype_trigger_twap::types::Side::Long,
                px: "50".into(),
                sz: "1".into(),
            })
            .unwrap();
            j.record(
                &hype_trigger_twap::journal::JournalRecord::SubmittedUnknown {
                    slice_idx: 1,
                    cloid: prior_cloid,
                },
            )
            .unwrap();
        }

        let mut server = mockito::Server::new_async().await;
        mock_full_live_run(&mut server).await;
        // The forced reconciliation for the PRIOR cloid: orderStatus by
        // cloid must report it as terminal/filled so no resend occurs.
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "orderStatus", "oid": prior_cloid.to_hex_string()}),
            ))
            .with_status(200)
            .with_body(format!(
                r#"{{"status":"order","order":{{"order":{{"oid":42,"coin":"HYPE","side":"B","cloid":"{prior_cloid}","origSz":"1","sz":"0","avgPx":"50"}},"status":"filled","statusTimestamp":0}}}}"#
            ))
            .create_async()
            .await;

        std::env::set_var("HL_AGENT_PK", TEST_PK);
        std::env::set_var("HL_AGENT_ADDRESS", AGENT);
        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));

        let args = live_cli(
            &["--network", "testnet", "--resume", "run-to-resume"],
            &state_dir,
        );
        let cli = Cli::try_parse_from(&args).unwrap();
        let result = run_with_cli(cli).await;

        std::env::remove_var("HL_AGENT_PK");
        std::env::remove_var("HL_AGENT_ADDRESS");
        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");

        result.expect("resume of a fully-mocked incomplete run must succeed");

        let records =
            hype_trigger_twap::journal::ExecutionJournal::read_all(&state_dir, "run-to-resume")
                .unwrap();
        let summary = hype_trigger_twap::journal::summarize(&records);
        assert!(
            summary.unresolved_cloids().is_empty(),
            "the resumed cloid must be resolved, not left dangling: {records:?}"
        );
    }

    /// `--usd`/`--slices` args for a TWO-slice plan (`$100` at mid `50` →
    /// 2 HYPE total, 1 HYPE/slice) — used by the Finding 1 regression test
    /// below, which needs a resumable run with a real remainder left after
    /// one slice's worth of prior fill.
    fn live_cli_two_slice(extra: &[&str], state_dir: &std::path::Path) -> Vec<String> {
        let mut args: Vec<String> = vec![
            "hype-twap".into(),
            "--symbol".into(),
            "HYPE".into(),
            "--side".into(),
            "long".into(),
            "--usd".into(),
            "100".into(),
            "--duration".into(),
            "2s".into(),
            "--slices".into(),
            "2".into(),
            "--max-notional-usd".into(),
            "1000000".into(),
            "--allow-custom-endpoints".into(),
            "--read-only".into(),
            "false".into(),
            "--state-dir".into(),
            state_dir.display().to_string(),
        ];
        args.extend(extra.iter().map(|s| s.to_string()));
        args
    }

    /// **Finding 1 (CRITICAL) regression test.** Before the fix, `--resume`
    /// rebuilt the ORIGINAL plan (`total_adjusted` = the full 2-slice
    /// target) and started `run_twap_journaled` fresh at `slice_idx=1` with
    /// zero in-memory `filled` — so a prior run's slice-1 fill (already
    /// journaled `Terminal`, therefore excluded from
    /// `unresolved_cloids()` and invisible to forced reconciliation) was
    /// silently re-executed on top of, doubling the total placed.
    ///
    /// Seeds a prior journal with BOTH:
    /// - a `Terminal`, partially-filled cloid for slice 1 (1 of 2 HYPE,
    ///   simulating "slice 1 filled, then the process crashed") — this is
    ///   the path Finding 1 is specifically about: a fill the ORIGINAL
    ///   implementation's `--resume` tests never seeded, so this exact bug
    ///   was never exercised or caught before now.
    /// - a `SubmittedUnknown` cloid for slice 2 (simulating "slice 2 was
    ///   sent, then the process crashed before reading the response") — so
    ///   both the reconciliation path AND the continuation-plan path are
    ///   exercised together in one run, per the finding's explicit
    ///   requirement.
    ///
    /// Asserts, via a mockito body-matcher + `.expect(1)` call-count on
    /// `/exchange`, that the resumed run places EXACTLY ONE new order of
    /// size `1` (the remainder: 2 HYPE total − 1 HYPE already filled), never
    /// the full original 2 HYPE — and that the final accounting (prior
    /// journaled fill + this run's own newly-executed fill) sums to exactly
    /// the original 2 HYPE target, each fill counted exactly once.
    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn resume_continues_only_the_remainder_not_the_full_original_plan() {
        let tmp = TempDir::new();
        let state_dir = tmp.path().join("state");
        let slice1_cloid = hype_trigger_twap::types::Cloid::new();
        let slice2_cloid = hype_trigger_twap::types::Cloid::new();

        std::env::set_var("HL_AGENT_PK", TEST_PK);
        std::env::set_var("HL_AGENT_ADDRESS", AGENT);

        // Derive the real plan_hash for this 2-slice/$100 plan the same way
        // the existing --resume tests do: a throwaway probe run against its
        // own state dir, reading the hash back out of its own header.
        let probe_state_dir = tmp.path().join("probe-state");
        {
            let mut probe_server = mockito::Server::new_async().await;
            mock_full_live_run(&mut probe_server).await;
            std::env::set_var("HL_INFO_URL", format!("{}/info", probe_server.url()));
            std::env::set_var(
                "HL_EXCHANGE_URL",
                format!("{}/exchange", probe_server.url()),
            );
            let probe_args = live_cli_two_slice(&["--network", "testnet"], &probe_state_dir);
            run_with_cli(Cli::try_parse_from(&probe_args).unwrap())
                .await
                .expect("probe run to derive plan_hash must succeed");
        }
        let probe_run_id = std::fs::read_dir(probe_state_dir.join("runs"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .file_name()
            .into_string()
            .unwrap();
        let probe_records =
            hype_trigger_twap::journal::ExecutionJournal::read_all(&probe_state_dir, &probe_run_id)
                .unwrap();
        let matching_plan_hash = probe_records
            .iter()
            .find_map(|r| match r {
                hype_trigger_twap::journal::JournalRecord::Header(h) => Some(h.plan_hash.clone()),
                _ => None,
            })
            .unwrap();

        // Seed the incomplete run: slice 1 already Terminal/filled (1 HYPE),
        // slice 2 SubmittedUnknown (ambiguous — needs reconciliation).
        {
            let mut j = hype_trigger_twap::journal::ExecutionJournal::start(
                &state_dir,
                "run-to-resume-partial".into(),
                hype_trigger_twap::journal::RunHeader {
                    run_id: "run-to-resume-partial".into(),
                    network: "testnet".into(),
                    agent: Some(hype_trigger_twap::types::Address::new(AGENT)),
                    master: Some(hype_trigger_twap::types::Address::new(MASTER)),
                    symbol: hype_trigger_twap::types::Symbol::new("HYPE"),
                    side: hype_trigger_twap::types::Side::Long,
                    slices: 2,
                    plan_hash: matching_plan_hash,
                    started_at_unix_ms: 0,
                },
            )
            .unwrap();
            j.record(&hype_trigger_twap::journal::JournalRecord::Prepared {
                slice_idx: 1,
                cloid: slice1_cloid,
                nonce: None,
                symbol: hype_trigger_twap::types::Symbol::new("HYPE"),
                side: hype_trigger_twap::types::Side::Long,
                px: "50".into(),
                sz: "1".into(),
            })
            .unwrap();
            j.record(&hype_trigger_twap::journal::JournalRecord::Terminal {
                slice_idx: 1,
                cloid: slice1_cloid,
                status: "filled".into(),
                filled_sz: "1".into(),
                avg_px: Some("50".into()),
            })
            .unwrap();
            j.record(&hype_trigger_twap::journal::JournalRecord::Prepared {
                slice_idx: 2,
                cloid: slice2_cloid,
                nonce: None,
                symbol: hype_trigger_twap::types::Symbol::new("HYPE"),
                side: hype_trigger_twap::types::Side::Long,
                px: "50".into(),
                sz: "1".into(),
            })
            .unwrap();
            j.record(
                &hype_trigger_twap::journal::JournalRecord::SubmittedUnknown {
                    slice_idx: 2,
                    cloid: slice2_cloid,
                },
            )
            .unwrap();
        }

        // NOTE: deliberately NOT reusing `mock_full_live_run` here (unlike
        // the other --resume tests) — its `/exchange` mock has no body
        // matcher and no `.expect()`, so mockito treats it as perpetually
        // "missing hits" and prefers it over ANY later, more specific
        // `/exchange` mock regardless of registration order. This test
        // needs the `/exchange` mock itself to be the size assertion, so
        // meta/userRole/l2Book are registered individually instead.
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({"type": "meta"})))
            .with_status(200)
            .with_body(r#"{"universe":[{"name":"HYPE","szDecimals":2,"maxLeverage":10,"onlyIsolated":false}]}"#)
            .create_async()
            .await;
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "userRole"}),
            ))
            .with_status(200)
            .with_body(format!(
                r#"{{"role":"agent","data":{{"user":"{MASTER}"}}}}"#
            ))
            .create_async()
            .await;
        let now_ms = chrono::Utc::now().timestamp_millis();
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "l2Book"}),
            ))
            .with_status(200)
            .with_body(book_body_at("HYPE", "49.9", "50.1", now_ms))
            .expect_at_least(1)
            .create_async()
            .await;
        // Forced reconciliation for slice 2's ambiguous cloid: HL reports it
        // as never received (unknownOid), so it resolves to a zero-fill
        // Terminal, NOT a resend on this codepath (resume/abandon never
        // resend — only a live run's own place_slice_reconciled may).
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "orderStatus", "oid": slice2_cloid.to_hex_string()}),
            ))
            .with_status(200)
            .with_body(r#"{"status":"unknownOid"}"#)
            .expect_at_least(2) // W1 policy requires >=2 consecutive unknownOid observations
            .create_async()
            .await;
        // The continuation's own new place: MUST be size 1 (the remainder:
        // 2 total - 1 already filled), never size 2 (the full original
        // total). `.expect(1)`: exactly one new order is placed — asserted
        // explicitly below via `.assert_async()`, since mockito's
        // `.expect(n)` is only CHECKED when a caller asks it to (it is not
        // enforced automatically on drop). This is the ONLY `/exchange`
        // mock on this server, so an unexpected size-2 (full-original-plan)
        // body would simply 501 rather than silently match a fallback.
        let exchange_mock = server
            .mock("POST", "/exchange")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({
                "action": { "orders": [{ "s": "1" }] }
            })))
            .with_status(200)
            .with_body(
                r#"{"status":"ok","response":{"type":"order","data":{"statuses":[
                    {"filled":{"oid":99,"totalSz":"1","avgPx":"50"}}]}}}"#,
            )
            .expect(1)
            .create_async()
            .await;

        std::env::set_var("HL_AGENT_PK", TEST_PK);
        std::env::set_var("HL_AGENT_ADDRESS", AGENT);
        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));

        let args = live_cli_two_slice(
            &["--network", "testnet", "--resume", "run-to-resume-partial"],
            &state_dir,
        );
        let cli = Cli::try_parse_from(&args).unwrap();
        let result = run_with_cli(cli).await;

        std::env::remove_var("HL_AGENT_PK");
        std::env::remove_var("HL_AGENT_ADDRESS");
        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");

        result.expect("resume continuing only the remainder must succeed");

        // Explicitly assert the `/exchange` mock's call-count expectation:
        // exactly one new order was placed, sized to the remainder only.
        // The pre-fix double-execution bug (placing the full original 2
        // HYPE on top of the already-filled 1 HYPE) would either fail
        // body-matching (a size-2 order would not match this mock's
        // `s: "1"` matcher, surfacing as an unmatched-request 501) or, if
        // some other body shape happened to match, would trip THIS
        // assertion by calling the mock more than once.
        exchange_mock.assert_async().await;

        // Final accounting: prior journaled fill (1, slice 1) + this run's
        // own newly-executed fill (1, slice 2's remainder) sums to EXACTLY
        // the original 2 HYPE target — each fill counted exactly once.
        let records = hype_trigger_twap::journal::ExecutionJournal::read_all(
            &state_dir,
            "run-to-resume-partial",
        )
        .unwrap();
        let summary = hype_trigger_twap::journal::summarize(&records);
        assert_eq!(
            summary.total_filled(),
            rust_decimal::Decimal::from(2),
            "resumed run must account for exactly the original 2 HYPE target, \
             each fill counted once: {records:?}"
        );
        assert!(
            summary.unresolved_cloids().is_empty(),
            "every cloid must be resolved by the end of the resumed run: {records:?}"
        );
    }

    /// `--resume <run-id>` must be rejected when this invocation's plan
    /// (sizing/timing/risk parameters) does not match the plan_hash stored
    /// in the run being resumed — protects against silently reinterpreting
    /// the remaining schedule of a run under different parameters than it
    /// was started with.
    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn resume_is_rejected_when_the_plan_hash_does_not_match() {
        let tmp = TempDir::new();
        let state_dir = tmp.path().join("state");
        let prior_cloid = hype_trigger_twap::types::Cloid::new();

        {
            let mut j = hype_trigger_twap::journal::ExecutionJournal::start(
                &state_dir,
                "run-mismatched".into(),
                hype_trigger_twap::journal::RunHeader {
                    run_id: "run-mismatched".into(),
                    network: "testnet".into(),
                    agent: Some(hype_trigger_twap::types::Address::new(AGENT)),
                    master: Some(hype_trigger_twap::types::Address::new(MASTER)),
                    symbol: hype_trigger_twap::types::Symbol::new("HYPE"),
                    side: hype_trigger_twap::types::Side::Long,
                    slices: 1,
                    plan_hash: "this-will-never-match-a-real-hash".into(),
                    started_at_unix_ms: 0,
                },
            )
            .unwrap();
            j.record(&hype_trigger_twap::journal::JournalRecord::Prepared {
                slice_idx: 1,
                cloid: prior_cloid,
                nonce: None,
                symbol: hype_trigger_twap::types::Symbol::new("HYPE"),
                side: hype_trigger_twap::types::Side::Long,
                px: "50".into(),
                sz: "1".into(),
            })
            .unwrap();
            j.record(
                &hype_trigger_twap::journal::JournalRecord::SubmittedUnknown {
                    slice_idx: 1,
                    cloid: prior_cloid,
                },
            )
            .unwrap();
        }

        // Reconciliation (which the brief requires to run BEFORE any
        // plan-consistency check, since it must never be skipped) is mocked
        // to succeed quickly via orderStatus; meta/userRole/l2Book are
        // mocked so sizing can complete and reach the plan_hash comparison.
        // NO `/exchange` mock is registered: the mismatch must be caught
        // before any NEW place is attempted, so a place attempt here would
        // 501 and fail this test as intended evidence that it never happened.
        let mut server = mockito::Server::new_async().await;
        mock_full_live_run(&mut server).await;
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "orderStatus", "oid": prior_cloid.to_hex_string()}),
            ))
            .with_status(200)
            .with_body(format!(
                r#"{{"status":"order","order":{{"order":{{"oid":42,"coin":"HYPE","side":"B","cloid":"{prior_cloid}","origSz":"1","sz":"0","avgPx":"50"}},"status":"filled","statusTimestamp":0}}}}"#
            ))
            .create_async()
            .await;

        std::env::set_var("HL_AGENT_PK", TEST_PK);
        std::env::set_var("HL_AGENT_ADDRESS", AGENT);
        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));

        let args = live_cli(
            &["--network", "testnet", "--resume", "run-mismatched"],
            &state_dir,
        );
        let cli = Cli::try_parse_from(&args).unwrap();
        let result = run_with_cli(cli).await;

        std::env::remove_var("HL_AGENT_PK");
        std::env::remove_var("HL_AGENT_ADDRESS");
        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");

        let err = result.expect_err("a plan_hash mismatch must reject --resume");
        assert!(err.contains("does not match"), "{err}");
        assert!(err.contains("plan_hash"), "{err}");
    }

    /// `--abandon-incomplete-run` force-reconciles the incomplete run's
    /// cloid, marks the run `Abandoned`, and does NOT continue/place
    /// anything new (the mock `/exchange` endpoint must never be hit).
    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn abandon_incomplete_run_reconciles_then_stops_without_placing_anything_new() {
        let tmp = TempDir::new();
        let state_dir = tmp.path().join("state");
        let prior_cloid = hype_trigger_twap::types::Cloid::new();

        {
            let mut j = hype_trigger_twap::journal::ExecutionJournal::start(
                &state_dir,
                "run-to-abandon".into(),
                hype_trigger_twap::journal::RunHeader {
                    run_id: "run-to-abandon".into(),
                    network: "testnet".into(),
                    agent: Some(hype_trigger_twap::types::Address::new(AGENT)),
                    master: Some(hype_trigger_twap::types::Address::new(MASTER)),
                    symbol: hype_trigger_twap::types::Symbol::new("HYPE"),
                    side: hype_trigger_twap::types::Side::Long,
                    slices: 1,
                    plan_hash: "irrelevant".into(),
                    started_at_unix_ms: 0,
                },
            )
            .unwrap();
            j.record(&hype_trigger_twap::journal::JournalRecord::Prepared {
                slice_idx: 1,
                cloid: prior_cloid,
                nonce: None,
                symbol: hype_trigger_twap::types::Symbol::new("HYPE"),
                side: hype_trigger_twap::types::Side::Long,
                px: "50".into(),
                sz: "1".into(),
            })
            .unwrap();
            j.record(
                &hype_trigger_twap::journal::JournalRecord::SubmittedUnknown {
                    slice_idx: 1,
                    cloid: prior_cloid,
                },
            )
            .unwrap();
        }

        let mut server = mockito::Server::new_async().await;
        // Only meta/userRole (startup) + orderStatus (forced reconciliation)
        // are mocked — NO /exchange mock, so a resend/new-place would 501.
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({"type": "meta"})))
            .with_status(200)
            .with_body(r#"{"universe":[{"name":"HYPE","szDecimals":2,"maxLeverage":10,"onlyIsolated":false}]}"#)
            .create_async()
            .await;
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "userRole"}),
            ))
            .with_status(200)
            .with_body(format!(
                r#"{{"role":"agent","data":{{"user":"{MASTER}"}}}}"#
            ))
            .create_async()
            .await;
        server
            .mock("POST", "/info")
            .match_body(mockito::Matcher::PartialJson(
                serde_json::json!({"type": "orderStatus", "oid": prior_cloid.to_hex_string()}),
            ))
            .with_status(200)
            .with_body(format!(
                r#"{{"status":"order","order":{{"order":{{"oid":42,"coin":"HYPE","side":"B","cloid":"{prior_cloid}","origSz":"1","sz":"1"}},"status":"canceled","statusTimestamp":0}}}}"#
            ))
            .create_async()
            .await;

        std::env::set_var("HL_AGENT_PK", TEST_PK);
        std::env::set_var("HL_AGENT_ADDRESS", AGENT);
        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));

        let args = live_cli(
            &["--network", "testnet", "--abandon-incomplete-run"],
            &state_dir,
        );
        let cli = Cli::try_parse_from(&args).unwrap();
        let result = run_with_cli(cli).await;

        std::env::remove_var("HL_AGENT_PK");
        std::env::remove_var("HL_AGENT_ADDRESS");
        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");

        result.expect("abandon of a fully-mocked incomplete run must succeed");

        let records =
            hype_trigger_twap::journal::ExecutionJournal::read_all(&state_dir, "run-to-abandon")
                .unwrap();
        let summary = hype_trigger_twap::journal::summarize(&records);
        assert!(
            summary.abandoned,
            "the run must be marked Abandoned: {records:?}"
        );
        assert!(
            summary.unresolved_cloids().is_empty(),
            "the abandoned cloid must still have been reconciled: {records:?}"
        );
    }

    // === Issue #4 / #10: no secrets in the journal (CLI-level audit) ===

    /// End-to-end audit: a completed live run's ENTIRE journal file, byte
    /// for byte, must never contain the private key, its raw hex digits, or
    /// the literal string "SecretString"/"private". This is in addition to
    /// `journal_never_serializes_secret_material` in `src/journal.rs` (which
    /// audits the record TYPES in isolation) — this test audits what an
    /// actual live run, driven through `HL_AGENT_PK`, writes to disk.
    #[tokio::test]
    #[serial_test::serial(hl_env_vars)]
    async fn live_run_journal_file_never_contains_the_private_key() {
        let tmp = TempDir::new();
        let state_dir = tmp.path().join("state");

        let mut server = mockito::Server::new_async().await;
        mock_full_live_run(&mut server).await;

        std::env::set_var("HL_AGENT_PK", TEST_PK);
        std::env::set_var("HL_AGENT_ADDRESS", AGENT);
        std::env::set_var("HL_INFO_URL", format!("{}/info", server.url()));
        std::env::set_var("HL_EXCHANGE_URL", format!("{}/exchange", server.url()));

        let args = live_cli(&[], &state_dir);
        let cli = Cli::try_parse_from(&args).unwrap();
        let result = run_with_cli(cli).await;

        std::env::remove_var("HL_AGENT_PK");
        std::env::remove_var("HL_AGENT_ADDRESS");
        std::env::remove_var("HL_INFO_URL");
        std::env::remove_var("HL_EXCHANGE_URL");

        result.expect("fully-mocked live run must succeed");

        let runs_dir = state_dir.join("runs");
        let run_id = std::fs::read_dir(&runs_dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .file_name()
            .into_string()
            .unwrap();
        let journal_path = runs_dir.join(&run_id).join("journal.jsonl");
        let raw = std::fs::read_to_string(&journal_path).unwrap();

        let pk_hex = TEST_PK.trim_start_matches("0x");
        assert!(
            !raw.to_lowercase().contains(&pk_hex.to_lowercase()),
            "journal file must never contain the raw private key hex"
        );
        for banned in ["secretstring", "private_key", "signature"] {
            assert!(
                !raw.to_lowercase().contains(banned),
                "journal file must never contain {banned:?}: {raw}"
            );
        }
    }
}
