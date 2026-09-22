# Methodology

How quant-edge decides what it knows, how it simulates trading, and — just as
important — what it does **not** know. Read this before trusting any number the
tool prints.

## 1. Point-in-time discipline

Every result depends on one rule: **a decision on date `D` may only use
information that was public on `D`.** Violating it (look-ahead bias) is the most
common way a backtest lies.

It is enforced structurally, in `src/data/asof.rs`. An `AsOf` view is created
for a date and exposes read-only accessors (`price_bars`, `fundamentals`,
`insider_trades`, `news_items`, `reddit_snapshots`, `macro_points`,
`pit_facts`). Each one:

* clamps its query to the view date in SQL, **and**
* re-filters the returned rows in Rust,

so a bug in either layer cannot leak the future. There is no accessor that takes
a later date and no way to reach the underlying cache from a view. The tests in
`asof.rs` seed data on both sides of a date across every source and assert
nothing after it is returned.

### Data provenance

| Source | Point-in-time? | Usable in a backtest? |
|---|---|---|
| **Prices** (Yahoo) | Yes for *returns*. `adj_close` is back-adjusted with dividends/splits that happen later, so its *level* is not what was quoted then. Raw `close` is used for P/B and liquidity. | Yes |
| **Fundamentals** | US issuers: SEC XBRL filings, each value keyed by its **filing date**; restatements apply only once filed (`data/sec_facts.rs`). Non-US (e.g. `.NS`): none historically. | US only |
| **Insider trades** (EDGAR Form 4) | Filing date is stored and enforced. Open-market purchases/sales only (codes P/S). | Only history already cached; accumulates going forward |
| **News** (GDELT) | **Disabled.** Its article-list mode returns no tone and it allows 1 request / 5 s, so it cannot serve a 400-name universe. The signal reports "no data" and is excluded from the composite. A working version needs GDELT's TimelineTone mode and a shortlist. | No |
| **Reddit** | **Disabled.** The anonymous search endpoint now returns HTTP 403 for every request (confirmed live, not a timeout). Needs an OAuth client. A snapshot is valid only on the day it was taken; rows collected later than their label are also hidden from past dates. | No |
| **Macro** (VIX, 10Y) | Yes — daily `^VIX` / `^TNX` from Yahoo (FRED as fallback); these are not revised. If both are missing the gate is fail-open **and warns loudly**. | Yes |
| **Correlation matrix** | Cache lookups are bound to `as_of` (never loads a later matrix). | Yes |

Fundamentals rows carry a `source` column. Only `sec_pit` rows are served for a
historical date. Rows from before this column existed hold a *current* Yahoo
snapshot stamped with an old date, so they are quarantined and never returned
for the past.

A signal with no usable data for a ticker is reported as **unavailable**
(`SignalScore.availability`) and excluded from that ticker's composite — it is
not silently scored as neutral. See `signals::composite_score`.

## 2. Backtest mechanics (`src/backtest/engine.rs`)

* **Execution timing.** Signals are computed on day `t`'s close and filled at
  day `t+1`'s **open** (`ExecutionTiming::NextOpen`, the default). The old
  same-bar behaviour is available as `SameClose` for comparison only. Opens are
  put on the same adjusted basis as `adj_close` (`PriceBar::adj_open`).
* **Rebalance calendar.** Targets are `start + k·N` calendar days, each rolled
  to the first trading day on or after it. No rebalance is skipped because it
  fell on a weekend or holiday.
* **Costs** (`src/costs.rs`): per-order commission, half-spread, flat slippage,
  and square-root market impact `k · σ_daily · √(order / ADV)` using data
  strictly before the fill date. Total costs and annualised turnover are
  reported.
* **Macro gate.** In risk-off (VIX ≥ 25 or a 10Y spike) the strategy holds
  cash; `filters.macro_filter_enabled = false` turns this off. Scores stay real
  in risk-off — the gate is enforced at selection time (`signals::select_picks`).
* **Sizing.** Equal weight at entry; existing positions are kept, not
  rebalanced back to equal weight.
* **Metrics.** Sharpe uses daily *excess* returns (`risk_free_annual`, default
  0). Signal quality is **rank (Spearman) IC** with a t-statistic. Win rate is
  computed from closed round trips, net of costs. A benchmark return and alpha
  are reported when benchmark prices are available.
* **Tax** (optional): short/long-term capital-gains on realised trades, with
  India-equity and US presets. Rates are illustrative — verify them.

## 3. Signal validation (`--validate`)

Monthly test dates; for each signal, rank IC between the signal and the return
earned from the next open over a 30-day window. A signal is called to have an
edge only if `mean IC > 0.02` **and** `t > 2` over at least 6 dates. Signals
without enough history are reported as **not evaluable**, not as "no edge".
Nothing is fitted, so the whole range is out-of-sample.

## 4. Forward testing (`src/forward_test`)

The only result immune to tuning. The morning job writes
`forward_log/YYYY-MM-DD.json` *before* outcomes exist: picks, full ranking,
regime, code version and a universe hash. Entries are append-only and hash-chained
(`prev_hash`), so editing or removing any earlier entry breaks the chain:

```bash
quant-edge --forward-verify          # check the chain
quant-edge --forward-eval            # score matured entries vs. the benchmark
scripts/commit_forward_log.sh        # commit it, so git timestamps it externally
```

A hash chain cannot detect deleting the *most recent* entries; committing to git
covers that. Evaluation uses the same next-open convention as the backtester.
Don't read anything into fewer than ~30 matured entries.

## 5. Known limitations

Stated plainly, because they bias results:

* **Survivorship bias, partially fixed.** `--backfill-days --us` now uses each
  day's *actual* point-in-time S&P 500 membership (`universe::historical_membership`,
  a community-maintained dataset covering 1996-present, verified against a
  known fact: TSLA is absent before 2020-12-21 and present after). The live
  daily job and the NSE side still use today's list (correct for the live job;
  a genuine gap for NSE). Residual limitation: some long-delisted names that
  *were* index members no longer have any Yahoo price data at all, so they are
  still silently skipped rather than truly reconstructed — full survivorship
  correction needs a paid point-in-time price vendor.
* **No delisting handling.** A position whose price series ends is marked at its
  last price indefinitely.
* **Sentiment and insider history is thin** for the past (see §1); backtests of
  those signals are limited to data collected since the cache started filling.
* **Non-US fundamentals** have no point-in-time source; NSE tickers are scored
  on the signals that remain.
* **Yahoo is an unofficial, undocumented API** and can change or rate-limit.
* **No FX handling.** Mixed INR/USD universes are treated in percentage terms;
  portfolio-level currency conversion is not modelled.
* **No interest on cash;** equal weighting only; no shorting; no position limits.
* **Overlapping forward windows** in signal validation make t-stats indicative.
* **`--backfill-days` clamps its end date** to stay outside the live-fetch window (`LIVE_GRACE_DAYS`, currently 3 days) and gives each day a hard wall-clock budget, so it can never itself trigger the live-fetch stall described in docs/AUDIT.md.
* **Debt/equity** uses the latest long-term + current debt tags, which may come
  from slightly different balance-sheet dates.
* Tax modelling ignores loss carry-forward, wash sales, surcharge and cess.
