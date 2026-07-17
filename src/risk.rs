//! Venue-neutral risk guards, evaluated before any order is sent (even in
//! dry-run). Three independent checks:
//!
//! - **Trade floor** — reject orders below a minimum USD notional.
//! - **Depth guard** — reject orders when the relevant book side is too thin.
//! - **Circuit breaker** — after N "large" trades within a rolling window, trip
//!   and reject everything for a cooldown period.

use crate::models::{Orderbook, PlannedOrder};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct CircuitBreakerConfig {
    pub max_consecutive_large_trades: u32,
    pub window_seconds: u64,
    pub cooldown_seconds: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RiskConfig {
    pub min_orderbook_depth_usd: f64,
    pub min_trade_size_usd: f64,
    pub circuit_breaker: CircuitBreakerConfig,
    /// Orders at or above this USD notional count as "large" for the breaker.
    #[serde(default = "default_large_trade_usd")]
    pub large_trade_usd: f64,
}

fn default_large_trade_usd() -> f64 {
    100.0
}

/// Reason an order was rejected by a guard.
#[derive(Debug, Clone, PartialEq)]
pub enum Rejection {
    BelowTradeFloor { notional_usd: f64, floor_usd: f64 },
    InsufficientDepth { depth_usd: f64, required_usd: f64 },
    CircuitBreakerTripped { until_secs: u64 },
}

/// Stateful risk guard. `check` is pure w.r.t. its inputs except for the
/// circuit-breaker history, which it mutates as trades are recorded.
pub struct RiskGuard {
    cfg: RiskConfig,
    /// Timestamps (unix secs) of recent large trades, oldest first.
    large_trade_times: Vec<u64>,
    /// If tripped, the unix-secs time at which trading may resume.
    tripped_until: Option<u64>,
}

impl RiskGuard {
    pub fn new(cfg: RiskConfig) -> Self {
        Self { cfg, large_trade_times: Vec::new(), tripped_until: None }
    }

    /// Evaluate all guards for `order` against `book` at time `now_secs`.
    /// Returns `Ok(())` if the order may proceed, or the first `Rejection`.
    pub fn check(
        &mut self,
        order: &PlannedOrder,
        book: &Orderbook,
        now_secs: u64,
    ) -> Result<(), Rejection> {
        // Circuit breaker first — a tripped breaker blocks everything.
        if let Some(until) = self.tripped_until {
            if now_secs < until {
                return Err(Rejection::CircuitBreakerTripped { until_secs: until });
            }
            self.tripped_until = None;
        }

        // Trade floor.
        let notional = order.notional_usd();
        if notional < self.cfg.min_trade_size_usd {
            return Err(Rejection::BelowTradeFloor {
                notional_usd: notional,
                floor_usd: self.cfg.min_trade_size_usd,
            });
        }

        // Depth guard — the side we're taking must have enough resting notional.
        let depth = book.depth_usd_for(order.side);
        if depth < self.cfg.min_orderbook_depth_usd {
            return Err(Rejection::InsufficientDepth {
                depth_usd: depth,
                required_usd: self.cfg.min_orderbook_depth_usd,
            });
        }

        Ok(())
    }

    /// Guard for an **exit** (e.g. a stop-loss sell), not an entry. The trade
    /// floor and depth guards deliberately do NOT apply: those exist to stop us
    /// from *entering* bad positions, but when cutting a loss we want out
    /// regardless of position size or how thin the (collapsing) book is —
    /// applying entry guards there is exactly what traps us in a loser. Only the
    /// circuit breaker still applies, since it governs runaway *sending*, not
    /// position risk.
    pub fn check_exit(&mut self, now_secs: u64) -> Result<(), Rejection> {
        if let Some(until) = self.tripped_until {
            if now_secs < until {
                return Err(Rejection::CircuitBreakerTripped { until_secs: until });
            }
            self.tripped_until = None;
        }
        Ok(())
    }

    /// Record that an order was sent, updating the circuit breaker. Call this
    /// only for orders that actually proceed. Trips the breaker if too many
    /// large trades land within the window.
    pub fn record_trade(&mut self, order: &PlannedOrder, now_secs: u64) {
        if order.notional_usd() < self.cfg.large_trade_usd {
            return;
        }
        let window = self.cfg.circuit_breaker.window_seconds;
        // Drop entries outside the rolling window.
        self.large_trade_times.retain(|&t| now_secs.saturating_sub(t) < window);
        self.large_trade_times.push(now_secs);

        if self.large_trade_times.len() as u32 >= self.cfg.circuit_breaker.max_consecutive_large_trades {
            self.tripped_until = Some(now_secs + self.cfg.circuit_breaker.cooldown_seconds);
            self.large_trade_times.clear();
        }
    }

    pub fn is_tripped(&self, now_secs: u64) -> bool {
        matches!(self.tripped_until, Some(until) if now_secs < until)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Level, OrderType, Side};

    fn cfg() -> RiskConfig {
        RiskConfig {
            min_orderbook_depth_usd: 250.0,
            min_trade_size_usd: 5.0,
            circuit_breaker: CircuitBreakerConfig {
                max_consecutive_large_trades: 3,
                window_seconds: 60,
                cooldown_seconds: 300,
            },
            large_trade_usd: 100.0,
        }
    }

    fn order(count: u32, price: u8) -> PlannedOrder {
        PlannedOrder {
            ticker: "T".into(),
            side: Side::Yes,
            order_type: OrderType::Limit,
            count,
            price_cents: price,
        }
    }

    /// A deep-enough yes book: 1000 contracts @ 50c = $500.
    fn deep_book() -> Orderbook {
        Orderbook {
            ticker: "T".into(),
            yes: vec![Level { price_cents: 50, size: 1000 }],
            no: vec![Level { price_cents: 50, size: 1000 }],
        }
    }

    #[test]
    fn rejects_below_trade_floor() {
        let mut g = RiskGuard::new(cfg());
        // 1 contract @ 40c = $0.40 < $5 floor.
        let r = g.check(&order(1, 40), &deep_book(), 0);
        assert!(matches!(r, Err(Rejection::BelowTradeFloor { .. })));
    }

    #[test]
    fn rejects_thin_depth() {
        let mut g = RiskGuard::new(cfg());
        let thin = Orderbook {
            ticker: "T".into(),
            yes: vec![Level { price_cents: 50, size: 10 }], // $5 depth < $250
            no: vec![],
        };
        // 20 @ 50c = $10 notional (clears floor), but book is thin.
        let r = g.check(&order(20, 50), &thin, 0);
        assert!(matches!(r, Err(Rejection::InsufficientDepth { .. })));
    }

    #[test]
    fn accepts_valid_order() {
        let mut g = RiskGuard::new(cfg());
        // 20 @ 50c = $10 notional, deep book.
        assert!(g.check(&order(20, 50), &deep_book(), 0).is_ok());
    }

    #[test]
    fn breaker_trips_after_n_large_trades_then_cools_down() {
        let mut g = RiskGuard::new(cfg());
        let big = order(400, 50); // 400 @ 50c = $200 >= $100 large threshold
        let book = deep_book();

        // 3 large trades within the 60s window -> trip.
        for t in [0u64, 10, 20] {
            assert!(g.check(&big, &book, t).is_ok());
            g.record_trade(&big, t);
        }
        assert!(g.is_tripped(21));

        // Blocked during cooldown (300s from t=20 -> until 320).
        assert!(matches!(
            g.check(&big, &book, 100),
            Err(Rejection::CircuitBreakerTripped { .. })
        ));

        // After cooldown, trading resumes.
        assert!(g.check(&big, &book, 321).is_ok());
        assert!(!g.is_tripped(321));
    }

    #[test]
    fn small_trades_do_not_trip_breaker() {
        let mut g = RiskGuard::new(cfg());
        let small = order(20, 50); // $10 < $100 large threshold
        let book = deep_book();
        for t in 0..10 {
            assert!(g.check(&small, &book, t).is_ok());
            g.record_trade(&small, t);
        }
        assert!(!g.is_tripped(11));
    }

    #[test]
    fn large_trades_outside_window_do_not_accumulate() {
        let mut g = RiskGuard::new(cfg());
        let big = order(400, 50);
        let book = deep_book();
        // Trades spaced > 60s apart never accumulate to 3-in-window.
        for t in [0u64, 100, 200, 300] {
            assert!(g.check(&big, &book, t).is_ok());
            g.record_trade(&big, t);
        }
        assert!(!g.is_tripped(301));
    }
}
