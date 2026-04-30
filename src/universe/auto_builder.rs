use anyhow::Result;
use tracing::{info, warn};

use crate::data::cache::Cache;

pub struct AutoUniverseBuilder {
    cache: Cache,
}

impl AutoUniverseBuilder {
    pub fn new(cache: Cache) -> Self {
        Self { cache }
    }

    /// Returns combined Nifty500 + SP500 tickers, refreshing from source if the
    /// weekly cache is stale.
    pub async fn get_all_tickers(&self) -> Result<Vec<String>> {
        let nse = self.get_nifty500().await;
        let us  = self.get_sp500().await;

        let mut all = Vec::with_capacity(nse.len() + us.len());
        all.extend(nse);
        all.extend(us);
        all.dedup();

        info!("Auto-universe: {} tickers ({} NSE + {} US)", all.len(),
              all.iter().filter(|t| t.ends_with(".NS")).count(),
              all.iter().filter(|t| !t.ends_with(".NS")).count());

        Ok(all)
    }

    // ── Nifty 500 ─────────────────────────────────────────────────────────────

    async fn get_nifty500(&self) -> Vec<String> {
        if let Ok(Some(cached)) = self.cache.load_universe_cache("nse500", 7) {
            info!("Nifty500: using cached list ({} tickers)", cached.len());
            return cached;
        }
        let tickers = Self::nifty500_static();
        if let Err(e) = self.cache.save_universe_cache("nse500", &tickers) {
            warn!("Failed to cache Nifty500 list: {e}");
        }
        tickers
    }

    // ── S&P 500 ───────────────────────────────────────────────────────────────

    async fn get_sp500(&self) -> Vec<String> {
        if let Ok(Some(cached)) = self.cache.load_universe_cache("sp500", 7) {
            info!("SP500: using cached list ({} tickers)", cached.len());
            return cached;
        }

        let tickers = match Self::fetch_sp500_wikipedia().await {
            Some(t) if t.len() > 400 => {
                info!("SP500: fetched {} tickers from Wikipedia", t.len());
                t
            }
            _ => {
                warn!("SP500: Wikipedia fetch failed or too few results, using static fallback");
                Self::sp500_static()
            }
        };

        if let Err(e) = self.cache.save_universe_cache("sp500", &tickers) {
            warn!("Failed to cache SP500 list: {e}");
        }
        tickers
    }

    async fn fetch_sp500_wikipedia() -> Option<Vec<String>> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .user_agent("Mozilla/5.0 (compatible; portfolio-sim/1.0)")
            .build()
            .ok()?;

        let html = client
            .get("https://en.wikipedia.org/wiki/List_of_S%26P_500_companies")
            .send()
            .await
            .ok()?
            .text()
            .await
            .ok()?;

        let tickers = Self::parse_sp500_html(&html);
        if tickers.len() > 400 { Some(tickers) } else { None }
    }

    fn parse_sp500_html(html: &str) -> Vec<String> {
        // Locate the "constituents" table
        let table_start = match html.find("id=\"constituents\"") {
            Some(i) => i,
            None => return vec![],
        };
        let after = &html[table_start..];
        let table_end = after.find("</table>").unwrap_or(after.len());
        let table = &after[..table_end];

        let mut tickers = Vec::new();
        let mut pos = 0;

        while let Some(tr_off) = table[pos..].find("<tr") {
            let tr_abs = pos + tr_off;
            let row_end = table[tr_abs..].find("</tr>")
                .map(|i| tr_abs + i + 5)
                .unwrap_or_else(|| (tr_abs + 1000).min(table.len()));

            let row = &table[tr_abs..row_end.min(table.len())];

            // Skip header rows (contain <th>)
            if !row.contains("<th") {
                if let Some(td_off) = row.find("<td") {
                    if let Some(tag_end) = row[td_off..].find('>') {
                        let content_start = td_off + tag_end + 1;
                        if let Some(td_close) = row[content_start..].find("</td>") {
                            let cell = &row[content_start..content_start + td_close];
                            let ticker = Self::strip_tags(cell).trim().to_uppercase();
                            if Self::is_valid_us_ticker(&ticker) {
                                tickers.push(ticker);
                            }
                        }
                    }
                }
            }

            pos = row_end;
            if pos >= table.len() { break; }
        }

        tickers
    }

    fn strip_tags(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut in_tag = false;
        for c in s.chars() {
            match c {
                '<' => in_tag = true,
                '>' => in_tag = false,
                _ if !in_tag => out.push(c),
                _ => {}
            }
        }
        out
    }

    fn is_valid_us_ticker(s: &str) -> bool {
        !s.is_empty()
            && s.len() <= 6
            && s.chars().all(|c| c.is_ascii_uppercase() || c == '.' || c == '-')
            && s.chars().any(|c| c.is_ascii_uppercase())
            && s != "NYSE"
            && s != "NASDAQ"
    }

    // ── Static embedded Nifty 500 ─────────────────────────────────────────────

    fn nifty500_static() -> Vec<String> {
        [
            // ── Nifty 50 ─────────────────────────────────────────────────────
            "RELIANCE.NS", "TCS.NS", "HDFCBANK.NS", "ICICIBANK.NS", "INFY.NS",
            "HINDUNILVR.NS", "ITC.NS", "SBIN.NS", "BHARTIARTL.NS", "AXISBANK.NS",
            "KOTAKBANK.NS", "LT.NS", "WIPRO.NS", "ONGC.NS", "NTPC.NS",
            "POWERGRID.NS", "TATAMOTORS.NS", "SUNPHARMA.NS", "ULTRACEMCO.NS", "NESTLEIND.NS",
            "BAJFINANCE.NS", "BAJAJFINSV.NS", "TITAN.NS", "ASIANPAINT.NS", "HCLTECH.NS",
            "MARUTI.NS", "TECHM.NS", "INDUSINDBK.NS", "CIPLA.NS", "GRASIM.NS",
            "TATASTEEL.NS", "ADANIPORTS.NS", "EICHERMOT.NS", "DRREDDY.NS", "APOLLOHOSP.NS",
            "BPCL.NS", "HEROMOTOCO.NS", "BRITANNIA.NS", "DIVISLAB.NS", "COALINDIA.NS",
            "JSWSTEEL.NS", "HINDALCO.NS", "TATACONSUM.NS", "SBILIFE.NS", "HDFCLIFE.NS",
            "ADANIENT.NS", "BAJAJ-AUTO.NS", "UPL.NS", "SHRIRAMFIN.NS",
            // ── Nifty Next 50 / Nifty 100 ────────────────────────────────────
            "SIEMENS.NS", "ABB.NS", "VEDL.NS", "HINDZINC.NS", "PIDILITIND.NS",
            "DABUR.NS", "GODREJCP.NS", "MARICO.NS", "COLPAL.NS", "BERGEPAINT.NS",
            "AUROPHARMA.NS", "BALKRISIND.NS", "PAGEIND.NS", "HAVELLS.NS", "VOLTAS.NS",
            "BOSCHLTD.NS", "JUBLFOOD.NS", "TRENT.NS", "CUMMINSIND.NS", "LUPIN.NS",
            "CHOLAFIN.NS", "APOLLOTYRE.NS", "MRF.NS", "EXIDEIND.NS",
            "AMBUJACEM.NS", "SHREECEM.NS", "NMDC.NS", "SAIL.NS", "NATIONALUM.NS",
            "CONCOR.NS", "ADANIGREEN.NS", "TATAPOWER.NS", "TORNTPOWER.NS", "JSWENERGY.NS",
            "CESC.NS", "IEX.NS", "NHPC.NS", "SJVN.NS", "RECLTD.NS",
            "PFC.NS", "IRFC.NS", "IRCTC.NS", "LTIM.NS", "ICICIGI.NS",
            "SBICARD.NS", "BANDHANBNK.NS", "FEDERALBNK.NS", "BANKBARODA.NS", "AUBANK.NS",
            "OBEROIRLTY.NS", "GODREJPROP.NS", "DLF.NS", "BRIGADE.NS", "SOBHA.NS",
            "TATAELXSI.NS", "MPHASIS.NS", "COFORGE.NS", "PERSISTENT.NS", "LTTS.NS",
            "ZOMATO.NS", "NAUKRI.NS", "PAYTM.NS", "DMART.NS",
            "HDFCAMC.NS", "NIPPONLIFE.NS", "MUTHOOTFIN.NS", "BAJAJHLDNG.NS",
            "TORNTPHARM.NS", "ALKEM.NS", "IPCALAB.NS", "LICHSGFIN.NS",
            "CROMPTON.NS", "DIXON.NS", "POLYCAB.NS", "KEI.NS",
            "BHEL.NS", "BEL.NS", "HAL.NS", "MAZDOCK.NS",
            "ASTRAL.NS", "SUPREMEIND.NS", "RELAXO.NS", "BATA.NS",
            "KANSAINER.NS", "ATUL.NS", "DEEPAKNTR.NS", "SRF.NS",
            "LALPATHLAB.NS", "METROPOLIS.NS",
            "TATACHEM.NS", "GSPL.NS", "PETRONET.NS", "GAIL.NS",
            "IOC.NS", "HINDPETRO.NS", "MRPL.NS",
            "OFSS.NS", "KPITTECH.NS",
            "ZYDUSLIFE.NS", "IDFCFIRSTB.NS", "RBLBANK.NS",
            "MANAPPURAM.NS", "SUNDARMFIN.NS", "BAJAJCON.NS", "EMAMILTD.NS",
            "VBL.NS", "RADICO.NS",
            "HONAUT.NS", "SKFINDIA.NS", "TIINDIA.NS", "SCHAEFFLER.NS",
            "WELSPUNLIV.NS", "RAYMOND.NS",
            "SPANDANA.NS", "CREDITACC.NS", "UJJIVAN.NS", "EQUITAS.NS",
            "RATNAMANI.NS", "INDHOTEL.NS", "LEMONTRE.NS", "EIH.NS",
            "INTERGLOBE.NS", "BLUESTAR.NS", "PRINCEPIPE.NS",
            "CENTURYTEX.NS", "HINDCOPPER.NS", "MOIL.NS", "GMRINFRA.NS",
            "SUZLON.NS", "GLENMARK.NS", "ABCAPITAL.NS", "CANFINHOME.NS",
            "PNBHOUSING.NS", "GRINDWELL.NS", "BALKRISIND.NS", "AIAENG.NS",
            "AMBER.NS", "INDIGOPNTS.NS", "BERGEPAINT.NS", "PGHH.NS",
            "ABBOTINDIA.NS", "PFIZER.NS", "SANOFI.NS", "GLAXO.NS",
            "NATCOPHARM.NS", "STRIDES.NS", "GRANULES.NS", "SOLARA.NS",
            "UNIONBANK.NS", "CANBK.NS", "INDIANB.NS", "IOB.NS", "PSB.NS",
            "CDSL.NS", "BSE.NS", "MCX.NS", "ANGELONE.NS", "IIFL.NS",
            "MFIN.NS", "UGROCAP.NS", "FIVE-STAR.NS", "APTUS.NS",
            "INOXWIND.NS", "IREDA.NS", "NTPCGREEN.NS", "ADANIENSOL.NS",
            "SWIGGY.NS", "NYKAA.NS", "POLICYBZR.NS",
            "SHOPERSTOP.NS", "VMART.NS", "TATACOMM.NS", "HFCL.NS",
            "TATALOCOMO.NS", "ASHOKLEY.NS", "ESCORTS.NS", "SWARAJENG.NS",
            "CEAT.NS", "TVSMOTORS.NS", "BAJAJHLDNG.NS",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    // ── Static S&P 500 fallback ───────────────────────────────────────────────

    fn sp500_static() -> Vec<String> {
        [
            // Technology
            "AAPL", "MSFT", "NVDA", "GOOGL", "GOOG", "META", "AVGO", "ORCL", "ADBE", "AMD",
            "QCOM", "TXN", "INTC", "MU", "AMAT", "LRCX", "KLAC", "MRVL", "CDNS", "SNPS",
            "ANSS", "PTC", "CTSH", "ACN", "IBM", "HPQ", "HPE", "DELL", "INTU", "NOW",
            "WDAY", "CRM", "MSCI", "VRSK", "EPAM", "PAYC", "PANW", "CRWD", "ZS", "FTNT",
            "NET", "DDOG", "MDB", "SNOW", "PLTR", "RBLX", "U",
            // Communication Services
            "NFLX", "DIS", "CMCSA", "VZ", "T", "CHTR", "TMUS", "WBD",
            "IPG", "OMC", "EA", "TTWO", "MTCH",
            // Consumer Discretionary
            "AMZN", "TSLA", "HD", "MCD", "NKE", "SBUX", "LOW", "TGT", "ROST", "TJX",
            "BKNG", "CMG", "YUM", "DG", "DLTR", "BBY", "AZO", "ORLY",
            "F", "GM", "APTV", "WH", "HLT", "MAR", "MGM", "LVS",
            "AMZN", "EBAY", "ETSY", "W",
            // Consumer Staples
            "PG", "KO", "PEP", "COST", "WMT", "PM", "MO", "MDLZ", "KHC", "GIS",
            "K", "CPB", "HSY", "MKC", "SJM", "CAG", "HRL", "CL", "CHD", "KMB",
            "EL", "COTY", "CLX",
            // Financials
            "BRK-B", "JPM", "BAC", "WFC", "GS", "MS", "C", "BLK", "AXP", "SPGI",
            "ICE", "CME", "CB", "PGR", "TRV", "MET", "AIG", "HIG", "AFL", "ALL",
            "V", "MA", "PYPL", "COF", "DFS", "ALLY", "USB", "PNC", "TFC", "MTB",
            "HBAN", "KEY", "RF", "CFG", "FITB", "SCHW", "ETFC",
            // Healthcare
            "LLY", "UNH", "JNJ", "ABBV", "MRK", "PFE", "ABT", "TMO", "DHR", "BMY",
            "AMGN", "GILD", "BIIB", "REGN", "VRTX", "ISRG", "MDT", "BSX", "EW",
            "ZBH", "SYK", "BDX", "BAX", "HUM", "CVS", "CI", "MCK", "CAH", "ABC",
            "HOLX", "DXCM", "ALGN", "IDXX", "IQV", "A", "MTD",
            // Industrials
            "GE", "RTX", "HON", "BA", "LMT", "GD", "NOC", "LHX", "TDG", "HWM",
            "CAT", "DE", "EMR", "ETN", "PH", "ROK", "SWK", "DOV", "IR", "AME",
            "UPS", "FDX", "CSX", "NSC", "UNP", "GWW", "FAST", "XYL", "IEX",
            "WM", "RSG", "CTAS", "EXPD", "CH", "CHRW",
            // Energy
            "XOM", "CVX", "COP", "EOG", "OXY", "SLB", "HAL", "MPC", "VLO", "PSX",
            "PXD", "DVN", "HES", "MRO", "APA", "FANG", "KMI", "WMB", "OKE", "LNG",
            // Materials
            "LIN", "APD", "DD", "DOW", "EMN", "CE", "CF", "MOS", "NEM", "FCX",
            "AA", "PKG", "IP", "FMC", "ALB", "SHW", "RPM",
            // Real Estate
            "AMT", "PLD", "CCI", "EQIX", "PSA", "O", "WELL", "DLR", "EXR", "AVB",
            "EQR", "VTR", "VICI", "SPG", "CBRE", "ARE", "BXP",
            // Utilities
            "NEE", "DUK", "SO", "D", "AEP", "SRE", "EXC", "XEL", "WEC", "ES",
            "ED", "ETR", "FE", "EIX", "AWK",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }
}
