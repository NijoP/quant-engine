//! Deterministic mathematics for the QUANT-ENGINE desk.
//!
//! Every function here is pure and has no hidden state or randomness. The only
//! pseudo-random routine (`SplitMix64`) is explicitly seeded, so identical inputs
//! always reproduce identical outputs. Formulas are documented inline.

pub mod barrier;
pub mod dist;
pub mod explain;
pub mod features;
pub mod gamble;
pub mod learn;
pub mod timeframe;

pub mod stats {
    /// Arithmetic mean. Returns `None` on empty input.
    pub fn mean(x: &[f64]) -> Option<f64> {
        if x.is_empty() {
            return None;
        }
        Some(x.iter().sum::<f64>() / x.len() as f64)
    }

    /// Unbiased sample variance: s^2 = sum (x_i - xbar)^2 / (n - 1).
    pub fn variance(x: &[f64]) -> Option<f64> {
        if x.len() < 2 {
            return None;
        }
        let m = mean(x)?;
        Some(x.iter().map(|v| (v - m).powi(2)).sum::<f64>() / (x.len() - 1) as f64)
    }

    pub fn stdev(x: &[f64]) -> Option<f64> {
        variance(x).map(f64::sqrt)
    }

    /// Log returns r_i = ln(p_i / p_{i-1}); non-positive prices are skipped.
    pub fn log_returns(p: &[f64]) -> Vec<f64> {
        p.windows(2)
            .filter(|w| w[0] > 0.0 && w[1] > 0.0)
            .map(|w| (w[1] / w[0]).ln())
            .collect()
    }

    /// Realised volatility per step: sqrt(sum r_i^2 / n) (zero-mean estimator,
    /// standard for very short horizons where the sample mean is noise).
    pub fn realized_vol(returns: &[f64]) -> Option<f64> {
        if returns.is_empty() {
            return None;
        }
        Some((returns.iter().map(|r| r * r).sum::<f64>() / returns.len() as f64).sqrt())
    }

    /// z = (x - mean) / stdev of the reference sample. `None` if stdev ~ 0.
    pub fn zscore(x: f64, reference: &[f64]) -> Option<f64> {
        let m = mean(reference)?;
        let s = stdev(reference)?;
        if s <= 1e-15 {
            return None;
        }
        Some((x - m) / s)
    }

    /// Exponentially weighted mean with smoothing alpha in (0,1]:
    /// m_t = alpha * x_t + (1 - alpha) * m_{t-1}, m_0 = x_0.
    pub fn ewma(x: &[f64], alpha: f64) -> Option<f64> {
        let mut it = x.iter();
        let mut m = *it.next()?;
        for v in it {
            m = alpha * v + (1.0 - alpha) * m;
        }
        Some(m)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn basic_moments() {
            let x = [1.0, 2.0, 3.0, 4.0];
            assert_eq!(mean(&x), Some(2.5));
            assert!((variance(&x).unwrap() - 1.666_666_666_666_666_7).abs() < 1e-12);
            assert_eq!(mean(&[]), None);
        }
        #[test]
        fn log_returns_and_vol() {
            let r = log_returns(&[100.0, 110.0, 99.0]);
            assert!((r[0] - (1.1f64).ln()).abs() < 1e-12);
            assert!((r[1] - (0.9f64).ln()).abs() < 1e-12);
            let v = realized_vol(&r).unwrap();
            assert!((v - ((r[0] * r[0] + r[1] * r[1]) / 2.0).sqrt()).abs() < 1e-12);
        }
        #[test]
        fn z_and_ewma() {
            let z = zscore(5.0, &[1.0, 2.0, 3.0]).unwrap();
            assert!((z - 3.0).abs() < 1e-12);
            assert_eq!(zscore(1.0, &[2.0, 2.0, 2.0]), None);
            assert!((ewma(&[0.0, 10.0], 0.5).unwrap() - 5.0).abs() < 1e-12);
        }
    }
}

pub mod micro {
    /// Mid price M = (bid + ask) / 2.
    pub fn mid(bid: f64, ask: f64) -> f64 {
        (bid + ask) / 2.0
    }

    /// Spread in basis points: (ask - bid) / M * 10_000.
    pub fn spread_bps(bid: f64, ask: f64) -> f64 {
        (ask - bid) / mid(bid, ask) * 10_000.0
    }

    /// Order-book imbalance over the first `levels` levels of each side:
    /// OBI = (Qb - Qa) / (Qb + Qa), in [-1, 1]. Levels are (price, qty),
    /// bids sorted descending and asks ascending.
    pub fn obi(bids: &[(f64, f64)], asks: &[(f64, f64)], levels: usize) -> Option<f64> {
        let qb: f64 = bids.iter().take(levels).map(|l| l.1).sum();
        let qa: f64 = asks.iter().take(levels).map(|l| l.1).sum();
        if qb + qa <= 0.0 {
            return None;
        }
        Some((qb - qa) / (qb + qa))
    }

    /// Microprice = (ask * Qb + bid * Qa) / (Qb + Qa) using top-of-book sizes.
    pub fn microprice(bid: f64, qb: f64, ask: f64, qa: f64) -> f64 {
        if qb + qa <= 0.0 {
            return mid(bid, ask);
        }
        (ask * qb + bid * qa) / (qb + qa)
    }

    /// Volume-weighted fill price when taking `qty` from `levels` (best first).
    /// Returns (vwap, filled_qty); filled < qty means the visible book was too thin.
    pub fn walk_book(levels: &[(f64, f64)], qty: f64) -> (f64, f64) {
        let mut left = qty;
        let mut cost = 0.0;
        for &(p, q) in levels {
            if left <= 0.0 {
                break;
            }
            let take = q.min(left);
            cost += take * p;
            left -= take;
        }
        let filled = qty - left.max(0.0);
        if filled <= 0.0 {
            return (f64::NAN, 0.0);
        }
        (cost / filled, filled)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn book_math() {
            assert!((spread_bps(99.0, 101.0) - 200.0).abs() < 1e-9);
            let bids = [(99.0, 3.0), (98.0, 1.0)];
            let asks = [(101.0, 1.0), (102.0, 1.0)];
            assert!((obi(&bids, &asks, 1).unwrap() - 0.5).abs() < 1e-12);
            assert!((obi(&bids, &asks, 2).unwrap() - (4.0 - 2.0) / 6.0).abs() < 1e-12);
            assert!((microprice(99.0, 3.0, 101.0, 1.0) - 100.5).abs() < 1e-12);
            let (vwap, f) = walk_book(&asks, 1.5);
            assert!((vwap - (101.0 + 0.5 * 102.0) / 1.5).abs() < 1e-12);
            assert_eq!(f, 1.5);
            assert_eq!(walk_book(&asks, 5.0).1, 2.0);
        }
    }
}

pub mod prob {
    use serde::Serialize;

    /// Standard normal CDF via erf (Abramowitz & Stegun 7.1.26, |err| < 1.5e-7).
    pub fn norm_cdf(x: f64) -> f64 {
        0.5 * (1.0 + erf(x / std::f64::consts::SQRT_2))
    }

    fn erf(x: f64) -> f64 {
        let s = x.signum();
        let x = x.abs();
        let t = 1.0 / (1.0 + 0.327_591_1 * x);
        let y = 1.0
            - (((((1.061_405_429 * t - 1.453_152_027) * t) + 1.421_413_741) * t - 0.284_496_736)
                * t
                + 0.254_829_592)
                * t
                * (-x * x).exp();
        s * y
    }

    /// First-passage outcome of a drifted random walk between a profit barrier
    /// and a loss barrier within a hard time limit.
    #[derive(Debug, Clone, Copy, PartialEq, Serialize)]
    pub struct FirstPassage {
        /// P(target reached before stop within T)
        pub p_target: f64,
        /// P(stop reached before target within T)
        pub p_stop: f64,
        /// P(neither within T) = 1 - p_target - p_stop
        pub p_neither: f64,
        /// E[time to target | target first] in seconds
        pub e_t_target: f64,
        /// E[time to stop | stop first] in seconds
        pub e_t_stop: f64,
        /// E[log-return at T | neither barrier touched]
        pub e_ret_neither: f64,
    }

    /// Deterministic binomial-lattice first-passage solver.
    ///
    /// Model: log-price X_t = mu t + sigma W_t (per-second mu, sigma), X_0 = 0,
    /// expressed in the trade's favourable direction (caller flips mu for shorts).
    /// Discretise with dt seconds: step h = sigma sqrt(dt), up-probability
    /// q = 1/2 (1 + mu sqrt(dt) / sigma) (clamped to [0.01, 0.99]), which matches
    /// the first two moments of the continuous process. Target barrier at +tp,
    /// stop at -sl (log-return units), each rounded to the nearest lattice level
    /// (>= 1). Mass absorbed at each barrier per step yields the probabilities and
    /// conditional hitting times; mass surviving to T/dt is P(neither).
    pub fn first_passage(
        mu: f64,
        sigma: f64,
        tp: f64,
        sl: f64,
        horizon_s: f64,
        dt: f64,
    ) -> FirstPassage {
        assert!(sigma > 0.0 && tp > 0.0 && sl > 0.0 && dt > 0.0 && horizon_s > 0.0);
        let h = sigma * dt.sqrt();
        let up = ((tp / h).round() as i64).clamp(1, 4000);
        let dn = ((sl / h).round() as i64).clamp(1, 4000);
        let q = (0.5 * (1.0 + mu * dt.sqrt() / sigma)).clamp(0.01, 0.99);
        let steps = (horizon_s / dt).round() as usize;
        // index i <-> level i - (dn - 1); interior levels -dn+1 ..= up-1
        let width = (up + dn - 1) as usize;
        let mut p = vec![0.0f64; width];
        p[(dn - 1) as usize] = 1.0;
        let (mut pt, mut ps, mut tt, mut ts) = (0.0, 0.0, 0.0, 0.0);
        let mut next = vec![0.0f64; width];
        for k in 1..=steps {
            next.iter_mut().for_each(|v| *v = 0.0);
            let t = k as f64 * dt;
            for i in 0..width {
                let m = p[i];
                if m == 0.0 {
                    continue;
                }
                if i + 1 == width {
                    pt += m * q;
                    tt += m * q * t;
                } else {
                    next[i + 1] += m * q;
                }
                if i == 0 {
                    ps += m * (1.0 - q);
                    ts += m * (1.0 - q) * t;
                } else {
                    next[i - 1] += m * (1.0 - q);
                }
            }
            std::mem::swap(&mut p, &mut next);
        }
        let pn: f64 = p.iter().sum();
        let e_neither = if pn > 0.0 {
            p.iter()
                .enumerate()
                .map(|(i, m)| m * (i as f64 - (dn - 1) as f64) * h)
                .sum::<f64>()
                / pn
        } else {
            0.0
        };
        FirstPassage {
            p_target: pt,
            p_stop: ps,
            p_neither: pn,
            e_t_target: if pt > 0.0 { tt / pt } else { f64::NAN },
            e_t_stop: if ps > 0.0 { ts / ps } else { f64::NAN },
            e_ret_neither: e_neither,
        }
    }

    /// Expected net value of a bracket trade in return units:
    /// EV = Pw * W - Pl * L + Pn * E[R | neither] - C
    /// where W, L are gross target/stop distances (fees NOT included) and C is the
    /// full round-trip cost (fees + spread + slippage), charged exactly once.
    pub fn bracket_ev(fp: &FirstPassage, w: f64, l: f64, cost: f64) -> f64 {
        fp.p_target * w - fp.p_stop * l + fp.p_neither * fp.e_ret_neither - cost
    }

    /// Brier score: mean (p_i - y_i)^2.
    pub fn brier(p: &[f64], y: &[bool]) -> Option<f64> {
        if p.is_empty() || p.len() != y.len() {
            return None;
        }
        Some(
            p.iter()
                .zip(y)
                .map(|(p, &y)| (p - if y { 1.0 } else { 0.0 }).powi(2))
                .sum::<f64>()
                / p.len() as f64,
        )
    }

    /// Log loss: -mean(y ln p + (1-y) ln(1-p)), p clamped to [1e-6, 1-1e-6].
    pub fn log_loss(p: &[f64], y: &[bool]) -> Option<f64> {
        if p.is_empty() || p.len() != y.len() {
            return None;
        }
        let s: f64 = p
            .iter()
            .zip(y)
            .map(|(p, &y)| {
                let p = p.clamp(1e-6, 1.0 - 1e-6);
                if y {
                    -p.ln()
                } else {
                    -(1.0 - p).ln()
                }
            })
            .sum();
        Some(s / p.len() as f64)
    }

    /// Expected calibration error with `bins` equal-width bins:
    /// ECE = sum_b (n_b / N) |mean(p in b) - freq(y in b)|.
    pub fn ece(p: &[f64], y: &[bool], bins: usize) -> Option<f64> {
        if p.is_empty() || p.len() != y.len() || bins == 0 {
            return None;
        }
        let mut sp = vec![0.0; bins];
        let mut sy = vec![0.0; bins];
        let mut n = vec![0usize; bins];
        for (&pi, &yi) in p.iter().zip(y) {
            let b = ((pi * bins as f64) as usize).min(bins - 1);
            sp[b] += pi;
            sy[b] += if yi { 1.0 } else { 0.0 };
            n[b] += 1;
        }
        let total = p.len() as f64;
        Some(
            (0..bins)
                .filter(|&b| n[b] > 0)
                .map(|b| (n[b] as f64 / total) * (sp[b] / n[b] as f64 - sy[b] / n[b] as f64).abs())
                .sum(),
        )
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn cdf_values() {
            assert!((norm_cdf(0.0) - 0.5).abs() < 1e-7);
            assert!((norm_cdf(1.96) - 0.975_002_1).abs() < 1e-6);
            assert!((norm_cdf(-1.0) - 0.158_655_3).abs() < 1e-6);
        }
        #[test]
        fn symmetric_driftless_passage() {
            let fp = first_passage(0.0, 0.001, 0.002, 0.002, 1000.0, 0.25);
            assert!((fp.p_target - fp.p_stop).abs() < 1e-9);
            assert!(fp.p_neither < 1e-6);
            assert!((fp.p_target + fp.p_stop + fp.p_neither - 1.0).abs() < 1e-9);
        }
        #[test]
        fn gamblers_ruin_ratio() {
            // driftless, unlimited time: P(up first) = b / (a + b)
            let fp = first_passage(0.0, 0.001, 0.001, 0.003, 5000.0, 0.25);
            assert!((fp.p_target - 0.75).abs() < 0.01, "{fp:?}");
        }
        #[test]
        fn positive_drift_favours_target_and_ev() {
            let a = first_passage(0.0, 0.0005, 0.002, 0.002, 120.0, 0.25);
            let b = first_passage(0.000_02, 0.0005, 0.002, 0.002, 120.0, 0.25);
            assert!(b.p_target > a.p_target);
            assert!(b.p_neither > 0.0 && a.p_neither > 0.0);
            assert!(bracket_ev(&b, 0.002, 0.002, 0.0) > bracket_ev(&a, 0.002, 0.002, 0.0));
            let d = bracket_ev(&b, 0.002, 0.002, 0.0) - bracket_ev(&b, 0.002, 0.002, 0.001);
            assert!((d - 0.001).abs() < 1e-15);
        }
        #[test]
        fn deterministic() {
            let a = first_passage(0.000_01, 0.0007, 0.0015, 0.001, 120.0, 0.25);
            let b = first_passage(0.000_01, 0.0007, 0.0015, 0.001, 120.0, 0.25);
            assert_eq!(a, b);
        }
        #[test]
        fn scoring_rules() {
            let p = [0.9, 0.1];
            let y = [true, false];
            assert!((brier(&p, &y).unwrap() - 0.01).abs() < 1e-12);
            assert!((log_loss(&p, &y).unwrap() + (0.9f64).ln()).abs() < 1e-12);
            assert!((ece(&[0.8, 0.8], &[true, false], 10).unwrap() - 0.3).abs() < 1e-12);
        }
    }
}

pub mod sizing {
    use serde::Serialize;

    /// Inputs that come from live instrument metadata and the risk policy.
    #[derive(Debug, Clone)]
    pub struct SizingInput {
        pub equity: f64,        // account currency (paper INR)
        pub risk_fraction: f64, // hard ceiling, e.g. 0.01
        pub price: f64,         // expected entry price (quote currency)
        pub stop_distance: f64, // |entry - stop| in quote currency per unit
        pub cost_per_unit: f64, // fees + spread + slippage per unit, quote currency
        pub fx_to_account: f64, // quote currency -> account currency
        pub qty_increment: f64,
        pub min_quantity: f64,
        pub max_quantity: f64,
        pub min_notional: f64,     // as the exchange reports it
        pub min_notional_fx: f64,  // min_notional currency -> quote currency factor
        pub leverage: f64,         // already selected, <= exchange max for side/tier
        pub available_margin: f64, // account currency
    }

    #[derive(Debug, Clone, Serialize, PartialEq)]
    pub struct Sized {
        pub qty: f64,
        pub notional_quote: f64,
        pub notional_account: f64,
        pub margin_account: f64,
        pub risk_account: f64,
    }

    /// Size a position from the stop-loss risk budget, independently of leverage.
    ///
    /// R = equity * risk_fraction (account currency)
    /// N_max = R / ((stop_distance + cost_per_unit) * fx)   (units)
    /// qty = floor(N_max / increment) * increment, clamped to max_quantity
    /// Reject if qty < min_quantity, notional < min_notional, or
    /// margin = notional / leverage > available margin. Minimum size is NEVER
    /// rounded up: if it would breach the risk budget the trade is rejected.
    pub fn size_position(i: &SizingInput) -> Result<Sized, String> {
        if i.equity <= 0.0 || i.price <= 0.0 || i.stop_distance <= 0.0 || i.leverage < 1.0 {
            return Err("invalid sizing input".into());
        }
        let r = i.equity * i.risk_fraction;
        let loss_per_unit = (i.stop_distance + i.cost_per_unit) * i.fx_to_account;
        let n_max = (r / loss_per_unit).min(i.max_quantity);
        let inc = if i.qty_increment > 0.0 {
            i.qty_increment
        } else {
            1e-9
        };
        let qty = (n_max / inc + 1e-9).floor() * inc;
        if qty < i.min_quantity || qty <= 0.0 {
            return Err(format!(
                "min quantity {} exceeds risk budget (max {:.6} units for risk {:.4})",
                i.min_quantity, n_max, r
            ));
        }
        let notional_quote = qty * i.price;
        let min_notional_quote = i.min_notional * i.min_notional_fx;
        if notional_quote < min_notional_quote {
            return Err(format!(
                "min notional {:.4} (quote) exceeds risk-budget notional {:.4}",
                min_notional_quote, notional_quote
            ));
        }
        let notional_account = notional_quote * i.fx_to_account;
        let margin_account = notional_account / i.leverage;
        if margin_account > i.available_margin {
            return Err(format!(
                "margin {:.4} exceeds available {:.4}",
                margin_account, i.available_margin
            ));
        }
        Ok(Sized {
            qty,
            notional_quote,
            notional_account,
            margin_account,
            risk_account: qty * loss_per_unit,
        })
    }

    /// Select leverage: CoinDCX `dynamic_position_leverage_details` maps
    /// leverage -> maximum position notional. Choose the highest leverage whose
    /// cap is >= the intended notional, also capped by the side maximum and the
    /// policy cap. Falls back to 1x if no tier admits the notional.
    pub fn select_leverage(
        tiers: &[(f64, f64)],
        notional: f64,
        side_max: f64,
        policy_cap: f64,
    ) -> f64 {
        let cap = side_max.min(policy_cap).max(1.0);
        if tiers.is_empty() {
            return cap;
        }
        tiers
            .iter()
            .filter(|(lev, max_notional)| *max_notional >= notional && *lev <= cap)
            .map(|(lev, _)| *lev)
            .fold(1.0, f64::max)
    }

    /// Kelly fraction for a binary bet paying b per unit risked with win prob p:
    /// f* = p - (1 - p) / b, floored at 0.
    pub fn kelly(p: f64, b: f64) -> f64 {
        if b <= 0.0 {
            return 0.0;
        }
        (p - (1.0 - p) / b).max(0.0)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        fn base() -> SizingInput {
            SizingInput {
                equity: 10_000.0,
                risk_fraction: 0.01,
                price: 100.0,
                stop_distance: 1.0,
                cost_per_unit: 0.0,
                fx_to_account: 1.0,
                qty_increment: 0.1,
                min_quantity: 0.1,
                max_quantity: 1e9,
                min_notional: 10.0,
                min_notional_fx: 1.0,
                leverage: 10.0,
                available_margin: 10_000.0,
            }
        }
        #[test]
        fn risk_budget_sizing() {
            let s = size_position(&base()).unwrap();
            assert!((s.qty - 100.0).abs() < 1e-9);
            assert!((s.margin_account - 1000.0).abs() < 1e-9);
            assert!(s.risk_account <= 100.0 + 1e-9);
        }
        #[test]
        fn leverage_does_not_change_size() {
            let mut a = base();
            a.leverage = 2.0;
            let mut b = base();
            b.leverage = 20.0;
            assert_eq!(
                size_position(&a).unwrap().qty,
                size_position(&b).unwrap().qty
            );
        }
        #[test]
        fn rejects_when_min_size_breaches_budget() {
            let mut i = base();
            i.equity = 100.0;
            i.min_quantity = 5.0;
            assert!(size_position(&i).is_err());
            let mut j = base();
            j.equity = 100.0;
            j.min_notional = 500.0;
            assert!(size_position(&j).is_err());
        }
        #[test]
        fn costs_shrink_size() {
            let mut i = base();
            i.cost_per_unit = 1.0;
            assert!((size_position(&i).unwrap().qty - 50.0).abs() < 1e-9);
        }
        #[test]
        fn tiers() {
            let t = [(5.0, 1_000_000.0), (10.0, 100_000.0), (20.0, 50_000.0)];
            assert_eq!(select_leverage(&t, 10_000.0, 20.0, 100.0), 20.0);
            assert_eq!(select_leverage(&t, 60_000.0, 20.0, 100.0), 10.0);
            assert_eq!(select_leverage(&t, 10_000.0, 15.0, 100.0), 10.0);
            assert_eq!(select_leverage(&t, 10_000.0, 20.0, 5.0), 5.0);
        }
        #[test]
        fn kelly_values() {
            assert!((kelly(0.6, 1.0) - 0.2).abs() < 1e-12);
            assert_eq!(kelly(0.4, 1.0), 0.0);
        }
    }
}

pub mod target {
    use serde::Serialize;

    /// Trades n such that C0 (1 + r)^n = Ct: n = ln(Ct/C0) / ln(1+r).
    pub fn trades_needed(c0: f64, ct: f64, r: f64) -> Option<f64> {
        if c0 <= 0.0 || ct <= 0.0 || r <= -1.0 || r == 0.0 {
            return None;
        }
        let n = (ct / c0).ln() / (1.0 + r).ln();
        if n.is_finite() && n >= 0.0 {
            Some(n)
        } else {
            None
        }
    }

    /// Required average return per trade over n trades: r = (Ct/C0)^(1/n) - 1.
    pub fn required_return(c0: f64, ct: f64, n: f64) -> Option<f64> {
        if c0 <= 0.0 || ct <= 0.0 || n <= 0.0 {
            return None;
        }
        Some((ct / c0).powf(1.0 / n) - 1.0)
    }

    /// Geometric mean per-trade growth: g = (E_n / E_0)^(1/n) - 1.
    pub fn geometric_growth(equities: &[f64]) -> Option<f64> {
        if equities.len() < 2 || equities[0] <= 0.0 {
            return None;
        }
        let n = (equities.len() - 1) as f64;
        Some((equities[equities.len() - 1] / equities[0]).powf(1.0 / n) - 1.0)
    }

    /// Explicitly seeded SplitMix64 PRNG (deterministic for a given seed).
    pub struct SplitMix64(pub u64);
    impl SplitMix64 {
        pub fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        pub fn next_f64(&mut self) -> f64 {
            (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    #[derive(Debug, Clone, Serialize)]
    pub struct SimResult {
        pub paths: usize,
        pub trades_per_path: usize,
        pub p_reach_target: f64,
        pub p_session_loss_first: f64,
        pub p_floor_breach: f64,
        pub p_losing_streak: f64,
        pub p_unrecovered_drawdown: f64,
        pub median_final_equity: f64,
    }

    /// Stochastic target simulation (not all-winning compounding): each trade wins
    /// with prob p_win returning +win_ret of equity, otherwise loses loss_ret.
    /// A path stops at the target, at the session-loss limit, at the capital
    /// floor, or after max_trades.
    #[allow(clippy::too_many_arguments)]
    pub fn simulate(
        c0: f64,
        target: f64,
        session_loss_frac: f64,
        floor: f64,
        p_win: f64,
        win_ret: f64,
        loss_ret: f64,
        max_trades: usize,
        streak_len: usize,
        paths: usize,
        seed: u64,
    ) -> SimResult {
        let mut rng = SplitMix64(seed);
        let (mut reach, mut sess, mut floor_b, mut streak_b, mut unrec) = (0, 0, 0, 0, 0);
        let mut finals = Vec::with_capacity(paths);
        let loss_limit = c0 * (1.0 - session_loss_frac);
        for _ in 0..paths {
            let mut e = c0;
            let mut hwm = c0;
            let mut streak = 0;
            let mut hit_streak = false;
            let mut outcome = 0; // 0 none, 1 target, 2 session loss / floor
            for _ in 0..max_trades {
                if rng.next_f64() < p_win {
                    e *= 1.0 + win_ret;
                    streak = 0;
                } else {
                    e *= 1.0 - loss_ret;
                    streak += 1;
                    if streak >= streak_len {
                        hit_streak = true;
                    }
                }
                hwm = hwm.max(e);
                if e < floor {
                    floor_b += 1;
                    outcome = 2;
                    break;
                }
                if e >= target {
                    outcome = 1;
                    break;
                }
                if e <= loss_limit {
                    outcome = 2;
                    break;
                }
            }
            match outcome {
                1 => reach += 1,
                2 => sess += 1,
                _ => {}
            }
            if hit_streak {
                streak_b += 1;
            }
            if outcome != 1 && e < hwm * 0.999 {
                unrec += 1;
            }
            finals.push(e);
        }
        finals.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let n = paths.max(1) as f64;
        SimResult {
            paths,
            trades_per_path: max_trades,
            p_reach_target: reach as f64 / n,
            p_session_loss_first: sess as f64 / n,
            p_floor_breach: floor_b as f64 / n,
            p_losing_streak: streak_b as f64 / n,
            p_unrecovered_drawdown: unrec as f64 / n,
            median_final_equity: finals.get(paths / 2).copied().unwrap_or(c0),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn compounding_identities() {
            let n = trades_needed(100.0, 18_000.0, 0.01).unwrap();
            assert!((100.0 * 1.01f64.powf(n) - 18_000.0).abs() < 1e-6);
            let r = required_return(100.0, 18_000.0, n).unwrap();
            assert!((r - 0.01).abs() < 1e-12);
            assert!((geometric_growth(&[100.0, 110.0, 121.0]).unwrap() - 0.1).abs() < 1e-12);
            assert_eq!(trades_needed(100.0, 200.0, 0.0), None);
        }
        #[test]
        fn simulation_is_seeded_and_sane() {
            let a = simulate(100.0, 200.0, 0.05, 50.0, 0.5, 0.01, 0.01, 500, 5, 2000, 42);
            let b = simulate(100.0, 200.0, 0.05, 50.0, 0.5, 0.01, 0.01, 500, 5, 2000, 42);
            assert_eq!(a.p_reach_target, b.p_reach_target);
            assert!(a.p_session_loss_first > a.p_reach_target);
            let sure = simulate(100.0, 110.0, 0.05, 50.0, 1.0, 0.01, 0.01, 100, 5, 100, 1);
            assert_eq!(sure.p_reach_target, 1.0);
        }
    }
}
