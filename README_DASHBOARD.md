# Quant Edge — Dashboard & Server Guide

## Quick start

```bash
# 1. Build release binary
cargo build --release

# 2. Set environment (copy and edit)
cp .env.example .env

# 3. Start the server
cargo run --bin quant-edge-server
# or: ./target/release/quant-edge-server

# 4. Open dashboard
#    http://localhost:8080
```

## Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `CACHE_FILE` | `cache.db` | SQLite database path |
| `GICS_FILE` | `data/gics.json` | GICS taxonomy JSON |
| `TICKERS_FILE` | `tickers.txt` | Universe ticker list (one per line) |
| `SERVER_PORT` | `8080` | HTTP port |
| `GEMINI_API_KEY` | _(none)_ | Google Gemini API key — required for AI strategy parsing and the "AI Picks" preset |
| `FRED_API_KEY` | _(none)_ | FRED key for macro data (VIX, yield curve). App runs without it but macro signal is disabled. |

Create a `.env` file in the project root with your keys:

```env
GEMINI_API_KEY=your_key_here
FRED_API_KEY=your_key_here
```

## Dashboard tabs

| Tab | What it does |
|-----|-------------|
| **Morning Briefing** | Daily scan — top picks, macro regime, paper portfolio snapshot. Hit "Run Morning Scan" to refresh. |
| **Strategy Playground** | Pick one of 12 preset strategies or describe your own in plain English. Click "Run" to see ranked picks with signal breakdown. |
| **Backtest Lab** | Backtest any strategy over a custom date range. Renders equity curve, drawdown, Sharpe, win rate. Results auto-save to the leaderboard. |
| **Paper Portfolio** | Simulated portfolio (no real money). Initialize with a strategy + capital, then track daily P&L. |
| **Universe Explorer** | Score every ticker in the universe. Filter by market (US / IN) or search by name. Click a row to open the signal detail modal. |
| **Correlations** | Industry correlation heatmap — see which sectors move together and where diversification breaks down. |

## REST API endpoints

The dashboard talks to the server via these endpoints — all available to external callers too.

```
GET  /api/health                  → {"status":"ok"}
GET  /api/morning                 → morning briefing JSON
GET  /api/evening                 → evening P&L JSON
POST /api/picks                   → run picking engine  (body: {market, strategy?, top_n?})
POST /api/strategy/parse          → parse NL → StrategySpec
GET  /api/strategies/presets      → list 12 preset strategies
POST /api/strategies/run          → run a preset  (body: {preset_id, top_n?})
GET  /api/universe                → score all universe tickers
GET  /api/signals/:ticker         → full signal breakdown for one ticker
POST /api/backtest                → run backtest  (body: {strategy, start_date, end_date, capital})
GET  /api/strategies/history      → leaderboard (sorted by Sharpe)
GET  /api/portfolio/history       → 30-day rolling P&L
GET  /api/correlations            → industry correlation matrix
GET  /api/paper/status            → paper portfolio snapshot
POST /api/paper/init              → initialise paper portfolio  (body: {strategy, capital})
POST /api/paper/update            → force price update on paper positions
```

## 12 preset strategies

| ID | Name | Bias |
|----|------|------|
| `momentum_growth` | Momentum + Growth | High momentum, high revenue growth |
| `deep_value` | Deep Value | Low P/B, high Piotroski |
| `quality_compounder` | Quality Compounder | High margins + low debt |
| `small_cap_momentum` | Small Cap Momentum | SmallCap filter + momentum |
| `dividend_fortress` | Dividend Fortress | Low leverage, large cap |
| `contrarian` | Contrarian Reversal | Negative momentum (mean-revert) |
| `insider_conviction` | Insider Conviction | Heavy insider-signal weight |
| `macro_sensitive` | Macro Sensitive | Macro gate + momentum |
| `sentiment_driven` | Sentiment Driven | News + Reddit sentiment |
| `pairs_arbitrage` | Pairs Arbitrage | Correlation divergence |
| `india_growth` | India Growth | NSE universe, growth focus |
| `ai_picks` | AI Picks | Gemini-generated strategy (requires API key) |

## Automation

### Windows (Task Scheduler)

```powershell
# Run once as Administrator
.\scripts\setup_task.ps1
```

Creates two tasks: **QuantEdge-Morning** (9:05 AM) and **QuantEdge-Evening** (4:35 PM), weekdays only.

### Linux / macOS (cron)

```bash
bash scripts/setup_cron.sh
```

Installs cron entries at 9:05 AM and 4:35 PM, Monday–Friday.

### Manual invocation

```powershell
# Windows
.\scripts\daily.ps1 -Mode morning
.\scripts\daily.ps1 -Mode evening
```

```bash
# Linux / macOS
bash scripts/daily.sh morning
bash scripts/daily.sh evening
```

## CLI (original binary)

The original CLI (`cargo run` / `quant-edge`) still works unchanged:

```
cargo run -- --market US --top-n 10
cargo run -- --morning
cargo run -- --evening
cargo run -- --paper-init --capital 100000
cargo run -- --paper-update
cargo run -- --backtest --start-date 2023-01-01 --end-date 2024-01-01
```

The server and CLI share the same SQLite cache — picks, paper portfolio, and backtest results are visible from both.

## Signal weights

Each signal is weighted 0.0–1.0 (re-normalised before use):

| Signal | Default weight | What it measures |
|--------|---------------|-----------------|
| Momentum | 0.30 | 12-1m price return vs peers, 200-day MA |
| Fundamental | 0.30 | Piotroski F-score, revenue growth, net margin, P/B |
| Insider | 0.20 | Net insider buying (SEC EDGAR filings) |
| Sentiment | 0.10 | News tone (GDELT) + Reddit mention surge |
| Pairs | 0.10 | Correlation-based mean-reversion within industry |

Final composite score is 0–100; macro gate (VIX + yield curve) halves non-cash scores in risk-off regimes.
