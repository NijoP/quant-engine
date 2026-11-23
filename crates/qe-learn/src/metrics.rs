//! §6.4 Per-horizon prediction-vs-outcome metrics, strictly chronological.
//!
//! Only outcomes with `recorded_ms <= as_of_ms` are used, so a report "as of"
//! time t never contains information unavailable at t. Forecast quality is
//! scored independently of P&L: every horizon with an observed move counts,
//! winners and losers alike.
use crate::outcome::{ExitReason, OutcomeRecord};
use crate::snapshot::DecisionSnapshot;
use crate::types::{Mode, HORIZONS_S};
use qe_math::prob::{brier, ece, log_loss};
use serde::{Deserialize, Serialize};

pub const ECE_BINS: usize = 10;

/// Which records a report covers. Paper and live are separated by default
/// only if the caller sets `mode`; dashboards should always set it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Filter {
    pub mode: Option<Mode>,
    pub model_version: Option<String>,
    pub instrument: Option<String>,
}

impl Filter {
    pub fn matches(&self, s: &DecisionSnapshot) -> bool {
        self.mode.is_none_or(|m| m == s.mode)
            && self
                .model_version
                .as_ref()
                .is_none_or(|v| v == &s.model_version)
            && self.instrument.as_ref().is_none_or(|i| i == &s.instrument)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BinaryScore {
    pub n: usize,
    pub brier: Option<f64>,
    pub log_loss: Option<f64>,
    pub calibration_error: Option<f64>,
    pub mean_p: Option<f64>,
    pub hit_rate: Option<f64>,
}

impl BinaryScore {
    pub fn from(p: &[f64], y: &[bool]) -> Self {
        let n = p.len();
        let mean = |v: &[f64]| (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64);
        let yf: Vec<f64> = y.iter().map(|&b| f64::from(u8::from(b))).collect();
        BinaryScore {
            n,
            brier: brier(p, y),
            log_loss: log_loss(p, y),
            calibration_error: ece(p, y, ECE_BINS),
            mean_p: mean(p),
            hit_rate: mean(&yf),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HorizonMetrics {
    pub horizon_s: u32,
    pub n: usize,
    /// mean(actual - predicted)
    pub mean_error: Option<f64>,
    pub mean_abs_error: Option<f64>,
    pub mean_sq_error: Option<f64>,
    /// share of non-zero moves whose sign matched the predicted sign
    pub directional_accuracy: Option<f64>,
    /// share of moves inside the forecast CI
    pub ci_coverage: Option<f64>,
    /// P(up) scored against actual > 0
    pub p_up: BinaryScore,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PredVsActual {
    pub n: usize,
    pub predicted_mean: Option<f64>,
    pub actual_mean: Option<f64>,
    /// mean(actual - predicted)
    pub mean_error: Option<f64>,
}

impl PredVsActual {
    fn from(pairs: &[(f64, f64)]) -> Self {
        let n = pairs.len();
        let m = |f: &dyn Fn(&(f64, f64)) -> f64| {
            (n > 0).then(|| pairs.iter().map(f).sum::<f64>() / n as f64)
        };
        PredVsActual {
            n,
            predicted_mean: m(&|p| p.0),
            actual_mean: m(&|p| p.1),
            mean_error: m(&|p| p.1 - p.0),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricsReport {
    pub as_of_ms: i64,
    pub filter: Filter,
    pub n_trades: usize,
    pub horizons: Vec<HorizonMetrics>,
    /// p_tp_first vs exit == take_profit
    pub tp_first: BinaryScore,
    /// p_sl_first vs exit == stop_loss
    pub sl_first: BinaryScore,
    /// win_probability vs net P&L > 0
    pub win: BinaryScore,
    pub mfe: PredVsActual,
    pub mae: PredVsActual,
    /// estimated vs actual round-trip cost, bps
    pub cost_bps: PredVsActual,
}

fn mean(v: &[f64]) -> Option<f64> {
    (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64)
}

/// `rows`: (decision, outcome) pairs in any order; filtered by `as_of_ms` and `filter`.
pub fn compute<'a>(
    rows: impl IntoIterator<Item = (&'a DecisionSnapshot, &'a OutcomeRecord)>,
    as_of_ms: i64,
    filter: &Filter,
) -> MetricsReport {
    let rows: Vec<_> = rows
        .into_iter()
        .filter(|(s, o)| o.outcome.recorded_ms <= as_of_ms && filter.matches(s))
        .collect();
    let horizons = HORIZONS_S
        .iter()
        .map(|&h| {
            let (mut err, mut p, mut y) = (Vec::new(), Vec::new(), Vec::new());
            let (mut dir_hit, mut dir_n, mut cover) = (0usize, 0usize, 0usize);
            for (s, o) in &rows {
                let (Some(fc), Some(a)) = (s.forecast_at(h), o.outcome.move_at(h)) else {
                    continue;
                };
                err.push(a - fc.expected_return);
                p.push(fc.p_up);
                y.push(a > 0.0);
                if a != 0.0 && fc.expected_return != 0.0 {
                    dir_n += 1;
                    if (a > 0.0) == (fc.expected_return > 0.0) {
                        dir_hit += 1;
                    }
                }
                if a >= fc.ci_low && a <= fc.ci_high {
                    cover += 1;
                }
            }
            let n = err.len();
            HorizonMetrics {
                horizon_s: h,
                n,
                mean_error: mean(&err),
                mean_abs_error: mean(&err.iter().map(|e| e.abs()).collect::<Vec<_>>()),
                mean_sq_error: mean(&err.iter().map(|e| e * e).collect::<Vec<_>>()),
                directional_accuracy: (dir_n > 0).then(|| dir_hit as f64 / dir_n as f64),
                ci_coverage: (n > 0).then(|| cover as f64 / n as f64),
                p_up: BinaryScore::from(&p, &y),
            }
        })
        .collect();
    let bin = |pf: &dyn Fn(&DecisionSnapshot) -> f64, yf: &dyn Fn(&OutcomeRecord) -> bool| {
        let p: Vec<f64> = rows.iter().map(|(s, _)| pf(s)).collect();
        let y: Vec<bool> = rows.iter().map(|(_, o)| yf(o)).collect();
        BinaryScore::from(&p, &y)
    };
    let pairs = |f: &dyn Fn(&DecisionSnapshot, &OutcomeRecord) -> (f64, f64)| {
        PredVsActual::from(&rows.iter().map(|(s, o)| f(s, o)).collect::<Vec<_>>())
    };
    MetricsReport {
        as_of_ms,
        filter: filter.clone(),
        n_trades: rows.len(),
        horizons,
        tp_first: bin(&|s| s.p_tp_first, &|o| {
            o.outcome.exit_reason == ExitReason::TakeProfit
        }),
        sl_first: bin(&|s| s.p_sl_first, &|o| {
            o.outcome.exit_reason == ExitReason::StopLoss
        }),
        win: bin(&|s| s.win_probability, &|o| o.won),
        mfe: pairs(&|s, o| (s.expected_mfe, o.outcome.mfe)),
        mae: pairs(&|s, o| (s.expected_mae, o.outcome.mae)),
        cost_bps: pairs(&|_, o| (o.est_cost_bps, o.actual_cost_bps)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outcome::{derive, tests::outcome};
    use crate::snapshot::tests::snap;

    #[test]
    fn per_horizon_errors_and_scores() {
        let s1 = snap("a", 1_000);
        let s2 = snap("b", 200_000);
        let r1 = derive(
            &s1,
            outcome("a", 1_000, "100.6", ExitReason::TakeProfit, 0.002),
        )
        .unwrap();
        let r2 = derive(
            &s2,
            outcome("b", 200_000, "99.5", ExitReason::StopLoss, -0.002),
        )
        .unwrap();
        let m = compute([(&s1, &r1), (&s2, &r2)], i64::MAX, &Filter::default());
        assert_eq!(m.n_trades, 2);
        let h120 = m.horizons.iter().find(|h| h.horizon_s == 120).unwrap();
        assert_eq!(h120.n, 2);
        // predicted +0.0004 both; errors +0.0016 and -0.0024
        assert!((h120.mean_error.unwrap() - (-0.0004)).abs() < 1e-12);
        assert!((h120.mean_abs_error.unwrap() - 0.002).abs() < 1e-12);
        assert_eq!(h120.directional_accuracy, Some(0.5));
        // Brier of p_up 0.58 vs {1, 0}: (0.42^2 + 0.58^2) / 2
        let b = (0.42f64.powi(2) + 0.58f64.powi(2)) / 2.0;
        assert!((h120.p_up.brier.unwrap() - b).abs() < 1e-12);
        assert_eq!(m.tp_first.n, 2);
        assert_eq!(m.tp_first.hit_rate, Some(0.5));
        assert!(m.cost_bps.mean_error.is_some());
    }

    #[test]
    fn as_of_excludes_future_outcomes() {
        let s1 = snap("a", 1_000);
        let r1 = derive(
            &s1,
            outcome("a", 1_000, "100.6", ExitReason::TakeProfit, 0.002),
        )
        .unwrap();
        let m = compute([(&s1, &r1)], r1.outcome.recorded_ms - 1, &Filter::default());
        assert_eq!(m.n_trades, 0);
        assert!(m
            .horizons
            .iter()
            .all(|h| h.n == 0 && h.mean_error.is_none()));
    }

    #[test]
    fn filter_separates_paper_and_live() {
        let s1 = snap("a", 1_000);
        let r1 = derive(
            &s1,
            outcome("a", 1_000, "100.6", ExitReason::TakeProfit, 0.002),
        )
        .unwrap();
        let live = Filter {
            mode: Some(Mode::Live),
            ..Default::default()
        };
        assert_eq!(compute([(&s1, &r1)], i64::MAX, &live).n_trades, 0);
    }
}
