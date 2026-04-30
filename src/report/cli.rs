use std::collections::HashMap;
use chrono::Datelike;

use crate::correlations::{concentration_guard::CorrelationWarning, CorrelationData, print_correlation_report};
use crate::metrics::stats::{MetricsReport, DrawdownPeriod, MonteCarloResult};
use crate::portfolio::engine::{SimulationResult, RolePerformance, IndustryPerformance};
use crate::portfolio::rebalancer::SwapEvent;
use crate::roles::classifier::Role;
use crate::signals::{SignalScore, macro_filter::macro_regime_description};
use crate::data::MacroSnapshot;
use std::sync::Arc;

// ── Colour constants (ANSI) ───────────────────────────────────────────────────

const RESET:  &str = "\x1b[0m";
const BOLD:   &str = "\x1b[1m";
const DIM:    &str = "\x1b[2m";
const GREEN:  &str = "\x1b[32m";
const RED:    &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const CYAN:   &str = "\x1b[36m";
const WHITE:  &str = "\x1b[97m";

// ── Reporter ──────────────────────────────────────────────────────────────────

pub struct CliReporter;

impl CliReporter {
    /// Master entry point — prints the full simulation report to stdout.
    pub fn print(result: &SimulationResult, metrics: &MetricsReport) {
        Self::print_header(result);
        Self::print_summary(metrics);
        Self::print_role_performance(&result.role_performance);
        Self::print_industry_performance(&result.industry_perf);
        Self::print_drawdowns(&metrics.drawdown_periods);
        Self::print_swap_log(&result.swap_log);
        if let Some(mc) = &metrics.monte_carlo {
            Self::print_monte_carlo(mc, metrics.total_return);
        }
        Self::print_footer();
    }

    // ── Header ────────────────────────────────────────────────────────────────

    fn print_header(result: &SimulationResult) {
        let cfg = &result.config;
        println!();
        println!("{BOLD}{WHITE}╔══════════════════════════════════════════════════════════╗{RESET}");
        println!("{BOLD}{WHITE}║          PORTFOLIO SIMULATOR — SIMULATION REPORT         ║{RESET}");
        println!("{BOLD}{WHITE}╚══════════════════════════════════════════════════════════╝{RESET}");
        println!();
        println!("{BOLD}Period      {RESET}{} → {}", cfg.start_date, cfg.end_date);
        println!("{BOLD}Capital     {RESET}${:.2}", cfg.initial_capital);
        println!("{BOLD}Rebalance   {RESET}{:?}", cfg.rebalance_freq);
        println!("{BOLD}Roles       {RESET}{}", cfg.active_roles.iter().map(|r| r.label()).collect::<Vec<_>>().join(", "));
        println!("{BOLD}Benchmark   {RESET}{}", cfg.benchmark_ticker);
        println!("{DIM}────────────────────────────────────────────────────────────{RESET}");
    }

    // ── Summary table ─────────────────────────────────────────────────────────

    fn print_summary(m: &MetricsReport) {
        println!();
        println!("{BOLD}{CYAN}  PERFORMANCE SUMMARY{RESET}");
        println!();

        let ret_colour   = colour_for(m.total_return);
        let alpha_colour = colour_for(m.alpha);
        let dd_colour    = if m.max_drawdown < -0.20 { RED } else { YELLOW };

        println!(
            "  {BOLD}Total Return       {RESET}{ret_colour}{:>+8.2}%{RESET}      \
             {BOLD}Benchmark Return   {RESET}{:>+8.2}%",
            m.total_return * 100.0,
            m.benchmark_return * 100.0,
        );
        println!(
            "  {BOLD}Annualised Return  {RESET}{ret_colour}{:>+8.2}%{RESET}      \
             {BOLD}Alpha (total)      {RESET}{alpha_colour}{:>+8.2}%{RESET}",
            m.annualised_return * 100.0,
            m.alpha * 100.0,
        );
        println!(
            "  {BOLD}Sharpe Ratio       {RESET}{:>8.2}           \
             {BOLD}Sortino Ratio      {RESET}{:>8.2}",
            m.sharpe_ratio,
            m.sortino_ratio,
        );
        println!(
            "  {BOLD}Max Drawdown       {RESET}{dd_colour}{:>+8.2}%{RESET}      \
             {BOLD}Volatility (ann.)  {RESET}{:>8.2}%",
            m.max_drawdown * 100.0,
            m.volatility * 100.0,
        );
        println!(
            "  {BOLD}Calmar Ratio       {RESET}{:>8.2}           \
             {BOLD}Annualised Alpha   {RESET}{alpha_colour}{:>+8.2}%{RESET}",
            m.calmar_ratio,
            m.annualised_alpha * 100.0,
        );
        println!(
            "  {BOLD}Trading Days       {RESET}{:>8}           \
             {BOLD}Total Swaps        {RESET}{:>8}",
            m.trading_days,
            m.total_swaps,
        );

        println!();
        println!("{DIM}────────────────────────────────────────────────────────────{RESET}");
    }

    // ── Role performance ──────────────────────────────────────────────────────

    fn print_role_performance(roles: &[RolePerformance]) {
        println!();
        println!("{BOLD}{CYAN}  PERFORMANCE BY ROLE{RESET}");
        println!();
        println!(
            "  {BOLD}{:<20}  {:>10}  {:>10}  {:>8}{RESET}",
            "Role", "Return", "Avg Weight", "Swaps"
        );
        println!("  {DIM}{}{RESET}", "─".repeat(54));

        let mut sorted = roles.to_vec();
        sorted.sort_by(|a, b| b.total_return.partial_cmp(&a.total_return).unwrap());

        for rp in &sorted {
            let c = colour_for(rp.total_return);
            println!(
                "  {:<20}  {c}{:>+9.2}%{RESET}  {:>9.1}%  {:>8}",
                rp.role.label(),
                rp.total_return * 100.0,
                rp.avg_weight * 100.0,
                rp.swap_count,
            );
        }

        println!();
        println!("{DIM}────────────────────────────────────────────────────────────{RESET}");
    }

    // ── Industry performance ──────────────────────────────────────────────────

    fn print_industry_performance(industries: &[IndustryPerformance]) {
        println!();
        println!("{BOLD}{CYAN}  PERFORMANCE BY INDUSTRY  (top 10 / bottom 5){RESET}");
        println!();
        println!(
            "  {BOLD}{:<40}  {:>10}{RESET}",
            "Industry", "Return"
        );
        println!("  {DIM}{}{RESET}", "─".repeat(54));

        let top_n    = industries.iter().take(10);
        let bottom_n = industries.iter().rev().take(5).collect::<Vec<_>>().into_iter().rev();

        for ip in top_n {
            let c = colour_for(ip.total_return);
            println!(
                "  {:<40}  {c}{:>+9.2}%{RESET}",
                truncate(&ip.industry_name, 40),
                ip.total_return * 100.0,
            );
        }

        println!("  {DIM}  ...{RESET}");

        for ip in bottom_n {
            let c = colour_for(ip.total_return);
            println!(
                "  {:<40}  {c}{:>+9.2}%{RESET}",
                truncate(&ip.industry_name, 40),
                ip.total_return * 100.0,
            );
        }

        println!();
        println!("{DIM}────────────────────────────────────────────────────────────{RESET}");
    }

    // ── Drawdown periods ──────────────────────────────────────────────────────

    fn print_drawdowns(periods: &[DrawdownPeriod]) {
        println!();
        println!("{BOLD}{CYAN}  DRAWDOWN PERIODS  (worst 5){RESET}");
        println!();
        println!(
            "  {BOLD}{:<12}  {:<12}  {:>10}  {:>12}  {:<14}{RESET}",
            "Peak", "Trough", "Drawdown", "Recovery", "Status"
        );
        println!("  {DIM}{}{RESET}", "─".repeat(66));

        for period in periods.iter().take(5) {
            let recovery_str = match period.recovery_days {
                Some(d) => format!("{d:>9}d"),
                None    => format!("{:>10}", "—"),
            };
            let status = match period.recovery_date {
                Some(_) => format!("{GREEN}Recovered{RESET}"),
                None    => format!("{RED}Open{RESET}"),
            };
            println!(
                "  {:<12}  {:<12}  {RED}{:>+9.2}%{RESET}  {:<12}  {status}",
                period.peak_date,
                period.trough_date,
                period.drawdown_pct * 100.0,
                recovery_str,
            );
        }

        println!();
        println!("{DIM}────────────────────────────────────────────────────────────{RESET}");
    }

    // ── Swap log ──────────────────────────────────────────────────────────────

    fn print_swap_log(swaps: &[SwapEvent]) {
        println!();
        println!("{BOLD}{CYAN}  SWAP LOG  ({} total){RESET}", swaps.len());

        if swaps.is_empty() {
            println!("  {DIM}No swaps during simulation period.{RESET}");
            println!();
            println!("{DIM}────────────────────────────────────────────────────────────{RESET}");
            return;
        }

        println!();
        println!(
            "  {BOLD}{:<12}  {:<26}  {:<16}  {:<10}  {:<10}{RESET}",
            "Date", "Industry", "Role", "Out", "In"
        );
        println!("  {DIM}{}{RESET}", "─".repeat(80));

        // Show last 20 swaps (most recent)
        let display_swaps: Vec<&SwapEvent> = swaps.iter().rev().take(20).collect();
        for swap in display_swaps.into_iter().rev() {
            println!(
                "  {:<12}  {:<26}  {:<16}  {YELLOW}{:<10}{RESET}  {GREEN}{:<10}{RESET}",
                swap.date,
                truncate(&swap.industry_name, 26),
                swap.role.label(),
                truncate(&swap.outgoing, 10),
                truncate(&swap.incoming, 10),
            );
        }

        if swaps.len() > 20 {
            println!(
                "  {DIM}  … and {} earlier swaps{RESET}",
                swaps.len() - 20
            );
        }

        // Swap frequency by role
        println!();
        println!("  {BOLD}Swaps by role:{RESET}");
        let mut by_role: HashMap<String, usize> = HashMap::new();
        for swap in swaps {
            *by_role.entry(swap.role.label().to_string()).or_insert(0) += 1;
        }
        let mut role_counts: Vec<(String, usize)> = by_role.into_iter().collect();
        role_counts.sort_by(|a, b| b.1.cmp(&a.1));
        for (label, count) in &role_counts {
            let bar = "█".repeat((*count).min(40));
            println!("  {DIM}{:<20}{RESET}  {CYAN}{}{RESET}  {}", label, bar, count);
        }

        println!();
        println!("{DIM}────────────────────────────────────────────────────────────{RESET}");
    }

    // ── Monte Carlo ───────────────────────────────────────────────────────────

    fn print_monte_carlo(mc: &MonteCarloResult, strategy_return: f64) {
        println!();
        println!("{BOLD}{CYAN}  MONTE CARLO BASELINE  ({} random portfolios){RESET}", mc.n_simulations);
        println!();

        let beat_colour = if mc.beat_strategy_pct < 25.0 { GREEN }
                          else if mc.beat_strategy_pct < 50.0 { YELLOW }
                          else { RED };

        println!(
            "  {BOLD}Our strategy return   {RESET}{:>+8.2}%",
            strategy_return * 100.0
        );
        println!(
            "  {BOLD}Random median return  {RESET}{:>+8.2}%",
            mc.median_return * 100.0
        );
        println!(
            "  {BOLD}Random p5  / p95      {RESET}{:>+8.2}%  /  {:>+6.2}%",
            mc.percentile_5 * 100.0,
            mc.percentile_95 * 100.0,
        );
        println!(
            "  {BOLD}Random beat strategy  {RESET}{beat_colour}{:>7.1}% of runs{RESET}",
            mc.beat_strategy_pct
        );

        // Visual distribution bar
        println!();
        Self::print_distribution_bar(mc, strategy_return);

        println!();
        println!("{DIM}────────────────────────────────────────────────────────────{RESET}");
    }

    /// ASCII distribution bar showing where our strategy sits vs random.
    fn print_distribution_bar(mc: &MonteCarloResult, strategy_return: f64) {
        let width   = 50usize;
        let lo      = mc.percentile_5.min(strategy_return) - 0.02;
        let hi      = mc.percentile_95.max(strategy_return) + 0.02;
        let range   = hi - lo;

        let pos_for = |v: f64| -> usize {
            ((v - lo) / range * width as f64).round().clamp(0.0, width as f64) as usize
        };

        let p5_pos  = pos_for(mc.percentile_5);
        let med_pos = pos_for(mc.median_return);
        let p95_pos = pos_for(mc.percentile_95);
        let str_pos = pos_for(strategy_return);

        let mut bar: Vec<char> = vec![' '; width + 1];

        // Fill p5→p95 range with dim blocks
        for i in p5_pos..=p95_pos.min(width) {
            bar[i] = '░';
        }
        // Median marker
        if med_pos <= width { bar[med_pos] = '│'; }
        // Strategy marker (overrides)
        if str_pos <= width { bar[str_pos] = '▲'; }

        let bar_str: String = bar.into_iter().collect();

        println!("  {DIM}p5{RESET}                                               {DIM}p95{RESET}");
        println!("  {CYAN}{bar_str}{RESET}");
        println!("  {DIM}{lo:>+.0}%{}{hi:>+.0}%{RESET}",
            " ".repeat(width.saturating_sub(6)),
        );
        println!("  {DIM}│ = median random   ▲ = our strategy{RESET}");
    }

    // ── Footer ────────────────────────────────────────────────────────────────

    fn print_footer() {
        println!();
        println!("{DIM}Generated by portfolio-sim  •  not financial advice{RESET}");
        println!();
    }

    // ── Picks report ──────────────────────────────────────────────────────────

    /// Print the ranked stock picks table to stdout and write picks JSON to disk.
    /// `warnings` is ticker → concentration warning (may be empty).
    pub fn picks_report(
        scores: &[SignalScore],
        macro_snapshot: &MacroSnapshot,
        as_of: &str,
        warnings: &HashMap<String, CorrelationWarning>,
    ) {
        let n = scores.len();

        println!();
        println!("{BOLD}{WHITE}╔══════════════════════════════════════════════════════════════════╗{RESET}");
        println!("{BOLD}{WHITE}║              STOCK PICKS ENGINE — RANKED OUTPUT                  ║{RESET}");
        println!("{BOLD}{WHITE}╚══════════════════════════════════════════════════════════════════╝{RESET}");
        println!();
        println!(
            "{BOLD}Date        {RESET}{}    {BOLD}Universe    {RESET}{} stocks",
            as_of, n
        );
        println!(
            "{BOLD}Macro       {RESET}{}",
            macro_regime_description(macro_snapshot)
        );
        println!("{DIM}────────────────────────────────────────────────────────────────────{RESET}");
        println!();

        // Table header — added Pairs and Warn columns
        println!(
            "{BOLD}  {:<4}  {:<8}  {:>6}  {:>7}  {:>8}  {:>6}  {:>7}  {:>5}  {:<30}  {}{RESET}",
            "Rank", "Ticker", "Score", "Momentm", "Fundmntl", "Insdr", "Sentmt", "Pairs",
            "Industry", "Warn"
        );
        println!("  {DIM}{}{RESET}", "─".repeat(96));

        // Show top 25 picks
        for s in scores.iter().take(25) {
            let score_colour = if s.composite >= 65.0 {
                GREEN
            } else if s.composite >= 45.0 {
                YELLOW
            } else {
                RED
            };

            let macro_flag = if !s.macro_on { format!("{RED}✗{RESET}") } else { String::new() };

            let warn_col = if let Some(w) = warnings.get(&s.ticker) {
                format!("{YELLOW}⚠ HIGH CORRELATION WITH {}{RESET}", w.correlated_with)
            } else {
                String::new()
            };

            println!(
                "  {:<4}  {:<8}  {score_colour}{:>5.0}{RESET}{macro_flag:1}  {:>+6.0}  {:>+7.0}  {:>+5.0}  {:>+6.0}  {:>+4.0}  {:<30}  {}",
                s.rank,
                s.ticker,
                s.composite,
                s.momentum_contrib - 50.0,
                s.fundamental_contrib - 50.0,
                s.insider_contrib - 50.0,
                s.sentiment_contrib - 50.0,
                s.pairs_contrib - 50.0,
                truncate(&s.industry, 30),
                warn_col,
            );
        }

        if scores.len() > 25 {
            println!("  {DIM}  … and {} more stocks{RESET}", scores.len() - 25);
        }

        println!();
        println!("{DIM}  Score range: 0 (strongest sell) → 100 (strongest buy) | neutral = 50{RESET}");
        println!("{DIM}  Signal columns show deviation from neutral (positive = bullish){RESET}");
        if !warnings.is_empty() {
            println!("{DIM}  {YELLOW}⚠{RESET}{DIM} = industry correlation r > 0.70 with a higher-ranked pick{RESET}");
        }
        println!();

        // ── Per-signal breakdown for top 5 ───────────────────────────────────
        println!("{BOLD}{CYAN}  SIGNAL BREAKDOWN — TOP 5{RESET}");
        println!();

        for s in scores.iter().take(5) {
            let score_col = if s.composite >= 65.0 { GREEN } else { YELLOW };
            println!(
                "  {BOLD}{}{:<6}{RESET}  Score: {score_col}{:.0}/100{RESET}  │  {}",
                WHITE, s.ticker, s.composite, s.industry
            );
            println!(
                "    Momentum:      {:>+6.1}  (raw: {:+.3})",
                s.momentum_contrib - 50.0, s.momentum_raw
            );
            println!(
                "    Fundamental:   {:>+6.1}  (raw: {:+.3})",
                s.fundamental_contrib - 50.0, s.fundamental_raw
            );
            println!(
                "    Insider:       {:>+6.1}  (raw: {:+.3})",
                s.insider_contrib - 50.0, s.insider_raw
            );
            println!(
                "    Sentiment:     {:>+6.1}  (raw: {:+.3})",
                s.sentiment_contrib - 50.0, s.sentiment_raw
            );
            println!(
                "    Pairs:         {:>+6.1}  (raw: {:+.3})",
                s.pairs_contrib - 50.0, s.pairs_raw
            );
            println!(
                "    Macro filter:  {}",
                if s.macro_on { format!("{GREEN}ON{RESET}") } else { format!("{RED}OFF{RESET}") }
            );
            println!();
        }

        println!("{DIM}────────────────────────────────────────────────────────────────────{RESET}");
        println!();
        println!("{DIM}Generated by portfolio-sim  •  not financial advice{RESET}");
        println!();

        // ── Write JSON ────────────────────────────────────────────────────────
        let filename = format!("picks_{}.json", as_of.replace('-', ""));
        let json = serde_json::json!({
            "as_of": as_of,
            "universe_size": n,
            "macro": {
                "on": macro_snapshot.macro_on,
                "vix": macro_snapshot.vix,
                "yield_10y": macro_snapshot.yield_10y,
            },
            "picks": scores.iter().take(50).map(|s| serde_json::json!({
                "rank": s.rank,
                "ticker": s.ticker,
                "industry": s.industry,
                "composite": s.composite,
                "momentum_raw": s.momentum_raw,
                "fundamental_raw": s.fundamental_raw,
                "insider_raw": s.insider_raw,
                "sentiment_raw": s.sentiment_raw,
                "pairs_raw": s.pairs_raw,
                "macro_on": s.macro_on,
                "correlation_warning": warnings.get(&s.ticker).map(|w| {
                    serde_json::json!({
                        "correlated_with": w.correlated_with,
                        "correlation": w.correlation,
                    })
                }),
            })).collect::<Vec<_>>(),
        });

        match std::fs::write(&filename, serde_json::to_string_pretty(&json).unwrap_or_default()) {
            Ok(_)  => println!("{DIM}Picks saved → {filename}{RESET}"),
            Err(e) => eprintln!("Warning: could not write {filename}: {e}"),
        }
    }

    /// Delegate to the correlation module's standalone report function.
    pub fn correlations_report(corr_data: &Arc<CorrelationData>) {
        print_correlation_report(corr_data);
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn colour_for(v: f64) -> &'static str {
    if v > 0.0 { GREEN } else if v < 0.0 { RED } else { WHITE }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max.saturating_sub(1)])
    }
}