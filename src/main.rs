//! BTC 期权权利金套利监控（conversion / reversal）vs Binance 现货。
//! 仅监控+记录，不下单；默认扣手续费，不计借贷/资金成本。
mod binance;
mod config;
mod db;
mod model;
mod poller;
mod scan;
mod web;

use crate::binance::ws_options::SubCmd;
use crate::config::Config;
use crate::model::{Position, Quotes, Universe};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cfg = Config::from_env();

    // 先建表，保证 Web 与写线程有 schema 可用
    db::init(&cfg.db_path)?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;

    let quotes = Arc::new(Quotes::new());
    let universe = Arc::new(RwLock::new(Universe::default()));

    let (sub_tx, sub_rx) = mpsc::channel::<SubCmd>(128);
    let (opp_tx, opp_rx) = mpsc::channel::<Position>(10_000);

    // 数据库写线程
    db::spawn_writer(cfg.db_path.clone(), opp_rx);

    info!(
        "option_arb 启动: 标的={} 现货={} Web=http://127.0.0.1:{} 库={} 费率={} 平仓折扣={} 行权费率={} 最小净年化={:.2}%",
        cfg.underlying,
        cfg.spot_symbol(),
        cfg.web_port,
        cfg.db_path,
        cfg.fee_rate,
        cfg.close_rate_discount,
        cfg.exercise_fee_rate,
        cfg.min_annual_rate * 100.0
    );

    let h_opts = tokio::spawn(binance::ws_options::run(
        cfg.options_ws.clone(),
        quotes.clone(),
        sub_rx,
    ));
    let h_spot = tokio::spawn(binance::ws_spot::run(
        cfg.spot_ws.clone(),
        cfg.spot_symbol(),
        quotes.clone(),
    ));
    let h_poll = tokio::spawn(poller::run(
        cfg.clone(),
        client.clone(),
        universe.clone(),
        sub_tx.clone(),
    ));
    let h_scan = tokio::spawn(scan::scanner(
        cfg.clone(),
        universe.clone(),
        quotes.clone(),
        opp_tx.clone(),
    ));
    let h_web = tokio::spawn(web::run(cfg.web_port, cfg.db_path.clone()));

    tokio::select! {
        _ = tokio::signal::ctrl_c() => { info!("收到 Ctrl-C，退出"); }
        _ = h_opts => { info!("期权 WS 任务结束"); }
        _ = h_spot => { info!("现货 WS 任务结束"); }
        _ = h_poll => { info!("poller 任务结束"); }
        _ = h_scan => { info!("scanner 任务结束"); }
        _ = h_web  => { info!("web 任务结束"); }
    }

    Ok(())
}
