mod arb;
mod exchanges;
mod history;
mod models;
mod web;

use crate::{
    arb::{build_opportunity, find_rough_candidates},
    exchanges::{fetch_binance_book, fetch_deribit_option_summaries, fetch_deribit_top_of_book},
    history::HistoryStore,
    models::{
        Config, MarketView, OpportunityRecord, OptionTopOfBook, Snapshot, SpotBook, UNDERLYINGS,
    },
    web::AppState,
};
use anyhow::{Context, Result};
use axum::Router;
use chrono::Utc;
use reqwest::Client;
use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};
use tokio::{net::TcpListener, sync::RwLock, time};

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::from_env();
    let client = Client::builder()
        .user_agent("option-arb-monitor/0.1")
        .timeout(Duration::from_secs(10))
        .build()
        .context("build HTTP client")?;

    let snapshot = Arc::new(RwLock::new(Snapshot::warming(&config)));
    let history_store = Arc::new(HistoryStore::from_config(&config));
    let history_records = match history_store.load_recent() {
        Ok(records) => records,
        Err(error) => {
            eprintln!("history load failed: {error:#}");
            Vec::new()
        }
    };
    let history = Arc::new(RwLock::new(history_records));
    refresh_snapshot(&config, &client, &snapshot, &history_store, &history).await;

    let poll_config = config.clone();
    let poll_client = client.clone();
    let poll_snapshot = Arc::clone(&snapshot);
    let poll_history_store = Arc::clone(&history_store);
    let poll_history = Arc::clone(&history);
    tokio::spawn(async move {
        run_poller(
            poll_config,
            poll_client,
            poll_snapshot,
            poll_history_store,
            poll_history,
        )
        .await;
    });

    let app = web::router(AppState { snapshot, history });
    serve(config, app).await
}

async fn serve(config: Config, app: Router) -> Result<()> {
    let addr: SocketAddr = format!("{}:{}", config.host, config.port)
        .parse()
        .context("parse listen address")?;
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;

    println!("dashboard: http://{addr}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serve dashboard")
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

async fn run_poller(
    config: Config,
    client: Client,
    snapshot: Arc<RwLock<Snapshot>>,
    history_store: Arc<HistoryStore>,
    history: Arc<RwLock<Vec<OpportunityRecord>>>,
) {
    let mut interval = time::interval(config.poll_interval);
    interval.tick().await;

    loop {
        interval.tick().await;
        refresh_snapshot(&config, &client, &snapshot, &history_store, &history).await;
    }
}

async fn refresh_snapshot(
    config: &Config,
    client: &Client,
    state: &Arc<RwLock<Snapshot>>,
    history_store: &Arc<HistoryStore>,
    history: &Arc<RwLock<Vec<OpportunityRecord>>>,
) {
    let mut snapshot = collect_snapshot(config, client).await;
    if !snapshot.opportunities.is_empty() {
        match history_store.append_snapshot(&snapshot) {
            Ok(records) => {
                let mut history = history.write().await;
                history.extend(records);
                history_store.trim_to_limit(&mut history);
            }
            Err(error) => {
                snapshot.status = "partial".to_string();
                snapshot
                    .errors
                    .push(format!("history append failed: {error:#}"));
            }
        }
    }

    *state.write().await = snapshot;
}

async fn collect_snapshot(config: &Config, client: &Client) -> Snapshot {
    let now = Utc::now();
    let mut errors = Vec::new();
    let mut markets = Vec::new();
    let mut opportunities = Vec::new();

    for (underlying, symbol) in UNDERLYINGS {
        let spot = match fetch_binance_book(client, underlying, symbol).await {
            Ok(spot) => spot,
            Err(error) => {
                errors.push(format!("{} Binance {}: {error:#}", underlying, symbol));
                continue;
            }
        };

        let summaries = match fetch_deribit_option_summaries(client, underlying).await {
            Ok(summaries) => summaries,
            Err(error) => {
                errors.push(format!("{underlying} Deribit summaries: {error:#}"));
                markets.push(market_view(&spot, 0, 0));
                continue;
            }
        };

        let rough_candidates = find_rough_candidates(config, &spot, &summaries, now);
        markets.push(market_view(&spot, summaries.len(), rough_candidates.len()));
        let selected = rough_candidates
            .into_iter()
            .take(config.max_candidates)
            .collect::<Vec<_>>();

        let mut books = HashMap::<String, OptionTopOfBook>::new();
        for instrument in selected
            .iter()
            .flat_map(|candidate| {
                [
                    candidate.call.instrument_name.clone(),
                    candidate.put.instrument_name.clone(),
                ]
            })
            .collect::<std::collections::HashSet<_>>()
        {
            match fetch_deribit_top_of_book(client, &instrument).await {
                Ok(book) => {
                    books.insert(instrument, book);
                }
                Err(error) => {
                    errors.push(format!("Deribit order book {instrument}: {error:#}"));
                }
            }
        }

        for candidate in selected {
            let Some(call_book) = books.get(&candidate.call.instrument_name) else {
                continue;
            };
            let Some(put_book) = books.get(&candidate.put.instrument_name) else {
                continue;
            };

            if let Some(opportunity) =
                build_opportunity(config, &candidate, &spot, call_book, put_book, now)
            {
                opportunities.push(opportunity);
            }
        }
    }

    opportunities.sort_by(|left, right| {
        right
            .annualized_profit_rate
            .partial_cmp(&left.annualized_profit_rate)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Snapshot {
        generated_at: now.to_rfc3339(),
        status: if errors.is_empty() {
            "ok".to_string()
        } else {
            "partial".to_string()
        },
        config: config.view(),
        markets,
        opportunities,
        errors,
    }
}

fn market_view(spot: &SpotBook, option_summaries: usize, rough_candidates: usize) -> MarketView {
    MarketView {
        underlying: spot.underlying.clone(),
        spot_symbol: spot.symbol.clone(),
        spot_bid: spot.bid_price,
        spot_ask: spot.ask_price,
        spot_bid_qty: spot.bid_qty,
        spot_ask_qty: spot.ask_qty,
        option_summaries,
        rough_candidates,
    }
}
