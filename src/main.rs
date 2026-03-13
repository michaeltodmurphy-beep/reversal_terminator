use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use url::Url;

// ── Configuration ──────────────────────────────────────────────────────────

/// Runtime configuration loaded from `config.json` at startup.
#[derive(Deserialize)]
struct Config {
    /// EMA smoothing factor for the RRI. Higher = more responsive.
    rri_alpha: f64,
    /// How often (in seconds) a price line is printed.
    tick_interval_secs: u64,
    /// RRI check fires every N ticks.
    rri_check_every_n_ticks: u64,
    /// Path to the CSV log file for RRI readings.
    log_file: String,
    /// How many hours of RRI log data to retain.
    log_retention_hours: u64,
    /// Kalshi API key (email). Leave empty to disable Kalshi integration.
    #[serde(default)]
    kalshi_api_key: String,
    /// Kalshi API secret (RSA private key PEM inline). Leave empty to disable Kalshi integration.
    #[serde(default)]
    kalshi_api_secret: String,
    /// Path to a file containing the Kalshi RSA private key in PEM format.
    /// When set, takes precedence over `kalshi_api_secret`.
    #[serde(default)]
    kalshi_api_secret_file: String,
    /// Kalshi series ticker for BTC contracts (e.g. "KXBTC15M").
    #[serde(default = "default_kalshi_series_ticker", alias = "kalshi_event_ticker")]
    kalshi_series_ticker: String,
}

fn default_kalshi_series_ticker() -> String {
    "KXBTC15M".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            rri_alpha: 0.5,
            tick_interval_secs: 1,
            rri_check_every_n_ticks: 5,
            log_file: "rri_log.csv".to_string(),
            log_retention_hours: 12,
            kalshi_api_key: String::new(),
            kalshi_api_secret: String::new(),
            kalshi_api_secret_file: String::new(),
            kalshi_series_ticker: default_kalshi_series_ticker(),
        }
    }
}

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

// ── Kalshi state ───────────────────────────────────────────────────────────

/// Real-time Kalshi YES/NO bid state for the active 15-minute BTC contract.
struct KalshiState {
    /// Current YES bid price (0.0–1.0).
    yes_bid: f64,
    /// Current NO bid price (0.0–1.0).
    no_bid: f64,
    /// YES bid at the previous tick (for % change display).
    prev_yes_bid: f64,
    /// NO bid at the previous tick (for % change display).
    prev_no_bid: f64,
    /// Ticker of the currently subscribed contract.
    active_ticker: String,
    /// Whether the WebSocket connection is live.
    connected: bool,
    /// True when there is no active contract for the current window.
    no_contract: bool,
}

impl KalshiState {
    fn new() -> Self {
        Self {
            yes_bid: 0.0,
            no_bid: 0.0,
            prev_yes_bid: 0.0,
            prev_no_bid: 0.0,
            active_ticker: String::new(),
            connected: false,
            no_contract: false,
        }
    }
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

/// Formats a percentage change as `+X.XX%` or `-X.XX%`.
/// Returns `+0.00%` when `prev` is zero (first tick).
fn pct_change_str(prev: f64, current: f64) -> String {
    if prev == 0.0 {
        return "+0.00%".to_string();
    }
    let pct = (current - prev) / prev * 100.0;
    if pct >= 0.0 {
        format!("+{pct:.2}%")
    } else {
        format!("{pct:.2}%")
    }
}

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
    /// Tick counter, used to determine RRI checkpoints.
    tick_count: u64,
    /// Rolling trade buffer (kept for up to 60 seconds) for volume-weighted averages.
    rolling_trades: VecDeque<TimedTrade>,
    /// Most recently calculated 30-second rolling VWAP.
    rolling_30s_avg: f64,
    /// Previous tick's 30-second rolling VWAP, for percentage-change display.
    prev_30s_avg: f64,
    /// Price at the previous tick, for percentage-change display.
    prev_price: f64,
    /// EMA-smoothed RRI value displayed to the user.
    smoothed_rri: f64,
    /// Previous smoothed RRI value, used to compute the trend arrow.
    previous_smoothed_rri: f64,
    /// False until the first RRI reading has been processed.
    rri_initialized: bool,

    // ── Config-driven fields ───────────────────────────────────────────────
    /// EMA smoothing factor for the RRI.
    rri_alpha: f64,
    /// RRI check fires every N ticks.
    rri_check_every_n_ticks: u64,
    /// Path to the CSV log file for RRI readings.
    log_file: String,
    /// How many hours of RRI log data to retain.
    log_retention_hours: u64,
    /// Counts RRI log writes; used to trigger periodic pruning.
    rri_write_count: u64,
}

impl AppState {
    fn new(config: &Config) -> Self {
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
            rolling_30s_avg: 0.0,
            prev_30s_avg: 0.0,
            prev_price: 0.0,
            smoothed_rri: 0.0,
            previous_smoothed_rri: 0.0,
            rri_initialized: false,

            rri_alpha: config.rri_alpha,
            rri_check_every_n_ticks: config.rri_check_every_n_ticks,
            log_file: config.log_file.clone(),
            log_retention_hours: config.log_retention_hours,
            rri_write_count: 0,
        }
    }

    /// Process a `match` message: silently update state, no printing.
    /// RSI and Bollinger are updated at tick closes (in `on_tick`),
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

    /// Called on every tick. Updates candle-based indicators, evicts stale
    /// rolling trades, recalculates the 30-second VWAP, and returns the
    /// formatted output lines (always a price line, plus a signal summary line
    /// every `rri_check_every_n_ticks` ticks).
    fn on_tick(&mut self, kalshi: Option<&Arc<Mutex<KalshiState>>>) -> Vec<String> {
        let mut output = Vec::new();

        if self.current_price == 0.0 {
            return output;
        }

        self.tick_count += 1;
        let now = Utc::now();
        let ts = now.format("%H:%M:%S");
        let now_ms = now.timestamp_millis() as u64;

        // Update tick-close indicators with the current price.
        self.rsi.update(self.current_price);
        self.bollinger.update(self.current_price);

        // Evict trades older than 60 seconds (we keep 60s to support potential
        // future averages; current display uses 30s).
        let cutoff_ms = now_ms.saturating_sub(60_000);
        while self.rolling_trades.front().is_some_and(|t| t.timestamp_ms < cutoff_ms) {
            self.rolling_trades.pop_front();
        }

        // Calculate the 30-second volume-weighted average.
        let cutoff_30s_ms = now_ms.saturating_sub(30_000);
        let (pv_30s, vol_30s) = self.rolling_trades.iter()
            .filter(|t| t.timestamp_ms >= cutoff_30s_ms)
            .fold((0.0f64, 0.0f64), |(pv, v), t| (pv + t.price * t.volume, v + t.volume));

        let new_30s_avg = if vol_30s > 0.0 {
            pv_30s / vol_30s
        } else {
            self.rolling_30s_avg // keep last known value when no trades in window
        };

        // Percentage changes (vs. previous tick).
        let price_pct_str = pct_change_str(self.prev_price, self.current_price);
        let avg_30s_pct_str = pct_change_str(self.prev_30s_avg, new_30s_avg);

        self.prev_price = self.current_price;
        self.prev_30s_avg = self.rolling_30s_avg;
        self.rolling_30s_avg = new_30s_avg;

        // ── Line 1: BTC price ──────────────────────────────────────────────
        let avg_30s_display = if self.rolling_30s_avg > 0.0 {
            format_usd(self.rolling_30s_avg)
        } else {
            "N/A".to_string()
        };
        output.push(format!(
            "💰 [{ts}] BTC: {} ({price_pct_str}) | 30s Avg: {avg_30s_display} ({avg_30s_pct_str})",
            format_usd(self.current_price)
        ));

        // ── Line 2: Kalshi YES/NO bids ─────────────────────────────────────
        if let Some(kalshi_arc) = kalshi {
            let kalshi_line = {
                let mut ks = kalshi_arc.lock().unwrap();
                if !ks.connected {
                    if ks.no_contract {
                        "   📊 Kalshi: no active contract".to_string()
                    } else {
                        "   📊 Kalshi: disconnected".to_string()
                    }
                } else if ks.yes_bid == 0.0 && ks.no_bid == 0.0 {
                    "   📊 Kalshi: awaiting data…".to_string()
                } else {
                    let yes_pct = pct_change_str(ks.prev_yes_bid, ks.yes_bid);
                    let no_pct = pct_change_str(ks.prev_no_bid, ks.no_bid);
                    ks.prev_yes_bid = ks.yes_bid;
                    ks.prev_no_bid = ks.no_bid;
                    format!(
                        "   📊 Kalshi YES: ${:.2} ({yes_pct}) | NO: ${:.2} ({no_pct})",
                        ks.yes_bid, ks.no_bid
                    )
                }
            };
            output.push(kalshi_line);
        }

        // ── Lines 3-4: RRI signal (every N ticks) ─────────────────────────
        if self.tick_count % self.rri_check_every_n_ticks == 0 {
            let price = self.current_price;

            // Finalise the delta window before scoring.
            self.delta.finalize_window(price);

            // Calculate the composite Reversal Risk Index.
            let (raw_rri, delta_score, rsi_score, vwap_score, bb_score, wall_score) =
                self.calculate_rri(price);

            // Apply EMA smoothing to the raw RRI.
            let is_first_reading = !self.rri_initialized;
            if is_first_reading {
                self.smoothed_rri = raw_rri;
                self.rri_initialized = true;
            } else {
                self.previous_smoothed_rri = self.smoothed_rri;
                self.smoothed_rri =
                    (self.rri_alpha * raw_rri) + ((1.0 - self.rri_alpha) * self.smoothed_rri);
            }

            // Derive trend arrow from change in smoothed RRI.
            let trend_arrow = if is_first_reading {
                "→" // first reading — no prior value to compare
            } else {
                let diff = self.smoothed_rri - self.previous_smoothed_rri;
                if diff > 0.3 {
                    "↑" // risk increasing
                } else if diff < -0.3 {
                    "↓" // risk decreasing
                } else {
                    "→" // stable
                }
            };

            let smoothed = self.smoothed_rri;
            let (emoji, label, meaning) = rri_label(smoothed);
            let signal_line = format!(
                "   {emoji} RRI: {smoothed:.1}/10 {trend_arrow} — {label} — {meaning}\n   [Delta: {delta_score:.1} | RSI: {rsi_score:.1} | VWAP: {vwap_score:.1} | BB: {bb_score:.2} | Wall: {wall_score:.1}]"
            );

            output.push(signal_line);

            // Append this reading to the rolling RRI log.
            let log_ts = now.format("%Y-%m-%dT%H:%M:%SZ").to_string();
            self.append_rri_log(
                &log_ts, raw_rri, trend_arrow, label,
                delta_score, rsi_score, vwap_score, bb_score, wall_score,
            );
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

    /// Append an RRI reading to the rolling CSV log file.
    fn append_rri_log(
        &mut self,
        timestamp: &str,
        raw_rri: f64,
        trend: &str,
        label: &str,
        delta_score: f64,
        rsi_score: f64,
        vwap_score: f64,
        bb_score: f64,
        wall_score: f64,
    ) {
        let needs_header = !std::path::Path::new(&self.log_file).exists();
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.log_file) {
            if needs_header {
                let _ = writeln!(f, "timestamp,smoothed_rri,raw_rri,trend,label,delta,rsi,vwap,bb,wall");
            }
            let _ = writeln!(
                f,
                "{},{:.1},{:.1},{},{},{:.1},{:.1},{:.1},{:.2},{:.1}",
                timestamp, self.smoothed_rri, raw_rri, trend, label,
                delta_score, rsi_score, vwap_score, bb_score, wall_score
            );
        }
        self.rri_write_count += 1;
        if self.rri_write_count % 100 == 0 {
            self.prune_rri_log();
        }
    }

    /// Remove log entries older than `log_retention_hours` from the CSV log file.
    fn prune_rri_log(&self) {
        let path = &self.log_file;
        let file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(_) => return,
        };
        let reader = std::io::BufReader::new(file);
        let lines: Vec<String> = reader.lines().filter_map(|l| l.ok()).collect();
        if lines.len() < 2 {
            return;
        }
        let cutoff = Utc::now() - chrono::TimeDelta::hours(self.log_retention_hours as i64);
        let header = lines[0].clone();
        let retained: Vec<&String> = lines[1..]
            .iter()
            .filter(|line| {
                let ts_str = line.split(',').next().unwrap_or("");
                chrono::DateTime::parse_from_rfc3339(ts_str)
                    .map(|dt| dt.with_timezone(&Utc) >= cutoff)
                    .unwrap_or(true)
            })
            .collect();
        if let Ok(f) = std::fs::File::create(path) {
            let mut writer = std::io::BufWriter::new(f);
            let _ = writeln!(writer, "{}", header);
            for line in retained {
                let _ = writeln!(writer, "{}", line);
            }
        }
    }
}

// ── Kalshi WebSocket task ──────────────────────────────────────────────────

const KALSHI_WS_URL: &str = "wss://api.elections.kalshi.com/trade-api/ws/v2";
const KALSHI_REST_BASE: &str = "https://api.elections.kalshi.com/trade-api/v2";

/// Sign a Kalshi API request using RSA-PSS SHA-256.
///
/// The message is `timestamp_str + method + path`.  Returns a base64-encoded
/// signature, or `None` if the private key cannot be parsed or signing fails.
fn kalshi_sign(private_key_pem: &str, timestamp: &str, method: &str, path: &str) -> Option<String> {
    use rsa::pkcs1::DecodeRsaPrivateKey;
    use rsa::pkcs8::DecodePrivateKey;
    use rsa::pss::SigningKey;
    use rsa::signature::{RandomizedSigner, SignatureEncoding};
    use sha2::Sha256;

    // The PEM stored in config.json may use literal `\n` instead of real
    // newlines (common when serialised as a single-line JSON string).
    let pem = private_key_pem.replace("\\n", "\n");

    // Try PKCS#8 first (`-----BEGIN PRIVATE KEY-----`), then fall back to
    // PKCS#1 (`-----BEGIN RSA PRIVATE KEY-----`) which is the format that
    // Kalshi generates.
    let private_key = rsa::RsaPrivateKey::from_pkcs8_pem(&pem)
        .or_else(|_| rsa::RsaPrivateKey::from_pkcs1_pem(&pem))
        .map_err(|e| eprintln!("Kalshi: failed to parse private key: {e}"))
        .ok()?;

    let signing_key = SigningKey::<Sha256>::new(private_key);
    let message = format!("{timestamp}{method}{path}");

    let mut rng = rand::thread_rng();
    let sig = signing_key
        .sign_with_rng(&mut rng, message.as_bytes())
        .to_bytes();

    Some(base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &sig))
}

/// Parse a serde_json::Value that may be a JSON number or a JSON string containing a number.
fn flex_parse_f64(val: &serde_json::Value) -> Option<f64> {
    val.as_f64().or_else(|| val.as_str().and_then(|s| s.parse::<f64>().ok()))
}

/// Query the Kalshi REST API to find the nearest ATM 15-minute BTC contract
/// expiring in the current (or next) 15-minute window.  Returns the market
/// ticker on success.
async fn kalshi_find_atm_contract(
    api_key: &str,
    api_secret: &str,
    series_ticker: &str,
    current_btc_price: f64,
) -> Option<String> {
    // Sign only the bare path — Kalshi's signature spec covers
    // `{timestamp}{METHOD}{path}` without query parameters.
    let bare_path = "/trade-api/v2/markets";
    let timestamp = Utc::now().timestamp_millis().to_string();
    let signature = kalshi_sign(api_secret, &timestamp, "GET", bare_path)?;

    let client = reqwest::Client::new();
    let url = format!("{KALSHI_REST_BASE}/markets?series_ticker={series_ticker}&status=open&limit=100");
    let resp = client
        .get(&url)
        .header("KALSHI-ACCESS-KEY", api_key)
        .header("KALSHI-ACCESS-SIGNATURE", signature)
        .header("KALSHI-ACCESS-TIMESTAMP", &timestamp)
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        eprintln!("Kalshi markets query failed: HTTP {}", resp.status());
        return None;
    }

    let data: serde_json::Value = resp.json().await.ok()?;
    let markets = data["markets"].as_array()?;

    // Filter to markets that are still open and pick the one whose strike
    // (cap_strike or floor_strike field) is closest to the current price.
    let now_ts = Utc::now().timestamp();
    let fifteen_min_secs: i64 = 15 * 60;

    let mut best_ticker: Option<String> = None;
    let mut best_dist = f64::MAX;

    for market in markets {
        // Only consider markets expiring within the current 15-minute window.
        let exp_ts = market["expiration_time"]
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.timestamp())
            .unwrap_or(0);

        // Accept contracts expiring in the next 15 minutes.
        if exp_ts < now_ts || exp_ts > now_ts + fifteen_min_secs {
            continue;
        }

        // Use the floor_strike (lower bound of the price range) as the ATM proxy.
        let strike = flex_parse_f64(&market["floor_strike"])
            .or_else(|| flex_parse_f64(&market["cap_strike"]))
            .unwrap_or(0.0);

        if strike == 0.0 {
            continue;
        }

        let dist = (strike - current_btc_price).abs();
        if dist < best_dist {
            best_dist = dist;
            best_ticker = market["ticker"].as_str().map(|s| s.to_string());
        }
    }

    best_ticker
}

/// Background task: authenticates with Kalshi, finds the nearest ATM contract,
/// opens a WebSocket, subscribes to its orderbook, and continuously updates
/// the shared `KalshiState`.  Reconnects automatically on failure.
async fn run_kalshi_task(
    api_key: String,
    api_secret: String,
    series_ticker: String,
    state: Arc<Mutex<KalshiState>>,
    current_price_ref: Arc<Mutex<f64>>,
) {
    // Outer loop: reconnect from scratch on any hard failure.
    loop {
        // Find ATM contract.
        let btc_price = { *current_price_ref.lock().unwrap() };
        let ticker = match kalshi_find_atm_contract(&api_key, &api_secret, &series_ticker, btc_price).await {
            Some(t) => t,
            None => {
                eprintln!("Kalshi: no active contract found, retrying in 60s…");
                {
                    let mut ks = state.lock().unwrap();
                    ks.connected = false;
                    ks.no_contract = true;
                }
                tokio::time::sleep(Duration::from_secs(60)).await;
                continue;
            }
        };

        {
            let mut ks = state.lock().unwrap();
            ks.active_ticker = ticker.clone();
            ks.no_contract = false;
        }

        // Connect WebSocket.
        let ws_url = Url::parse(KALSHI_WS_URL).expect("invalid Kalshi WS URL");
        let ws = match connect_async(ws_url).await {
            Ok((ws, _)) => ws,
            Err(e) => {
                eprintln!("Kalshi WS connect failed ({e}), retrying in 10s…");
                {
                    let mut ks = state.lock().unwrap();
                    ks.connected = false;
                }
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
        };

        let (mut sink, mut stream) = ws.split();

        // Authenticate over the WebSocket connection.
        let ws_path = "/trade-api/ws/v2";
        let ws_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let ws_ts_str = ws_ts.to_string();
        let ws_signature = match kalshi_sign(&api_secret, &ws_ts_str, "GET", ws_path) {
            Some(s) => s,
            None => {
                eprintln!("Kalshi WS: failed to sign auth, reconnecting…");
                continue;
            }
        };
        let auth_msg = serde_json::json!({
            "id": 1,
            "cmd": "auth",
            "params": {
                "api_key": api_key,
                "timestamp": ws_ts,
                "signature": ws_signature
            }
        });
        if sink.send(Message::Text(auth_msg.to_string())).await.is_err() {
            eprintln!("Kalshi WS auth send failed, reconnecting…");
            continue;
        }

        // Subscribe to the orderbook for the active contract.
        let sub_msg = serde_json::json!({
            "id": 2,
            "cmd": "subscribe",
            "params": {
                "channels": ["orderbook_delta"],
                "market_tickers": [ticker]
            }
        });
        if sink.send(Message::Text(sub_msg.to_string())).await.is_err() {
            eprintln!("Kalshi WS subscribe send failed, reconnecting…");
            continue;
        }

        {
            let mut ks = state.lock().unwrap();
            ks.connected = true;
        }

        // Contract refresh timer: re-evaluate the active contract every 60s.
        let mut refresh_ticker = tokio::time::interval(Duration::from_secs(60));
        refresh_ticker.reset();

        // Inner loop: process incoming messages.
        loop {
            tokio::select! {
                msg = stream.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                                kalshi_handle_message(&v, &state);
                            }
                        }
                        Some(Err(e)) => {
                            eprintln!("Kalshi WS error: {e}");
                            break;
                        }
                        None => {
                            eprintln!("Kalshi WS closed, reconnecting…");
                            break;
                        }
                        _ => {}
                    }
                }
                _ = refresh_ticker.tick() => {
                    // Check whether a newer ATM contract has become active.
                    let btc = { *current_price_ref.lock().unwrap() };
                    if btc > 0.0 {
                        if let Some(new_ticker) = kalshi_find_atm_contract(&api_key, &api_secret, &series_ticker, btc).await {
                            let current = {
                                let ks = state.lock().unwrap();
                                ks.active_ticker.clone()
                            };
                            if new_ticker != current {
                                // Roll to the new contract by breaking out to reconnect.
                                eprintln!("Kalshi: rolling contract from {current} → {new_ticker}");
                                break;
                            }
                        }
                    }
                }
            }
        }

        {
            let mut ks = state.lock().unwrap();
            ks.connected = false;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Parse a Kalshi WebSocket message and update the shared state with the
/// latest YES/NO bid prices.
fn kalshi_handle_message(msg: &serde_json::Value, state: &Arc<Mutex<KalshiState>>) {
    // Kalshi orderbook_delta messages carry bid prices in `yes` and `no` fields
    // within the `msg` payload.  The prices are in cents (0–100).
    let msg_type = msg["type"].as_str().unwrap_or("");
    if msg_type != "orderbook_delta" && msg_type != "orderbook_snapshot" {
        return;
    }

    let payload = &msg["msg"];

    // Extract best YES bid.
    let yes_bid_cents = payload["yes"]
        .as_array()
        .and_then(|arr| {
            arr.iter()
                .filter_map(|entry| {
                    let price = flex_parse_f64(&entry[0])?;
                    let size = flex_parse_f64(&entry[1]).unwrap_or(0.0);
                    if size > 0.0 { Some(price) } else { None }
                })
                .reduce(f64::max)
        });

    // Extract best NO bid.
    let no_bid_cents = payload["no"]
        .as_array()
        .and_then(|arr| {
            arr.iter()
                .filter_map(|entry| {
                    let price = flex_parse_f64(&entry[0])?;
                    let size = flex_parse_f64(&entry[1]).unwrap_or(0.0);
                    if size > 0.0 { Some(price) } else { None }
                })
                .reduce(f64::max)
        });

    let mut ks = state.lock().unwrap();
    if let Some(c) = yes_bid_cents {
        ks.yes_bid = c / 100.0;
    }
    if let Some(c) = no_bid_cents {
        ks.no_bid = c / 100.0;
    }
}

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

    // Load configuration from config.json, falling back to defaults if missing.
    let config = std::fs::read_to_string("config.json")
        .ok()
        .and_then(|s| serde_json::from_str::<Config>(&s).ok())
        .unwrap_or_default();

    let kalshi_enabled = !config.kalshi_api_key.is_empty()
        && (!config.kalshi_api_secret.is_empty() || !config.kalshi_api_secret_file.is_empty());

    println!(
        "📋 Config: rri_alpha={} | tick={}s | rri_every={}t | log={} | retention={}h | kalshi={}",
        config.rri_alpha,
        config.tick_interval_secs,
        config.rri_check_every_n_ticks,
        config.log_file,
        config.log_retention_hours,
        if kalshi_enabled { "enabled" } else { "disabled (no credentials)" },
    );
    println!("   Connecting to Coinbase Exchange WebSocket…\n");

    let ws = connect_ws_coinbase().await;
    let mut stream = ws.fuse();

    let tick_secs = config.tick_interval_secs;
    let mut state = AppState::new(&config);
    let mut ticker = tokio::time::interval(Duration::from_secs(tick_secs));

    // Shared current BTC price for Kalshi contract discovery.
    let shared_btc_price: Arc<Mutex<f64>> = Arc::new(Mutex::new(0.0));

    // Kalshi shared state + background task.
    let kalshi_state: Option<Arc<Mutex<KalshiState>>> = if kalshi_enabled {
        // Resolve the RSA private key: prefer a file path over the inline string.
        let kalshi_secret = if !config.kalshi_api_secret_file.is_empty() {
            match std::fs::read_to_string(&config.kalshi_api_secret_file) {
                Ok(pem) => pem,
                Err(e) => {
                    eprintln!(
                        "Kalshi: failed to read key file '{}': {e}; falling back to inline secret",
                        config.kalshi_api_secret_file
                    );
                    if config.kalshi_api_secret.is_empty() {
                        eprintln!("Kalshi: inline kalshi_api_secret is also empty — Kalshi will be non-functional");
                    }
                    config.kalshi_api_secret.clone()
                }
            }
        } else {
            config.kalshi_api_secret.clone()
        };

        let ks = Arc::new(Mutex::new(KalshiState::new()));
        let ks_clone = ks.clone();
        let price_clone = shared_btc_price.clone();
        tokio::spawn(run_kalshi_task(
            config.kalshi_api_key.clone(),
            kalshi_secret,
            config.kalshi_series_ticker.clone(),
            ks_clone,
            price_clone,
        ));
        Some(ks)
    } else {
        None
    };

    println!("📡 Streaming live data — watching for reversals…\n");

    loop {
        tokio::select! {
            msg = stream.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<CoinbaseMessage>(&text) {
                            Ok(CoinbaseMessage::Match { price, size, side }) => {
                                state.handle_match(&price, &size, &side);
                                // Keep shared price up-to-date for Kalshi contract selection.
                                if let Ok(p) = price.parse::<f64>() {
                                    *shared_btc_price.lock().unwrap() = p;
                                }
                            }
                            Ok(CoinbaseMessage::Snapshot { asks }) => {
                                state.sell_wall.apply_snapshot(&asks);
                            }
                            Ok(CoinbaseMessage::L2Update { changes }) => {
                                state.sell_wall.apply_changes(&changes);
                                // Sell wall signal is evaluated at the RRI
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
                for line in state.on_tick(kalshi_state.as_ref()) {
                    println!("{line}");
                }
            }
        }
    }
}
