use chrono::Utc;
use futures_util::StreamExt;
use serde::Deserialize;
use std::collections::VecDeque;
use tokio_tungstenite::connect_async;
use url::Url;

// ── Binance message types ──────────────────────────────────────────────────

#[derive(Deserialize, Debug)]
struct AggTrade {
    #[serde(rename = "p")]
    price: String,
    #[serde(rename = "q")]
    qty: String,
    /// true  → buyer is the market maker → this is a SELL order
    /// false → buyer is the taker        → this is a BUY order
    #[serde(rename = "m")]
    is_buyer_maker: bool,
}

#[derive(Deserialize, Debug)]
struct DepthUpdate {
    #[serde(rename = "asks")]
    asks: Vec<[String; 2]>,
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

    fn update(&mut self, price: f64, qty: f64, is_buyer_maker: bool) {
        // is_buyer_maker == true  → market SELL (negative delta)
        // is_buyer_maker == false → market BUY  (positive delta)
        if is_buyer_maker {
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

struct SellWallState {
    last_signal: Option<String>,
}

impl SellWallState {
    fn new() -> Self {
        Self { last_signal: None }
    }

    fn update(&mut self, asks: &[[String; 2]], current_price: f64) {
        let threshold = current_price * 1.001; // 0.1% above current price
        // A "sell wall" is a large ask order that sits just above the current
        // price and acts as a ceiling.  We require: (a) within 0.1% of price
        // so it is immediately relevant, and (b) ≥ 10 BTC so it is large
        // enough to meaningfully absorb market buys.
        for level in asks {
            let ask_price: f64 = level[0].parse().unwrap_or(0.0);
            let ask_qty: f64 = level[1].parse().unwrap_or(0.0);
            if ask_price > 0.0 && ask_price <= threshold && ask_qty >= 10.0 {
                self.last_signal = Some(format!(
                    "🧱 SELL WALL: {:.3} BTC ask at ${:.2} ({:.3}% above price) — 95% YES is a trap!",
                    ask_qty,
                    ask_price,
                    (ask_price - current_price) / current_price * 100.0,
                ));
                return;
            }
        }
        // no wall found
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
    // per-block trade counter used for status prints
    block_trade_count: u64,
    // closing price of the previous 1-minute block (for change calculation)
    prev_block_close_price: f64,
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
            prev_block_close_price: 0.0,
        }
    }

    fn maybe_reset_block(&mut self) {
        let now = Utc::now();
        let minute = now.timestamp() as u32 / 60;
        if minute != self.current_minute {
            if self.current_minute != 0 {
                let ts = now.format("%H:%M:%S");
                let prev_vwap = self.vwap.vwap();
                let vwap_str = if prev_vwap > 0.0 {
                    format!(" | VWAP (prev): {}", format_usd(prev_vwap))
                } else {
                    String::new()
                };
                let change_str = if self.prev_block_close_price > 0.0 {
                    let diff = self.current_price - self.prev_block_close_price;
                    format!(" | Change: {}", format_usd_change(diff))
                } else {
                    String::new()
                };
                println!(
                    "\n💰 [{ts}] BTC Price: {}{vwap_str}{change_str}",
                    format_usd(self.current_price)
                );
                println!(
                    "── New 1-min block #{minute} — resetting VWAP & delta state ──"
                );
                self.prev_block_close_price = self.current_price;
            }
            self.current_minute = minute;
            self.vwap = VwapState::new();
            self.delta.reset_block(self.current_price);
            self.block_trade_count = 0;
        }
    }

    fn handle_trade(&mut self, msg: AggTrade) {
        let price: f64 = msg.price.parse().unwrap_or(0.0);
        let qty: f64 = msg.qty.parse().unwrap_or(0.0);
        if price == 0.0 || qty == 0.0 {
            return;
        }

        self.maybe_reset_block();
        self.current_price = price;
        self.trade_count += 1;
        self.block_trade_count += 1;

        // update indicators
        self.vwap.update(price, qty);
        self.rsi.update(price);
        self.delta.update(price, qty, msg.is_buyer_maker);
        self.bollinger.update(price);

        // periodic RSI hook check
        let rsi_signal = self.rsi.check_signal();

        // print status every ~200 trades
        if self.block_trade_count.is_multiple_of(200) {
            let sec = seconds_into_block();
            let vwap_price = self.vwap.vwap();
            let delta = self.delta.cumulative_delta;
            println!(
                "📊 BTC: ${:.2} | VWAP: ${:.2} | Delta: {:.3} | Sec: {}/60",
                price, vwap_price, delta, sec
            );
        }

        // danger zone logic
        if is_danger_zone() {
            let sec = seconds_into_block();
            let remaining = 60u64.saturating_sub(sec);

            // print danger zone header once per 10-trade cadence in the zone
            if self.block_trade_count.is_multiple_of(10) {
                println!(
                    "🔴 DANGER ZONE — Second {}/60 — Last {} seconds!",
                    sec, remaining
                );
            }

            // gather signals
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
            // still print RSI hooks outside danger zone
            println!("{s}");
        }
    }

    fn handle_depth(&mut self, msg: DepthUpdate) {
        if self.current_price == 0.0 {
            return;
        }
        self.sell_wall.update(&msg.asks, self.current_price);
    }
}

// ── WebSocket task helpers ─────────────────────────────────────────────────

/// Connect (with simple retry) and return the WebSocket stream.
async fn connect_ws(
    url_str: &str,
) -> tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
> {
    let url = Url::parse(url_str).expect("invalid URL");
    loop {
        match connect_async(url.clone()).await {
            Ok((ws, _)) => {
                println!("✅ Connected to {url_str}");
                return ws;
            }
            Err(e) => {
                eprintln!("⚠️  Connection failed ({e}), retrying in 3s …");
                tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
            }
        }
    }
}

// ── Entry point ────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    println!("🚀 Reversal Terminator — BTC/USD 1-min chart monitor starting…");
    println!("   Connecting to Binance WebSocket streams…\n");

    let trade_url = "wss://stream.binance.us:9443/ws/btcusdt@aggTrade";
    let depth_url = "wss://stream.binance.us:9443/ws/btcusdt@depth20@100ms";

    let trade_ws = connect_ws(trade_url).await;
    let depth_ws = connect_ws(depth_url).await;

    let mut trade_stream = trade_ws.fuse();
    let mut depth_stream = depth_ws.fuse();

    let mut state = AppState::new();

    println!("📡 Streaming live data — watching for reversals…\n");

    loop {
        tokio::select! {
            msg = trade_stream.next() => {
                match msg {
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                        if let Ok(trade) = serde_json::from_str::<AggTrade>(&text) {
                            state.handle_trade(trade);
                        }
                    }
                    Some(Err(e)) => eprintln!("Trade stream error: {e}"),
                    None => {
                        eprintln!("Trade stream closed, reconnecting…");
                        let ws = connect_ws(trade_url).await;
                        trade_stream = ws.fuse();
                    }
                    _ => {}
                }
            }
            msg = depth_stream.next() => {
                match msg {
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                        if let Ok(depth) = serde_json::from_str::<DepthUpdate>(&text) {
                            state.handle_depth(depth);
                        }
                    }
                    Some(Err(e)) => eprintln!("Depth stream error: {e}"),
                    None => {
                        eprintln!("Depth stream closed, reconnecting…");
                        let ws = connect_ws(depth_url).await;
                        depth_stream = ws.fuse();
                    }
                    _ => {}
                }
            }
        }
    }
}
