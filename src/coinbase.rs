//! Coinbase public market-data feed (no auth) — spot price and recent 1-minute
//! candles, plus realized-volatility estimation. Used by the crypto-convergence
//! strategy to model the true probability a threshold market resolves YES.

use anyhow::{Context, Result};
use tokio::runtime::Handle;

/// Fetches spot + candles for crypto symbols from Coinbase.
pub struct Coinbase {
    client: reqwest::Client,
    rt: Handle,
}

impl Coinbase {
    pub fn new(rt: Handle) -> Self {
        Self { client: reqwest::Client::new(), rt }
    }

    /// Current spot price for `symbol` (e.g. "BTC") in USD.
    pub fn spot(&self, symbol: &str) -> Result<f64> {
        let url = format!("https://api.coinbase.com/v2/prices/{symbol}-USD/spot");
        let client = self.client.clone();
        let body = self
            .rt
            .block_on(async move { client.get(&url).send().await?.text().await })
            .context("fetching Coinbase spot")?;
        let v: serde_json::Value = serde_json::from_str(&body).context("parsing spot json")?;
        v.get("data")
            .and_then(|d| d.get("amount"))
            .and_then(|a| a.as_str())
            .and_then(|s| s.parse().ok())
            .context("no spot amount in response")
    }

    /// Recent 1-minute candle **close** prices, newest first. Up to ~300.
    pub fn candles_1min(&self, symbol: &str) -> Result<Vec<f64>> {
        let url = format!(
            "https://api.exchange.coinbase.com/products/{symbol}-USD/candles?granularity=60"
        );
        let client = self.client.clone();
        let body = self
            .rt
            .block_on(async move {
                client.get(&url).header("User-Agent", "kalshi-bot").send().await?.text().await
            })
            .context("fetching Coinbase candles")?;
        let v: serde_json::Value = serde_json::from_str(&body).context("parsing candles json")?;
        // Each candle: [time, low, high, open, close, volume].
        let closes = v
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter_map(|c| c.get(4).and_then(|x| x.as_f64()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Ok(closes)
    }

    /// Realized 1-minute return volatility (stddev) from the last
    /// `lookback` candles.
    pub fn realized_vol_1min(&self, symbol: &str, lookback: usize) -> Result<f64> {
        let closes = self.candles_1min(symbol)?;
        Ok(realized_vol_from_closes(&closes, lookback))
    }
}

/// Stddev of 1-minute simple returns from `closes` (newest first), using at most
/// `lookback` intervals. Returns 0.0 if insufficient data.
pub fn realized_vol_from_closes(closes: &[f64], lookback: usize) -> f64 {
    let n = closes.len().min(lookback + 1);
    if n < 3 {
        return 0.0;
    }
    let window = &closes[..n];
    let rets: Vec<f64> = window
        .windows(2)
        .filter_map(|w| {
            if w[1] != 0.0 {
                Some((w[0] - w[1]) / w[1])
            } else {
                None
            }
        })
        .collect();
    if rets.len() < 2 {
        return 0.0;
    }
    let mean = rets.iter().sum::<f64>() / rets.len() as f64;
    let var = rets.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / rets.len() as f64;
    var.sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vol_of_flat_series_is_zero() {
        let closes = vec![100.0; 20];
        assert_eq!(realized_vol_from_closes(&closes, 60), 0.0);
    }

    #[test]
    fn vol_of_known_series() {
        // Closes newest-first: alternating +1%/-1% roughly.
        let closes = vec![101.0, 100.0, 101.0, 100.0, 101.0, 100.0];
        let v = realized_vol_from_closes(&closes, 60);
        // returns are ~ +/-1%, stddev ~1%.
        assert!(v > 0.005 && v < 0.02, "got {v}");
    }

    #[test]
    fn insufficient_data_is_zero() {
        assert_eq!(realized_vol_from_closes(&[100.0], 60), 0.0);
        assert_eq!(realized_vol_from_closes(&[], 60), 0.0);
    }
}
