use crate::data::FundamentalSnapshot;
use super::{MarketData, Signal, clamp_signal, zscore_signal};

/// Combines four quality/value/growth sub-signals:
///   1. Revenue growth YoY vs industry median
///   2. Net margin vs industry median
///   3. P/E relative to sector (low = value, context-dependent)
///   4. Piotroski F-score proxy (from available metrics)
pub struct FundamentalSignal;

impl Signal for FundamentalSignal {
    fn name(&self) -> &str {
        "Fundamental"
    }

    fn compute(&self, ticker: &str, data: &MarketData) -> f64 {
        let Some(fund) = &data.fundamentals else {
            return 0.0;
        };

        let peer_snaps: Vec<&FundamentalSnapshot> = data.peer_fundamentals.values().collect();

        // ── 1. Revenue growth relative to peers ──────────────────────────────
        let rev_score = match fund.revenue_cagr_3yr {
            Some(cagr) => {
                let peer_cagrs: Vec<f64> = peer_snaps
                    .iter()
                    .filter_map(|p| p.revenue_cagr_3yr)
                    .collect();
                if peer_cagrs.is_empty() {
                    // Absolute: positive CAGR is good
                    clamp_signal(cagr * 3.0) // e.g. 30% CAGR → +0.9
                } else {
                    zscore_signal(cagr, &peer_cagrs, true)
                }
            }
            None => 0.0,
        };

        // ── 2. Net margin relative to peers ──────────────────────────────────
        let margin_score = match fund.net_margin_pct {
            Some(margin) => {
                let peer_margins: Vec<f64> = peer_snaps
                    .iter()
                    .filter_map(|p| p.net_margin_pct)
                    .collect();
                if peer_margins.is_empty() {
                    clamp_signal(margin / 30.0) // 30% net margin → +1
                } else {
                    zscore_signal(margin, &peer_margins, true)
                }
            }
            None => 0.0,
        };

        // ── 3. P/B ratio relative to peers (lower is better = deep value) ───
        let pb_score = match fund.price_to_book {
            Some(pb) if pb > 0.0 => {
                let peer_pbs: Vec<f64> = peer_snaps
                    .iter()
                    .filter_map(|p| p.price_to_book.filter(|&v| v > 0.0))
                    .collect();
                if peer_pbs.is_empty() {
                    // Low P/B is value; P/B < 1 is strongly positive
                    clamp_signal(1.5 / pb.max(0.1) - 1.0)
                } else {
                    zscore_signal(pb, &peer_pbs, false) // lower P/B = better
                }
            }
            _ => 0.0,
        };

        // ── 4. Piotroski F-score proxy ────────────────────────────────────────
        // Full F-score needs 9 signals; we compute from available metrics.
        let f_score = piotroski_proxy(fund);
        // F-score: 0–9, normalise to [-1, +1]: (score/9)*2 - 1
        let f_signal = (f_score as f64 / 9.0) * 2.0 - 1.0;

        // Combine: revenue 35%, margin 35%, P/B 15%, Piotroski 15%
        let raw = 0.35 * rev_score + 0.35 * margin_score + 0.15 * pb_score + 0.15 * f_signal;
        clamp_signal(raw)
    }
}

// ── Piotroski F-score proxy ───────────────────────────────────────────────────
// Uses the 5 metrics we actually have from Yahoo fundamentals.
// Each criterion contributes 1 point.

fn piotroski_proxy(fund: &FundamentalSnapshot) -> u8 {
    let mut score = 0u8;

    // Profitability signals (up to 3 points)
    if let Some(margin) = fund.net_margin_pct {
        if margin > 0.0 { score += 1; }       // positive net income
        if margin > 5.0 { score += 1; }       // healthy margin
    }
    if let Some(cagr) = fund.revenue_cagr_3yr {
        if cagr > 0.05 { score += 1; }        // growing revenue
    }

    // Leverage / liquidity (up to 2 points)
    if let Some(d2e) = fund.debt_to_equity {
        if d2e < 100.0 { score += 1; }        // D/E < 1.0 (stored as %)
        if d2e < 50.0  { score += 1; }        // D/E < 0.5
    }

    // Efficiency / value (up to 2 points)
    if let Some(pb) = fund.price_to_book {
        if pb > 0.0 && pb < 3.0 { score += 1; } // reasonable valuation
    }
    if let Some(rev) = fund.revenue_ttm {
        if rev > 1_000_000_000.0 { score += 1; }  // scale/moat proxy
    }

    // Momentum proxy (1 point)
    if let Some(ret) = fund.price_return_12m_1m {
        if ret > 0.0 { score += 1; }
    }

    // Market share / operational strength (1 point)
    if let Some(ms) = fund.market_share_proxy {
        if ms > 0.05 { score += 1; } // > 5% industry revenue share
    }

    score.min(9)
}
