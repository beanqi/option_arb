# option_arb — BTC 期权权利金套利监控

实时监控 **Binance 期权**（全部到期日 × 全部执行价）与 **Binance 现货 BTCUSDT** 之间的
**conversion / reversal**（转换 / 反转）权利金套利机会，并把每笔机会当作一个**持仓生命周期**追踪:
**开仓 → 达标早平 / 到期结算**,记录到 SQLite 的 `positions` 表并提供网页查看。
**仅监控+记录,不下单;手续费按往返 6 笔(开仓 3 腿 + 平仓 3 腿)扣除,不计滑点/借贷/资金成本。**

## 策略

用「买 call + 卖 put」可合成一个行权价 K 的远期多头,与现货对冲后锁定价差。期权为
USDT **现金交割**、每张 = 1 BTC。对同一 `(到期,执行价)`,**平掉一笔 reversal 恰好等于做一次
conversion 动作**(反之亦然),K 在往返中自动抵消,故往返净利只需两条毛利公式相加再扣 6 笔手续费。

**开仓(锁定目标)** — 每 BTC 毛利:

| 方向 | 三条腿 | 每 BTC 毛利 | 顶档可执行量 |
|------|--------|-------------|--------------|
| **reversal** | 买 call(ask) + 卖 put(bid) + 卖现货(bid) | `spot_bid + put_bid − call_ask − K` | `min(call_ask量, put_bid量, spot_bid量)` |
| **conversion** | 卖 call(bid) + 买 put(ask) + 买现货(ask) | `call_bid − put_ask + K − spot_ask` | `min(call_bid量, put_ask量, spot_ask量)` |

- `open_fee = OA_FEE_RATE ×(spot + call + put)`;`open_net = open_gross − open_fee`
- `锁定率 open_rate = open_net / open_spot`;`年化 = open_rate × 365 / 到期天数`
- 开仓过滤沿用 `OA_MIN_PROFIT_RATE / OA_MIN_ANNUAL_RATE / OA_MIN_EXEC_QTY`

**早平(反向动作)** — 平 reversal 用 conversion 侧盘口、平 conversion 用 reversal 侧盘口:

- `往返净利 = open_gross + close_gross − open_fee − close_fee`(**6 笔手续费,K 抵消**)
- `往返净利率 = 往返净利 / open_spot`
- **平仓条件**:`往返净利率 ≥ OA_CLOSE_RATE_DISCOUNT × open_rate`(默认 0.8,即锁定率打 8 折即平)

**到期结算(现金交割)** — 到期前未达标平仓则持有到交割:

- `结算净利 = open_gross − open_fee − OA_FEE_RATE×P − OA_EXERCISE_FEE_RATE×P`
  (`P` ≈ 现货中间价;现货腿按结算价平回 + ITM 期权腿扣行权费,近似,忽略 10% 封顶)

> reversal 需融币做空现货、conversion 需 USDT 买现货,借贷与资金成本、滑点均未计入;
> 期权手续费仍用单一 `OA_FEE_RATE` 近似(未建模「指数价×0.03% 且封顶权利金 10%」的精确规则)。

## 数据来源（均为公开接口，无需 API key）

- **期权行情**：WS `wss://fstream.binance.com/market/ws`，订阅
  `btcusdt@optionMarkPrice`，推送 BTCUSDT 标的下所有期权合约的顶档买卖价量（`bo/ao/bq/aq`）。
- **现货行情**：WS `wss://stream.binance.com:9443/ws/btcusdt@bookTicker`。
- **到期日发现 / 交割检测**：REST `GET /eapi/v1/exchangeInfo` 定时轮询，刷新可交易期权宇宙；
  行情订阅保持为标的级单流，长期运行自动适配新挂牌/已交割合约。

## 运行

```bash
cargo run --release
```

默认在 `http://127.0.0.1:8080` 提供网页，机会写入当前目录 `data.db`。

```bash
# 直接查库
sqlite3 data.db "SELECT kind,status,expiry,strike,open_rate,net_profit_rate,est_profit \
  FROM positions ORDER BY open_ts DESC LIMIT 20"
```

## 配置（环境变量，均有默认值）

| 变量 | 默认 | 说明 |
|------|------|------|
| `OA_UNDERLYING` | `BTC` | 标的代号 |
| `OA_MIN_PROFIT_RATE` | `0.0005` | 开仓门槛：扣费后开仓锁定率下限（默认万分之 5） |
| `OA_MIN_ANNUAL_RATE` | `0.10` | 开仓门槛：扣费后年化锁定率下限（0.10 = 10%） |
| `OA_MIN_EXEC_QTY` | `0.0` | 开仓门槛：可执行量下限（BTC） |
| `OA_FEE_RATE` | `0.0025` | 单腿手续费率，默认万分之 25（开/平各 3 腿共 6 笔） |
| `OA_CLOSE_RATE_DISCOUNT` | `0.8` | 平仓折扣：往返净利率 ≥ 系数 × 开仓锁定率 时平仓 |
| `OA_EXERCISE_FEE_RATE` | `0.00015` | 到期 ITM 期权腿行权费率 |
| `OA_SCAN_INTERVAL_MS` | `250` | 扫描间隔 |
| `OA_RECORD_INTERVAL_MS` | `5000` | 持仓盯市更新节流 + 同 key 平仓后再开仓节流 |
| `OA_INFO_POLL_SECS` | `300` | exchangeInfo 轮询间隔 |
| `OA_WEB_PORT` | `8080` | 网页端口 |
| `OA_DB_PATH` | `data.db` | SQLite 路径 |
| `OA_EAPI_BASE` | `https://eapi.binance.com` | 期权 REST 基址 |
| `OA_OPTIONS_WS` | `wss://fstream.binance.com/market` | 期权 WS 基址 |
| `OA_SPOT_WS` | `wss://stream.binance.com:9443` | 现货 WS 基址 |

## ⚠️ 地区限制

Binance 生产域名（`eapi.binance.com`、`fstream.binance.com`、`stream.binance.com`）对部分地区
可能返回 **HTTP 451**。请在 Binance 可访问的网络/地区，或经代理运行。
现货可用公开镜像 `wss://data-stream.binance.vision` / `https://data-api.binance.vision`
（期权无镜像）。

## 架构

tokio 多任务，`Arc<DashMap>` 共享报价：

- `binance::ws_options` — 期权 WS，动态 SUBSCRIBE/UNSUBSCRIBE，断线重连重订阅
- `binance::ws_spot` — 现货 WS bookTicker，断线重连
- `poller` — exchangeInfo 轮询，刷新期权宇宙并 diff 目标行情流下发订阅增量
- `scan` — 持仓追踪:每轮先对已开仓位判到期结算/达标平仓/盯市更新,再对空闲 (类型,到期,执行价) 建仓;启动时从库回填未平仓位(重启不丢追踪)
- `db` — 独立线程批量事务 UPSERT 写 `positions` 表(WAL,同 id 开仓→平仓覆盖);Web 只读查询与回填
- `web` — axum 提供页面与 JSON API（`/api/opportunities` 查持仓、`/api/stats`;支持 `status` 过滤）

## 测试

```bash
cargo test   # 套利公式、机会评估、DB 写入/查询往返
```
