use crate::models::{
    Config, DERIBIT_OPTION_FEE_UNDERLYING, DERIBIT_OPTION_PREMIUM_CAP_RATE, Opportunity,
    OptionBookView, OptionSummary, OptionTopOfBook, OrderLeg, SpotBook, Strategy,
};
use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Utc};
use std::collections::HashMap;

const SECONDS_PER_YEAR: f64 = 365.0 * 24.0 * 60.0 * 60.0;

#[derive(Clone, Debug)]
pub struct Candidate {
    pub underlying: String,
    pub strategy: Strategy,
    pub expiry_code: String,
    pub expiry_utc: DateTime<Utc>,
    pub strike: f64,
    pub call: OptionSummary,
    pub put: OptionSummary,
    pub rough_annualized: f64,
}

pub fn find_rough_candidates(
    config: &Config,
    spot: &SpotBook,
    summaries: &[OptionSummary],
    now: DateTime<Utc>,
) -> Vec<Candidate> {
    let mut pairs: HashMap<PairKey, OptionPair> = HashMap::new();

    for summary in summaries {
        let Some(parsed) = parse_instrument_name(&summary.instrument_name) else {
            continue;
        };

        if parsed.underlying != spot.underlying || parsed.expiry_utc <= now || parsed.strike <= 0.0
        {
            continue;
        }

        let pair = pairs.entry(parsed.key()).or_insert_with(|| OptionPair {
            underlying: parsed.underlying.clone(),
            expiry_code: parsed.expiry_code.clone(),
            expiry_utc: parsed.expiry_utc,
            strike: parsed.strike,
            call: None,
            put: None,
        });

        match parsed.option_type {
            OptionType::Call => pair.call = Some(summary.clone()),
            OptionType::Put => pair.put = Some(summary.clone()),
        }
    }

    let mut candidates = Vec::new();

    for pair in pairs.into_values() {
        let (Some(call), Some(put)) = (pair.call.clone(), pair.put.clone()) else {
            continue;
        };

        if let Some(annualized) =
            rough_annualized_for(&pair, &call, &put, spot, config, Strategy::Conversion, now)
            && annualized >= config.min_annualized
        {
            candidates.push(Candidate {
                underlying: pair.underlying.clone(),
                strategy: Strategy::Conversion,
                expiry_code: pair.expiry_code.clone(),
                expiry_utc: pair.expiry_utc,
                strike: pair.strike,
                call: call.clone(),
                put: put.clone(),
                rough_annualized: annualized,
            });
        }

        if let Some(annualized) =
            rough_annualized_for(&pair, &call, &put, spot, config, Strategy::Reversal, now)
            && annualized >= config.min_annualized
        {
            candidates.push(Candidate {
                underlying: pair.underlying.clone(),
                strategy: Strategy::Reversal,
                expiry_code: pair.expiry_code.clone(),
                expiry_utc: pair.expiry_utc,
                strike: pair.strike,
                call,
                put,
                rough_annualized: annualized,
            });
        }
    }

    candidates.sort_by(|left, right| {
        right
            .rough_annualized
            .partial_cmp(&left.rough_annualized)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    candidates
}

pub fn build_opportunity(
    config: &Config,
    candidate: &Candidate,
    spot: &SpotBook,
    call_book: &OptionTopOfBook,
    put_book: &OptionTopOfBook,
    now: DateTime<Utc>,
) -> Option<Opportunity> {
    let seconds_to_expiry = (candidate.expiry_utc - now).num_seconds() as f64;
    if seconds_to_expiry <= 0.0 {
        return None;
    }

    let spot_reference = (spot.bid_price + spot.ask_price) / 2.0;
    let (math, quantity, legs, note) = match candidate.strategy {
        Strategy::Conversion => {
            let call_bid = call_book.bid_price?;
            let put_ask = put_book.ask_price?;
            let quantity = spot
                .ask_qty
                .min(call_book.bid_amount)
                .min(put_book.ask_amount);

            if quantity <= 0.0 {
                return None;
            }

            let call_premium_usd = call_bid * spot_reference;
            let put_premium_usd = put_ask * spot_reference;
            let spot_fee = spot.ask_price * config.spot_fee_rate;
            let call_fee = deribit_option_fee_per_unit_usd(call_bid, spot_reference);
            let put_fee = deribit_option_fee_per_unit_usd(put_ask, spot_reference);
            let fees = spot_fee + call_fee + put_fee;
            let gross = candidate.strike - spot.ask_price - put_premium_usd + call_premium_usd;
            let net = gross - fees;

            let legs = vec![
                OrderLeg {
                    venue: "Binance Spot".to_string(),
                    instrument: spot.symbol.clone(),
                    action: "BUY".to_string(),
                    price: spot.ask_price,
                    price_unit: "USDT".to_string(),
                    quantity,
                    available_quantity: spot.ask_qty,
                    notional_usd: spot.ask_price * quantity,
                    fee_usd: spot_fee * quantity,
                },
                OrderLeg {
                    venue: "Deribit Options".to_string(),
                    instrument: put_book.instrument_name.clone(),
                    action: "BUY PUT".to_string(),
                    price: put_ask,
                    price_unit: candidate.underlying.clone(),
                    quantity,
                    available_quantity: put_book.ask_amount,
                    notional_usd: put_premium_usd * quantity,
                    fee_usd: put_fee * quantity,
                },
                OrderLeg {
                    venue: "Deribit Options".to_string(),
                    instrument: call_book.instrument_name.clone(),
                    action: "SELL CALL".to_string(),
                    price: call_bid,
                    price_unit: candidate.underlying.clone(),
                    quantity,
                    available_quantity: call_book.bid_amount,
                    notional_usd: call_premium_usd * quantity,
                    fee_usd: call_fee * quantity,
                },
            ];

            (
                MathResult { gross, fees, net },
                quantity,
                legs,
                "Conversion: buy spot, buy put, sell call; covered short call is hedged by spot."
                    .to_string(),
            )
        }
        Strategy::Reversal => {
            let call_ask = call_book.ask_price?;
            let put_bid = put_book.bid_price?;
            let quantity = spot
                .bid_qty
                .min(call_book.ask_amount)
                .min(put_book.bid_amount);

            if quantity <= 0.0 {
                return None;
            }

            let call_premium_usd = call_ask * spot_reference;
            let put_premium_usd = put_bid * spot_reference;
            let spot_fee = spot.bid_price * config.spot_fee_rate;
            let call_fee = deribit_option_fee_per_unit_usd(call_ask, spot_reference);
            let put_fee = deribit_option_fee_per_unit_usd(put_bid, spot_reference);
            let fees = spot_fee + call_fee + put_fee;
            let gross = spot.bid_price + put_premium_usd - call_premium_usd - candidate.strike;
            let net = gross - fees;

            let legs = vec![
                OrderLeg {
                    venue: "Binance Spot".to_string(),
                    instrument: spot.symbol.clone(),
                    action: "SELL / SHORT".to_string(),
                    price: spot.bid_price,
                    price_unit: "USDT".to_string(),
                    quantity,
                    available_quantity: spot.bid_qty,
                    notional_usd: spot.bid_price * quantity,
                    fee_usd: spot_fee * quantity,
                },
                OrderLeg {
                    venue: "Deribit Options".to_string(),
                    instrument: put_book.instrument_name.clone(),
                    action: "SELL PUT".to_string(),
                    price: put_bid,
                    price_unit: candidate.underlying.clone(),
                    quantity,
                    available_quantity: put_book.bid_amount,
                    notional_usd: put_premium_usd * quantity,
                    fee_usd: put_fee * quantity,
                },
                OrderLeg {
                    venue: "Deribit Options".to_string(),
                    instrument: call_book.instrument_name.clone(),
                    action: "BUY CALL".to_string(),
                    price: call_ask,
                    price_unit: candidate.underlying.clone(),
                    quantity,
                    available_quantity: call_book.ask_amount,
                    notional_usd: call_premium_usd * quantity,
                    fee_usd: call_fee * quantity,
                },
            ];

            (
                MathResult { gross, fees, net },
                quantity,
                legs,
                "Reversal needs existing spot inventory or a borrow/margin route; Binance spot alone does not create a short."
                    .to_string(),
            )
        }
    };

    let capital_per_unit = match candidate.strategy {
        Strategy::Conversion => spot.ask_price,
        Strategy::Reversal => spot.bid_price,
    };
    if capital_per_unit <= 0.0 {
        return None;
    }

    let profit_rate = math.net / capital_per_unit;
    let annualized_profit_rate = profit_rate * SECONDS_PER_YEAR / seconds_to_expiry;
    if annualized_profit_rate < config.min_annualized {
        return None;
    }

    let notional_usd = capital_per_unit * quantity;

    Some(Opportunity {
        id: format!(
            "{}-{}-{:.4}-{:?}",
            candidate.underlying, candidate.expiry_code, candidate.strike, candidate.strategy
        ),
        underlying: candidate.underlying.clone(),
        strategy: candidate.strategy,
        expiry: candidate.expiry_code.clone(),
        expiry_utc: candidate.expiry_utc.to_rfc3339(),
        days_to_expiry: seconds_to_expiry / 86_400.0,
        strike: candidate.strike,
        quantity,
        notional_usd,
        gross_profit_usd: math.gross * quantity,
        fees_usd: math.fees * quantity,
        net_profit_usd: math.net * quantity,
        net_profit_per_unit_usd: math.net,
        profit_rate,
        annualized_profit_rate,
        spot_bid: spot.bid_price,
        spot_ask: spot.ask_price,
        call: book_view(call_book, &candidate.call),
        put: book_view(put_book, &candidate.put),
        legs,
        note,
    })
}

fn rough_annualized_for(
    pair: &OptionPair,
    call: &OptionSummary,
    put: &OptionSummary,
    spot: &SpotBook,
    config: &Config,
    strategy: Strategy,
    now: DateTime<Utc>,
) -> Option<f64> {
    let seconds_to_expiry = (pair.expiry_utc - now).num_seconds() as f64;
    if seconds_to_expiry <= 0.0 {
        return None;
    }

    let spot_reference = (spot.bid_price + spot.ask_price) / 2.0;
    let (net, capital) = match strategy {
        Strategy::Conversion => {
            let call_bid_usd = call.bid_price? * spot_reference;
            let put_ask_usd = put.ask_price? * spot_reference;
            let fees = spot.ask_price * config.spot_fee_rate
                + deribit_option_fee_per_unit_usd(call.bid_price?, spot_reference)
                + deribit_option_fee_per_unit_usd(put.ask_price?, spot_reference);
            (
                pair.strike - spot.ask_price - put_ask_usd + call_bid_usd - fees,
                spot.ask_price,
            )
        }
        Strategy::Reversal => {
            let call_ask_usd = call.ask_price? * spot_reference;
            let put_bid_usd = put.bid_price? * spot_reference;
            let fees = spot.bid_price * config.spot_fee_rate
                + deribit_option_fee_per_unit_usd(call.ask_price?, spot_reference)
                + deribit_option_fee_per_unit_usd(put.bid_price?, spot_reference);
            (
                spot.bid_price + put_bid_usd - call_ask_usd - pair.strike - fees,
                spot.bid_price,
            )
        }
    };

    if capital <= 0.0 {
        return None;
    }

    Some((net / capital) * SECONDS_PER_YEAR / seconds_to_expiry)
}

fn deribit_option_fee_per_unit_usd(option_price_underlying: f64, spot_reference: f64) -> f64 {
    DERIBIT_OPTION_FEE_UNDERLYING.min(DERIBIT_OPTION_PREMIUM_CAP_RATE * option_price_underlying)
        * spot_reference
}

fn book_view(book: &OptionTopOfBook, summary: &OptionSummary) -> OptionBookView {
    OptionBookView {
        instrument: book.instrument_name.clone(),
        bid_price: book.bid_price,
        ask_price: book.ask_price,
        bid_amount: book.bid_amount,
        ask_amount: book.ask_amount,
        open_interest: book.open_interest.or(summary.open_interest),
        volume: book.volume.or(summary.volume),
    }
}

#[derive(Debug)]
struct MathResult {
    gross: f64,
    fees: f64,
    net: f64,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct PairKey {
    underlying: String,
    expiry_code: String,
    strike_text: String,
}

#[derive(Debug)]
struct OptionPair {
    underlying: String,
    expiry_code: String,
    expiry_utc: DateTime<Utc>,
    strike: f64,
    call: Option<OptionSummary>,
    put: Option<OptionSummary>,
}

#[derive(Debug)]
struct ParsedInstrument {
    underlying: String,
    expiry_code: String,
    expiry_utc: DateTime<Utc>,
    strike: f64,
    strike_text: String,
    option_type: OptionType,
}

impl ParsedInstrument {
    fn key(&self) -> PairKey {
        PairKey {
            underlying: self.underlying.clone(),
            expiry_code: self.expiry_code.clone(),
            strike_text: self.strike_text.clone(),
        }
    }
}

#[derive(Debug)]
enum OptionType {
    Call,
    Put,
}

fn parse_instrument_name(instrument: &str) -> Option<ParsedInstrument> {
    let mut parts = instrument.split('-');
    let underlying = parts.next()?.to_string();
    let expiry_code = parts.next()?.to_uppercase();
    let strike_text = parts.next()?.to_string();
    let kind = parts.next()?;
    if parts.next().is_some() {
        return None;
    }

    let option_type = match kind {
        "C" => OptionType::Call,
        "P" => OptionType::Put,
        _ => return None,
    };

    let strike = strike_text.parse::<f64>().ok()?;
    let expiry_date = NaiveDate::parse_from_str(&expiry_code, "%d%b%y").ok()?;
    let expiry_utc = Utc
        .with_ymd_and_hms(
            expiry_date.year(),
            expiry_date.month(),
            expiry_date.day(),
            8,
            0,
            0,
        )
        .single()?;

    Some(ParsedInstrument {
        underlying,
        expiry_code,
        expiry_utc,
        strike,
        strike_text,
        option_type,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Config, OptionTopOfBook, SpotBook};
    use chrono::Timelike;
    use std::time::Duration;

    fn config() -> Config {
        Config {
            host: "0.0.0.0".to_string(),
            port: 8080,
            poll_interval: Duration::from_secs(15),
            spot_fee_rate: 0.00024,
            min_annualized: 0.10,
            max_candidates: 40,
            history_path: "data/opportunities.jsonl".to_string(),
            history_max_records: 500,
        }
    }

    #[test]
    fn parses_deribit_option_name() {
        let parsed = parse_instrument_name("BTC-25SEP26-75000-C").unwrap();

        assert_eq!(parsed.underlying, "BTC");
        assert_eq!(parsed.expiry_code, "25SEP26");
        assert_eq!(parsed.strike, 75_000.0);
        assert!(matches!(parsed.option_type, OptionType::Call));
        assert_eq!(parsed.expiry_utc.hour(), 8);
    }

    #[test]
    fn computes_conversion_after_fees() {
        let cfg = config();
        let now = Utc.with_ymd_and_hms(2026, 7, 8, 0, 0, 0).unwrap();
        let expiry = Utc.with_ymd_and_hms(2026, 9, 25, 8, 0, 0).unwrap();
        let spot = SpotBook {
            underlying: "BTC".to_string(),
            symbol: "BTCUSDT".to_string(),
            bid_price: 62_990.0,
            bid_qty: 3.0,
            ask_price: 63_000.0,
            ask_qty: 2.0,
        };
        let candidate = Candidate {
            underlying: "BTC".to_string(),
            strategy: Strategy::Conversion,
            expiry_code: "25SEP26".to_string(),
            expiry_utc: expiry,
            strike: 64_000.0,
            call: OptionSummary {
                instrument_name: "BTC-25SEP26-64000-C".to_string(),
                bid_price: Some(0.0400),
                ask_price: Some(0.0410),
                open_interest: None,
                volume: None,
            },
            put: OptionSummary {
                instrument_name: "BTC-25SEP26-64000-P".to_string(),
                bid_price: Some(0.0020),
                ask_price: Some(0.0030),
                open_interest: None,
                volume: None,
            },
            rough_annualized: 1.0,
        };
        let call_book = OptionTopOfBook {
            instrument_name: candidate.call.instrument_name.clone(),
            bid_price: Some(0.0400),
            ask_price: Some(0.0410),
            bid_amount: 1.5,
            ask_amount: 1.0,
            open_interest: None,
            volume: None,
        };
        let put_book = OptionTopOfBook {
            instrument_name: candidate.put.instrument_name.clone(),
            bid_price: Some(0.0020),
            ask_price: Some(0.0030),
            bid_amount: 1.0,
            ask_amount: 1.2,
            open_interest: None,
            volume: None,
        };

        let opportunity =
            build_opportunity(&cfg, &candidate, &spot, &call_book, &put_book, now).unwrap();

        assert_eq!(opportunity.quantity, 1.2);
        assert!(opportunity.net_profit_per_unit_usd > 0.0);
        assert!(opportunity.annualized_profit_rate > cfg.min_annualized);
    }

    #[test]
    fn deribit_option_fee_uses_premium_cap() {
        let spot_reference = 100_000.0;

        assert!((deribit_option_fee_per_unit_usd(0.008, spot_reference) - 30.0).abs() < 1e-9);
        assert!((deribit_option_fee_per_unit_usd(0.002, spot_reference) - 25.0).abs() < 1e-9);
    }
}
