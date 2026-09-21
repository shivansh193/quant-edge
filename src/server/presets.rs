use serde::{Deserialize, Serialize};

use crate::llm::strategy_spec::{SignalWeightOverride, StrategyFilters, StrategySpec};

/// A named preset strategy shown in the Strategy Playground.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresetStrategy {
    pub id:          String,
    pub name:        String,
    pub description: String,
    pub icon:        String,
    /// Pre-built spec — callers can override fields before running.
    pub spec:        StrategySpec,
}

/// Return all 12 preset strategies.
pub fn all_presets() -> Vec<PresetStrategy> {
    vec![
        PresetStrategy {
            id:          "momentum_pure".into(),
            name:        "Pure Momentum".into(),
            description: "90% weight on price momentum — chase the trend.".into(),
            icon:        "🚀".into(),
            spec: StrategySpec {
                name: "Pure Momentum".into(),
                signal_weights: SignalWeightOverride {
                    momentum:    Some(0.90),
                    fundamental: Some(0.025),
                    insider:     Some(0.025),
                    sentiment:   Some(0.025),
                    pairs:       Some(0.025),
                },
                filters: StrategyFilters::default(),
                universe_override: None,
                top_n: 15,
                holding_period_days: 20,
            },
        },
        PresetStrategy {
            id:          "value_deep".into(),
            name:        "Deep Value".into(),
            description: "85% fundamental weight — buy cheap, ignore noise.".into(),
            icon:        "💎".into(),
            spec: StrategySpec {
                name: "Deep Value".into(),
                signal_weights: SignalWeightOverride {
                    momentum:    Some(0.05),
                    fundamental: Some(0.85),
                    insider:     Some(0.025),
                    sentiment:   Some(0.025),
                    pairs:       Some(0.05),
                },
                filters: StrategyFilters::default(),
                universe_override: None,
                top_n: 12,
                holding_period_days: 90,
            },
        },
        PresetStrategy {
            id:          "quality_growth".into(),
            name:        "Quality Growth".into(),
            description: "Strong fundamentals + momentum — quality at a reasonable price.".into(),
            icon:        "📈".into(),
            spec: StrategySpec {
                name: "Quality Growth".into(),
                signal_weights: SignalWeightOverride {
                    momentum:    Some(0.30),
                    fundamental: Some(0.60),
                    insider:     None,
                    sentiment:   Some(0.10),
                    pairs:       None,
                },
                filters: StrategyFilters::default(),
                universe_override: None,
                top_n: 15,
                holding_period_days: 60,
            },
        },
        PresetStrategy {
            id:          "insider_follow".into(),
            name:        "Follow the Insiders".into(),
            description: "70% insider signal — bet with management who know the most.".into(),
            icon:        "🕵️".into(),
            spec: StrategySpec {
                name: "Follow the Insiders".into(),
                signal_weights: SignalWeightOverride {
                    momentum:    Some(0.20),
                    fundamental: Some(0.10),
                    insider:     Some(0.70),
                    sentiment:   None,
                    pairs:       None,
                },
                filters: StrategyFilters::default(),
                universe_override: None,
                top_n: 10,
                holding_period_days: 45,
            },
        },
        PresetStrategy {
            id:          "sentiment_driven".into(),
            name:        "Sentiment Surge".into(),
            description: "80% sentiment — ride social and news momentum waves.".into(),
            icon:        "📣".into(),
            spec: StrategySpec {
                name: "Sentiment Surge".into(),
                signal_weights: SignalWeightOverride {
                    momentum:    Some(0.15),
                    fundamental: Some(0.025),
                    insider:     Some(0.025),
                    sentiment:   Some(0.80),
                    pairs:       None,
                },
                filters: StrategyFilters::default(),
                universe_override: None,
                top_n: 10,
                holding_period_days: 14,
            },
        },
        PresetStrategy {
            id:          "low_correlation".into(),
            name:        "Diversification Max".into(),
            description: "60% pairs signal — build the most uncorrelated portfolio possible.".into(),
            icon:        "🌐".into(),
            spec: StrategySpec {
                name: "Diversification Max".into(),
                signal_weights: SignalWeightOverride {
                    momentum:    Some(0.10),
                    fundamental: Some(0.30),
                    insider:     None,
                    sentiment:   None,
                    pairs:       Some(0.60),
                },
                filters: StrategyFilters::default(),
                universe_override: None,
                top_n: 20,
                holding_period_days: 30,
            },
        },
        PresetStrategy {
            id:          "macro_aware".into(),
            name:        "Macro Regime".into(),
            description: "Macro gate always on — go flat when VIX spikes or yields surge.".into(),
            icon:        "🏦".into(),
            spec: StrategySpec {
                name: "Macro Regime".into(),
                signal_weights: SignalWeightOverride {
                    momentum:    Some(0.50),
                    fundamental: Some(0.50),
                    insider:     None,
                    sentiment:   None,
                    pairs:       None,
                },
                filters: StrategyFilters {
                    macro_filter_enabled: true,
                    ..StrategyFilters::default()
                },
                universe_override: None,
                top_n: 15,
                holding_period_days: 30,
            },
        },
        PresetStrategy {
            id:          "india_growth".into(),
            name:        "India Growth".into(),
            description: "NSE-only universe — India's fastest-growing mid and large caps.".into(),
            icon:        "🇮🇳".into(),
            spec: StrategySpec {
                name: "India Growth".into(),
                signal_weights: SignalWeightOverride {
                    momentum:    Some(0.40),
                    fundamental: Some(0.50),
                    insider:     None,
                    sentiment:   Some(0.10),
                    pairs:       None,
                },
                filters: StrategyFilters::default(),
                // NSE tickers end with .NS — filtered in engine by universe market setting
                universe_override: None,
                top_n: 15,
                holding_period_days: 30,
            },
        },
        PresetStrategy {
            id:          "us_tech_momentum".into(),
            name:        "US Tech Momentum".into(),
            description: "NYSE/NASDAQ tech sector with heavy momentum tilt.".into(),
            icon:        "💻".into(),
            spec: StrategySpec {
                name: "US Tech Momentum".into(),
                signal_weights: SignalWeightOverride {
                    momentum:    Some(0.70),
                    fundamental: Some(0.20),
                    insider:     None,
                    sentiment:   Some(0.10),
                    pairs:       None,
                },
                filters: StrategyFilters {
                    sectors: Some(vec!["Information Technology".into()]),
                    ..StrategyFilters::default()
                },
                universe_override: None,
                top_n: 15,
                holding_period_days: 20,
            },
        },
        PresetStrategy {
            id:          "contrarian".into(),
            name:        "Contrarian".into(),
            description: "Inverted momentum — buy beaten-down quality stocks the crowd hates.".into(),
            icon:        "🔄".into(),
            spec: StrategySpec {
                name: "Contrarian".into(),
                // Negative momentum weight handled in engine by negating momentum_raw
                signal_weights: SignalWeightOverride {
                    momentum:    Some(-0.20),
                    fundamental: Some(0.60),
                    insider:     None,
                    sentiment:   None,
                    pairs:       Some(0.20),
                },
                filters: StrategyFilters::default(),
                universe_override: None,
                top_n: 12,
                holding_period_days: 60,
            },
        },
        PresetStrategy {
            id:          "balanced".into(),
            name:        "Balanced All-Signal".into(),
            description: "Equal weight across all 5 signals — no single factor dominates.".into(),
            icon:        "⚖️".into(),
            spec: StrategySpec {
                name: "Balanced All-Signal".into(),
                signal_weights: SignalWeightOverride {
                    momentum:    Some(0.20),
                    fundamental: Some(0.20),
                    insider:     Some(0.20),
                    sentiment:   Some(0.20),
                    pairs:       Some(0.20),
                },
                filters: StrategyFilters::default(),
                universe_override: None,
                top_n: 15,
                holding_period_days: 30,
            },
        },
        PresetStrategy {
            id:          "ai_picks".into(),
            name:        "AI Curated".into(),
            description: "Gemini generates the strategy spec from scratch — fully AI-driven.".into(),
            icon:        "🤖".into(),
            spec: StrategySpec {
                name: "AI Curated".into(),
                signal_weights: SignalWeightOverride::default(),
                filters: StrategyFilters::default(),
                universe_override: None,
                top_n: 10,
                holding_period_days: 30,
            },
        },
    ]
}

/// Look up a preset by ID.
pub fn find_preset(id: &str) -> Option<PresetStrategy> {
    all_presets().into_iter().find(|p| p.id == id)
}

/// The hardcoded prompt used when running the ai_picks preset.
pub const AI_PICKS_PROMPT: &str =
    "Pick the 10 most promising stocks right now across US and Indian markets \
     based on all available signals. Prioritize quality and momentum. \
     Avoid highly correlated picks.";
