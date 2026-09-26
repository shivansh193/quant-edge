# quant-edge

[![CI](https://github.com/shivansh193/quant-edge/actions/workflows/ci.yml/badge.svg)](https://github.com/shivansh193/quant-edge/actions/workflows/ci.yml)
[![Daily forward-test log](https://github.com/shivansh193/quant-edge/actions/workflows/forward-log.yml/badge.svg)](https://github.com/shivansh193/quant-edge/actions/workflows/forward-log.yml)

A Rust quant research platform: a point-in-time data layer, a multi-factor
stock-picking engine, a realistic backtester, and — the part that actually
matters — a tamper-evident **forward-test log** that records picks before
their outcomes exist, so results can't be tuned with hindsight after the
fact.

Backtests can always be made to look good by fitting to the past. This
project is built around the belief that the only honest evidence is a
prediction logged *before* you know if it was right, so most of the design
effort goes into making that log impossible to quietly edit and impossible
to poison with look-ahead bias in the first place.

## What makes this different

- **Point-in-time discipline is structural, not a promise.** An `AsOf` view
  physically cannot return data dated after its as-of date — clamped in SQL
  *and* re-filtered in Rust, so a bug in either layer can't leak the future.
  Fundamentals are keyed to their actual SEC filing date, not restated.
- **The forward-test log is hash-chained and append-only.** Every entry
  stores the SHA-256 of the one before it; editing or deleting any past
  entry breaks the chain (`--forward-verify`). Entries are committed to git,
  so their timestamp is external, independent evidence they weren't chosen
  with hindsight.
- **Runs unattended, daily, on real market data.** A GitHub Actions workflow
  logs the model's actual picks every US trading morning before the open —
  see the badge above for today's run.
- **Known limitations are documented, not hidden.** [docs/METHODOLOGY.md](docs/METHODOLOGY.md)
  states plainly what's *not* solved (survivorship bias status, no delisting
  handling, thin sentiment history) instead of overselling the numbers.

## Features

- **Multi-signal picking engine** — momentum, fundamental, insider, sentiment, and macro filters combined into a scored, ranked composite
- **Point-in-time fundamentals** — SEC XBRL filings keyed by filing date, not restated; point-in-time S&P 500 membership (survivorship-bias correction) for historical universes
- **Backtesting** — next-open fills, a real cost model (spread, slippage, square-root market impact), optional tax, benchmark/alpha, rank-IC signal validation, drawdown circuit breaker, beta/VaR/CVaR risk reporting
- **Forward-test log** — tamper-evident, hash-chained daily picks recorded *before* outcomes are known; `--forward-status` shows mark-to-market on open calls without waiting for the full holding period to mature
- **Portfolio risk controls** — position caps, Herfindahl concentration, historical VaR/CVaR, a drawdown breaker with hysteresis
- **Decision journal** — log your own discretionary calls against the model's picks and score agreement vs. disagreement over time
- **Real holdings & XIRR** — import actual broker trade history, reconcile against the model's current picks, multi-currency XIRR
- **Staleness & earnings awareness** — flags picks with no recent price data, and picks reporting earnings within a configurable window (live-only by design — see Methodology)
- **Paper trading** — simulated live positions without real capital; morning/evening routine automation
- **LLM strategy layer** — parse natural-language investment strategies into executable specs via the Gemini API
- **Correlation guard** — pairwise correlation and concentration analysis to avoid overlapping positions
- **Multi-market** — NSE (India) and NYSE/NASDAQ (US), with an experimental web dashboard (see [README_DASHBOARD.md](README_DASHBOARD.md))
- **Data sources** — Yahoo Finance, SEC EDGAR (XBRL + Form 4 insider filings), FRED (with a Yahoo fallback). GDELT and Reddit were evaluated live and disabled — see Methodology for why — rather than shipped as fake signals.

## Prerequisites

- Rust 1.75+ (`rustup`)
- A `.env` file with API keys (see `.env.example`)

## Installation

```bash
git clone https://github.com/shivansh193/quant-edge.git
cd quant-edge
cargo build --release
```

## Configuration

Copy `.env.example` to `.env` and fill in your keys:

```bash
cp .env.example .env
```

Put your stock universe in `tickers.txt`, one ticker per line (or use `--us`
for an automatic, point-in-time S&P 500 universe). Industry data is read
from `data/gics.csv`.

## Usage

```bash
# Score and rank stocks in your universe
./target/release/quant-edge

# The daily morning report: auto-universe, picks, staleness/earnings warnings
./target/release/quant-edge --morning --us

# Run a backtest over a historical period
./target/release/quant-edge --backtest --start 2023-01-01 --end 2024-01-01

# Check the forward-test log's integrity and see how open calls are doing
./target/release/quant-edge --forward-verify
./target/release/quant-edge --forward-status

# Import real broker trades and reconcile against the model's current picks
./target/release/quant-edge --holdings-import trades.csv --reconcile

# Use a natural-language strategy via the LLM layer
./target/release/quant-edge --strategy "Focus on high-momentum large-cap tech stocks with strong insider buying"
```

Run `./target/release/quant-edge --help` for the full list of flags (~50).

## Project structure

```
src/
├── backtest/       — backtesting engine and reports
├── correlations/   — correlation analysis and concentration guard
├── daily/          — morning/evening routine automation, point-in-time backfill
├── data/           — data fetchers (Yahoo, SEC EDGAR, FRED) and the AsOf point-in-time cache
├── forward_test/   — the hash-chained forward-test log: record, verify, evaluate, diff
├── gics/           — GICS taxonomy and industry classification
├── llm/            — Gemini integration for strategy parsing
├── metrics/        — performance statistics, rank IC, analytics
├── paper_trading/  — paper trading simulation and trade log
├── portfolio/      — simulation engine, rebalancer, and weights
├── report/         — CLI output and reporting
├── roles/          — role classifier (growth, revenue, profitability, etc.)
├── server/, bin/server.rs — an HTTP API for the dashboard (see README_DASHBOARD.md)
├── signals/        — signal implementations (momentum, fundamental, etc.)
├── universe/       — stock universe builder, point-in-time S&P 500 membership
├── risk.rs         — beta, VaR/CVaR, concentration, drawdown breaker
├── journal.rs      — discretionary decision journal
├── holdings.rs     — real broker-trade import, reconciliation, XIRR
├── fx.rs           — currency conversion for multi-market portfolios
├── staleness.rs, earnings.rs — data-freshness and earnings-proximity warnings
├── main.rs
└── lib.rs
frontend/           — Next.js dashboard UI (see README_DASHBOARD.md)
```

## Signals

| Signal | Description |
|--------|-------------|
| Momentum | Price trend over configurable lookback windows |
| Fundamental | Earnings, revenue growth, profitability ratios — point-in-time SEC filings |
| Insider | SEC Form 4 open-market buy/sell activity |
| Sentiment | News/social — currently disabled (see Methodology); reported as unavailable, not fabricated |
| Macro | VIX / 10Y yield regime overlay, with a fail-open warning if both are missing |

## Roles

Stocks are classified into one of seven roles that influence portfolio weighting: `FastestGrower`, `LargestByRevenue`, `MostProfitable`, `MostLeveraged`, `ConsumerReach`, `DeepValue`, `MomentumLeader`.

## Testing

```bash
cargo test --lib                              # 220+ unit tests, no network
cargo test --test live_data -- --ignored       # opt-in checks against the real SEC / Yahoo APIs
cargo clippy --all-targets -- -D clippy::correctness -D clippy::suspicious
```

Set `SEC_USER_AGENT="Your Name you@example.com"` in `.env` (SEC asks clients
to identify themselves). CI runs build, tests and the clippy correctness
gate on every push. Several critical paths (the drawdown breaker, position
caps, retry logic) are mutation-tested — the fix is verified by reverting it
and confirming the test actually fails.

## Forward testing

```bash
quant-edge --morning --us            # writes forward_log/YYYY-MM-DD.json
quant-edge --forward-verify          # verify the hash chain
quant-edge --forward-status          # mark-to-market on calls still open
quant-edge --forward-eval            # score matured entries against the benchmark
quant-edge --diff                    # what changed since the last entry
scripts/daily_forward.ps1 -Push      # run + commit + push (also runs on GitHub Actions daily)
```

Don't trust `--forward-eval` with fewer than ~30 matured entries — the log
says so itself.

## Documentation

- [docs/METHODOLOGY.md](docs/METHODOLOGY.md) — what point-in-time means here, how trades are simulated, and every known limitation stated plainly
- [docs/AUDIT.md](docs/AUDIT.md) — real bugs found in review, how each was diagnosed, and how the fix was verified
- [README_DASHBOARD.md](README_DASHBOARD.md) — the web dashboard and API server

## License

Private — all rights reserved.
