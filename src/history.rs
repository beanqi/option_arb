use crate::models::{Config, OpportunityRecord, Snapshot};
use anyhow::{Context, Result};
use chrono::Utc;
use std::{
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug)]
pub struct HistoryStore {
    path: PathBuf,
    max_records: usize,
}

impl HistoryStore {
    pub fn from_config(config: &Config) -> Self {
        Self {
            path: PathBuf::from(&config.history_path),
            max_records: config.history_max_records,
        }
    }

    pub fn load_recent(&self) -> Result<Vec<OpportunityRecord>> {
        load_recent(&self.path, self.max_records)
    }

    pub fn append_snapshot(&self, snapshot: &Snapshot) -> Result<Vec<OpportunityRecord>> {
        if snapshot.opportunities.is_empty() {
            return Ok(Vec::new());
        }

        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)
                .with_context(|| format!("create history directory {}", parent.display()))?;
        }

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("open history file {}", self.path.display()))?;

        let recorded_at = Utc::now().to_rfc3339();
        let records = snapshot
            .opportunities
            .iter()
            .map(|opportunity| OpportunityRecord {
                record_id: format!("{}:{}", snapshot.generated_at, opportunity.id),
                recorded_at: recorded_at.clone(),
                snapshot_generated_at: snapshot.generated_at.clone(),
                min_annualized: snapshot.config.min_annualized,
                opportunity: opportunity.clone(),
            })
            .collect::<Vec<_>>();

        for record in &records {
            serde_json::to_writer(&mut file, record)
                .with_context(|| format!("serialize history record {}", record.record_id))?;
            file.write_all(b"\n").context("write history newline")?;
        }
        file.flush().context("flush history file")?;

        Ok(records)
    }

    pub fn trim_to_limit(&self, records: &mut Vec<OpportunityRecord>) {
        trim_to_limit(records, self.max_records);
    }
}

fn load_recent(path: &Path, max_records: usize) -> Result<Vec<OpportunityRecord>> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let file =
        fs::File::open(path).with_context(|| format!("open history file {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut records = Vec::new();

    for (index, line) in reader.lines().enumerate() {
        let raw = line.with_context(|| format!("read history line {}", index + 1))?;
        if raw.trim().is_empty() {
            continue;
        }

        let record = serde_json::from_str::<OpportunityRecord>(&raw)
            .with_context(|| format!("parse history line {}", index + 1))?;
        records.push(record);
    }

    trim_to_limit(&mut records, max_records);
    Ok(records)
}

fn trim_to_limit(records: &mut Vec<OpportunityRecord>, max_records: usize) {
    if records.len() > max_records {
        records.drain(0..records.len() - max_records);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{ConfigView, Opportunity, OptionBookView, OrderLeg, Strategy};

    #[test]
    fn appends_records_and_loads_recent_limit() {
        let path = std::env::temp_dir().join(format!(
            "option-arb-history-{}-{}.jsonl",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let store = HistoryStore {
            path: path.clone(),
            max_records: 1,
        };

        store
            .append_snapshot(&snapshot(vec![opportunity("one"), opportunity("two")]))
            .unwrap();

        let records = store.load_recent().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].opportunity.id, "two");
        assert_eq!(records[0].min_annualized, 0.1);

        let _ = fs::remove_file(path);
    }

    fn snapshot(opportunities: Vec<Opportunity>) -> Snapshot {
        Snapshot {
            generated_at: "2026-07-08T00:00:00Z".to_string(),
            status: "ok".to_string(),
            config: ConfigView {
                poll_secs: 15,
                fee_rate: 0.00024,
                spot_fee_rate: 0.00024,
                deribit_option_fee_underlying: 0.0003,
                deribit_option_premium_cap_rate: 0.125,
                min_annualized: 0.1,
                max_candidates: 40,
                history_max_records: 1,
            },
            markets: Vec::new(),
            opportunities,
            errors: Vec::new(),
        }
    }

    fn opportunity(id: &str) -> Opportunity {
        Opportunity {
            id: id.to_string(),
            underlying: "BTC".to_string(),
            strategy: Strategy::Conversion,
            expiry: "25SEP26".to_string(),
            expiry_utc: "2026-09-25T08:00:00Z".to_string(),
            days_to_expiry: 80.0,
            strike: 100_000.0,
            quantity: 1.0,
            notional_usd: 100_000.0,
            gross_profit_usd: 150.0,
            fees_usd: 30.0,
            net_profit_usd: 120.0,
            net_profit_per_unit_usd: 120.0,
            profit_rate: 0.0012,
            annualized_profit_rate: 0.21,
            spot_bid: 99_990.0,
            spot_ask: 100_000.0,
            call: option_book("BTC-25SEP26-100000-C"),
            put: option_book("BTC-25SEP26-100000-P"),
            legs: vec![OrderLeg {
                venue: "Binance Spot".to_string(),
                instrument: "BTCUSDT".to_string(),
                action: "BUY".to_string(),
                price: 100_000.0,
                price_unit: "USDT".to_string(),
                quantity: 1.0,
                available_quantity: 1.0,
                notional_usd: 100_000.0,
                fee_usd: 24.0,
            }],
            note: "test".to_string(),
        }
    }

    fn option_book(instrument: &str) -> OptionBookView {
        OptionBookView {
            instrument: instrument.to_string(),
            bid_price: Some(0.01),
            ask_price: Some(0.011),
            bid_amount: 1.0,
            ask_amount: 1.0,
            open_interest: None,
            volume: None,
        }
    }
}
