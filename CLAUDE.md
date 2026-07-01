# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A read-only monitor (Rust/tokio) that watches **Binance options** (every expiry × every strike) against **Binance spot BTCUSDT** for **conversion / reversal** put-call-parity arbitrage. It tracks each opportunity as a **position lifecycle** — open → (early close when profitable | expiry settlement) — and records it to SQLite (`positions` table), serving a web page. **It never places orders.** Fees are deducted on a **round-trip basis: 6 legs** (open 3 + close 3) via `OA_FEE_RATE=0.0001`; it ignores slippage and does not model borrow/funding costs. Code comments and the README are in Chinese; keep that convention when editing.

**Key model insight**: for a given `(expiry, strike)`, closing a `reversal` is exactly doing the `conversion` action at close time (and vice versa), so K cancels and round-trip net = `open_gross + close_gross − 6 leg fees`. The two gross formulas are reused for both open and close.

## Commands

```bash
cargo run --release            # run the monitor + web UI (http://127.0.0.1:8080)
cargo test                     # all tests (arb formulas, pair evaluation, DB roundtrip)
cargo test reversal_formula    # single test by name
cargo test --package option_arb db::tests   # one module's tests
cargo build                    # debug build
```

Inspect recorded data directly:
```bash
sqlite3 data.db "SELECT kind,status,expiry,strike,open_rate,net_profit_rate,est_profit \
  FROM positions ORDER BY open_ts DESC LIMIT 20"
```

All runtime config is via `OA_*` environment variables (see `src/config.rs` / README table). Notable knobs: `OA_FEE_RATE` (per leg, charged 6×), `OA_CLOSE_RATE_DISCOUNT` (close when round-trip rate ≥ discount × locked open rate, default 0.8), `OA_EXERCISE_FEE_RATE` (ITM exercise fee at settlement), `OA_MIN_ANNUAL_RATE`, `OA_MIN_PROFIT_RATE`, `OA_MIN_EXEC_QTY` (open thresholds), `OA_SCAN_INTERVAL_MS`, `OA_RECORD_INTERVAL_MS` (mark-update + re-open throttle), `OA_DB_PATH`, `OA_WEB_PORT`.

## Architecture

Five tokio tasks spawned in `main.rs`, communicating through two `mpsc` channels and two shared structures (`Arc<Quotes>`, `Arc<RwLock<Universe>>`). Nothing here is obvious from any single file:

```
poller ──SubCmd──▶ ws_options ─┐
(exchangeInfo)                 ├─▶ Quotes (DashMap<symbol,BookTop> + RwLock<spot>)
ws_spot ───────────────────────┘            │
                                            ▼  (read snapshot)
                                         scanner ──Position──▶ db writer thread ──▶ SQLite
                                         (holds position map)                          ▲
                                                                                   web (read-only)
```

- **`poller`** (`poller.rs`) polls `GET /eapi/v1/exchangeInfo` every `OA_INFO_POLL_SECS`, rebuilds the `Universe` (expiry → strike → {call,put} symbols) via `binance::rest::fetch_universe`, and ensures the option market stream `btcusdt@optionMarkPrice` is subscribed while tradable symbols exist. The universe refresh is what makes the scanner self-adapting to newly listed or delivered contracts.
- **`ws_options`** (`binance/ws_options.rs`) holds one WS connection to `fstream` market data and subscribes `<underlyingPair>@optionMarkPrice` (for BTC, `btcusdt@optionMarkPrice`). This stream pushes top-of-book fields for all option symbols under the underlying. It owns the authoritative `current` subscription set, re-subscribes everything on reconnect, and chunks SUBSCRIBE/UNSUBSCRIBE into ≤50-param frames. Quote fields come from Binance's option market keys `bo/bq/ao/aq`.
- **`ws_spot`** (`binance/ws_spot.rs`) subscribes `btcusdt@bookTicker`, writes the single spot `BookTop`.
- **`scanner`** (`scan.rs`) is a **stateful position tracker**, not a stateless detector. It holds `positions: HashMap<kind|expiry|strikekey, Position>` (seeded at startup from `db::load_open_positions` so restarts don't orphan open rows). Each `OA_SCAN_INTERVAL_MS`: **(1)** for every open position — if `now ≥ expiry_ms` → `settle_position` (spot rebuy fee + ITM exercise fee); else compute the reverse-action `close_legs`, and if round-trip net rate `≥ target_close_rate` (= `OA_CLOSE_RATE_DISCOUNT × open_rate`) → close, otherwise emit a throttled mark-to-market update. **(2)** walk the universe and, for each idle `(kind,expiry,strike)` passing thresholds + re-open throttle, open a position. Helpers: `open_reversal`/`open_conversion`/`close_legs`/`settle_position`; gross via `reversal_profit`/`conversion_profit`. Every open/close/settle/mark is `try_send`'d as a full `Position` (drops on full — acceptable).
- **`db`** (`db.rs`) runs a **dedicated OS thread** that `blocking_recv`s `Position`s and **UPSERTs** them (`ON CONFLICT(id)`) in batched transactions (≤500/tx) into the `positions` table — a position is emitted multiple times under the same `id` (open → mark → close), and **open_\* columns are never overwritten**. WAL + `synchronous=NORMAL`. Web opens **separate read-only connections**; `load_open_positions` also reads read-only. Writer and readers never share a `Connection`.
- **`web`** (`web/mod.rs`) is axum serving the embedded `index.html` (`include_str!`) plus `/api/opportunities` (filterable incl. `status`) and `/api/stats` (open/closed/settled counts + realized profit). DB calls run in `spawn_blocking`.

### Conventions worth knowing

- **Strike keys**: strikes are `f64` but keyed in maps as `(strike*1000).round() as i64` via `strike_key()` — always use that helper, never the raw float, for map keys. Position map key is `format!("{kind}|{expiry}|{strike_key}")`; the DB primary key `id` is `"{kind}-{expiry}-{strike_key}-{open_ts}"`.
- **The two gross profit formulas in `scan.rs` are the spec** (also tabulated in README) and are covered by unit tests; changing a sign changes what counts as an opportunity. `reversal_profit` uses spot_bid/put_bid/call_ask; `conversion_profit` uses spot_ask/put_ask/call_bid. **They serve double duty**: `open_reversal`/`open_conversion` for opening, and `close_legs` reuses the *opposite* one for the reverse-action close. Fees are `OA_FEE_RATE * (spot+call+put)` per side, charged on both open and close.
- **Quote parsing** tolerates Binance sending numbers as JSON strings — use `model::parse_f64`, not direct deserialization, for price/qty fields.
- `Position.kind` (open direction) and `Position.status` (`"open"`/`"closed"`/`"settled"`) are `&'static str`, relied on by the DB layer and web filters; `db::intern_kind`/`intern_status` re-intern DB strings back to `&'static str` when loading.

## Geo-restriction (important for testing here)

Binance production hosts (`eapi.binance.com`, `fstream.binance.com`, `stream.binance.com`) may return **HTTP 451** from some regions. Spot-only checks can use the public mirror `https://data-api.binance.vision` / `wss://data-stream.binance.vision` (no options mirror exists). Override hosts with `OA_EAPI_BASE` / `OA_OPTIONS_WS` / `OA_SPOT_WS`.

## Repo state

Not a git repository. `data.db` (a live SQLite file) is checked into the working directory.
