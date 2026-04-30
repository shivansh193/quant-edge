use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::{info, warn};

use crate::data::source::{AssetInfo, DataSource, MarketCap};
use crate::gics::taxonomy::{GicsTaxonomy, Industry};

// ── Config types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, clap::ValueEnum, PartialEq)]
pub enum Market {
    NSE,
    NYSE,
    Both,
}

#[derive(Debug, Clone, Serialize, Deserialize, clap::ValueEnum, PartialEq)]
pub enum CapFilter {
    SmallCap,
    MidCap,
    LargeCap,
    Mixed,
}

/// Everything the builder needs — constructed from CLI args, passed around.
#[derive(Debug, Clone)]
pub struct UniverseConfig {
    pub market:           Market,
    pub cap_filter:       CapFilter,
    pub n_industries:     usize,
    pub exclude_industry_codes: Vec<u32>,
}

// ── Output types ──────────────────────────────────────────────────────────────

/// One company slot in the universe — ticker resolved and validated.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompanySlot {
    pub ticker:        String,
    pub name:          String,
    pub exchange:      String,
    pub currency:      String,
    pub market_cap_usd: Option<f64>,
    pub market_cap_tier: Option<MarketCap>,
    pub industry_code: u32,
    pub industry_name: String,
    pub sector_code:   u32,
    /// GICS sector name e.g. "Information Technology" — populated by enrich_gics
    pub sector_name:   String,
    pub yahoo_industry: Option<String>
}

/// The resolved universe — a flat list of company slots grouped by industry.
/// Everything downstream (role classifier, simulation engine) reads from this.
#[derive(Debug)]
pub struct Universe {
    /// industry_code → Vec of candidate companies in that industry
    pub by_industry: HashMap<u32, Vec<CompanySlot>>,
    pub config:      UniverseConfig,
}

impl Universe {
    /// Flat iterator over all slots regardless of industry.
    pub fn all_slots(&self) -> impl Iterator<Item = &CompanySlot> {
        self.by_industry.values().flatten()
    }

    /// All unique tickers in the universe.
    pub fn tickers(&self) -> Vec<String> {
        self.all_slots().map(|s| s.ticker.clone()).collect()
    }

    /// How many industries have at least one company.
    pub fn populated_industry_count(&self) -> usize {
        self.by_industry.values().filter(|v| !v.is_empty()).count()
    }

    /// Total company count across all industries.
    pub fn total_companies(&self) -> usize {
        self.by_industry.values().map(|v| v.len()).sum()
    }

    /// Return a new Universe restricted to companies in the given GICS sector names.
    pub fn filter_by_sectors(&self, sectors: &[String]) -> Universe {
        if sectors.is_empty() {
            return self.clone_structure();
        }
        let norm: Vec<String> = sectors.iter().map(|s| s.to_lowercase()).collect();
        self.filter_slots(|slot| norm.contains(&slot.sector_name.to_lowercase()))
    }

    /// Return a new Universe with companies in the given sectors removed.
    pub fn filter_exclude_sectors(&self, sectors: &[String]) -> Universe {
        if sectors.is_empty() {
            return self.clone_structure();
        }
        let norm: Vec<String> = sectors.iter().map(|s| s.to_lowercase()).collect();
        self.filter_slots(|slot| !norm.contains(&slot.sector_name.to_lowercase()))
    }

    fn clone_structure(&self) -> Universe {
        Universe {
            by_industry: self.by_industry.clone(),
            config: self.config.clone(),
        }
    }

    fn filter_slots<F>(&self, predicate: F) -> Universe
    where
        F: Fn(&CompanySlot) -> bool,
    {
        let mut by_industry: HashMap<u32, Vec<CompanySlot>> = HashMap::new();
        for (&code, slots) in &self.by_industry {
            let filtered: Vec<CompanySlot> = slots.iter().filter(|s| predicate(s)).cloned().collect();
            by_industry.insert(code, filtered);
        }
        Universe { by_industry, config: self.config.clone() }
    }

    /// Keep at most `n` industries, dropping the smallest ones if there are more.
    /// Also removes excluded codes. Call after enrich_gics.
    pub fn trim_to_n_industries(&mut self, n: usize, exclude_codes: &[u32]) {
        self.by_industry.remove(&0); // drop staging bucket
        self.by_industry.retain(|code, _| !exclude_codes.contains(code));

        if self.by_industry.len() > n {
            let mut ranked: Vec<u32> = self.by_industry.keys().copied().collect();
            // Keep the N most-populated industries
            ranked.sort_by(|a, b| {
                let la = self.by_industry[b].len();
                let lb = self.by_industry[a].len();
                la.cmp(&lb)
            });
            let keep: std::collections::HashSet<u32> = ranked.into_iter().take(n).collect();
            self.by_industry.retain(|code, _| keep.contains(code));
        }
    }

    /// Build an industry-name → ticker-list map for the correlation engine.
    pub fn industry_ticker_map(&self) -> HashMap<String, Vec<String>> {
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        for slots in self.by_industry.values() {
            for slot in slots {
                if !slot.industry_name.is_empty() {
                    map.entry(slot.industry_name.clone())
                        .or_default()
                        .push(slot.ticker.clone());
                }
            }
        }
        map
    }
}

// ── Builder ───────────────────────────────────────────────────────────────────

pub struct UniverseBuilder<'a> {
    source:   &'a dyn DataSource,
    taxonomy: &'a GicsTaxonomy,
}

impl<'a> UniverseBuilder<'a> {
    pub fn new(source: &'a dyn DataSource, taxonomy: &'a GicsTaxonomy) -> Self {
        Self { source, taxonomy }
    }

    /// Build a universe directly from an in-memory list of tickers.
    pub async fn build_from_tickers(
        &self,
        tickers: Vec<String>,
        config: UniverseConfig,
    ) -> Result<Universe> {
        info!("Building universe from {} tickers", tickers.len());
        let slots = self.resolve_tickers(&tickers, &config).await;

        let mut by_industry: HashMap<u32, Vec<CompanySlot>> = HashMap::new();
        let mut filtered_cap = 0usize;
        let mut filtered_market = 0usize;
        let exclude = &config.exclude_industry_codes;

        for slot in slots {
            if !market_matches(&slot.exchange, &config.market) {
                filtered_market += 1;
                continue;
            }
            if !cap_matches(&slot.market_cap_tier, &config.cap_filter) {
                filtered_cap += 1;
                continue;
            }

            match self.resolve_industry_code(&slot.ticker, &slot) {
                Some(code) if exclude.contains(&code) => {
                    tracing::debug!(ticker = %slot.ticker, "excluded industry — skipping");
                }
                Some(code) => {
                    by_industry.entry(code).or_default().push(slot);
                }
                None => {
                    by_industry.entry(0).or_default().push(slot);
                }
            }
        }

        let staged = by_industry.get(&0).map(|v| v.len()).unwrap_or(0);
        let placed = by_industry.values().map(|v| v.len()).sum::<usize>().saturating_sub(staged);
        info!(
            "Universe built: {} placed, {} staged, {} filtered (cap={}, market={})",
            placed, staged, filtered_cap + filtered_market, filtered_cap, filtered_market,
        );

        Ok(Universe { by_industry, config })
    }

    /// Build a universe from a ticker list file.
    ///
    /// The file format is one ticker per line, optionally with exchange suffix:
    ///   RELIANCE.NS
    ///   INFY.NS
    ///   AAPL
    ///   MSFT
    ///
    /// Each ticker is resolved via the DataSource to get AssetInfo,
    /// then filtered by market + cap, then grouped by GICS industry.
    pub async fn build_from_file(
        &self,
        ticker_file: &str,
        config: UniverseConfig,
    ) -> Result<Universe> {
        let tickers = self.load_ticker_file(ticker_file)?;
        info!("Loaded {} tickers from {}", tickers.len(), ticker_file);

        // Resolve all tickers concurrently
        let slots = self.resolve_tickers(&tickers, &config).await;

        let mut by_industry: HashMap<u32, Vec<CompanySlot>> = HashMap::new();
        let mut filtered_cap = 0usize;
        let mut filtered_market = 0usize;

        let exclude = &config.exclude_industry_codes;

        for slot in slots {
            if !market_matches(&slot.exchange, &config.market) {
                filtered_market += 1;
                continue;
            }
            if !cap_matches(&slot.market_cap_tier, &config.cap_filter) {
                filtered_cap += 1;
                continue;
            }

            match self.resolve_industry_code(&slot.ticker, &slot) {
                Some(code) if exclude.contains(&code) => {
                    tracing::debug!(ticker = %slot.ticker, "excluded industry — skipping");
                }
                Some(code) => {
                    by_industry.entry(code).or_default().push(slot);
                }
                None => {
                    // GICS unknown from cache — stage in bucket 0 for enrich_gics
                    tracing::debug!(ticker = %slot.ticker, "GICS unknown — staging for enrichment");
                    by_industry.entry(0).or_default().push(slot);
                }
            }
        }

        let staged = by_industry.get(&0).map(|v| v.len()).unwrap_or(0);
        let placed = by_industry.values().map(|v| v.len()).sum::<usize>().saturating_sub(staged);
        info!(
            "Universe built: {} placed across {} industries, {} staged for enrichment \
             ({} filtered by cap, {} filtered by market)",
            placed,
            by_industry.len().saturating_sub(if staged > 0 { 1 } else { 0 }),
            staged,
            filtered_cap,
            filtered_market,
        );

        Ok(Universe { by_industry, config })
    }

    /// Resolve all tickers concurrently, drop failures with a warning.
    async fn resolve_tickers(
        &self,
        tickers: &[String],
        config: &UniverseConfig,
    ) -> Vec<CompanySlot> {
        use futures::future::join_all;

        let futures: Vec<_> = tickers
            .iter()
            .map(|ticker| self.resolve_single(ticker.clone()))
            .collect();

        let results = join_all(futures).await;

        results
            .into_iter()
            .filter_map(|res| match res {
                Ok(slot) => Some(slot),
                Err(e) => {
                    warn!("Ticker resolution failed: {:#}", e);
                    None
                }
            })
            .collect()
    }

    async fn resolve_single(&self, ticker: String) -> Result<CompanySlot> {
        let info: AssetInfo = self
            .source
            .asset_info(&ticker)
            .await
            .with_context(|| format!("Failed to fetch asset info for {}", ticker))?;
        tracing::debug!(ticker = %ticker, gics = ?info.gics_industry, "resolved asset info");

        Ok(CompanySlot {
            industry_code: 0, // placeholder — filled during grouping
            industry_name: String::new(),
            sector_code:   0,
            sector_name:   String::new(),
            ticker:        info.ticker,
            name:          info.name,
            exchange:      info.exchange,
            currency:      info.currency,
            market_cap_usd: info.market_cap_usd,
            market_cap_tier: info.market_cap_tier,
            yahoo_industry: info.gics_industry,
        })
    }

    /// Try to find an industry code for a resolved slot.
    /// Yahoo doesn't give us GICS directly, so we rely on the ticker's
    /// AssetInfo.gics_industry field (populated from cache if a prior
    /// enrichment run set it, or None). A separate enrichment step
    /// (see enrich_gics below) populates this from Yahoo's assetProfile.
fn resolve_industry_code(&self, _ticker: &str, slot: &CompanySlot) -> Option<u32> {
    let industry_str = slot.yahoo_industry.as_deref()?;
    self.taxonomy
        .resolve(industry_str)
        .or_else(|| self.alias_lookup(industry_str))
        .map(|ind| ind.code)
}

    /// Second-pass enrichment: fetch Yahoo assetProfile for each ticker
    /// to get the industry string, resolve it via GICS taxonomy, then
    /// patch the CompanySlot. Called after build_from_file if needed.
    pub async fn enrich_gics(
        &self,
        universe: &mut Universe,
    ) -> Result<()> {
        use futures::future::join_all;

        let all_tickers: Vec<(u32, usize, String)> = universe
            .by_industry
            .iter()
            .flat_map(|(&code, slots)| {
                slots
                    .iter()
                    .enumerate()
                    .map(move |(i, s)| (code, i, s.ticker.clone()))
            })
            .collect();

        let futures: Vec<_> = all_tickers
            .iter()
            .map(|(_, _, ticker)| self.fetch_yahoo_industry(ticker.clone()))
            .collect();

        let results = join_all(futures).await;

        for ((industry_code, slot_idx, ticker), result) in
            all_tickers.iter().zip(results)
        {
            match result {
                Ok((yahoo_industry_str, sector_code, ind_code, ind_name)) => {
                    if let Some(slots) = universe.by_industry.get_mut(industry_code) {
                        if let Some(slot) = slots.get_mut(*slot_idx) {
                            slot.industry_code = ind_code;
                            slot.industry_name = ind_name;
                            slot.sector_code   = sector_code;
                            slot.sector_name   = self
                                .taxonomy
                                .sector_by_code(sector_code)
                                .map(|s| s.name.clone())
                                .unwrap_or_default();
                        }
                    }
                }
                Err(e) => {
                    warn!(ticker = %ticker, "GICS enrichment failed: {:#}", e);
                }
            }
        }

        // Re-group by actual GICS codes now that slots are enriched
        self.regroup_by_gics(universe);

        Ok(())
    }

    /// Fetch Yahoo's industry string from assetProfile module.
    /// Returns (yahoo_industry_str, sector_code, industry_code, industry_name)
async fn fetch_yahoo_industry(
    &self,
    ticker: String,
) -> Result<(String, u32, u32, String)> {
    let info = self.source.asset_info(&ticker).await
        .with_context(|| format!("asset_info failed for {}", ticker))?;

    // Yahoo assetProfile industry string is stored in gics_industry
    // (populated by yahoo.rs from the assetProfile module)
    let industry_str = info.gics_industry
        .as_deref()
        .unwrap_or("")
        .to_string();

    let industry = self
        .taxonomy
        .resolve(&industry_str)
        .or_else(|| self.alias_lookup(&industry_str))
        .with_context(|| format!(
            "Cannot map Yahoo industry '{}' to GICS for {}",
            industry_str, ticker
        ))?;

    Ok((
        industry_str,
        industry.sector_code,
        industry.code,
        industry.name.clone(),
    ))
}

/// Alias table for Yahoo Finance industry strings → GICS industry names.
/// GICS names must match the CSV exactly (verified against data/gics.csv).
fn alias_lookup(&self, yahoo: &str) -> Option<&Industry> {
    let aliases: &[(&str, &str)] = &[
        // ── Software / Tech ───────────────────────────────────────────────
        ("Semiconductors",                           "Semiconductors & Semiconductor Equipment"),
        ("Software—Application",                     "Software"),
        ("Software—Infrastructure",                  "Software"),
        ("Software - Application",                   "Software"),
        ("Software - Infrastructure",                "Software"),
        ("Information Technology Services",          "IT Services"),
        // CSV has no comma: "Electronic Equipment Instruments & Components"
        ("Electronic Components",                    "Electronic Equipment Instruments & Components"),
        ("Electronic Equipment & Instruments",       "Electronic Equipment Instruments & Components"),

        // ── Banks / Finance ───────────────────────────────────────────────
        ("Banks - Regional",                         "Banks"),
        ("Banks - Diversified",                      "Banks"),
        ("Asset Management",                         "Capital Markets"),
        ("Financial Data & Stock Exchanges",         "Capital Markets"),
        ("Credit Services",                          "Consumer Finance"),
        ("Mortgage Finance",                         "Thrifts & Mortgage Finance"),

        // ── Insurance ─────────────────────────────────────────────────────
        ("Insurance - Diversified",                  "Insurance"),
        ("Insurance - Life",                         "Insurance"),
        ("Insurance—Diversified",                    "Insurance"),
        ("Insurance—Life",                           "Insurance"),
        ("Insurance - Property & Casualty",          "Insurance"),

        // ── Energy — CSV has "Oil Gas & Consumable Fuels" (no comma) ──────
        ("Oil & Gas Refining & Marketing",           "Oil Gas & Consumable Fuels"),
        ("Oil & Gas E&P",                            "Oil Gas & Consumable Fuels"),
        ("Oil & Gas Integrated",                     "Oil Gas & Consumable Fuels"),
        ("Oil & Gas Midstream",                      "Oil Gas & Consumable Fuels"),
        ("Oil & Gas Drilling",                       "Energy Equipment & Services"),
        ("Oil & Gas Equipment & Services",           "Energy Equipment & Services"),

        // ── Materials ─────────────────────────────────────────────────────
        ("Steel",                                    "Metals & Mining"),
        ("Copper",                                   "Metals & Mining"),
        ("Aluminum",                                 "Metals & Mining"),
        ("Gold",                                     "Metals & Mining"),
        ("Silver",                                   "Metals & Mining"),
        ("Other Precious Metals & Mining",           "Metals & Mining"),
        ("Building Materials",                       "Construction Materials"),
        ("Cement",                                   "Construction Materials"),

        // ── Consumer / Food ───────────────────────────────────────────────
        ("Auto Manufacturers",                       "Automobiles"),
        ("Auto Parts",                               "Automobile Components"),
        ("Household & Personal Products",            "Household Products"),
        ("Personal Products",                        "Personal Care Products"),
        ("Packaged Foods",                           "Food Products"),
        ("Agricultural Inputs",                      "Chemicals"),
        ("Beverages - Non-Alcoholic",                "Beverages"),
        ("Beverages - Alcoholic",                    "Beverages"),
        ("Beverages - Brewers",                      "Beverages"),
        ("Tobacco Products & Accessories",           "Tobacco"),
        ("Discount Stores",                          "Broadline Retail"),
        ("Department Stores",                        "Broadline Retail"),
        ("Apparel Retail",                           "Specialty Retail"),
        ("Apparel Manufacturing",                    "Textiles Apparel & Luxury Goods"),
        ("Luxury Goods",                             "Textiles Apparel & Luxury Goods"),

        // ── Telecom ───────────────────────────────────────────────────────
        ("Telecom Services",                         "Diversified Telecommunication Services"),
        ("Communication Services",                   "Diversified Telecommunication Services"),
        ("Wireless Services",                        "Wireless Telecommunication Services"),

        // ── Healthcare ────────────────────────────────────────────────────
        ("Drug Manufacturers - General",             "Pharmaceuticals"),
        ("Drug Manufacturers - Specialty & Generic", "Pharmaceuticals"),
        ("Hospitals",                                "Health Care Providers & Services"),
        ("Medical Devices",                          "Health Care Equipment & Supplies"),
        ("Medical Instruments & Supplies",           "Health Care Equipment & Supplies"),
        ("Diagnostics & Research",                   "Life Sciences Tools & Services"),

        // ── Utilities ─────────────────────────────────────────────────────
        ("Utilities - Regulated Electric",           "Electric Utilities"),
        ("Utilities - Diversified",                  "Multi-Utilities"),
        ("Utilities - Regulated Gas",                "Gas Utilities"),
        ("Utilities - Regulated Water",              "Water Utilities"),
        ("Utilities - Renewable",                    "Independent Power and Renewable Electricity Producers"),

        // ── Industrials ───────────────────────────────────────────────────
        ("Engineering & Construction",               "Construction & Engineering"),
        ("Conglomerates",                            "Industrial Conglomerates"),
        ("Aerospace & Defense",                      "Aerospace & Defense"),
        ("Farm & Heavy Construction Machinery",      "Machinery"),
        ("Industrial Distribution",                  "Trading Companies & Distributors"),
        ("Staffing & Employment Services",           "Professional Services"),
        ("Trucking",                                 "Ground Transportation"),
        ("Airlines",                                 "Passenger Airlines"),
        ("Marine Shipping",                          "Marine Transportation"),
        ("Railroads",                                "Ground Transportation"),
        ("Waste Management",                         "Commercial Services & Supplies"),
    ];

    let yahoo_norm = yahoo.trim().to_lowercase();
    for &(yahoo_name, gics_name) in aliases {
        if yahoo_name.to_lowercase() == yahoo_norm {
            return self.taxonomy.industry_by_name(gics_name);
        }
    }
    None
}
    /// After enrichment, re-group slots by their actual resolved industry codes.
    /// Slots whose industry_code is still 0 (enrichment failed) are dropped.
    fn regroup_by_gics(&self, universe: &mut Universe) {
        // Exclude staging bucket (code 0) from the set of valid target codes
        let selected_codes: std::collections::HashSet<u32> = universe
            .by_industry
            .keys()
            .copied()
            .filter(|&c| c != 0)
            .collect();

        let all_slots: Vec<CompanySlot> = universe
            .by_industry
            .drain()
            .flat_map(|(_, slots)| slots)
            .collect();

        let mut regrouped: HashMap<u32, Vec<CompanySlot>> = selected_codes
            .iter()
            .map(|&c| (c, Vec::new()))
            .collect();

        for slot in all_slots {
            if slot.industry_code == 0 {
                continue; // enrichment failed
            }
            if selected_codes.contains(&slot.industry_code) {
                regrouped
                    .entry(slot.industry_code)
                    .or_default()
                    .push(slot);
            }
        }

        universe.by_industry = regrouped;
    }

    // ── Ticker file loader ────────────────────────────────────────────────────

    fn load_ticker_file(&self, path: &str) -> Result<Vec<String>> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Cannot read ticker file: {}", path))?;

        let tickers: Vec<String> = content
            .lines()
            .map(|l| l.trim().to_uppercase())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();

        Ok(tickers)
    }
}

// ── Filter helpers ────────────────────────────────────────────────────────────

fn market_matches(exchange: &str, filter: &Market) -> bool {
    match filter {
        Market::Both => true,
        Market::NSE => {
            matches!(exchange.to_uppercase().as_str(), "NSI" | "NSE" | "BSE")
        }
        Market::NYSE => {
            matches!(
                exchange.to_uppercase().as_str(),
                "NYQ" | "NMS" | "NYSE" | "NGM" | "NCM" | "NASDAQ"
            )
        }
    }
}

fn cap_matches(tier: &Option<MarketCap>, filter: &CapFilter) -> bool {
    match filter {
        CapFilter::Mixed => true,
        CapFilter::SmallCap => matches!(tier, Some(MarketCap::SmallCap)),
        CapFilter::MidCap => matches!(tier, Some(MarketCap::MidCap)),
        CapFilter::LargeCap => matches!(tier, Some(MarketCap::LargeCap)),
    }
}