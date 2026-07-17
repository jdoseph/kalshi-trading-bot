//! Read-only market/account data. These are the first authenticated calls to
//! exercise — a 200 here proves the whole signing chain end to end.

use crate::client::{KalshiClient, RequestSender};
use crate::models::{Balance, Level, Market, Orderbook, Position};
use anyhow::{Context, Result};

pub struct MarketData<'a, S: RequestSender> {
    client: &'a KalshiClient<S>,
}

impl<'a, S: RequestSender> MarketData<'a, S> {
    pub fn new(client: &'a KalshiClient<S>) -> Self {
        Self { client }
    }

    /// The underlying signed client — used by strategies that place orders.
    pub fn client(&self) -> &'a KalshiClient<S> {
        self.client
    }

    /// `GET /portfolio/balance` — the canonical "is my auth working" call.
    pub fn balance(&self) -> Result<Balance> {
        let v = self.client.request_json("GET", "/portfolio/balance", None)?;
        serde_json::from_value(v).context("decoding balance")
    }

    /// `GET /markets?limit=N` — a page of open markets.
    pub fn markets(&self, limit: u32) -> Result<Vec<Market>> {
        let route = format!("/markets?limit={}", limit);
        let v = self.client.request_json("GET", &route, None)?;
        let markets = v
            .get("markets")
            .cloned()
            .unwrap_or(serde_json::Value::Array(vec![]));
        serde_json::from_value(markets).context("decoding markets")
    }

    /// Scan open markets closing within `window_secs` from now, keeping only
    /// those with at least `min_open_interest` contracts. Paginates up to
    /// `max_pages` pages of 1000. This surfaces real, near-resolution markets —
    /// the naive `markets()` listing is dominated by zero-OI placeholders.
    pub fn markets_closing_within(
        &self,
        window_secs: u64,
        min_open_interest: f64,
        max_pages: u32,
        now_secs: u64,
    ) -> Result<Vec<Market>> {
        let max_close_ts = now_secs + window_secs;
        let mut cursor = String::new();
        let mut out = Vec::new();
        for _ in 0..max_pages {
            let route = if cursor.is_empty() {
                format!("/markets?limit=1000&status=open&max_close_ts={}", max_close_ts)
            } else {
                format!(
                    "/markets?limit=1000&status=open&max_close_ts={}&cursor={}",
                    max_close_ts, cursor
                )
            };
            let v = self.client.request_json("GET", &route, None)?;
            let arr = v
                .get("markets")
                .cloned()
                .unwrap_or(serde_json::Value::Array(vec![]));
            let page: Vec<Market> = serde_json::from_value(arr).context("decoding markets page")?;
            for m in page {
                if m.open_interest >= min_open_interest {
                    out.push(m);
                }
            }
            cursor = v
                .get("cursor")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            if cursor.is_empty() {
                break;
            }
        }
        Ok(out)
    }

    /// `GET /markets/{ticker}/orderbook` — parsed into our `Orderbook`.
    pub fn orderbook(&self, ticker: &str) -> Result<Orderbook> {
        let route = format!("/markets/{}/orderbook", ticker);
        let v = self.client.request_json("GET", &route, None)?;
        Ok(parse_orderbook(ticker, &v))
    }

    /// `GET /portfolio/positions` — open positions.
    pub fn positions(&self) -> Result<Vec<Position>> {
        let v = self.client.request_json("GET", "/portfolio/positions", None)?;
        let positions = v
            .get("market_positions")
            .cloned()
            .unwrap_or(serde_json::Value::Array(vec![]));
        serde_json::from_value(positions).context("decoding positions")
    }
}

/// Parse a Kalshi orderbook response.
///
/// The live API returns `orderbook_fp` with `yes_dollars` / `no_dollars`, each a
/// list of `["<price_dollars>", "<size>"]` **string** pairs, e.g.
/// `["0.7500", "200.00"]`. (Older/mocked responses used an `orderbook` object
/// with integer `[cents, size]` pairs; we still accept that for robustness.)
fn parse_orderbook(ticker: &str, v: &serde_json::Value) -> Orderbook {
    // Prefer the real `orderbook_fp` shape; fall back to `orderbook`.
    let (ob, dollars_format) = match v.get("orderbook_fp") {
        Some(fp) => (fp, true),
        None => (v.get("orderbook").unwrap_or(v), false),
    };

    let parse_row = move |row: &serde_json::Value| -> Option<Level> {
        let arr = row.as_array()?;
        let (price_cents, size) = if dollars_format {
            // String dollars: "0.7500" -> 75 cents; "200.00" -> 200 contracts.
            let price: f64 = arr.first()?.as_str()?.parse().ok()?;
            let size: f64 = arr.get(1)?.as_str()?.parse().ok()?;
            ((price * 100.0).round() as u8, size.round() as u32)
        } else {
            // Integer cents/size.
            (arr.first()?.as_u64()? as u8, arr.get(1)?.as_u64()? as u32)
        };
        Some(Level { price_cents, size })
    };

    let side = |keys: &[&str]| -> Vec<Level> {
        keys.iter()
            .find_map(|k| ob.get(k).and_then(|a| a.as_array()))
            .map(|rows| rows.iter().filter_map(&parse_row).collect())
            .unwrap_or_default()
    };

    Orderbook {
        ticker: ticker.to_string(),
        yes: side(&["yes_dollars", "yes"]),
        no: side(&["no_dollars", "no"]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_orderbook_fp_dollar_shape() {
        // The actual live Kalshi shape that our old parser silently dropped.
        let v = serde_json::json!({
            "orderbook_fp": {
                "yes_dollars": [["0.7500", "200.00"], ["0.7600", "117.04"]],
                "no_dollars":  [["0.1100", "50.00"]]
            }
        });
        let ob = parse_orderbook("T", &v);
        // yes/no ladders are BID ladders parsed straight from the wire.
        assert_eq!(ob.yes.len(), 2);
        assert_eq!(ob.yes[0], Level { price_cents: 75, size: 200 });
        assert_eq!(ob.yes[1], Level { price_cents: 76, size: 117 });
        assert_eq!(ob.no[0], Level { price_cents: 11, size: 50 });
        // Best yes ask = 100 - best no bid (11) = 89c.
        assert_eq!(ob.best_yes_ask_cents(), Some(89));
        // Yes-buy depth = 50 no-bid contracts @ (100-11)=89c = $44.50.
        assert!((ob.yes_buy_depth_usd() - 44.50).abs() < 0.01, "got {}", ob.yes_buy_depth_usd());
    }

    #[test]
    fn still_parses_legacy_integer_shape() {
        let v = serde_json::json!({
            "orderbook": { "yes": [[50, 100], [49, 200]], "no": [[45, 10]] }
        });
        let ob = parse_orderbook("T", &v);
        assert_eq!(ob.yes[0], Level { price_cents: 50, size: 100 });
        assert_eq!(ob.no[0], Level { price_cents: 45, size: 10 });
    }

    #[test]
    fn tolerates_missing_side() {
        let v = serde_json::json!({ "orderbook_fp": { "yes_dollars": [["0.10", "5"]] } });
        let ob = parse_orderbook("T", &v);
        assert_eq!(ob.yes.len(), 1);
        assert!(ob.no.is_empty());
    }
}
