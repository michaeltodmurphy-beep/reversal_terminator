# Reversal Terminator 🚀

A real-time **BTC/USD reversal detection system** written in Rust that connects to the **Coinbase Exchange WebSocket** and monitors the live BTC-USD market. It uses 5 technical signals combined into a single **Reversal Risk Index (RRI)** to flag last-minute reversals — designed for binary options trading (e.g. [Kalshi](https://kalshi.com)).

---

## What it does

The system opens a single connection to the **Coinbase Exchange WebSocket** (`wss://ws-feed.exchange.coinbase.com`) and subscribes to two channels:

| Channel | Purpose |
|---|---|
| `matches` | Real-time trade price, size, and buy/sell side |
| `level2_batch` | Level-2 order book snapshot + incremental updates |

Every **1 second** (configurable) the current price and 30-second rolling VWAP are printed with percentage changes. Every **5 seconds** (every 5th tick, configurable) a **Reversal Risk Index (RRI)** is printed.

Optionally, real-time **Kalshi YES/NO bid prices** for the nearest ATM 15-minute BTC contract are shown on a second line every tick.

---

## Configuration

Copy `config.example.json` to `config.json` and edit to your preferences:

```json
{
  "rri_alpha": 0.5,
  "tick_interval_secs": 1,
  "rri_check_every_n_ticks": 5,
  "log_file": "rri_log.csv",
  "log_retention_hours": 12,
  "kalshi_api_key": "",
  "kalshi_api_secret": "",
  "kalshi_event_ticker": "KXBTCD"
}
```

> **Note**: `config.json` is in `.gitignore` so your API keys are never committed.  
> `config.example.json` is the safe template — fill in your keys in `config.json`.

| Key | Default | What it does |
|---|---|---|
| `rri_alpha` | `0.5` | EMA smoothing factor. Raise (e.g. `0.7`) for raw-like responsiveness, lower (e.g. `0.3`) for more dampening |
| `tick_interval_secs` | `1` | Price line printed every N seconds |
| `rri_check_every_n_ticks` | `5` | RRI fires every Nth tick (5 × 1s = every 5s) |
| `log_file` | `"rri_log.csv"` | Where RRI readings are appended |
| `log_retention_hours` | `12` | Prune log entries older than this |
| `kalshi_api_key` | `""` | Kalshi account email. Leave empty to disable Kalshi |
| `kalshi_api_secret` | `""` | Kalshi account password. Leave empty to disable Kalshi |
| `kalshi_event_ticker` | `"KXBTCD"` | Kalshi event ticker prefix for 15-minute BTC contracts |

---

## Reversal Risk Index (RRI)

Each indicator contributes a weighted sub-score. The raw RRI is their sum, floored at 1.0 and capped at 10.0. To eliminate erratic jumps, the displayed RRI is smoothed with an **Exponential Moving Average (EMA)** before it is shown.

```rust
let raw_rri = (delta_score + rsi_score + vwap_score + bb_score + wall_score).clamp(1.0, 10.0);
// EMA smoothing (α = 0.5, ~10s convergence at 1s ticks):
smoothed_rri = 0.5 * raw_rri + 0.5 * smoothed_rri;
```

### EMA Smoothing

| Parameter | Value | Effect |
|-----------|-------|--------|
| Alpha (α) | 0.5 | ~2 readings / ~10 second smoothing window |
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

### 2. Fast RSI — 2-Period RSI on 1-Second Closes
A very short (2-period) Relative Strength Index calculated on **1-second candle closes**. RSI is computed from the last 50 closes.

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

### 4. Bollinger Band Width — 1-Second Closes
Maintains a 20-period Bollinger Band on **1-second closes** (= 20 seconds of data). Updated every tick.

**Scoring** (band width percentage):
- `0.0`  = band width > 0.10% (normal volatility)
- `0.5`  = band width 0.05–0.10% (narrowing)
- `0.75` = band width 0.03–0.05% (tight squeeze)
- `1.5`  = band width < 0.03% (extreme squeeze)

### 5. Sell Wall Detection
Scans the ask side of the live order book for large limit orders close to the current price. The order book is kept up-to-date from depth snapshots and incremental updates; the wall score is **only evaluated at the RRI checkpoint**, not on every depth update.

**Scoring**:
- `0.0` = No significant wall nearby
- `0.3` = Wall (> 5 BTC) within 0.2% of current price
- `0.5` = Wall (> 10 BTC) within 0.1% of current price
- `1.0` = Massive wall (> 20 BTC) within 0.05% of current price

---

## Output Format

```
💰 [21:35:03] BTC: $70,648.36 (+0.12%) | 30s Avg: $70,645.20 (+0.04%)
   📊 Kalshi YES: $0.62 (+3.3%) | NO: $0.38 (-5.0%)
   🟡 RRI: 4.3/10 ↑ — MODERATE — Caution, some signals warming
   [Delta: 1.5 | RSI: 2.0 | VWAP: 0.5 | BB: 1.50 | Wall: 0.0]
```

- **Line 1** — BTC price every tick, with % change vs previous tick + 30s VWAP with % change
- **Line 2** — Kalshi YES/NO bid prices (only shown when Kalshi credentials are configured)
- **Lines 3–4** — RRI signal, printed every `rri_check_every_n_ticks` ticks

---

## Sample Output

```
🚀 Reversal Terminator — BTC/USD monitor starting…
   Connecting to Coinbase Exchange WebSocket…

✅ Connected to wss://ws-feed.exchange.coinbase.com
📡 Streaming live data — watching for reversals…

💰 [21:35:01] BTC: $70,648.36 (+0.00%) | 30s Avg: $70,645.20 (+0.00%)
💰 [21:35:02] BTC: $70,648.80 (+0.01%) | 30s Avg: $70,645.50 (+0.00%)
💰 [21:35:03] BTC: $70,649.30 (+0.01%) | 30s Avg: $70,645.90 (+0.01%)
💰 [21:35:04] BTC: $70,649.00 (-0.00%) | 30s Avg: $70,645.85 (-0.00%)
💰 [21:35:05] BTC: $70,648.70 (-0.00%) | 30s Avg: $70,645.70 (-0.00%)
   🟡 RRI: 4.3/10 ↑ — MODERATE — Caution, some signals warming
   [Delta: 1.5 | RSI: 2.0 | VWAP: 0.5 | BB: 1.50 | Wall: 0.0]
```

---

## RRI Log (`rri_log.csv`)

Every RRI checkpoint appends a line:

```csv
timestamp,smoothed_rri,raw_rri,trend,label,delta,rsi,vwap,bb,wall
2026-03-11T14:31:15Z,4.3,5.5,↑,MODERATE,1.5,2.0,0.5,1.50,0.0
```

The log auto-prunes entries older than `log_retention_hours` (triggered every ~100 writes).

---

## Build & Run

### Prerequisites

- [Rust toolchain](https://rustup.rs/) (stable, edition 2021)

### Setup

```bash
cp config.example.json config.json
# Edit config.json to add your Kalshi credentials (optional)
```

### Build

```bash
cargo build --release
```

### Run

```bash
cargo run --release
```

> **Note**: This application connects to **Coinbase Exchange** (`wss://ws-feed.exchange.coinbase.com`), which is fully available in the US with no regional restrictions. No API keys are required for the BTC price feed — it is public.

---

## Kalshi Integration

When `kalshi_api_key` and `kalshi_api_secret` are set in `config.json`, the app will:

1. Authenticate with the Kalshi REST API
2. Find the nearest ATM 15-minute BTC contract currently open
3. Open a WebSocket to stream real-time YES/NO bid prices
4. Automatically roll to the new contract as 15-minute windows expire

If credentials are empty, the Kalshi line is simply not printed.

---

## Disclaimer

This software is for **educational and informational purposes only**. It does not constitute financial advice. Binary options and cryptocurrency trading involve significant risk of loss. Always do your own research before trading.
