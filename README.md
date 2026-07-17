# Kalshi Trading Bot

A Rust automated execution engine for [Kalshi](https://kalshi.com) (CFTC-regulated US
prediction markets). It scans near-resolution and short-dated markets, applies risk
controls, and places orders through a guarded, dry-run-safe order path.

---

## ⚠️ Read this first — real money

This bot can place **real orders on a real account.** It is protected by a
**two-gate** model. A live order is sent **only** when **both** gates are open:

```yaml
enable_trading: true    # gate 1
mock_trading: false     # gate 2
```

If **either** gate is at its safe value (`enable_trading: false` **or**
`mock_trading: true`), the bot computes and logs what it *would* do but **sends
nothing to the network**. `config.example.yaml` ships with both gates safe.
**Leave them safe until you have watched a dry-run and understand the behavior.**

Your credentials live in `config.yaml`, which is **gitignored and never
committed**. Never move it into a tracked path, and never paste it anywhere.

---

## Setup

**Prerequisites:** Rust 1.70+ (`rustup`), a Kalshi account, and a Kalshi API key
(an API key ID + an RSA private key — generate these in your Kalshi account
settings under API access).

```bash
# 1. Clone
git clone https://github.com/jdoseph/kalshi-trading-bot.git
cd kalshi-trading-bot

# 2. Create your private config from the template
cp config.example.yaml config.yaml
#    Then edit config.yaml: paste your api_key_id and RSA private_key,
#    and keep enable_trading: false / mock_trading: true for now.

# 3. Build & test
cargo build --release
cargo test          # 73 tests should pass

# 4. Verify auth works (read-only, safe)
cargo run -- balance
```

If `balance` prints a dollar figure, your credentials and signing are working.

---

## Commands

The binary is `kalshi-bot`. All commands take an optional `--config <path>`
(defaults to `config.yaml`). During development use `cargo run -- <command>`.

| Command | What it does | Touches money? |
|---|---|---|
| `balance` | Print account balance (the canonical auth check). | No (read-only) |
| `positions` | List open positions (ticker, shares, cost, fees). | No (read-only) |
| `markets --limit N` | List open markets. | No (read-only) |
| `orderbook <TICKER>` | Show a market's orderbook + derived best asks/depth. | No (read-only) |
| `tui` | Live dashboard: balance, positions, circuit-breaker, mode. | No (read-only) |
| `snipe --max-pages N` | Run the **Resolution Sniper** loop continuously. | **Only if both gates open** |
| `crypto --max-pages N` | Run the **Crypto Convergence** loop continuously. | **Only if both gates open** |
| `buy <TICKER> <yes\|no> <count> <price_cents>` | Place one limit order. | **Only if both gates open** |

Read-only commands are always safe to run. `snipe`, `crypto`, and `buy` respect
the two-gate model — in dry-run they log `DRY-RUN would ...` and send nothing.

---

## Strategies

### Resolution Sniper (`snipe`)

Buys YES in near-certain markets (`yes_ask ≥ threshold_cents`, e.g. 97¢) to
collect the small remaining drift to $1.00, under strict budget caps.

> **Honest note on edge:** buying at ~97¢ to win $1 is roughly the *fair* price —
> the market already prices it at ~97% likely. After fees this is approximately
> break-even to slightly negative expected value. The risk controls below limit
> losses; they do not by themselves create profit. Real profit requires finding
> markets whose true probability exceeds the ask.

**Risk & growth controls:**

- **Stop-loss** — if a held position's best YES bid falls **below**
  `stop_loss_floor_cents` for `stop_loss_confirm_scans` consecutive scans, the
  whole position is sold (the 2-scan confirm filters spread/noise dips). Freed
  budget is released for redeployment.
- **Compounding** — when `position_pct_of_budget > 0`, each position is capped at
  that fraction of the **current working budget** (real cash, capped at
  `compound_target_usd`). As settled cash grows, position size grows with it; as
  it shrinks, positions shrink. In this mode `max_total_budget_usd` is ignored —
  the working budget is the global cap. New positions stop once the balance
  reaches `compound_target_usd`.

### Crypto Convergence (`crypto`)

Trades short-dated BTC/ETH threshold markets when a realized-volatility model
implies an edge over the market price, after fees. See
`docs/superpowers/specs/` for the design.

---

## Configuration reference (`resolution_sniper`)

```yaml
strategies:
  resolution_sniper:
    enabled: true
    threshold_cents: 97          # buy YES at or above this ask
    per_snipe_budget_usd: 2      # spend per individual order (non-compounding mode)
    max_per_market_usd: 4        # per-ticker cap (non-compounding mode)
    max_total_budget_usd: 1000   # global cap (IGNORED when compounding is on)
    scan_interval_secs: 30       # seconds between scans
    close_window_secs: 5400      # only markets closing within this many seconds
    min_open_interest: 100       # skip zero-OI placeholder markets

    # --- stop-loss ---
    stop_loss_floor_cents: 80    # sell if YES bid drops below this; 0 = disabled
    stop_loss_confirm_scans: 2   # require N consecutive scans below floor first

    # --- compounding ---
    position_pct_of_budget: 0.05 # each position ≤ this fraction of budget; 0 = off
    compound_target_usd: 5000    # stop opening new positions at this balance; 0 = none
```

**Key interactions to understand before running live:**
- Compounding **on** (`position_pct_of_budget > 0`): budget = `min(real cash,
  compound_target_usd)`; per-position cap = `position_pct_of_budget × budget`;
  `max_total_budget_usd` and `max_per_market_usd` are **not used**.
- Compounding **off** (`position_pct_of_budget: 0`): the fixed dollar caps apply,
  so set `max_total_budget_usd` to something sane for your account size.

---

## Development

```bash
cargo test        # unit tests (pure planning logic, order-gate safety, stop-loss)
cargo clippy      # lint
cargo build --release
```

The strategy logic is split into pure, tested functions (`plan_snipes`,
`plan_stops`, `SniperState`) and thin I/O loops, so the decision logic is
testable without network access. Order placement is the only path that can move
money and is the most guarded — see `src/orders.rs`.
