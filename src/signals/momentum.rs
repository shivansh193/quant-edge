use crate::data::PriceBar;
use super::{MarketData, Signal, clamp_signal, zscore_signal};

/// Combines three price-momentum sub-signals:
///   1. 12-1 month price return (skip last month to avoid reversal)
///   2. Relative strength vs industry peers
///   3. 50-day MA vs 200-day MA crossover (golden cross)
pub struct MomentumSignal;

impl Signal for MomentumSignal {
    fn name(&self) -> &str {
        "Momentum"
    }

    fn compute(&self, _ticker: &str, data: &MarketData) -> f64 {
        let bars = &data.price_bars;

        let signal_12m1m = compute_12m1m(bars);
        let signal_ma    = compute_ma_crossover(bars);
        let signal_rel   = compute_relative_strength(
            signal_12m1m,
            &data.peer_returns_12m1m,
        );

        // Weights: 12m-1m momentum 50%, relative strength 30%, MA crossover 20%
        let raw = 0.50 * signal_12m1m + 0.30 * signal_rel + 0.20 * signal_ma;
        clamp_signal(raw)
    }
}

// ── 12-1 month price return ───────────────────────────────────────────────────

/// (price[t-1m] - price[t-12m]) / price[t-12m], normalised to [-1,1].
/// Bars must span at least 12 months; we skip the most recent ~21 trading days.
fn compute_12m1m(bars: &[PriceBar]) -> f64 {
    if bars.len() < 50 {
        return 0.0;
    }
    // "1 month ago" ≈ 21 trading days from end; "12 months ago" ≈ first bar
    let skip = 21.min(bars.len() / 10);
    let end_idx = bars.len().saturating_sub(skip + 1);
    let start_idx = 0usize;

    let price_end   = bars[end_idx].adj_close;
    let price_start = bars[start_idx].adj_close;

    if price_start <= 0.0 {
        return 0.0;
    }
    let ret = (price_end - price_start) / price_start;
    // Typical 12-1m returns range roughly -50% to +150%; scale so ±100% → ±1
    clamp_signal(ret)
}

// ── 50-day / 200-day moving average crossover ─────────────────────────────────

fn compute_ma_crossover(bars: &[PriceBar]) -> f64 {
    if bars.len() < 200 {
        return 0.0;
    }
    let ma50  = moving_avg(&bars[bars.len() - 50..]);
    let ma200 = moving_avg(&bars[bars.len() - 200..]);

    if ma200 <= 0.0 {
        return 0.0;
    }
    // Spread: how far above/below 200-day MA is the 50-day MA
    let spread = (ma50 - ma200) / ma200;
    // ±10% spread → ±1
    clamp_signal(spread * 10.0)
}

fn moving_avg(bars: &[PriceBar]) -> f64 {
    if bars.is_empty() {
        return 0.0;
    }
    bars.iter().map(|b| b.adj_close).sum::<f64>() / bars.len() as f64
}

// ── Relative strength vs industry peers ──────────────────────────────────────

fn compute_relative_strength(
    own_return: f64,
    peer_returns: &std::collections::HashMap<String, f64>,
) -> f64 {
    if peer_returns.is_empty() {
        return 0.0;
    }
    let universe: Vec<f64> = peer_returns.values().copied().collect();
    zscore_signal(own_return, &universe, true)
}
