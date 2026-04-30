use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

// ── GICS hierarchy types ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sector {
    pub code: u32,       // e.g. 10
    pub name: String,    // e.g. "Energy"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndustryGroup {
    pub code: u32,       // e.g. 1010
    pub name: String,    // e.g. "Energy"
    pub sector_code: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Industry {
    pub code: u32,               // e.g. 101010
    pub name: String,            // e.g. "Energy Equipment & Services"
    pub industry_group_code: u32,
    pub sector_code: u32,
}

// ── CSV row shape (matches data/gics.csv) ────────────────────────────────────

#[derive(Debug, Deserialize)]
struct GicsRow {
    sector_code:          u32,
    sector_name:          String,
    industry_group_code:  u32,
    industry_group_name:  String,
    industry_code:        u32,
    industry_name:        String,
}

// ── Taxonomy ──────────────────────────────────────────────────────────────────

/// The full GICS hierarchy loaded into memory.
/// Lookup is O(1) by code; iteration is stable (insertion order).
#[derive(Debug)]
pub struct GicsTaxonomy {
    pub sectors:         Vec<Sector>,
    pub industry_groups: Vec<IndustryGroup>,
    pub industries:      Vec<Industry>,

    // Fast lookup maps
    sector_by_code:   HashMap<u32, usize>,   // code → index into sectors
    industry_by_code: HashMap<u32, usize>,   // code → index into industries
    industry_by_name: HashMap<String, usize>, // normalised name → index
}

impl GicsTaxonomy {
    /// Load from `data/gics.csv`.
    /// Expected columns (no header quoting required):
    ///   sector_code, sector_name, industry_group_code, industry_group_name,
    ///   industry_code, industry_name
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let mut rdr = csv::ReaderBuilder::new()
            .trim(csv::Trim::All)
            .from_path(path.as_ref())
            .with_context(|| format!("Cannot open GICS CSV at {:?}", path.as_ref()))?;

        let mut sectors: Vec<Sector> = Vec::new();
        let mut industry_groups: Vec<IndustryGroup> = Vec::new();
        let mut industries: Vec<Industry> = Vec::new();

        let mut seen_sectors: HashMap<u32, ()> = HashMap::new();
        let mut seen_groups: HashMap<u32, ()> = HashMap::new();

        for result in rdr.deserialize::<GicsRow>() {
            let row: GicsRow = result.context("Malformed GICS CSV row")?;
            // Deduplicate sectors
            if seen_sectors.insert(row.sector_code, ()).is_none() {
                sectors.push(Sector {
                    code: row.sector_code,
                    name: row.sector_name.clone(),
                });
            }

            // Deduplicate industry groups
            if seen_groups.insert(row.industry_group_code, ()).is_none() {
                industry_groups.push(IndustryGroup {
                    code: row.industry_group_code,
                    name: row.industry_group_name.clone(),
                    sector_code: row.sector_code,
                });
            }

            // Industries are always unique rows in the CSV
            industries.push(Industry {
                code: row.industry_code,
                name: row.industry_name.clone(),
                industry_group_code: row.industry_group_code,
                sector_code: row.sector_code,
            });
        }

        // Build lookup maps
        let sector_by_code = sectors
            .iter()
            .enumerate()
            .map(|(i, s)| (s.code, i))
            .collect();

        let industry_by_code = industries
            .iter()
            .enumerate()
            .map(|(i, ind)| (ind.code, i))
            .collect();

        let industry_by_name = industries
            .iter()
            .enumerate()
            .map(|(i, ind)| (normalise(&ind.name), i))
            .collect();

        Ok(Self {
            sectors,
            industry_groups,
            industries,
            sector_by_code,
            industry_by_code,
            industry_by_name,
        })
    }

    // ── Lookups ───────────────────────────────────────────────────────────────

    pub fn sector_by_code(&self, code: u32) -> Option<&Sector> {
        self.sector_by_code.get(&code).map(|&i| &self.sectors[i])
    }

    pub fn industry_by_code(&self, code: u32) -> Option<&Industry> {
        self.industry_by_code.get(&code).map(|&i| &self.industries[i])
    }

    /// Case-insensitive, whitespace-normalised name lookup.
    pub fn industry_by_name(&self, name: &str) -> Option<&Industry> {
        self.industry_by_name
            .get(&normalise(name))
            .map(|&i| &self.industries[i])
    }

    // ── Filters ───────────────────────────────────────────────────────────────

    /// All industries belonging to a sector.
    pub fn industries_in_sector(&self, sector_code: u32) -> Vec<&Industry> {
        self.industries
            .iter()
            .filter(|i| i.sector_code == sector_code)
            .collect()
    }

    /// All industries belonging to an industry group.
    pub fn industries_in_group(&self, group_code: u32) -> Vec<&Industry> {
        self.industries
            .iter()
            .filter(|i| i.industry_group_code == group_code)
            .collect()
    }

    /// Resolve a loose string (name or numeric code) to an Industry.
    /// Used when parsing CLI --exclude-industries arguments.
    pub fn resolve(&self, input: &str) -> Option<&Industry> {
        // Try numeric code first
        if let Ok(code) = input.trim().parse::<u32>() {
            return self.industry_by_code(code);
        }
        // Fall back to name lookup
        self.industry_by_name(input)
    }

    /// Return up to `n` industries, optionally excluding a set by code.
    /// Order is stable (CSV insertion order) — deterministic across runs.
    pub fn pick_n(
        &self,
        n: usize,
        exclude_codes: &[u32],
    ) -> Vec<&Industry> {
        self.industries
            .iter()
            .filter(|i| !exclude_codes.contains(&i.code))
            .take(n)
            .collect()
    }

    pub fn industry_count(&self) -> usize {
        self.industries.len()
    }
}

/// Lowercase + collapse whitespace — used for name-based lookup
fn normalise(s: &str) -> String {
    s.trim().to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")
}