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
use hype_trigger_twap::signer::{Eip712AgentSigner, Signer};
use hype_trigger_twap::trigger::{wait_for_trigger, TriggerConfig, TriggerReason, TriggerWhen};
use hype_trigger_twap::twap::{
    compute_sizing, fetch_fresh_book, run_twap, usd_to_coin, TwapPlan, MIN_NOTIONAL_USD,
    READ_ONLY_BANNER,
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

  HL_INFO_URL         Optional. Override the /info endpoint (testing).
  HL_EXCHANGE_URL     Optional. Override the /exchange endpoint (testing).
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

    /// Slippage cushion for the IOC limit price, in basis points.
    #[arg(long, default_value = "20")]
    slippage_bps: Decimal,

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
        if self.slippage_bps < Decimal::ZERO {
            return Err(format!(
                "--slippage-bps must be >= 0, got {}",
                self.slippage_bps
            ));
        }
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
    let cli = Cli::parse();
    cli.validate()?;

    let symbol = Symbol::new(&cli.symbol);
    let side: Side = cli.side.into();
    let network: Network = cli.network.into();

    if cli.read_only {
        println!("{READ_ONLY_BANNER}");
    } else {
        tracing::warn!("LIVE MODE: orders WILL be sent to {network}");
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

    let config = HlConfig::new(network).with_overrides(
        std::env::var("HL_INFO_URL").ok(),
        std::env::var("HL_EXCHANGE_URL").ok(),
    );
    let client = HlClient::new(config, signer).map_err(|e| e.to_string())?;

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
    let reason = wait_for_trigger(&client, &symbol, &trigger_cfg)
        .await
        .map_err(|e| format!("trigger wait: {e}"))?;
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
            fetch_fresh_book(&client, &symbol, cli.max_book_age_ms)
                .await
                .map_err(|e| format!("pre-flight l2Book: {e}"))?
        }
    };
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

    // §8: the loop.
    let plan = TwapPlan {
        symbol,
        side,
        asset_index: asset.asset_index,
        sz_decimals: asset.sz_decimals,
        per_slice: sizing.per_slice,
        total_adjusted: sizing.total_adjusted,
        total_requested: total_coin,
        slices: cli.slices,
        duration: cli.duration,
        slippage_bps: cli.slippage_bps,
        max_book_age_ms: cli.max_book_age_ms,
        read_only: cli.read_only,
        agent: agent_address,
        master,
    };
    let report = run_twap(&client, &plan).await;
    print!("{}", report.render());

    if report.exit_code() == 0 {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::FAILURE)
    }
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
}
