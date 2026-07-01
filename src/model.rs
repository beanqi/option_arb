//! 核心数据类型与共享状态。
use dashmap::DashMap;
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// 当前 Unix 毫秒时间戳。
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 把执行价转成可作为 BTreeMap key 的整数（保留 3 位小数精度）。
pub fn strike_key(strike: f64) -> i64 {
    (strike * 1000.0).round() as i64
}

/// 从 JSON 值解析 f64，兼容字符串（Binance 数字常以字符串下发）与数字。
pub fn parse_f64(v: &serde_json::Value) -> f64 {
    match v {
        serde_json::Value::String(s) => s.parse().unwrap_or(0.0),
        serde_json::Value::Number(n) => n.as_f64().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// 盘口顶档（最优买/卖价与量）。
#[derive(Clone, Copy, Debug, Default)]
pub struct BookTop {
    pub bid: f64,
    pub bid_qty: f64,
    pub ask: f64,
    pub ask_qty: f64,
    #[allow(dead_code)] // 行情时间戳：保留用于调试/未来的陈旧报价过滤
    pub ts_ms: i64,
}

impl BookTop {
    pub fn has_bid(&self) -> bool {
        self.bid > 0.0 && self.bid_qty > 0.0
    }
    pub fn has_ask(&self) -> bool {
        self.ask > 0.0 && self.ask_qty > 0.0
    }
}

/// 单个执行价对应的 call / put 合约符号。
#[derive(Clone, Debug, Default)]
pub struct StrikePair {
    pub strike: f64,
    pub call: Option<String>,
    pub put: Option<String>,
}

/// 单个到期日下的全部执行价。
#[derive(Clone, Debug, Default)]
pub struct ExpiryEntry {
    pub expiry_ms: i64,
    pub strikes: BTreeMap<i64, StrikePair>, // key = strike_key(strike)
}

/// 全部可交易期权的宇宙：到期日(YYMMDD) -> 执行价表。
#[derive(Clone, Debug, Default)]
pub struct Universe {
    pub expiries: BTreeMap<String, ExpiryEntry>,
}

/// 全局共享报价：期权按符号、现货单独存。
pub struct Quotes {
    pub options: DashMap<String, BookTop>,
    pub spot: RwLock<BookTop>,
}

impl Quotes {
    pub fn new() -> Self {
        Quotes {
            options: DashMap::new(),
            spot: RwLock::new(BookTop::default()),
        }
    }
    pub fn spot_snapshot(&self) -> BookTop {
        *self.spot.read().unwrap()
    }
}

/// 一笔完整套利仓位的生命周期记录:开仓 → (早平 | 到期结算)。
///
/// `kind` 为开仓方向(reversal/conversion),平仓则是同 (到期,执行价) 上的反向动作,
/// 行权价 K 在往返中自动抵消。往返净利 = open_gross + close_gross − 6 笔手续费。
#[derive(Clone, Debug, Serialize)]
pub struct Position {
    /// 唯一主键:"{kind}-{expiry}-{strike_key}-{open_ts}"。
    pub id: String,
    pub kind: &'static str,   // 开仓方向 "reversal" | "conversion"
    pub status: &'static str, // "open" | "closed" | "settled"
    pub expiry: String,
    pub expiry_ms: i64,
    pub strike: f64,
    pub call_symbol: String,
    pub put_symbol: String,

    // —— 开仓(落库后不可变)——
    pub open_ts: i64,
    pub open_spot: f64, // reversal: spot_bid; conversion: spot_ask
    pub open_call: f64, // reversal: call_ask; conversion: call_bid
    pub open_put: f64,  // reversal: put_bid;  conversion: put_ask
    pub open_gross: f64,
    pub open_fee: f64,
    pub open_net: f64,         // open_gross - open_fee
    pub open_rate: f64,        // 锁定目标净收益率 = open_net / open_spot
    pub open_annual_rate: f64, // 按到期天数年化
    pub target_close_rate: f64, // = close_rate_discount * open_rate

    // —— 平仓 / 结算(open 时为 0)——
    pub close_ts: i64,
    pub close_spot: f64,
    pub close_call: f64,
    pub close_put: f64,
    pub close_gross: f64,
    pub close_fee: f64,

    // —— 已实现(closed/settled)/ 当前盯市(open)——
    pub exec_qty: f64,
    pub net_profit_per_btc: f64, // 往返净利/BTC(open 时为当前盯市值)
    pub net_profit_rate: f64,    // net_profit_per_btc / open_spot
    pub hold_annual_rate: f64,   // 按实际持仓天数年化(open 时沿用 open_annual_rate)
    pub est_profit: f64,         // net_profit_per_btc * exec_qty
}
