use chrono::NaiveDate;

use super::engine::{BacktestResult, TradeSide};

/// Print a full backtest report to stdout.
pub fn print_backtest_report(result: &BacktestResult, spec_name: &str) {
    println!();
    println!("\x1b[1m\x1b[97m══════════════════════════════════════════════\x1b[0m");
    println!("\x1b[1m\x1b[97m  BACKTEST REPORT — {}\x1b[0m", spec_name);
    println!("\x1b[1m\x1b[97m══════════════════════════════════════════════\x1b[0m");
    println!();

    // Performance summary table
    println!("  \x1b[1mPerformance Summary\x1b[0m");
    println!("  ─────────────────────────────────────────");
    println!("  Final Portfolio Value   {:>14.2}", result.final_value);
    println!("  Total Return            {:>13.2}%", result.total_return_pct);
    println!("  Annualised Return       {:>13.2}%", result.annualised_return_pct);
    println!("  Sharpe Ratio            {:>14.2}", result.sharpe_ratio);
    println!("  Max Drawdown            {:>13.2}%", result.max_drawdown_pct);
    println!("  Win Rate                {:>13.2}%", result.win_rate_pct);
    println!("  Trades Executed         {:>14}", result.trades.len());

    // Signal IC summary
    if !result.signal_ic_per_period.is_empty() {
        let mean_ic: f64 =
            result.signal_ic_per_period.iter().sum::<f64>() / result.signal_ic_per_period.len() as f64;
        println!("  Mean Signal IC          {:>14.4}", mean_ic);
        println!("  IC Periods              {:>14}", result.signal_ic_per_period.len());
    }

    if result.beta.is_some() || result.cvar_95.is_some() || result.final_concentration_hhi.is_some() {
        println!();
        println!("  \x1b[1mRisk\x1b[0m");
        println!("  ─────────────────────────────────────────");
        if let Some(b) = result.beta {
            println!("  Beta (vs. benchmark)    {:>14.2}", b);
        }
        if let Some(cv) = result.cvar_95 {
            println!("  95% CVaR (daily)        {:>13.2}%", cv * 100.0);
        }
        if let Some(hhi) = result.final_concentration_hhi {
            let effective_n = if hhi > 1e-12 { 1.0 / hhi } else { 0.0 };
            println!("  Final HHI               {:>14.3}", hhi);
            println!("  Effective # positions   {:>14.1}", effective_n);
        }
        if result.breaker_trip_days_pct > 0.0 {
            println!("  Drawdown breaker active {:>13.1}%  of days", result.breaker_trip_days_pct);
        }
    }

    println!();
    println!("  \x1b[1mEquity Curve (monthly samples)\x1b[0m");
    println!("  ─────────────────────────────────────────");
    print_equity_sparkline(&result.daily_equity);

    // Recent trades
    println!();
    println!("  \x1b[1mRecent Trades (last 20)\x1b[0m");
    println!("  ─────────────────────────────────────────");
    println!("  {:>10}  {:>6}  {:>4}  {:>10}  {:>9}",
        "Date", "Ticker", "Side", "Shares", "Price");
    println!("  ─────────────────────────────────────────");
    let recent_trades: Vec<_> = result.trades.iter().rev().take(20).collect();
    for t in recent_trades.iter().rev() {
        let side_str = match t.side {
            TradeSide::Buy  => "\x1b[32mBUY \x1b[0m",
            TradeSide::Sell => "\x1b[31mSELL\x1b[0m",
        };
        println!(
            "  {:>10}  {:>6}  {}  {:>10.2}  {:>9.2}",
            t.date, t.ticker, side_str, t.shares, t.price,
        );
    }

    // Signal IC per period
    if !result.signal_ic_per_period.is_empty() {
        println!();
        println!("  \x1b[1mSignal IC Per Rebalance Period\x1b[0m");
        println!("  ─────────────────────────────────────────");
        for (i, ic) in result.signal_ic_per_period.iter().enumerate() {
            let colour = if *ic > 0.0 { "\x1b[32m" } else { "\x1b[31m" };
            println!("  Period {:>3}:  {}{:>+.4}\x1b[0m", i + 1, colour, ic);
        }
    }

    println!();
}

/// Sample one point per ~21 trading days and render a mini sparkline.
fn print_equity_sparkline(equity: &[(NaiveDate, f64)]) {
    if equity.is_empty() {
        return;
    }

    let step = (equity.len() / 60).max(1);
    let samples: Vec<f64> = equity.iter().step_by(step).map(|(_, v)| *v).collect();

    if samples.is_empty() {
        return;
    }

    let min = samples.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = samples.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let range = (max - min).max(1.0);

    const BLOCKS: &[char] = &[' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let bar: String = samples
        .iter()
        .map(|v| {
            let norm = ((v - min) / range * 8.0) as usize;
            BLOCKS[norm.min(8)]
        })
        .collect();

    println!("  {:>8.0} ┤{}├ {:.0}", min, bar, max);
    println!(
        "  Dates: {} → {}",
        equity.first().map(|(d, _)| d.to_string()).unwrap_or_default(),
        equity.last().map(|(d, _)| d.to_string()).unwrap_or_default(),
    );
}
