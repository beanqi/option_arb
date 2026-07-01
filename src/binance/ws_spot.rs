//! 现货 WebSocket：订阅 `<symbol>@bookTicker`，更新现货顶档买卖价量；断线自动重连。
use crate::model::{parse_f64, BookTop, Quotes};
use futures_util::{SinkExt, StreamExt};
use std::sync::{Arc, Once};
use std::time::Duration;
use tokio::time::sleep;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

static FIRST_SPOT: Once = Once::new();

fn handle_spot(data: &[u8], quotes: &Quotes) {
    let v: serde_json::Value = match serde_json::from_slice(data) {
        Ok(v) => v,
        Err(_) => return,
    };
    // bookTicker: {"u":..,"s":"BTCUSDT","b":"bid","B":"bidQty","a":"ask","A":"askQty"}
    let obj = match v.as_object() {
        Some(o) => o,
        None => return,
    };
    if !obj.contains_key("b") || !obj.contains_key("a") {
        return;
    }
    let top = BookTop {
        bid: obj.get("b").map(parse_f64).unwrap_or(0.0),
        bid_qty: obj.get("B").map(parse_f64).unwrap_or(0.0),
        ask: obj.get("a").map(parse_f64).unwrap_or(0.0),
        ask_qty: obj.get("A").map(parse_f64).unwrap_or(0.0),
        ts_ms: crate::model::now_ms(),
    };
    FIRST_SPOT.call_once(|| info!("首次收到现货行情 bid={} ask={}", top.bid, top.ask));
    *quotes.spot.write().unwrap() = top;
}

/// 现货 WS 主循环。
pub async fn run(spot_ws: String, symbol: String, quotes: Arc<Quotes>) {
    let stream = format!("{}@bookTicker", symbol.to_lowercase());
    let url = format!("{}/ws/{}", spot_ws.trim_end_matches('/'), stream);
    loop {
        match connect_async(&url).await {
            Ok((ws, _)) => {
                info!("现货 WS 已连接: {}", stream);
                let (mut write, mut read) = ws.split();
                while let Some(msg) = read.next().await {
                    match msg {
                        Ok(m) => {
                            if m.is_ping() {
                                let _ = write.send(Message::Pong(m.into_data())).await;
                            } else if m.is_close() {
                                break;
                            } else if m.is_text() || m.is_binary() {
                                handle_spot(&m.into_data(), &quotes);
                            }
                        }
                        Err(e) => {
                            warn!(error = %e, "现货 WS 读错误");
                            break;
                        }
                    }
                }
            }
            Err(e) => warn!(error = %e, "现货 WS 连接失败"),
        }
        sleep(Duration::from_secs(3)).await;
    }
}
