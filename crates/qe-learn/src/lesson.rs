//! §6.5 D Post-trade lesson report: what was predicted, what happened, the
//! error size, the probable cause, whether it is known or uncertain, a new
//! hypothesis to test, and the evidence that would justify a model change.
//!
//! A lesson never changes anything by itself: a hypothesis becomes a
//! challenger model, which can only be promoted through the registry gate.
use crate::classify::{Cause, Certainty, MistakeReport, Verdict};
use crate::outcome::OutcomeRecord;
use crate::registry::PromotionCriteria;
use crate::snapshot::DecisionSnapshot;
use crate::types::Mode;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LessonReport {
    pub trade_id: String,
    pub mode: Mode,
    pub instrument: String,
    pub model_version: String,
    pub strategy_version: String,
    pub predicted: String,
    pub happened: String,
    pub horizon_s: Option<u32>,
    pub predicted_return: Option<f64>,
    pub actual_return: Option<f64>,
    pub return_error: Option<f64>,
    /// error in predicted-sd units
    pub error_z: Option<f64>,
    pub net_pnl: String,
    pub verdict: Verdict,
    pub probable_causes: Vec<String>,
    pub certainty: Certainty,
    pub hypothesis: String,
    pub evidence_required: String,
    pub recorded_ms: i64,
}

fn hypothesis(c: Cause) -> &'static str {
    use Cause::*;
    match c {
        IncorrectDirection => "the directional signal has no edge in this regime/feature state; test a regime-conditioned direction model",
        IncorrectMagnitude => "the expected-move model is mis-scaled; test a magnitude model conditioned on realised volatility",
        IncorrectTiming => "the edge appears beyond the stop's survival time; test entry timing / delayed confirmation",
        VolatilityEstimate => "the volatility forecast lags; test a faster EW or regime-switching volatility estimator",
        ProbabilityCalibration => "first-passage probabilities are mis-calibrated in this bin; test a recalibrated mapping",
        RegimeMisclassification => "the regime rules mislabel this state; test revised regime thresholds",
        OrderBookSnapshotLimitation => "snapshot-only books under-represent liquidity; test a larger slippage allowance when snapshot intervals are long",
        StaleData => "stale inputs reached the decision; test a stricter freshness gate",
        SpreadExpansion => "spread widens around entries; test a spread-forecast term in the cost model",
        Slippage => "the slippage model is optimistic; test a depth-walk slippage estimate",
        PartialFill => "the order size is too large for displayed depth; test a depth-capped size",
        StopTooTight => "the stop sits inside normal noise; test a volatility-scaled stop with size reduced to keep risk constant",
        TargetTooAmbitious => "the target is beyond typical reach in 120 s; test a volatility-scaled target",
        ModelDisagreement => "entries taken despite model disagreement lose; test an agreement gate",
        EventOutsideInformation => "an outside event moved the price; no model change unless such moves become predictable",
        StrategyExecutionError => "the lifecycle executor misbehaved; fix and test the execution path (not a model issue)",
        RiskSizingError => "realised loss exceeded the risk budget; audit sizing and stop execution (not a model issue)",
        ExchangeApiError => "exchange/API errors affected the trade; test retry/reconcile handling (not a model issue)",
    }
}

pub fn lesson(
    snap: &DecisionSnapshot,
    rec: &OutcomeRecord,
    mistakes: &MistakeReport,
    criteria: &PromotionCriteria,
) -> LessonReport {
    let fc = snap.trade_forecast();
    let actual = fc.and_then(|f| rec.outcome.move_at(f.horizon_s));
    let err = fc.zip(actual).map(|(f, a)| a - f.expected_return);
    let mode = match snap.mode {
        Mode::Paper => "PAPER",
        Mode::Live => "LIVE",
    };
    let predicted = match fc {
        Some(f) => format!(
            "{mode} {:?} {}: {}s return {:+.5} (sd {:.5}, P(up) {:.3}); P(TP first) {:.3}, P(SL first) {:.3}, P(neither) {:.3}; est cost {:.2} bps",
            snap.direction, snap.instrument, f.horizon_s, f.expected_return, f.return_sd, f.p_up,
            snap.p_tp_first, snap.p_sl_first, snap.p_neither(), snap.est_cost_bps()
        ),
        None => format!("{mode} {:?} {}: no forecast recorded", snap.direction, snap.instrument),
    };
    let happened = format!(
        "exit {:?} after {} ms; {}s move {}; MFE {:.5} MAE {:.5}; actual cost {:.2} bps; net P&L {} ({})",
        rec.outcome.exit_reason,
        rec.holding_ms,
        fc.map(|f| f.horizon_s).unwrap_or(0),
        actual.map(|a| format!("{a:+.5}")).unwrap_or_else(|| "not observed".into()),
        rec.outcome.mfe,
        rec.outcome.mae,
        rec.actual_cost_bps,
        rec.net_pnl,
        mode
    );
    let (hyp, certainty) = match mistakes.primary() {
        Some(f) => (hypothesis(f.cause).to_string(), f.certainty),
        None if mistakes.verdict == Verdict::NoMistake => (
            "none: outcome consistent with forecast and execution estimates".to_string(),
            Certainty::Known,
        ),
        None => (
            "none yet: the loss is consistent with forecast noise; keep observing".to_string(),
            Certainty::Uncertain,
        ),
    };
    let evidence_required = if mistakes
        .primary()
        .is_some_and(|f| !f.cause.is_execution() && f.cause != Cause::EventOutsideInformation)
    {
        format!(
            "a challenger built on this hypothesis must pass >= {} out-of-sample and >= {} paper-forward trades with Brier <= {:.0}% of the champion's, ECE <= {} and net expectancy >= {} bps; one trade never justifies a change",
            criteria.min_oos_n,
            criteria.min_paper_n,
            criteria.max_brier_ratio * 100.0,
            criteria.max_ece,
            criteria.min_net_expectancy_bps
        )
    } else if mistakes.primary().is_some() {
        "fix verified by tests and >= 50 paper trades without recurrence; no model change, no risk loosening".to_string()
    } else {
        "no change proposed".to_string()
    };
    LessonReport {
        trade_id: snap.trade_id.clone(),
        mode: snap.mode,
        instrument: snap.instrument.clone(),
        model_version: snap.model_version.clone(),
        strategy_version: snap.strategy_version.clone(),
        predicted,
        happened,
        horizon_s: fc.map(|f| f.horizon_s),
        predicted_return: fc.map(|f| f.expected_return),
        actual_return: actual,
        return_error: err,
        error_z: mistakes.forecast_z,
        net_pnl: rec.net_pnl.to_string(),
        verdict: mistakes.verdict,
        probable_causes: mistakes
            .findings
            .iter()
            .map(|f| format!("{:?} ({:?}): {}", f.cause, f.certainty, f.evidence))
            .collect(),
        certainty,
        hypothesis: hyp,
        evidence_required,
        recorded_ms: rec.outcome.recorded_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::{classify, ClassifierRules};
    use crate::outcome::{derive, tests::outcome, ExitReason};
    use crate::snapshot::tests::snap;

    #[test]
    fn lesson_for_wrong_direction_loss() {
        let s = snap("a", 1_000);
        let r = derive(
            &s,
            outcome("a", 1_000, "99.5", ExitReason::StopLoss, -0.004),
        )
        .unwrap();
        let m = classify(&s, &r, None, &ClassifierRules::default());
        let l = lesson(&s, &r, &m, &PromotionCriteria::default());
        assert!(l.predicted.starts_with("PAPER"));
        assert!(l.happened.contains("PAPER"));
        assert!((l.return_error.unwrap() - (-0.0044)).abs() < 1e-12);
        assert_eq!(l.certainty, Certainty::Probable);
        assert!(l.hypothesis.contains("directional"));
        assert!(l.evidence_required.contains("out-of-sample"));
        let j = serde_json::to_value(&l).unwrap();
        assert_eq!(j["mode"], "paper");
    }

    #[test]
    fn lesson_for_clean_winner_proposes_nothing() {
        let s = snap("a", 1_000);
        let r = derive(
            &s,
            outcome("a", 1_000, "100.6", ExitReason::TakeProfit, 0.001),
        )
        .unwrap();
        let m = classify(&s, &r, None, &ClassifierRules::default());
        let l = lesson(&s, &r, &m, &PromotionCriteria::default());
        assert_eq!(l.verdict, Verdict::NoMistake);
        assert_eq!(l.evidence_required, "no change proposed");
    }
}
