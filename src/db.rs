//! SQLite 持久化:建表(WAL)、持仓 UPSERT 写线程、Web 只读查询。
//!
//! 一条 `positions` 记录 = 一次完整套利生命周期:开仓(open)→ 早平(closed)/ 到期结算(settled)。
//! 同一 `id` 会被多次 emit(开仓 → 盯市更新 → 平仓),写入用 UPSERT,开仓字段落库后不再覆盖。
use crate::model::Position;
use anyhow::Result;
use rusqlite::{params, params_from_iter, types::Value, Connection, OpenFlags, Row};
use tokio::sync::mpsc;
use tracing::{error, info};

const SCHEMA: &str = "
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
CREATE TABLE IF NOT EXISTS positions (
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,
  status TEXT NOT NULL,
  expiry TEXT NOT NULL,
  expiry_ms INTEGER NOT NULL,
  strike REAL NOT NULL,
  call_symbol TEXT NOT NULL,
  put_symbol TEXT NOT NULL,
  open_ts INTEGER NOT NULL,
  open_spot REAL NOT NULL,
  open_call REAL NOT NULL,
  open_put REAL NOT NULL,
  open_gross REAL NOT NULL,
  open_fee REAL NOT NULL,
  open_net REAL NOT NULL,
  open_rate REAL NOT NULL,
  open_annual_rate REAL NOT NULL,
  target_close_rate REAL NOT NULL,
  close_ts INTEGER NOT NULL,
  close_spot REAL NOT NULL,
  close_call REAL NOT NULL,
  close_put REAL NOT NULL,
  close_gross REAL NOT NULL,
  close_fee REAL NOT NULL,
  exec_qty REAL NOT NULL,
  net_profit_per_btc REAL NOT NULL,
  net_profit_rate REAL NOT NULL,
  hold_annual_rate REAL NOT NULL,
  est_profit REAL NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_pos_status ON positions(status);
CREATE INDEX IF NOT EXISTS idx_pos_open_ts ON positions(open_ts);
CREATE INDEX IF NOT EXISTS idx_pos_expiry_strike ON positions(expiry, strike);
";

const COLS: &str = "id,kind,status,expiry,expiry_ms,strike,call_symbol,put_symbol,\
open_ts,open_spot,open_call,open_put,open_gross,open_fee,open_net,open_rate,open_annual_rate,target_close_rate,\
close_ts,close_spot,close_call,close_put,close_gross,close_fee,\
exec_qty,net_profit_per_btc,net_profit_rate,hold_annual_rate,est_profit";

// 开仓字段落库后不可变;冲突时仅更新平仓/盯市字段。
const UPSERT_SQL: &str = "INSERT INTO positions (\
id,kind,status,expiry,expiry_ms,strike,call_symbol,put_symbol,\
open_ts,open_spot,open_call,open_put,open_gross,open_fee,open_net,open_rate,open_annual_rate,target_close_rate,\
close_ts,close_spot,close_call,close_put,close_gross,close_fee,\
exec_qty,net_profit_per_btc,net_profit_rate,hold_annual_rate,est_profit) \
VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27,?28,?29) \
ON CONFLICT(id) DO UPDATE SET \
status=excluded.status,close_ts=excluded.close_ts,close_spot=excluded.close_spot,\
close_call=excluded.close_call,close_put=excluded.close_put,close_gross=excluded.close_gross,\
close_fee=excluded.close_fee,exec_qty=excluded.exec_qty,net_profit_per_btc=excluded.net_profit_per_btc,\
net_profit_rate=excluded.net_profit_rate,hold_annual_rate=excluded.hold_annual_rate,est_profit=excluded.est_profit";

fn intern_kind(s: &str) -> &'static str {
    if s == "conversion" {
        "conversion"
    } else {
        "reversal"
    }
}

fn intern_status(s: &str) -> &'static str {
    match s {
        "closed" => "closed",
        "settled" => "settled",
        _ => "open",
    }
}

/// 打开连接并建表(幂等)。
pub fn init(path: &str) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

/// 启动独立写线程:从异步通道接收持仓事件,批量事务 UPSERT。
pub fn spawn_writer(path: String, mut rx: mpsc::Receiver<Position>) {
    std::thread::spawn(move || {
        let conn = match init(&path) {
            Ok(c) => c,
            Err(e) => {
                error!(error = %e, "数据库初始化失败,写线程退出");
                return;
            }
        };
        info!("数据库写线程就绪: {path}");
        loop {
            let first = match rx.blocking_recv() {
                Some(o) => o,
                None => break, // 通道关闭
            };
            let mut batch = vec![first];
            while let Ok(o) = rx.try_recv() {
                batch.push(o);
                if batch.len() >= 500 {
                    break;
                }
            }
            if let Err(e) = write_batch(&conn, &batch) {
                error!(error = %e, "批量写入失败");
            }
        }
        info!("数据库写线程退出");
    });
}

fn write_batch(conn: &Connection, batch: &[Position]) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    {
        let mut stmt = tx.prepare_cached(UPSERT_SQL)?;
        for o in batch {
            stmt.execute(params![
                o.id,
                o.kind,
                o.status,
                o.expiry,
                o.expiry_ms,
                o.strike,
                o.call_symbol,
                o.put_symbol,
                o.open_ts,
                o.open_spot,
                o.open_call,
                o.open_put,
                o.open_gross,
                o.open_fee,
                o.open_net,
                o.open_rate,
                o.open_annual_rate,
                o.target_close_rate,
                o.close_ts,
                o.close_spot,
                o.close_call,
                o.close_put,
                o.close_gross,
                o.close_fee,
                o.exec_qty,
                o.net_profit_per_btc,
                o.net_profit_rate,
                o.hold_annual_rate,
                o.est_profit,
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// 把一行读成 JSON(供 Web API)。列顺序须与 `COLS` 一致。
fn row_to_json(row: &Row) -> rusqlite::Result<serde_json::Value> {
    Ok(serde_json::json!({
        "id": row.get::<_, String>(0)?,
        "kind": row.get::<_, String>(1)?,
        "status": row.get::<_, String>(2)?,
        "expiry": row.get::<_, String>(3)?,
        "expiry_ms": row.get::<_, i64>(4)?,
        "strike": row.get::<_, f64>(5)?,
        "call_symbol": row.get::<_, String>(6)?,
        "put_symbol": row.get::<_, String>(7)?,
        "open_ts": row.get::<_, i64>(8)?,
        "open_spot": row.get::<_, f64>(9)?,
        "open_call": row.get::<_, f64>(10)?,
        "open_put": row.get::<_, f64>(11)?,
        "open_gross": row.get::<_, f64>(12)?,
        "open_fee": row.get::<_, f64>(13)?,
        "open_net": row.get::<_, f64>(14)?,
        "open_rate": row.get::<_, f64>(15)?,
        "open_annual_rate": row.get::<_, f64>(16)?,
        "target_close_rate": row.get::<_, f64>(17)?,
        "close_ts": row.get::<_, i64>(18)?,
        "close_spot": row.get::<_, f64>(19)?,
        "close_call": row.get::<_, f64>(20)?,
        "close_put": row.get::<_, f64>(21)?,
        "close_gross": row.get::<_, f64>(22)?,
        "close_fee": row.get::<_, f64>(23)?,
        "exec_qty": row.get::<_, f64>(24)?,
        "net_profit_per_btc": row.get::<_, f64>(25)?,
        "net_profit_rate": row.get::<_, f64>(26)?,
        "hold_annual_rate": row.get::<_, f64>(27)?,
        "est_profit": row.get::<_, f64>(28)?,
    }))
}

/// 把一行读成 Position(供重启回填)。列顺序须与 `COLS` 一致。
fn row_to_position(row: &Row) -> rusqlite::Result<Position> {
    Ok(Position {
        id: row.get(0)?,
        kind: intern_kind(&row.get::<_, String>(1)?),
        status: intern_status(&row.get::<_, String>(2)?),
        expiry: row.get(3)?,
        expiry_ms: row.get(4)?,
        strike: row.get(5)?,
        call_symbol: row.get(6)?,
        put_symbol: row.get(7)?,
        open_ts: row.get(8)?,
        open_spot: row.get(9)?,
        open_call: row.get(10)?,
        open_put: row.get(11)?,
        open_gross: row.get(12)?,
        open_fee: row.get(13)?,
        open_net: row.get(14)?,
        open_rate: row.get(15)?,
        open_annual_rate: row.get(16)?,
        target_close_rate: row.get(17)?,
        close_ts: row.get(18)?,
        close_spot: row.get(19)?,
        close_call: row.get(20)?,
        close_put: row.get(21)?,
        close_gross: row.get(22)?,
        close_fee: row.get(23)?,
        exec_qty: row.get(24)?,
        net_profit_per_btc: row.get(25)?,
        net_profit_rate: row.get(26)?,
        hold_annual_rate: row.get(27)?,
        est_profit: row.get(28)?,
    })
}

/// 回填库中所有 status='open' 的仓位(scanner 重启用)。
pub fn load_open_positions(path: &str) -> Result<Vec<Position>> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let sql = format!("SELECT {COLS} FROM positions WHERE status='open'");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], row_to_position)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Web 查询过滤条件。
#[derive(Debug, Default)]
pub struct Filters {
    pub from: Option<i64>, // open_ts >=
    pub to: Option<i64>,   // open_ts <=
    pub kind: Option<String>,
    pub status: Option<String>,
    pub expiry: Option<String>,
    pub min_rate: Option<f64>, // open_rate(锁定率)>=
    pub limit: i64,
}

/// 只读查询持仓记录,返回 JSON 行。
pub fn query(path: &str, f: &Filters) -> Result<Vec<serde_json::Value>> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut clauses: Vec<&str> = Vec::new();
    let mut args: Vec<Value> = Vec::new();
    if let Some(v) = f.from {
        clauses.push("open_ts>=?");
        args.push(Value::Integer(v));
    }
    if let Some(v) = f.to {
        clauses.push("open_ts<=?");
        args.push(Value::Integer(v));
    }
    if let Some(ref v) = f.kind {
        clauses.push("kind=?");
        args.push(Value::Text(v.clone()));
    }
    if let Some(ref v) = f.status {
        clauses.push("status=?");
        args.push(Value::Text(v.clone()));
    }
    if let Some(ref v) = f.expiry {
        clauses.push("expiry=?");
        args.push(Value::Text(v.clone()));
    }
    if let Some(v) = f.min_rate {
        clauses.push("open_rate>=?");
        args.push(Value::Real(v));
    }
    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
    let limit = if f.limit <= 0 { 500 } else { f.limit.min(5000) };
    let sql =
        format!("SELECT {COLS} FROM positions {where_sql} ORDER BY open_ts DESC LIMIT {limit}");

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(args.iter()), |row| row_to_json(row))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// 概要统计。
pub fn stats(path: &str) -> Result<serde_json::Value> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let count = |st: &str| -> Result<i64> {
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM positions WHERE status=?",
            params![st],
            |r| r.get(0),
        )?)
    };
    let total: i64 = conn.query_row("SELECT COUNT(*) FROM positions", [], |r| r.get(0))?;
    let realized: f64 = conn.query_row(
        "SELECT COALESCE(SUM(est_profit),0) FROM positions WHERE status IN ('closed','settled')",
        [],
        |r| r.get(0),
    )?;
    let latest_ts: i64 = conn.query_row(
        "SELECT COALESCE(MAX(open_ts),0) FROM positions",
        [],
        |r| r.get(0),
    )?;
    Ok(serde_json::json!({
        "total": total,
        "open": count("open")?,
        "closed": count("closed")?,
        "settled": count("settled")?,
        "realized_profit": realized,
        "latest_ts": latest_ts,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_open(id: &str, kind: &'static str, open_rate: f64) -> Position {
        Position {
            id: id.into(),
            kind,
            status: "open",
            expiry: "260925".into(),
            expiry_ms: 1_790_000_000_000,
            strike: 60000.0,
            call_symbol: "BTC-260925-60000-C".into(),
            put_symbol: "BTC-260925-60000-P".into(),
            open_ts: 1_700_000_000_000,
            open_spot: 60000.0,
            open_call: 1000.0,
            open_put: 1200.0,
            open_gross: 200.0,
            open_fee: 6.22,
            open_net: 193.78,
            open_rate,
            open_annual_rate: open_rate * 12.0,
            target_close_rate: open_rate * 0.8,
            close_ts: 0,
            close_spot: 0.0,
            close_call: 0.0,
            close_put: 0.0,
            close_gross: 0.0,
            close_fee: 0.0,
            exec_qty: 1.5,
            net_profit_per_btc: 193.78,
            net_profit_rate: open_rate,
            hold_annual_rate: open_rate * 12.0,
            est_profit: 290.67,
        }
    }

    #[test]
    fn open_then_close_upsert_roundtrip() {
        let dir = std::env::temp_dir();
        let path = dir
            .join(format!("oa_test_{}.db", std::process::id()))
            .to_string_lossy()
            .to_string();
        let _ = std::fs::remove_file(&path);

        let conn = init(&path).unwrap();

        // 两笔开仓
        let p1 = sample_open("id-rev", "reversal", 0.003);
        let p2 = sample_open("id-conv", "conversion", 0.001);
        write_batch(&conn, &[p1.clone(), p2]).unwrap();

        // 只回填 open
        assert_eq!(load_open_positions(&path).unwrap().len(), 2);

        // 平掉 p1(同 id UPSERT)
        let mut closed = p1;
        closed.status = "closed";
        closed.close_ts = 1_700_000_100_000;
        closed.close_gross = -10.0;
        closed.close_fee = 6.2;
        closed.net_profit_per_btc = 177.58;
        closed.net_profit_rate = 0.00296;
        closed.est_profit = 266.37;
        write_batch(&conn, &[closed]).unwrap();

        // 同 id 从 open 变 closed,总行数仍为 2
        let all = query(
            &path,
            &Filters {
                limit: 100,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(all.len(), 2);

        let only_closed = query(
            &path,
            &Filters {
                status: Some("closed".into()),
                limit: 100,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(only_closed.len(), 1);
        assert_eq!(only_closed[0]["id"], "id-rev");
        assert_eq!(only_closed[0]["net_profit_per_btc"], 177.58);
        // 开仓字段未被覆盖
        assert_eq!(only_closed[0]["open_gross"], 200.0);

        // 回填只剩仍 open 的那笔
        let open_left = load_open_positions(&path).unwrap();
        assert_eq!(open_left.len(), 1);
        assert_eq!(open_left[0].id, "id-conv");

        let s = stats(&path).unwrap();
        assert_eq!(s["total"], 2);
        assert_eq!(s["open"], 1);
        assert_eq!(s["closed"], 1);
        assert_eq!(s["settled"], 0);

        let _ = std::fs::remove_file(&path);
    }
}
