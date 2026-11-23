//! Timeframe model: trade-to-window aggregation, 1s candles built from trades,
//! 2d candles built from two completed 1d candles, and completeness handling.
//!
//! Rules (see docs/TIMEFRAME_MODEL.md):
//! * Buckets are aligned to the Unix epoch: bucket start = floor(ts / w) * w.
//!   A bucket covers [start, start + w).
//! * A candle is `complete` only when `start + w <= now_ms`. Incomplete candles
//!   are never used as completed observations ([`completed`] filters them).
//! * Empty buckets are NOT synthesised: no trades -> no candle. CoinDCX does not
//!   publish native 1s candles; these are built locally from timestamped trades.
//! * Invalid trades (non-finite or non-positive price/qty) are dropped and counted.

use serde::Serialize;

pub const MS_PER_DAY: i64 = 86_400_000;
/// Microstructure windows built locally from trades, seconds.
pub const MICRO_WINDOWS_S: [i64; 8] = [1, 5, 10, 15, 30, 60, 90, 120];

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Trade {
    pub ts_ms: i64,
    pub price: f64,
    pub qty: f64,
    /// Some(true) if the buyer was the aggressor (taker buy); None if the feed
    /// did not say. Never inferred from price ticks.
    pub buyer_aggressor: Option<bool>,
}

impl Trade {
    pub fn is_valid(&self) -> bool {
        self.price.is_finite() && self.price > 0.0 && self.qty.is_finite() && self.qty > 0.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Candle {
    pub open_ms: i64,
    pub width_ms: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
    pub buy_volume: f64,
    pub sell_volume: f64,
    pub trades: u32,
    pub complete: bool,
}

impl Candle {
    pub fn close_ms(&self) -> i64 {
        self.open_ms + self.width_ms
    }
    pub fn log_return(&self) -> Option<f64> {
        if self.open > 0.0 && self.close > 0.0 {
            Some((self.close / self.open).ln())
        } else {
            None
        }
    }
}

/// Context horizons consumed from exchange candles (2d is derived locally).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum ContextTf {
    M1,
    M5,
    M15,
    H1,
    D1,
    D2,
    D3,
}

impl ContextTf {
    pub const ALL: [ContextTf; 7] = [
        ContextTf::D3,
        ContextTf::D2,
        ContextTf::D1,
        ContextTf::H1,
        ContextTf::M15,
        ContextTf::M5,
        ContextTf::M1,
    ];
    pub fn width_ms(self) -> i64 {
        match self {
            ContextTf::M1 => 60_000,
            ContextTf::M5 => 300_000,
            ContextTf::M15 => 900_000,
            ContextTf::H1 => 3_600_000,
            ContextTf::D1 => MS_PER_DAY,
            ContextTf::D2 => 2 * MS_PER_DAY,
            ContextTf::D3 => 3 * MS_PER_DAY,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            ContextTf::M1 => "1m",
            ContextTf::M5 => "5m",
            ContextTf::M15 => "15m",
            ContextTf::H1 => "1h",
            ContextTf::D1 => "1d",
            ContextTf::D2 => "2d",
            ContextTf::D3 => "3d",
        }
    }
}

/// Result of aggregating trades: candles plus a count of rejected trades.
#[derive(Debug, Clone, Serialize)]
pub struct Aggregation {
    pub width_ms: i64,
    pub candles: Vec<Candle>,
    pub dropped_invalid: usize,
}

/// Aggregate trades into epoch-aligned candles of `width_ms`.
/// Trades are sorted by timestamp (stable) before bucketing, so OHLC follow
/// event time, not arrival order.
pub fn aggregate_trades(trades: &[Trade], width_ms: i64, now_ms: i64) -> Aggregation {
    assert!(width_ms > 0);
    let mut v: Vec<Trade> = trades.iter().copied().filter(Trade::is_valid).collect();
    let dropped_invalid = trades.len() - v.len();
    v.sort_by_key(|t| t.ts_ms);
    let mut candles: Vec<Candle> = Vec::new();
    for t in v {
        let start = t.ts_ms.div_euclid(width_ms) * width_ms;
        let (bv, sv) = match t.buyer_aggressor {
            Some(true) => (t.qty, 0.0),
            Some(false) => (0.0, t.qty),
            None => (0.0, 0.0),
        };
        match candles.last_mut() {
            Some(c) if c.open_ms == start => {
                c.high = c.high.max(t.price);
                c.low = c.low.min(t.price);
                c.close = t.price;
                c.volume += t.qty;
                c.buy_volume += bv;
                c.sell_volume += sv;
                c.trades += 1;
            }
            _ => candles.push(Candle {
                open_ms: start,
                width_ms,
                open: t.price,
                high: t.price,
                low: t.price,
                close: t.price,
                volume: t.qty,
                buy_volume: bv,
                sell_volume: sv,
                trades: 1,
                complete: start + width_ms <= now_ms,
            }),
        }
    }
    Aggregation {
        width_ms,
        candles,
        dropped_invalid,
    }
}

/// 1-second candles from trades.
pub fn candles_1s(trades: &[Trade], now_ms: i64) -> Aggregation {
    aggregate_trades(trades, 1_000, now_ms)
}

/// Mark exchange candles complete iff open + width <= now.
pub fn mark_completeness(candles: &mut [Candle], now_ms: i64) {
    for c in candles {
        c.complete = c.open_ms + c.width_ms <= now_ms;
    }
}

/// Only completed candles.
pub fn completed(candles: &[Candle]) -> Vec<Candle> {
    candles.iter().copied().filter(|c| c.complete).collect()
}

/// Build 2d candles from completed 1d candles. A 2d candle aligned to an
/// epoch boundary (open_ms % 2d == 0) is emitted only when BOTH constituent
/// days are present, complete and contiguous. Missing days produce no candle.
pub fn aggregate_2d(daily: &[Candle]) -> Vec<Candle> {
    let mut d: Vec<Candle> = daily
        .iter()
        .copied()
        .filter(|c| c.complete && c.width_ms == MS_PER_DAY)
        .collect();
    d.sort_by_key(|c| c.open_ms);
    d.dedup_by_key(|c| c.open_ms);
    let w = 2 * MS_PER_DAY;
    let mut out = Vec::new();
    for pair in d.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if a.open_ms.rem_euclid(w) != 0 || b.open_ms != a.open_ms + MS_PER_DAY {
            continue;
        }
        out.push(Candle {
            open_ms: a.open_ms,
            width_ms: w,
            open: a.open,
            high: a.high.max(b.high),
            low: a.low.min(b.low),
            close: b.close,
            volume: a.volume + b.volume,
            buy_volume: a.buy_volume + b.buy_volume,
            sell_volume: a.sell_volume + b.sell_volume,
            trades: a.trades + b.trades,
            complete: true,
        });
    }
    out
}

/// Rolling statistics over trades with ts in (now - window, now].
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct WindowStats {
    pub window_ms: i64,
    pub trades: usize,
    pub volume: f64,
    pub buy_volume: f64,
    pub sell_volume: f64,
    pub unknown_side_volume: f64,
    pub vwap: f64,
    pub first_price: f64,
    pub last_price: f64,
    /// ln(last / first); 0 with fewer than 2 trades
    pub log_return: f64,
    /// trades per second
    pub intensity: f64,
}

pub fn window_stats(trades: &[Trade], now_ms: i64, window_ms: i64) -> WindowStats {
    let mut v: Vec<&Trade> = trades
        .iter()
        .filter(|t| t.is_valid() && t.ts_ms > now_ms - window_ms && t.ts_ms <= now_ms)
        .collect();
    v.sort_by_key(|t| t.ts_ms);
    let (mut vol, mut bv, mut sv, mut uv, mut pq) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for t in &v {
        vol += t.qty;
        pq += t.price * t.qty;
        match t.buyer_aggressor {
            Some(true) => bv += t.qty,
            Some(false) => sv += t.qty,
            None => uv += t.qty,
        }
    }
    let first = v.first().map(|t| t.price).unwrap_or(f64::NAN);
    let last = v.last().map(|t| t.price).unwrap_or(f64::NAN);
    WindowStats {
        window_ms,
        trades: v.len(),
        volume: vol,
        buy_volume: bv,
        sell_volume: sv,
        unknown_side_volume: uv,
        vwap: if vol > 0.0 { pq / vol } else { f64::NAN },
        first_price: first,
        last_price: last,
        log_return: if v.len() >= 2 {
            (last / first).ln()
        } else {
            0.0
        },
        intensity: v.len() as f64 / (window_ms as f64 / 1000.0),
    }
}

/// Window stats for every microstructure window in [`MICRO_WINDOWS_S`].
pub fn micro_windows(trades: &[Trade], now_ms: i64) -> Vec<WindowStats> {
    MICRO_WINDOWS_S
        .iter()
        .map(|s| window_stats(trades, now_ms, s * 1000))
        .collect()
}

/// Per-second log returns from candles, scaled for gaps:
/// r_i / sqrt(dt_i) where dt_i = (open_i - open_{i-1}) / width. This gives
/// per-width-unit returns with unit variance scaling under a random walk, so
/// missing (tradeless) buckets do not bias volatility upward or downward.
pub fn gap_scaled_returns(candles: &[Candle]) -> Vec<f64> {
    candles
        .windows(2)
        .filter(|w| w[0].close > 0.0 && w[1].close > 0.0 && w[1].open_ms > w[0].open_ms)
        .map(|w| {
            let dt = (w[1].open_ms - w[0].open_ms) as f64 / w[1].width_ms as f64;
            (w[1].close / w[0].close).ln() / dt.sqrt()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn t(ts: i64, p: f64, q: f64, b: Option<bool>) -> Trade {
        Trade {
            ts_ms: ts,
            price: p,
            qty: q,
            buyer_aggressor: b,
        }
    }
    #[test]
    fn one_second_candles_from_unsorted_trades() {
        let tr = [
            t(1_500, 101.0, 1.0, Some(true)),
            t(1_100, 100.0, 2.0, Some(false)),
            t(1_900, 99.0, 1.0, None),
            t(3_200, 102.0, 1.0, Some(true)),
            t(3_300, f64::NAN, 1.0, None),
        ];
        let a = candles_1s(&tr, 3_500);
        assert_eq!(a.dropped_invalid, 1);
        assert_eq!(
            a.candles.len(),
            2,
            "no synthetic candle for the empty 2s bucket"
        );
        let c = a.candles[0];
        assert_eq!((c.open, c.high, c.low, c.close), (100.0, 101.0, 99.0, 99.0));
        assert_eq!(
            (c.volume, c.buy_volume, c.sell_volume, c.trades),
            (4.0, 1.0, 2.0, 3)
        );
        assert!(c.complete);
        assert!(!a.candles[1].complete, "3s bucket still open at now=3.5s");
        assert_eq!(completed(&a.candles).len(), 1);
    }
    #[test]
    fn two_day_from_completed_days_only() {
        let day = |i: i64, o: f64, c: f64, complete: bool| Candle {
            open_ms: i * MS_PER_DAY,
            width_ms: MS_PER_DAY,
            open: o,
            high: o.max(c) + 1.0,
            low: o.min(c) - 1.0,
            close: c,
            volume: 10.0,
            buy_volume: 0.0,
            sell_volume: 0.0,
            trades: 5,
            complete,
        };
        let d = [
            day(10, 100.0, 105.0, true),
            day(11, 105.0, 103.0, true),
            day(12, 103.0, 110.0, true),
            day(13, 110.0, 111.0, false),
        ];
        let two = aggregate_2d(&d);
        assert_eq!(two.len(), 1, "day 12+13 rejected: 13 incomplete");
        let c = two[0];
        assert_eq!(c.open_ms, 10 * MS_PER_DAY);
        assert_eq!(
            (c.open, c.close, c.high, c.low),
            (100.0, 103.0, 106.0, 99.0)
        );
        assert_eq!(c.volume, 20.0);
        // misaligned pair (11,12) never forms a 2d candle
        assert!(aggregate_2d(&d[1..3]).is_empty());
    }
    #[test]
    fn rolling_windows() {
        let tr = [
            t(0, 100.0, 1.0, Some(true)),
            t(9_000, 101.0, 1.0, Some(false)),
            t(10_000, 102.0, 2.0, None),
        ];
        let w = window_stats(&tr, 10_000, 10_000);
        assert_eq!(w.trades, 2, "(now-10s, now] excludes ts=0");
        assert!((w.log_return - (102.0f64 / 101.0).ln()).abs() < 1e-12);
        assert!((w.vwap - (101.0 + 204.0) / 3.0).abs() < 1e-12);
        assert_eq!(w.unknown_side_volume, 2.0);
        assert_eq!(micro_windows(&tr, 10_000).len(), 8);
    }
    #[test]
    fn gap_scaling() {
        let mk = |s: i64, c: f64| Candle {
            open_ms: s * 1000,
            width_ms: 1000,
            open: c,
            high: c,
            low: c,
            close: c,
            volume: 1.0,
            buy_volume: 0.0,
            sell_volume: 0.0,
            trades: 1,
            complete: true,
        };
        let r = gap_scaled_returns(&[mk(0, 100.0), mk(4, 100.0 * (0.02f64).exp())]);
        assert!((r[0] - 0.01).abs() < 1e-12);
    }
}
