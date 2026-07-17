# Crypto Convergence Strategy — Design

**Date:** 2026-07-10
**Status:** Approved, implementing

## Goal

A quantitative strategy that trades Kalshi crypto threshold markets ("will BTC be
above $X at time T") near expiry, when the underlying's distance from the strike
is large relative to how far it can realistically move in the remaining time.

Unlike the resolution sniper (which buys near-certainties at fair prices and has
~zero edge), this strategy has a real, computable edge: it compares the market's
price to a **model probability** derived from live spot price + realized
volatility, and bets only when the modeled edge exceeds fees plus a margin.

## The edge, precisely

Most of the gap between spot and a Kalshi crypto price is **fair volatility
pricing**, not error — with hours to close, crypto can move enough to cross the
strike. The edge appears **near expiry**, when little time remains for the
underlying to move but the market hasn't fully converged.

Per market, each scan:
1. `spot` (Coinbase) and `strike` (parsed from ticker `...-T<price>.99`).
2. `sigma_1min` = recent 1-min realized volatility (Coinbase candles).
3. `sigma_remaining = sigma_1min * sqrt(minutes_to_close) * safety_factor`.
4. `cushion = (spot - strike) / spot` (signed).
5. `z = cushion / sigma_remaining`.
6. `model_prob` = `norm_cdf(z)` — probability spot ends above strike (YES).
7. Direction + edge:
   - spot > strike: near-certain YES. `edge = model_prob - yes_ask`. Buy YES if edge big.
   - spot < strike: near-certain NO. `edge = (1 - model_prob) - no_ask`. Buy NO if edge big.
8. Bet only when `edge >= min_edge` (covers fees + margin).

Worked example (live data, 2026-07-10):
- BTC spot $64,128; 1-min stddev 0.061%.
- Over 5h (~300 min): sigma ~1.06% -> an 0.86% cushion is < 1 sigma -> 86c is FAIR, no edge.
- Over 10 min: sigma ~0.19% -> an 0.86% cushion is ~4.5 sigma -> model prob ~99.999%.
  If Kalshi still shows 86c, that is a genuine ~14c edge.

## Non-goals (v1)

- Non-crypto markets (weather etc.) — separate strategy later.
- Exit/hedging logic — bets are held to settlement.
- Sophisticated vol models (GARCH etc.) — realized vol from recent candles is enough.
- Multi-exchange spot aggregation — Coinbase only.

## Architecture

New module `src/strategy/crypto_convergence.rs` + `src/coinbase.rs` for the
external feed. Mirrors the sniper's pure-core / thin-loop split.

- **`coinbase.rs`** — public Coinbase API (no auth): `spot(symbol) -> f64`,
  `candles_1min(symbol) -> Vec<f64>` (closes), and `realized_vol_1min` from them.
  Isolated + mockable.
- **Pure core** — `evaluate(market, spot, sigma_1min, cfg, now) -> Option<CryptoSignal>`.
  All inputs as data; computes strike/cushion/z/model_prob/edge; returns a bet
  (side, limit price, edge) or None. Fully unit-testable, no network.
- **`norm_cdf(z)`** — normal CDF via an erf approximation. No external crate.
- **Thin run loop** — scan Kalshi crypto markets closing within
  `max_minutes_to_close` (reuse `markets_closing_within`), fetch spot+vol once
  per symbol, run `evaluate` per market, route bets through the existing
  `OrderPlacer` (two-gate safety, depth guard, budget caps, position seeding).

Ticker->strike parsing, the closing-soon scan, and the whole order/risk/gate path
already exist and are reused.

## Config

```yaml
strategies:
  crypto_convergence:
    enabled: true
    symbols: ["BTC", "ETH"]
    max_minutes_to_close: 30
    min_edge_cents: 3
    vol_lookback_minutes: 60
    safety_factor: 1.0
    per_trade_budget_usd: 2
    max_per_market_usd: 4
    max_total_budget_usd: 15
    scan_interval_secs: 20
```

## Safety (layered)

- Same two-gate model via `OrderPlacer` — `enable_trading`/`mock_trading` govern
  it identically; dry-run logs edge + would-be bet, sends nothing.
- `min_edge_cents` — fee firewall; a bet fires only if modeled edge exceeds fees
  plus margin. Stops fee-bleed.
- `safety_factor` (>1 = more conservative) inflates sigma, guarding against the
  model underestimating a sudden move.
- Position seeding + per-market/global budget caps (same as sniper).
- Depth guard still applies — won't bet into an empty book.

**Residual risk config can't remove:** the model assumes roughly-normal moves and
that realized vol predicts the next few minutes. A jump (news, liquidation
cascade) can blow through the cushion — tail risk. `safety_factor` mitigates but
does not eliminate it. Hence: backtest / dry-run before trusting it live.

## Testing

**Unit (pure, no network):**
- `norm_cdf`: z=0 -> 0.5; z=1.65 -> ~0.95; z=2.33 -> ~0.99.
- `evaluate` (table-driven): spot >> strike + little time -> bets YES; spot <
  strike -> bets NO; small cushion vs sigma -> no bet (fair vol); edge <
  min_edge -> no bet (fee firewall); safety_factor>1 suppresses marginal bets;
  beyond max_minutes_to_close -> skipped.
- `coinbase` vol math: fixed candle series -> known sigma.

**Integration (real, safe):** run against live Kalshi + live Coinbase in dry-run;
log every would-be bet with spot/strike/sigma/z/model_prob/edge; send nothing.
See the edge in the wild before risking capital.

**Gate:** do NOT go live until dry-run across several near-expiry windows shows
real, fillable edges (or a historical backtest confirms it beats fees). Dry-run
logging is built to make that assessment possible.

**Discipline:** TDD — `evaluate` and `norm_cdf` are pure with exact answers;
tests first.
