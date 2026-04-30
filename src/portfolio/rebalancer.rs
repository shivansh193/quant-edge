use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::info;

use crate::roles::classifier::{IndustryRoster, Role, RoleClassifier};
use crate::universe::builder::Universe;
use crate::data::source::DataSource;
use super::weights::{WeightMap, WeightMode, compute_weights};

// ── Swap log ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapEvent {
    pub date:          NaiveDate,
    pub industry_code: u32,
    pub industry_name: String,
    pub role:          Role,
    pub outgoing:      String,   // ticker losing the role
    pub incoming:      String,   // ticker gaining the role
    pub reason:        String,   // human-readable metric explanation
}

// ── Rebalancer ────────────────────────────────────────────────────────────────

pub struct Rebalancer<'a> {
    classifier: RoleClassifier<'a>,
    mode:       WeightMode,
}

impl<'a> Rebalancer<'a> {
    pub fn new(source: &'a dyn DataSource, active_roles: Vec<Role>, mode: WeightMode) -> Self {
        Self {
            classifier: RoleClassifier::new(source, active_roles),
            mode,
        }
    }

    /// Run a single rebalance step:
    /// 1. Classify every industry as of `date`
    /// 2. Diff against previous rosters to detect swaps
    /// 3. Emit swap events
    /// 4. Return new rosters + updated WeightMap
    pub async fn rebalance(
        &self,
        universe: &Universe,
        date: NaiveDate,
        previous_rosters: &HashMap<u32, IndustryRoster>,
    ) -> anyhow::Result<RebalanceResult> {
        // Fresh classification as of this date
        let new_rosters = self
            .classifier
            .classify_universe(universe, date)
            .await?;

        // Detect swaps by diffing against previous rosters
        let mut swap_events: Vec<SwapEvent> = Vec::new();

        for (industry_code, new_roster) in &new_rosters {
            if let Some(prev_roster) = previous_rosters.get(industry_code) {
                let swaps = RoleClassifier::detect_swaps(prev_roster, new_roster);

                for (role, outgoing, incoming) in swaps {
                    let reason = self.format_swap_reason(
                        &role,
                        &outgoing,
                        &incoming,
                        new_roster,
                    );

                    info!(
                        date = %date,
                        industry = %new_roster.industry_name,
                        role = %role.label(),
                        "{} → {}  ({})",
                        outgoing, incoming, reason
                    );

                    swap_events.push(SwapEvent {
                        date,
                        industry_code: *industry_code,
                        industry_name: new_roster.industry_name.clone(),
                        role,
                        outgoing,
                        incoming,
                        reason,
                    });
                }
            }
        }

        // Build flat (ticker, role) list for weight computation
        let assignments: Vec<(String, Role)> = new_rosters
            .values()
            .flat_map(|roster| {
                roster.assignments.values().map(|a| (a.ticker.clone(), a.role.clone()))
            })
            .collect();

        let weights = compute_weights(&assignments, &self.mode);

        Ok(RebalanceResult {
            rosters: new_rosters,
            weights,
            swap_events,
        })
    }

    /// Human-readable explanation of why a swap happened.
    fn format_swap_reason(
        &self,
        role: &Role,
        outgoing: &str,
        incoming: &str,
        roster: &IndustryRoster,
    ) -> String {
        let winner = roster.assignments.get(role);

        match winner {
            Some(a) => format!(
                "{} now leads {} with metric value {:.4}",
                incoming,
                role.label(),
                a.metric_value
            ),
            None => format!(
                "{} displaced {} for role {}",
                incoming, outgoing, role.label()
            ),
        }
    }
}

pub struct RebalanceResult {
    pub rosters:     HashMap<u32, IndustryRoster>,
    pub weights:     WeightMap,
    pub swap_events: Vec<SwapEvent>,
}