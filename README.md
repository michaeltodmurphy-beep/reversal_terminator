# Reversal Terminator 🚀

A real-time **BTC/USD reversal detection system** written in Rust that connects to the **Coinbase Exchange WebSocket** and monitors the live BTC-USD market. It uses 5 technical signals to flag last-minute reversals — designed for binary options trading (e.g. [Kalshi](https://kalshi.com)).

---

## What it does

The system opens a single connection to the **Coinbase Exchange WebSocket** (`wss://ws-feed.exchange.coinbase.com`) and subscribes to two channels:

| Channel | Purpose |
|---|---|
| `matches` | Real-time trade price, size, and buy/sell side |
| `level2_batch` | Level-2 order book snapshot + incremental updates |

Every **5 seconds** the current price, 60-second rolling volume-weighted average, and 5-second price change are printed. Every **15 seconds** (at `:00`, `:15`, `:30`, `:45` of each minute) a consolidated signal summary is printed showing all 5 indicator values. When 2 or more indicators fire simultaneously at the **`:45` danger zone checkpoint**, a prominent `DO NOT ENTER` warning is printed.

---

## The 5 Indicators

### 1. VWAP — 5-Minute Rolling VWAP ⚠️
Tracks a rolling 5-minute volume-weighted average price. Entries older than 5 minutes are evicted continuously, keeping the VWAP relevant across minute boundaries.

**Signal**: Price deviating more than **1.5 standard deviations** from the rolling VWAP z-score (shown as `VWAP Z` in signal checks).

### 2. Fast RSI — 2-Period RSI on 5-Second Closes 🚨
A very short (2-period) Relative Strength Index calculated on **5-second candle closes** (12 updates per minute). RSI is computed from the last 50 five-second closes.

**Signal**: An RSI "hook" is detected when:
- RSI was ≥ 95 and is now falling → **buying exhaustion**
- RSI was ≤ 5 and is now rising → **selling exhaustion**

### 3. Order Flow Delta — 15-Second Windows 🔶
Accumulates buy minus sell volume in **15-second windows**. At each 15-second checkpoint the window is finalised and up to 4 completed windows (1 minute lookback) are retained.

**Signal**: Divergence within the most recently completed window — price rose while net delta was negative (bearish absorption), or price fell while net delta was positive (bullish accumulation).

### 4. Bollinger Band Width — 5-Second Candle Closes ⚠️
Maintains a 20-period Bollinger Band on **5-second candle closes** (= 100 seconds of data). Updated at 5-second intervals, not on every trade.

**Signal**: Band width falling below **0.05%** indicates a volatility squeeze — an explosive breakout (in either direction) is imminent.

### 5. Sell Wall Detection 🧱
Scans the ask side of the live order book for large limit orders close to the current price. The order book is kept up-to-date from depth snapshots and incremental updates; the sell wall is **only evaluated at the 15-second signal checkpoint**, not on every depth update.

**Signal**: An ask order ≥ **10 BTC** within **0.1%** above the current price is flagged as a potential ceiling — the *"95% Yes is a trap"* warning.

---

## Output Cadence

```
0s          5s          10s         15s ← SIGNAL CHECK
│─────── price ──────────────────────│
│─────── price ──────────────────────│─ signal check ─│
```

- **Price line** — printed every 5 seconds
- **Signal check** — printed every 15 seconds (every 3rd 5-second tick)

### Danger Zone — Last 15 Seconds (`:45` checkpoint)

The **`:45`** signal check is the danger zone: the last 15 seconds of each 1-minute block. When 2 or more indicators fire at this checkpoint, a `DO NOT ENTER` warning replaces the standard signal summary.

```
0s ─────────────────────── 45s ⚠️ DANGER ZONE ──── 60s
│◄──── Safe to trade ─────►│◄── Reversal risk ───►│
```

---

## Sample Output

```
🚀 Reversal Terminator — BTC/USD monitor starting…
   Connecting to Coinbase Exchange WebSocket…

✅ Connected to wss://ws-feed.exchange.coinbase.com
📡 Streaming live data — watching for reversals…

💰 [19:31:00] BTC: $67,243.26 | 60s Avg: $67,240.15 | Δ5s: +$2.30
💰 [19:31:05] BTC: $67,245.56 | 60s Avg: $67,241.20 | Δ5s: +$2.30
💰 [19:31:10] BTC: $67,250.10 | 60s Avg: $67,243.50 | Δ5s: +$4.54
💰 [19:31:15] BTC: $67,248.80 | 60s Avg: $67,244.10 | Δ5s: -$1.30
   📈 [SIGNAL CHECK] RSI: 72.3 | Delta: +0.42 | BB Width: 0.08% | VWAP Z: 0.8 — ✅ No warnings

💰 [19:31:20] BTC: $67,252.00 | 60s Avg: $67,245.60 | Δ5s: +$3.20
💰 [19:31:25] BTC: $67,255.30 | 60s Avg: $67,247.80 | Δ5s: +$3.30
💰 [19:31:30] BTC: $67,253.10 | 60s Avg: $67,248.90 | Δ5s: -$2.20
   ⚠️ [SIGNAL CHECK] RSI: 94.2 | Delta: -0.12 (DIVERGENCE) | BB Width: 0.03% (SQUEEZE) — 2 warnings

💰 [19:31:35] BTC: $67,249.50 | 60s Avg: $67,249.10 | Δ5s: -$3.60
💰 [19:31:40] BTC: $67,247.20 | 60s Avg: $67,249.00 | Δ5s: -$2.30
💰 [19:31:45] BTC: $67,244.80 | 60s Avg: $67,248.50 | Δ5s: -$2.40
   🚨 [DANGER ZONE] 🚨 RSI HOOK DOWN: RSI 94.0 → 88.0 — buying exhaustion detected! | Delta: -0.25 | VWAP Z: 1.7 — 3/5 DO NOT ENTER!

💰 [19:31:50] BTC: $67,242.10 | 60s Avg: $67,247.80 | Δ5s: -$2.70
💰 [19:31:55] BTC: $67,240.50 | 60s Avg: $67,247.00 | Δ5s: -$1.60
💰 [19:32:00] BTC: $67,241.80 | 60s Avg: $67,246.50 | Δ5s: +$1.30
   📈 [SIGNAL CHECK] RSI: 45.1 | Delta: +0.05 | BB Width: 0.12% | VWAP Z: 0.3 — ✅ No warnings
```

---

## Signal Summary Table

| # | Indicator | Timeframe | Trigger | Tag |
|---|-----------|-----------|---------|-----|
| 1 | VWAP z-score | 5-minute rolling | z-score deviation > 1.5 | `VWAP Z` in signal line |
| 2 | Fast RSI hook | 5-second closes | RSI ≥ 95 turning down / ≤ 5 turning up | 🚨 RSI HOOK in signal line |
| 3 | Order flow divergence | 15-second windows | Price & delta moving in opposite directions | `(DIVERGENCE)` |
| 4 | Bollinger squeeze | 5-second closes (20-period) | Band width < 0.05% | `(SQUEEZE)` |
| 5 | Sell wall | Evaluated at 15s checkpoint | Ask ≥ 10 BTC within 0.1% of price | 🧱 |

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

- **Price line** is printed every **5 seconds**.
- **Signal summary** is printed every **15 seconds** at `:00`, `:15`, `:30`, `:45` of each minute.
- **60-second rolling average** (`60s Avg`) is a volume-weighted average of all trades in the last 60 seconds. If no trades have occurred in that window, the last known value is shown with a `⚠️ Stale` warning.
- **Δ5s** shows the price change since the previous 5-second tick.

---

## Disclaimer

This software is for **educational and informational purposes only**. It does not constitute financial advice. Binary options and cryptocurrency trading involve significant risk of loss. Always do your own research before trading.
