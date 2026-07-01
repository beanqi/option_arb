//! 期权 WebSocket：连 fstream market，订阅 `<underlyingPair>@optionMarkPrice` 流，
//! 解析推送的期权报价数组（含 bo/ao/bq/aq 顶档买卖价量）写入共享报价表；断线自动重连并重订阅。
use crate::model::{parse_f64, BookTop, Quotes};
use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use std::collections::HashSet;
use std::sync::{Arc, Once};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

static FIRST_OPT: Once = Once::new();

/// 订阅增量指令：由 InfoPoller 在目标行情流变化时下发。
#[derive(Debug, Clone)]
pub struct SubCmd {
    pub subscribe: Vec<String>,
    pub unsubscribe: Vec<String>,
}

/// 把 SUBSCRIBE/UNSUBSCRIBE 拆成不超过 50 个 params 的若干帧。
fn sub_frames(method: &str, params: &[String], id: &mut u64) -> Vec<Message> {
    let mut frames = Vec::new();
    for chunk in params.chunks(50) {
        let payload = serde_json::json!({ "method": method, "params": chunk, "id": *id });
        *id += 1;
        frames.push(Message::Text(payload.to_string().into()));
    }
    frames
}

/// 解析一条推送消息（可能是报价数组、单条报价、或订阅 ack），更新报价表。
fn handle_msg(data: &[u8], quotes: &Quotes) {
    let v: serde_json::Value = match serde_json::from_slice(data) {
        Ok(v) => v,
        Err(_) => return,
    };
    match v {
        serde_json::Value::Array(arr) => {
            FIRST_OPT.call_once(|| info!("首次收到期权行情，本批 {} 条报价", arr.len()));
            for t in &arr {
                apply_ticker(t, quotes);
            }
        }
        serde_json::Value::Object(ref map) => {
            // 组合流包装 {"stream":..,"data":[..]}
            if let Some(data) = map.get("data") {
                if let Some(arr) = data.as_array() {
                    for t in arr {
                        apply_ticker(t, quotes);
                    }
                } else {
                    apply_ticker(data, quotes);
                }
            } else if map.contains_key("e") {
                apply_ticker(&v, quotes);
            }
            // 否则是订阅 ack（{"result":null,"id":..}），忽略。
        }
        _ => {}
    }
}

fn apply_ticker(t: &serde_json::Value, quotes: &Quotes) {
    let sym = match t.get("s").and_then(|x| x.as_str()) {
        Some(s) => s.to_string(),
        None => return,
    };
    let top = BookTop {
        bid: t.get("bo").map(parse_f64).unwrap_or(0.0),
        bid_qty: t.get("bq").map(parse_f64).unwrap_or(0.0),
        ask: t.get("ao").map(parse_f64).unwrap_or(0.0),
        ask_qty: t.get("aq").map(parse_f64).unwrap_or(0.0),
        ts_ms: t.get("E").and_then(|x| x.as_i64()).unwrap_or(0),
    };
    quotes.options.insert(sym, top);
}

/// 单次连接生命周期：返回 Ok(true)=断开需重连，Ok(false)=收到关闭信号需退出。
async fn connect_once(
    url: &str,
    quotes: &Arc<Quotes>,
    cmd_rx: &mut mpsc::Receiver<SubCmd>,
    current: &mut HashSet<String>,
    id: &mut u64,
) -> Result<bool> {
    let (ws, _) = connect_async(url).await?;
    info!("期权 WS 已连接，重订阅 {} 个流", current.len());
    let (mut write, mut read) = ws.split();

    if !current.is_empty() {
        let subs: Vec<String> = current.iter().cloned().collect();
        for f in sub_frames("SUBSCRIBE", &subs, id) {
            write.send(f).await?;
        }
    }

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { return Ok(false); };
                for s in &cmd.subscribe { current.insert(s.clone()); }
                for s in &cmd.unsubscribe { current.remove(s); }
                for f in sub_frames("SUBSCRIBE", &cmd.subscribe, id) { write.send(f).await?; }
                for f in sub_frames("UNSUBSCRIBE", &cmd.unsubscribe, id) { write.send(f).await?; }
            }
            msg = read.next() => {
                let Some(msg) = msg else { return Ok(true); };
                let msg = msg?;
                if msg.is_ping() {
                    write.send(Message::Pong(msg.into_data())).await?;
                } else if msg.is_close() {
                    return Ok(true);
                } else if msg.is_text() || msg.is_binary() {
                    handle_msg(&msg.into_data(), quotes);
                }
            }
        }
    }
}

/// 期权 WS 主循环。
pub async fn run(options_ws: String, quotes: Arc<Quotes>, mut cmd_rx: mpsc::Receiver<SubCmd>) {
    let url = format!("{}/ws", options_ws.trim_end_matches('/'));
    let mut current: HashSet<String> = HashSet::new();
    let mut id: u64 = 1;
    loop {
        match connect_once(&url, &quotes, &mut cmd_rx, &mut current, &mut id).await {
            Ok(false) => {
                info!("期权 WS 收到退出信号");
                return;
            }
            Ok(true) => warn!("期权 WS 断开，3 秒后重连"),
            Err(e) => warn!(error = %e, "期权 WS 异常，3 秒后重连"),
        }
        sleep(Duration::from_secs(3)).await;
    }
}
