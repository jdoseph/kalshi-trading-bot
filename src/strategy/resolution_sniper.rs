//! Resolution Sniper — buys YES in near-certain markets (`yes_ask >= threshold`)
//! for the small remaining edge to $1.00, under strict per-market and global
//! budget caps. Routes every order through the engine's guarded `OrderPlacer`,
//! so it is dry-run-safe by default.
//!
//! Split into a pure core (`SniperState`, `plan_snipes`) and a thin `run` loop.

use crate::client::RequestSender;
use crate::config::SniperConfig;
use crate::market_data::MarketData;
use crate::models::{Market, OrderType, PlannedOrder, Side};
use crate::orders::{OrderOutcome, OrderPlacer};
use crate::risk::RiskGuard;
use anyhow::Result;
use std::collections::HashMap;
use std::time::Duration;
use tracing::{info, warn};

/// Tracks capital deployed, per-ticker and in aggregate. Pure — no I/O.
#[derive(Debug, Default)]
pub struct SniperState {
    per_ticker: HashMap<String, f64>,
    total: f64,
}

impl SniperState {
    pub fn new() -> Self {
        Self::default()
    }

    /// USD still available for `ticker` given the per-market cap.
    pub fn remaining_for(&self, ticker: &str, cap: f64) -> f64 {
        (cap - self.per_ticker.get(ticker).copied().unwrap_or(0.0)).max(0.0)
    }

    /// USD still available globally given the total cap.
    pub fn remaining_total(&self, cap: f64) -> f64 {
        (cap - self.total).max(0.0)
    }

    /// Record `usd` deployed into `ticker`.
    pub fn record(&mut self, ticker: &str, usd: f64) {
        *self.per_ticker.entry(ticker.to_string()).or_insert(0.0) += usd;
        self.total += usd;
    }

    /// Free deployed budget for `ticker` after exiting it (stop-loss). We drop
    /// the ticker's whole deployed amount — a stop exit sells the *entire*
    /// position — and subtract that from the global total so the freed capital
    /// can fund new snipes. Both floored at zero.
    pub fn release(&mut self, ticker: &str) {
        if let Some(deployed) = self.per_ticker.remove(ticker) {
            self.total = (self.total - deployed).max(0.0);
        }
    }

    pub fn total_deployed(&self) -> f64 {
        self.total
    }
}

/// The whole decision, as a pure function: which orders to place given the
/// current markets, deployment state, and config. Data in, orders out.
///
/// Each returned order respects both budget caps at planning time. Callers
/// apply the results to `state` (via [`SniperState::record`]) as orders are
/// accepted so subsequent scans see the updated deployment.
///
/// `real_ask` maps ticker -> the true best YES ask (cents) derived from the live
/// orderbook (`Orderbook::best_yes_ask_cents`). Markets missing from the map have
/// no fillable ask and are skipped. We evaluate the threshold and price the order
/// against this real ask, NOT the stale market-summary `yes_ask`.
pub fn plan_snipes(
    markets: &[Market],
    real_ask: &HashMap<String, u8>,
    state: &SniperState,
    cfg: &SniperConfig,
    budget_usd: f64,
) -> Vec<PlannedOrder> {
    let mut orders = Vec::new();
    // Track budget consumed *within this planning pass* so multiple candidates
    // in one scan don't collectively blow the global cap.
    let mut pass_total = 0.0;
    let mut pass_per_ticker: HashMap<String, f64> = HashMap::new();

    // Compounding mode: derive caps from the *current* budget so position size
    // grows/shrinks with capital. Otherwise fall back to the fixed dollar caps.
    let compounding = cfg.position_pct_of_budget > 0.0;
    let global_cap = if compounding { budget_usd } else { cfg.max_total_budget_usd };
    let per_market_cap = if compounding {
        // Per-position cap = % of budget (your loss-minimization limit). Never
        // below the trade floor's reach — a % that rounds every order to zero
        // contracts is a config error the operator should see, not silently hide.
        budget_usd * cfg.position_pct_of_budget
    } else {
        cfg.max_per_market_usd
    };
    // The per-snipe increment also scales with budget when compounding, so a
    // single scan can actually fill a position to its % cap rather than dribbling
    // the old fixed `per_snipe_budget_usd`.
    let per_snipe = if compounding { per_market_cap } else { cfg.per_snipe_budget_usd };

    for m in markets {
        if m.status != "active" {
            continue;
        }
        // Use the REAL best ask from the live orderbook, not the summary.
        let Some(&ask) = real_ask.get(&m.ticker) else { continue };
        if ask < cfg.threshold_cents {
            continue;
        }

        // Remaining budgets, accounting for what earlier candidates consumed.
        let global_left = state.remaining_total(global_cap) - pass_total;
        let ticker_left = state.remaining_for(&m.ticker, per_market_cap)
            - pass_per_ticker.get(&m.ticker).copied().unwrap_or(0.0);
        if global_left <= 0.0 || ticker_left <= 0.0 {
            continue;
        }

        // Spend the per-snipe increment, but never exceed remaining caps.
        let spend = per_snipe
            .min(global_left)
            .min(ticker_left);
        let price_usd = ask as f64 / 100.0;
        let count = (spend / price_usd).floor() as u32;
        if count == 0 {
            continue;
        }

        let notional = count as f64 * price_usd;
        orders.push(PlannedOrder {
            ticker: m.ticker.clone(),
            side: Side::Yes,
            order_type: OrderType::Limit,
            count,
            price_cents: ask,
        });
        pass_total += notional;
        *pass_per_ticker.entry(m.ticker.clone()).or_insert(0.0) += notional;
    }

    orders
}

/// Tracks how many *consecutive* scans each held ticker's best YES bid has sat
/// below the stop-loss floor. Reset to zero the moment a ticker recovers above
/// the floor, so only a sustained reprice — not a one-tick dip — trips the stop.
#[derive(Debug, Default)]
pub struct StopState {
    below_floor_scans: HashMap<String, u32>,
}

impl StopState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record this scan's observation for `ticker`: `below` = is the bid under
    /// the floor right now. Returns the updated consecutive-scan count.
    fn observe(&mut self, ticker: &str, below: bool) -> u32 {
        let entry = self.below_floor_scans.entry(ticker.to_string()).or_insert(0);
        if below {
            *entry += 1;
        } else {
            *entry = 0;
        }
        *entry
    }

    /// Forget a ticker once we've exited it, so a re-entry starts clean.
    fn clear(&mut self, ticker: &str) {
        self.below_floor_scans.remove(ticker);
    }
}

/// A stop-loss exit: sell the whole `shares` count of YES in `ticker` by BUYING
/// NO (Side::No) at the complement of `yes_bid_cents`, taken immediately (IOC).
/// `recovered_usd` is the approximate cash the exit returns, used to free budget.
#[derive(Debug, Clone, PartialEq)]
pub struct StopExit {
    pub order: PlannedOrder,
    pub recovered_usd: f64,
}

/// Pure stop-loss decision. For each held position, look at the realizable exit
/// price (best YES bid) in its book. If that bid is **below** `floor_cents` and
/// has been for `confirm_scans` consecutive scans, emit a full-position exit.
///
/// Data in, exits out — mirrors [`plan_snipes`]. The `stop` counter is mutated
/// so persistence carries across scans; callers hold one `StopState` per run.
///
/// `floor_cents == 0` disables the stop entirely (returns no exits).
pub fn plan_stops(
    positions: &[crate::models::Position],
    books: &HashMap<String, crate::models::Orderbook>,
    stop: &mut StopState,
    floor_cents: u8,
    confirm_scans: u32,
) -> Vec<StopExit> {
    if floor_cents == 0 {
        return Vec::new();
    }
    let mut exits = Vec::new();
    for p in positions {
        let shares = p.shares.floor() as u32;
        if shares == 0 {
            continue;
        }
        // Need a book with a YES bid to know what we could actually sell into.
        let Some(book) = books.get(&p.ticker) else { continue };
        let Some(yes_bid) = book.best_yes_bid_cents() else {
            // No YES bid at all: nothing to sell into. Don't count this as a
            // breach (an empty book isn't a reprice we can act on) and don't
            // fabricate an exit we couldn't fill.
            continue;
        };

        let below = yes_bid < floor_cents;
        let scans = stop.observe(&p.ticker, below);
        if !below || scans < confirm_scans {
            continue;
        }

        // Sell YES == buy NO at (100 - yes_bid). We express the exit as a NO
        // order priced so `order_payload` emits an ask at `yes_bid` cents.
        let exit = PlannedOrder {
            ticker: p.ticker.clone(),
            side: Side::No,
            order_type: OrderType::Market,
            count: shares,
            price_cents: 100 - yes_bid,
        };
        // Cash back ~= shares * yes_bid. Freed so it can fund new snipes.
        let recovered_usd = shares as f64 * yes_bid as f64 / 100.0;
        exits.push(StopExit { order: exit, recovered_usd });
    }
    exits
}

/// Run the sniper loop against live data. One iteration: scan -> plan -> place
/// -> record. Sleeps `scan_interval_secs` between scans. `live_allowed` comes
/// from the two-gate config.
///
/// Synchronous to match the engine's blocking client design. Runs until the
/// process is interrupted.
pub fn run<S: RequestSender>(
    md: &MarketData<'_, S>,
    placer_factory: impl Fn() -> (RiskGuard, bool),
    cfg: &SniperConfig,
    max_scan_pages: u32,
    now_secs: impl Fn() -> u64,
) -> Result<()> {
    if !cfg.enabled {
        info!("resolution_sniper disabled in config");
        return Ok(());
    }

    let mut state = SniperState::new();

    // Pre-seed deployment from positions we already hold, so a restart doesn't
    // re-buy markets up to the caps again. State is per-run in memory; live
    // positions are the source of truth for what's already deployed.
    match md.positions() {
        Ok(positions) => {
            let mut seeded = 0.0;
            for p in &positions {
                if p.cost_usd > 0.0 {
                    state.record(&p.ticker, p.cost_usd);
                    seeded += p.cost_usd;
                }
            }
            info!(positions = positions.len(), seeded_usd = seeded, "seeded deployment from held positions");
        }
        Err(e) => warn!(error = %e, "could not fetch positions to seed state; starting from zero"),
    }

    let mut stop = StopState::new();

    info!(
        threshold_cents = cfg.threshold_cents,
        per_snipe_budget_usd = cfg.per_snipe_budget_usd,
        max_total_budget_usd = cfg.max_total_budget_usd,
        stop_loss_floor_cents = cfg.stop_loss_floor_cents,
        already_deployed_usd = state.total_deployed(),
        "resolution sniper started"
    );

    loop {
        // Stop-loss pass FIRST, every scan, independent of buy budget — you must
        // always be able to exit a losing position even when fully deployed.
        if cfg.stop_loss_floor_cents > 0 {
            match md.positions() {
                Ok(positions) => {
                    // Fetch the book for each held ticker to read its real exit bid.
                    let mut held_books: HashMap<String, crate::models::Orderbook> = HashMap::new();
                    for p in &positions {
                        if p.shares.floor() as u32 == 0 {
                            continue;
                        }
                        if let Ok(book) = md.orderbook(&p.ticker) {
                            held_books.insert(p.ticker.clone(), book);
                        }
                    }
                    let exits = plan_stops(
                        &positions,
                        &held_books,
                        &mut stop,
                        cfg.stop_loss_floor_cents,
                        cfg.stop_loss_confirm_scans,
                    );
                    for exit in exits {
                        let book = match held_books.get(&exit.order.ticker) {
                            Some(b) => b.clone(),
                            None => continue,
                        };
                        let (mut guard, live_allowed) = placer_factory();
                        let mut placer = OrderPlacer::new(md.client(), &mut guard, live_allowed);
                        match placer.place(&exit.order, &book, now_secs()) {
                            Ok(OrderOutcome::Sent { .. }) => {
                                state.release(&exit.order.ticker);
                                stop.clear(&exit.order.ticker);
                                warn!(ticker = %exit.order.ticker, count = exit.order.count, exit_price = 100 - exit.order.price_cents, recovered_usd = exit.recovered_usd, "STOP-LOSS SOLD");
                            }
                            Ok(OrderOutcome::DryRun { payload }) => {
                                // Mirror live: free budget + clear so dry-run
                                // simulates the same state transition.
                                state.release(&exit.order.ticker);
                                stop.clear(&exit.order.ticker);
                                warn!(ticker = %exit.order.ticker, %payload, "DRY-RUN would STOP-LOSS sell");
                            }
                            Ok(OrderOutcome::Rejected(r)) => {
                                warn!(ticker = %exit.order.ticker, reason = ?r, "stop-loss exit rejected by guard");
                            }
                            Err(e) => warn!(ticker = %exit.order.ticker, error = %e, "stop-loss exit send failed"),
                        }
                    }
                }
                Err(e) => warn!(error = %e, "could not fetch positions for stop-loss pass"),
            }
        }

        // Working budget for this scan. When compounding, it's the real cash
        // balance capped at the target — so position sizes track actual settled
        // capital (compounding up as wins settle, down on losses). When not
        // compounding, the fixed config cap stands in.
        let compounding = cfg.position_pct_of_budget > 0.0;
        let budget_usd = if compounding {
            match md.balance() {
                Ok(b) => {
                    let cash = b.usd();
                    let capped = if cfg.compound_target_usd > 0.0 {
                        cash.min(cfg.compound_target_usd)
                    } else {
                        cash
                    };
                    if cfg.compound_target_usd > 0.0 && cash >= cfg.compound_target_usd {
                        info!(cash, target = cfg.compound_target_usd, "compound target reached — no new positions");
                    }
                    capped
                }
                Err(e) => {
                    warn!(error = %e, "could not fetch balance for compounding; skipping buy pass this scan");
                    // Skip buying this scan rather than guess a budget.
                    std::thread::sleep(Duration::from_secs(cfg.scan_interval_secs));
                    continue;
                }
            }
        } else {
            cfg.max_total_budget_usd
        };

        let global_cap = if compounding { budget_usd } else { cfg.max_total_budget_usd };
        if state.remaining_total(global_cap) <= 0.0 {
            info!(deployed = state.total_deployed(), budget = budget_usd, "global budget exhausted — no new positions");
        } else {
            match md.markets_closing_within(
                cfg.close_window_secs,
                cfg.min_open_interest,
                max_scan_pages,
                now_secs(),
            ) {
                Ok(markets) => {
                    // Fetch live orderbooks for plausible candidates and derive
                    // the REAL best yes ask. Pre-filter on the summary ask (loose)
                    // to avoid fetching books for obvious non-candidates.
                    let prefilter = cfg.threshold_cents.saturating_sub(15);
                    let mut real_ask: HashMap<String, u8> = HashMap::new();
                    let mut books: HashMap<String, crate::models::Orderbook> = HashMap::new();
                    for m in &markets {
                        if m.status != "active" {
                            continue;
                        }
                        if m.yes_ask.map(|a| a >= prefilter).unwrap_or(false) {
                            if let Ok(book) = md.orderbook(&m.ticker) {
                                if let Some(ask) = book.best_yes_ask_cents() {
                                    real_ask.insert(m.ticker.clone(), ask);
                                    books.insert(m.ticker.clone(), book);
                                }
                            }
                        }
                    }

                    let planned = plan_snipes(&markets, &real_ask, &state, cfg, budget_usd);
                    info!(
                        scanned = markets.len(),
                        priced = real_ask.len(),
                        candidates = planned.len(),
                        deployed_usd = state.total_deployed(),
                        "scan complete"
                    );
                    for order in planned {
                        // Reuse the book we already fetched for this ticker.
                        let book = match books.get(&order.ticker) {
                            Some(b) => b.clone(),
                            None => continue,
                        };
                        let (mut guard, live_allowed) = placer_factory();
                        let mut placer = OrderPlacer::new(md.client(), &mut guard, live_allowed);
                        match placer.place(&order, &book, now_secs()) {
                            Ok(OrderOutcome::Sent { .. }) => {
                                state.record(&order.ticker, order.notional_usd());
                                info!(ticker = %order.ticker, count = order.count, price = order.price_cents, "SNIPED");
                            }
                            Ok(OrderOutcome::DryRun { payload }) => {
                                // Count against budgets so dry-run simulates caps.
                                state.record(&order.ticker, order.notional_usd());
                                info!(ticker = %order.ticker, %payload, "DRY-RUN would snipe");
                            }
                            Ok(OrderOutcome::Rejected(r)) => {
                                info!(ticker = %order.ticker, reason = ?r, "snipe rejected by guard");
                            }
                            Err(e) => warn!(ticker = %order.ticker, error = %e, "snipe send failed"),
                        }
                    }
                }
                Err(e) => warn!(error = %e, "market scan failed"),
            }
        }

        std::thread::sleep(Duration::from_secs(cfg.scan_interval_secs));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> SniperConfig {
        SniperConfig {
            enabled: true,
            threshold_cents: 97,
            per_snipe_budget_usd: 20.0,
            max_per_market_usd: 60.0,
            max_total_budget_usd: 500.0,
            scan_interval_secs: 30,
            close_window_secs: 172_800,
            min_open_interest: 100.0,
            stop_loss_floor_cents: 0, // stop disabled by default in these tests
            stop_loss_confirm_scans: 2,
            position_pct_of_budget: 0.0, // compounding off by default in tests
            compound_target_usd: 0.0,
        }
    }

    fn market(ticker: &str, ask: Option<u8>, status: &str) -> Market {
        // Build via the real Kalshi wire shape (dollar strings + liquidity), so
        // tests exercise the actual deserializer.
        let (ask_dollars, liq) = match ask {
            Some(c) => (format!("{:.4}", c as f64 / 100.0), "1000.00"),
            None => ("1.0000".to_string(), "0.0000"), // empty-book placeholder
        };
        serde_json::from_value(serde_json::json!({
            "ticker": ticker,
            "title": "",
            "yes_ask_dollars": ask_dollars,
            "yes_bid_dollars": "0.0000",
            "liquidity_dollars": liq,
            "open_interest_fp": "1000.0",
            "status": status,
        }))
        .unwrap()
    }

    /// Test convenience: build the `real_ask` map from each market's summary ask
    /// (in tests, the summary ask stands in for the real orderbook ask) and call
    /// `plan_snipes`.
    fn plan(markets: &[Market], state: &SniperState, cfg: &SniperConfig) -> Vec<PlannedOrder> {
        let real_ask: HashMap<String, u8> = markets
            .iter()
            .filter_map(|m| m.yes_ask.map(|a| (m.ticker.clone(), a)))
            .collect();
        // Non-compounding cfg() in tests: budget_usd is unused (fixed caps win),
        // so the global cap value is a harmless stand-in.
        plan_snipes(markets, &real_ask, state, cfg, cfg.max_total_budget_usd)
    }

    // ---- SniperState ----

    #[test]
    fn state_records_and_reports_remaining() {
        let mut s = SniperState::new();
        assert_eq!(s.remaining_for("A", 60.0), 60.0);
        assert_eq!(s.remaining_total(500.0), 500.0);

        s.record("A", 20.0);
        s.record("A", 10.0);
        s.record("B", 5.0);

        assert_eq!(s.remaining_for("A", 60.0), 30.0);
        assert_eq!(s.remaining_for("B", 60.0), 55.0);
        assert_eq!(s.remaining_total(500.0), 465.0);
        assert_eq!(s.total_deployed(), 35.0);
    }

    #[test]
    fn remaining_never_negative_past_cap() {
        let mut s = SniperState::new();
        s.record("A", 100.0);
        assert_eq!(s.remaining_for("A", 60.0), 0.0);
    }

    // ---- plan_snipes filtering ----

    #[test]
    fn includes_at_or_above_threshold_excludes_below() {
        let markets = vec![
            market("LOW", Some(96), "active"),  // below 97
            market("AT", Some(97), "active"),   // at threshold
            market("HIGH", Some(99), "active"), // above
        ];
        let orders = plan(&markets, &SniperState::new(), &cfg());
        let tickers: Vec<&str> = orders.iter().map(|o| o.ticker.as_str()).collect();
        assert!(!tickers.contains(&"LOW"));
        assert!(tickers.contains(&"AT"));
        assert!(tickers.contains(&"HIGH"));
    }

    #[test]
    fn excludes_non_active_markets() {
        let markets = vec![market("CLOSED", Some(98), "closed")];
        assert!(plan(&markets, &SniperState::new(), &cfg()).is_empty());
    }

    #[test]
    fn excludes_markets_without_an_ask() {
        let markets = vec![market("NOASK", None, "active")];
        assert!(plan(&markets, &SniperState::new(), &cfg()).is_empty());
    }

    // ---- sizing ----

    #[test]
    fn sizes_budget_into_contracts_at_ask() {
        // $20 @ 98c -> floor(20 / 0.98) = 20 contracts.
        let orders = plan(&[market("T", Some(98), "active")], &SniperState::new(), &cfg());
        assert_eq!(orders[0].count, 20);
        assert_eq!(orders[0].price_cents, 98);
    }

    #[test]
    fn cheaper_price_buys_more_contracts() {
        let mut c = cfg();
        c.threshold_cents = 80;
        // $20 @ 80c -> floor(20 / 0.80) = 25.
        let orders = plan(&[market("T", Some(80), "active")], &SniperState::new(), &c);
        assert_eq!(orders[0].count, 25);
    }

    #[test]
    fn budget_rounding_to_zero_produces_no_order() {
        let mut c = cfg();
        c.per_snipe_budget_usd = 0.5; // $0.50 @ 98c -> floor(0.51) = 0
        assert!(plan(&[market("T", Some(98), "active")], &SniperState::new(), &c).is_empty());
    }

    // ---- caps ----

    #[test]
    fn per_market_cap_halts_a_ticker() {
        let mut s = SniperState::new();
        s.record("T", 55.0); // cap is 60 -> only $5 left
        // $5 @ 98c -> floor(5/0.98) = 5 contracts (not the full 20-budget worth).
        let orders = plan(&[market("T", Some(98), "active")], &s, &cfg());
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].count, 5);

        // Now fully deployed on T -> nothing more.
        let mut s2 = SniperState::new();
        s2.record("T", 60.0);
        assert!(plan(&[market("T", Some(98), "active")], &s2, &cfg()).is_empty());
    }

    #[test]
    fn global_cap_stops_all_new_orders() {
        let mut s = SniperState::new();
        s.record("X", 500.0); // total cap reached
        let markets = vec![market("A", Some(98), "active"), market("B", Some(99), "active")];
        assert!(plan(&markets, &s, &cfg()).is_empty());
    }

    #[test]
    fn multiple_candidates_in_one_pass_respect_global_cap() {
        let mut c = cfg();
        c.max_total_budget_usd = 30.0; // only room for ~one $20 snipe
        c.per_snipe_budget_usd = 20.0;
        let markets = vec![
            market("A", Some(98), "active"),
            market("B", Some(98), "active"),
            market("C", Some(98), "active"),
        ];
        let orders = plan(&markets, &SniperState::new(), &c);
        // First takes ~$19.6, leaving ~$10.4 -> second sized down, third gets nothing.
        let total: f64 = orders.iter().map(|o| o.notional_usd()).sum();
        assert!(total <= 30.0, "planned {total} exceeds global cap 30");
        assert!(!orders.is_empty());
    }

    // ---- compounding / %-of-budget sizing ----

    #[test]
    fn compounding_sizes_position_as_pct_of_budget() {
        let mut c = cfg();
        c.threshold_cents = 80;
        c.position_pct_of_budget = 0.05; // 5% per position
        // Budget $100 -> per-position cap $5. @ 80c -> floor(5/0.80) = 6 contracts.
        let orders = plan_snipes(
            &[market("T", Some(80), "active")],
            &[("T".to_string(), 80u8)].into_iter().collect(),
            &SniperState::new(),
            &c,
            100.0,
        );
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].count, 6);
        // Notional ~$4.80, i.e. <= 5% of $100.
        assert!(orders[0].notional_usd() <= 5.0);
    }

    #[test]
    fn compounding_grows_position_as_budget_grows() {
        let mut c = cfg();
        c.threshold_cents = 80;
        c.position_pct_of_budget = 0.10; // 10%
        let mk = [market("T", Some(80), "active")];
        let ask: HashMap<String, u8> = [("T".to_string(), 80u8)].into_iter().collect();

        // Budget $50 -> cap $5 -> floor(5/0.8)=6.
        let small = plan_snipes(&mk, &ask, &SniperState::new(), &c, 50.0);
        // Budget $500 -> cap $50 -> floor(50/0.8)=62.
        let big = plan_snipes(&mk, &ask, &SniperState::new(), &c, 500.0);
        assert!(big[0].count > small[0].count, "position must grow with budget");
        assert_eq!(small[0].count, 6);
        assert_eq!(big[0].count, 62);
    }

    #[test]
    fn compounding_global_cap_is_the_budget() {
        let mut c = cfg();
        c.threshold_cents = 80;
        c.position_pct_of_budget = 1.0; // allow one position to use whole budget
        // Already deployed $18 of a $20 budget -> only $2 headroom left.
        let mut s = SniperState::new();
        s.record("OTHER", 18.0);
        let orders = plan_snipes(
            &[market("T", Some(80), "active")],
            &[("T".to_string(), 80u8)].into_iter().collect(),
            &s,
            &c,
            20.0,
        );
        // At most $2 more can deploy -> floor(2/0.8)=2 contracts.
        assert_eq!(orders[0].count, 2);
    }

    #[test]
    fn compounding_loss_cap_bounds_single_position() {
        // The whole point: one losing position can't cost more than pct*budget.
        let mut c = cfg();
        c.threshold_cents = 80;
        c.position_pct_of_budget = 0.05;
        let orders = plan_snipes(
            &[market("T", Some(97), "active")],
            &[("T".to_string(), 97u8)].into_iter().collect(),
            &SniperState::new(),
            &c,
            200.0,
        );
        // 5% of $200 = $10 max at risk in this position.
        assert!(orders[0].notional_usd() <= 10.0);
    }

    // ---- stop-loss (plan_stops) ----

    fn position(ticker: &str, shares: f64) -> crate::models::Position {
        crate::models::Position {
            ticker: ticker.into(),
            shares,
            cost_usd: shares * 0.97,
            exposure_usd: shares * 0.97,
            fees_usd: 0.0,
        }
    }

    /// A book whose best YES bid is `bid` cents (one resting YES bid).
    fn book_with_yes_bid(ticker: &str, bid: u8) -> crate::models::Orderbook {
        crate::models::Orderbook {
            ticker: ticker.into(),
            yes: vec![crate::models::Level { price_cents: bid, size: 1000 }],
            no: vec![],
        }
    }

    #[test]
    fn stop_disabled_when_floor_zero() {
        let positions = vec![position("T", 10.0)];
        let mut books = HashMap::new();
        books.insert("T".to_string(), book_with_yes_bid("T", 50)); // way below any floor
        let mut stop = StopState::new();
        // floor 0 => disabled, no exits regardless of price.
        assert!(plan_stops(&positions, &books, &mut stop, 0, 1).is_empty());
    }

    #[test]
    fn stop_fires_only_after_confirm_scans() {
        let positions = vec![position("T", 10.0)];
        let mut books = HashMap::new();
        books.insert("T".to_string(), book_with_yes_bid("T", 79)); // below 80 floor
        let mut stop = StopState::new();

        // Scan 1: below floor but confirm=2 not yet met -> no exit.
        assert!(plan_stops(&positions, &books, &mut stop, 80, 2).is_empty());
        // Scan 2: still below -> fires.
        let exits = plan_stops(&positions, &books, &mut stop, 80, 2);
        assert_eq!(exits.len(), 1);
        let e = &exits[0];
        assert_eq!(e.order.side, Side::No);
        assert_eq!(e.order.count, 10); // full position
        // Sell YES @ 79c == buy NO @ 21c.
        assert_eq!(e.order.price_cents, 21);
        assert!((e.recovered_usd - 7.9).abs() < 1e-9); // 10 * 0.79
    }

    #[test]
    fn stop_resets_on_recovery_and_does_not_fire() {
        let positions = vec![position("T", 10.0)];
        let mut stop = StopState::new();

        // Scan 1: below floor.
        let mut below = HashMap::new();
        below.insert("T".to_string(), book_with_yes_bid("T", 78));
        assert!(plan_stops(&positions, &below, &mut stop, 80, 2).is_empty());

        // Scan 2: recovered above floor -> counter resets, no exit.
        let mut above = HashMap::new();
        above.insert("T".to_string(), book_with_yes_bid("T", 95));
        assert!(plan_stops(&positions, &above, &mut stop, 80, 2).is_empty());

        // Scan 3: dips below again -> this is only the 1st consecutive scan, so
        // still no fire (proves the reset actually happened).
        assert!(plan_stops(&positions, &below, &mut stop, 80, 2).is_empty());
    }

    #[test]
    fn stop_ignores_position_above_floor() {
        let positions = vec![position("T", 10.0)];
        let mut books = HashMap::new();
        books.insert("T".to_string(), book_with_yes_bid("T", 96)); // healthy
        let mut stop = StopState::new();
        assert!(plan_stops(&positions, &books, &mut stop, 80, 1).is_empty());
    }

    #[test]
    fn stop_skips_ticker_with_no_yes_bid() {
        let positions = vec![position("T", 10.0)];
        let mut books = HashMap::new();
        // Empty YES book -> nothing to sell into; must not fabricate an exit.
        books.insert("T".to_string(), crate::models::Orderbook {
            ticker: "T".into(),
            yes: vec![],
            no: vec![crate::models::Level { price_cents: 5, size: 100 }],
        });
        let mut stop = StopState::new();
        assert!(plan_stops(&positions, &books, &mut stop, 80, 1).is_empty());
    }

    #[test]
    fn release_frees_budget_for_redeployment() {
        let mut s = SniperState::new();
        s.record("A", 10.0);
        s.record("B", 5.0);
        assert_eq!(s.total_deployed(), 15.0);
        s.release("A");
        assert_eq!(s.total_deployed(), 5.0);
        assert_eq!(s.remaining_for("A", 4.0), 4.0); // A fully freed
    }

    // ---- full scan -> plan -> place chain (fake sender, no network) ----

    #[test]
    fn planned_snipe_flows_through_order_placer_as_dry_run() {
        use crate::auth::Signer;
        use crate::client::{KalshiClient, RawResponse, RequestSender, SignedRequest};
        use crate::models::Orderbook;
        use crate::orders::{OrderOutcome, OrderPlacer};
        use crate::risk::{CircuitBreakerConfig, RiskConfig};
        use rsa::pkcs1::{EncodeRsaPrivateKey, LineEnding};
        use rsa::RsaPrivateKey;
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc;

        // A sender that must NEVER be called in dry-run.
        #[derive(Clone)]
        struct MustNotSend(Arc<AtomicU32>);
        impl RequestSender for MustNotSend {
            fn send(&self, _req: SignedRequest) -> Result<RawResponse> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(RawResponse { status: 200, body: "{}".into() })
            }
        }

        let mut rng = rand::thread_rng();
        let key = RsaPrivateKey::new(&mut rng, 2048).unwrap();
        let pem = key.to_pkcs1_pem(LineEnding::LF).unwrap().to_string();
        let signer = Signer::new("kid", &pem).unwrap();
        let sends = Arc::new(AtomicU32::new(0));
        let client = KalshiClient::new("https://host/trade-api/v2", signer, MustNotSend(sends.clone()));

        // One qualifying market.
        let markets = vec![market("KXWIN", Some(98), "active")];
        let planned = plan(&markets, &SniperState::new(), &cfg());
        assert_eq!(planned.len(), 1);

        // Deep book so the depth guard passes: NO bids at 2c provide yes-buy
        // liquidity (buy YES @ 98c). 1000 @ 98c = $980 yes-buy depth.
        let book = Orderbook {
            ticker: "KXWIN".into(),
            yes: vec![],
            no: vec![crate::models::Level { price_cents: 2, size: 1000 }],
        };
        let risk_cfg = RiskConfig {
            min_orderbook_depth_usd: 100.0,
            min_trade_size_usd: 5.0,
            circuit_breaker: CircuitBreakerConfig {
                max_consecutive_large_trades: 5,
                window_seconds: 60,
                cooldown_seconds: 300,
            },
            large_trade_usd: 100.0,
        };
        let mut guard = RiskGuard::new(risk_cfg);
        let mut placer = OrderPlacer::new(&client, &mut guard, false); // dry-run
        let outcome = placer.place(&planned[0], &book, 0).unwrap();

        match outcome {
            OrderOutcome::DryRun { payload } => {
                assert!(payload.contains("\"ticker\":\"KXWIN\""));
                // v2 shape: buy yes = bid, price in dollars.
                assert!(payload.contains("\"side\":\"bid\""), "{payload}");
                assert!(payload.contains("\"price\":\"0.9800\""), "{payload}");
            }
            other => panic!("expected DryRun, got {other:?}"),
        }
        assert_eq!(sends.load(Ordering::SeqCst), 0, "dry-run must not send");
    }

    #[test]
    fn simulated_scans_eventually_halt_at_global_cap() {
        let mut c = cfg();
        c.max_total_budget_usd = 100.0;
        c.max_per_market_usd = 1000.0; // don't let per-market be the limiter
        let mut s = SniperState::new();
        let markets = vec![market("T", Some(98), "active")];

        let mut scans = 0;
        loop {
            let planned = plan(&markets, &s, &c);
            if planned.is_empty() {
                break;
            }
            for o in planned {
                s.record(&o.ticker, o.notional_usd());
            }
            scans += 1;
            assert!(scans < 100, "should halt well before 100 scans");
        }
        assert!(s.total_deployed() <= 100.0);
        assert!(s.total_deployed() > 80.0, "should deploy near the cap");
    }
}
