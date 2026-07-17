//! Order placement — the only path that can move real money, and therefore the
//! most guarded. Flow for every order:
//!
//! 1. Run risk guards ([`crate::risk::RiskGuard`]) — even in dry-run.
//! 2. Check the two-gate model (`enable_trading` && !`mock_trading`).
//! 3. Live only if both gates permit; otherwise return a synthetic
//!    "would-have-sent" outcome and never touch the network.

use crate::client::{KalshiClient, RequestSender};
use crate::models::{OrderType, PlannedOrder, Side};
use crate::risk::{RiskGuard, Rejection};
use anyhow::Result;
use serde_json::json;

/// What happened to an order request.
#[derive(Debug, Clone, PartialEq)]
pub enum OrderOutcome {
    /// A guard rejected it before any send decision.
    Rejected(Rejection),
    /// Gates blocked a live send; this is what *would* have been sent.
    DryRun { payload: String },
    /// Actually sent to Kalshi; carries the raw response body.
    Sent { response: String },
}

pub struct OrderPlacer<'a, S: RequestSender> {
    client: &'a KalshiClient<S>,
    guard: &'a mut RiskGuard,
    live_allowed: bool,
}

impl<'a, S: RequestSender> OrderPlacer<'a, S> {
    pub fn new(client: &'a KalshiClient<S>, guard: &'a mut RiskGuard, live_allowed: bool) -> Self {
        Self { client, guard, live_allowed }
    }

    /// Place an order against a known orderbook (used by the depth guard), at
    /// wall-clock `now_secs`.
    pub fn place(
        &mut self,
        order: &PlannedOrder,
        book: &crate::models::Orderbook,
        now_secs: u64,
    ) -> Result<OrderOutcome> {
        // 1. Risk guards — always, even in dry-run.
        if let Err(rej) = self.guard.check(order, book, now_secs) {
            return Ok(OrderOutcome::Rejected(rej));
        }

        let payload = order_payload(order);

        // 2 & 3. Two-gate check.
        if !self.live_allowed {
            return Ok(OrderOutcome::DryRun { payload });
        }

        let resp = self.client.request_json("POST", ORDER_ROUTE, Some(payload))?;
        self.guard.record_trade(order, now_secs);
        Ok(OrderOutcome::Sent { response: resp.to_string() })
    }

    /// Place an **exit** order (e.g. a stop-loss sell). Identical to [`place`]
    /// except it uses the exit guard, which skips the trade-floor and depth
    /// checks — an exit must be able to get out of a small position, or sell
    /// into a thin/collapsing book, which is precisely when a stop needs to
    /// fire. The circuit breaker and the two-gate live check still apply.
    pub fn place_exit(
        &mut self,
        order: &PlannedOrder,
        now_secs: u64,
    ) -> Result<OrderOutcome> {
        if let Err(rej) = self.guard.check_exit(now_secs) {
            return Ok(OrderOutcome::Rejected(rej));
        }

        let payload = order_payload(order);

        if !self.live_allowed {
            return Ok(OrderOutcome::DryRun { payload });
        }

        let resp = self.client.request_json("POST", ORDER_ROUTE, Some(payload))?;
        self.guard.record_trade(order, now_secs);
        Ok(OrderOutcome::Sent { response: resp.to_string() })
    }
}

/// Kalshi v2 create-order endpoint. (The v1 `/portfolio/orders` path now 410s.)
const ORDER_ROUTE: &str = "/portfolio/events/orders";

/// Build the Kalshi **v2** order body.
///
/// v2 quotes everything from the YES side with a single `bid`/`ask` book and
/// fixed-point dollar strings:
///   - buy YES  -> `side: "bid"`, `price` = yes price in dollars.
///   - buy NO   -> `side: "ask"` (offer to sell YES) at `(100 - no_price)` yes cents.
/// We use `immediate_or_cancel` so a snipe takes whatever is available now and
/// cancels the rest — no resting order left hanging.
fn order_payload(o: &PlannedOrder) -> String {
    let (side, yes_price_cents) = match o.side {
        Side::Yes => ("bid", o.price_cents),
        Side::No => ("ask", 100 - o.price_cents),
    };
    let tif = match o.order_type {
        // A market snipe still uses IOC; limit uses IOC too (take-or-cancel).
        OrderType::Market | OrderType::Limit => "immediate_or_cancel",
    };
    let body = json!({
        "ticker": o.ticker,
        "side": side,
        "count": format!("{}.00", o.count),
        "price": format!("{:.4}", yes_price_cents as f64 / 100.0),
        "time_in_force": tif,
        "self_trade_prevention_type": "taker_at_cross",
    });
    body.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Signer;
    use crate::client::{KalshiClient, RawResponse, RequestSender, SignedRequest};
    use crate::models::{Level, Orderbook};
    use crate::risk::{CircuitBreakerConfig, RiskConfig};
    use rsa::pkcs1::{EncodeRsaPrivateKey, LineEnding};
    use rsa::RsaPrivateKey;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    fn signer() -> Signer {
        let mut rng = rand::thread_rng();
        let key = RsaPrivateKey::new(&mut rng, 2048).unwrap();
        let pem = key.to_pkcs1_pem(LineEnding::LF).unwrap().to_string();
        Signer::new("kid", &pem).unwrap()
    }

    /// Counts how many times it was asked to send.
    #[derive(Clone)]
    struct CountingSender(Arc<AtomicU32>);
    impl RequestSender for CountingSender {
        fn send(&self, _req: SignedRequest) -> Result<RawResponse> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(RawResponse { status: 200, body: r#"{"order":{"order_id":"x"}}"#.into() })
        }
    }

    fn risk() -> RiskConfig {
        RiskConfig {
            min_orderbook_depth_usd: 100.0,
            min_trade_size_usd: 5.0,
            circuit_breaker: CircuitBreakerConfig {
                max_consecutive_large_trades: 5,
                window_seconds: 60,
                cooldown_seconds: 300,
            },
            large_trade_usd: 100.0,
        }
    }

    fn deep_book() -> Orderbook {
        Orderbook {
            ticker: "T".into(),
            yes: vec![Level { price_cents: 50, size: 1000 }],
            no: vec![Level { price_cents: 50, size: 1000 }],
        }
    }

    fn valid_order() -> PlannedOrder {
        PlannedOrder {
            ticker: "T".into(),
            side: Side::Yes,
            order_type: OrderType::Limit,
            count: 20,
            price_cents: 50, // $10 notional, clears floor
        }
    }

    fn run_gate(live_allowed: bool) -> (OrderOutcome, u32) {
        let count = Arc::new(AtomicU32::new(0));
        let sender = CountingSender(count.clone());
        let client = KalshiClient::new("https://host/trade-api/v2", signer(), sender);
        let mut guard = RiskGuard::new(risk());
        let mut placer = OrderPlacer::new(&client, &mut guard, live_allowed);
        let outcome = placer.place(&valid_order(), &deep_book(), 0).unwrap();
        (outcome, count.load(Ordering::SeqCst))
    }

    #[test]
    fn live_send_only_when_gate_allows() {
        // live_allowed = true  -> exactly one network send.
        let (outcome, sends) = run_gate(true);
        assert!(matches!(outcome, OrderOutcome::Sent { .. }));
        assert_eq!(sends, 1);
    }

    #[test]
    fn dry_run_never_touches_network() {
        // live_allowed = false -> DryRun, zero sends.
        let (outcome, sends) = run_gate(false);
        assert!(matches!(outcome, OrderOutcome::DryRun { .. }));
        assert_eq!(sends, 0, "dry-run must not send");
    }

    #[test]
    fn risk_rejection_short_circuits_before_any_send_even_when_live() {
        let count = Arc::new(AtomicU32::new(0));
        let client = KalshiClient::new("https://host/trade-api/v2", signer(), CountingSender(count.clone()));
        let mut guard = RiskGuard::new(risk());
        let mut placer = OrderPlacer::new(&client, &mut guard, true);

        // Sub-floor order: 1 @ 50c = $0.50 < $5.
        let tiny = PlannedOrder {
            ticker: "T".into(),
            side: Side::Yes,
            order_type: OrderType::Limit,
            count: 1,
            price_cents: 50,
        };
        let outcome = placer.place(&tiny, &deep_book(), 0).unwrap();
        assert!(matches!(outcome, OrderOutcome::Rejected(_)));
        assert_eq!(count.load(Ordering::SeqCst), 0, "rejected orders never send");
    }

    #[test]
    fn dry_run_payload_is_v2_shape() {
        // Order: buy YES, 20 contracts @ 50c -> v2 bid, price "0.5000", count "20.00".
        let (outcome, _) = run_gate(false);
        if let OrderOutcome::DryRun { payload } = outcome {
            assert!(payload.contains("\"ticker\":\"T\""), "{payload}");
            assert!(payload.contains("\"side\":\"bid\""), "buy yes = bid: {payload}");
            assert!(payload.contains("\"price\":\"0.5000\""), "yes price in dollars: {payload}");
            assert!(payload.contains("\"count\":\"20.00\""), "fixed-point count: {payload}");
            assert!(payload.contains("immediate_or_cancel"), "{payload}");
        } else {
            panic!("expected DryRun");
        }
    }

    #[test]
    fn exit_bypasses_trade_floor_and_depth_guards() {
        // A sub-floor sell into a paper-thin book: `place` would REJECT it, but
        // `place_exit` must let it through (minus the two-gate check) so a
        // stop-loss can actually get out of a small/collapsing position.
        let count = Arc::new(AtomicU32::new(0));
        let client = KalshiClient::new("https://host/trade-api/v2", signer(), CountingSender(count.clone()));
        let mut guard = RiskGuard::new(risk()); // floor $5, depth $100
        let mut placer = OrderPlacer::new(&client, &mut guard, true); // live

        // Sell YES @ 70c (buy NO @ 30c), only 1 contract = $0.30 notional,
        // into an essentially empty book — both entry guards would reject.
        let exit = PlannedOrder {
            ticker: "T".into(),
            side: Side::No,
            order_type: OrderType::Market,
            count: 1,
            price_cents: 30,
        };
        let thin = Orderbook { ticker: "T".into(), yes: vec![], no: vec![] };

        // Sanity: the normal entry path DOES reject this.
        let rejected = placer.place(&exit, &thin, 0).unwrap();
        assert!(matches!(rejected, OrderOutcome::Rejected(_)), "entry guard should reject a sub-floor thin order");
        assert_eq!(count.load(Ordering::SeqCst), 0);

        // The exit path sends it.
        let sent = placer.place_exit(&exit, 0).unwrap();
        assert!(matches!(sent, OrderOutcome::Sent { .. }), "exit must bypass floor/depth guards");
        assert_eq!(count.load(Ordering::SeqCst), 1, "exit actually sent");
    }

    #[test]
    fn exit_still_respects_two_gate_dry_run() {
        // Even on the exit path, dry-run must not touch the network.
        let count = Arc::new(AtomicU32::new(0));
        let client = KalshiClient::new("https://host/trade-api/v2", signer(), CountingSender(count.clone()));
        let mut guard = RiskGuard::new(risk());
        let mut placer = OrderPlacer::new(&client, &mut guard, false); // dry-run
        let exit = PlannedOrder {
            ticker: "T".into(),
            side: Side::No,
            order_type: OrderType::Market,
            count: 1,
            price_cents: 30,
        };
        let outcome = placer.place_exit(&exit, 0).unwrap();
        assert!(matches!(outcome, OrderOutcome::DryRun { .. }));
        assert_eq!(count.load(Ordering::SeqCst), 0, "dry-run exit must not send");
    }

    #[test]
    fn buy_no_becomes_ask_at_complement_price() {
        let o = PlannedOrder {
            ticker: "T".into(),
            side: Side::No,
            order_type: OrderType::Limit,
            count: 5,
            price_cents: 30, // buy NO @ 30c == sell YES @ 70c
        };
        let payload = order_payload(&o);
        assert!(payload.contains("\"side\":\"ask\""), "{payload}");
        assert!(payload.contains("\"price\":\"0.7000\""), "100-30=70c: {payload}");
    }
}
