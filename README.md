# Reversal Terminator 🚀

A real-time **BTC/USD reversal detection system** written in Rust that connects to Binance WebSocket streams and monitors the **1-minute chart**. It uses 5 technical signals to flag last-minute reversals — designed for binary options trading (e.g. [Kalshi](https://kalshi.com)).

---

## What it does

The system monitors two live Binance WebSocket feeds:

| Stream | Purpose |
|---|---|
| `btcusdt@aggTrade` | Real-time trade price, quantity, and buy/sell side |
| `btcusdt@depth20@100ms` | Level-2 order book snapshots (top 20 bids & asks) |

Every 1-minute block is analysed for signs that the current trend is about to **reverse**. When 2 or more indicators fire simultaneously inside the **danger zone** (last 16 seconds of the block), a prominent `DO NOT ENTER` warning is printed.

---

## The 5 Indicators

### 1. VWAP — Volume Weighted Average Price ⚠️
Tracks cumulative volume-weighted price for the current block, along with a rolling standard deviation of the last 200 ticks.

**Signal**: Price deviating more than **1.5 standard deviations** from VWAP suggests a potential mean reversion.

### 2. Fast RSI — 2-Period RSI 🚨
A very short (2-period) Relative Strength Index that reacts almost instantly to price momentum changes.

**Signal**: An RSI "hook" is detected when:
- RSI was ≥ 95 and is now falling → **buying exhaustion**
- RSI was ≤ 5 and is now rising → **selling exhaustion**

### 3. Order Flow Delta — Cumulative Delta 🔶
Tracks the running sum of buy volume minus sell volume using the `is_buyer_maker` flag from the aggTrade stream.

**Signal**: Divergence between price direction and delta direction:
- Price rising but delta falling → **bearish absorption** (sellers absorbing buyers)
- Price falling but delta rising → **bullish accumulation** (buyers absorbing sellers)

### 4. Bollinger Band Width — Volatility Squeeze ⚠️
Maintains a 20-sample Bollinger Band on recent 1-second price snapshots.

**Signal**: Band width falling below **0.05%** indicates a volatility squeeze — an explosive breakout (in either direction) is imminent.

### 5. Sell Wall Detection 🧱
Scans the top-20 ask side of the live order book for large limit orders close to the current price.

**Signal**: An ask order ≥ **10 BTC** within **0.1%** above the current price is flagged as a potential ceiling — the *"95% Yes is a trap"* warning.

---

## Danger Zone — Last 16 Seconds

Each 1-minute block is 60 seconds long. The **danger zone** starts at second **44** (i.e. 16 seconds remain in the block). This is the proportional equivalent of the "last 4 minutes" concept used on 15-minute charts.

```
0s ────────────────────────── 44s ⚠️ DANGER ZONE ────────── 60s
│◄──────── Safe to trade ────────►│◄──── Reversal risk ────►│
```

During the danger zone:
- All 5 signals are evaluated continuously
- When **2 or more** signals fire at the same time, a combined warning is printed:

```
🚨🚨🚨 MULTIPLE REVERSAL SIGNALS (3/5)! DO NOT ENTER even at 95% probability! 🚨🚨🚨
```

---

## Sample Output

```
🚀 Reversal Terminator — BTC/USD 1-min chart monitor starting…
   Connecting to Binance WebSocket streams…

✅ Connected to wss://stream.binance.com:9443/ws/btcusdt@aggTrade
✅ Connected to wss://stream.binance.com:9443/ws/btcusdt@depth20@100ms
📡 Streaming live data — watching for reversals…

📊 BTC: $96423.10 | VWAP: $96418.55 | Delta: 1.243 | Sec: 12/60
📊 BTC: $96441.20 | VWAP: $96425.80 | Delta: 2.891 | Sec: 24/60
🔴 DANGER ZONE — Second 44/60 — Last 16 seconds!
⚠️  VWAP: Price $96512.00 is ABOVE VWAP $96425.80 by 2.1σ — mean-reversion risk
🔶 BEARISH DIVERGENCE: Price ↑ $96512.00 but delta ↓ 0.412 — absorption (sellers absorbing buyers)

🚨🚨🚨 MULTIPLE REVERSAL SIGNALS (2/5)! DO NOT ENTER even at 95% probability! 🚨🚨🚨
```

---

## Signal Summary Table

| # | Indicator | Trigger | Emoji |
|---|-----------|---------|-------|
| 1 | VWAP deviation | Price > 1.5σ from VWAP | ⚠️ |
| 2 | Fast RSI hook | RSI ≥ 95 turning down / ≤ 5 turning up | 🚨 |
| 3 | Order flow divergence | Price & delta moving in opposite directions | 🔶 |
| 4 | Bollinger squeeze | Band width < 0.05% | ⚠️ |
| 5 | Sell wall | Ask ≥ 10 BTC within 0.1% of price | 🧱 |

---

## Build & Run

### Prerequisites

- [Rust toolchain](https://rustup.rs/) (stable, edition 2021)

### Build

```bash
cargo build --release
```

### Run

```bash
cargo run --release
```

> **Note for US users**: Binance.com may be geo-restricted in the United States. If you see connection errors, replace the stream URLs in `src/main.rs` with the US endpoints:
> ```
> wss://stream.binance.us:9443/ws/btcusdt@aggTrade
> wss://stream.binance.us:9443/ws/btcusdt@depth20@100ms
> ```

---

## Block State Reset

At the start of each new 1-minute block:
- **Reset**: VWAP accumulator, cumulative delta, per-block trade counter
- **Carry over**: RSI price history, Bollinger price history (for indicator continuity)

---

## Disclaimer

This software is for **educational and informational purposes only**. It does not constitute financial advice. Binary options and cryptocurrency trading involve significant risk of loss. Always do your own research before trading.
