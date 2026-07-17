//! Kalshi domain types.
//!
//! Only the fields the engine actually uses are modeled; unknown fields in
//! Kalshi responses are ignored (serde default). Prices on Kalshi are integer
//! **cents** (1..=99) per contract; we keep them as cents and convert to USD
//! only at display / risk boundaries.

use serde::{Deserialize, Serialize};

/// Buy or sell a contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Yes,
    No,
}

/// Order aggressiveness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrderType {
    /// Rest at a price.
    Limit,
    /// Take whatever is available now.
    Market,
}

/// An order we intend to place, before it touches the network.
///
/// `price_cents` is the limit price in cents (1..=99); ignored for market orders.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedOrder {
    pub ticker: String,
    pub side: Side,
    pub order_type: OrderType,
    pub count: u32,
    pub price_cents: u8,
}

impl PlannedOrder {
    /// Worst-case notional in whole USD (count * price, cents -> dollars).
    /// For a market order we can't know fill price ahead of time, so callers
    /// pass a reference price; here we use the stored `price_cents` as that
    /// reference.
    pub fn notional_usd(&self) -> f64 {
        (self.count as f64) * (self.price_cents as f64) / 100.0
    }
}

/// A single price level in an orderbook (price in cents, size in contracts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Level {
    pub price_cents: u8,
    pub size: u32,
}

/// Orderbook for one market.
///
/// IMPORTANT Kalshi semantics: the `yes` and `no` ladders are **bids** — resting
/// orders to *buy* that side. There is no separate "ask" ladder. To BUY YES you
/// must lift the NO bids: a NO bid at price `p` cents is equivalent to an offer
/// to sell YES at `100 - p` cents. So:
///   - the best YES ask (cheapest you can buy YES) = 100 - (highest NO bid),
///   - YES-buy depth = sum over NO bids of size * (100 - p).
/// Symmetrically for buying NO against the YES bids.
#[derive(Debug, Clone, Default)]
pub struct Orderbook {
    pub ticker: String,
    /// Bids to buy YES (price they'll pay, in cents), best (highest) first-ish.
    pub yes: Vec<Level>,
    /// Bids to buy NO (price they'll pay, in cents).
    pub no: Vec<Level>,
}

impl Orderbook {
    /// Best (lowest) price in cents at which you can BUY YES right now, or `None`
    /// if no one is offering (i.e. the NO book is empty). Derived from the
    /// highest NO bid: `100 - max(no bid)`.
    pub fn best_yes_ask_cents(&self) -> Option<u8> {
        self.no.iter().map(|l| l.price_cents).max().map(|p| 100 - p)
    }

    /// Best (lowest) price in cents at which you can BUY NO, from the YES bids.
    pub fn best_no_ask_cents(&self) -> Option<u8> {
        self.yes.iter().map(|l| l.price_cents).max().map(|p| 100 - p)
    }

    /// Best (highest) price in cents at which you can SELL YES right now — i.e.
    /// the highest resting YES bid you could hit. `None` if the YES book is empty
    /// (nothing to sell into). This is the realizable exit price for a held YES
    /// position, and what the stop-loss compares against its floor.
    pub fn best_yes_bid_cents(&self) -> Option<u8> {
        self.yes.iter().map(|l| l.price_cents).max()
    }

    /// USD notional available to BUY YES: each NO bid of `size` at `p` lets you
    /// buy `size` YES contracts at `(100 - p)` cents.
    pub fn yes_buy_depth_usd(&self) -> f64 {
        self.no
            .iter()
            .map(|l| (l.size as f64) * ((100 - l.price_cents) as f64) / 100.0)
            .sum()
    }

    /// USD notional available to BUY NO, from the YES bids.
    pub fn no_buy_depth_usd(&self) -> f64 {
        self.yes
            .iter()
            .map(|l| (l.size as f64) * ((100 - l.price_cents) as f64) / 100.0)
            .sum()
    }

    /// Buy-side depth available for a given side.
    pub fn depth_usd_for(&self, side: Side) -> f64 {
        match side {
            Side::Yes => self.yes_buy_depth_usd(),
            Side::No => self.no_buy_depth_usd(),
        }
    }

    /// Best ask (cents) for buying a given side.
    pub fn best_ask_cents_for(&self, side: Side) -> Option<u8> {
        match side {
            Side::Yes => self.best_yes_ask_cents(),
            Side::No => self.best_no_ask_cents(),
        }
    }
}

/// A market summary as returned by `GET /markets`.
///
/// Kalshi reports prices as **dollar strings** (e.g. `yes_ask_dollars: "0.9800"`),
/// not integer cents, and uses a `"1.0000"` ask with no bid / zero liquidity as
/// a placeholder for an empty book. We normalize both here: `yes_ask` / `yes_bid`
/// are real prices in **cents**, or `None` when there is no genuine quote.
#[derive(Debug, Clone)]
pub struct Market {
    pub ticker: String,
    pub title: String,
    pub yes_bid: Option<u8>,
    pub yes_ask: Option<u8>,
    pub status: String,
    /// Open interest (contracts). A proxy for whether the market is real and
    /// active — the naive market listing is full of zero-OI placeholder markets.
    pub open_interest: f64,
    /// Close time as RFC3339 string, e.g. "2026-07-10T21:00:00Z".
    pub close_time: String,
}

impl Market {
    /// Minutes until this market closes, given `now` (unix secs). `None` if the
    /// close time can't be parsed. Negative clamped to 0.
    pub fn minutes_to_close(&self, now_secs: u64) -> Option<f64> {
        use time::format_description::well_known::Rfc3339;
        let close = time::OffsetDateTime::parse(&self.close_time, &Rfc3339).ok()?;
        let close_secs = close.unix_timestamp();
        let remaining = close_secs - now_secs as i64;
        Some((remaining.max(0) as f64) / 60.0)
    }
}

/// Raw wire shape from Kalshi, before normalization.
#[derive(Deserialize)]
struct RawMarket {
    ticker: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    yes_ask_dollars: Option<String>,
    #[serde(default)]
    yes_bid_dollars: Option<String>,
    #[serde(default)]
    liquidity_dollars: Option<String>,
    /// Open interest is reported as a string in `_fp` form.
    #[serde(default)]
    open_interest_fp: Option<String>,
    #[serde(default)]
    close_time: Option<String>,
}

/// Parse a Kalshi dollar string (e.g. `"0.9800"`) into cents (0..=100).
fn dollars_to_cents(s: &str) -> Option<u8> {
    let dollars: f64 = s.parse().ok()?;
    let cents = (dollars * 100.0).round();
    if (0.0..=100.0).contains(&cents) {
        Some(cents as u8)
    } else {
        None
    }
}

impl<'de> Deserialize<'de> for Market {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawMarket::deserialize(deserializer)?;
        let bid = raw.yes_bid_dollars.as_deref().and_then(dollars_to_cents);
        let ask_raw = raw.yes_ask_dollars.as_deref().and_then(dollars_to_cents);
        let liquidity = raw
            .liquidity_dollars
            .as_deref()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);

        // An ask of exactly 100c with no bid and no liquidity is an empty-book
        // placeholder, not a tradeable quote.
        let ask = match ask_raw {
            Some(100) if bid.unwrap_or(0) == 0 && liquidity == 0.0 => None,
            other => other,
        };

        let open_interest = raw
            .open_interest_fp
            .as_deref()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);

        Ok(Market {
            ticker: raw.ticker,
            title: raw.title,
            yes_bid: bid,
            yes_ask: ask,
            status: raw.status,
            open_interest,
            close_time: raw.close_time.unwrap_or_default(),
        })
    }
}

/// Account balance from `GET /portfolio/balance` (Kalshi returns cents).
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Balance {
    /// Available balance in cents.
    pub balance: i64,
}

impl Balance {
    pub fn usd(&self) -> f64 {
        self.balance as f64 / 100.0
    }
}

/// An open position from `GET /portfolio/positions` (`market_positions`).
///
/// Kalshi reports these as dollar strings; we normalize to numbers.
#[derive(Debug, Clone)]
pub struct Position {
    pub ticker: String,
    /// Contracts held.
    pub shares: f64,
    /// Total cost basis in USD.
    pub cost_usd: f64,
    /// Current market exposure in USD.
    pub exposure_usd: f64,
    /// Fees paid to date in USD.
    pub fees_usd: f64,
}

#[derive(Deserialize)]
struct RawPosition {
    ticker: String,
    /// Share count, e.g. "2.00".
    #[serde(default)]
    position_fp: Option<String>,
    /// Cost basis, e.g. "1.940000".
    #[serde(default)]
    total_traded_dollars: Option<String>,
    #[serde(default)]
    market_exposure_dollars: Option<String>,
    #[serde(default)]
    fees_paid_dollars: Option<String>,
}

fn parse_f64(s: &Option<String>) -> f64 {
    s.as_deref().and_then(|x| x.parse().ok()).unwrap_or(0.0)
}

impl<'de> Deserialize<'de> for Position {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawPosition::deserialize(deserializer)?;
        Ok(Position {
            ticker: raw.ticker,
            shares: parse_f64(&raw.position_fp),
            cost_usd: parse_f64(&raw.total_traded_dollars),
            exposure_usd: parse_f64(&raw.market_exposure_dollars),
            fees_usd: parse_f64(&raw.fees_paid_dollars),
        })
    }
}

/// A fill / execution record.
#[derive(Debug, Clone, Deserialize)]
pub struct Fill {
    pub ticker: String,
    #[serde(default)]
    pub count: u32,
    #[serde(default)]
    pub price: u8,
    #[serde(default)]
    pub side: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn market_parses_dollar_ask_into_cents() {
        // Real Kalshi shape: prices are dollar strings.
        let m: Market = serde_json::from_value(serde_json::json!({
            "ticker": "KXWIN",
            "title": "Test",
            "status": "active",
            "yes_ask_dollars": "0.9800",
            "yes_bid_dollars": "0.9500",
            "liquidity_dollars": "1234.00"
        }))
        .unwrap();
        assert_eq!(m.yes_ask, Some(98));
        assert_eq!(m.yes_bid, Some(95));
    }

    #[test]
    fn empty_book_placeholder_ask_becomes_none() {
        // ask=$1.00 with no bid and no liquidity is a placeholder, not a quote.
        let m: Market = serde_json::from_value(serde_json::json!({
            "ticker": "T",
            "status": "active",
            "yes_ask_dollars": "1.0000",
            "yes_bid_dollars": "0.0000",
            "liquidity_dollars": "0.0000"
        }))
        .unwrap();
        assert_eq!(m.yes_ask, None, "placeholder ask must not be treated as tradeable");
    }

    #[test]
    fn genuine_full_price_ask_is_kept() {
        // A real $1.00 ask WITH liquidity is a genuine quote, keep it.
        let m: Market = serde_json::from_value(serde_json::json!({
            "ticker": "T",
            "status": "active",
            "yes_ask_dollars": "1.0000",
            "yes_bid_dollars": "0.9900",
            "liquidity_dollars": "500.00"
        }))
        .unwrap();
        assert_eq!(m.yes_ask, Some(100));
    }

    #[test]
    fn missing_price_fields_are_none() {
        let m: Market = serde_json::from_value(serde_json::json!({
            "ticker": "T", "status": "active"
        }))
        .unwrap();
        assert_eq!(m.yes_ask, None);
        assert_eq!(m.yes_bid, None);
    }

    #[test]
    fn notional_is_count_times_price_in_usd() {
        let o = PlannedOrder {
            ticker: "T".into(),
            side: Side::Yes,
            order_type: OrderType::Limit,
            count: 10,
            price_cents: 40,
        };
        // 10 contracts * 40c = 400c = $4.00
        assert_eq!(o.notional_usd(), 4.0);
    }

    #[test]
    fn best_yes_ask_is_derived_from_no_bids() {
        // Real market KXTRUMPENDORSEMENTS-A5: NO bids at 75/85/87c, summary
        // yes_ask was 13c. Best yes ask = 100 - max(no bid 87) = 13c.
        let ob = Orderbook {
            ticker: "T".into(),
            yes: vec![Level { price_cents: 12, size: 12 }], // yes bids (irrelevant to buying yes)
            no: vec![
                Level { price_cents: 75, size: 300 },
                Level { price_cents: 85, size: 100 },
                Level { price_cents: 87, size: 610 },
            ],
        };
        assert_eq!(ob.best_yes_ask_cents(), Some(13), "yes ask = 100 - best no bid");
    }

    #[test]
    fn yes_buy_depth_uses_no_bids_at_complement_price() {
        // Buying YES lifts NO bids: a NO bid of size 610 @ 87c lets you buy 610
        // YES @ 13c = $79.30; NO bid 100 @ 85c -> 100 YES @ 15c = $15; NO bid
        // 300 @ 75c -> 300 YES @ 25c = $75. Total = $169.30.
        let ob = Orderbook {
            ticker: "T".into(),
            yes: vec![],
            no: vec![
                Level { price_cents: 75, size: 300 },
                Level { price_cents: 85, size: 100 },
                Level { price_cents: 87, size: 610 },
            ],
        };
        assert!((ob.yes_buy_depth_usd() - 169.30).abs() < 0.01, "got {}", ob.yes_buy_depth_usd());
        assert!((ob.depth_usd_for(Side::Yes) - 169.30).abs() < 0.01);
    }

    #[test]
    fn empty_no_book_means_no_yes_ask_and_zero_depth() {
        let ob = Orderbook { ticker: "T".into(), yes: vec![Level { price_cents: 5, size: 10 }], no: vec![] };
        assert_eq!(ob.best_yes_ask_cents(), None);
        assert_eq!(ob.yes_buy_depth_usd(), 0.0);
    }

    #[test]
    fn balance_converts_cents_to_usd() {
        assert_eq!(Balance { balance: 12345 }.usd(), 123.45);
    }
}
