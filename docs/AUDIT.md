# Audit log

An independent review of the simulator found the issues below. Each was
reproduced, fixed, and covered by a test that fails on the old behaviour.
"Verified" means checked against real data or by reintroducing the bug and
watching the test fail (mutation check), not just by the test passing.

## Backtest validity

| # | Issue | Impact | Fix | Verified by |
|---|---|---|---|---|
| 1 | Fundamentals were Yahoo's *current* snapshot stamped with the historical `as_of` date | Look-ahead in every fundamentals-weighted backtest. The existing cache held **1,797 such rows across 48 dates** | Point-in-time facts from SEC filings by filing date; legacy rows quarantined by a `source` column | `asof` tests; live SEC test; real Apple filings (TTM revenue $387.5B after the Q1 FY23 10-Q, $394.3B before) |
| 2 | Signals ranked on the close were filled at the **same close** | Free look-ahead on the day of the signal | Fill at the next bar's open (adjusted basis) | Mutation: ignoring the open fails 2 tests |
| 3 | GDELT/Reddit/EDGAR fetched "now" data for historical dates | Present-day sentiment/insider stamped onto the past | Historical dates read stored, date-filtered data only | `asof` tests |
| 4 | Correlation-matrix cache picked `MAX(date)` by wall-clock freshness | Re-running an old backtest loaded a matrix built from later data | Lookup bound to `as_of` | `correlation_matrix_lookup_is_bound_to_the_as_of_date` |
| 5 | Rebalance dates were exact calendar offsets tested against trading days | Rebalances silently skipped on weekends/holidays; a Saturday start produced none | Roll each target to the next trading day | `rebalance_*`, `weekend_start_date_still_rebalances` |
| 6 | Portfolio-sim `is_checkpoint` compared day-of-month | Skipped months (day 29–31), weekends | Date-arithmetic checkpoints from the start date | `checkpoints` tests |
| 7 | Benchmark start fell back to `1.0` when the start date wasn't a trading day | Benchmark line and alpha were nonsense | First real benchmark close | — |

## Wrong numbers

| # | Issue | Fix | Verified by |
|---|---|---|---|
| 8 | Monte Carlo baseline drew n names from a pool of exactly n (the strategy's own holdings), so every "random" portfolio was identical | Sample from the whole universe without replacement | Mutation: restoring n-of-n fails `monte_carlo_draws_differ…` |
| 9 | Win rate was ~0%: entry prices were deleted before the lookup | Closed round trips tracked explicitly, net of costs | `win_rate_is_computed_from_closed_round_trips` |
| 10 | "Sharpe" was return/volatility with no risk-free rate; Sortino used the wrong downside definition | Excess-return Sharpe; downside deviation over all observations | `sharpe_uses_excess_returns`, `downside_deviation_*` |
| 11 | IC used Pearson on raw returns | Spearman rank IC + t-stat | `metrics::ic` tests |
| 12 | Walk-forward validator fed empty insider/news inputs, so three of four signals scored IC = 0 and were reported as "no edge"; sampled with replacement; "training period" unused | Point-in-time inputs via `AsOf`; unavailable signals reported *not evaluable*; edge needs significance; distinct sampling | `signals::backtest` tests |

## Silently broken components

| # | Issue | Fix | Verified by |
|---|---|---|---|
| 13 | **Insider signal returned nothing for every ticker.** (a) The client requested gzip but reqwest had no gzip support, so every CIK lookup failed to decode and the error was swallowed by `unwrap_or_default()`. (b) Form 4 URLs pointed at the XSL-rendered HTML page, not the raw XML. (c) `transactionCode` was read as a nested `<value>` and never found. (d) The filter comment said "P/S only" but an `\|\|` admitted grants, option exercises and tax withholding as if they were trades. (e) "Vice President" got the President weight. | Fixed all five | Live: NVDA returns 24 open-market trades in 180 days; parser unit tests on real filing structure |
| 14 | `revenue_cagr_3yr` and `price_return_12m_1m` were hard-coded `None`, so ~35% of the fundamental signal and one Piotroski criterion never contributed | Computed from SEC filings and prices | `revenue_cagr_*` tests |
| 15 | Macro-off set **every** composite to exactly 50, so ranking became arbitrary and the backtest bought whatever sorted first instead of holding cash; `macro_filter_enabled` was parsed but never read | Scores stay real; the gate is enforced at selection and honours the flag | `risk_off_liquidates_to_cash…`, `macro_gate_can_be_disabled…` |
| 16 | A failed data fetch produced a fabricated neutral 50 that could still be picked | Unscorable tickers are skipped | — |
| 17 | Missing signals were scored as neutral 0, diluting the composite toward 50 | Weights renormalised over available signals | `missing_signals_are_excluded…` |
| 18 | Morning handler derived the regime from `picks.first()` and defaulted to risk-on when empty — which the new gate makes common | Regime read from the full ranking | (found while fixing #15) |
| 19 | Tie-breaks depended on `HashMap` order, so identical runs could differ | Fixed industry order + ticker tie-break | `ties_break_by_ticker…`, `results_are_deterministic` |

## Data sources that had never worked

The existing cache held **0 insider rows, 0 news rows and 0 macro rows**, and
1,329 Reddit rows. Testing each source against its real API explains why:

| # | Source | What was wrong | Status |
|---|---|---|---|
| 22 | **Macro gate (VIX / 10Y)** | FRED's CSV endpoint never loaded (unreachable/timeouts), so `macro_data` was empty and the gate was **permanently fail-open** — `vix: null` in the sample picks file — with no warning. FRED's `GS10` is also a *monthly* series, too coarse for a 30-day spike test. | Fixed: daily `^VIX`/`^TNX` from Yahoo (FRED fallback, `DGS10`), and total failure now logs loudly. Verified live: VIX 66.0 → risk-off on 2020-03-20, risk-on mid-2021 |
| 23 | **News (GDELT)** | The article-list mode has no `tone` field, so `unwrap_or(0.0)` made every article a fabricated neutral; the API also rate-limits to 1 req / 5 s with a plain-text reply the parser choked on | **Disabled** with an explicit reason rather than fabricating data. Needs a redesign (see METHODOLOGY) |
| 24 | **Reddit** | Historical runs stamped today's search results with old dates; separately, the anonymous search endpoint now returns HTTP 403 for every request, confirmed by a direct curl (0.5s, not a timeout) | Rows collected later than their label are hidden from past dates; **fetching disabled** (needs OAuth) — see #26 |
| 26 | A 30-day `--backfill-days --us` run stalled: one day took 6.5 hours before it was killed. Root cause not fully isolated (Reddit's 403s don't arithmetically account for the whole gap), but two real, fixable causes were found and confirmed: (a) Reddit was wastefully retried for every ticker every day despite writing 0 rows the entire run; (b) `is_historical()` uses live wall-clock time, so a long-running backfill's tail dates can drift into the "live fetch" window it was never meant to enter | Reddit fetching disabled (#24); `--backfill-days` now clamps its end date to stay outside the live window regardless of run length, and each day gets a 180s hard budget so a stuck call is skipped, not stalled | `last_day_is_clamped_outside_the_live_window`; re-run completed without incident (see run log) |
| 25 | **Insider (EDGAR)** | see #13 | Fixed and verified live |

## Performance

| # | Issue | Fix |
|---|---|---|
| 20 | Price lookup scanned an entire hash map per ticker per day | `O(log n)` binary search over sorted series (`data/prices.rs`) |
| 21 | Ticker→CIK map (~1 MB) was re-downloaded on every miss | Process-wide map; fail fast once loaded |

## Tests

From 0 tests to 113 unit tests (plus 4 opt-in network/fixture tests), and CI
(`.github/workflows/ci.yml`) running build, tests and a clippy correctness gate.
