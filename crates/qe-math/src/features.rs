//! Feature engine: one unified multi-timeframe feature object per instrument.
//!
//! Formulas (all documented in docs/MATHEMATICAL_MODEL.md):
//! * mid M = (bid + ask)/2; spread_bps = (ask - bid)/M * 1e4
//! * OBI_k = (sum_{i<k} Qb_i - sum_{i<k} Qa_i) / (sum Qb + sum Qa), k in {1,5,10,20}
//! * microprice = (ask*Qb1 + bid*Qa1)/(Qb1 + Qa1)
//! * trade-flow imbalance TFI = (V_buy - V_sell)/(V_buy + V_sell), aggressor-flagged
//!   trades only (unflagged volume reported, never assigned a side)
//! * book OFI (Cont-Kukanov-Stoikov) between two CONSECUTIVE full snapshots:
//!   e = 1[Pb'>=Pb] Qb' - 1[Pb'<=Pb] Qb - 1[Pa'<=Pa] Qa' + 1[Pa'>=Pa] Qa,
//!   only when the snapshot sequence is verifiably consecutive.
//! * realised vol per second sigma_1 = sqrt(mean(r_i^2)) of gap-scaled 1s returns;
//!   horizon vol sigma_h = sigma_1 sqrt(h) (random-walk scaling, labelled historical)
//! * momentum_h = ln(P_now / P_{now-h}); momentum_z_h = momentum_h / (sigma_1 sqrt h)
//! * trade intensity = trades/s per window; intensity_ratio = lambda_10s / lambda_120s
//! * EW stats: m <- m + a(x - m); v <- (1 - a)(v + a(x - m_old)^2)

use crate::explain::Explanation;
use crate::micro;
use crate::stats;
use crate::timeframe::{self, Candle, ContextTf, Trade, WindowStats};
use serde::Serialize;
use std::collections::BTreeMap;

/// Exponentially weighted mean/variance (online, O(1)).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct EwStats {
    pub alpha: f64,
    pub mean: f64,
    pub var: f64,
    pub n: u64,
}

impl EwStats {
    pub fn new(alpha: f64) -> Self {
        assert!(alpha > 0.0 && alpha <= 1.0);
        Self {
            alpha,
            mean: 0.0,
            var: 0.0,
            n: 0,
        }
    }
    /// alpha from a half-life in observations: a = 1 - 2^(-1/h).
    pub fn from_half_life(h: f64) -> Self {
        Self::new(1.0 - 0.5f64.powf(1.0 / h))
    }
    pub fn update(&mut self, x: f64) {
        if !x.is_finite() {
            return;
        }
        if self.n == 0 {
            self.mean = x;
            self.var = 0.0;
        } else {
            let d = x - self.mean;
            let inc = self.alpha * d;
            self.mean += inc;
            self.var = (1.0 - self.alpha) * (self.var + d * inc);
        }
        self.n += 1;
    }
    pub fn sd(&self) -> f64 {
        self.var.max(0.0).sqrt()
    }
    /// Effective sample size of the EW window, capped by observations seen.
    pub fn n_eff(&self) -> f64 {
        ((2.0 - self.alpha) / self.alpha).min(self.n as f64)
    }
}

/// A full depth snapshot (CoinDCX futures `depth-snapshot` events are full
/// snapshots; see docs). Bids descending, asks ascending, (price, qty).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BookSnapshot {
    pub ts_ms: i64,
    /// Exchange version/sequence if the feed provides one.
    pub version: Option<u64>,
    pub bids: Vec<(f64, f64)>,
    pub asks: Vec<(f64, f64)>,
}

impl BookSnapshot {
    pub fn is_sane(&self) -> bool {
        match (self.bids.first(), self.asks.first()) {
            (Some(b), Some(a)) => b.0 > 0.0 && a.0 > b.0 && b.1 > 0.0 && a.1 > 0.0,
            _ => false,
        }
    }
    pub fn depth(&self, levels: usize) -> f64 {
        self.bids.iter().take(levels).map(|l| l.1).sum::<f64>()
            + self.asks.iter().take(levels).map(|l| l.1).sum::<f64>()
    }
}

/// Book OFI between consecutive snapshots. Returns None (feature unavailable)
/// when either book is not sane, when versions are present but not strictly
/// increasing by exactly one, or when the time gap exceeds `max_gap_ms`.
/// Never invents depth changes when integrity cannot be established.
pub fn book_ofi(prev: &BookSnapshot, cur: &BookSnapshot, max_gap_ms: i64) -> Option<f64> {
    if !prev.is_sane() || !cur.is_sane() {
        return None;
    }
    if cur.ts_ms < prev.ts_ms || cur.ts_ms - prev.ts_ms > max_gap_ms {
        return None;
    }
    if let (Some(a), Some(b)) = (prev.version, cur.version) {
        if b != a + 1 {
            return None;
        }
    }
    let (pb, qb) = prev.bids[0];
    let (pa, qa) = prev.asks[0];
    let (pb2, qb2) = cur.bids[0];
    let (pa2, qa2) = cur.asks[0];
    let mut e = 0.0;
    if pb2 >= pb {
        e += qb2;
    }
    if pb2 <= pb {
        e -= qb;
    }
    if pa2 <= pa {
        e -= qa2;
    }
    if pa2 >= pa {
        e += qa;
    }
    Some(e)
}

/// Trade-flow imbalance from aggressor-flagged volume. None if no flagged volume.
pub fn trade_flow_imbalance(w: &WindowStats) -> Option<f64> {
    let s = w.buy_volume + w.sell_volume;
    if s <= 0.0 {
        None
    } else {
        Some((w.buy_volume - w.sell_volume) / s)
    }
}

/// Per-second realised volatility from 1s candles (gap scaled, zero-mean).
pub fn sigma_per_second(c1s: &[Candle]) -> Option<f64> {
    stats::realized_vol(&timeframe::gap_scaled_returns(c1s))
}

/// Context features for one completed-candle timeframe.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct TfContext {
    pub tf: ContextTf,
    pub completed_candles: usize,
    /// log return of the last completed candle
    pub last_return: f64,
    /// per-candle realised vol (zero-mean) over the completed sample
    pub sigma: f64,
    /// trend z = ln(C_last / C_first) / (sigma sqrt(n-1))
    pub trend_z: f64,
    /// last completed candle (high - low)/close relative to the sample mean range
    pub range_ratio: f64,
    /// last completed volume relative to the sample mean (z-score)
    pub volume_z: f64,
}

pub fn tf_context(tf: ContextTf, candles: &[Candle]) -> Option<TfContext> {
    let c = timeframe::completed(candles);
    if c.len() < 3 {
        return None;
    }
    let closes: Vec<f64> = c.iter().map(|x| x.close).collect();
    let r = stats::log_returns(&closes);
    let sigma = stats::realized_vol(&r)?;
    let first = closes[0];
    let last = *closes.last()?;
    let n = (closes.len() - 1) as f64;
    let trend_z = if sigma > 0.0 {
        (last / first).ln() / (sigma * n.sqrt())
    } else {
        0.0
    };
    let ranges: Vec<f64> = c
        .iter()
        .map(|x| (x.high - x.low) / x.close.max(1e-300))
        .collect();
    let mr = stats::mean(&ranges)?;
    let lc = *c.last()?;
    let vols: Vec<f64> = c[..c.len() - 1].iter().map(|x| x.volume).collect();
    Some(TfContext {
        tf,
        completed_candles: c.len(),
        last_return: lc.log_return().unwrap_or(0.0),
        sigma,
        trend_z,
        range_ratio: if mr > 0.0 {
            ranges[ranges.len() - 1] / mr
        } else {
            1.0
        },
        volume_z: stats::zscore(lc.volume, &vols).unwrap_or(0.0),
    })
}

/// Everything the feature engine needs for one instrument at `now_ms`.
#[derive(Debug, Clone, Default)]
pub struct MarketSnapshot {
    pub now_ms: i64,
    /// recent trades (at least the last 120s, ideally ~15 min for vol estimates)
    pub trades: Vec<Trade>,
    pub book: Option<BookSnapshot>,
    pub prev_book: Option<BookSnapshot>,
    /// exchange candles per context timeframe (2d may be omitted: derived from 1d)
    pub candles: BTreeMap<ContextTf, Vec<Candle>>,
    /// reference (EW) depth over top 10 levels and EW spread, from the caller's
    /// running state; None if not yet established
    pub ref_depth10: Option<f64>,
    pub ref_spread_bps: Option<f64>,
    /// age of the freshest market event, ms (caller measures against wall clock)
    pub data_age_ms: i64,
}

/// Unified multi-timeframe feature object.
#[derive(Debug, Clone, Serialize)]
pub struct FeatureVector {
    pub now_ms: i64,
    pub mid: Option<f64>,
    pub spread_bps: Option<f64>,
    pub microprice: Option<f64>,
    /// (microprice - mid) / mid * 1e4
    pub microprice_offset_bps: Option<f64>,
    pub obi: BTreeMap<usize, f64>,
    pub book_ofi: Option<f64>,
    /// book OFI normalised by mean top-of-book size
    pub book_ofi_norm: Option<f64>,
    pub depth10: Option<f64>,
    pub depth_ratio: Option<f64>,
    pub spread_ratio: Option<f64>,
    pub windows: Vec<WindowStats>,
    pub tfi: BTreeMap<i64, f64>,
    pub sigma_1s: Option<f64>,
    /// historical (realised) vol scaled to each micro horizon
    pub sigma_h: BTreeMap<i64, f64>,
    /// short/long realised vol ratio: sigma(last 30s) / sigma(full sample)
    pub vol_ratio_micro: Option<f64>,
    /// micro sigma vs 1m-candle sigma scaled to per-second
    pub vol_ratio_context: Option<f64>,
    pub momentum: BTreeMap<i64, f64>,
    pub momentum_z: BTreeMap<i64, f64>,
    pub intensity_10s: f64,
    pub intensity_120s: f64,
    pub intensity_ratio: Option<f64>,
    pub volume_z_60s: Option<f64>,
    pub context: BTreeMap<String, TfContext>,
    pub last_price: Option<f64>,
    pub data_age_ms: i64,
    pub quality_flags: Vec<String>,
}

pub const OBI_LEVELS: [usize; 4] = [1, 5, 10, 20];

/// Compute the full feature vector plus an explanation of every value.
pub fn compute_features(s: &MarketSnapshot) -> (FeatureVector, Explanation) {
    let mut flags: Vec<String> = Vec::new();
    let mut ex = Explanation::new(
        "features.v1",
        "see module docs: mid, spread_bps, OBI_k, microprice, OFI, TFI, sigma_h, momentum_z, intensity",
    )
    .input("now_ms", s.now_ms as f64)
    .input("trades", s.trades.len() as f64)
    .input("data_age_ms", s.data_age_ms as f64);

    let book = s.book.as_ref().filter(|b| b.is_sane());
    if s.book.is_some() && book.is_none() {
        flags.push("book_insane".into());
    }
    if book.is_none() {
        flags.push("no_book".into());
    }
    let (mut mid, mut spread, mut mp, mut mpo, mut obi) = (None, None, None, None, BTreeMap::new());
    let (mut depth10, mut depth_ratio, mut spread_ratio) = (None, None, None);
    if let Some(b) = book {
        let (bid, qb) = b.bids[0];
        let (ask, qa) = b.asks[0];
        let m = micro::mid(bid, ask);
        let sp = micro::spread_bps(bid, ask);
        let mpr = micro::microprice(bid, qb, ask, qa);
        mid = Some(m);
        spread = Some(sp);
        mp = Some(mpr);
        mpo = Some((mpr - m) / m * 1e4);
        for k in OBI_LEVELS {
            if let Some(v) = micro::obi(&b.bids, &b.asks, k) {
                obi.insert(k, v);
            }
        }
        let d = b.depth(10);
        depth10 = Some(d);
        depth_ratio = s.ref_depth10.filter(|r| *r > 0.0).map(|r| d / r);
        spread_ratio = s.ref_spread_bps.filter(|r| *r > 0.0).map(|r| sp / r);
        ex = ex
            .input("best_bid", bid)
            .input("best_ask", ask)
            .input("bid_qty1", qb)
            .input("ask_qty1", qa)
            .mid("mid", m)
            .mid("spread_bps", sp)
            .mid("microprice", mpr);
    }
    let (ofi, ofi_norm) = match (s.prev_book.as_ref(), book) {
        (Some(p), Some(c)) => match book_ofi(p, c, 5_000) {
            Some(e) => {
                let scale = (p.bids[0].1 + p.asks[0].1 + c.bids[0].1 + c.asks[0].1) / 4.0;
                (Some(e), Some(e / scale.max(1e-300)))
            }
            None => {
                flags.push("ofi_unavailable_sequence".into());
                (None, None)
            }
        },
        _ => (None, None),
    };

    let windows = timeframe::micro_windows(&s.trades, s.now_ms);
    let mut tfi = BTreeMap::new();
    for w in &windows {
        if let Some(v) = trade_flow_imbalance(w) {
            tfi.insert(w.window_ms / 1000, v);
        }
    }
    let agg = timeframe::candles_1s(&s.trades, s.now_ms);
    let c1 = timeframe::completed(&agg.candles);
    let sigma_1s = sigma_per_second(&c1).filter(|v| *v > 0.0);
    if sigma_1s.is_none() {
        flags.push("sigma_unavailable".into());
    }
    let mut sigma_h = BTreeMap::new();
    if let Some(sg) = sigma_1s {
        for h in timeframe::MICRO_WINDOWS_S {
            sigma_h.insert(h, sg * (h as f64).sqrt());
        }
    }
    let recent: Vec<Candle> = c1
        .iter()
        .copied()
        .filter(|c| c.open_ms >= s.now_ms - 30_000)
        .collect();
    let vol_ratio_micro = match (sigma_per_second(&recent), sigma_1s) {
        (Some(a), Some(b)) if recent.len() >= 5 => Some(a / b),
        _ => None,
    };

    // momentum from 1s closes: price at or before now - h
    let last_price = c1.last().map(|c| c.close).or_else(|| {
        windows
            .last()
            .map(|w| w.last_price)
            .filter(|p| p.is_finite())
    });
    let mut momentum = BTreeMap::new();
    let mut momentum_z = BTreeMap::new();
    if let Some(lp) = last_price {
        for h in [5i64, 15, 30, 60, 120] {
            let cutoff = s.now_ms - h * 1000;
            if let Some(c) = c1.iter().rev().find(|c| c.close_ms() <= cutoff) {
                let m = (lp / c.close).ln();
                momentum.insert(h, m);
                if let Some(sg) = sigma_1s {
                    momentum_z.insert(h, m / (sg * (h as f64).sqrt()));
                }
            }
        }
    }
    let w10 = windows.iter().find(|w| w.window_ms == 10_000).copied();
    let w120 = windows.iter().find(|w| w.window_ms == 120_000).copied();
    let i10 = w10.map(|w| w.intensity).unwrap_or(0.0);
    let i120 = w120.map(|w| w.intensity).unwrap_or(0.0);
    let intensity_ratio = if i120 > 0.0 { Some(i10 / i120) } else { None };

    // 60s volume vs previous 60s blocks over the trade sample
    let vol60: Vec<f64> = (1..=10)
        .map(|k| timeframe::window_stats(&s.trades, s.now_ms - k * 60_000, 60_000).volume)
        .collect();
    let cur60 = timeframe::window_stats(&s.trades, s.now_ms, 60_000).volume;
    let volume_z_60s = if vol60.iter().filter(|v| **v > 0.0).count() >= 3 {
        stats::zscore(cur60, &vol60)
    } else {
        None
    };

    let mut context = BTreeMap::new();
    for tf in ContextTf::ALL {
        let derived;
        let cs: &[Candle] = if tf == ContextTf::D2 {
            match s.candles.get(&ContextTf::D2) {
                Some(v) => v,
                None => {
                    derived = timeframe::aggregate_2d(
                        s.candles.get(&ContextTf::D1).map(|v| &v[..]).unwrap_or(&[]),
                    );
                    &derived
                }
            }
        } else {
            match s.candles.get(&tf) {
                Some(v) => v,
                None => continue,
            }
        };
        if let Some(c) = tf_context(tf, cs) {
            context.insert(tf.label().to_string(), c);
        }
    }
    let vol_ratio_context = match (sigma_1s, context.get("1m")) {
        (Some(a), Some(c)) if c.sigma > 0.0 => Some(a / (c.sigma / 60f64.sqrt())),
        _ => None,
    };
    if s.data_age_ms > 5_000 {
        flags.push("stale_data".into());
    }

    if let Some(v) = sigma_1s {
        ex = ex.mid("sigma_1s", v);
    }
    for (k, v) in &momentum_z {
        ex = ex.mid(&format!("momentum_z_{k}s"), *v);
    }
    for (k, v) in &obi {
        ex = ex.mid(&format!("obi_{k}"), *v);
    }
    ex = ex.mid("intensity_10s", i10).mid("intensity_120s", i120);
    for f in &flags {
        ex = ex.note(f.clone());
    }

    (
        FeatureVector {
            now_ms: s.now_ms,
            mid,
            spread_bps: spread,
            microprice: mp,
            microprice_offset_bps: mpo,
            obi,
            book_ofi: ofi,
            book_ofi_norm: ofi_norm,
            depth10,
            depth_ratio,
            spread_ratio,
            windows,
            tfi,
            sigma_1s,
            sigma_h,
            vol_ratio_micro,
            vol_ratio_context,
            momentum,
            momentum_z,
            intensity_10s: i10,
            intensity_120s: i120,
            intensity_ratio,
            volume_z_60s,
            context,
            last_price,
            data_age_ms: s.data_age_ms,
            quality_flags: flags,
        },
        ex,
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn book(ts: i64, v: Option<u64>, bid: (f64, f64), ask: (f64, f64)) -> BookSnapshot {
        BookSnapshot {
            ts_ms: ts,
            version: v,
            bids: vec![bid, (bid.0 - 1.0, 5.0)],
            asks: vec![ask, (ask.0 + 1.0, 5.0)],
        }
    }

    /// Deterministic synthetic trade tape: random walk with drift, 4 trades/s.
    pub fn tape(n_s: i64, drift_per_s: f64, sigma_per_s: f64, seed: u64) -> Vec<Trade> {
        let mut rng = crate::target::SplitMix64(seed);
        let mut lp = 100f64.ln();
        let mut v = Vec::new();
        for i in 0..n_s * 4 {
            let u1 = rng.next_f64().max(1e-12);
            let u2 = rng.next_f64();
            let z = (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos();
            lp += drift_per_s / 4.0 + sigma_per_s / 2.0 * z;
            v.push(Trade {
                ts_ms: i * 250,
                price: lp.exp(),
                qty: 1.0,
                buyer_aggressor: Some(z > 0.0),
            });
        }
        v
    }

    #[test]
    fn ew_stats_converge() {
        let mut e = EwStats::new(0.05);
        for i in 0..2000 {
            e.update(if i % 2 == 0 { 1.0 } else { -1.0 });
        }
        assert!(e.mean.abs() < 0.05);
        assert!((e.sd() - 1.0).abs() < 0.05);
        assert!((EwStats::from_half_life(1.0).alpha - 0.5).abs() < 1e-12);
    }

    #[test]
    fn ofi_semantics_and_integrity() {
        let a = book(0, Some(1), (100.0, 2.0), (101.0, 3.0));
        // bid size grows at same price, ask unchanged: e = 5 - 2 = +3
        let b = book(100, Some(2), (100.0, 5.0), (101.0, 3.0));
        assert_eq!(book_ofi(&a, &b, 1000), Some(3.0));
        // sequence gap -> unavailable, never guessed
        let c = book(100, Some(4), (100.0, 5.0), (101.0, 3.0));
        assert_eq!(book_ofi(&a, &c, 1000), None);
        // stale gap
        let d = book(10_000, None, (100.0, 5.0), (101.0, 3.0));
        assert_eq!(book_ofi(&a, &d, 1000), None);
        // crossed book
        let x = book(100, Some(2), (102.0, 5.0), (101.0, 3.0));
        assert_eq!(book_ofi(&a, &x, 1000), None);
    }

    #[test]
    fn feature_vector_end_to_end() {
        let trades = tape(900, 0.0, 0.0005, 7);
        let now = 900_000;
        let s = MarketSnapshot {
            now_ms: now,
            trades,
            book: Some(book(now, Some(2), (99.99, 4.0), (100.01, 1.0))),
            prev_book: Some(book(now - 100, Some(1), (99.99, 2.0), (100.01, 1.0))),
            ref_depth10: Some(12.0),
            ref_spread_bps: Some(2.0),
            data_age_ms: 100,
            ..Default::default()
        };
        let (f, ex) = compute_features(&s);
        assert!((f.spread_bps.unwrap() - 2.0).abs() < 1e-6);
        assert!(f.obi[&1] > 0.0);
        assert!(f.book_ofi.unwrap() > 0.0);
        let sg = f.sigma_1s.unwrap();
        assert!((sg / 0.0005 - 1.0).abs() < 0.1, "sigma_1s {sg}");
        assert!((f.sigma_h[&120] - sg * 120f64.sqrt()).abs() < 1e-15);
        assert_eq!(f.windows.len(), 8);
        assert!((f.intensity_120s - 4.0).abs() < 0.05);
        assert!(f.momentum_z.contains_key(&60));
        assert!(f.quality_flags.is_empty(), "{:?}", f.quality_flags);
        assert!(ex.to_json().contains("sigma_1s"));
        // determinism
        let (g, ex2) = compute_features(&s);
        assert_eq!(
            serde_json::to_string(&f).unwrap(),
            serde_json::to_string(&g).unwrap()
        );
        assert_eq!(ex, ex2);
    }
}
