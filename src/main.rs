use anyhow::{Context, Result};
use chrono::NaiveDate;
use clap::{Parser, ValueEnum};
use tracing_subscriber::EnvFilter;

use quant_edge::{
    backtest::{BacktestConfig, BacktestEngine, print_backtest_report},
    correlations::{ConcentrationGuard, CorrelationEngine},
    daily::{run_morning, run_evening},
    data::{yahoo::YahooFinance, cache::Cache, DataSource},
    gics::GicsTaxonomy,
    llm::parse_strategy,
    paper_trading::{PaperTradingEngine, print_portfolio_status},
    universe::{UniverseBuilder, UniverseConfig},
    roles::{RoleClassifier, Role},
    portfolio::{SimulationEngine, SimulationConfig, RebalanceFrequency, WeightMode},
    metrics::compute_metrics,
    report::CliReporter,
    signals::{PickingEngine, WalkForwardValidator},
};

// ---------------------------------------------------------------------------
// Clap mirror enums (need ValueEnum; can't derive on library types directly)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, ValueEnum)]
enum CliMarket {
    Nse,
    Nyse,
    Both,
}

#[derive(Clone, Debug, ValueEnum)]
enum CliCapFilter {
    SmallCap,
    MidCap,
    LargeCap,
    Mixed,
}

#[derive(Clone, Debug, ValueEnum)]
enum CliRebalanceFreq {
    Daily,
    Weekly,
    Monthly,
    Quarterly,
    Annual,
}

#[derive(Clone, Debug, ValueEnum)]
enum CliWeightMode {
    Equal,
    RoleWeighted,
}

#[derive(Clone, Debug, ValueEnum)]
enum CliRole {
    FastestGrower,
    LargestByRevenue,
    MostProfitable,
    MostLeveraged,
    ConsumerReach,
    DeepValue,
    MomentumLeader,
}

// ---------------------------------------------------------------------------
// CLI struct
// ---------------------------------------------------------------------------

/// VC-style portfolio simulator — backtests a rules-based multi-industry portfolio
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Path to ticker file (one ticker per line, e.g. "RELIANCE.NS")
    #[arg(short, long, default_value = "tickers.txt")]
    tickers: String,

    /// Path to GICS taxonomy CSV
    #[arg(long, default_value = "data/gics.csv")]
    gics: String,

    /// Path to SQLite cache file
    #[arg(long, default_value = "cache.db")]
    cache: String,

    /// Backtest start date (YYYY-MM-DD)
    #[arg(long, default_value = "2020-01-01")]
    start: String,

    /// Backtest end date (YYYY-MM-DD)
    #[arg(long, default_value = "2024-12-31")]
    end: String,

    /// Starting capital in base currency units
    #[arg(long, default_value_t = 1_000_000.0)]
    capital: f64,

    /// Number of GICS industries to include
    #[arg(long, default_value_t = 20)]
    n_industries: usize,

    /// Market filter
    #[arg(long, default_value = "nse")]
    market: CliMarket,

    /// Market cap filter
    #[arg(long, default_value = "mixed")]
    cap: CliCapFilter,

    /// Rebalancing frequency
    #[arg(long, default_value = "monthly")]
    rebalance: CliRebalanceFreq,

    /// Weighting mode (role-weighted optimizer not yet implemented — falls back to equal)
    #[arg(long, default_value = "equal")]
    weight_mode: CliWeightMode,

    /// Roles to include (space-separated). Defaults to all 7.
    #[arg(long, num_args = 1.., value_delimiter = ' ')]
    roles: Option<Vec<CliRole>>,

    /// Benchmark ticker for alpha calculation
    #[arg(long, default_value = "^NSEI")]
    benchmark: String,

    /// One-way transaction cost in basis points, charged on turnover at every rebalance
    #[arg(long, default_value_t = 10.0)]
    cost_bps: f64,

    /// GICS industry codes to exclude (space-separated integers)
    #[arg(long, num_args = 0.., value_delimiter = ' ')]
    exclude_industries: Option<Vec<u32>>,

    /// Run Monte Carlo baseline comparison
    #[arg(long, default_value_t = true)]
    monte_carlo: bool,

    /// Run stock-picking engine and output ranked picks instead of simulation
    #[arg(long)]
    picks: bool,

    /// Run walk-forward signal validation (IC computation) instead of simulation
    #[arg(long)]
    validate: bool,

    /// Show industry correlation heatmap and divergence opportunities
    #[arg(long)]
    correlations: bool,

    /// End of training period for walk-forward validation (YYYY-MM-DD)
    #[arg(long, default_value = "2018-12-31")]
    train_end: String,

    /// Verbose logging (set RUST_LOG=debug for more)
    #[arg(short, long)]
    verbose: bool,

    // ── Phase 6: LLM strategy layer ──────────────────────────────────────────

    /// Natural-language strategy description — Gemini parses it into a StrategySpec,
    /// then runs picks with those settings.
    /// Example: --strategy "High momentum tech stocks, ignore financials, 20 day hold"
    #[arg(long)]
    strategy: Option<String>,

    /// Run a full historical backtest using the strategy from --strategy.
    /// Requires --strategy, optionally --from and --to.
    #[arg(long)]
    backtest: bool,

    /// Backtest / paper-init start date (YYYY-MM-DD, defaults to --start)
    #[arg(long)]
    from: Option<String>,

    /// Backtest end date (YYYY-MM-DD, defaults to --end)
    #[arg(long)]
    to: Option<String>,

    /// Initialise a new paper portfolio from --strategy and start tracking.
    #[arg(long)]
    paper_init: bool,

    /// Mark paper portfolio to market and rebalance if due.
    #[arg(long)]
    paper_update: bool,

    /// Print current paper portfolio status without rebalancing.
    #[arg(long)]
    paper_status: bool,

    // ── Phase 7: daily usability layer ───────────────────────────────────────

    /// Run the morning report: auto-universe → signals → picks + market context + portfolio.
    #[arg(long)]
    morning: bool,

    /// Run the evening mark-to-market report for all open paper positions.
    #[arg(long)]
    evening: bool,

    /// Verify the forward-test log's hash chain (detects edited or removed entries).
    #[arg(long)]
    forward_verify: bool,

    /// Score matured forward-test entries against the benchmark.
    #[arg(long)]
    forward_eval: bool,

    /// Holding horizon in trading days for --forward-eval.
    #[arg(long, default_value_t = 21)]
    horizon: usize,

    /// Restrict --morning / --backfill-days to the US market (S&P 500).
    #[arg(long)]
    us: bool,

    /// Replay the daily job over the last N calendar days, point-in-time, into a
    /// SEPARATE log (default backfill_log/). A backtest, not forward evidence.
    #[arg(long)]
    backfill_days: Option<i64>,

    /// Log directory for --forward-verify/--forward-eval/--backfill-days
    /// (default: $FORWARD_LOG_DIR or forward_log; backfill defaults to backfill_log).
    #[arg(long)]
    log_dir: Option<String>,

    // ── Decision journal ──────────────────────────────────────────────────────

    /// Log a new discretionary call: your own thesis, before the outcome is known.
    /// Requires --ticker, --thesis and --holding-days; --entry-price is fetched
    /// from the latest close if omitted.
    #[arg(long)]
    journal_add: bool,

    /// List journal entries (default: open ones). Combine with --status closed/all.
    #[arg(long)]
    journal_list: bool,

    /// Close a journal entry by id: --journal-close --id N [--exit-price P] [--notes "..."].
    /// --exit-price is fetched from the latest close if omitted.
    #[arg(long)]
    journal_close: bool,

    /// List open journal entries whose expected holding period has elapsed
    /// and print a summary of every closed entry so far (hit rate, mean
    /// return, and how you did when you agreed vs. disagreed with the model).
    #[arg(long)]
    journal_score: bool,

    #[arg(long)]
    ticker: Option<String>,
    #[arg(long)]
    thesis: Option<String>,
    #[arg(long)]
    holding_days: Option<u32>,
    #[arg(long)]
    entry_price: Option<f64>,
    #[arg(long)]
    exit_price: Option<f64>,
    #[arg(long)]
    notes: Option<String>,
    #[arg(long)]
    id: Option<i64>,
    #[arg(long, default_value = "open")]
    status: String,

    // ── Real holdings ─────────────────────────────────────────────────────────

    /// Import a broker trade history (CSV: date,ticker,side,quantity,price[,fees] —
    /// see src/holdings.rs for the schema) and report holdings, realised P&L
    /// and portfolio XIRR. Combine with --reconcile to compare against the
    /// most recent forward-log picks.
    #[arg(long)]
    holdings_import: Option<String>,

    /// With --holdings-import: also report which held tickers the model
    /// currently likes, doesn't, and which of its picks you don't hold.
    #[arg(long)]
    reconcile: bool,

    /// Diff the two most recent forward-log entries: picks added/removed,
    /// a regime flip, and the biggest composite-score movers.
    #[arg(long)]
    diff: bool,
}

// ---------------------------------------------------------------------------
// Conversion helpers (CLI mirror types → library types)
// ---------------------------------------------------------------------------

fn to_market(m: &CliMarket) -> quant_edge::universe::Market {
    use quant_edge::universe::Market;
    match m {
        CliMarket::Nse  => Market::NSE,
        CliMarket::Nyse => Market::NYSE,
        CliMarket::Both => Market::Both,
    }
}

fn to_cap_filter(c: &CliCapFilter) -> quant_edge::universe::CapFilter {
    use quant_edge::universe::CapFilter;
    match c {
        CliCapFilter::SmallCap => CapFilter::SmallCap,
        CliCapFilter::MidCap   => CapFilter::MidCap,
        CliCapFilter::LargeCap => CapFilter::LargeCap,
        CliCapFilter::Mixed    => CapFilter::Mixed,
    }
}

fn to_rebalance_freq(r: &CliRebalanceFreq) -> RebalanceFrequency {
    match r {
        CliRebalanceFreq::Daily     => RebalanceFrequency::Daily,
        CliRebalanceFreq::Weekly    => RebalanceFrequency::Weekly,
        CliRebalanceFreq::Monthly   => RebalanceFrequency::Monthly,
        CliRebalanceFreq::Quarterly => RebalanceFrequency::Quarterly,
        CliRebalanceFreq::Annual    => RebalanceFrequency::Annual,
    }
}

fn to_weight_mode(w: &CliWeightMode) -> WeightMode {
    match w {
        CliWeightMode::Equal        => WeightMode::Equal,
        // 1/7 per role, equal within role (no custom multipliers from CLI).
        CliWeightMode::RoleWeighted => WeightMode::RoleWeighted(std::collections::HashMap::new()),
    }
}

fn to_roles(roles: Option<&Vec<CliRole>>) -> Vec<Role> {
    match roles {
        None => Role::all(),
        Some(rs) => rs.iter().map(|r| match r {
            CliRole::FastestGrower    => Role::FastestGrower,
            CliRole::LargestByRevenue => Role::LargestByRevenue,
            CliRole::MostProfitable   => Role::MostProfitable,
            CliRole::MostLeveraged    => Role::MostLeveraged,
            CliRole::ConsumerReach    => Role::ConsumerReach,
            CliRole::DeepValue        => Role::DeepValue,
            CliRole::MomentumLeader   => Role::MomentumLeader,
        }).collect(),
    }
}

fn parse_date(s: &str, label: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .with_context(|| format!("Invalid {label} date: {s} — expected YYYY-MM-DD"))
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env before anything else so GEMINI_API_KEY and weight overrides are visible
    dotenvy::dotenv().ok();

    let cli = Cli::parse();

    // Logging
    let filter = if cli.verbose { "debug" } else { "quant_edge=info,warn" };
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(filter))
        .with_target(false)
        .compact()
        .init();

    // ── 1. Infrastructure ────────────────────────────────────────────────────
    let cache = Cache::open(&cli.cache)
        .with_context(|| format!("Failed to open cache at {}", cli.cache))?;

    let source = YahooFinance::new(cache.clone());

    let taxonomy = GicsTaxonomy::load(&cli.gics)
        .with_context(|| format!("Failed to load GICS taxonomy from {}", cli.gics))?;

    // ── 2. Parse dates ───────────────────────────────────────────────────────
    let start_date = parse_date(&cli.start, "start")?;
    let end_date   = parse_date(&cli.end, "end")?;

    anyhow::ensure!(start_date < end_date, "start date must be before end date");

    // ── Phase 6 paper-status: no universe needed ─────────────────────────────
    if cli.paper_status {
        let today = chrono::Local::now().date_naive();
        let engine = PaperTradingEngine::new(cache.clone());
        let portfolio = engine.status()?;
        print_portfolio_status(&portfolio, today);
        return Ok(());
    }

    // ── Diff: read-only, no universe needed ───────────────────────────────────
    if cli.diff {
        let log_dir = cli.log_dir.clone().unwrap_or_else(|| quant_edge::forward_test::DEFAULT_DIR.to_string());
        match quant_edge::forward_test::diff::diff_latest(std::path::Path::new(&log_dir))? {
            None => println!("Need at least two forward-log entries in {log_dir} to diff."),
            Some(dd) => {
                println!();
                println!("\x1b[1m\x1b[97mChanges: {} -> {}\x1b[0m", dd.prev_date, dd.curr_date);
                if let Some((prev, curr)) = dd.regime_changed {
                    let fmt = |on: bool| if on { "RISK-ON" } else { "RISK-OFF" };
                    println!("  Regime flipped: {} -> {}", fmt(prev), fmt(curr));
                }
                if dd.prev_universe_size != dd.curr_universe_size {
                    println!("  Universe size: {} -> {}", dd.prev_universe_size, dd.curr_universe_size);
                }
                println!();
                println!("  Picks added:   {}", if dd.picks_added.is_empty() { "none".into() } else { dd.picks_added.join(", ") });
                println!("  Picks removed: {}", if dd.picks_removed.is_empty() { "none".into() } else { dd.picks_removed.join(", ") });
                if !dd.biggest_movers.is_empty() {
                    println!();
                    println!("  Biggest movers:");
                    for m in dd.biggest_movers.iter().take(10) {
                        println!("    {:<10} {:>6.1} -> {:>6.1}  ({:+.1})", m.ticker, m.prev_composite, m.curr_composite, m.delta);
                    }
                }
            }
        }
        return Ok(());
    }

    // ── Real holdings: read-only, no universe needed ──────────────────────────
    if let Some(path) = &cli.holdings_import {
        use quant_edge::holdings::{compute_holdings, parse_trades_csv, portfolio_xirr, reconcile};
        let today = chrono::Local::now().date_naive();

        let text = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
        let trades = parse_trades_csv(&text).context("parsing the trade history")?;
        anyhow::ensure!(!trades.is_empty(), "no trades found in {path}");
        let holdings = compute_holdings(&trades);

        println!();
        println!("\x1b[1m\x1b[97mHoldings ({} trade(s) imported)\x1b[0m", trades.len());
        println!("{:<10} {:>12} {:>12} {:>12} {:>14} {:>14}", "Ticker", "Qty", "Avg Cost", "Last", "Unrealised", "Realised");

        let yahoo = YahooFinance::new(cache.clone());
        let mut current_prices = std::collections::HashMap::new();
        let mut tickers: Vec<&String> = holdings.keys().collect();
        tickers.sort();
        for ticker in &tickers {
            let h = &holdings[*ticker];
            let last = if h.quantity > 0.0 {
                match yahoo.price_history(ticker, today - chrono::Duration::days(10), today).await {
                    Ok(bars) if !bars.is_empty() => {
                        let px = bars.last().unwrap().close;
                        current_prices.insert((*ticker).clone(), px);
                        Some(px)
                    }
                    _ => None,
                }
            } else {
                None
            };
            let unrealized = last.map(|px| h.quantity * (px - h.avg_cost));
            println!(
                "{:<10} {:>12.2} {:>12.2} {:>12} {:>14} {:>14.2}",
                ticker, h.quantity, h.avg_cost,
                last.map(|p| format!("{p:.2}")).unwrap_or_else(|| "n/a".to_string()),
                unrealized.map(|u| format!("{u:+.2}")).unwrap_or_else(|| "n/a".to_string()),
                h.realized_pnl,
            );
        }

        match portfolio_xirr(&trades, &current_prices, today) {
            Ok(Some(rate)) => println!("\nPortfolio XIRR: {:+.2}%", rate * 100.0),
            Ok(None) => println!("\nPortfolio XIRR: not computable (need both an outflow and an inflow)"),
            Err(e) => println!("\nPortfolio XIRR: unavailable - {e:#}"),
        }

        if cli.reconcile {
            let log_dir = cli.log_dir.clone().unwrap_or_else(|| quant_edge::forward_test::DEFAULT_DIR.to_string());
            match quant_edge::forward_test::read_all(std::path::Path::new(&log_dir)) {
                Ok(entries) if !entries.is_empty() => {
                    let latest = entries.last().unwrap();
                    let model_picks: std::collections::HashSet<String> =
                        latest.picks.iter().map(|p| p.ticker.clone()).collect();
                    let r = reconcile(&holdings, &model_picks);
                    println!("\n\x1b[1mReconciliation vs. {} picks ({})\x1b[0m", latest.as_of, log_dir);
                    println!("  Held and the model likes:  {}", if r.in_both.is_empty() { "none".into() } else { r.in_both.join(", ") });
                    println!("  Held, model doesn't pick:  {}", if r.only_held.is_empty() { "none".into() } else { r.only_held.join(", ") });
                    println!("  Model likes, you don't hold: {}", if r.only_model.is_empty() { "none".into() } else { r.only_model.join(", ") });
                }
                _ => println!("\nReconciliation: no forward-log entries found in {log_dir} yet"),
            }
        }
        return Ok(());
    }

    // ── Decision journal: read/write, no universe needed ─────────────────────
    if cli.journal_add || cli.journal_list || cli.journal_close || cli.journal_score {
        use quant_edge::journal::{self, JournalEntry, NewDecision};
        let today = chrono::Local::now().date_naive();

        async fn last_close(cache: &Cache, ticker: &str, today: NaiveDate) -> Result<f64> {
            let yahoo = YahooFinance::new(cache.clone());
            let bars = yahoo.price_history(ticker, today - chrono::Duration::days(10), today).await?;
            bars.last().map(|b| b.close).context("no recent price data for this ticker")
        }

        if cli.journal_add {
            let ticker = cli.ticker.clone().context("--journal-add requires --ticker")?;
            let thesis = cli.thesis.clone().context("--journal-add requires --thesis \"...\"")?;
            let holding_days = cli.holding_days.context("--journal-add requires --holding-days")?;
            let entry_price = match cli.entry_price {
                Some(p) => p,
                None => last_close(&cache, &ticker, today).await?,
            };
            let log_dir = cli.log_dir.clone().unwrap_or_else(|| quant_edge::forward_test::DEFAULT_DIR.to_string());
            let model_composite_at_entry = journal::find_model_composite(std::path::Path::new(&log_dir), &ticker, today);

            let id = journal::add(&cache, &NewDecision {
                ticker: ticker.to_uppercase(), entry_date: today, thesis, expected_holding_days: holding_days,
                entry_price, model_composite_at_entry,
            })?;
            println!("Logged decision #{id}: {} @ {:.2} on {}", ticker.to_uppercase(), entry_price, today);
            if let Some(c) = model_composite_at_entry {
                println!("  (model composite around this date: {c:.1})");
            } else {
                println!("  (no model score found for this ticker/date in {log_dir} - that's fine, it's optional)");
            }
        }

        if cli.journal_close {
            let id = cli.id.context("--journal-close requires --id N")?;
            let entries = journal::list(&cache, None)?;
            let entry = entries.iter().find(|e| e.id == id).with_context(|| format!("no journal entry #{id}"))?;
            let exit_price = match cli.exit_price {
                Some(p) => p,
                None => last_close(&cache, &entry.ticker, today).await?,
            };
            journal::close(&cache, id, today, exit_price, cli.notes.as_deref())?;
            let ret = (exit_price / entry.entry_price - 1.0) * 100.0;
            println!("Closed #{id}: {} @ {:.2} ({:+.1}% from {:.2} on {})", entry.ticker, exit_price, ret, entry.entry_price, entry.entry_date);
        }

        if cli.journal_score {
            let all = journal::list(&cache, None)?;
            let due: Vec<&JournalEntry> = all.iter().filter(|e| e.is_due(today)).collect();
            if due.is_empty() {
                println!("No open entries are past their expected holding period.");
            } else {
                println!("Due for review ({} entries):", due.len());
                for e in &due {
                    println!("  #{:<4} {:<8} entered {} ({} days ago), thesis: {}", e.id, e.ticker, e.entry_date, e.days_held(today), e.thesis);
                }
            }
            let summary = journal::summarize(&all);
            if summary.n_closed > 0 {
                println!();
                println!("Closed entries so far: {}", summary.n_closed);
                println!("  Mean return       {:>+7.2}%", summary.mean_return_pct);
                println!("  Hit rate          {:>7.1}%", summary.hit_rate_pct);
                if summary.n_agreed_with_model + summary.n_disagreed_with_model > 0 {
                    println!("  Agreed w/ model   {:>4} calls, mean return {:>+.2}%", summary.n_agreed_with_model, summary.mean_return_when_agreed_pct);
                    println!("  Disagreed         {:>4} calls, mean return {:>+.2}%", summary.n_disagreed_with_model, summary.mean_return_when_disagreed_pct);
                }
            }
        }

        if cli.journal_list {
            let status = match cli.status.as_str() {
                "all" => None,
                s => Some(s.to_string()),
            };
            let entries = journal::list(&cache, status.as_deref())?;
            if entries.is_empty() {
                println!("No journal entries.");
            }
            for e in &entries {
                match e.return_pct() {
                    Some(r) => println!("#{:<4} {:<8} {} -> {}  {:+.1}%   {}", e.id, e.ticker, e.entry_date, e.exit_date.unwrap(), r, e.thesis),
                    None => println!("#{:<4} {:<8} {} (open, {} days)   {}", e.id, e.ticker, e.entry_date, e.days_held(today), e.thesis),
                }
            }
        }
        return Ok(());
    }

    // ── Forward-test log: read-only, no universe needed ──────────────────────
    if cli.forward_verify || cli.forward_eval {
        let dir = cli.log_dir.clone().unwrap_or_else(|| {
            std::env::var("FORWARD_LOG_DIR")
                .unwrap_or_else(|_| quant_edge::forward_test::DEFAULT_DIR.to_string())
        });
        let dir = std::path::Path::new(&dir);

        let v = quant_edge::forward_test::verify(dir)?;
        match &v.first_error {
            None => println!("Forward-test log: {} entries, hash chain intact.", v.n_entries),
            Some(e) => {
                println!("Forward-test log INTEGRITY FAILURE: {e}");
                anyhow::bail!("forward-test log failed verification");
            }
        }

        if cli.forward_eval {
            let (outcomes, sum, pending) =
                quant_edge::forward_test::evaluate(dir, &cache, cli.horizon).await?;
            println!();
            println!("Forward-test results ({}-day horizon, next-open entry)", cli.horizon);
            println!("  Matured entries      {:>8}   (pending: {})", sum.n, pending);
            if sum.n == 0 {
                println!("  Nothing has matured yet - check back after {} trading days.", cli.horizon);
            } else {
                println!("  Mean basket return   {:>+7.2}%", sum.mean_basket * 100.0);
                println!("  Mean excess vs index {:>+7.2}%   (t = {:+.2})", sum.mean_excess * 100.0, sum.excess_t_stat);
                println!("  Beat the index       {:>7.0}%   of entries", sum.hit_rate * 100.0);
                println!("  Recommended cash     {:>8}   entries", sum.cash_entries);
                if sum.ic_n > 0 {
                    println!(
                        "  Whole-ranking rank IC {:>+6.4}   (t = {:+.2}, {} entries; overlapping windows inflate t)",
                        sum.mean_ic, sum.ic_t_stat, sum.ic_n
                    );
                }
                if sum.n < 30 {
                    println!("  (fewer than 30 matured entries: too few to conclude anything yet)");
                }
                println!();
                for o in outcomes.iter().rev().take(10).rev() {
                    println!(
                        "  {}  {:>2} picks  basket {:>+6.2}%  index {}  excess {}",
                        o.as_of,
                        o.n_picks,
                        o.basket_return * 100.0,
                        o.benchmark_return.map_or("   n/a".to_string(), |b| format!("{:>+6.2}%", b * 100.0)),
                        o.excess.map_or("   n/a".to_string(), |x| format!("{:>+6.2}%", x * 100.0)),
                    );
                }
            }
        }
        return Ok(());
    }

    // ── Phase 7 morning report: builds auto-universe internally ──────────────
    if cli.morning {
        let today = chrono::Local::now().date_naive();
        let output = run_morning(cache, &taxonomy, today, cli.us)
            .await
            .context("Morning report failed")?;
        print!("{}", output);
        return Ok(());
    }

    // ── Backfill: point-in-time replay of the daily job (a backtest) ─────────
    if let Some(days) = cli.backfill_days {
        let last_day = chrono::Local::now().date_naive() - chrono::Duration::days(1);
        let dir = cli.log_dir.clone().unwrap_or_else(|| quant_edge::daily::backfill::BACKFILL_DIR.to_string());
        let msg = quant_edge::daily::backfill::run_backfill(
            cache, &taxonomy, last_day, days, cli.us, std::path::Path::new(&dir),
        )
        .await
        .context("Backfill failed")?;
        println!("{msg}");
        println!("Evaluate with: quant-edge --forward-eval --log-dir {dir} --horizon 5");
        return Ok(());
    }

    // ── Phase 7 evening report: read-only mark-to-market ─────────────────────
    if cli.evening {
        let today = chrono::Local::now().date_naive();
        let output = run_evening(cache, today)
            .await
            .context("Evening report failed")?;
        print!("{}", output);
        return Ok(());
    }

    // ── 3. Build universe ────────────────────────────────────────────────────
    let exclude_industries = cli.exclude_industries.clone().unwrap_or_default();
    let universe_config = UniverseConfig {
        market:                 to_market(&cli.market),
        cap_filter:             to_cap_filter(&cli.cap),
        n_industries:           cli.n_industries,
        exclude_industry_codes: exclude_industries.clone(),
    };

    let builder = UniverseBuilder::new(&source, &taxonomy);

    tracing::info!("Building universe from {}", cli.tickers);
    let mut universe = builder
        .build_from_file(&cli.tickers, universe_config)
        .await
        .context("Universe build failed")?;

    tracing::info!("Enriching GICS classifications via Yahoo Finance...");
    builder.enrich_gics(&mut universe).await
        .context("GICS enrichment failed")?;

    // Trim to N most-populated industries after enrichment (correct order matters:
    // enrichment assigns real codes, then we select which industries to keep)
    universe.trim_to_n_industries(cli.n_industries, &exclude_industries);

    tracing::info!(
        "Universe ready — {} companies across {} industries populated",
        universe.total_companies(),
        universe.populated_industry_count(),
    );

    // ── Mode: --strategy (Gemini → StrategySpec → picks) ─────────────────────
    if let Some(ref strategy_text) = cli.strategy {
        if !cli.backtest && !cli.paper_init {
            tracing::info!("Calling Gemini to parse strategy...");
            let spec = parse_strategy(strategy_text)
                .await
                .context("Gemini strategy parsing failed")?;

            println!();
            println!("\x1b[1m\x1b[97mParsed StrategySpec\x1b[0m");
            println!("{}", serde_json::to_string_pretty(&spec).unwrap_or_default());
            println!();

            // Run picks with spec weights
            let mut engine = PickingEngine::new(cache.clone());
            engine.apply_strategy_spec(&spec);

            tracing::info!("Running picks with strategy '{}' as of {}...", spec.name, end_date);
            let scores = engine
                .rank_universe(&universe, end_date)
                .await
                .context("Picking engine failed")?;

            // Apply top_n and min_score from spec
            let mut filtered: Vec<_> = scores
                .iter()
                .filter(|s| spec.filters.min_score.map(|m| s.composite >= m).unwrap_or(true))
                .cloned()
                .collect();
            filtered.truncate(spec.top_n);

            let macro_snap = quant_edge::data::fred::FredFetcher::new(cache.clone())
                .macro_snapshot(end_date)
                .await;

            let no_warnings = std::collections::HashMap::new();
            CliReporter::picks_report(&filtered, &macro_snap, &end_date.to_string(), &no_warnings);
            return Ok(());
        }
    }

    // ── Mode: --backtest ──────────────────────────────────────────────────────
    if cli.backtest {
        let strategy_text = cli.strategy.as_deref()
            .context("--backtest requires --strategy \"...\"")?;

        tracing::info!("Calling Gemini to parse strategy...");
        let spec = parse_strategy(strategy_text)
            .await
            .context("Gemini strategy parsing failed")?;

        println!();
        println!("\x1b[1m\x1b[97mParsed StrategySpec\x1b[0m");
        println!("{}", serde_json::to_string_pretty(&spec).unwrap_or_default());
        println!();

        let from = cli.from.as_deref().unwrap_or(&cli.start);
        let to   = cli.to.as_deref().unwrap_or(&cli.end);
        let from_date = parse_date(from, "from")?;
        let to_date   = parse_date(to, "to")?;

        let config = BacktestConfig::new(spec.clone(), from_date, to_date);
        let engine = BacktestEngine::new(cache.clone());

        let result = engine
            .run(&universe, &config)
            .await
            .context("Backtest failed")?;

        print_backtest_report(&result, &spec.name);
        return Ok(());
    }

    // ── Mode: --paper-init ────────────────────────────────────────────────────
    if cli.paper_init {
        let strategy_text = cli.strategy.as_deref()
            .context("--paper-init requires --strategy \"...\"")?;

        tracing::info!("Calling Gemini to parse strategy...");
        let spec = parse_strategy(strategy_text)
            .await
            .context("Gemini strategy parsing failed")?;

        println!();
        println!("\x1b[1m\x1b[97mParsed StrategySpec\x1b[0m");
        println!("{}", serde_json::to_string_pretty(&spec).unwrap_or_default());

        let today = chrono::Local::now().date_naive();
        let engine = PaperTradingEngine::new(cache.clone());
        let portfolio = engine
            .init_portfolio(spec, &universe, today, cli.capital)
            .await
            .context("Paper portfolio init failed")?;

        print_portfolio_status(&portfolio, today);
        return Ok(());
    }

    // ── Mode: --paper-update ──────────────────────────────────────────────────
    if cli.paper_update {
        let today = chrono::Local::now().date_naive();
        let engine = PaperTradingEngine::new(cache.clone());
        let portfolio = engine
            .update_or_init_portfolio(&universe, today, cli.capital)
            .await
            .context("Paper portfolio update failed")?;
        print_portfolio_status(&portfolio, today);
        return Ok(());
    }

    // ── Mode: stock picks ────────────────────────────────────────────────────
    if cli.picks {
        let engine = PickingEngine::new(cache.clone());
        tracing::info!("Running picking engine as of {}...", end_date);
        let scores = engine
            .rank_universe(&universe, end_date)
            .await
            .context("Picking engine failed")?;

        let industry_tickers = universe.industry_ticker_map();
        let warnings = CorrelationEngine::new(cache.clone())
            .load_or_compute(&industry_tickers, end_date)
            .map(|corr_data| ConcentrationGuard::check(&scores, &corr_data))
            .unwrap_or_default();

        let macro_snap = quant_edge::data::fred::FredFetcher::new(cache.clone())
            .macro_snapshot(end_date)
            .await;

        CliReporter::picks_report(&scores, &macro_snap, &end_date.to_string(), &warnings);
        return Ok(());
    }

    // ── Mode: walk-forward signal validation ─────────────────────────────────
    if cli.validate {
        let train_end  = parse_date(&cli.train_end, "train_end")?;
        let test_start = train_end + chrono::Duration::days(1);

        let validator = WalkForwardValidator::new(cache.clone());
        tracing::info!(
            "Running walk-forward validation: train ≤{}, test {} → {}",
            train_end, test_start, end_date
        );

        let report = validator
            .validate(&universe, train_end, test_start, end_date, cli.monte_carlo)
            .await
            .context("Validation failed")?;

        println!();
        println!("\x1b[1m\x1b[97mSIGNAL VALIDATION REPORT\x1b[0m");
        println!();
        for ic in &report.signal_ic {
            let verdict = if !ic.evaluable {
                "\x1b[33m? not evaluable\x1b[0m"
            } else if ic.has_edge {
                "\x1b[32m✓ has edge\x1b[0m"
            } else {
                "\x1b[31m✗ no significant edge\x1b[0m"
            };
            println!(
                "  {:<12}  rank IC={:>+.4}  σ={:.4}  t={:>+.2}  hit={:>3.0}%  n={:<3} {}",
                ic.signal_name, ic.ic_mean, ic.ic_std, ic.t_stat, ic.hit_rate * 100.0, ic.n_dates, verdict
            );
            if let Some(note) = &ic.note {
                println!("      \x1b[2m{}\x1b[0m", note);
            }
        }
        for note in &report.notes {
            println!("  \x1b[2m• {}\x1b[0m", note);
        }
        if let Some(mc) = &report.monte_carlo {
            println!();
            println!(
                "  Strategy annualised: {:>+.2}%   Random median: {:>+.2}%   Beats {:.1}% of random portfolios",
                mc.strategy_annualised_return * 100.0,
                mc.random_median_return * 100.0,
                mc.percentile_rank,
            );
        }
        println!();

        let json_path = "signal_validation_report.json";
        std::fs::write(json_path, serde_json::to_string_pretty(&report).unwrap_or_default())
            .with_context(|| format!("Failed to write {}", json_path))?;
        println!("Validation report written → {}", json_path);
        return Ok(());
    }

    // ── Mode: correlation report ─────────────────────────────────────────────
    if cli.correlations {
        let industry_tickers = universe.industry_ticker_map();
        let corr_data = CorrelationEngine::new(cache.clone())
            .load_or_compute(&industry_tickers, end_date)
            .context("Correlation engine failed")?;
        CliReporter::correlations_report(&corr_data);
        return Ok(());
    }

    // ── Mode: portfolio simulation (default) ─────────────────────────────────

    let active_roles = to_roles(cli.roles.as_ref());
    tracing::info!("Active roles: {:?}", active_roles);

    let classifier = RoleClassifier::new(&source, active_roles.clone());
    let _initial_roster = classifier
        .classify_universe(&universe, start_date)
        .await
        .context("Initial role classification failed")?;

    let sim_config = SimulationConfig {
        start_date,
        end_date,
        initial_capital:  cli.capital,
        rebalance_freq:   to_rebalance_freq(&cli.rebalance),
        weight_mode:      to_weight_mode(&cli.weight_mode),
        active_roles,
        benchmark_ticker: cli.benchmark.clone(),
        transaction_cost_bps: cli.cost_bps,
    };

    let engine = SimulationEngine::new(&source, sim_config);

    tracing::info!(
        "Running simulation {} → {} ({:?} rebalancing)...",
        cli.start, cli.end, cli.rebalance
    );

    let result = engine.run(&universe).await
        .context("Simulation failed")?;

    let metrics = compute_metrics(&result, cli.monte_carlo);
    CliReporter::print(&result, &metrics);

    Ok(())
}
