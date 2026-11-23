//! §6.3 Mistake classifier: explicit numeric rules over the 18 causes.
//!
//! Runs on every losing trade and on every *poorly executed* trade (winner or
//! loser). A loss is not evidence that the model was wrong: when the realised
//! move sits inside the forecast distribution and no rule fires, the verdict
//! is [`Verdict::Uncertain`]. Forecast-quality causes are never assigned to a
//! winning trade (a win does not prove the model right either; forecast quality
//! is scored separately in [`crate::metrics`]).
//!
//! Each finding carries a [`Certainty`]: `known` = directly measured (fills,
//! flags, costs); `probable` = inferred from forecast vs outcome.
use crate::outcome::{ExitReason, OutcomeRecord};
use crate::snapshot::DecisionSnapshot;
use crate::types::{to_f64, LIFETIME_MS};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cause {
    IncorrectDirection,
    IncorrectMagnitude,
    IncorrectTiming,
    VolatilityEstimate,
    ProbabilityCalibration,
    RegimeMisclassification,
    OrderBookSnapshotLimitation,
    StaleData,
    SpreadExpansion,
    Slippage,
    PartialFill,
    StopTooTight,
    TargetTooAmbitious,
    ModelDisagreement,
    EventOutsideInformation,
    StrategyExecutionError,
    RiskSizingError,
    ExchangeApiError,
}

impl Cause {
    pub const ALL: [Cause; 18] = [
        Cause::IncorrectDirection,
        Cause::IncorrectMagnitude,
        Cause::IncorrectTiming,
        Cause::VolatilityEstimate,
        Cause::ProbabilityCalibration,
        Cause::RegimeMisclassification,
        Cause::OrderBookSnapshotLimitation,
        Cause::StaleData,
        Cause::SpreadExpansion,
        Cause::Slippage,
        Cause::PartialFill,
        Cause::StopTooTight,
        Cause::TargetTooAmbitious,
        Cause::ModelDisagreement,
        Cause::EventOutsideInformation,
        Cause::StrategyExecutionError,
        Cause::RiskSizingError,
        Cause::ExchangeApiError,
    ];
    /// execution / data causes are scored on winners too
    pub fn is_execution(self) -> bool {
        matches!(
            self,
            Cause::StaleData
                | Cause::SpreadExpansion
                | Cause::Slippage
                | Cause::PartialFill
                | Cause::StrategyExecutionError
                | Cause::ExchangeApiError
                | Cause::OrderBookSnapshotLimitation
                | Cause::RiskSizingError
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Certainty {
    Known,
    Probable,
    Uncertain,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub cause: Cause,
    pub certainty: Certainty,
    /// the rule that fired, with the measured numbers
    pub evidence: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// a winner with acceptable execution: nothing to classify
    NoMistake,
    /// at least one rule fired
    Classified,
    /// a loss / poor execution with no rule firing, or not enough observations
    Uncertain,
}

/// Calibration evidence available *before* this trade's outcome (supplied by
/// the engine from the online calibrator, so no look-ahead).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CalibrationContext {
    /// empirical TP-first rate in the bin of this trade's p_tp_first
    pub bin_rate: f64,
    pub bin_n_eff: f64,
}

/// Numeric thresholds. Defaults are documented in docs/LEARNING_ENGINE.md.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClassifierRules {
    /// wrong sign and |actual| > this many predicted sd => incorrect direction
    pub direction_noise_sd: f64,
    /// right sign but |actual - predicted| / sd above this => incorrect magnitude
    pub magnitude_z: f64,
    /// max(MFE, MAE) / predicted sd outside [lo, hi] => volatility estimate
    pub vol_ratio_lo: f64,
    pub vol_ratio_hi: f64,
    /// |p_tp_first - empirical bin rate| above this, with >= calib_min_n => calibration
    pub calib_gap: f64,
    pub calib_min_n: f64,
    /// book or trade age above this at decision => stale data
    pub stale_ms: i64,
    /// snapshot interval above this and slippage over estimate => book snapshot limitation
    pub book_interval_ms: i64,
    /// spread at entry/exit > mult * estimate and > estimate + abs => spread expansion
    pub spread_mult: f64,
    pub spread_abs_bps: f64,
    /// slippage > mult * estimate and > estimate + abs => slippage
    pub slip_mult: f64,
    pub slip_abs_bps: f64,
    /// stopped out and stop distance < this many predicted sd => stop too tight
    pub stop_tight_sd: f64,
    /// no TP and target distance > this many sd => target too ambitious
    pub target_far_sd: f64,
    /// no TP but MFE reached this fraction of the target distance => target too ambitious
    pub target_near_frac: f64,
    /// ensemble disagreement above this many sd => model disagreement
    pub disagreement_sd: f64,
    /// this many horizons (>= 30 s) forecasting against the direction => disagreement
    pub disagreement_horizons: usize,
    /// |z| of the realised move above this => event outside available information
    pub event_z: f64,
    /// holding beyond lifetime + grace => strategy execution error
    pub deadline_grace_ms: i64,
    /// realised loss > budget * this => risk sizing error
    pub sizing_tolerance: f64,
    /// actual cost > mult * estimated and > estimate + abs => poorly executed
    pub poor_cost_mult: f64,
    pub poor_cost_abs_bps: f64,
}

impl Default for ClassifierRules {
    fn default() -> Self {
        ClassifierRules {
            direction_noise_sd: 0.25,
            magnitude_z: 2.0,
            vol_ratio_lo: 0.3,
            vol_ratio_hi: 3.0,
            calib_gap: 0.15,
            calib_min_n: 30.0,
            stale_ms: 3_000,
            book_interval_ms: 1_000,
            spread_mult: 2.0,
            spread_abs_bps: 2.0,
            slip_mult: 2.0,
            slip_abs_bps: 2.0,
            stop_tight_sd: 0.75,
            target_far_sd: 2.0,
            target_near_frac: 0.6,
            disagreement_sd: 1.0,
            disagreement_horizons: 2,
            event_z: 5.0,
            deadline_grace_ms: 5_000,
            sizing_tolerance: 1.10,
            poor_cost_mult: 1.5,
            poor_cost_abs_bps: 2.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MistakeReport {
    pub trade_id: String,
    pub won: bool,
    pub poorly_executed: bool,
    pub verdict: Verdict,
    pub findings: Vec<Finding>,
    /// realised trade-horizon move inside the forecast CI (None if unobserved)
    pub within_forecast_ci: Option<bool>,
    /// (actual - predicted) / predicted sd at the trade horizon
    pub forecast_z: Option<f64>,
    pub note: String,
}

impl MistakeReport {
    pub fn primary(&self) -> Option<&Finding> {
        self.findings.first()
    }
}

fn over(actual: f64, est: f64, mult: f64, abs: f64) -> bool {
    actual > est * mult && actual > est + abs
}

pub fn classify(
    snap: &DecisionSnapshot,
    rec: &OutcomeRecord,
    cal: Option<CalibrationContext>,
    r: &ClassifierRules,
) -> MistakeReport {
    let o = &rec.outcome;
    let dir = snap.direction.sign();
    let est_slip = snap.est_slippage_bps;
    let actual_slip = rec.entry_slippage_bps + rec.exit_slippage_bps - snap.est_spread_bps; // slippage beyond the touch
    let poorly_executed = over(
        rec.actual_cost_bps,
        rec.est_cost_bps,
        r.poor_cost_mult,
        r.poor_cost_abs_bps,
    ) || rec.partial_fill
        || o.flags.exchange_error.is_some()
        || o.flags.execution_error.is_some()
        || o.flags.stale_data
        || rec.holding_ms > LIFETIME_MS + r.deadline_grace_ms;
    let mut f: Vec<Finding> = Vec::new();
    let mut push = |cause, certainty, evidence: String| {
        f.push(Finding {
            cause,
            certainty,
            evidence,
        })
    };

    // ---- execution / data causes (scored on winners and losers) ----
    if let Some(e) = &o.flags.exchange_error {
        push(
            Cause::ExchangeApiError,
            Certainty::Known,
            format!("exchange error: {e}"),
        );
    } else if matches!(o.exit_reason, ExitReason::ExchangeReject) {
        push(
            Cause::ExchangeApiError,
            Certainty::Known,
            "exit reason exchange_reject".into(),
        );
    }
    let late = rec.holding_ms - LIFETIME_MS;
    if let Some(e) = &o.flags.execution_error {
        push(
            Cause::StrategyExecutionError,
            Certainty::Known,
            format!("execution error: {e}"),
        );
    } else if late > r.deadline_grace_ms {
        push(
            Cause::StrategyExecutionError,
            Certainty::Known,
            format!(
                "held {} ms, {} ms past the 120 s deadline",
                rec.holding_ms, late
            ),
        );
    } else if matches!(o.exit_reason, ExitReason::TakeProfit) && rec.gross_pnl.is_sign_negative() {
        push(
            Cause::StrategyExecutionError,
            Certainty::Known,
            "take-profit exit with negative gross P&L".into(),
        );
    } else if matches!(o.exit_reason, ExitReason::Error) {
        push(
            Cause::StrategyExecutionError,
            Certainty::Known,
            "exit reason error".into(),
        );
    }
    let dq = &snap.data_quality;
    if o.flags.stale_data
        || dq.book_age_ms > r.stale_ms
        || dq.trade_age_ms > r.stale_ms
        || !dq.book_valid
    {
        push(
            Cause::StaleData,
            Certainty::Known,
            format!(
                "book age {} ms, trade age {} ms (limit {}), book_valid={}, stale during trade={}",
                dq.book_age_ms, dq.trade_age_ms, r.stale_ms, dq.book_valid, o.flags.stale_data
            ),
        );
    }
    let slip_over = over(actual_slip, est_slip, r.slip_mult, r.slip_abs_bps);
    if slip_over {
        push(
            Cause::Slippage,
            Certainty::Known,
            format!("slippage beyond touch {actual_slip:.2} bps vs est {est_slip:.2} bps"),
        );
        if dq.book_snapshot_interval_ms > r.book_interval_ms {
            push(
                Cause::OrderBookSnapshotLimitation,
                Certainty::Probable,
                format!(
                    "book snapshots {} ms apart (> {} ms): liquidity between snapshots unseen",
                    dq.book_snapshot_interval_ms, r.book_interval_ms
                ),
            );
        }
    }
    for (side, s) in [
        ("entry", o.flags.spread_entry_bps),
        ("exit", o.flags.spread_exit_bps),
    ] {
        if let Some(s) = s {
            if over(s, snap.est_spread_bps, r.spread_mult, r.spread_abs_bps) {
                push(
                    Cause::SpreadExpansion,
                    Certainty::Known,
                    format!(
                        "{side} spread {s:.2} bps vs est {:.2} bps",
                        snap.est_spread_bps
                    ),
                );
                break;
            }
        }
    }
    if rec.partial_fill {
        push(
            Cause::PartialFill,
            Certainty::Known,
            format!("entry filled {} of {}", rec.entry_qty, snap.qty),
        );
    }
    let loss = -to_f64(rec.net_pnl);
    let budget = to_f64(snap.risk.risk_budget);
    if loss > budget * r.sizing_tolerance {
        push(
            Cause::RiskSizingError,
            Certainty::Known,
            format!(
                "loss {loss:.4} exceeded risk budget {budget:.4} x {}",
                r.sizing_tolerance
            ),
        );
    }

    // ---- forecast causes (losers only) ----
    let fc = snap.trade_forecast();
    let actual = fc.and_then(|fc| o.move_at(fc.horizon_s));
    let (mut within, mut z) = (None, None);
    if let (Some(fc), Some(a)) = (fc, actual) {
        within = Some(a >= fc.ci_low && a <= fc.ci_high);
        if fc.return_sd > 0.0 {
            z = Some((a - fc.expected_return) / fc.return_sd);
        }
    }
    if !rec.won {
        if let (Some(fc), Some(a)) = (fc, actual) {
            let sd = fc.return_sd.max(1e-12);
            let zz = (a - fc.expected_return) / sd;
            let right_sign = a * dir > 0.0;
            if a * dir < 0.0 && a.abs() > r.direction_noise_sd * sd {
                push(
                    Cause::IncorrectDirection,
                    Certainty::Probable,
                    format!(
                        "{}s move {a:.5} against direction (> {} sd = {:.5})",
                        fc.horizon_s,
                        r.direction_noise_sd,
                        r.direction_noise_sd * sd
                    ),
                );
            } else if right_sign && zz.abs() > r.magnitude_z {
                push(
                    Cause::IncorrectMagnitude,
                    Certainty::Probable,
                    format!(
                        "{}s move {a:.5} vs predicted {:.5}: z={zz:.2}",
                        fc.horizon_s, fc.expected_return
                    ),
                );
            }
            if zz.abs() > r.event_z || o.flags.external_event {
                push(
                    Cause::EventOutsideInformation,
                    if o.flags.external_event {
                        Certainty::Known
                    } else {
                        Certainty::Uncertain
                    },
                    format!(
                        "z={zz:.2} (limit {}), tagged event={}",
                        r.event_z, o.flags.external_event
                    ),
                );
            }
            let excursion = o.mfe.max(o.mae);
            let ratio = excursion / sd;
            if ratio > r.vol_ratio_hi || ratio < r.vol_ratio_lo {
                push(
                    Cause::VolatilityEstimate,
                    Certainty::Probable,
                    format!(
                        "max excursion {excursion:.5} = {ratio:.2} predicted sd (band {}..{})",
                        r.vol_ratio_lo, r.vol_ratio_hi
                    ),
                );
            }
            if matches!(o.exit_reason, ExitReason::StopLoss) {
                if right_sign {
                    push(
                        Cause::IncorrectTiming,
                        Certainty::Probable,
                        format!(
                            "stopped out, but {}s move {a:.5} ended in the trade direction",
                            fc.horizon_s
                        ),
                    );
                }
                let stop = snap.stop_distance_frac();
                if stop < r.stop_tight_sd * sd {
                    push(
                        Cause::StopTooTight,
                        Certainty::Probable,
                        format!("stop distance {stop:.5} < {} x sd {sd:.5}", r.stop_tight_sd),
                    );
                }
            }
            if !matches!(o.exit_reason, ExitReason::TakeProfit) {
                let tgt = snap.target_distance_frac();
                if tgt > r.target_far_sd * sd {
                    push(
                        Cause::TargetTooAmbitious,
                        Certainty::Probable,
                        format!(
                            "target distance {tgt:.5} > {} x sd {sd:.5}",
                            r.target_far_sd
                        ),
                    );
                } else if o.mfe >= r.target_near_frac * tgt && o.mfe < tgt {
                    push(
                        Cause::TargetTooAmbitious,
                        Certainty::Probable,
                        format!(
                            "MFE {:.5} reached {:.0}% of target {tgt:.5} but not TP",
                            o.mfe,
                            100.0 * o.mfe / tgt
                        ),
                    );
                }
            }
        }
        if let Some(c) = cal {
            if c.bin_n_eff >= r.calib_min_n && (snap.p_tp_first - c.bin_rate).abs() > r.calib_gap {
                push(
                    Cause::ProbabilityCalibration,
                    Certainty::Probable,
                    format!(
                        "p_tp_first {:.3} vs empirical {:.3} over n_eff {:.1}",
                        snap.p_tp_first, c.bin_rate, c.bin_n_eff
                    ),
                );
            }
        }
        if let Some(after) = &o.flags.regime_after {
            if after != &snap.regime {
                push(
                    Cause::RegimeMisclassification,
                    Certainty::Probable,
                    format!("regime at decision '{}' vs observed '{after}'", snap.regime),
                );
            }
        }
        let against = snap
            .forecasts
            .iter()
            .filter(|h| h.horizon_s >= 30 && h.expected_return * dir < 0.0)
            .count();
        let dis = snap.model_disagreement_sd.unwrap_or(0.0);
        if dis > r.disagreement_sd || against >= r.disagreement_horizons {
            push(
                Cause::ModelDisagreement,
                Certainty::Probable,
                format!("ensemble disagreement {dis:.2} sd; {against} horizons >= 30 s forecast against"),
            );
        }
    }

    let (verdict, note) = if f.is_empty() {
        if rec.won && !poorly_executed {
            (
                Verdict::NoMistake,
                "winner, execution within estimate".to_string(),
            )
        } else if actual.is_none() {
            (
                Verdict::Uncertain,
                "trade-horizon move not observed: forecast causes cannot be assessed".to_string(),
            )
        } else {
            (
                Verdict::Uncertain,
                format!(
                    "no rule fired; realised move within forecast CI = {:?}: loss consistent with forecast noise, not evidence of model error",
                    within
                ),
            )
        }
    } else {
        (Verdict::Classified, String::new())
    };
    MistakeReport {
        trade_id: snap.trade_id.clone(),
        won: rec.won,
        poorly_executed,
        verdict,
        findings: f,
        within_forecast_ci: within,
        forecast_z: z,
        note,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outcome::{derive, tests::outcome};
    use crate::snapshot::tests::snap;
    use crate::types::dec;

    fn run(
        s: &DecisionSnapshot,
        o: crate::outcome::TradeOutcome,
        cal: Option<CalibrationContext>,
    ) -> MistakeReport {
        let rec = derive(s, o).unwrap();
        classify(s, &rec, cal, &ClassifierRules::default())
    }
    fn has(r: &MistakeReport, c: Cause) -> bool {
        r.findings.iter().any(|f| f.cause == c)
    }

    #[test]
    fn clean_winner_is_no_mistake() {
        let s = snap("a", 1_000);
        let r = run(
            &s,
            outcome("a", 1_000, "100.6", ExitReason::TakeProfit, 0.001),
            None,
        );
        assert_eq!(r.verdict, Verdict::NoMistake, "{r:?}");
    }

    #[test]
    fn noise_loss_is_uncertain_not_model_error() {
        let s = snap("a", 1_000);
        // 120 s move +0.0001 (right sign, inside CI) but a time exit at a small loss
        let mut o = outcome("a", 1_000, "100.0", ExitReason::TimeExit, 0.0001);
        o.mfe = 0.002;
        o.mae = 0.001;
        let r = run(&s, o, None);
        assert_eq!(r.verdict, Verdict::Uncertain, "{r:?}");
        assert_eq!(r.within_forecast_ci, Some(true));
    }

    #[test]
    fn wrong_direction_stop() {
        let s = snap("a", 1_000);
        let r = run(
            &s,
            outcome("a", 1_000, "99.5", ExitReason::StopLoss, -0.004),
            None,
        );
        assert!(has(&r, Cause::IncorrectDirection), "{r:?}");
        assert!(!has(&r, Cause::IncorrectTiming));
    }

    #[test]
    fn stopped_then_recovered_is_timing() {
        let s = snap("a", 1_000);
        let r = run(
            &s,
            outcome("a", 1_000, "99.5", ExitReason::StopLoss, 0.003),
            None,
        );
        assert!(has(&r, Cause::IncorrectTiming), "{r:?}");
    }

    #[test]
    fn magnitude_volatility_and_event() {
        let s = snap("a", 1_000);
        // sd at 120 s = 0.004; move of +0.024 is z ~ 5.9 but loss via stop first
        let mut o = outcome("a", 1_000, "99.5", ExitReason::StopLoss, 0.024);
        o.mae = 0.013;
        let r = run(&s, o, None);
        assert!(has(&r, Cause::IncorrectMagnitude), "{r:?}");
        assert!(has(&r, Cause::EventOutsideInformation));
        assert!(has(&r, Cause::VolatilityEstimate));
    }

    #[test]
    fn stop_too_tight_and_target_too_ambitious() {
        let mut s = snap("a", 1_000);
        s.stop_price = dec("99.9"); // 10 bps < 0.75 * 40 bps
        let r = run(
            &s,
            outcome("a", 1_000, "99.9", ExitReason::StopLoss, -0.001),
            None,
        );
        assert!(has(&r, Cause::StopTooTight), "{r:?}");
        let mut s = snap("a", 1_000);
        s.target_price = dec("101"); // 100 bps > 2 * 40 bps
        let r = run(
            &s,
            outcome("a", 1_000, "99.9", ExitReason::TimeExit, -0.001),
            None,
        );
        assert!(has(&r, Cause::TargetTooAmbitious), "{r:?}");
    }

    #[test]
    fn execution_causes_score_winners_too() {
        let mut s = snap("a", 1_000);
        s.data_quality.book_snapshot_interval_ms = 2_000;
        let mut o = outcome("a", 1_000, "100.6", ExitReason::TakeProfit, 0.006);
        o.entry_fills[0].price = dec("100.08"); // 8 bps entry slip
        o.entry_fills[0].qty = dec("0.5");
        o.exit_fills[0].qty = dec("0.5");
        o.flags.spread_exit_bps = Some(6.0);
        o.flags.exchange_error = Some("429 rate limited on first exit".into());
        let r = run(&s, o, None);
        assert!(r.won && r.poorly_executed);
        for c in [
            Cause::Slippage,
            Cause::OrderBookSnapshotLimitation,
            Cause::SpreadExpansion,
            Cause::PartialFill,
            Cause::ExchangeApiError,
        ] {
            assert!(has(&r, c), "{c:?} missing: {r:?}");
        }
        assert!(!has(&r, Cause::IncorrectDirection));
    }

    #[test]
    fn stale_deadline_sizing_regime_disagreement_calibration() {
        let mut s = snap("a", 1_000);
        s.data_quality.book_age_ms = 4_000;
        s.model_disagreement_sd = Some(1.5);
        let mut o = outcome("a", 1_000, "98", ExitReason::StopLoss, -0.004);
        o.exit_fills[0].ts_ms = 1_000 + 130_000;
        o.recorded_ms = 1_000 + 131_000;
        o.flags.regime_after = Some("chop".into());
        let r = run(
            &s,
            o,
            Some(CalibrationContext {
                bin_rate: 0.3,
                bin_n_eff: 40.0,
            }),
        );
        for c in [
            Cause::StaleData,
            Cause::StrategyExecutionError,
            Cause::RiskSizingError,
            Cause::RegimeMisclassification,
            Cause::ModelDisagreement,
            Cause::ProbabilityCalibration,
        ] {
            assert!(has(&r, c), "{c:?} missing: {r:?}");
        }
    }

    #[test]
    fn calibration_needs_minimum_sample() {
        let s = snap("a", 1_000);
        let r = run(
            &s,
            outcome("a", 1_000, "99.5", ExitReason::StopLoss, -0.004),
            Some(CalibrationContext {
                bin_rate: 0.1,
                bin_n_eff: 5.0,
            }),
        );
        assert!(!has(&r, Cause::ProbabilityCalibration));
    }

    #[test]
    fn unobserved_horizon_is_uncertain() {
        let s = snap("a", 1_000);
        let mut o = outcome("a", 1_000, "100.0", ExitReason::TimeExit, 0.0);
        for m in &mut o.horizon_moves {
            m.actual_return = None;
        }
        let r = run(&s, o, None);
        assert_eq!(r.verdict, Verdict::Uncertain);
        assert!(r.findings.is_empty());
    }
}
