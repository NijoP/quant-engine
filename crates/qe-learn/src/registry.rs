//! §6.5 C Champion/challenger model registry.
//!
//! Promotion criteria are fixed when the registry is created
//! ([`PromotionCriteria`] is private after construction): a challenger becomes
//! champion only with BOTH an out-of-sample evaluation and a paper-forward
//! evaluation that each meet the minimum sample, beat the champion's Brier by
//! the required margin, stay under the ECE cap and keep net expectancy above
//! the floor. Windows must be chronological: training ends before validation
//! starts, the out-of-sample window starts after training ends, and the
//! paper-forward window starts after the challenger was registered (it is a
//! forward test, not a replay). Every promotion and rollback is recorded.
use crate::types::{LearnError, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeatureDef {
    pub name: String,
    pub definition: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Window {
    pub start_ms: i64,
    pub end_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelSpec {
    pub version: String,
    pub parent: Option<String>,
    pub registered_ms: i64,
    pub training: Window,
    pub validation: Window,
    pub features: Vec<FeatureDef>,
    pub assumptions: Vec<String>,
    pub limitations: Vec<String>,
    /// e.g. a link to the calibration report
    pub calibration_report: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalKind {
    OutOfSample,
    PaperForward,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalResult {
    pub kind: EvalKind,
    pub window: Window,
    pub n: usize,
    pub brier: f64,
    pub log_loss: f64,
    pub ece: f64,
    pub directional_accuracy: f64,
    /// net of fees, spread, slippage
    pub net_expectancy_bps: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Champion,
    Challenger,
    Retired,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub spec: ModelSpec,
    pub role: Role,
    pub evals: Vec<EvalResult>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromotionCriteria {
    pub min_oos_n: usize,
    pub min_paper_n: usize,
    /// challenger Brier must be <= champion Brier * this, on each eval kind
    pub max_brier_ratio: f64,
    pub max_ece: f64,
    pub min_net_expectancy_bps: f64,
}

impl Default for PromotionCriteria {
    fn default() -> Self {
        PromotionCriteria {
            min_oos_n: 300,
            min_paper_n: 50,
            max_brier_ratio: 0.98,
            max_ece: 0.05,
            min_net_expectancy_bps: 0.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "action")]
pub enum HistoryEvent {
    Registered {
        version: String,
        at_ms: i64,
    },
    Promoted {
        version: String,
        replaced: Option<String>,
        at_ms: i64,
        evidence: Vec<String>,
    },
    RolledBack {
        from: String,
        to: String,
        at_ms: i64,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Registry {
    criteria: PromotionCriteria,
    entries: Vec<Entry>,
    history: Vec<HistoryEvent>,
    /// champions in promotion order; last = current; earlier = rollback targets
    lineage: Vec<String>,
}

impl Registry {
    pub fn new(criteria: PromotionCriteria) -> Self {
        Registry {
            criteria,
            entries: Vec::new(),
            history: Vec::new(),
            lineage: Vec::new(),
        }
    }
    pub fn criteria(&self) -> &PromotionCriteria {
        &self.criteria
    }
    pub fn champion(&self) -> Option<&Entry> {
        self.lineage.last().and_then(|v| self.get(v))
    }
    pub fn get(&self, version: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.spec.version == version)
    }
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }
    pub fn history(&self) -> &[HistoryEvent] {
        &self.history
    }
    fn idx(&self, v: &str) -> Result<usize> {
        self.entries
            .iter()
            .position(|e| e.spec.version == v)
            .ok_or_else(|| LearnError::UnknownTrade(format!("model {v}")))
    }

    /// The first model registered becomes the initial champion (the baseline
    /// already running); every later one starts as a challenger.
    pub fn register(&mut self, spec: ModelSpec) -> Result<Role> {
        if spec.version.trim().is_empty() {
            return Err(LearnError::Invalid("empty model version".into()));
        }
        if self.get(&spec.version).is_some() {
            return Err(LearnError::Duplicate(spec.version));
        }
        if spec.training.start_ms > spec.training.end_ms
            || spec.validation.start_ms > spec.validation.end_ms
        {
            return Err(LearnError::Invalid("window start after end".into()));
        }
        if spec.validation.start_ms < spec.training.end_ms {
            return Err(LearnError::Invalid(
                "validation window overlaps training window (leakage)".into(),
            ));
        }
        if spec.features.is_empty() {
            return Err(LearnError::Invalid("feature definitions required".into()));
        }
        let role = if self.lineage.is_empty() {
            Role::Champion
        } else {
            Role::Challenger
        };
        self.history.push(HistoryEvent::Registered {
            version: spec.version.clone(),
            at_ms: spec.registered_ms,
        });
        if role == Role::Champion {
            self.lineage.push(spec.version.clone());
        }
        self.entries.push(Entry {
            spec,
            role,
            evals: Vec::new(),
        });
        Ok(role)
    }

    pub fn add_eval(&mut self, version: &str, e: EvalResult) -> Result<()> {
        let i = self.idx(version)?;
        let spec = &self.entries[i].spec;
        if e.window.start_ms > e.window.end_ms {
            return Err(LearnError::Invalid("eval window start after end".into()));
        }
        if e.window.start_ms < spec.training.end_ms {
            return Err(LearnError::Invalid(
                "evaluation window overlaps training (not out-of-sample)".into(),
            ));
        }
        if e.kind == EvalKind::PaperForward && e.window.start_ms < spec.registered_ms {
            return Err(LearnError::Invalid(
                "paper-forward window starts before registration (not forward)".into(),
            ));
        }
        for x in [
            e.brier,
            e.log_loss,
            e.ece,
            e.directional_accuracy,
            e.net_expectancy_bps,
        ] {
            crate::types::finite("eval metric", x)?;
        }
        self.entries[i].evals.push(e);
        Ok(())
    }

    fn latest(&self, i: usize, k: EvalKind) -> Option<&EvalResult> {
        self.entries[i].evals.iter().rev().find(|e| e.kind == k)
    }

    /// Check every criterion; returns the evidence lines on success.
    pub fn check_promotion(&self, version: &str) -> Result<Vec<String>> {
        let i = self.idx(version)?;
        let c = &self.criteria;
        let mut fail = Vec::new();
        let mut ok = Vec::new();
        if self.entries[i].role != Role::Challenger {
            fail.push(format!("{version} is not a challenger"));
        }
        let champ = self.lineage.last().map(|v| self.idx(v)).transpose()?;
        for (kind, min_n) in [
            (EvalKind::OutOfSample, c.min_oos_n),
            (EvalKind::PaperForward, c.min_paper_n),
        ] {
            let Some(e) = self.latest(i, kind) else {
                fail.push(format!("no {kind:?} evaluation"));
                continue;
            };
            if e.n < min_n {
                fail.push(format!("{kind:?} n={} < {min_n}", e.n));
            }
            if e.ece > c.max_ece {
                fail.push(format!("{kind:?} ECE {:.4} > {}", e.ece, c.max_ece));
            }
            if e.net_expectancy_bps < c.min_net_expectancy_bps {
                fail.push(format!(
                    "{kind:?} net expectancy {:.2} bps < {}",
                    e.net_expectancy_bps, c.min_net_expectancy_bps
                ));
            }
            if let Some(ci) = champ {
                match self.latest(ci, kind) {
                    Some(ce) if e.brier <= ce.brier * c.max_brier_ratio => ok.push(format!(
                        "{kind:?} Brier {:.5} <= champion {:.5} x {}",
                        e.brier, ce.brier, c.max_brier_ratio
                    )),
                    Some(ce) => fail.push(format!(
                        "{kind:?} Brier {:.5} not below champion {:.5} x {}",
                        e.brier, ce.brier, c.max_brier_ratio
                    )),
                    None => fail.push(format!("champion has no {kind:?} evaluation to compare")),
                }
            }
            ok.push(format!(
                "{kind:?} n={} ECE={:.4} net={:.2} bps",
                e.n, e.ece, e.net_expectancy_bps
            ));
        }
        if fail.is_empty() {
            Ok(ok)
        } else {
            Err(LearnError::PromotionRejected(fail))
        }
    }

    pub fn promote(&mut self, version: &str, at_ms: i64) -> Result<()> {
        let evidence = self.check_promotion(version)?;
        let i = self.idx(version)?;
        let replaced = self.lineage.last().cloned();
        if let Some(r) = &replaced {
            let ri = self.idx(r)?;
            self.entries[ri].role = Role::Retired;
        }
        self.entries[i].role = Role::Champion;
        self.lineage.push(version.to_string());
        self.history.push(HistoryEvent::Promoted {
            version: version.to_string(),
            replaced,
            at_ms,
            evidence,
        });
        Ok(())
    }

    /// Restore the previous champion. Always allowed: rolling back reduces
    /// exposure to an unproven model.
    pub fn rollback(&mut self, at_ms: i64, reason: &str) -> Result<String> {
        if self.lineage.len() < 2 {
            return Err(LearnError::Invalid(
                "no previous champion to roll back to".into(),
            ));
        }
        let from = self.lineage.pop().expect("len >= 2");
        let to = self.lineage.last().cloned().expect("len >= 1");
        let fi = self.idx(&from)?;
        let ti = self.idx(&to)?;
        self.entries[fi].role = Role::Retired;
        self.entries[ti].role = Role::Champion;
        self.history.push(HistoryEvent::RolledBack {
            from,
            to: to.clone(),
            at_ms,
            reason: reason.to_string(),
        });
        Ok(to)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(v: &str, reg: i64) -> ModelSpec {
        ModelSpec {
            version: v.into(),
            parent: None,
            registered_ms: reg,
            training: Window {
                start_ms: 0,
                end_ms: 1_000,
            },
            validation: Window {
                start_ms: 1_000,
                end_ms: 2_000,
            },
            features: vec![FeatureDef {
                name: "obi_5".into(),
                definition: "(bid5-ask5)/(bid5+ask5)".into(),
            }],
            assumptions: vec!["1 s returns ~ locally Gaussian".into()],
            limitations: vec!["futures book is snapshot-only".into()],
            calibration_report: None,
        }
    }
    fn ev(kind: EvalKind, start: i64, n: usize, brier: f64) -> EvalResult {
        EvalResult {
            kind,
            window: Window {
                start_ms: start,
                end_ms: start + 1_000,
            },
            n,
            brier,
            log_loss: 0.68,
            ece: 0.02,
            directional_accuracy: 0.53,
            net_expectancy_bps: 1.0,
        }
    }

    fn reg() -> Registry {
        let mut r = Registry::new(PromotionCriteria::default());
        assert_eq!(r.register(spec("v1", 0)).unwrap(), Role::Champion);
        r.add_eval("v1", ev(EvalKind::OutOfSample, 1_000, 500, 0.250))
            .unwrap();
        r.add_eval("v1", ev(EvalKind::PaperForward, 3_000, 80, 0.250))
            .unwrap();
        assert_eq!(r.register(spec("v2", 2_500)).unwrap(), Role::Challenger);
        r
    }

    #[test]
    fn promotion_needs_oos_and_paper_forward() {
        let mut r = reg();
        r.add_eval("v2", ev(EvalKind::OutOfSample, 1_000, 500, 0.240))
            .unwrap();
        let e = r.promote("v2", 5_000).unwrap_err();
        assert!(
            matches!(e, LearnError::PromotionRejected(ref v) if v.iter().any(|m| m.contains("PaperForward")))
        );
        r.add_eval("v2", ev(EvalKind::PaperForward, 3_000, 60, 0.240))
            .unwrap();
        r.promote("v2", 5_000).unwrap();
        assert_eq!(r.champion().unwrap().spec.version, "v2");
        assert_eq!(r.get("v1").unwrap().role, Role::Retired);
    }

    #[test]
    fn small_sample_or_marginal_gain_is_rejected() {
        let mut r = reg();
        r.add_eval("v2", ev(EvalKind::OutOfSample, 1_000, 1, 0.10))
            .unwrap(); // one trade
        r.add_eval("v2", ev(EvalKind::PaperForward, 3_000, 60, 0.249))
            .unwrap(); // < 2% better
        let LearnError::PromotionRejected(v) = r.promote("v2", 5_000).unwrap_err() else {
            panic!()
        };
        assert!(v.iter().any(|m| m.contains("n=1")));
        assert!(v.iter().any(|m| m.contains("not below champion")));
        assert_eq!(r.champion().unwrap().spec.version, "v1");
    }

    #[test]
    fn leakage_windows_rejected() {
        let mut r = reg();
        // OOS overlapping training
        assert!(r
            .add_eval("v2", ev(EvalKind::OutOfSample, 500, 500, 0.2))
            .is_err());
        // paper-forward before registration (2 500)
        assert!(r
            .add_eval("v2", ev(EvalKind::PaperForward, 2_000, 60, 0.2))
            .is_err());
        let mut bad = spec("v3", 0);
        bad.validation.start_ms = 500;
        assert!(r.register(bad).is_err());
    }

    #[test]
    fn rollback_restores_previous_champion_and_logs_history() {
        let mut r = reg();
        r.add_eval("v2", ev(EvalKind::OutOfSample, 1_000, 500, 0.240))
            .unwrap();
        r.add_eval("v2", ev(EvalKind::PaperForward, 3_000, 60, 0.240))
            .unwrap();
        r.promote("v2", 5_000).unwrap();
        assert_eq!(r.rollback(6_000, "live calibration drift").unwrap(), "v1");
        assert_eq!(r.champion().unwrap().spec.version, "v1");
        assert!(matches!(
            r.history().last(),
            Some(HistoryEvent::RolledBack { .. })
        ));
        assert!(r.rollback(7_000, "again").is_err());
    }
}
