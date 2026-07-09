use crate::models::{OptionSummary, OptionTopOfBook, SpotBook};
use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde::Deserialize;

const DERIBIT_BASE: &str = "https://www.deribit.com/api/v2/public";
const BINANCE_BASE: &str = "https://api.binance.com";

pub async fn fetch_binance_book(
    client: &Client,
    underlying: &str,
    symbol: &str,
) -> Result<SpotBook> {
    let url = format!("{BINANCE_BASE}/api/v3/ticker/bookTicker");
    let raw = client
        .get(url)
        .query(&[("symbol", symbol)])
        .send()
        .await
        .context("request Binance bookTicker")?
        .error_for_status()
        .context("Binance bookTicker status")?
        .json::<BinanceBookTicker>()
        .await
        .context("decode Binance bookTicker")?;

    Ok(SpotBook {
        underlying: underlying.to_string(),
        symbol: raw.symbol,
        bid_price: parse_decimal(&raw.bid_price, "Binance bidPrice")?,
        bid_qty: parse_decimal(&raw.bid_qty, "Binance bidQty")?,
        ask_price: parse_decimal(&raw.ask_price, "Binance askPrice")?,
        ask_qty: parse_decimal(&raw.ask_qty, "Binance askQty")?,
    })
}

pub async fn fetch_deribit_option_summaries(
    client: &Client,
    currency: &str,
) -> Result<Vec<OptionSummary>> {
    let url = format!("{DERIBIT_BASE}/get_book_summary_by_currency");
    let response = client
        .get(url)
        .query(&[("currency", currency), ("kind", "option")])
        .send()
        .await
        .context("request Deribit book summary")?
        .error_for_status()
        .context("Deribit book summary status")?
        .json::<DeribitSummaryResponse>()
        .await
        .context("decode Deribit book summary")?;

    Ok(response
        .result
        .into_iter()
        .map(|summary| OptionSummary {
            instrument_name: summary.instrument_name,
            bid_price: positive_price(summary.bid_price),
            ask_price: positive_price(summary.ask_price),
            open_interest: summary.open_interest,
            volume: summary.volume,
        })
        .collect())
}

pub async fn fetch_deribit_top_of_book(
    client: &Client,
    instrument_name: &str,
) -> Result<OptionTopOfBook> {
    let url = format!("{DERIBIT_BASE}/get_order_book");
    let response = client
        .get(url)
        .query(&[("instrument_name", instrument_name), ("depth", "1")])
        .send()
        .await
        .context("request Deribit order book")?
        .error_for_status()
        .context("Deribit order book status")?
        .json::<DeribitOrderBookResponse>()
        .await
        .context("decode Deribit order book")?;

    let bid = response.result.bids.first().copied();
    let ask = response.result.asks.first().copied();

    Ok(OptionTopOfBook {
        instrument_name: response.result.instrument_name,
        bid_price: bid.map(|level| level[0]).or(response.result.best_bid_price),
        ask_price: ask.map(|level| level[0]).or(response.result.best_ask_price),
        bid_amount: bid
            .map(|level| level[1])
            .or(response.result.best_bid_amount)
            .unwrap_or(0.0),
        ask_amount: ask
            .map(|level| level[1])
            .or(response.result.best_ask_amount)
            .unwrap_or(0.0),
        open_interest: response.result.open_interest,
        volume: response.result.stats.and_then(|stats| stats.volume),
    })
}

fn parse_decimal(raw: &str, label: &str) -> Result<f64> {
    let value = raw
        .parse::<f64>()
        .with_context(|| format!("parse {label}: {raw}"))?;

    if value.is_finite() && value >= 0.0 {
        Ok(value)
    } else {
        bail!("{label} is not a finite positive number: {value}");
    }
}

fn positive_price(value: Option<f64>) -> Option<f64> {
    value.filter(|price| price.is_finite() && *price > 0.0)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BinanceBookTicker {
    symbol: String,
    bid_price: String,
    bid_qty: String,
    ask_price: String,
    ask_qty: String,
}

#[derive(Debug, Deserialize)]
struct DeribitSummaryResponse {
    result: Vec<DeribitSummary>,
}

#[derive(Debug, Deserialize)]
struct DeribitSummary {
    instrument_name: String,
    bid_price: Option<f64>,
    ask_price: Option<f64>,
    open_interest: Option<f64>,
    volume: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct DeribitOrderBookResponse {
    result: DeribitOrderBook,
}

#[derive(Debug, Deserialize)]
struct DeribitOrderBook {
    instrument_name: String,
    bids: Vec<[f64; 2]>,
    asks: Vec<[f64; 2]>,
    best_bid_price: Option<f64>,
    best_ask_price: Option<f64>,
    best_bid_amount: Option<f64>,
    best_ask_amount: Option<f64>,
    open_interest: Option<f64>,
    stats: Option<DeribitStats>,
}

#[derive(Debug, Deserialize)]
struct DeribitStats {
    volume: Option<f64>,
}
