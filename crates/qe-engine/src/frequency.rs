//! Trade-frequency calculator: r = (Ct/C0)^(1/n) - 1 and Cn = C0 * prod(1 + r_i).
//!
//! These are mathematical requirements, not forecasts. A trade count is a reporting
//! metric, never a quota: nothing here can cause an entry.

use serde::Serialize;

pub const RATES_PER_HOUR: [u32; 5] = [5, 10, 15, 20, 30];

/// Required return per trade over n trades.
pub fn required_return(c0: f64, ct: f64, n: u32) -> Option<f64> {
    (c0 > 0.0 && ct > 0.0 && n > 0).then(|| (ct / c0).powf(1.0 / n as f64) - 1.0)
}

/// Cn = C0 * prod(1 + r_i).
pub fn compound(c0: f64, returns: &[f64]) -> f64 {
    returns.iter().fold(c0, |c, r| c * (1.0 + r))
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FrequencyRow {
    pub trades_per_hour: u32,
    pub n_trades: u32,
    pub required_return_per_trade: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Requirement {
    pub equity: f64,
    pub target: f64,
    pub hours: f64,
    pub multiple: f64,
    pub ideal_hourly_return: Option<f64>,
    pub rows: Vec<FrequencyRow>,
    pub note: &'static str,
}

impl Requirement {
    pub fn new(equity: f64, target: f64, hours: f64) -> Self {
        let rows = RATES_PER_HOUR
            .iter()
            .filter_map(|&k| {
                let n = (k as f64 * hours).ceil() as u32;
                Some(FrequencyRow {
                    trades_per_hour: k,
                    n_trades: n,
                    required_return_per_trade: required_return(equity, target, n)?,
                })
            })
            .collect();
        Self {
            equity,
            target,
            hours,
            multiple: if equity > 0.0 { target / equity } else { 0.0 },
            ideal_hourly_return: (equity > 0.0 && target > 0.0 && hours > 0.0)
                .then(|| (target / equity).powf(1.0 / hours) - 1.0),
            rows,
            note: "target-implied mathematical requirements, not forecasts; a trade count is never a quota",
        }
    }
}

/// Seeded SplitMix64 (deterministic for a seed).
#[derive(Debug, Clone)]
pub struct Rng(pub u64);
impl Rng {
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    pub fn f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    /// Uniform in [m*(1-s), m*(1+s)]: variable magnitude around a mean.
    pub fn around(&mut self, m: f64, s: f64) -> f64 {
        m * (1.0 + s * (2.0 * self.f64() - 1.0))
    }
}

/// Stochastic simulation inputs. Returns are fractions of equity per trade.
#[derive(Debug, Clone, Serialize)]
pub struct SimParams {
    pub c0: f64,
    pub target: f64,
    pub hours: f64,
    /// Opportunity windows per hour (e.g. 30 two-minute windows).
    pub windows_per_hour: u32,
    pub p_win: f64,
    pub win_ret: f64,
    pub loss_ret: f64,
    /// Relative spread of magnitudes (0.5 = +/-50 %).
    pub magnitude_spread: f64,
    /// Round-trip fees + spread + slippage as a fraction of equity.
    pub cost_ret: f64,
    /// Funding per trade as a fraction of equity (about 0 for 120 s holds).
    pub funding_ret: f64,
    /// Probability a window has no qualified opportunity (or the governor rejects it).
    pub p_no_opportunity: f64,
    /// Probability an order is rejected by the exchange.
    pub p_reject: f64,
    /// Probability of a partial fill, and the filled fraction when it happens.
    pub p_partial: f64,
    pub partial_fill_frac: f64,
    /// Probability a window is lost to downtime or stale data.
    pub p_downtime: f64,
    /// Session loss stop as a fraction of stage start.
    pub session_loss_frac: f64,
    pub paths: u32,
    pub seed: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SimSummary {
    pub params: SimParams,
    pub windows: u32,
    pub p_reach_target: f64,
    pub p_session_stop: f64,
    pub median_final: f64,
    pub p05_final: f64,
    pub p95_final: f64,
    pub mean_trades: f64,
    pub label: &'static str,
}

pub fn simulate(p: &SimParams) -> SimSummary {
    let mut rng = Rng(p.seed);
    let windows = (p.windows_per_hour as f64 * p.hours).ceil().max(0.0) as u32;
    let floor = p.c0 * (1.0 - p.session_loss_frac);
    let (mut reach, mut stop, mut trades) = (0u32, 0u32, 0u64);
    let mut finals = Vec::with_capacity(p.paths as usize);
    for _ in 0..p.paths {
        let mut e = p.c0;
        for _ in 0..windows {
            if rng.f64() < p.p_downtime || rng.f64() < p.p_no_opportunity {
                continue;
            }
            if rng.f64() < p.p_reject {
                continue;
            }
            let fill = if rng.f64() < p.p_partial {
                p.partial_fill_frac
            } else {
                1.0
            };
            let gross = if rng.f64() < p.p_win {
                rng.around(p.win_ret, p.magnitude_spread)
            } else {
                -rng.around(p.loss_ret, p.magnitude_spread)
            };
            let r = fill * (gross - p.cost_ret - p.funding_ret);
            e *= 1.0 + r.max(-1.0);
            trades += 1;
            if e >= p.target {
                reach += 1;
                break;
            }
            if e <= floor {
                stop += 1;
                break;
            }
        }
        finals.push(e);
    }
    finals.sort_by(|a, b| a.total_cmp(b));
    let q = |f: f64| {
        finals
            .get(((finals.len() as f64 - 1.0) * f).round() as usize)
            .copied()
            .unwrap_or(p.c0)
    };
    let n = p.paths.max(1) as f64;
    SimSummary {
        params: p.clone(),
        windows,
        p_reach_target: reach as f64 / n,
        p_session_stop: stop as f64 / n,
        median_final: q(0.5),
        p05_final: q(0.05),
        p95_final: q(0.95),
        mean_trades: trades as f64 / n,
        label: "HYPOTHETICAL simulation from assumed inputs; not a forecast of actual results",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pct(x: f64) -> f64 {
        (x * 1e6).round() / 1e4
    }

    fn table(r: &Requirement) -> Vec<(u32, u32, f64)> {
        r.rows
            .iter()
            .map(|x| {
                (
                    x.trades_per_hour,
                    x.n_trades,
                    pct(x.required_return_per_trade),
                )
            })
            .collect()
    }

    #[test]
    fn stage1_reference_table() {
        let r = Requirement::new(100.0, 3000.0, 2.5);
        assert_eq!(
            table(&r),
            vec![
                (5, 13, 29.9047),
                (10, 25, 14.5737),
                (15, 38, 9.3633),
                (20, 50, 7.0391),
                (30, 75, 4.6393)
            ]
        );
        assert_eq!(pct(r.ideal_hourly_return.unwrap()), 289.806);
    }

    #[test]
    fn stage2_reference_table() {
        let r = Requirement::new(1000.0, 50000.0, 18.5);
        assert_eq!(
            table(&r),
            vec![
                (5, 93, 4.2962),
                (10, 185, 2.1371),
                (15, 278, 1.4172),
                (20, 370, 1.0629),
                (30, 555, 0.7074)
            ]
        );
        assert_eq!(pct(r.ideal_hourly_return.unwrap()), 23.5481);
    }

    #[test]
    fn compounding_identity() {
        let r = required_return(100.0, 3000.0, 75).unwrap();
        assert!((compound(100.0, &[r; 75]) - 3000.0).abs() < 1e-6);
    }

    fn base() -> SimParams {
        SimParams {
            c0: 100.0,
            target: 3000.0,
            hours: 2.5,
            windows_per_hour: 30,
            p_win: 0.5,
            win_ret: 0.01,
            loss_ret: 0.01,
            magnitude_spread: 0.5,
            cost_ret: 0.001,
            funding_ret: 0.0,
            p_no_opportunity: 0.5,
            p_reject: 0.02,
            p_partial: 0.1,
            partial_fill_frac: 0.5,
            p_downtime: 0.02,
            session_loss_frac: 0.05,
            paths: 2000,
            seed: 7,
        }
    }

    #[test]
    fn simulation_is_seeded_and_costs_hurt() {
        let a = simulate(&base());
        let b = simulate(&base());
        assert_eq!(a.median_final, b.median_final);
        assert_eq!(a.windows, 75);
        assert_eq!(a.p_reach_target, 0.0);
        let mut free = base();
        free.cost_ret = 0.0;
        assert!(simulate(&free).median_final >= a.median_final);
        let mut sure = base();
        sure.p_win = 1.0;
        sure.win_ret = 0.05;
        sure.magnitude_spread = 0.0;
        sure.cost_ret = 0.0;
        sure.p_no_opportunity = 0.0;
        sure.p_reject = 0.0;
        sure.p_partial = 0.0;
        sure.p_downtime = 0.0;
        assert_eq!(simulate(&sure).p_reach_target, 1.0);
    }
}
