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
    /// Rolling 5-minute window: (price, volume, timestamp_ms).
    entries: VecDeque<(f64, f64, u64)>,
}

impl VwapState {
    fn new() -> Self {
        Self {
            entries: VecDeque::new(),
        }
    }

    fn update(&mut self, price: f64, vol: f64, timestamp_ms: u64) {
        self.entries.push_back((price, vol, timestamp_ms));
        // Drop entries older than 5 minutes.
        let cutoff_ms = timestamp_ms.saturating_sub(5 * 60 * 1_000);
        while let Some(&(_, _, ts)) = self.entries.front() {
            if ts < cutoff_ms {
                self.entries.pop_front();
            } else {
                break;
            }
        }
    }

    fn vwap(&self) -> f64 {
        let (cum_pv, cum_vol) = self.entries.iter().fold(
            (0.0f64, 0.0f64),
            |(pv, v), &(p, vol, _)| (pv + p * vol, v + vol),
        );
        if cum_vol == 0.0 { 0.0 } else { cum_pv / cum_vol }
    }

    fn std_dev(&self) -> f64 {
        let n = self.entries.len() as f64;
        if n < 2.0 {
            return 0.0;
        }
        let mean = self.entries.iter().map(|&(p, _, _)| p).sum::<f64>() / n;
        let var = self.entries.iter().map(|&(p, _, _)| (p - mean).powi(2)).sum::<f64>() / (n - 1.0);
        var.sqrt()
    }

    fn z_score(&self, price: f64) -> f64 {
        let vwap = self.vwap();
        let sd = self.std_dev();
        if vwap == 0.0 || sd == 0.0 {
            return 0.0;
        }
        (price - vwap) / sd
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
    /// Net delta accumulating in the current 15-second window.
    window_delta: f64,
    /// Price at the start of the current 15-second window.
    window_open_price: f64,
    /// Completed windows: (net_delta, open_price, close_price); keeps last 4
    /// (= 1 minute of lookback).
    completed_windows: VecDeque<(f64, f64, f64)>,
}

impl DeltaState {
    fn new() -> Self {
        Self {
            window_delta: 0.0,
            window_open_price: 0.0,
            completed_windows: VecDeque::new(),
        }
    }

    fn update(&mut self, price: f64, qty: f64, is_sell: bool) {
        // is_sell == true  → market SELL (negative delta)
        // is_sell == false → market BUY  (positive delta)
        if is_sell {
            self.window_delta -= qty;
        } else {
            self.window_delta += qty;
        }
        // Record the open price on the first trade of this window.
        if self.window_open_price == 0.0 {
            self.window_open_price = price;
        }
    }

    /// Called at each 15-second checkpoint to finalise the current window.
    fn finalize_window(&mut self, close_price: f64) {
        self.completed_windows.push_back((
            self.window_delta,
            self.window_open_price,
            close_price,
        ));
        if self.completed_windows.len() > 4 {
            self.completed_windows.pop_front();
        }
        self.window_delta = 0.0;
        self.window_open_price = close_price;
    }

    /// Returns the net delta accumulated in the current (not-yet-finalised) window.
    fn current_delta(&self) -> f64 {
        self.window_delta
    }

    /// Returns true if the most recently completed window shows divergence
    /// (price direction opposite to delta direction).
    fn check_divergence(&self) -> bool {
        let Some(&(delta, open, close)) = self.completed_windows.back() else {
            return false;
        };
        let price_up = close > open;
        let price_down = close < open;
        (price_up && delta < 0.0) || (price_down && delta > 0.0)
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

// ── Application state ──────────────────────────────────────────────────────

struct AppState {
    vwap: VwapState,
    rsi: RsiState,
    delta: DeltaState,
    bollinger: BollingerState,
    sell_wall: SellWallState,

    current_price: f64,
    trade_count: u64,
    /// 5-second tick counter, used to determine 15-second checkpoints.
    tick_count: u64,
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
            trade_count: 0,
            tick_count: 0,
            rolling_trades: VecDeque::new(),
            rolling_60s_avg: 0.0,
            prev_5s_price: 0.0,
        }
    }

    /// Process a `match` message: silently update state, no printing.
    /// RSI and Bollinger are updated at 5-second candle closes (in `on_tick`),
    /// not on every individual trade.
    fn handle_match(&mut self, price_str: &str, size_str: &str, side: &str) {
        let price: f64 = price_str.parse().unwrap_or(0.0);
        let qty: f64 = size_str.parse().unwrap_or(0.0);
        if price == 0.0 || qty == 0.0 {
            return;
        }

        self.current_price = price;
        self.trade_count += 1;

        // Record trade in the rolling buffer.
        let now_ms = Utc::now().timestamp_millis() as u64;
        self.rolling_trades.push_back(TimedTrade { price, volume: qty, timestamp_ms: now_ms });

        // Update rolling-window VWAP and current 15-second order flow delta.
        self.vwap.update(price, qty, now_ms);
        // Coinbase: side "sell" → taker was a seller → negative delta
        self.delta.update(price, qty, side == "sell");
    }

    /// Called on every 5-second tick. Updates 5-second candle-based indicators,
    /// evicts stale rolling trades, recalculates the 60-second average, and
    /// returns the formatted output lines (always a price line, plus a signal
    /// summary line every 15 seconds).
    fn on_tick(&mut self) -> Vec<String> {
        let mut output = Vec::new();

        if self.current_price == 0.0 {
            return output;
        }

        self.tick_count += 1;
        let now = Utc::now();
        let ts = now.format("%H:%M:%S");
        let now_ms = now.timestamp_millis() as u64;

        // Update 5-second candle-based indicators with the current close price.
        self.rsi.update(self.current_price);
        self.bollinger.update(self.current_price);

        // Evict trades older than 60 seconds from the rolling buffer.
        let cutoff_ms = now_ms.saturating_sub(60_000);
        while self.rolling_trades.front().map_or(false, |t| t.timestamp_ms < cutoff_ms) {
            self.rolling_trades.pop_front();
        }

        // Calculate the 60-second volume-weighted average.
        let avg_str = if self.rolling_trades.is_empty() {
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

        let delta_str = if self.prev_5s_price > 0.0 {
            let delta_5s = self.current_price - self.prev_5s_price;
            format!(" | Δ5s: {}", format_usd_change(delta_5s))
        } else {
            String::new()
        };

        self.prev_5s_price = self.current_price;

        // Price line — printed every 5 seconds.
        output.push(format!(
            "💰 [{ts}] BTC: {} | 60s Avg: {avg_str}{delta_str}",
            format_usd(self.current_price)
        ));

        // Signal check — printed every 15 seconds (every 3rd tick).
        if self.tick_count % 3 == 0 {
            let price = self.current_price;

            // Get current window delta before finalising.
            let delta_val = self.delta.current_delta();
            // Finalise the 15-second delta window.
            self.delta.finalize_window(price);
            // Evaluate sell wall at the 15-second checkpoint.
            self.sell_wall.update_signal(price);

            // Collect signal values.
            let rsi_val = self.rsi.rsi(2);
            let rsi_signal = self.rsi.check_signal();
            let divergence = self.delta.check_divergence();
            let bb_width = self.bollinger.band_width();
            let bb_squeeze = bb_width.map_or(false, |w| w < 0.0005);
            let vwap_z = self.vwap.z_score(price);
            let vwap_warn = vwap_z.abs() > 1.5;
            let sell_wall_sig = self.sell_wall.check_signal().map(|s| s.to_string());

            // Count active warnings.
            let mut warn_count = 0usize;
            if rsi_signal.is_some() { warn_count += 1; }
            if divergence { warn_count += 1; }
            if bb_squeeze { warn_count += 1; }
            if vwap_warn { warn_count += 1; }
            if sell_wall_sig.is_some() { warn_count += 1; }

            // Danger zone: last 15 seconds of the minute (:45 checkpoint).
            let is_danger = seconds_into_block() >= 45;

            let signal_line = if is_danger && warn_count >= 2 {
                let mut parts: Vec<String> = Vec::new();
                if let Some(s) = rsi_signal {
                    parts.push(s);
                }
                let sign = if delta_val >= 0.0 { "+" } else { "" };
                parts.push(format!("Delta: {sign}{:.2}", delta_val));
                if vwap_warn {
                    parts.push(format!("VWAP Z: {:.1}", vwap_z));
                }
                if let Some(sw) = sell_wall_sig {
                    parts.push(sw);
                }
                format!(
                    "   🚨 [DANGER ZONE] {} — {warn_count}/5 DO NOT ENTER!",
                    parts.join(" | ")
                )
            } else {
                let rsi_str = rsi_val.map_or("N/A".to_string(), |r| format!("{:.1}", r));
                let sign = if delta_val >= 0.0 { "+" } else { "" };
                let div_tag = if divergence { " (DIVERGENCE)" } else { "" };
                let bb_str = bb_width.map_or("N/A".to_string(), |w| {
                    let sq_tag = if bb_squeeze { " (SQUEEZE)" } else { "" };
                    format!("{:.2}%{sq_tag}", w * 100.0)
                });
                let content = format!(
                    "RSI: {rsi_str} | Delta: {sign}{:.2}{div_tag} | BB Width: {bb_str} | VWAP Z: {:.1}",
                    delta_val, vwap_z
                );
                if warn_count == 0 {
                    format!("   📈 [SIGNAL CHECK] {content} — ✅ No warnings")
                } else {
                    let sw_part = sell_wall_sig
                        .as_deref()
                        .map_or(String::new(), |s| format!(" | {s}"));
                    let plural = if warn_count == 1 { "" } else { "s" };
                    format!("   ⚠️ [SIGNAL CHECK] {content}{sw_part} — {warn_count} warning{plural}")
                }
            };

            output.push(signal_line);
        }

        output
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
                                // Sell wall signal is evaluated at the 15-second
                                // checkpoint, not on every depth update.
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
                for line in state.on_tick() {
                    println!("{line}");
                }
            }
        }
    }
}
