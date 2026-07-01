//! Binance 期权 REST：拉 exchangeInfo，构建可交易期权宇宙。
use crate::model::{strike_key, ExpiryEntry, StrikePair, Universe};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Deserialize)]
struct ExchangeInfo {
    #[serde(rename = "optionSymbols")]
    option_symbols: Vec<OptionSymbol>,
}

#[derive(Debug, Deserialize)]
struct OptionSymbol {
    symbol: String,
    side: String, // "CALL" | "PUT"
    #[serde(rename = "strikePrice")]
    strike_price: String,
    #[serde(rename = "expiryDate")]
    expiry_date: i64, // 毫秒
    #[serde(default)]
    status: String, // "TRADING" 等
}

/// 拉取 exchangeInfo 并构建指定标的（如 "BTC"）的可交易期权宇宙。
pub async fn fetch_universe(
    client: &reqwest::Client,
    eapi_base: &str,
    underlying: &str,
) -> Result<Universe> {
    let url = format!("{eapi_base}/eapi/v1/exchangeInfo");
    let info: ExchangeInfo = client
        .get(&url)
        .send()
        .await
        .context("请求 exchangeInfo 失败")?
        .error_for_status()
        .context("exchangeInfo 返回错误状态")?
        .json()
        .await
        .context("解析 exchangeInfo JSON 失败")?;

    Ok(build_universe(&info.option_symbols, underlying))
}

/// 从 optionSymbols 列表构建指定标的的期权宇宙（与网络解耦，便于单测）。
fn build_universe(option_symbols: &[OptionSymbol], underlying: &str) -> Universe {
    let prefix = format!("{underlying}-");
    let mut expiries: BTreeMap<String, ExpiryEntry> = BTreeMap::new();
    for s in option_symbols {
        if !s.symbol.starts_with(&prefix) || s.status != "TRADING" {
            continue;
        }
        // 符号形如 BTC-260925-145000-C
        let parts: Vec<&str> = s.symbol.split('-').collect();
        if parts.len() != 4 {
            continue;
        }
        let expiry = parts[1].to_string();
        let strike: f64 = match s.strike_price.parse() {
            Ok(v) => v,
            Err(_) => continue,
        };

        let entry = expiries.entry(expiry).or_insert_with(|| ExpiryEntry {
            expiry_ms: s.expiry_date,
            strikes: BTreeMap::new(),
        });
        let pair = entry
            .strikes
            .entry(strike_key(strike))
            .or_insert_with(|| StrikePair {
                strike,
                call: None,
                put: None,
            });
        match s.side.as_str() {
            "CALL" => pair.call = Some(s.symbol.clone()),
            "PUT" => pair.put = Some(s.symbol.clone()),
            _ => {}
        }
    }

    Universe { expiries }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(symbol: &str, side: &str, strike: &str) -> OptionSymbol {
        OptionSymbol {
            symbol: symbol.to_string(),
            side: side.to_string(),
            strike_price: strike.to_string(),
            expiry_date: 1_790_000_000_000,
            status: "TRADING".to_string(),
        }
    }

    #[test]
    fn pairs_call_and_put_filters_non_trading_and_other_underlying() {
        let mut symbols = vec![
            sym("BTC-260925-145000-C", "CALL", "145000"),
            sym("BTC-260925-145000-P", "PUT", "145000"),
            sym("ETH-260925-3000-C", "CALL", "3000"), // 非目标标的，应忽略
        ];
        let mut expired = sym("BTC-260925-150000-C", "CALL", "150000");
        expired.status = "EXPIRED".to_string(); // 非 TRADING，应忽略
        symbols.push(expired);

        let uni = build_universe(&symbols, "BTC");
        assert_eq!(uni.expiries.len(), 1);
        let entry = &uni.expiries["260925"];
        assert_eq!(entry.strikes.len(), 1);
        let pair = &entry.strikes[&strike_key(145000.0)];
        assert_eq!(pair.call.as_deref(), Some("BTC-260925-145000-C"));
        assert_eq!(pair.put.as_deref(), Some("BTC-260925-145000-P"));
    }
}
