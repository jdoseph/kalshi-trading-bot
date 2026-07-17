# Resolution Sniper Strategy — Design

**Date:** 2026-07-10
**Status:** Approved, implementing

## Goal

The first automated strategy on the Kalshi execution engine. Buys YES contracts
in markets trading at or above a price threshold (near-certainties), for the
small remaining edge to the $1.00 settlement. Runs continuously with strict
budget caps, routed through the engine's existing two-gate / risk-guarded order
path so it is dry-run-safe by default.

This is v1: deliberately simple, price-only signal, so the logic is easy to
trust and reason about. Not a money printer — the edge is thin and fee-sensitive
(see caveats). Value is a real, measured strategy on solid infrastructure.

## Decisions (locked)

- **Signal:** price threshold only — buy YES when `yes_ask >= threshold_cents`
  and market `status == "active"`. The price is the market's own probability
  estimate. (Depth guard at execution still filters empty-book traps.)
- **Sizing:** fixed USD budget per snipe — `contracts = floor(budget / (ask/100))`.
- **Loop:** continuous scan every `scan_interval_secs`, with a global
  `max_total_budget_usd` cap. Keeps scanning but stops opening positions once
  the cap is hit.
- **Re-buy:** allowed up to a per-market cap (`max_per_market_usd`). Track
  deployed-per-ticker; stop that ticker at its cap.
- **Safety:** reuse the existing `OrderPlacer` — `enable_trading` + `mock_trading`
  govern it identically to manual orders. Dry-run logs "would-snipe" and counts
  it against budgets, sends nothing.

## Non-goals (v1)

- Time-to-close or spread/liquidity in the *signal* (price-only for now).
- Percentage-of-balance sizing.
- Surviving restarts via live-position reconciliation (state is per-run).
- Selling / exit logic — snipes are held to settlement.

## Behavior — one scan cycle

1. **Scan** — `market_data.markets()`.
2. **Filter** — keep markets with `yes_ask >= threshold_cents` and
   `status == "active"`.
3. **Budget gate** — skip a candidate if its ticker has hit `max_per_market_usd`
   or the global deployed total has hit `max_total_budget_usd`.
4. **Size** — `contracts = floor(per_snipe_budget_usd / (yes_ask/100))`, capped
   to not exceed remaining per-market or global budget. Skip if 0.
5. **Execute** — build a `PlannedOrder` (buy YES, limit at `yes_ask`), route
   through `OrderPlacer`. Depth guard, trade floor, and the two-gate all apply.
   On success (or dry-run "would send"), add notional to per-ticker and global
   tallies.
6. **Sleep** `scan_interval_secs`, repeat.

**Dry-run fidelity:** tallies increment on planned orders even in dry-run, so a
dry-run faithfully simulates how the caps fire live.

## Config

New optional block in `config.yaml` (existing configs without it still load):

```yaml
strategies:
  resolution_sniper:
    enabled: true
    threshold_cents: 97
    per_snipe_budget_usd: 20
    max_per_market_usd: 60
    max_total_budget_usd: 500
    scan_interval_secs: 30
```

Parses into `SniperConfig` in `config.rs` as `Option<...>`.

## Code structure

New module `src/strategy/resolution_sniper.rs` (new `strategy/` dir for future
strategies). Split for testability:

- **`SniperState`** (pure): `deployed_per_ticker: HashMap<String, f64>`,
  `deployed_total: f64`. Methods: `remaining_for(ticker, cap)`,
  `remaining_total(cap)`, `record(ticker, usd)`.
- **`plan_snipes(markets, state, cfg) -> Vec<PlannedOrder>`** (pure): the whole
  decision — filter, budget caps, sizing. Data in, orders out. No I/O.
- **`run(...)`** (thin loop): `market_data.markets()` -> `plan_snipes` ->
  `OrderPlacer` per order -> update state on success -> sleep -> repeat. The only
  part touching network/time.

New CLI subcommand `snipe` in `main.rs`, with the same dry-run banner.

**Dependency flow:** `resolution_sniper -> {market_data, orders, risk, config}` —
all existing layers, nothing new underneath.

## Testing

**Unit (pure, no network):**
- `SniperState`: `record` accumulates per-ticker + total; `remaining_*` shrink;
  ticker at cap reports 0.
- `plan_snipes` (table-driven):
  - below threshold excluded; at/above included.
  - non-active excluded.
  - sizing: `$20 @ 98c -> 20`; `$20 @ 80c -> 25`; rounds-to-0 -> no order.
  - per-market cap halts a ticker at `max_per_market_usd`.
  - global cap -> empty result once total exhausted.
  - multi-scan simulation: feed `plan_snipes` output into `state.record`
    repeatedly; assert caps eventually halt it.

**Integration (real network, safe):** run `snipe` against live Kalshi in dry-run
(`enable_trading: false`). Scans real markets, logs every "would-snipe" with
payload + running tallies, sends nothing. A genuine live-data paper-trade.

**Discipline:** TDD — `plan_snipes` and `SniperState` are pure; tests first.

## Caveats (carried into any live use)

- Thin edge: buying 98c -> $1.00 is ~2% gross, and Kalshi fees can eat much of it.
- Tail risk: "near-certain" isn't certain; rare NO resolutions lose the full
  stake. Distribution is many small gains, rare large losses.
- Liquidity: high-price markets often have little resting size; the depth guard
  will reject many. Expect fewer real fills than candidates.
- No realized return exists until measured. Dry-run paper-trading (and later a
  backtest) is the honest way to estimate it before risking capital.
