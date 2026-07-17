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
    /// Dump raw fills + settlements JSON (read-only) to inspect the wire shape.
    History {
        #[arg(long, default_value_t = 100)]
        limit: u32,
    },
    /// Edge analysis: join fills to settlements, measure realized win rate vs
    /// implied entry price per market family, and flag statistically meaningful
    /// edge (read-only).
    Analyze {
        /// Page size for the paginated fetches.
        #[arg(long, default_value_t = 200)]
        limit: u32,
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
        Command::Analyze { limit } => {
            use std::collections::HashMap;

            // Page a paginated portfolio endpoint fully into a Vec of rows.
            let page_all = |kind: &str, arr_key: &str| -> Result<Vec<serde_json::Value>> {
                let mut cursor: Option<String> = None;
                let mut out = Vec::new();
                loop {
                    let page = match kind {
                        "fills" => md.fills_raw(limit, cursor.as_deref())?,
                        _ => md.settlements_raw(limit, cursor.as_deref())?,
                    };
                    if let Some(a) = page.get(arr_key).and_then(|x| x.as_array()) {
                        out.extend(a.iter().cloned());
                    }
                    match page.get("cursor").and_then(|c| c.as_str()) {
                        Some(c) if !c.is_empty() => cursor = Some(c.to_string()),
                        _ => break,
                    }
                    if out.len() > 20000 { break; }
                }
                Ok(out)
            };

            let fills = page_all("fills", "fills")?;
            let settlements = page_all("settlements", "settlements")?;

            let f_str = |v: &serde_json::Value, k: &str| -> f64 {
                v.get(k).and_then(|x| x.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0)
            };

            // Per-ticker entry: total contracts + cost (to derive implied price).
            // Only YES buys (the sniper/directional pattern); mixed sides are rare
            // here and would need per-side handling.
            struct Entry { contracts: f64, cost: f64 }
            let mut entries: HashMap<String, Entry> = HashMap::new();
            for fill in &fills {
                let ticker = fill.get("ticker").and_then(|t| t.as_str()).unwrap_or("").to_string();
                if ticker.is_empty() { continue; }
                let count = f_str(fill, "count_fp");
                let price = f_str(fill, "yes_price_dollars");
                let e = entries.entry(ticker).or_insert(Entry { contracts: 0.0, cost: 0.0 });
                e.contracts += count;
                e.cost += count * price;
            }

            // Family = ticker up to the first digit (e.g. KXNBAGAME, KXWCGAME).
            let family = |t: &str| -> String {
                let mut s = String::new();
                for ch in t.chars() {
                    if ch.is_ascii_digit() { break; }
                    s.push(ch);
                }
                s.trim_end_matches('-').to_string()
            };

            // Accumulate per family: bets, wins, summed implied price, net pnl.
            struct Fam { n: u32, wins: u32, implied_sum: f64, cost: f64, revenue: f64 }
            let mut fams: HashMap<String, Fam> = HashMap::new();
            for s in &settlements {
                let ticker = s.get("ticker").and_then(|t| t.as_str()).unwrap_or("");
                if ticker.is_empty() { continue; }
                let revenue = s.get("revenue").and_then(|x| x.as_f64()).unwrap_or(0.0) / 100.0;
                let won = revenue > 0.0001;
                // Implied entry price from the matched fills, if we have them.
                let (implied, cost) = match entries.get(ticker) {
                    Some(e) if e.contracts > 0.0 => (e.cost / e.contracts, e.cost),
                    _ => (f64::NAN, f_str(s, "yes_total_cost_dollars") + f_str(s, "no_total_cost_dollars")),
                };
                let fam = fams.entry(family(ticker)).or_insert(Fam { n: 0, wins: 0, implied_sum: 0.0, cost: 0.0, revenue: 0.0 });
                fam.n += 1;
                if won { fam.wins += 1; }
                if implied.is_finite() { fam.implied_sum += implied; }
                fam.cost += cost;
                fam.revenue += revenue;
            }

            // Report, sorted by net pnl. Edge = win_rate - avg_implied_price.
            // Significance: 2 standard errors of a proportion; flag when the
            // observed edge exceeds it AND n is not tiny.
            let mut list: Vec<(&String, &Fam)> = fams.iter().collect();
            list.sort_by(|a, b| (b.1.revenue - b.1.cost).partial_cmp(&(a.1.revenue - a.1.cost)).unwrap());

            println!("{:<26} {:>4} {:>7} {:>8} {:>7} {:>8} {}",
                "family", "n", "win%", "implied%", "edge", "net$", "verdict");
            for (name, f) in list {
                let n = f.n as f64;
                let win_rate = f.wins as f64 / n;
                let implied = if f.implied_sum > 0.0 { f.implied_sum / n } else { f64::NAN };
                let edge = win_rate - implied;
                let net = f.revenue - f.cost;
                let se = (win_rate * (1.0 - win_rate) / n).sqrt();
                let verdict = if !implied.is_finite() {
                    "no entry data"
                } else if f.n < 30 {
                    "too few samples"
                } else if edge > 2.0 * se {
                    "SIGNIFICANT edge"
                } else if edge < -2.0 * se {
                    "significant NEGATIVE"
                } else {
                    "no clear edge"
                };
                println!("{:<26} {:>4} {:>6.1} {:>8} {:>+6.1} {:>+8.2}  {}",
                    name, f.n, 100.0 * win_rate,
                    if implied.is_finite() { format!("{:.1}", 100.0*implied) } else { "  ?".into() },
                    100.0 * edge, net, verdict);
            }
            println!("\nEdge = your actual win% minus the price you paid (the market's implied %).");
            println!("Positive edge with >=30 samples and edge > 2*SE is a real lead worth pursuing.");
        }
        Command::History { limit } => {
            // Page through ALL settlements (that's where realized P&L lives).
            let mut cursor: Option<String> = None;
            let mut rows: Vec<serde_json::Value> = Vec::new();
            loop {
                let page = md.settlements_raw(limit, cursor.as_deref())?;
                if let Some(arr) = page.get("settlements").and_then(|s| s.as_array()) {
                    rows.extend(arr.iter().cloned());
                }
                match page.get("cursor").and_then(|c| c.as_str()) {
                    Some(c) if !c.is_empty() => cursor = Some(c.to_string()),
                    _ => break,
                }
                if rows.len() > 5000 { break; } // safety bound
            }

            let dollars = |v: &serde_json::Value, k: &str| -> f64 {
                v.get(k).and_then(|x| x.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0)
            };
            let cents = |v: &serde_json::Value, k: &str| -> f64 {
                v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0) / 100.0
            };

            let (mut total_cost, mut total_revenue, mut total_fees) = (0.0, 0.0, 0.0);
            let (mut wins, mut losses) = (0u32, 0u32);
            println!("{:<44} {:>8} {:>8} {:>8} {:>8}", "market", "cost", "revenue", "fee", "pnl");
            for s in &rows {
                let cost = dollars(s, "yes_total_cost_dollars") + dollars(s, "no_total_cost_dollars");
                let revenue = cents(s, "revenue");
                let fee = dollars(s, "fee_cost");
                let pnl = revenue - cost - fee;
                if pnl >= 0.0 { wins += 1; } else { losses += 1; }
                total_cost += cost;
                total_revenue += revenue;
                total_fees += fee;
                let ticker = s.get("ticker").and_then(|t| t.as_str()).unwrap_or("?");
                println!("{:<44} {:>8.2} {:>8.2} {:>8.4} {:>+8.2}", ticker, cost, revenue, fee, pnl);
            }
            let net = total_revenue - total_cost - total_fees;
            println!("\n===== REALIZED P&L ({} settled markets) =====", rows.len());
            println!("  total paid (cost):   ${:.2}", total_cost);
            println!("  total received:      ${:.2}", total_revenue);
            println!("  total fees:          ${:.4}", total_fees);
            println!("  NET REALIZED P&L:    ${:+.2}", net);
            println!("  win / loss markets:  {} / {}", wins, losses);
            if total_cost > 0.0 {
                println!("  return on cost:      {:+.1}%", 100.0 * net / total_cost);
            }
        }
    }
    Ok(())
}
