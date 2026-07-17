# Kalshi Execution Engine — Design

**Date:** 2026-07-10
**Status:** Approved, implementing

## Goal

Give this repo a real, working automated-execution capability for **Kalshi** —
the venue-neutral core of what the Polymarket `polymarket-toolkits` engine does
(authenticated client, live market data, safe order placement with risk guards),
built Kalshi-native.

The existing repo is a branded launcher over a Polymarket copy-trading engine
with **no Kalshi support** (copy-trading watches on-chain whale wallets, which
Kalshi does not have; the one production path signs EIP-712 orders for the
Polymarket CTF Exchange on Polygon). We are not porting copy-trading — Kalshi
exposes no public wallets to copy. We are building the reusable execution engine
that every strategy sits on.

## Decisions (locked)

- **Code location:** standalone in THIS repo. We do not fork or depend on the
  upstream engine crate (it is a repo we do not control).
- **Reuse level:** fully standalone. Drop the `polymarket-toolkits` dependency
  and its Polymarket-shaped config entirely. No Polymarket concepts leak in.
- **Core capability (Phase A):** automated execution engine — auth + market data
  + safe order execution + risk guards. No strategy yet.
- **Phase B:** ratatui TUI as a view over the Phase A engine (specced later).
- **Safety default:** `enable_trading: false`, `mock_trading: true` — dry-run.

## Non-goals

- Copy-trading / whale tracking (no Kalshi equivalent).
- Any on-chain / EIP-712 / Polygon logic.
- Automated strategies (market-making, sniping, etc.) — built later on this core.
- The TUI (Phase B).

## Architecture

Single standalone binary crate. Layers depend inward only; leaves (`auth`,
`models`) have no internal deps and are unit-testable in isolation.

```
src/
  main.rs          # CLI (clap) — subcommands to exercise the engine
  config.rs        # Kalshi-native config schema
  auth.rs          # RSA-PSS request signing -> headers
  client.rs        # signs + sends REST requests, maps errors
  models.rs        # Market, Orderbook, Order, Fill, Position, Balance
  market_data.rs   # read: list markets, orderbook, positions, balance
  orders.rs        # write: place/cancel — gated by enable_trading + mock_trading
  risk.rs          # depth guard, trade floor, circuit breaker (venue-neutral)
  ws.rs            # (optional, phase A.2) live orderbook/fills websocket
  venue.rs         # Kalshi metadata (kept as-is)
```

Dependency flow: `main -> {market_data, orders} -> client -> auth`;
`orders -> risk`.

Dropped from the Polymarket engine: `wallets_to_track`, EIP-712/on-chain signing,
CTF exchange addresses, Polygon RPCs, copy-trading, whale tracking, Gamma/CLOB
Polymarket URLs.

## Auth (auth.rs)

Kalshi uses **API key ID (UUID) + RSA private key**, RSA-PSS/SHA-256 request
signing. Every private request carries three headers, computed per-request:

- `KALSHI-ACCESS-KEY`: the API key ID (UUID).
- `KALSHI-ACCESS-TIMESTAMP`: current time in **milliseconds** since epoch.
- `KALSHI-ACCESS-SIGNATURE`:
  `base64( RSA-PSS-SHA256-sign( timestamp + METHOD + path ) )`

Signed string = concat of the millisecond timestamp, the **uppercase** HTTP
method, and the request **path including `/trade-api/v2`, excluding query string**.
PSS salt length = digest length (32).

- Parse the PKCS#1 RSA key once at startup.
- `auth.rs` exposes `sign(method, path) -> (timestamp_ms, signature_b64)`.
- Crates: `rsa`, `sha2`, `base64`.

**Open assumption to verify empirically:** whether the signed path includes the
`/trade-api/v2` prefix. Kalshi's docs say yes; the first authenticated GET
(`/portfolio/balance`) confirms it — `200` = correct, `401` = adjust signing
string. This is the fast feedback loop, not a guess baked in blind.

## Order safety (orders.rs)

Two-gate model, carried over verbatim from the Polymarket engine (venue-neutral,
good design):

```
enable_trading = false  -> compute + log the order, DO NOT send   (dry-run)
mock_trading   = true   -> order path returns early with a log    (mock)
live order sent  <=>  enable_trading == true  AND  mock_trading == false
```

- Both flags must be permissive for a real order. Default config is safe.
- Every write path checks the gate **before** any network call and returns a
  synthetic "would-have-sent" result in dry-run.
- `risk.rs` guards run **before** the gate, so dry-run still reports whether an
  order would have been rejected (thin depth, tripped breaker, sub-floor size).

## Config schema (config.rs)

Single Kalshi-native `config.yaml` replaces the Polymarket `config.json` + yaml.
`config.json` is removed.

```yaml
venue: kalshi
enable_trading: false      # dry-run gate 1
mock_trading: true         # dry-run gate 2

credentials:
  api_key_id: "..."        # the UUID (was api_key)
  private_key: |           # RSA PKCS#1 PEM (was api_secret, bare base64)
    -----BEGIN RSA PRIVATE KEY-----
    ...
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
```

Renames vs. current file: `api_key` -> `api_key_id`, `api_secret` -> `private_key`
(as a PEM block). Existing values migrated into this shape. `strategies:` block
removed for Phase A.

## Data flow

`main` parses config -> builds signed `client` -> read command
(`markets` / `orderbook <ticker>` / `balance`) via `market_data` -> print.
Write command (`buy` / `sell`) -> build `PlannedOrder` -> `risk` guards ->
two-gate check -> log "would send" (dry-run) or sign + POST (live) -> print.

## Testing & verification

**Unit (no network):**
- `auth`: fixed key + fixed timestamp -> exact known signature; verify with
  public key. Locks the signing-string format against regressions.
- `risk`: depth guard, trade floor, circuit-breaker trip + cooldown. Table-driven.
- `orders`: two-gate matrix — live POST attempted **only** when
  `enable_trading && !mock_trading`; other three combinations return dry-run
  result and never touch the network (injected fake sender).
- `models`: deserialize sample Kalshi JSON into our types.

**Integration (real network, read-only):** first milestone is an authenticated
read (`balance` / `markets`). `200` proves the whole auth chain; `401` means the
signing string needs adjusting. This is the empirical auth check.

**Order path:** exercised in dry-run first (`enable_trading: false`) — place a
buy, confirm it logs the exact would-send order and never hits the network. A
single tiny real order only after the user reviews the would-send output and
gives explicit go-ahead at that moment.

**Discipline:** TDD per component; auth and risk are pure logic with clear
right answers — test first.

## Phase B (later)

Ratatui TUI as a view over the Phase A engine: live positions, balance, P&L,
circuit-breaker state. Reuses every layer; adds only rendering. Specced when we
reach it.
