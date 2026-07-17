//! Crypto convergence — trades Kalshi crypto threshold markets near expiry when
//! the underlying's distance from the strike is large relative to how far it can
//! realistically move in the time remaining.
//!
//! Pure core (`norm_cdf`, `evaluate`) + thin `run` loop, mirroring the sniper.

use crate::client::RequestSender;
use crate::config::CryptoConfig;
use crate::coinbase::Coinbase;
use crate::market_data::MarketData;
use crate::models::{OrderType, PlannedOrder, Side};
use crate::orders::{OrderOutcome, OrderPlacer};
use crate::risk::RiskGuard;
use crate::strategy::resolution_sniper::SniperState;
use anyhow::Result;
use std::collections::HashMap;
use std::time::Duration;
use tracing::{info, warn};

/// A bet the strategy wants to make.
#[derive(Debug, Clone, PartialEq)]
pub struct CryptoSignal {
    pub ticker: String,
    pub side: Side,
    /// Limit price in cents for the chosen side.
    pub price_cents: u8,
    /// Modeled edge in cents (model prob - market price).
    pub edge_cents: f64,
    /// Model probability the bet resolves in our favor (0..1).
    pub model_prob: f64,
}

/// Standard normal CDF via the Abramowitz-Stegun erf approximation.
pub fn norm_cdf(z: f64) -> f64 {
    0.5 * (1.0 + erf(z / std::f64::consts::SQRT_2))
}

/// erf approximation (Abramowitz & Stegun 7.1.26), max error ~1.5e-7.
fn erf(x: f64) -> f64 {
    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let x = x.abs();
    let t = 1.0 / (1.0 + 0.3275911 * x);
    let y = 1.0
        - (((((1.061405429 * t - 1.453152027) * t) + 1.421413741) * t - 0.284496736) * t
            + 0.254829592)
            * t
            * (-x * x).exp();
    sign * y
}

/// Inputs for evaluating one market. `yes_ask_cents`/`no_ask_cents` are the real
/// best asks (cents) to BUY each side, from the live orderbook.
pub struct MarketQuote {
    pub ticker: String,
    pub strike: f64,
    pub minutes_to_close: f64,
    pub yes_ask_cents: Option<u8>,
    pub no_ask_cents: Option<u8>,
}

/// The pure decision. Returns a bet if the modeled edge on the near-certain side
/// clears `min_edge_cents`, else `None`.
pub fn evaluate(
    q: &MarketQuote,
    spot: f64,
    sigma_1min: f64,
    cfg: &CryptoConfig,
) -> Option<CryptoSignal> {
    if q.minutes_to_close <= 0.0 || q.minutes_to_close > cfg.max_minutes_to_close {
        return None;
    }
    if spot <= 0.0 || q.strike <= 0.0 || sigma_1min <= 0.0 {
        return None;
    }

    let sigma_remaining = sigma_1min * q.minutes_to_close.sqrt() * cfg.safety_factor;
    if sigma_remaining <= 0.0 {
        return None;
    }

    // Signed cushion: positive when spot is above strike.
    let cushion = (spot - q.strike) / spot;
    let z = cushion / sigma_remaining;
    // Probability spot ends >= strike (i.e. YES resolves true).
    let prob_yes = norm_cdf(z);

    // Bet the near-certain side.
    if spot > q.strike {
        // YES near-certain. Edge = model prob - price we'd pay.
        let ask = q.yes_ask_cents?;
        let edge = prob_yes * 100.0 - ask as f64;
        if edge >= cfg.min_edge_cents {
            return Some(CryptoSignal {
                ticker: q.ticker.clone(),
                side: Side::Yes,
                price_cents: ask,
                edge_cents: edge,
                model_prob: prob_yes,
            });
        }
    } else {
        // NO near-certain. prob_no = 1 - prob_yes.
        let ask = q.no_ask_cents?;
        let prob_no = 1.0 - prob_yes;
        let edge = prob_no * 100.0 - ask as f64;
        if edge >= cfg.min_edge_cents {
            return Some(CryptoSignal {
                ticker: q.ticker.clone(),
                side: Side::No,
                price_cents: ask,
                edge_cents: edge,
                model_prob: prob_no,
            });
        }
    }
    None
}

/// Parse the strike price from a Kalshi crypto ticker like `KXBTCD-...-T63499.99`.
pub fn parse_strike(ticker: &str) -> Option<f64> {
    ticker.rsplit("-T").next()?.parse().ok()
}

/// Which configured symbol (if any) a ticker belongs to.
pub fn symbol_of<'a>(ticker: &str, symbols: &'a [String]) -> Option<&'a String> {
    symbols.iter().find(|s| ticker.contains(s.as_str()))
}

#[allow(clippy::too_many_arguments)]
pub fn run<S: RequestSender>(
    md: &MarketData<'_, S>,
    coinbase: &Coinbase,
    placer_factory: impl Fn() -> (RiskGuard, bool),
    cfg: &CryptoConfig,
    max_scan_pages: u32,
    now_secs: impl Fn() -> u64,
) -> Result<()> {
    if !cfg.enabled {
        info!("crypto_convergence disabled in config");
        return Ok(());
    }

    let mut state = SniperState::new();
    match md.positions() {
        Ok(positions) => {
            for p in &positions {
                if p.cost_usd > 0.0 {
                    state.record(&p.ticker, p.cost_usd);
                }
            }
            info!(deployed = state.total_deployed(), "seeded from held positions");
        }
        Err(e) => warn!(error = %e, "could not seed positions"),
    }

    info!(
        symbols = ?cfg.symbols,
        max_minutes = cfg.max_minutes_to_close,
        min_edge_cents = cfg.min_edge_cents,
        "crypto convergence started"
    );

    let window_secs = (cfg.max_minutes_to_close * 60.0) as u64;

    loop {
        if state.remaining_total(cfg.max_total_budget_usd) <= 0.0 {
            info!(deployed = state.total_deployed(), "budget exhausted");
        } else if let Err(e) = scan_once(md, coinbase, &placer_factory, cfg, &mut state, max_scan_pages, window_secs, &now_secs) {
            warn!(error = %e, "scan failed");
        }
        std::thread::sleep(Duration::from_secs(cfg.scan_interval_secs));
    }
}

#[allow(clippy::too_many_arguments)]
fn scan_once<S: RequestSender>(
    md: &MarketData<'_, S>,
    coinbase: &Coinbase,
    placer_factory: &impl Fn() -> (RiskGuard, bool),
    cfg: &CryptoConfig,
    state: &mut SniperState,
    max_scan_pages: u32,
    window_secs: u64,
    now_secs: &impl Fn() -> u64,
) -> Result<()> {
    let now = now_secs();
    let markets = md.markets_closing_within(window_secs, 100.0, max_scan_pages, now)?;

    // Fetch spot + vol once per symbol we actually see.
    let mut spot: HashMap<String, f64> = HashMap::new();
    let mut vol: HashMap<String, f64> = HashMap::new();
    for sym in &cfg.symbols {
        if let (Ok(s), Ok(v)) = (
            coinbase.spot(sym),
            coinbase.realized_vol_1min(sym, cfg.vol_lookback_minutes),
        ) {
            spot.insert(sym.clone(), s);
            vol.insert(sym.clone(), v);
        }
    }

    let mut evaluated = 0;
    let mut signals = 0;
    for m in &markets {
        if m.status != "active" {
            continue;
        }
        let Some(sym) = symbol_of(&m.ticker, &cfg.symbols) else { continue };
        let (Some(&s), Some(&v)) = (spot.get(sym), vol.get(sym)) else { continue };
        let Some(strike) = parse_strike(&m.ticker) else { continue };

        // Real minutes-to-close from the market's close_time; skip if unknown.
        let Some(minutes) = m.minutes_to_close(now) else { continue };
        if minutes <= 0.0 || minutes > cfg.max_minutes_to_close {
            continue;
        }
        let book = match md.orderbook(&m.ticker) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let q = MarketQuote {
            ticker: m.ticker.clone(),
            strike,
            minutes_to_close: minutes,
            yes_ask_cents: book.best_yes_ask_cents(),
            no_ask_cents: book.best_no_ask_cents(),
        };
        evaluated += 1;
        if let Some(sig) = evaluate(&q, s, v, cfg) {
            signals += 1;
            place_signal(md, placer_factory, cfg, state, &sig, &book, now)?;
        }
    }
    info!(scanned = markets.len(), evaluated, signals, deployed = state.total_deployed(), "crypto scan complete");
    Ok(())
}

fn place_signal<S: RequestSender>(
    md: &MarketData<'_, S>,
    placer_factory: &impl Fn() -> (RiskGuard, bool),
    cfg: &CryptoConfig,
    state: &mut SniperState,
    sig: &CryptoSignal,
    book: &crate::models::Orderbook,
    now: u64,
) -> Result<()> {
    let global_left = state.remaining_total(cfg.max_total_budget_usd);
    let ticker_left = state.remaining_for(&sig.ticker, cfg.max_per_market_usd);
    let spend = cfg.per_trade_budget_usd.min(global_left).min(ticker_left);
    let price_usd = sig.price_cents as f64 / 100.0;
    if price_usd <= 0.0 {
        return Ok(());
    }
    let count = (spend / price_usd).floor() as u32;
    if count == 0 {
        return Ok(());
    }
    let order = PlannedOrder {
        ticker: sig.ticker.clone(),
        side: sig.side,
        order_type: OrderType::Limit,
        count,
        price_cents: sig.price_cents,
    };
    let (mut guard, live) = placer_factory();
    let mut placer = OrderPlacer::new(md.client(), &mut guard, live);
    match placer.place(&order, book, now)? {
        OrderOutcome::Sent { .. } => {
            state.record(&sig.ticker, order.notional_usd());
            info!(ticker = %sig.ticker, side = ?sig.side, price = sig.price_cents, edge = sig.edge_cents, prob = sig.model_prob, "CRYPTO BET placed");
        }
        OrderOutcome::DryRun { payload } => {
            state.record(&sig.ticker, order.notional_usd());
            info!(ticker = %sig.ticker, edge = sig.edge_cents, prob = sig.model_prob, %payload, "DRY-RUN crypto bet");
        }
        OrderOutcome::Rejected(r) => {
            info!(ticker = %sig.ticker, reason = ?r, "crypto bet rejected by guard");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> CryptoConfig {
        CryptoConfig {
            enabled: true,
            symbols: vec!["BTC".into(), "ETH".into()],
            max_minutes_to_close: 30.0,
            min_edge_cents: 3.0,
            vol_lookback_minutes: 60,
            safety_factor: 1.0,
            per_trade_budget_usd: 2.0,
            max_per_market_usd: 4.0,
            max_total_budget_usd: 15.0,
            scan_interval_secs: 20,
        }
    }

    // ---- norm_cdf ----

    #[test]
    fn norm_cdf_known_values() {
        assert!((norm_cdf(0.0) - 0.5).abs() < 1e-4);
        assert!((norm_cdf(1.645) - 0.95).abs() < 1e-3);
        assert!((norm_cdf(2.326) - 0.99).abs() < 1e-3);
        assert!((norm_cdf(-1.645) - 0.05).abs() < 1e-3);
        assert!(norm_cdf(5.0) > 0.9999);
    }

    // ---- parse_strike / symbol_of ----

    #[test]
    fn parses_strike_from_ticker() {
        assert_eq!(parse_strike("KXBTCD-26JUL1017-T63499.99"), Some(63499.99));
        assert_eq!(parse_strike("KXETHD-26JUL1017-T1779.99"), Some(1779.99));
    }

    #[test]
    fn identifies_symbol() {
        let syms = vec!["BTC".to_string(), "ETH".to_string()];
        assert_eq!(symbol_of("KXBTCD-x-T1", &syms).map(|s| s.as_str()), Some("BTC"));
        assert_eq!(symbol_of("KXETHD-x-T1", &syms).map(|s| s.as_str()), Some("ETH"));
        assert_eq!(symbol_of("KXWEATHER-x", &syms), None);
    }

    // ---- evaluate ----

    fn quote(strike: f64, minutes: f64, yes_ask: Option<u8>, no_ask: Option<u8>) -> MarketQuote {
        MarketQuote {
            ticker: "KXBTCD-x-T1".into(),
            strike,
            minutes_to_close: minutes,
            yes_ask_cents: yes_ask,
            no_ask_cents: no_ask,
        }
    }

    #[test]
    fn bets_yes_when_spot_far_above_strike_near_expiry_and_underpriced() {
        // spot 64100 vs strike 63500 = +0.94% cushion; 10 min; sigma_1min 0.0006
        // sigma_remaining = 0.0006*sqrt(10) = 0.0019 -> z ~ 4.9 -> prob ~1.0.
        // yes_ask 90c -> edge ~10c >= 3c -> bet.
        let q = quote(63500.0, 10.0, Some(90), Some(10));
        let sig = evaluate(&q, 64100.0, 0.0006, &cfg()).expect("should bet");
        assert_eq!(sig.side, Side::Yes);
        assert_eq!(sig.price_cents, 90);
        assert!(sig.edge_cents >= 3.0);
    }

    #[test]
    fn bets_no_when_spot_below_strike() {
        // spot 63000 vs strike 64000 -> NO near-certain. no_ask 90c, prob_no ~1 -> edge ~10c.
        let q = quote(64000.0, 10.0, Some(10), Some(90));
        let sig = evaluate(&q, 63000.0, 0.0006, &cfg()).expect("should bet no");
        assert_eq!(sig.side, Side::No);
    }

    #[test]
    fn no_bet_when_cushion_small_vs_volatility() {
        // spot barely above strike, far from expiry-scaled certainty.
        // cushion 0.05%, 30 min, sigma 0.001 -> sigma_rem 0.0055 -> z ~0.09 -> prob ~0.54.
        // yes_ask 53c -> edge ~1c < 3c -> no bet (correctly sees it as fair vol).
        let q = quote(63968.0, 30.0, Some(53), Some(47));
        assert!(evaluate(&q, 64000.0, 0.001, &cfg()).is_none());
    }

    #[test]
    fn no_bet_when_edge_below_min() {
        // Strong z (near-certain YES) but yes_ask already 99c -> edge ~1c < 3c.
        let q = quote(63000.0, 5.0, Some(99), Some(1));
        assert!(evaluate(&q, 64000.0, 0.0005, &cfg()).is_none());
    }

    #[test]
    fn no_bet_beyond_time_window() {
        // 60 min > max_minutes_to_close 30 -> skipped even with huge edge.
        let q = quote(63000.0, 60.0, Some(80), Some(20));
        assert!(evaluate(&q, 64000.0, 0.0005, &cfg()).is_none());
    }

    #[test]
    fn safety_factor_suppresses_marginal_bets() {
        // A bet that fires at safety_factor 1.0 should be suppressed at 5.0
        // (sigma inflated -> z shrinks -> prob drops -> edge below min).
        // spot 64000, strike 63500 (+0.78%), 15 min, sigma 0.0007:
        //   safety 1.0 -> z~2.88 -> prob~0.998 -> edge ~9.8c (fires)
        //   safety 5.0 -> z~0.58 -> prob~0.72 -> edge negative (suppressed)
        let q = quote(63500.0, 15.0, Some(90), Some(10));
        let base = cfg();
        let mut strict = cfg();
        strict.safety_factor = 5.0;
        let fired_base = evaluate(&q, 64000.0, 0.0007, &base).is_some();
        let fired_strict = evaluate(&q, 64000.0, 0.0007, &strict).is_some();
        assert!(fired_base, "should fire at safety 1.0");
        assert!(!fired_strict, "should be suppressed at safety 5.0");
    }
}
