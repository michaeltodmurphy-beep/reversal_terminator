# Reversal Terminator 🚀

A real-time **BTC/USD reversal detection system** written in Rust that connects to the **Coinbase Exchange WebSocket** and monitors the live BTC-USD market. It uses 5 technical signals combined into a single **Reversal Risk Index (RRI)** to flag last-minute reversals — designed for binary options trading (e.g. [Kalshi](https://kalshi.com)).

---

## What it does

The system opens a single connection to the **Coinbase Exchange WebSocket** (`wss://ws-feed.exchange.coinbase.com`) and subscribes to two channels:

| Channel | Purpose |
|---|---|
| `matches` | Real-time trade price, size, and buy/sell side |
| `level2_batch` | Level-2 order book snapshot + incremental updates |

Every **5 seconds** the current price, 60-second rolling volume-weighted average, and 5-second price change are printed. Every **15 seconds** (at `:00`, `:15`, `:30`, `:45` of each minute) a **Reversal Risk Index (RRI)** is printed combining all 5 indicator scores into one clean, actionable number from 1.0 to 10.0.

---

## Reversal Risk Index (RRI)

Each indicator contributes a weighted sub-score. The raw RRI is their sum, floored at 1.0 and capped at 10.0. To eliminate erratic jumps, the displayed RRI is smoothed with an **Exponential Moving Average (EMA)** before it is shown.

```rust
let raw_rri = (delta_score + rsi_score + vwap_score + bb_score + wall_score).clamp(1.0, 10.0);
// EMA smoothing (α = 0.3, ~1 minute window):
smoothed_rri = 0.3 * raw_rri + 0.7 * smoothed_rri;
```

### EMA Smoothing

| Parameter | Value | Effect |
|-----------|-------|--------|
| Alpha (α) | 0.3 | ~4 readings / ~1 minute smoothing window |
| First reading | Initialised to raw RRI | No lag on start-up |

The smoothed value is used for both the displayed score and the risk level label/emoji. The raw component scores in the bracket line are always the **unsmoothed** values so you can see exactly what is driving the RRI.

### Trend Arrow

A directional arrow is printed after the score to show whether risk is rising or falling:

| Arrow | Meaning | Condition |
|-------|---------|-----------|
| `↑` | Risk increasing | Smoothed RRI rose by > 0.3 since last reading |
| `↓` | Risk decreasing | Smoothed RRI fell by > 0.3 since last reading |
| `→` | Stable | Change ≤ 0.3, or first reading |

### Component Weights

| # | Indicator | Max Score | Weight |
|---|-----------|-----------|--------|
| 1 | Order Flow Delta Divergence | 3.0 | 30% |
| 2 | RSI Exhaustion | 2.5 | 25% |
| 3 | VWAP Deviation | 2.0 | 20% |
| 4 | Bollinger Squeeze | 1.5 | 15% |
| 5 | Sell/Buy Wall Proximity | 1.0 | 10% |

### Risk Level Labels

| RRI Score | Label | Emoji | Meaning |
|---|---|---|---|
| 1.0 – 2.5 | `LOW RISK` | 🟢 | Safe to trade — momentum is clean |
| 2.6 – 4.5 | `MODERATE` | 🟡 | Proceed with caution — some signals warming up |
| 4.6 – 6.5 | `ELEVATED` | 🟠 | Reversal pressure building — reduce position size |
| 6.6 – 8.0 | `HIGH RISK` | 🔴 | Strong reversal signals — avoid new entries |
| 8.1 – 10.0 | `EXTREME` | 🚨 | Near-certain reversal — DO NOT ENTER |

---

## The 5 Indicators

### 1. VWAP — 5-Minute Rolling VWAP
Tracks a rolling 5-minute volume-weighted average price. Entries older than 5 minutes are evicted continuously, keeping the VWAP relevant across minute boundaries.

**Scoring** (standard deviations from VWAP):
- `0.0` = within 0.5σ of VWAP
- `0.5` = 0.5–1.0σ from VWAP
- `1.0` = 1.0–1.5σ from VWAP
- `1.5` = 1.5–2.0σ from VWAP
- `2.0` = > 2.0σ from VWAP

### 2. Fast RSI — 2-Period RSI on 5-Second Closes
A very short (2-period) Relative Strength Index calculated on **5-second candle closes** (12 updates per minute). RSI is computed from the last 50 five-second closes.

**Scoring**:
- `0.0` = RSI 30–70 (neutral)
- `0.5` = RSI 20–30 or 70–80 (warming up)
- `1.0` = RSI 10–20 or 80–90 (extended)
- `2.0` = RSI < 10 or > 90 (extreme)
- `2.5` = RSI hook from extreme (was > 90 turning down, or was < 10 turning up) — strongest signal

### 3. Order Flow Delta — 15-Second Windows
Accumulates buy minus sell volume in **15-second windows**. At each 15-second checkpoint the window is finalised and up to 4 completed windows (1 minute lookback) are retained.

**Scoring**:
- `0.0` = No divergence (price and delta moving in the same direction)
- `1.5` = Mild divergence (price moving one way, delta flat or weakly opposite)
- `3.0` = Strong divergence (price moving one way, delta strongly the other)

### 4. Bollinger Band Width — 5-Second Candle Closes
Maintains a 20-period Bollinger Band on **5-second candle closes** (= 100 seconds of data). Updated at 5-second intervals.

**Scoring** (band width percentage):
- `0.0`  = band width > 0.10% (normal volatility)
- `0.5`  = band width 0.05–0.10% (narrowing)
- `0.75` = band width 0.03–0.05% (tight squeeze)
- `1.5`  = band width < 0.03% (extreme squeeze)

### 5. Sell Wall Detection
Scans the ask side of the live order book for large limit orders close to the current price. The order book is kept up-to-date from depth snapshots and incremental updates; the wall score is **only evaluated at the 15-second signal checkpoint**, not on every depth update.

**Scoring**:
- `0.0` = No significant wall nearby
- `0.3` = Wall (> 5 BTC) within 0.2% of current price
- `0.5` = Wall (> 10 BTC) within 0.1% of current price
- `1.0` = Massive wall (> 20 BTC) within 0.05% of current price

---

## Output Cadence

```
0s          5s          10s         15s ← RRI CHECK
│─────── price ──────────────────────│
│─────── price ──────────────────────│─ RRI check ─│
```

- **Price line** — printed every 5 seconds
- **RRI check** — printed every 15 seconds (every 3rd 5-second tick)

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
   🟢 RRI: 1.8/10 → — LOW RISK — Safe to trade
   [Delta: 0.0 | RSI: 0.5 | VWAP: 0.8 | BB: 0.50 | Wall: 0.0]

💰 [19:31:20] BTC: $67,252.00 | 60s Avg: $67,245.60 | Δ5s: +$3.20
💰 [19:31:25] BTC: $67,255.30 | 60s Avg: $67,247.80 | Δ5s: +$3.30
💰 [19:31:30] BTC: $67,253.10 | 60s Avg: $67,248.90 | Δ5s: -$2.20
   🟡 RRI: 2.8/10 ↑ — MODERATE — Caution, some signals warming
   [Delta: 0.8 | RSI: 1.0 | VWAP: 1.0 | BB: 0.40 | Wall: 0.0]

💰 [19:31:35] BTC: $67,249.50 | 60s Avg: $67,249.10 | Δ5s: -$3.60
💰 [19:31:40] BTC: $67,247.20 | 60s Avg: $67,249.00 | Δ5s: -$2.30
💰 [19:31:45] BTC: $67,244.80 | 60s Avg: $67,248.50 | Δ5s: -$2.40
   🟠 RRI: 4.7/10 ↑ — ELEVATED — Reversal pressure building
   [Delta: 2.1 | RSI: 1.5 | VWAP: 1.2 | BB: 0.50 | Wall: 0.5]

💰 [19:31:50] BTC: $67,242.10 | 60s Avg: $67,247.80 | Δ5s: -$2.70
💰 [19:31:55] BTC: $67,240.50 | 60s Avg: $67,247.00 | Δ5s: -$1.60
💰 [19:32:00] BTC: $67,241.80 | 60s Avg: $67,246.50 | Δ5s: +$1.30
   🟡 RRI: 3.6/10 ↓ — MODERATE — Caution, some signals warming
   [Delta: 0.0 | RSI: 0.0 | VWAP: 0.3 | BB: 0.00 | Wall: 0.0]
```

---

## RRI Score Examples

| Example | RRI | Label | Component Breakdown |
|---|---|---|---|
| All clear | 1.8 | 🟢 LOW RISK | Delta: 0.2, RSI: 0.3, VWAP: 0.8, BB: 0.5, Wall: 0.0 |
| Warming up | 3.2 | 🟡 MODERATE | Delta: 0.8, RSI: 1.0, VWAP: 1.0, BB: 0.4, Wall: 0.0 |
| Building | 5.8 | 🟠 ELEVATED | Delta: 2.1, RSI: 1.5, VWAP: 1.2, BB: 0.5, Wall: 0.5 |
| Danger | 7.2 | 🔴 HIGH RISK | Delta: 2.5, RSI: 2.0, VWAP: 1.5, BB: 0.7, Wall: 0.5 |
| Extreme | 8.4 | 🚨 EXTREME | Delta: 2.8, RSI: 2.5, VWAP: 1.6, BB: 0.5, Wall: 1.0 |

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
- **RRI summary** is printed every **15 seconds** at `:00`, `:15`, `:30`, `:45` of each minute.
- **60-second rolling average** (`60s Avg`) is a volume-weighted average of all trades in the last 60 seconds. If no trades have occurred in that window, the last known value is shown with a `⚠️ Stale` warning.
- **Δ5s** shows the price change since the previous 5-second tick.

---

## Disclaimer

This software is for **educational and informational purposes only**. It does not constitute financial advice. Binary options and cryptocurrency trading involve significant risk of loss. Always do your own research before trading.
