use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use url::Url;

// ── Coinbase message types ──────────────────────────────────────────────────

/// Parsed subset of messages from the Coinbase Exchange WebSocket feed.
#[derive(Deserialize, Debug)]
#[serde(tag = "type")]
enum CoinbaseMessage {
    /// Real-time trade from the `matches` channel.
    #[serde(rename = "match")]
    Match {
        price: String,
        size: String,
        /// "buy"  → taker was a buyer  → positive delta
        /// "sell" → taker was a seller → negative delta
        side: String,
    },
    /// Initial order-book snapshot from the `level2_batch` channel.
    #[serde(rename = "snapshot")]
    Snapshot {
        asks: Vec<[String; 2]>,
    },
    /// Incremental order-book update from the `level2_batch` channel.
    #[serde(rename = "l2update")]
    L2Update {
        /// Each element is `[side, price, size]`; size "0" means remove the level.
        changes: Vec<[String; 3]>,
    },
    /// Any other message type (subscriptions confirmation, heartbeats, …).
    #[serde(other)]
    Other,
}

// ── Rolling trade record ───────────────────────────────────────────────────

struct TimedTrade {
    price: f64,
    volume: f64,
    timestamp_ms: u64,
}

// ── Indicator state ────────────────────────────────────────────────────────

struct VwapState {
    cum_vol: f64,
    cum_pv: f64,
    prices: VecDeque<f64>,
}

impl VwapState {
    fn new() -> Self {
        Self {
            cum_vol: 0.0,
            cum_pv: 0.0,
            prices: VecDeque::new(),
        }
    }

    fn update(&mut self, price: f64, vol: f64) {
        self.cum_vol += vol;
        self.cum_pv += price * vol;
        self.prices.push_back(price);
        // keep a rolling window for std-dev calculation (last 200 ticks)
        if self.prices.len() > 200 {
            self.prices.pop_front();
        }
    }

    fn vwap(&self) -> f64 {
        if self.cum_vol == 0.0 {
            return 0.0;
        }
        self.cum_pv / self.cum_vol
    }

    fn std_dev(&self) -> f64 {
        let n = self.prices.len() as f64;
        if n < 2.0 {
            return 0.0;
        }
        let mean = self.prices.iter().sum::<f64>() / n;
        let var = self.prices.iter().map(|p| (p - mean).powi(2)).sum::<f64>() / (n - 1.0);
        var.sqrt()
    }

    /// Returns Some(signal_text) if price deviates > 1.5σ from VWAP
    fn check_signal(&self, price: f64) -> Option<String> {
        let vwap = self.vwap();
        let sd = self.std_dev();
        if vwap == 0.0 || sd == 0.0 {
            return None;
        }
        let deviation = (price - vwap).abs();
        if deviation > 1.5 * sd {
            let side = if price > vwap { "ABOVE" } else { "BELOW" };
            return Some(format!(
                "⚠️  VWAP: Price ${:.2} is {side} VWAP ${:.2} by {:.1}σ — mean-reversion risk",
                price,
                vwap,
                deviation / sd,
            ));
        }
        None
    }
}

// ── Fast RSI (2-period) ────────────────────────────────────────────────────

struct RsiState {
    prices: VecDeque<f64>,
    prev_rsi: Option<f64>,
}

impl RsiState {
    fn new() -> Self {
        Self {
            prices: VecDeque::new(),
            prev_rsi: None,
        }
    }

    fn update(&mut self, price: f64) {
        self.prices.push_back(price);
        if self.prices.len() > 50 {
            self.prices.pop_front();
        }
    }

    /// Wilder RSI over `period` using the stored price window
    fn rsi(&self, period: usize) -> Option<f64> {
        if self.prices.len() < period + 1 {
            return None;
        }
        let recent: Vec<f64> = self.prices.iter().rev().take(period + 1).cloned().collect();
        let mut gains = 0.0f64;
        let mut losses = 0.0f64;
        for i in 0..period {
            let change = recent[i] - recent[i + 1]; // newest first
            if change > 0.0 {
                gains += change;
            } else {
                losses += change.abs();
            }
        }
        let avg_gain = gains / period as f64;
        let avg_loss = losses / period as f64;
        if avg_loss == 0.0 {
            return Some(100.0);
        }
        let rs = avg_gain / avg_loss;
        Some(100.0 - 100.0 / (1.0 + rs))
    }

    /// Returns Some(signal_text) on RSI hook detection, updates prev_rsi
    fn check_signal(&mut self) -> Option<String> {
        let current_rsi = self.rsi(2)?;
        let result = if let Some(prev) = self.prev_rsi {
            if prev >= 95.0 && current_rsi < prev {
                Some(format!(
                    "🚨 RSI HOOK DOWN: RSI {:.1} → {:.1} — buying exhaustion detected!",
                    prev, current_rsi
                ))
            } else if prev <= 5.0 && current_rsi > prev {
                Some(format!(
                    "🚨 RSI HOOK UP: RSI {:.1} → {:.1} — selling exhaustion detected!",
                    prev, current_rsi
                ))
            } else {
                None
            }
        } else {
            None
        };
        self.prev_rsi = Some(current_rsi);
        result
    }
}

// ── Order Flow Delta ───────────────────────────────────────────────────────

struct DeltaState {
    cumulative_delta: f64,
    prev_block_delta: f64,
    prev_block_price: f64,
    block_open_price: f64,
    trade_count: u64,
}

impl DeltaState {
    fn new() -> Self {
        Self {
            cumulative_delta: 0.0,
            prev_block_delta: 0.0,
            prev_block_price: 0.0,
            block_open_price: 0.0,
            trade_count: 0,
        }
    }

    fn update(&mut self, price: f64, qty: f64, is_sell: bool) {
        // is_sell == true  → market SELL (negative delta)
        // is_sell == false → market BUY  (positive delta)
        if is_sell {
            self.cumulative_delta -= qty;
        } else {
            self.cumulative_delta += qty;
        }
        self.trade_count += 1;
        if self.trade_count == 1 {
            self.block_open_price = price;
        }
    }

    fn reset_block(&mut self, current_price: f64) {
        self.prev_block_delta = self.cumulative_delta;
        self.prev_block_price = current_price;
        self.cumulative_delta = 0.0;
        self.block_open_price = current_price;
        self.trade_count = 0;
    }

    /// Returns Some(signal_text) if bullish/bearish divergence is detected
    fn check_signal(&self, current_price: f64) -> Option<String> {
        if self.prev_block_price == 0.0 {
            return None;
        }
        let price_up = current_price > self.prev_block_price;
        let price_down = current_price < self.prev_block_price;
        let delta_up = self.cumulative_delta > self.prev_block_delta;
        let delta_down = self.cumulative_delta < self.prev_block_delta;

        if price_up && delta_down {
            return Some(format!(
                "🔶 BEARISH DIVERGENCE: Price ↑ ${:.2} but delta ↓ {:.3} — absorption (sellers absorbing buyers)",
                current_price, self.cumulative_delta
            ));
        }
        if price_down && delta_up {
            return Some(format!(
                "🔶 BULLISH DIVERGENCE: Price ↓ ${:.2} but delta ↑ {:.3} — accumulation (buyers absorbing sellers)",
                current_price, self.cumulative_delta
            ));
        }
        None
    }
}

// ── Bollinger Band Width ───────────────────────────────────────────────────

struct BollingerState {
    prices: VecDeque<f64>,
    period: usize,
}

impl BollingerState {
    fn new(period: usize) -> Self {
        Self {
            prices: VecDeque::new(),
            period,
        }
    }

    fn update(&mut self, price: f64) {
        self.prices.push_back(price);
        if self.prices.len() > self.period {
            self.prices.pop_front();
        }
    }

    fn band_width(&self) -> Option<f64> {
        if self.prices.len() < self.period {
            return None;
        }
        let n = self.period as f64;
        let mean = self.prices.iter().sum::<f64>() / n;
        if mean == 0.0 {
            return None;
        }
        let var = self.prices.iter().map(|p| (p - mean).powi(2)).sum::<f64>() / n;
        // Bollinger Bands conventionally use population std dev (÷ n), not
        // the unbiased estimator (÷ n-1) used by VWAP's rolling std dev.
        let sd = var.sqrt();
        let upper = mean + 2.0 * sd;
        let lower = mean - 2.0 * sd;
        Some((upper - lower) / mean)
    }

    /// Returns Some(signal_text) when band width < 0.05% (squeeze)
    fn check_signal(&self) -> Option<String> {
        let bw = self.band_width()?;
        if bw < 0.0005 {
            Some(format!(
                "⚠️  BB SQUEEZE: Band width {:.4}% < 0.05% — explosive breakout imminent!",
                bw * 100.0
            ))
        } else {
            None
        }
    }
}

// ── Sell Wall Detection ────────────────────────────────────────────────────

/// Converts a USD price to an integer cent key for use in the ask BTreeMap.
#[inline]
fn price_to_cents(price: f64) -> u64 {
    (price * 100.0).round() as u64
}

struct SellWallState {
    /// Ask-side order book: price in cents (price * 100 rounded) → size in BTC.
    asks: BTreeMap<u64, f64>,
    last_signal: Option<String>,
}

impl SellWallState {
    fn new() -> Self {
        Self {
            asks: BTreeMap::new(),
            last_signal: None,
        }
    }

    /// Replace the entire ask book from a Coinbase `snapshot` message.
    fn apply_snapshot(&mut self, asks: &[[String; 2]]) {
        self.asks.clear();
        for level in asks {
            let price: f64 = level[0].parse().unwrap_or(0.0);
            let size: f64 = level[1].parse().unwrap_or(0.0);
            if price > 0.0 && size > 0.0 {
                self.asks.insert(price_to_cents(price), size);
            }
        }
    }

    /// Apply incremental changes from a Coinbase `l2update` message.
    /// Each change is `[side, price, size]`; size "0" removes the level.
    fn apply_changes(&mut self, changes: &[[String; 3]]) {
        for change in changes {
            if change[0] == "sell" {
                let price: f64 = change[1].parse().unwrap_or(0.0);
                let size: f64 = change[2].parse().unwrap_or(0.0);
                if price > 0.0 {
                    let key = price_to_cents(price);
                    if size == 0.0 {
                        self.asks.remove(&key);
                    } else {
                        self.asks.insert(key, size);
                    }
                }
            }
        }
    }

    /// Scan the current ask book and update `last_signal`.
    fn update_signal(&mut self, current_price: f64) {
        if current_price == 0.0 {
            self.last_signal = None;
            return;
        }
        let threshold = current_price * 1.001; // 0.1% above current price
        let min_key = price_to_cents(current_price);
        let max_key = price_to_cents(threshold);
        // A "sell wall" is a large ask order that sits just above the current
        // price and acts as a ceiling.  We require: (a) within 0.1% of price
        // so it is immediately relevant, and (b) ≥ 10 BTC so it is large
        // enough to meaningfully absorb market buys.
        for (&price_cents, &size) in self.asks.range(min_key..=max_key) {
            if size >= 10.0 {
                let ask_price = price_cents as f64 * 0.01;
                self.last_signal = Some(format!(
                    "🧱 SELL WALL: {:.3} BTC ask at ${:.2} ({:.3}% above price) — 95% YES is a trap!",
                    size,
                    ask_price,
                    (ask_price - current_price) / current_price * 100.0,
                ));
                return;
            }
        }
        self.last_signal = None;
    }

    fn check_signal(&self) -> Option<&str> {
        self.last_signal.as_deref()
    }
}

// ── Formatting helpers ─────────────────────────────────────────────────────

/// Formats an absolute USD value as `$X,XXX.XX` with comma-separated thousands.
fn format_usd(value: f64) -> String {
    let abs_val = value.abs();
    let cents = (abs_val * 100.0).round() as u64;
    let whole = cents / 100;
    let frac = (cents % 100) as u32;

    let whole_str = whole.to_string();
    let len = whole_str.len();
    let mut comma_str = String::new();
    for (i, ch) in whole_str.chars().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            comma_str.push(',');
        }
        comma_str.push(ch);
    }

    format!("${comma_str}.{frac:02}")
}

/// Formats a signed USD change as `+$X,XXX.XX` or `-$X,XXX.XX`.
fn format_usd_change(value: f64) -> String {
    let sign = if value >= 0.0 { "+" } else { "-" };
    format!("{sign}{}", format_usd(value.abs()))
}

// ── Timing helpers ─────────────────────────────────────────────────────────

/// Returns how many seconds we are into the current 1-minute block (0–59).
fn seconds_into_block() -> u64 {
    let now = Utc::now();
    now.timestamp() as u64 % 60
}

fn is_danger_zone() -> bool {
    seconds_into_block() >= 44
}

// ── Application state ──────────────────────────────────────────────────────

struct AppState {
    vwap: VwapState,
    rsi: RsiState,
    delta: DeltaState,
    bollinger: BollingerState,
    sell_wall: SellWallState,

    current_price: f64,
    current_minute: u32,
    trade_count: u64,
    /// Per-block trade counter used for danger-zone cadence.
    block_trade_count: u64,
    /// Rolling 60-second trade buffer for volume-weighted average.
    rolling_trades: VecDeque<TimedTrade>,
    /// Most recently calculated 60-second rolling average (kept for "Stale" display).
    rolling_60s_avg: f64,
    /// Price at the previous 5-second tick, for Δ5s calculation.
    prev_5s_price: f64,
}

impl AppState {
    fn new() -> Self {
        Self {
            vwap: VwapState::new(),
            rsi: RsiState::new(),
            delta: DeltaState::new(),
            bollinger: BollingerState::new(20),
            sell_wall: SellWallState::new(),

            current_price: 0.0,
            current_minute: 0,
            trade_count: 0,
            block_trade_count: 0,
            rolling_trades: VecDeque::new(),
            rolling_60s_avg: 0.0,
            prev_5s_price: 0.0,
        }
    }

    fn maybe_reset_block(&mut self) {
        let minute = Utc::now().timestamp() as u32 / 60;
        if minute != self.current_minute {
            // Fallback state reset when a trade crosses a minute boundary before
            // the clock-driven timer has had a chance to fire.
            self.current_minute = minute;
            self.vwap = VwapState::new();
            self.delta.reset_block(self.current_price);
            self.block_trade_count = 0;
        }
    }

    /// Process a `match` message from the Coinbase `matches` channel.
    fn handle_match(&mut self, price_str: &str, size_str: &str, side: &str) {
        let price: f64 = price_str.parse().unwrap_or(0.0);
        let qty: f64 = size_str.parse().unwrap_or(0.0);
        if price == 0.0 || qty == 0.0 {
            return;
        }

        self.maybe_reset_block();
        self.current_price = price;
        self.trade_count += 1;
        self.block_trade_count += 1;

        // Record trade in the rolling buffer.
        let now_ms = Utc::now().timestamp_millis() as u64;
        self.rolling_trades.push_back(TimedTrade { price, volume: qty, timestamp_ms: now_ms });

        // Update per-minute indicators.
        self.vwap.update(price, qty);
        self.rsi.update(price);
        // Coinbase: side "sell" → taker was a seller → negative delta
        self.delta.update(price, qty, side == "sell");
        self.bollinger.update(price);

        // Periodic RSI hook check.
        let rsi_signal = self.rsi.check_signal();

        // Danger zone logic.
        if is_danger_zone() {
            let sec = seconds_into_block();
            let remaining = 60u64.saturating_sub(sec);

            // Print danger zone header once per 10-trade cadence in the zone.
            if self.block_trade_count % 10 == 0 {
                println!("🔴 DANGER ZONE — Second {}/60 — Last {} seconds!", sec, remaining);
            }

            // Gather all active signals.
            let mut signals: Vec<String> = Vec::new();

            if let Some(s) = self.vwap.check_signal(price) {
                signals.push(s);
            }
            if let Some(s) = rsi_signal {
                signals.push(s);
            }
            if let Some(s) = self.delta.check_signal(price) {
                signals.push(s);
            }
            if let Some(s) = self.bollinger.check_signal() {
                signals.push(s);
            }
            if let Some(s) = self.sell_wall.check_signal() {
                signals.push(s.to_string());
            }

            for s in &signals {
                println!("{s}");
            }

            if signals.len() >= 2 {
                println!(
                    "\n🚨🚨🚨 MULTIPLE REVERSAL SIGNALS ({}/5)! DO NOT ENTER even at 95% probability! 🚨🚨🚨\n",
                    signals.len()
                );
            }
        } else if let Some(s) = rsi_signal {
            // Still print RSI hooks outside the danger zone.
            println!("{s}");
        }
    }

    /// Called on every 5-second tick. Evicts stale rolling trades, recalculates
    /// the 60-second average, and returns the formatted status line.
    fn on_tick(&mut self) -> Option<String> {
        if self.current_price == 0.0 {
            return None;
        }

        let now = Utc::now();
        let ts = now.format("%H:%M:%S");
        let now_ms = now.timestamp_millis() as u64;

        // Evict trades older than 60 seconds from the rolling buffer.
        let cutoff_ms = now_ms.saturating_sub(60_000);
        while let Some(front) = self.rolling_trades.front() {
            if front.timestamp_ms < cutoff_ms {
                self.rolling_trades.pop_front();
            } else {
                break;
            }
        }

        // Calculate the 60-second volume-weighted average.
        let avg_str = if self.rolling_trades.is_empty() {
            // No trades in the last 60 s — show last known value with a warning.
            if self.rolling_60s_avg > 0.0 {
                format!("{} ⚠️ Stale", format_usd(self.rolling_60s_avg))
            } else {
                "N/A".to_string()
            }
        } else {
            let (total_pv, total_vol) = self.rolling_trades.iter().fold(
                (0.0f64, 0.0f64),
                |(pv, v), t| (pv + t.price * t.volume, v + t.volume),
            );
            if total_vol > 0.0 {
                self.rolling_60s_avg = total_pv / total_vol;
            }
            format_usd(self.rolling_60s_avg)
        };

        let vwap = self.vwap.vwap();
        let vwap_str = if vwap > 0.0 {
            format!(" | VWAP: {}", format_usd(vwap))
        } else {
            String::new()
        };

        let delta_str = if self.prev_5s_price > 0.0 {
            let delta_5s = self.current_price - self.prev_5s_price;
            format!(" | Δ5s: {}", format_usd_change(delta_5s))
        } else {
            String::new()
        };

        self.prev_5s_price = self.current_price;

        Some(format!(
            "💰 [{ts}] BTC Price: {} | 60s Avg: {avg_str}{vwap_str}{delta_str}",
            format_usd(self.current_price)
        ))
    }
}

// ── WebSocket task helpers ─────────────────────────────────────────────────

const COINBASE_URL: &str = "wss://ws-feed.exchange.coinbase.com";

/// Subscribe message sent to Coinbase immediately after connecting.
const SUBSCRIBE_MSG: &str =
    r#"{"type":"subscribe","product_ids":["BTC-USD"],"channels":["matches","level2_batch"]}"#;

/// Connect to the Coinbase Exchange WebSocket and send the subscribe message.
/// Retries indefinitely on failure.
async fn connect_ws_coinbase() -> tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
> {
    let url = Url::parse(COINBASE_URL).expect("invalid URL");
    loop {
        match connect_async(url.clone()).await {
            Ok((mut ws, _)) => {
                println!("✅ Connected to {COINBASE_URL}");
                match ws.send(Message::Text(SUBSCRIBE_MSG.to_string())).await {
                    Ok(_) => return ws,
                    Err(e) => {
                        eprintln!("⚠️  Subscribe failed ({e}), retrying in 3s …");
                        tokio::time::sleep(Duration::from_secs(3)).await;
                    }
                }
            }
            Err(e) => {
                eprintln!("⚠️  Connection failed ({e}), retrying in 3s …");
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
}

// ── Entry point ────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    println!("🚀 Reversal Terminator — BTC/USD monitor starting…");
    println!("   Connecting to Coinbase Exchange WebSocket…\n");

    let ws = connect_ws_coinbase().await;
    let mut stream = ws.fuse();

    let mut state = AppState::new();
    // 5-second tick drives the status line; no initial alignment sleep needed.
    let mut ticker = tokio::time::interval(Duration::from_secs(5));

    println!("📡 Streaming live data — watching for reversals…\n");

    loop {
        tokio::select! {
            msg = stream.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<CoinbaseMessage>(&text) {
                            Ok(CoinbaseMessage::Match { price, size, side }) => {
                                state.handle_match(&price, &size, &side);
                            }
                            Ok(CoinbaseMessage::Snapshot { asks }) => {
                                state.sell_wall.apply_snapshot(&asks);
                            }
                            Ok(CoinbaseMessage::L2Update { changes }) => {
                                state.sell_wall.apply_changes(&changes);
                                if state.current_price > 0.0 {
                                    state.sell_wall.update_signal(state.current_price);
                                }
                            }
                            _ => {}
                        }
                    }
                    Some(Err(e)) => eprintln!("WebSocket error: {e}"),
                    None => {
                        eprintln!("WebSocket closed, reconnecting…");
                        let ws = connect_ws_coinbase().await;
                        stream = ws.fuse();
                    }
                    _ => {}
                }
            }
            _ = ticker.tick() => {
                if let Some(line) = state.on_tick() {
                    println!("{line}");
                }
            }
        }
    }
}
