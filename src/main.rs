use anyhow::{Context, Result};
use chrono::NaiveDate;
use clap::{Parser, ValueEnum};
use tracing_subscriber::EnvFilter;

use quant_edge::{
    backtest::{BacktestConfig, BacktestEngine, print_backtest_report},
    correlations::{ConcentrationGuard, CorrelationEngine},
    daily::{run_morning, run_evening},
    data::{yahoo::YahooFinance, cache::Cache},
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

    // ── Forward-test log: read-only, no universe needed ──────────────────────
    if cli.forward_verify || cli.forward_eval {
        let dir = std::env::var("FORWARD_LOG_DIR")
            .unwrap_or_else(|_| quant_edge::forward_test::DEFAULT_DIR.to_string());
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
        let output = run_morning(cache, &taxonomy, today)
            .await
            .context("Morning report failed")?;
        print!("{}", output);
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
