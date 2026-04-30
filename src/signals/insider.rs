use super::{MarketData, Signal, clamp_signal};

/// Insider signal from SEC EDGAR Form 4 filings:
///   - Net shares bought by insiders in last 30/60/90 days
///   - CEO/CFO buys weighted 2× vs director 1×
///   - Exponential time-decay: recent trades weighted more
pub struct InsiderSignal;

impl Signal for InsiderSignal {
    fn name(&self) -> &str {
        "Insider"
    }

    fn compute(&self, _ticker: &str, data: &MarketData) -> f64 {
        if data.insider_trades.is_empty() {
            return 0.0;
        }

        let as_of = data.as_of;
        let mut net_score = 0.0f64;
        let mut max_possible = 0.0f64;

        for trade in &data.insider_trades {
            let days_ago = (as_of - trade.trade_date).num_days().max(0) as f64;
            if days_ago > 90.0 {
                continue;
            }

            // Role weight
            let role_weight = role_multiplier(&trade.insider_role);

            // Time decay: e^(-λt), half-life ≈ 30 days → λ = ln(2)/30
            let decay = (-0.0231 * days_ago).exp(); // λ = ln2/30 ≈ 0.0231

            // Direction: acquired = +1, disposed = -1
            let direction = if trade.transaction_type == "A" { 1.0 } else { -1.0 };

            // Normalise shares: cap at 100k shares per trade to prevent outlier domination
            let shares_norm = (trade.shares / 100_000.0).min(1.0);

            net_score    += direction * role_weight * decay * shares_norm;
            max_possible += role_weight * decay * shares_norm;
        }

        if max_possible < 1e-9 {
            return 0.0;
        }

        // net_score ∈ [-max_possible, +max_possible] → normalise to [-1, +1]
        clamp_signal(net_score / max_possible)
    }
}

// ── Role weight table ─────────────────────────────────────────────────────────

fn role_multiplier(role: &str) -> f64 {
    match role {
        "CEO" | "CFO" => 2.0,
        "President" | "COO" => 1.5,
        "Officer" => 1.2,
        "Director" => 1.0,
        _ => 0.8,
    }
}
