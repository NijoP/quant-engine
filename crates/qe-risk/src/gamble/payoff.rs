//! Gamble payoff per bet (levers 1b and 3): cost-aware instrument ranking and
//! volatility-scaled TP/SL with liquidation-safe leverage.
//!
//! - Cost: actual maker/taker fee, spread and a depth-impact term, so maker
//!   (post-only) entries and cheap instruments rank higher.
//! - Exclusion: an instrument whose round-trip cost exceeds
//!   `max_cost_share_of_tp` of the TP distance is not bet on.
//! - Barriers: TP is a share of the measured 120 s range, so it is reachable
//!   within the 120 s time exit; SL is set from TP and is never removed.
//! - Leverage: min(owner, instrument max), lowered until the liquidation
//!   price lies beyond the SL (plus cost and a buffer).
//! - Output: `win_p`, `payoff_ratio` and `cost_frac` for `qe_math::gamble`
//!   target odds.
//!
//! Pure and deterministic: no I/O, no clock, no orders.
#![allow(clippy::neg_cmp_op_on_partial_ord)]
use serde::{Deserialize, Serialize};

/// Owner-tunable payoff policy (`[payoff]` in `configs/gamble.toml`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PayoffCfg {
    /// Exclude when round-trip cost / TP distance exceeds this share.
    pub max_cost_share_of_tp: f64,
    /// TP distance as a share of the measured 120 s high-low range.
    pub tp_range_share: f64,
    /// SL distance = TP distance * this ratio (SL is always set).
    pub sl_to_tp: f64,
    /// Price an entry as maker (post-only limit) instead of taker.
    pub maker_entry: bool,
    /// Extra distance (fraction of price) kept between SL and liquidation.
    pub liq_buffer_frac: f64,
    /// Shrinkage weight (pseudo-samples) of the model hit probability
    /// against measured TP-first hits.
    pub prior_weight: f64,
}

impl Default for PayoffCfg {
    fn default() -> Self {
        Self {
            max_cost_share_of_tp: 0.25,
            tp_range_share: 0.5,
            sl_to_tp: 0.5,
            maker_entry: true,
            liq_buffer_frac: 0.001,
            prior_weight: 20.0,
        }
    }
}

impl PayoffCfg {
    pub fn validate(&self) -> Result<(), String> {
        let pos = |n: &str, v: f64| {
            if v > 0.0 && v.is_finite() {
                Ok(())
            } else {
                Err(format!("payoff.{n} = {v} must be > 0"))
            }
        };
        pos("max_cost_share_of_tp", self.max_cost_share_of_tp)?;
        pos("tp_range_share", self.tp_range_share)?;
        pos("sl_to_tp", self.sl_to_tp)?;
        pos("prior_weight", self.prior_weight)?;
        if !(self.max_cost_share_of_tp < 1.0) {
            return Err("payoff.max_cost_share_of_tp must be < 1".into());
        }
        if !(self.liq_buffer_frac >= 0.0) {
            return Err("payoff.liq_buffer_frac must be >= 0".into());
        }
        Ok(())
    }
}

/// Live market facts for one instrument and side, fractions of price.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstrumentQuote {
    pub pair: String,
    pub long: bool,
    pub maker_fee: f64,
    pub taker_fee: f64,
    /// (ask - bid) / mid.
    pub spread_frac: f64,
    /// Top-of-book depth on the side the exit trades against (INR notional).
    pub depth_inr: f64,
    pub max_leverage: u32,
    /// Maintenance margin rate.
    pub maint_margin_frac: f64,
    /// Measured median 120 s high-low range / price.
    pub range_120s_frac: f64,
    /// Measured 120 s return standard deviation / price.
    pub sigma_120s_frac: f64,
    /// Measured TP-first outcomes at these barriers: (samples, hits).
    pub measured: Option<(u32, u32)>,
}

/// Round-trip cost per unit notional. Entry: maker fee and no spread when
/// posting, else taker fee plus half the spread. Exit (TP/SL/time) is assumed
/// taker: taker fee plus half the spread plus impact `notional/depth * spread`.
pub fn round_trip_cost(q: &InstrumentQuote, maker_entry: bool, notional: f64) -> f64 {
    let spread = q.spread_frac.max(0.0);
    let half = spread / 2.0;
    let entry = if maker_entry {
        q.maker_fee
    } else {
        q.taker_fee + half
    };
    let impact = if q.depth_inr > 0.0 {
        (notional.max(0.0) / q.depth_inr).min(1.0) * spread
    } else {
        spread
    };
    entry + q.taker_fee + half + impact
}

fn norm_cdf(x: f64) -> f64 {
    // Abramowitz-Stegun 7.1.26 erf approximation (|err| < 1.5e-7).
    let z = x.abs() / std::f64::consts::SQRT_2;
    let t = 1.0 / (1.0 + 0.327_591_1 * z);
    let poly = t
        * (0.254_829_592
            + t * (-0.284_496_736
                + t * (1.421_413_741 + t * (-1.453_152_027 + t * 1.061_405_429))));
    let erf = 1.0 - poly * (-z * z).exp();
    if x >= 0.0 {
        0.5 * (1.0 + erf)
    } else {
        0.5 * (1.0 - erf)
    }
}

/// Driftless model of P(TP hit first within the horizon): the chance to touch
/// the nearer barrier (reflection principle, 2 * (1 - Phi(d / sigma))) times
/// the gambler's-ruin share sl / (tp + sl).
pub fn model_hit_prob(tp: f64, sl: f64, sigma: f64) -> f64 {
    if !(tp > 0.0 && sl > 0.0 && sigma > 0.0) {
        return 0.0;
    }
    let reach = (2.0 * (1.0 - norm_cdf(tp.min(sl) / sigma))).clamp(0.0, 1.0);
    (reach * sl / (tp + sl)).clamp(0.0, 1.0)
}

/// One sized-and-priced bet plan.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PayoffPlan {
    pub pair: String,
    pub long: bool,
    pub maker_entry: bool,
    pub tp_frac: f64,
    pub sl_frac: f64,
    pub cost_frac: f64,
    /// cost / TP distance.
    pub cost_share_of_tp: f64,
    /// P(TP first within 120 s): model shrunk toward measured hits.
    pub win_p: f64,
    /// Net win / net loss: (tp - cost) / (sl + cost).
    pub payoff_ratio: f64,
    pub leverage: u32,
    /// Liquidation distance from entry (fraction of price).
    pub liq_frac: f64,
    /// Expected net return per unit notional (stop or time exit = loss).
    pub ev_frac: f64,
    /// Expected net return per unit margin: ev_frac * leverage (rank key).
    pub ev_per_margin: f64,
    /// 120 s range / round-trip cost (tie-break: prefer more range per cost).
    pub range_vs_cost: f64,
}

/// Plan one bet or say why the instrument is excluded. `notional` is the
/// intended bet size (INR), used only for the depth-impact cost.
pub fn plan(
    q: &InstrumentQuote,
    cfg: &PayoffCfg,
    owner_leverage: u32,
    notional: f64,
) -> Result<PayoffPlan, String> {
    cfg.validate()?;
    if !(q.range_120s_frac > 0.0 && q.sigma_120s_frac > 0.0) {
        return Err(format!("{}: no live 120 s volatility", q.pair));
    }
    let cost = round_trip_cost(q, cfg.maker_entry, notional);
    if !(cost >= 0.0 && cost.is_finite()) {
        return Err(format!("{}: round-trip cost not computable", q.pair));
    }
    let tp = q.range_120s_frac * cfg.tp_range_share;
    let sl = tp * cfg.sl_to_tp;
    let share = cost / tp;
    if share > cfg.max_cost_share_of_tp {
        return Err(format!(
            "{}: round-trip cost {:.2} bps is {:.0}% of TP {:.2} bps (max {:.0}%)",
            q.pair,
            cost * 1e4,
            share * 100.0,
            tp * 1e4,
            cfg.max_cost_share_of_tp * 100.0
        ));
    }
    // Liquidation distance ~ 1/L - mmr must exceed sl + cost + buffer.
    let need = sl + cost + cfg.liq_buffer_frac + q.maint_margin_frac.max(0.0);
    let liq_cap = (1.0 / need).floor().max(0.0) as u32;
    let leverage = owner_leverage.min(q.max_leverage).min(liq_cap);
    if leverage < 1 {
        return Err(format!(
            "{}: no leverage keeps liquidation beyond the SL",
            q.pair
        ));
    }
    let liq_frac = 1.0 / leverage as f64 - q.maint_margin_frac.max(0.0);
    let p_model = model_hit_prob(tp, sl, q.sigma_120s_frac);
    let win_p = match q.measured {
        Some((n, hits)) if n > 0 => {
            (hits.min(n) as f64 + cfg.prior_weight * p_model) / (n as f64 + cfg.prior_weight)
        }
        _ => p_model,
    }
    .clamp(0.0, 1.0);
    let win = tp - cost;
    let loss = sl + cost;
    let ev = win_p * win - (1.0 - win_p) * loss;
    Ok(PayoffPlan {
        pair: q.pair.clone(),
        long: q.long,
        maker_entry: cfg.maker_entry,
        tp_frac: tp,
        sl_frac: sl,
        cost_frac: cost,
        cost_share_of_tp: share,
        win_p,
        payoff_ratio: win / loss,
        leverage,
        liq_frac,
        ev_frac: ev,
        ev_per_margin: ev * leverage as f64,
        range_vs_cost: if cost > 0.0 {
            q.range_120s_frac / cost
        } else {
            f64::INFINITY
        },
    })
}

/// Plan every quote, drop excluded ones (returned with their reason), and rank
/// the rest by expected net payoff per unit margin, then range per cost.
pub fn rank(
    quotes: &[InstrumentQuote],
    cfg: &PayoffCfg,
    owner_leverage: u32,
    notional: f64,
) -> (Vec<PayoffPlan>, Vec<String>) {
    let mut ok = Vec::new();
    let mut excluded = Vec::new();
    for q in quotes {
        match plan(q, cfg, owner_leverage, notional) {
            Ok(p) => ok.push(p),
            Err(e) => excluded.push(e),
        }
    }
    ok.sort_by(|a, b| {
        b.ev_per_margin
            .total_cmp(&a.ev_per_margin)
            .then(b.range_vs_cost.total_cmp(&a.range_vs_cost))
    });
    (ok, excluded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(pair: &str, range: f64) -> InstrumentQuote {
        InstrumentQuote {
            pair: pair.into(),
            long: true,
            maker_fee: 0.0002,
            taker_fee: 0.0005,
            spread_frac: 0.0002,
            depth_inr: 1_000_000.0,
            max_leverage: 50,
            maint_margin_frac: 0.005,
            range_120s_frac: range,
            sigma_120s_frac: range / 2.0,
            measured: None,
        }
    }

    #[test]
    fn maker_entry_is_cheaper_than_taker() {
        let x = q("B-BTC_USDT", 0.01);
        let m = round_trip_cost(&x, true, 1000.0);
        let t = round_trip_cost(&x, false, 1000.0);
        assert!(m < t, "maker {m} taker {t}");
        // maker 2 + taker 5 + half spread 1 bps, impact ~0
        assert!((m - 0.0008).abs() < 1e-5, "{m}");
    }

    #[test]
    fn depth_impact_raises_cost() {
        let mut x = q("X", 0.01);
        let a = round_trip_cost(&x, true, 1000.0);
        x.depth_inr = 1000.0;
        let b = round_trip_cost(&x, true, 1000.0);
        assert!(b > a);
    }

    #[test]
    fn excludes_when_cost_is_large_share_of_tp() {
        // range 20 bps -> TP 10 bps, cost ~8 bps = 80% > 25%
        let e = plan(&q("CALM", 0.002), &PayoffCfg::default(), 10, 1000.0).unwrap_err();
        assert!(e.contains("round-trip cost"), "{e}");
        assert!(plan(&q("WILD", 0.02), &PayoffCfg::default(), 10, 1000.0).is_ok());
    }

    #[test]
    fn tp_tracks_volatility_and_sl_is_always_set() {
        let p = plan(&q("WILD", 0.02), &PayoffCfg::default(), 10, 1000.0).unwrap();
        assert!((p.tp_frac - 0.01).abs() < 1e-12);
        assert!((p.sl_frac - 0.005).abs() < 1e-12);
        assert!(p.payoff_ratio > 1.0);
        assert!(p.win_p > 0.0 && p.win_p < 1.0);
    }

    #[test]
    fn leverage_capped_by_owner_instrument_and_liquidation() {
        let cfg = PayoffCfg::default();
        let x = q("WILD", 0.02);
        assert_eq!(plan(&x, &cfg, 5, 1000.0).unwrap().leverage, 5);
        let mut y = x.clone();
        y.max_leverage = 3;
        assert_eq!(plan(&y, &cfg, 100, 1000.0).unwrap().leverage, 3);
        // owner 100x, instrument 50x, TP 5%: liquidation (31x) must stay beyond SL
        let p = plan(&q("HUGE", 0.1), &cfg, 100, 1000.0).unwrap();
        assert_eq!(p.leverage, 31);
        assert!(p.leverage < 50);
        assert!(p.liq_frac > p.sl_frac + p.cost_frac, "{p:?}");
    }

    #[test]
    fn measured_hits_pull_win_p() {
        let cfg = PayoffCfg::default();
        let mut x = q("WILD", 0.02);
        let base = plan(&x, &cfg, 10, 1000.0).unwrap().win_p;
        x.measured = Some((200, 180));
        let hi = plan(&x, &cfg, 10, 1000.0).unwrap().win_p;
        assert!(hi > base && hi < 0.9 + 1e-9, "{base} {hi}");
    }

    #[test]
    fn rank_prefers_higher_payoff_per_margin_and_reports_exclusions() {
        let cfg = PayoffCfg::default();
        let mut good = q("GOOD", 0.02);
        good.measured = Some((100, 60));
        let mut bad = q("BAD", 0.02);
        bad.measured = Some((100, 10));
        let (ok, ex) = rank(&[bad, q("CALM", 0.002), good], &cfg, 10, 1000.0);
        assert_eq!(ok.len(), 2);
        assert_eq!(ok[0].pair, "GOOD");
        assert_eq!(ex.len(), 1);
        assert!(ex[0].starts_with("CALM"));
    }

    #[test]
    fn model_hit_prob_sane() {
        assert_eq!(model_hit_prob(0.0, 0.01, 0.01), 0.0);
        let p = model_hit_prob(0.01, 0.01, 0.01);
        assert!(p > 0.1 && p <= 0.5, "{p}");
        assert!(model_hit_prob(0.01, 0.01, 0.1) > model_hit_prob(0.01, 0.01, 0.005));
        assert!((norm_cdf(0.0) - 0.5).abs() < 1e-7);
    }

    #[test]
    fn bad_cfg_refused() {
        let cfg = PayoffCfg {
            sl_to_tp: 0.0,
            ..PayoffCfg::default()
        };
        assert!(plan(&q("WILD", 0.02), &cfg, 10, 1000.0).is_err());
    }
}
