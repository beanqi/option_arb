//! exchangeInfo 轮询：定期刷新可交易期权宇宙，并确保期权 WS 订阅标的级
//! `<underlyingPair>@optionMarkPrice` 流（自动适配交割与新挂牌）。
use crate::binance::rest;
use crate::binance::ws_options::SubCmd;
use crate::config::Config;
use crate::model::Universe;
use std::collections::HashSet;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::interval;
use tracing::{info, warn};

fn option_mark_price_stream(spot_symbol: &str) -> String {
    format!("{}@optionMarkPrice", spot_symbol.to_lowercase())
}

pub async fn run(
    cfg: Config,
    client: reqwest::Client,
    universe: Arc<RwLock<Universe>>,
    cmd_tx: mpsc::Sender<SubCmd>,
) {
    let mut prev_streams: HashSet<String> = HashSet::new();
    let mut tick = interval(Duration::from_secs(cfg.info_poll_secs));

    loop {
        tick.tick().await; // 首次立即触发，之后按间隔
        match rest::fetch_universe(&client, &cfg.eapi_base, &cfg.underlying).await {
            Ok(uni) => {
                let new_streams: HashSet<String> = if uni.expiries.is_empty() {
                    HashSet::new()
                } else {
                    std::iter::once(option_mark_price_stream(&cfg.spot_symbol())).collect()
                };

                let to_sub: Vec<String> = new_streams.difference(&prev_streams).cloned().collect();
                let to_unsub: Vec<String> =
                    prev_streams.difference(&new_streams).cloned().collect();

                let total_pairs: usize = uni.expiries.values().map(|e| e.strikes.len()).sum();
                if !to_sub.is_empty() || !to_unsub.is_empty() {
                    info!(
                        "期权宇宙更新: 到期日 {} 个 / 执行价档 {} 个；新增订阅 {}，退订 {}",
                        uni.expiries.len(),
                        total_pairs,
                        to_sub.len(),
                        to_unsub.len()
                    );
                    for s in &to_sub {
                        info!("  + 期权行情订阅 {s}");
                    }
                    for s in &to_unsub {
                        info!("  - 期权行情退订 {s}");
                    }
                    if cmd_tx
                        .send(SubCmd {
                            subscribe: to_sub,
                            unsubscribe: to_unsub,
                        })
                        .await
                        .is_err()
                    {
                        warn!("订阅指令通道已关闭，poller 退出");
                        return;
                    }
                    prev_streams = new_streams;
                }

                *universe.write().unwrap() = uni;
            }
            Err(e) => warn!(error = %e, "拉取 exchangeInfo 失败，下一轮重试"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_mark_price_stream_uses_lower_underlying_pair() {
        assert_eq!(
            option_mark_price_stream("BTCUSDT"),
            "btcusdt@optionMarkPrice"
        );
    }
}
