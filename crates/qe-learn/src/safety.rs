//! §6.6 Learning safety rules, enforced in code.
//!
//! Learning improves accuracy, calibration and execution. It is never a
//! loss-recovery mechanism. Two mechanisms enforce that:
//!
//! 1. [`LearningSignal`] is the *only* thing learning hands to the desk. Its
//!    multipliers are clamped to `[0, 1]` at construction, so applying it can
//!    only shrink confidence or size. It has no leverage, stop or limit field.
//! 2. [`vet_adjustment`] checks any proposed change to risk parameters that a
//!    learning process originates. Loosening is always refused: higher
//!    leverage, larger size or per-trade risk, a larger session-loss limit,
//!    removing or widening the stop, or dropping the take-profit. Changes
//!    motivated by a recent loss, a losing streak or a missed target may only
//!    tighten. A change backed by fewer than `min_evidence` trades may only
//!    tighten (no reaction to one trade or a short streak).
//!
//! The risk governor stays the final authority; this module cannot approve
//! anything, it can only refuse or shrink.
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// What learning may tell the desk: shrink-only multipliers plus reasons.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LearningSignal {
    confidence_multiplier: f64,
    size_multiplier: f64,
    pub reasons: Vec<String>,
}

fn shrink(x: f64) -> f64 {
    if x.is_finite() {
        x.clamp(0.0, 1.0)
    } else {
        0.0 // unknown => most conservative
    }
}

impl LearningSignal {
    pub fn new(confidence_multiplier: f64, size_multiplier: f64, reasons: Vec<String>) -> Self {
        LearningSignal {
            confidence_multiplier: shrink(confidence_multiplier),
            size_multiplier: shrink(size_multiplier),
            reasons,
        }
    }
    pub fn neutral() -> Self {
        Self::new(1.0, 1.0, Vec::new())
    }
    pub fn confidence_multiplier(&self) -> f64 {
        self.confidence_multiplier
    }
    pub fn size_multiplier(&self) -> f64 {
        self.size_multiplier
    }
    /// combine two signals: the more conservative wins on each axis
    pub fn and(mut self, o: &LearningSignal) -> Self {
        self.confidence_multiplier = self.confidence_multiplier.min(o.confidence_multiplier);
        self.size_multiplier = self.size_multiplier.min(o.size_multiplier);
        self.reasons.extend(o.reasons.iter().cloned());
        self
    }
    /// apply to a probability edge: never increases it
    pub fn apply_confidence(&self, p: f64) -> f64 {
        // pull p toward 0.5 (no information) by the multiplier
        0.5 + (p - 0.5) * self.confidence_multiplier
    }
    /// apply to a governor-approved quantity: never increases it
    pub fn apply_size(&self, qty: Decimal) -> Decimal {
        let m = Decimal::from_f64_retain(self.size_multiplier).unwrap_or(Decimal::ZERO);
        (qty * m).min(qty).max(Decimal::ZERO)
    }
}

/// The risk parameters a learning process could be tempted to touch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RiskParams {
    pub leverage: Decimal,
    /// fraction of equity risked per trade
    pub risk_per_trade: Decimal,
    pub max_session_loss: Decimal,
    pub max_open_positions: u32,
    /// None = no stop: always refused
    pub stop_distance_frac: Option<Decimal>,
    pub take_profit_required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Motive {
    CalibrationUpdate,
    RegimeShift,
    LessonReview,
    ChampionPromotion,
    RecentLoss,
    LosingStreak,
    TargetMissed,
}

impl Motive {
    fn tighten_only(self) -> bool {
        matches!(
            self,
            Motive::RecentLoss | Motive::LosingStreak | Motive::TargetMissed
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdjustmentRequest {
    pub motive: Motive,
    /// number of out-of-sample trades behind the change
    pub evidence_n: usize,
    pub current: RiskParams,
    pub proposed: RiskParams,
}

/// minimum trades behind any change that is not a pure tightening
pub const MIN_EVIDENCE: usize = 30;

/// Returns the accepted parameters, or every violation found.
pub fn vet_adjustment(req: &AdjustmentRequest) -> crate::types::Result<RiskParams> {
    let (c, p) = (&req.current, &req.proposed);
    let mut v = Vec::new();
    if p.leverage > c.leverage {
        v.push(format!(
            "leverage {} -> {} increases risk",
            c.leverage, p.leverage
        ));
    }
    if p.risk_per_trade > c.risk_per_trade {
        v.push(format!(
            "risk per trade {} -> {} increases size",
            c.risk_per_trade, p.risk_per_trade
        ));
    }
    if p.max_session_loss > c.max_session_loss {
        v.push(format!(
            "session loss limit {} -> {} loosened",
            c.max_session_loss, p.max_session_loss
        ));
    }
    if p.max_open_positions > c.max_open_positions {
        v.push("more concurrent positions".to_string());
    }
    match (c.stop_distance_frac, p.stop_distance_frac) {
        (_, None) => v.push("stop removed".to_string()),
        (Some(a), Some(b)) if b > a => v.push(format!("stop widened {a} -> {b}")),
        _ => {}
    }
    if c.take_profit_required && !p.take_profit_required {
        v.push("take-profit requirement removed".to_string());
    }
    // every check above is a loosening; a pure tightening is always allowed
    let tightening = v.is_empty();
    if !tightening && req.motive.tighten_only() {
        v.push(format!("{:?} may only tighten risk", req.motive));
    }
    if !tightening && req.evidence_n < MIN_EVIDENCE {
        v.push(format!(
            "only {} trades of evidence (< {MIN_EVIDENCE})",
            req.evidence_n
        ));
    }
    if v.is_empty() {
        Ok(p.clone())
    } else {
        Err(crate::types::LearnError::SafetyViolation(v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::dec;

    fn params() -> RiskParams {
        RiskParams {
            leverage: dec("10"),
            risk_per_trade: dec("0.01"),
            max_session_loss: dec("0.05"),
            max_open_positions: 1,
            stop_distance_frac: Some(dec("0.005")),
            take_profit_required: true,
        }
    }
    fn req(motive: Motive, n: usize, f: impl Fn(&mut RiskParams)) -> AdjustmentRequest {
        let mut p = params();
        f(&mut p);
        AdjustmentRequest {
            motive,
            evidence_n: n,
            current: params(),
            proposed: p,
        }
    }

    #[test]
    fn signal_can_only_shrink() {
        let s = LearningSignal::new(1.7, 3.0, vec![]);
        assert_eq!(s.confidence_multiplier(), 1.0);
        assert_eq!(s.size_multiplier(), 1.0);
        assert_eq!(s.apply_size(dec("2")), dec("2"));
        let s =
            LearningSignal::new(0.5, 0.25, vec![]).and(&LearningSignal::new(f64::NAN, 1.0, vec![]));
        assert_eq!(s.confidence_multiplier(), 0.0);
        assert_eq!(s.apply_size(dec("2")), dec("0.5"));
        assert_eq!(s.apply_confidence(0.9), 0.5);
        assert!(LearningSignal::new(0.5, 1.0, vec![]).apply_confidence(0.7) < 0.7);
    }

    #[test]
    fn never_raise_leverage_size_or_limits_or_remove_stops() {
        for (motive, n) in [
            (Motive::CalibrationUpdate, 10_000),
            (Motive::RecentLoss, 10_000),
        ] {
            assert!(vet_adjustment(&req(motive, n, |p| p.leverage = dec("20"))).is_err());
            assert!(vet_adjustment(&req(motive, n, |p| p.risk_per_trade = dec("0.02"))).is_err());
            assert!(vet_adjustment(&req(motive, n, |p| p.max_session_loss = dec("0.1"))).is_err());
            assert!(vet_adjustment(&req(motive, n, |p| p.stop_distance_frac = None)).is_err());
            assert!(
                vet_adjustment(&req(motive, n, |p| p.stop_distance_frac = Some(dec("0.01"))))
                    .is_err()
            );
            assert!(vet_adjustment(&req(motive, n, |p| p.take_profit_required = false)).is_err());
            assert!(vet_adjustment(&req(motive, n, |p| p.max_open_positions = 2)).is_err());
        }
    }

    #[test]
    fn tightening_is_always_allowed_even_after_a_loss() {
        for motive in [
            Motive::RecentLoss,
            Motive::TargetMissed,
            Motive::LessonReview,
        ] {
            let out = vet_adjustment(&req(motive, 1, |p| {
                p.leverage = dec("5");
                p.risk_per_trade = dec("0.005");
            }))
            .unwrap();
            assert_eq!(out.leverage, dec("5"));
        }
    }

    #[test]
    fn missed_target_cannot_loosen_anything() {
        let e = vet_adjustment(&req(Motive::TargetMissed, 1_000, |p| {
            p.leverage = dec("11")
        }))
        .unwrap_err();
        let crate::types::LearnError::SafetyViolation(v) = e else {
            panic!()
        };
        assert!(v.iter().any(|m| m.contains("may only tighten")));
    }
}
