//! 运行配置：全部从环境变量读取，带合理默认值。
use std::env;

#[derive(Clone, Debug)]
pub struct Config {
    /// 标的代号（出现在期权符号前缀），默认 "BTC"。
    pub underlying: String,
    /// 期权 REST 基地址。
    pub eapi_base: String,
    /// 期权 WS 基地址（market /ws 端点会在此基础上拼接）。
    pub options_ws: String,
    /// 现货 WS 基地址。
    pub spot_ws: String,

    /// 记录门槛：利润率（profit/spot）下限。默认 0 = 记录所有正利润机会。
    pub min_profit_rate: f64,
    /// 记录门槛：净年化利润率下限。默认 10%。
    pub min_annual_rate: f64,
    /// 记录门槛：顶档可执行量下限（BTC）。
    pub min_exec_qty: f64,
    /// 单腿手续费率。默认万分之一，按每条腿成交价格分别扣除（开仓 3 腿 + 平仓 3 腿）。
    pub fee_rate: f64,
    /// 平仓折扣系数：往返净收益率 ≥ 系数 × 开仓锁定率 时平仓。默认 0.8（打 8 折）。
    pub close_rate_discount: f64,
    /// 到期 ITM 期权腿行权费率。默认万分之 1.5。
    pub exercise_fee_rate: f64,
    /// 扫描间隔（毫秒）。
    pub scan_interval_ms: u64,
    /// 同一 (类型,到期,执行价) 落库节流间隔（毫秒）。
    pub record_interval_ms: i64,
    /// exchangeInfo 轮询间隔（秒）。
    pub info_poll_secs: u64,

    /// Web 服务端口。
    pub web_port: u16,
    /// SQLite 文件路径。
    pub db_path: String,
}

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_str(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

impl Config {
    pub fn from_env() -> Self {
        Config {
            underlying: env_str("OA_UNDERLYING", "BTC"),
            eapi_base: env_str("OA_EAPI_BASE", "https://eapi.binance.com"),
            options_ws: env_str("OA_OPTIONS_WS", "wss://fstream.binance.com/market"),
            spot_ws: env_str("OA_SPOT_WS", "wss://stream.binance.com:9443"),

            min_profit_rate: env_or("OA_MIN_PROFIT_RATE", 0.0),
            min_annual_rate: env_or("OA_MIN_ANNUAL_RATE", 0.10),
            min_exec_qty: env_or("OA_MIN_EXEC_QTY", 0.0),
            fee_rate: env_or("OA_FEE_RATE", 0.0001),
            close_rate_discount: env_or("OA_CLOSE_RATE_DISCOUNT", 0.8),
            exercise_fee_rate: env_or("OA_EXERCISE_FEE_RATE", 0.00015),
            scan_interval_ms: env_or("OA_SCAN_INTERVAL_MS", 250),
            record_interval_ms: env_or("OA_RECORD_INTERVAL_MS", 5000),
            info_poll_secs: env_or("OA_INFO_POLL_SECS", 300),

            web_port: env_or("OA_WEB_PORT", 8080),
            db_path: env_str("OA_DB_PATH", "data.db"),
        }
    }

    /// 现货交易对，例如 "BTCUSDT"。
    pub fn spot_symbol(&self) -> String {
        format!("{}USDT", self.underlying)
    }
}
