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

    /// Returns a score 0.0–2.0 for VWAP deviation (standard deviations from VWAP).
    ///
    /// - 0.0 = within 0.5σ
    /// - 0.5 = 0.5–1.0σ
    /// - 1.0 = 1.0–1.5σ
    /// - 1.5 = 1.5–2.0σ
    /// - 2.0 = > 2.0σ
    fn score_vwap_deviation(&self, price: f64) -> f64 {
        let z = self.z_score(price).abs();
        if z > 2.0 {
            2.0
        } else if z > 1.5 {
            1.5
        } else if z > 1.0 {
            1.0
        } else if z > 0.5 {
            0.5
        } else {
            0.0
        }
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

    /// Returns a score 0.0–2.5 representing RSI exhaustion level, and updates prev_rsi.
    ///
    /// - 0.0  = RSI 30–70 (neutral)
    /// - 0.5  = RSI 20–30 or 70–80 (warming up)
    /// - 1.0  = RSI 10–20 or 80–90 (extended)
    /// - 2.0  = RSI < 10 or > 90 (extreme)
    /// - 2.5  = RSI hook from extreme (was > 90 turning down, or was < 10 turning up)
    fn score_rsi_exhaustion(&mut self) -> f64 {
        let current = match self.rsi(2) {
            Some(r) => r,
            None => {
                self.prev_rsi = None;
                return 0.0;
            }
        };
        let score = if let Some(prev) = self.prev_rsi {
            if (prev > 90.0 && current < prev) || (prev < 10.0 && current > prev) {
                2.5 // hook from extreme
            } else if !(10.0..=90.0).contains(&current) {
                2.0
            } else if !(20.0..=80.0).contains(&current) {
                1.0
            } else if !(30.0..=70.0).contains(&current) {
                0.5
            } else {
                0.0
            }
        } else if !(10.0..=90.0).contains(&current) {
            2.0
        } else if !(20.0..=80.0).contains(&current) {
            1.0
        } else if !(30.0..=70.0).contains(&current) {
            0.5
        } else {
            0.0
        };
        self.prev_rsi = Some(current);
        score
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

    /// Returns a score 0.0–3.0 for order-flow delta divergence.
    ///
    /// - 0.0 = No divergence (price and delta in the same direction)
    /// - 1.5 = Mild divergence (price moving one way, delta flat or weakly opposite)
    /// - 3.0 = Strong divergence (price moving one way, delta strongly the other)
    fn score_delta_divergence(&self) -> f64 {
        let Some(&(delta, open, close)) = self.completed_windows.back() else {
            return 0.0;
        };
        let price_change = close - open;
        if price_change == 0.0 || delta == 0.0 {
            return 0.0;
        }
        let price_up = price_change > 0.0;
        let delta_up = delta > 0.0;
        if price_up == delta_up {
            return 0.0; // same direction — no divergence
        }
        // Divergence confirmed.  Classify as strong if both the price move and
        // the opposing delta are significant: price moved at least 0.01% (1 bp)
        // and delta reached at least 0.5 BTC in the opposite direction.
        let price_pct = (price_change / open).abs();
        let delta_abs = delta.abs();
        if price_pct >= 0.0001 && delta_abs >= 0.5 {
            3.0
        } else {
            1.5
        }
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

    /// Returns a score 0.0–1.5 for Bollinger Band squeeze (based on band width %).
    ///
    /// - 0.0  = band width > 0.10% (normal volatility)
    /// - 0.5  = band width 0.05–0.10% (narrowing)
    /// - 0.75 = band width 0.03–0.05% (tight squeeze)
    /// - 1.5  = band width < 0.03% (extreme squeeze)
    fn score_bollinger_squeeze(&self) -> f64 {
        match self.band_width() {
            None => 0.0,
            Some(w) => {
                if w < 0.0003 {
                    1.5 // < 0.03%
                } else if w < 0.0005 {
                    0.75 // 0.03–0.05%
                } else if w < 0.001 {
                    0.5 // 0.05–0.10%
                } else {
                    0.0 // > 0.10%
                }
            }
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
}

impl SellWallState {
    fn new() -> Self {
        Self {
            asks: BTreeMap::new(),
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

    /// Returns a score 0.0–1.0 for sell-wall proximity.
    ///
    /// - 0.0 = No significant wall nearby
    /// - 0.3 = Wall > 5 BTC within 0.2% of current price
    /// - 0.5 = Wall > 10 BTC within 0.1% of current price
    /// - 1.0 = Massive wall > 20 BTC within 0.05% of current price
    fn score_wall_proximity(&self, current_price: f64) -> f64 {
        if current_price == 0.0 {
            return 0.0;
        }
        let min_key = price_to_cents(current_price);

        // Tier 1: massive wall (> 20 BTC) within 0.05%
        let max_key_005 = price_to_cents(current_price * 1.0005);
        for (&_, &size) in self.asks.range(min_key..=max_key_005) {
            if size > 20.0 {
                return 1.0;
            }
        }

        // Tier 2: large wall (> 10 BTC) within 0.1%
        let max_key_01 = price_to_cents(current_price * 1.001);
        for (&_, &size) in self.asks.range(min_key..=max_key_01) {
            if size > 10.0 {
                return 0.5;
            }
        }

        // Tier 3: significant wall (> 5 BTC) within 0.2%
        let max_key_02 = price_to_cents(current_price * 1.002);
        for (&_, &size) in self.asks.range(min_key..=max_key_02) {
            if size > 5.0 {
                return 0.3;
            }
        }

        0.0
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

// ── RRI helpers ────────────────────────────────────────────────────────────

/// Maps an RRI score to (emoji, label, short meaning).
fn rri_label(rri: f64) -> (&'static str, &'static str, &'static str) {
    if rri >= 8.1 {
        ("🚨", "EXTREME", "DO NOT ENTER!")
    } else if rri >= 6.6 {
        ("🔴", "HIGH RISK", "Avoid new entries")
    } else if rri >= 4.6 {
        ("🟠", "ELEVATED", "Reversal pressure building")
    } else if rri >= 2.6 {
        ("🟡", "MODERATE", "Caution, some signals warming")
    } else {
        ("🟢", "LOW RISK", "Safe to trade")
    }
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
        while self.rolling_trades.front().is_some_and(|t| t.timestamp_ms < cutoff_ms) {
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

            // Finalise the 15-second delta window before scoring.
            self.delta.finalize_window(price);

            // Calculate the composite Reversal Risk Index.
            let (rri, delta_score, rsi_score, vwap_score, bb_score, wall_score) =
                self.calculate_rri(price);

            let (emoji, label, meaning) = rri_label(rri);
            let signal_line = format!(
                "   {emoji} RRI: {rri:.1}/10 — {label} — {meaning}\n   [Delta: {delta_score:.1} | RSI: {rsi_score:.1} | VWAP: {vwap_score:.1} | BB: {bb_score:.2} | Wall: {wall_score:.1}]"
            );

            output.push(signal_line);
        }

        output
    }

    /// Calculate the Reversal Risk Index from all 5 component scores.
    /// Returns (total_rri, delta_score, rsi_score, vwap_score, bb_score, wall_score).
    fn calculate_rri(&mut self, price: f64) -> (f64, f64, f64, f64, f64, f64) {
        let delta_score = self.delta.score_delta_divergence();
        let rsi_score = self.rsi.score_rsi_exhaustion();
        let vwap_score = self.vwap.score_vwap_deviation(price);
        let bb_score = self.bollinger.score_bollinger_squeeze();
        let wall_score = self.sell_wall.score_wall_proximity(price);
        let total = (delta_score + rsi_score + vwap_score + bb_score + wall_score)
            .clamp(1.0, 10.0);
        (total, delta_score, rsi_score, vwap_score, bb_score, wall_score)
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
