//! Kalshi-native configuration. A single `config.yaml` — no Polymarket JSON.

use crate::risk::RiskConfig;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_venue")]
    pub venue: String,

    /// Dry-run gate 1: when false, orders are computed and logged but not sent.
    #[serde(default)]
    pub enable_trading: bool,

    /// Dry-run gate 2: when true, the order path returns early with a log line.
    /// Both flags must be permissive for a live order.
    #[serde(default = "default_true")]
    pub mock_trading: bool,

    pub credentials: Credentials,
    pub api: ApiConfig,
    pub risk: RiskConfig,

    /// Optional strategy configs. Absent = strategy unavailable.
    #[serde(default)]
    pub strategies: Strategies,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Strategies {
    pub resolution_sniper: Option<SniperConfig>,
    pub crypto_convergence: Option<CryptoConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CryptoConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Crypto symbols to trade, e.g. ["BTC", "ETH"].
    pub symbols: Vec<String>,
    /// Only trade markets closing within this many minutes.
    pub max_minutes_to_close: f64,
    /// Required edge in cents (model prob - market price) after fees.
    pub min_edge_cents: f64,
    /// Candle lookback (minutes) for realized-vol estimate.
    pub vol_lookback_minutes: usize,
    /// Multiply sigma by this; >1 = more conservative.
    #[serde(default = "default_safety_factor")]
    pub safety_factor: f64,
    pub per_trade_budget_usd: f64,
    pub max_per_market_usd: f64,
    pub max_total_budget_usd: f64,
    pub scan_interval_secs: u64,
}

fn default_safety_factor() -> f64 {
    1.0
}

#[derive(Debug, Clone, Deserialize)]
pub struct SniperConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Buy YES when `yes_ask` is at or above this many cents.
    pub threshold_cents: u8,
    /// USD to spend per individual snipe.
    pub per_snipe_budget_usd: f64,
    /// Ceiling on total USD deployed into any single market (allows re-buys).
    pub max_per_market_usd: f64,
    /// Global ceiling on USD deployed across all markets.
    pub max_total_budget_usd: f64,
    /// Seconds between scans.
    pub scan_interval_secs: u64,

    /// Only consider markets closing within this many seconds (near-resolution).
    #[serde(default = "default_close_window_secs")]
    pub close_window_secs: u64,

    /// Skip markets with less than this open interest (filters placeholders).
    #[serde(default = "default_min_open_interest")]
    pub min_open_interest: f64,

    /// Stop-loss: sell a held position when its best YES bid falls **below** this
    /// many cents. An absolute price floor (not a % of cost) so it ignores small
    /// spread/noise dips and fires only on a genuine reprice. `0` disables it.
    #[serde(default)]
    pub stop_loss_floor_cents: u8,

    /// Only trigger the stop after the bid has been below the floor for this many
    /// *consecutive* scans — filters one-tick noise. Defaults to 2.
    #[serde(default = "default_stop_confirm_scans")]
    pub stop_loss_confirm_scans: u32,

    /// Compounding mode. When set (> 0), the per-position cap becomes this
    /// fraction of the *current working budget* instead of the fixed
    /// `max_per_market_usd` — so as the budget grows, positions grow with it, and
    /// as it shrinks, they shrink. A loss is capped at this % of budget. E.g.
    /// `0.05` = each position is at most 5% of budget. `0.0` = disabled (use the
    /// fixed dollar caps).
    #[serde(default)]
    pub position_pct_of_budget: f64,

    /// Compounding target in USD. When compounding, the working budget is
    /// `min(real_cash_balance, target)`; once the budget reaches this the sniper
    /// stops opening new positions. `0.0` = no target (deploy up to real cash).
    #[serde(default)]
    pub compound_target_usd: f64,
}

fn default_close_window_secs() -> u64 {
    172_800 // 48h
}
fn default_min_open_interest() -> f64 {
    100.0
}
fn default_stop_confirm_scans() -> u32 {
    2
}

#[derive(Debug, Clone, Deserialize)]
pub struct Credentials {
    /// Kalshi API key ID (a UUID).
    pub api_key_id: String,
    /// RSA private key in PEM form (PKCS#1 or PKCS#8).
    pub private_key: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApiConfig {
    pub base_url: String,
}

fn default_venue() -> String {
    "kalshi".into()
}
fn default_true() -> bool {
    true
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config from {}", path.display()))?;
        let cfg: Config = serde_yaml::from_str(&raw).context("parsing config.yaml")?;
        Ok(cfg)
    }

    /// A live order may be sent only when both gates are permissive.
    pub fn live_trading_allowed(&self) -> bool {
        self.enable_trading && !self.mock_trading
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
venue: kalshi
enable_trading: false
mock_trading: true
credentials:
  api_key_id: "abc-123"
  private_key: |
    -----BEGIN RSA PRIVATE KEY-----
    MIIE...
    -----END RSA PRIVATE KEY-----
api:
  base_url: "https://api.elections.kalshi.com/trade-api/v2"
risk:
  min_orderbook_depth_usd: 250
  min_trade_size_usd: 5
  circuit_breaker:
    max_consecutive_large_trades: 5
    window_seconds: 60
    cooldown_seconds: 300
"#;

    #[test]
    fn parses_sample_config() {
        let cfg: Config = serde_yaml::from_str(SAMPLE).unwrap();
        assert_eq!(cfg.venue, "kalshi");
        assert_eq!(cfg.credentials.api_key_id, "abc-123");
        assert!(cfg.credentials.private_key.contains("BEGIN RSA PRIVATE KEY"));
        assert_eq!(cfg.risk.min_trade_size_usd, 5.0);
    }

    #[test]
    fn dry_run_by_default_blocks_live_trading() {
        let cfg: Config = serde_yaml::from_str(SAMPLE).unwrap();
        assert!(!cfg.live_trading_allowed());
    }

    #[test]
    fn live_requires_both_gates() {
        let mut cfg: Config = serde_yaml::from_str(SAMPLE).unwrap();
        cfg.enable_trading = true;
        cfg.mock_trading = true;
        assert!(!cfg.live_trading_allowed(), "mock still on -> blocked");
        cfg.mock_trading = false;
        assert!(cfg.live_trading_allowed(), "both permissive -> allowed");
    }
}
