# Reversal Terminator 🚀

A real-time **BTC/USD reversal detection system** written in Rust that connects to the **Coinbase Exchange WebSocket** and monitors the live BTC-USD market. It uses 5 technical signals to flag last-minute reversals — designed for binary options trading (e.g. [Kalshi](https://kalshi.com)).

---

## What it does

The system opens a single connection to the **Coinbase Exchange WebSocket** (`wss://ws-feed.exchange.coinbase.com`) and subscribes to two channels:

| Channel | Purpose |
|---|---|
| `matches` | Real-time trade price, size, and buy/sell side |
| `level2_batch` | Level-2 order book snapshot + incremental updates |

Every **5 seconds** the current price, 60-second rolling volume-weighted average, VWAP, and 5-second price change are printed. When 2 or more indicators fire simultaneously inside the **danger zone** (last 16 seconds of the 1-minute block), a prominent `DO NOT ENTER` warning is printed.

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
Tracks the running sum of buy volume minus sell volume using the `side` field from the `matches` channel.

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
🚀 Reversal Terminator — BTC/USD monitor starting…
   Connecting to Coinbase Exchange WebSocket…

✅ Connected to wss://ws-feed.exchange.coinbase.com
📡 Streaming live data — watching for reversals…

💰 [19:31:05] BTC Price: $67,243.26 | 60s Avg: $67,240.15 | VWAP: $67,238.90 | Δ5s: +$3.11
💰 [19:31:10] BTC Price: $67,255.56 | 60s Avg: $67,242.80 | VWAP: $67,241.20 | Δ5s: +$12.30
🔴 DANGER ZONE — Second 44/60 — Last 16 seconds!
⚠️  VWAP: Price $67,512.00 is ABOVE VWAP $67,425.80 by 2.1σ — mean-reversion risk
🔶 BEARISH DIVERGENCE: Price ↑ $67,512.00 but delta ↓ 0.412 — absorption (sellers absorbing buyers)

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

> **Note**: This application connects to **Coinbase Exchange** (`wss://ws-feed.exchange.coinbase.com`), which is fully available in the US with no regional restrictions. No API keys are required — the feed is public.

---

## Update Interval & Rolling Average

- Status is printed every **5 seconds**.
- **60-second rolling average** (`60s Avg`) is a volume-weighted average of all trades in the last 60 seconds. If no trades have occurred in that window, the last known value is shown with a `⚠️ Stale` warning.
- **Δ5s** shows the price change since the previous 5-second tick.

---

## Block State Reset

At the start of each new 1-minute block:
- **Reset**: VWAP accumulator, cumulative delta, per-block trade counter
- **Carry over**: RSI price history, Bollinger price history (for indicator continuity), rolling 60-second trade buffer (spans minute boundaries by design)

---

## Disclaimer

This software is for **educational and informational purposes only**. It does not constitute financial advice. Binary options and cryptocurrency trading involve significant risk of loss. Always do your own research before trading.
