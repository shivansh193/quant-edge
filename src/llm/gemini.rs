use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use super::strategy_spec::StrategySpec;

// Model ID — no "-latest" suffix; Google routes this to the stable latest automatically
const MODEL: &str = "gemini-2.5-flash";
const BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta/models";

const SYSTEM_PROMPT: &str = r#"You are a quantitative strategy parser. Convert the user's natural language strategy description into a JSON object matching this exact schema:

{
  "name": string,
  "signal_weights": {
    "momentum": number | null,
    "fundamental": number | null,
    "insider": number | null,
    "sentiment": number | null,
    "pairs": number | null
  },
  "filters": {
    "min_market_cap_b": number | null,
    "sectors": [string] | null,
    "exclude_sectors": [string] | null,
    "macro_filter_enabled": boolean,
    "min_score": number | null
  },
  "universe_override": [string] | null,
  "top_n": integer,
  "holding_period_days": integer
}

Rules:
- Signal weights should reflect the user's emphasis. If they say "focus on momentum", set momentum high (0.5+). If they don't mention a signal, set it null (engine will use default).
- Sectors must be valid GICS sector names: Energy, Materials, Industrials, Consumer Discretionary, Consumer Staples, Health Care, Financials, Information Technology, Communication Services, Utilities, Real Estate.
- Return ONLY the JSON object. No explanation, no markdown, no code fences."#;

// ── Gemini REST request/response shapes ───────────────────────────────────────

#[derive(Serialize)]
struct GeminiRequest {
    // API field is camelCase
    #[serde(rename = "systemInstruction")]
    system_instruction: SystemInstruction,
    contents: Vec<Content>,
    #[serde(rename = "generationConfig")]
    generation_config: GenerationConfig,
}

#[derive(Serialize)]
struct SystemInstruction {
    parts: Vec<Part>,
}

#[derive(Serialize, Deserialize)]
struct Content {
    parts: Vec<Part>,
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Part {
    text: String,
    /// Present in 2.5 Flash responses when thinking is enabled.
    /// true  → internal reasoning token (skip)
    /// false / absent → final answer (use this)
    #[serde(default)]
    thought: bool,
}

#[derive(Serialize)]
struct GenerationConfig {
    temperature: f64,
    #[serde(rename = "maxOutputTokens")]
    max_output_tokens: u32,
    /// Disable thinking for this simple structured-extraction task:
    /// faster, cheaper, and avoids thought parts in the response.
    #[serde(rename = "thinkingConfig")]
    thinking_config: ThinkingConfig,
}

#[derive(Serialize)]
struct ThinkingConfig {
    /// 0 = disabled (valid range for 2.5 Flash: 0–24 576)
    #[serde(rename = "thinkingBudget")]
    thinking_budget: u32,
}

#[derive(Deserialize)]
struct GeminiResponse {
    candidates: Vec<Candidate>,
}

#[derive(Deserialize)]
struct Candidate {
    content: Content,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Call Gemini once to parse a natural-language strategy into a `StrategySpec`.
/// Gemini is called exactly once per invocation; all subsequent processing is pure Rust.
pub async fn parse_strategy(natural_language: &str) -> Result<StrategySpec> {
    let api_key = std::env::var("GEMINI_API_KEY")
        .context("GEMINI_API_KEY not set — add it to .env or environment")?;

    // No "-latest" suffix — Google routes to stable latest automatically
    let url = format!("{}/{}:generateContent?key={}", BASE_URL, MODEL, api_key);

    let request = GeminiRequest {
        system_instruction: SystemInstruction {
            parts: vec![Part { text: SYSTEM_PROMPT.to_string(), thought: false }],
        },
        contents: vec![Content {
            role: Some("user".to_string()),
            parts: vec![Part { text: natural_language.to_string(), thought: false }],
        }],
        generation_config: GenerationConfig {
            temperature: 0.0,
            max_output_tokens: 1024,
            thinking_config: ThinkingConfig { thinking_budget: 0 },
        },
    };

    let client = reqwest::Client::new();
    let response = client
        .post(&url)
        .json(&request)
        .send()
        .await
        .context("Gemini API request failed")?;

    let status = response.status();
    let body = response.text().await.context("Failed to read Gemini response body")?;

    if !status.is_success() {
        bail!("Gemini API returned {}: {}", status, body);
    }

    let gemini_resp: GeminiResponse =
        serde_json::from_str(&body).context("Failed to parse Gemini response JSON")?;

    // Skip any thought parts (thought: true) — take the first non-thought part
    let text = gemini_resp
        .candidates
        .into_iter()
        .next()
        .context("Gemini returned no candidates")?
        .content
        .parts
        .into_iter()
        .find(|p| !p.thought)
        .map(|p| p.text)
        .context("Gemini response had no non-thought parts")?;

    // Strip markdown fences if Gemini added them despite instructions
    let json_text = strip_markdown_fences(&text);

    let mut spec: StrategySpec =
        serde_json::from_str(json_text).with_context(|| {
            format!("Gemini response is not valid StrategySpec JSON:\n{}", json_text)
        })?;

    spec.validate()?;

    Ok(spec)
}

fn strip_markdown_fences(s: &str) -> &str {
    let s = s.trim();
    let s = s.strip_prefix("```json").or_else(|| s.strip_prefix("```")).unwrap_or(s);
    let s = s.strip_suffix("```").unwrap_or(s);
    s.trim()
}
