use crate::data::MacroSnapshot;

/// Binary macro regime filter — true = risk-on, false = suppress all longs.
///
/// Rules:
///   - VIX > 25                         → risk-off
///   - 10Y yield spike > 0.5% in 30d   → risk-off
///   - Otherwise                        → risk-on
pub struct MacroFilter;

impl MacroFilter {
    pub fn compute_macro_on(snapshot: &MacroSnapshot) -> bool {
        snapshot.macro_on
    }
}

/// Human-readable description of the macro regime.
pub fn macro_regime_description(snapshot: &MacroSnapshot) -> String {
    let regime = if snapshot.macro_on { "RISK-ON ✓" } else { "RISK-OFF ✗" };

    let vix_str = snapshot
        .vix
        .map(|v| format!("VIX: {:.1}", v))
        .unwrap_or_else(|| "VIX: N/A".to_string());

    let yield_str = match (snapshot.yield_10y, snapshot.yield_10y_30d_ago) {
        (Some(now), Some(ago)) => {
            let spike = now - ago;
            format!("10Y: {:.2}% ({:+.2}% vs 30d ago)", now, spike)
        }
        (Some(now), None) => format!("10Y: {:.2}%", now),
        _ => "10Y: N/A".to_string(),
    };

    let reason = if !snapshot.macro_on {
        let vix_trigger = snapshot.vix.map(|v| v >= 25.0).unwrap_or(false);
        let yield_trigger = match (snapshot.yield_10y, snapshot.yield_10y_30d_ago) {
            (Some(now), Some(ago)) => now - ago >= 0.5,
            _ => false,
        };
        if vix_trigger && yield_trigger {
            " [VIX spike + yield spike]"
        } else if vix_trigger {
            " [VIX > 25]"
        } else if yield_trigger {
            " [yield spike > 0.5% in 30d]"
        } else {
            " [data unavailable]"
        }
    } else {
        ""
    };

    format!("{regime}{reason}  ({vix_str}, {yield_str})")
}
