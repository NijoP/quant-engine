//! SL/TP brackets from 120s volatility, and trade-frequency target scenarios.
//!
//! SL_distance = k_s * sigma_120, TP_distance = k_t * sigma_120 in log-return
//! units, sigma_120 = sigma_1s * sqrt(120). Long: TP = E e^{+TP}, SL = E e^{-SL}.
//! Short: TP = E e^{-TP}, SL = E e^{+SL}. k_s, k_t are strategy parameters that
//! must be calibrated chronologically; no universal percentage is hard-coded.

use crate::explain::Explanation;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Side {
    Long,
    Short,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Bracket {
    pub side: Side,
    pub entry: f64,
    pub sigma_120: f64,
    pub tp_ret: f64,
    pub sl_ret: f64,
    pub tp_price: f64,
    pub sl_price: f64,
}

pub fn bracket(
    side: Side,
    entry: f64,
    sigma_1s: f64,
    k_s: f64,
    k_t: f64,
) -> Option<(Bracket, Explanation)> {
    if !(entry > 0.0 && sigma_1s > 0.0 && k_s > 0.0 && k_t > 0.0) {
        return None;
    }
    let s120 = sigma_1s * 120f64.sqrt();
    let (tp, sl) = (k_t * s120, k_s * s120);
    let (tp_price, sl_price) = match side {
        Side::Long => (entry * tp.exp(), entry * (-sl).exp()),
        Side::Short => (entry * (-tp).exp(), entry * sl.exp()),
    };
    let b = Bracket {
        side,
        entry,
        sigma_120: s120,
        tp_ret: tp,
        sl_ret: sl,
        tp_price,
        sl_price,
    };
    let ex = Explanation::new(
        "bracket.ksigma.v1",
        "SL = k_s*sigma_1s*sqrt(120); TP = k_t*sigma_1s*sqrt(120)",
    )
    .input("entry", entry)
    .input("sigma_1s", sigma_1s)
    .input("k_s", k_s)
    .input("k_t", k_t)
    .mid("sigma_120", s120)
    .out("tp_price", tp_price)
    .out("sl_price", sl_price);
    Some((b, ex))
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Scenario {
    pub trades_per_hour: f64,
    pub n_trades: f64,
    /// required average per-trade return r = (Ct/C)^(1/n) - 1
    pub required_return: f64,
}

pub const SCENARIO_RATES: [f64; 5] = [5.0, 10.0, 15.0, 20.0, 30.0];

/// Required per-trade return at each trade frequency for the time remaining.
pub fn scenarios(equity: f64, target: f64, hours_remaining: f64) -> Vec<Scenario> {
    SCENARIO_RATES
        .iter()
        .filter_map(|&tph| {
            let n = tph * hours_remaining;
            crate::target::required_return(equity, target, n).map(|r| Scenario {
                trades_per_hour: tph,
                n_trades: n,
                required_return: r,
            })
        })
        .collect()
}

/// TradesPerHour = n / HoursRemaining, n = ln(Ct/C)/ln(1+r).
pub fn trades_per_hour_needed(
    equity: f64,
    target: f64,
    r: f64,
    hours_remaining: f64,
) -> Option<f64> {
    if hours_remaining <= 0.0 {
        return None;
    }
    crate::target::trades_needed(equity, target, r).map(|n| n / hours_remaining)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn long_short_mirror() {
        let (l, _) = bracket(Side::Long, 100.0, 0.0005, 1.0, 1.5).unwrap();
        let (s, _) = bracket(Side::Short, 100.0, 0.0005, 1.0, 1.5).unwrap();
        assert!(l.tp_price > 100.0 && l.sl_price < 100.0);
        assert!(s.tp_price < 100.0 && s.sl_price > 100.0);
        assert!((l.tp_ret - 1.5 * 0.0005 * 120f64.sqrt()).abs() < 1e-15);
        assert!(((l.tp_price / 100.0).ln() + (s.tp_price / 100.0).ln()).abs() < 1e-12);
        assert!(bracket(Side::Long, 100.0, 0.0, 1.0, 1.0).is_none());
    }
    #[test]
    fn scenario_identities() {
        let sc = scenarios(100.0, 18_000.0, 6.0);
        assert_eq!(sc.len(), 5);
        for s in &sc {
            assert!((100.0 * (1.0 + s.required_return).powf(s.n_trades) - 18_000.0).abs() < 1e-6);
        }
        assert!(sc[0].required_return > sc[4].required_return);
        let tph = trades_per_hour_needed(100.0, 18_000.0, sc[1].required_return, 6.0).unwrap();
        assert!((tph - 10.0).abs() < 1e-9);
    }
}
