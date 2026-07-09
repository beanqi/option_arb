# Option Arb Monitor

BTC/ETH conversion and reversal monitor using Deribit options and Binance spot top-of-book data.

## Run

```bash
cargo run
```

Open:

```text
http://127.0.0.1:8080
```

The service binds to `0.0.0.0:8080` by default. Use the machine's LAN/VPN IP when opening it from another device.

Configuration:

| Env | Default | Meaning |
| --- | --- | --- |
| `OPTION_ARB_HOST` | `0.0.0.0` | Dashboard bind host |
| `OPTION_ARB_PORT` | `8080` | Dashboard port |
| `OPTION_ARB_POLL_SECS` | `15` | Market refresh interval, minimum 5 seconds |
| `OPTION_ARB_SPOT_FEE_RATE` | `0.00024` | Binance spot fee rate. Legacy `OPTION_ARB_FEE_RATE` is still accepted as a fallback |
| `OPTION_ARB_MIN_ANNUALIZED` | `0.10` | Minimum annualized return shown on the dashboard |
| `OPTION_ARB_MAX_CANDIDATES` | `40` | Rough candidates per underlying to verify with live option books |
| `OPTION_ARB_HISTORY_PATH` | `data/opportunities.jsonl` | JSONL file used to append every qualifying open opportunity |
| `OPTION_ARB_HISTORY_MAX_RECORDS` | `500` | Recent history records kept in memory and shown by `/api/history` |

## Calculation

The monitor pairs Deribit calls and puts by underlying, expiry, and strike. It converts option premium quotes from BTC/ETH into USD using the current spot mid.

Deribit BTC/ETH option trading fees are calculated per option leg as:

```text
fee_underlying_per_unit = min(0.0003, 0.125 * option_price_underlying)
fee_usd = fee_underlying_per_unit * spot_mid * quantity
```

The Binance spot leg uses `OPTION_ARB_SPOT_FEE_RATE`.

Conversion:

```text
buy spot + buy put - sell call
net = strike - spot_ask - put_ask_usd + call_bid_usd - fees
capital = spot_ask
```

Reversal:

```text
sell/short spot + sell put - buy call
net = spot_bid + put_bid_usd - call_ask_usd - strike - fees
capital = spot_bid
```

Annualized return:

```text
(net / capital) * 365 / days_to_expiry
```

Only opportunities with annualized return greater than or equal to the configured threshold are returned.

## History

Every opportunity that passes the configured annualized-return threshold is appended to `OPTION_ARB_HISTORY_PATH` during polling. The dashboard's history tab reads `/api/history` and shows the most recent `OPTION_ARB_HISTORY_MAX_RECORDS` records, newest first.

## Scope

This is a monitor and order-ticket generator. It does not place orders. Reversal rows assume you already have spot inventory to sell or a valid borrow/margin route for the short spot leg.
