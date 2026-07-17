//! Kalshi trading bot — CLI over the standalone execution engine.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use kalshi_trading_bot::{
    auth::Signer,
    client::{KalshiClient, ReqwestSender},
    config::Config,
    market_data::MarketData,
    models::{OrderType, PlannedOrder, Side},
    orders::{OrderOutcome, OrderPlacer},
    risk::RiskGuard,
    coinbase::Coinbase,
    strategy::{crypto_convergence, resolution_sniper},
    tui,
    venue,
};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "kalshi-bot")]
#[command(about = "Kalshi automated execution engine.", long_about = None)]
struct Cli {
    /// Path to the Kalshi config (YAML).
    #[arg(long, default_value = "config.yaml")]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Launch the live dashboard (balance, positions, breaker, mode).
    Tui,
    /// Run the Resolution Sniper strategy (continuous; respects dry-run gates).
    Snipe {
        /// Max pages (of 1000 markets) to scan per cycle.
        #[arg(long, default_value_t = 10)]
        max_pages: u32,
    },
    /// Run the Crypto Convergence strategy (continuous; respects dry-run gates).
    Crypto {
        #[arg(long, default_value_t = 10)]
        max_pages: u32,
    },
    /// Fetch account balance — the canonical auth check.
    Balance,
    /// List open markets.
    Markets {
        #[arg(long, default_value_t = 10)]
        limit: u32,
    },
    /// Show the orderbook for a market ticker.
    Orderbook { ticker: String },
    /// Show open positions.
    Positions,
    /// Place a limit order (respects dry-run gates).
    Buy {
        ticker: String,
        /// `yes` or `no`.
        #[arg(long, default_value = "yes")]
        side: String,
        #[arg(long)]
        count: u32,
        /// Limit price in cents (1..=99).
        #[arg(long)]
        price: u8,
    },
}

fn build_client(cfg: &Config) -> Result<KalshiClient<ReqwestSender>> {
    let signer = Signer::new(&cfg.credentials.api_key_id, &cfg.credentials.private_key)
        .context("building request signer from credentials")?;
    let sender = ReqwestSender::new(tokio::runtime::Handle::current());
    Ok(KalshiClient::new(&cfg.api.base_url, signer, sender))
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

/// Current local time as `HH:MM:SS` for the dashboard's refresh stamp.
fn now_hms() -> String {
    use time::format_description::well_known::Iso8601;
    let now = time::OffsetDateTime::now_local()
        .unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    now.format(&Iso8601::DEFAULT)
        .ok()
        .and_then(|s| s.split('T').nth(1).map(|t| t.chars().take(8).collect()))
        .unwrap_or_else(|| "--:--:--".into())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_target(false)
        .init();

    info!(venue = venue::NAME, kind = venue::VENUE_TYPE, "starting {} execution engine", venue::NAME);

    let cli = Cli::parse();
    let cfg = Config::load(&cli.config).context("loading configuration")?;

    if !cfg.live_trading_allowed() {
        info!(
            enable_trading = cfg.enable_trading,
            mock_trading = cfg.mock_trading,
            "DRY-RUN: orders will be computed and logged, not sent"
        );
    } else {
        warn!("LIVE TRADING ENABLED — orders will be sent to Kalshi");
    }

    // The reqwest sender needs the tokio runtime handle; run the (blocking)
    // engine calls on a blocking thread so we don't block the runtime.
    let cfg2 = cfg.clone();
    tokio::task::spawn_blocking(move || run_command(cli.command, cfg2))
        .await
        .context("engine task panicked")?
}

fn run_command(command: Command, cfg: Config) -> Result<()> {
    let client = build_client(&cfg)?;
    let md = MarketData::new(&client);

    match command {
        Command::Tui => {
            let live = cfg.live_trading_allowed();
            tui::run::<ReqwestSender, _>(live, || {
                tui::refresh_snapshot(&md, &now_hms())
            })?;
        }
        Command::Snipe { max_pages } => {
            let sniper_cfg = match &cfg.strategies.resolution_sniper {
                Some(c) => c.clone(),
                None => anyhow::bail!(
                    "no `strategies.resolution_sniper` block in config.yaml — add one to run the sniper"
                ),
            };
            let live = cfg.live_trading_allowed();
            let risk = cfg.risk.clone();
            resolution_sniper::run(
                &md,
                || (RiskGuard::new(risk.clone()), live),
                &sniper_cfg,
                max_pages,
                now_secs,
            )?;
        }
        Command::Crypto { max_pages } => {
            let crypto_cfg = match &cfg.strategies.crypto_convergence {
                Some(c) => c.clone(),
                None => anyhow::bail!(
                    "no `strategies.crypto_convergence` block in config.yaml — add one to run it"
                ),
            };
            let live = cfg.live_trading_allowed();
            let risk = cfg.risk.clone();
            let coinbase = Coinbase::new(tokio::runtime::Handle::current());
            crypto_convergence::run(
                &md,
                &coinbase,
                || (RiskGuard::new(risk.clone()), live),
                &crypto_cfg,
                max_pages,
                now_secs,
            )?;
        }
        Command::Balance => {
            let b = md.balance()?;
            info!(usd = b.usd(), "account balance");
            println!("Balance: ${:.2}", b.usd());
        }
        Command::Markets { limit } => {
            let markets = md.markets(limit)?;
            println!("{} markets:", markets.len());
            for m in markets {
                println!(
                    "  {:<24} {:<40} bid={:?} ask={:?} [{}]",
                    m.ticker, m.title, m.yes_bid, m.yes_ask, m.status
                );
            }
        }
        Command::Orderbook { ticker } => {
            let ob = md.orderbook(&ticker)?;
            println!(
                "Orderbook {} — best yes ask {:?}c (buy-depth ${:.2}), best no ask {:?}c (buy-depth ${:.2})",
                ob.ticker,
                ob.best_yes_ask_cents(),
                ob.yes_buy_depth_usd(),
                ob.best_no_ask_cents(),
                ob.no_buy_depth_usd()
            );
            println!("  YES: {:?}", ob.yes);
            println!("  NO:  {:?}", ob.no);
        }
        Command::Positions => {
            let ps = md.positions()?;
            println!("{} open positions:", ps.len());
            for p in ps {
                println!(
                    "  {:<40} shares={} cost=${:.2} exposure=${:.2} fees=${:.4}",
                    p.ticker, p.shares, p.cost_usd, p.exposure_usd, p.fees_usd
                );
            }
        }
        Command::Buy { ticker, side, count, price } => {
            let side = match side.as_str() {
                "yes" => Side::Yes,
                "no" => Side::No,
                other => anyhow::bail!("side must be 'yes' or 'no', got '{other}'"),
            };
            let order = PlannedOrder {
                ticker: ticker.clone(),
                side,
                order_type: OrderType::Limit,
                count,
                price_cents: price,
            };
            // Depth guard needs the live book.
            let book = md.orderbook(&ticker)?;
            let mut guard = RiskGuard::new(cfg.risk.clone());
            let mut placer = OrderPlacer::new(&client, &mut guard, cfg.live_trading_allowed());
            match placer.place(&order, &book, now_secs())? {
                OrderOutcome::Rejected(r) => println!("REJECTED by risk guard: {r:?}"),
                OrderOutcome::DryRun { payload } => {
                    println!("DRY-RUN — would send:\n  {payload}");
                }
                OrderOutcome::Sent { response } => println!("SENT. Response: {response}"),
            }
        }
    }
    Ok(())
}
