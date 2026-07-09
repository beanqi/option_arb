use serde::{Deserialize, Serialize};
use std::{env, time::Duration};

pub const UNDERLYINGS: &[(&str, &str)] = &[("BTC", "BTCUSDT"), ("ETH", "ETHUSDT")];
pub const DERIBIT_OPTION_FEE_UNDERLYING: f64 = 0.0003;
pub const DERIBIT_OPTION_PREMIUM_CAP_RATE: f64 = 0.125;

#[derive(Clone, Debug)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub poll_interval: Duration,
    pub spot_fee_rate: f64,
    pub min_annualized: f64,
    pub max_candidates: usize,
    pub history_path: String,
    pub history_max_records: usize,
}

impl Config {
    pub fn from_env() -> Self {
        let poll_secs = read_env("OPTION_ARB_POLL_SECS", 15_u64).max(5);
        let legacy_fee_rate = read_env("OPTION_ARB_FEE_RATE", 0.00024_f64);

        Self {
            host: env::var("OPTION_ARB_HOST").unwrap_or_else(|_| "0.0.0.0".to_string()),
            port: read_env("OPTION_ARB_PORT", 8080_u16),
            poll_interval: Duration::from_secs(poll_secs),
            spot_fee_rate: read_env("OPTION_ARB_SPOT_FEE_RATE", legacy_fee_rate),
            min_annualized: read_env("OPTION_ARB_MIN_ANNUALIZED", 0.10_f64),
            max_candidates: read_env("OPTION_ARB_MAX_CANDIDATES", 40_usize),
            history_path: env::var("OPTION_ARB_HISTORY_PATH")
                .unwrap_or_else(|_| "data/opportunities.jsonl".to_string()),
            history_max_records: read_env("OPTION_ARB_HISTORY_MAX_RECORDS", 500_usize),
        }
    }

    pub fn view(&self) -> ConfigView {
        ConfigView {
            poll_secs: self.poll_interval.as_secs(),
            fee_rate: self.spot_fee_rate,
            spot_fee_rate: self.spot_fee_rate,
            deribit_option_fee_underlying: DERIBIT_OPTION_FEE_UNDERLYING,
            deribit_option_premium_cap_rate: DERIBIT_OPTION_PREMIUM_CAP_RATE,
            min_annualized: self.min_annualized,
            max_candidates: self.max_candidates,
            history_max_records: self.history_max_records,
        }
    }
}

fn read_env<T>(key: &str, default: T) -> T
where
    T: std::str::FromStr,
{
    env::var(key)
        .ok()
        .and_then(|raw| raw.parse::<T>().ok())
        .unwrap_or(default)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConfigView {
    pub poll_secs: u64,
    pub fee_rate: f64,
    pub spot_fee_rate: f64,
    pub deribit_option_fee_underlying: f64,
    pub deribit_option_premium_cap_rate: f64,
    pub min_annualized: f64,
    pub max_candidates: usize,
    pub history_max_records: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub generated_at: String,
    pub status: String,
    pub config: ConfigView,
    pub markets: Vec<MarketView>,
    pub opportunities: Vec<Opportunity>,
    pub errors: Vec<String>,
}

impl Snapshot {
    pub fn warming(config: &Config) -> Self {
        Self {
            generated_at: String::new(),
            status: "warming_up".to_string(),
            config: config.view(),
            markets: Vec::new(),
            opportunities: Vec::new(),
            errors: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MarketView {
    pub underlying: String,
    pub spot_symbol: String,
    pub spot_bid: f64,
    pub spot_ask: f64,
    pub spot_bid_qty: f64,
    pub spot_ask_qty: f64,
    pub option_summaries: usize,
    pub rough_candidates: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Opportunity {
    pub id: String,
    pub underlying: String,
    pub strategy: Strategy,
    pub expiry: String,
    pub expiry_utc: String,
    pub days_to_expiry: f64,
    pub strike: f64,
    pub quantity: f64,
    pub notional_usd: f64,
    pub gross_profit_usd: f64,
    pub fees_usd: f64,
    pub net_profit_usd: f64,
    pub net_profit_per_unit_usd: f64,
    pub profit_rate: f64,
    pub annualized_profit_rate: f64,
    pub spot_bid: f64,
    pub spot_ask: f64,
    pub call: OptionBookView,
    pub put: OptionBookView,
    pub legs: Vec<OrderLeg>,
    pub note: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OpportunityRecord {
    pub record_id: String,
    pub recorded_at: String,
    pub snapshot_generated_at: String,
    pub min_annualized: f64,
    pub opportunity: Opportunity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    Conversion,
    Reversal,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OptionBookView {
    pub instrument: String,
    pub bid_price: Option<f64>,
    pub ask_price: Option<f64>,
    pub bid_amount: f64,
    pub ask_amount: f64,
    pub open_interest: Option<f64>,
    pub volume: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OrderLeg {
    pub venue: String,
    pub instrument: String,
    pub action: String,
    pub price: f64,
    pub price_unit: String,
    pub quantity: f64,
    pub available_quantity: f64,
    pub notional_usd: f64,
    pub fee_usd: f64,
}

#[derive(Clone, Debug)]
pub struct SpotBook {
    pub underlying: String,
    pub symbol: String,
    pub bid_price: f64,
    pub bid_qty: f64,
    pub ask_price: f64,
    pub ask_qty: f64,
}

#[derive(Clone, Debug)]
pub struct OptionSummary {
    pub instrument_name: String,
    pub bid_price: Option<f64>,
    pub ask_price: Option<f64>,
    pub open_interest: Option<f64>,
    pub volume: Option<f64>,
}

#[derive(Clone, Debug)]
pub struct OptionTopOfBook {
    pub instrument_name: String,
    pub bid_price: Option<f64>,
    pub ask_price: Option<f64>,
    pub bid_amount: f64,
    pub ask_amount: f64,
    pub open_interest: Option<f64>,
    pub volume: Option<f64>,
}
