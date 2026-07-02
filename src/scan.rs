//! 持仓生命周期追踪(仅用一档盘口):开仓 → (达标早平 | 到期结算)。
//!
//! 同一 (到期,执行价) 上,平掉一笔 reversal 恰好等于做一次 conversion 动作(反之亦然),
//! 行权价 K 在往返中自动抵消,故复用 `reversal_profit`/`conversion_profit` 两条公式。
use crate::config::Config;
use crate::db;
use crate::model::{now_ms, strike_key, BookTop, Position, Quotes, Universe};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::interval;
use tracing::{info, warn};

/// Reversal:买 call、卖 put、卖现货。每 BTC 毛利。
pub fn reversal_profit(spot_bid: f64, put_bid: f64, call_ask: f64, strike: f64) -> f64 {
    spot_bid + put_bid - call_ask - strike
}

/// Conversion:卖 call、买 put、买现货。每 BTC 毛利。
pub fn conversion_profit(call_bid: f64, put_ask: f64, strike: f64, spot_ask: f64) -> f64 {
    call_bid - put_ask + strike - spot_ask
}

/// 把收益率按持有天数线性年化。
fn annualize_over_days(rate: f64, days: f64) -> f64 {
    if days > 0.0 {
        rate * 365.0 / days
    } else {
        0.0
    }
}

/// 年化收益率(按到期天数线性年化)。
fn annualize(rate: f64, expiry_ms: i64, now: i64) -> f64 {
    annualize_over_days(rate, (expiry_ms - now) as f64 / 86_400_000.0)
}

/// 三条腿手续费之和(每 BTC):现货腿按 `spot_rate`、call/put 腿按 `option_rate`。
fn trading_fee_per_btc(
    spot_price: f64,
    call_price: f64,
    put_price: f64,
    spot_rate: f64,
    option_rate: f64,
) -> f64 {
    spot_rate * spot_price + option_rate * (call_price + put_price)
}

/// 单侧建仓/平仓的一档腿信息(成交毛利、三腿成交价、可执行量)。
#[derive(Clone, Copy, Debug)]
struct Legs {
    gross: f64,
    spot: f64,
    call: f64,
    put: f64,
    qty: f64,
}

/// reversal 开仓腿(买 call@ask、卖 put@bid、卖现货@bid)。
fn open_reversal(call: &BookTop, put: &BookTop, spot: &BookTop, strike: f64) -> Option<Legs> {
    (call.has_ask() && put.has_bid() && spot.has_bid()).then(|| Legs {
        gross: reversal_profit(spot.bid, put.bid, call.ask, strike),
        spot: spot.bid,
        call: call.ask,
        put: put.bid,
        qty: call.ask_qty.min(put.bid_qty).min(spot.bid_qty),
    })
}

/// conversion 开仓腿(卖 call@bid、买 put@ask、买现货@ask)。
fn open_conversion(call: &BookTop, put: &BookTop, spot: &BookTop, strike: f64) -> Option<Legs> {
    (call.has_bid() && put.has_ask() && spot.has_ask()).then(|| Legs {
        gross: conversion_profit(call.bid, put.ask, strike, spot.ask),
        spot: spot.ask,
        call: call.bid,
        put: put.ask,
        qty: call.bid_qty.min(put.ask_qty).min(spot.ask_qty),
    })
}

/// 给定开仓方向,在当前盘口计算平仓(反向动作)腿。None = 反向腿盘口不全。
fn close_legs(kind: &str, call: &BookTop, put: &BookTop, spot: &BookTop, strike: f64) -> Option<Legs> {
    match kind {
        // 平 reversal = conversion 动作
        "reversal" => open_conversion(call, put, spot, strike),
        // 平 conversion = reversal 动作
        _ => open_reversal(call, put, spot, strike),
    }
}

/// 现金交割近似结算价:现货中间价(缺一侧则取另一侧)。
fn settlement_price(spot: &BookTop) -> f64 {
    match (spot.has_bid(), spot.has_ask()) {
        (true, true) => (spot.bid + spot.ask) / 2.0,
        (true, false) => spot.bid,
        (false, true) => spot.ask,
        _ => 0.0,
    }
}

fn pos_key(kind: &str, expiry: &str, strike: f64) -> String {
    format!("{}|{}|{}", kind, expiry, strike_key(strike))
}

/// 到期交割结算:现货腿按结算价平回 + ITM 期权腿扣行权费。
fn settle_position(pos: &Position, spot: &BookTop, now: i64, spot_fee_rate: f64, exercise_rate: f64) -> Position {
    let p = settlement_price(spot);
    let spot_close_fee = spot_fee_rate * p;
    let exercise_fee = exercise_rate * p; // 恰一条期权 ITM 被行权(近似,忽略 10% 封顶)
    let net = pos.open_gross - pos.open_fee - spot_close_fee - exercise_fee;
    let rate = net / pos.open_spot;
    let ann = annualize_over_days(rate, (now - pos.open_ts) as f64 / 86_400_000.0);
    let mut out = pos.clone();
    out.status = "settled";
    out.close_ts = now;
    out.close_spot = p;
    out.close_call = 0.0;
    out.close_put = 0.0;
    out.close_gross = 0.0;
    out.close_fee = spot_close_fee + exercise_fee;
    out.net_profit_per_btc = net;
    out.net_profit_rate = rate;
    out.hold_annual_rate = ann;
    out.est_profit = net * pos.exec_qty;
    out
}

fn hms(now_ms: i64) -> String {
    let secs = (now_ms / 1000) % 86_400;
    format!(
        "{:02}:{:02}:{:02} UTC",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

fn print_summary(now: i64, positions: &HashMap<String, Position>, opened: u32, closed: u32) {
    if positions.is_empty() && opened == 0 && closed == 0 {
        return;
    }
    let mut v: Vec<&Position> = positions.values().collect();
    v.sort_by(|a, b| {
        b.open_rate
            .partial_cmp(&a.open_rate)
            .unwrap_or(Ordering::Equal)
    });
    println!(
        "\n[{}] 持仓 {} 笔(本轮 +{} 开 / -{} 平)",
        hms(now),
        positions.len(),
        opened,
        closed
    );
    println!(
        "{:<11}{:<9}{:>10}{:>11}{:>12}{:>12}{:>10}",
        "类型", "到期", "执行价", "锁定率%", "目标平仓%", "当前往返%", "可执行BTC"
    );
    for p in v.iter().take(8) {
        println!(
            "{:<11}{:<9}{:>10.0}{:>11.4}{:>12.4}{:>12.4}{:>10.3}",
            p.kind,
            p.expiry,
            p.strike,
            p.open_rate * 100.0,
            p.target_close_rate * 100.0,
            p.net_profit_rate * 100.0,
            p.exec_qty
        );
    }
}

/// 扫描任务:每轮先处理已开仓位(到期结算 / 达标早平 / 盯市更新),
/// 再遍历宇宙对空闲 (类型,到期,执行价) 建仓。所有状态变更 emit 到 DB。
pub async fn scanner(
    cfg: Config,
    universe: Arc<RwLock<Universe>>,
    quotes: Arc<Quotes>,
    opp_tx: mpsc::Sender<Position>,
) {
    // 重启回填:把库里 status='open' 的仓位读回内存,长跑重启不丢追踪。
    let mut positions: HashMap<String, Position> = HashMap::new();
    match db::load_open_positions(&cfg.db_path) {
        Ok(v) => {
            for p in v {
                positions.insert(pos_key(p.kind, &p.expiry, p.strike), p);
            }
            if !positions.is_empty() {
                info!("从库回填未平仓位 {} 笔", positions.len());
            }
        }
        Err(e) => warn!(error = %e, "回填未平仓位失败"),
    }

    let mut last_open: HashMap<String, i64> = HashMap::new(); // key -> 上次开仓(用于平仓后再开节流)
    let mut last_emit: HashMap<String, i64> = HashMap::new(); // id  -> 上次盯市 emit
    let mut tick = interval(Duration::from_millis(cfg.scan_interval_ms));
    let print_every = (2000 / cfg.scan_interval_ms).max(1);
    let mut scan_count: u64 = 0;

    loop {
        tick.tick().await;
        let spot = quotes.spot_snapshot();
        if !spot.has_bid() && !spot.has_ask() {
            continue; // 现货行情未就绪
        }
        let now = now_ms();
        let mut opened: u32 = 0;
        let mut closed: u32 = 0;

        // —— 阶段 1:处理已开仓位(持仓自带 expiry_ms/strike/symbols,不依赖宇宙)——
        let keys: Vec<String> = positions.keys().cloned().collect();
        for key in keys {
            let pos = positions.get(&key).unwrap().clone();

            // 到期结算
            if now >= pos.expiry_ms {
                let settled = settle_position(&pos, &spot, now, cfg.spot_fee_rate, cfg.exercise_fee_rate);
                let _ = opp_tx.try_send(settled);
                positions.remove(&key);
                last_open.insert(key.clone(), now);
                last_emit.remove(&pos.id);
                closed += 1;
                continue;
            }

            // 早平评估:反向腿盘口齐全才算
            let call = quotes.options.get(&pos.call_symbol).map(|r| *r).unwrap_or_default();
            let put = quotes.options.get(&pos.put_symbol).map(|r| *r).unwrap_or_default();
            let Some(cl) = close_legs(pos.kind, &call, &put, &spot, pos.strike) else {
                continue;
            };
            let close_fee = trading_fee_per_btc(cl.spot, cl.call, cl.put, cfg.spot_fee_rate, cfg.option_fee_rate);
            let roundtrip_net = pos.open_gross + cl.gross - pos.open_fee - close_fee;
            let rate = roundtrip_net / pos.open_spot;

            if rate >= pos.target_close_rate {
                // 达标平仓
                let exec = pos.exec_qty.min(cl.qty);
                let ann = annualize_over_days(rate, (now - pos.open_ts) as f64 / 86_400_000.0);
                let mut done = pos.clone();
                done.status = "closed";
                done.close_ts = now;
                done.close_spot = cl.spot;
                done.close_call = cl.call;
                done.close_put = cl.put;
                done.close_gross = cl.gross;
                done.close_fee = close_fee;
                done.exec_qty = exec;
                done.net_profit_per_btc = roundtrip_net;
                done.net_profit_rate = rate;
                done.hold_annual_rate = ann;
                done.est_profit = roundtrip_net * exec;
                let _ = opp_tx.try_send(done);
                positions.remove(&key);
                last_open.insert(key.clone(), now);
                last_emit.remove(&pos.id);
                closed += 1;
            } else {
                // 未达标:节流盯市更新(便于 Web 观察逼近目标)
                let should = match last_emit.get(&pos.id) {
                    Some(&t) => now - t >= cfg.record_interval_ms,
                    None => true,
                };
                if should {
                    if let Some(m) = positions.get_mut(&key) {
                        m.net_profit_per_btc = roundtrip_net;
                        m.net_profit_rate = rate;
                        m.est_profit = roundtrip_net * m.exec_qty;
                        let _ = opp_tx.try_send(m.clone());
                    }
                    last_emit.insert(pos.id.clone(), now);
                }
            }
        }

        // —— 阶段 2:遍历宇宙,对空闲 key 建仓(读锁内收集,不跨 await)——
        let mut new_opens: Vec<Position> = Vec::new();
        {
            let uni = universe.read().unwrap();
            for (expiry, entry) in &uni.expiries {
                for pair in entry.strikes.values() {
                    let (Some(cs), Some(ps)) = (&pair.call, &pair.put) else {
                        continue;
                    };
                    let call = quotes.options.get(cs).map(|r| *r).unwrap_or_default();
                    let put = quotes.options.get(ps).map(|r| *r).unwrap_or_default();
                    for kind in ["reversal", "conversion"] {
                        let key = pos_key(kind, expiry, pair.strike);
                        if positions.contains_key(&key) {
                            continue; // 已有持仓
                        }
                        if let Some(&t) = last_open.get(&key) {
                            if now - t < cfg.record_interval_ms {
                                continue; // 平仓后再开节流
                            }
                        }
                        let Some(ol) = (match kind {
                            "reversal" => open_reversal(&call, &put, &spot, pair.strike),
                            _ => open_conversion(&call, &put, &spot, pair.strike),
                        }) else {
                            continue;
                        };
                        let open_fee = trading_fee_per_btc(ol.spot, ol.call, ol.put, cfg.spot_fee_rate, cfg.option_fee_rate);
                        let open_net = ol.gross - open_fee;
                        if open_net <= 0.0 {
                            continue;
                        }
                        let open_rate = open_net / ol.spot;
                        let open_annual = annualize(open_rate, entry.expiry_ms, now);
                        if open_rate < cfg.min_profit_rate
                            || open_annual < cfg.min_annual_rate
                            || ol.qty < cfg.min_exec_qty
                        {
                            continue;
                        }
                        // 当前往返盯市(初始 net_profit;开仓瞬间通常为负 = 点差成本)。
                        // 可平性闸门:反向腿盘口齐全时,若「立即平仓」的往返净率跌破 −max_open_mark_loss,
                        // 说明反向点差过宽、这机会根本平不掉,直接放弃开仓(避免开进来后拖到期的大额浮亏)。
                        let (mark_net, mark_rate) =
                            match close_legs(kind, &call, &put, &spot, pair.strike) {
                                Some(cl) => {
                                    let cf = trading_fee_per_btc(cl.spot, cl.call, cl.put, cfg.spot_fee_rate, cfg.option_fee_rate);
                                    let rn = ol.gross + cl.gross - open_fee - cf;
                                    let mr = rn / ol.spot;
                                    if mr < -cfg.max_open_mark_loss {
                                        continue; // 点差吃掉一切,平不掉
                                    }
                                    (rn, mr)
                                }
                                None => (open_net, open_rate),
                            };
                        new_opens.push(Position {
                            id: format!("{}-{}-{}-{}", kind, expiry, strike_key(pair.strike), now),
                            kind,
                            status: "open",
                            expiry: expiry.clone(),
                            expiry_ms: entry.expiry_ms,
                            strike: pair.strike,
                            call_symbol: cs.clone(),
                            put_symbol: ps.clone(),
                            open_ts: now,
                            open_spot: ol.spot,
                            open_call: ol.call,
                            open_put: ol.put,
                            open_gross: ol.gross,
                            open_fee,
                            open_net,
                            open_rate,
                            open_annual_rate: open_annual,
                            target_close_rate: cfg.close_rate_discount * open_rate,
                            close_ts: 0,
                            close_spot: 0.0,
                            close_call: 0.0,
                            close_put: 0.0,
                            close_gross: 0.0,
                            close_fee: 0.0,
                            exec_qty: ol.qty,
                            net_profit_per_btc: mark_net,
                            net_profit_rate: mark_rate,
                            hold_annual_rate: open_annual,
                            est_profit: mark_net * ol.qty,
                        });
                    }
                }
            }
        }
        for p in new_opens {
            let key = pos_key(p.kind, &p.expiry, p.strike);
            last_open.insert(key.clone(), now);
            last_emit.insert(p.id.clone(), now);
            let _ = opp_tx.try_send(p.clone());
            positions.insert(key, p);
            opened += 1;
        }

        scan_count += 1;
        if scan_count % print_every == 0 {
            print_summary(now, &positions, opened, closed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book(bid: f64, bid_qty: f64, ask: f64, ask_qty: f64) -> BookTop {
        BookTop {
            bid,
            bid_qty,
            ask,
            ask_qty,
            ts_ms: 0,
        }
    }

    #[test]
    fn reversal_formula() {
        // 现货卖 60000,put 卖 1200,call 买 1000,K=60000 → 60000+1200-1000-60000=200
        assert!((reversal_profit(60000.0, 1200.0, 1000.0, 60000.0) - 200.0).abs() < 1e-9);
    }

    #[test]
    fn conversion_formula() {
        // call 卖 1000,put 买 800,K=60000,现货买 60000 → 1000-800+60000-60000=200
        assert!((conversion_profit(1000.0, 800.0, 60000.0, 60000.0) - 200.0).abs() < 1e-9);
    }

    #[test]
    fn open_reversal_picks_gross_and_qty() {
        let call = book(950.0, 5.0, 1000.0, 2.0);
        let put = book(1200.0, 3.0, 1250.0, 4.0);
        let spot = book(60000.0, 1.5, 60001.0, 1.0);
        let ol = open_reversal(&call, &put, &spot, 60000.0).unwrap();
        assert!((ol.gross - 200.0).abs() < 1e-9);
        // 可执行量 = min(call.ask_qty=2, put.bid_qty=3, spot.bid_qty=1.5) = 1.5
        assert!((ol.qty - 1.5).abs() < 1e-9);
    }

    #[test]
    fn no_open_when_efficient() {
        // 紧贴盘口,扣费后开仓净利 <= 0
        let call = book(990.0, 1.0, 1010.0, 1.0);
        let put = book(990.0, 1.0, 1010.0, 1.0);
        let spot = book(60000.0, 1.0, 60001.0, 1.0);
        let (sf, of) = (0.00025, 0.0025);
        let rev = open_reversal(&call, &put, &spot, 60000.0).unwrap();
        let conv = open_conversion(&call, &put, &spot, 60000.0).unwrap();
        assert!(rev.gross - trading_fee_per_btc(rev.spot, rev.call, rev.put, sf, of) <= 0.0);
        assert!(conv.gross - trading_fee_per_btc(conv.spot, conv.call, conv.put, sf, of) <= 0.0);
    }

    #[test]
    fn open_fee_deducted() {
        let call = book(950.0, 5.0, 1000.0, 2.0);
        let put = book(1200.0, 3.0, 1250.0, 4.0);
        let spot = book(60000.0, 1.5, 60001.0, 1.0);
        let ol = open_reversal(&call, &put, &spot, 60000.0).unwrap();
        let (sf, of) = (0.00025, 0.0025);
        let fee = trading_fee_per_btc(ol.spot, ol.call, ol.put, sf, of);
        let expected_fee = 60000.0 * sf + (1000.0 + 1200.0) * of;
        assert!((fee - expected_fee).abs() < 1e-9);
        assert!((ol.gross - fee - (200.0 - expected_fee)).abs() < 1e-9);
    }

    /// 构造一笔 reversal 开仓,推进平仓侧盘口至往返净率越过 0.8×锁定率 → 触发平仓,
    /// 并验证往返净利恰好扣了 6 笔手续费(开仓 3 + 平仓 3)。
    #[test]
    fn close_trigger_at_discount_with_six_fees() {
        let (sf, of) = (0.00025, 0.0025);
        let k = 60000.0;
        // 开仓 reversal
        let o_call = book(950.0, 5.0, 1000.0, 2.0);
        let o_put = book(1200.0, 3.0, 1250.0, 4.0);
        let o_spot = book(60000.0, 1.5, 60001.0, 1.0);
        let ol = open_reversal(&o_call, &o_put, &o_spot, k).unwrap();
        let open_fee = trading_fee_per_btc(ol.spot, ol.call, ol.put, sf, of);
        let open_net = ol.gross - open_fee;
        let open_rate = open_net / ol.spot;
        let target = 0.8 * open_rate;

        // 平仓侧(conversion 动作):call_bid=1000, put_ask=1010, spot_ask=60000 → close_gross=-10
        let c_call = book(1000.0, 5.0, 1050.0, 5.0);
        let c_put = book(950.0, 5.0, 1010.0, 5.0);
        let c_spot = book(59999.0, 5.0, 60000.0, 5.0);
        let cl = close_legs("reversal", &c_call, &c_put, &c_spot, k).unwrap();
        let close_fee = trading_fee_per_btc(cl.spot, cl.call, cl.put, sf, of);
        let roundtrip_net = ol.gross + cl.gross - open_fee - close_fee;
        let rate = roundtrip_net / ol.spot;

        assert!((cl.gross - (-10.0)).abs() < 1e-9);
        assert!(rate >= target, "应触发平仓: rate={rate} target={target}");
        // 恰好 6 笔手续费
        let six_fees = open_fee + close_fee;
        assert!((roundtrip_net - (ol.gross + cl.gross - six_fees)).abs() < 1e-9);
    }

    #[test]
    fn settle_at_expiry_deducts_spot_and_exercise_fee() {
        let (sf, of) = (0.00025, 0.0025);
        let ex = 0.00015;
        let mut pos = Position {
            id: "x".into(),
            kind: "reversal",
            status: "open",
            expiry: "260925".into(),
            expiry_ms: 1_000,
            strike: 60000.0,
            call_symbol: "C".into(),
            put_symbol: "P".into(),
            open_ts: 0,
            open_spot: 60000.0,
            open_call: 1000.0,
            open_put: 1200.0,
            open_gross: 200.0,
            open_fee: 60000.0 * sf + (1000.0 + 1200.0) * of,
            open_net: 200.0 - (60000.0 * sf + (1000.0 + 1200.0) * of),
            open_rate: 0.0,
            open_annual_rate: 0.0,
            target_close_rate: 0.0,
            close_ts: 0,
            close_spot: 0.0,
            close_call: 0.0,
            close_put: 0.0,
            close_gross: 0.0,
            close_fee: 0.0,
            exec_qty: 1.5,
            net_profit_per_btc: 0.0,
            net_profit_rate: 0.0,
            hold_annual_rate: 0.0,
            est_profit: 0.0,
        };
        pos.open_net = pos.open_gross - pos.open_fee;
        let spot = book(60000.0, 1.0, 60002.0, 1.0); // 结算价 = 60001
        let settled = settle_position(&pos, &spot, 86_400_000, sf, ex);
        let p = 60001.0;
        let expected = pos.open_gross - pos.open_fee - sf * p - ex * p;
        assert_eq!(settled.status, "settled");
        assert!((settled.net_profit_per_btc - expected).abs() < 1e-6);
        assert!((settled.est_profit - expected * 1.5).abs() < 1e-6);
    }
}
