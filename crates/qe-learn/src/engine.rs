//! The facade s3-desk calls: one [`LearningEngine`] per desk process.
use crate::calibration::{self, CalibConfig, Calibrated, OnlineCalibrator, TrackerView};
use crate::classify::{classify, CalibrationContext, ClassifierRules, MistakeReport, Verdict};
use crate::lesson::{lesson, LessonReport};
use crate::metrics::{compute, Filter, MetricsReport};
use crate::outcome::{derive, ExitReason, OutcomeRecord, TradeOutcome};
use crate::regime::{RegimeConfig, RegimeMonitor, RegimeState, Signal};
use crate::registry::{PromotionCriteria, Registry};
use crate::safety::LearningSignal;
use crate::snapshot::{seal_hash, DecisionLog, DecisionSnapshot, SealedDecision};
use crate::types::{LearnError, Mode, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EngineConfig {
    pub rules: ClassifierRules,
    pub calibration: CalibConfig,
    pub regime: RegimeConfig,
    pub promotion: PromotionCriteria,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionReceipt {
    pub trade_id: String,
    pub seq: u64,
    pub sha256: String,
}

/// Live market observation for regime tracking (all optional).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct MarketObs {
    pub volatility: Option<f64>,
    pub spread_bps: Option<f64>,
    /// displayed depth (e.g. top-5 notional)
    pub depth: Option<f64>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum JournalLine {
    Decision(SealedDecision),
    Outcome(OutcomeRecord),
}

pub struct LearningEngine {
    cfg: EngineConfig,
    decisions: DecisionLog,
    outcomes: Vec<OutcomeRecord>,
    by_trade: HashMap<String, usize>,
    mistakes: HashMap<String, MistakeReport>,
    calibrator: OnlineCalibrator,
    regimes: BTreeMap<String, RegimeMonitor>,
    registry: Registry,
    journal: Option<File>,
    last_outcome_ms: i64,
}

fn outcome_hash(r: &OutcomeRecord) -> String {
    let mut c = r.clone();
    c.sha256.clear();
    seal_hash(&c)
}

fn io(e: std::io::Error) -> LearnError {
    LearnError::Io(e.to_string())
}

impl LearningEngine {
    pub fn new(cfg: EngineConfig) -> Self {
        LearningEngine {
            calibrator: OnlineCalibrator::new(cfg.calibration.clone()),
            registry: Registry::new(cfg.promotion.clone()),
            cfg,
            decisions: DecisionLog::default(),
            outcomes: Vec::new(),
            by_trade: HashMap::new(),
            mistakes: HashMap::new(),
            regimes: BTreeMap::new(),
            journal: None,
            last_outcome_ms: i64::MIN,
        }
    }

    /// Open (or create) an append-only JSONL journal, replaying and verifying
    /// every existing line first. A hash or sequence mismatch refuses to open.
    pub fn open(cfg: EngineConfig, path: &Path) -> Result<Self> {
        let mut e = Self::new(cfg);
        if path.exists() {
            let f = File::open(path).map_err(io)?;
            for (i, line) in BufReader::new(f).lines().enumerate() {
                let line = line.map_err(io)?;
                if line.trim().is_empty() {
                    continue;
                }
                let parsed: JournalLine = serde_json::from_str(&line)
                    .map_err(|err| LearnError::Tampered(format!("line {}: {err}", i + 1)))?;
                match parsed {
                    JournalLine::Decision(d) => {
                        if !d.verify() || d.seq != e.decisions.len() as u64 {
                            return Err(LearnError::Tampered(format!("decision line {}", i + 1)));
                        }
                        e.apply_decision(d.record)?;
                    }
                    JournalLine::Outcome(o) => {
                        let (seq, sha) = (o.seq, o.sha256.clone());
                        let r = e.apply_outcome(o.outcome)?;
                        if r.seq != seq || r.sha256 != sha {
                            return Err(LearnError::Tampered(format!("outcome line {}", i + 1)));
                        }
                    }
                }
            }
        }
        e.journal = Some(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(io)?,
        );
        Ok(e)
    }

    fn write(&mut self, line: &JournalLine) -> Result<()> {
        if let Some(f) = self.journal.as_mut() {
            let mut s = serde_json::to_string(line).map_err(|e| LearnError::Io(e.to_string()))?;
            s.push('\n');
            f.write_all(s.as_bytes()).map_err(io)?;
            f.flush().map_err(io)?;
        }
        Ok(())
    }

    fn apply_decision(&mut self, snap: DecisionSnapshot) -> Result<SealedDecision> {
        Ok(self.decisions.append(snap)?.clone())
    }

    /// §6.1: store the immutable snapshot BEFORE any order is sent.
    pub fn record_decision(&mut self, snap: DecisionSnapshot) -> Result<DecisionReceipt> {
        let sealed = self.apply_decision(snap)?;
        self.write(&JournalLine::Decision(sealed.clone()))?;
        Ok(DecisionReceipt {
            trade_id: sealed.record.trade_id,
            seq: sealed.seq,
            sha256: sealed.sha256,
        })
    }

    fn apply_outcome(&mut self, o: TradeOutcome) -> Result<OutcomeRecord> {
        let snap = self
            .decisions
            .get(&o.trade_id)
            .ok_or_else(|| LearnError::UnknownTrade(o.trade_id.clone()))?
            .record
            .clone();
        if self.by_trade.contains_key(&o.trade_id) {
            return Err(LearnError::Duplicate(o.trade_id));
        }
        if o.recorded_ms < self.last_outcome_ms {
            return Err(LearnError::NotChronological {
                last_ms: self.last_outcome_ms,
                got_ms: o.recorded_ms,
            });
        }
        let mut rec = derive(&snap, o)?;
        // calibration evidence from BEFORE this outcome (no look-ahead)
        let tp_key = calibration::key(&snap.model_version, "tp_first");
        let prior = self.calibrator.recalibrate(&tp_key, snap.p_tp_first);
        let cal = prior.bin_rate.map(|bin_rate| CalibrationContext {
            bin_rate,
            bin_n_eff: prior.bin_n_eff,
        });
        let report = classify(&snap, &rec, cal, &self.cfg.rules);
        let t = rec.outcome.recorded_ms;
        let mv = &snap.model_version;
        self.calibrator.update(
            &tp_key,
            t,
            snap.p_tp_first,
            rec.outcome.exit_reason == ExitReason::TakeProfit,
        )?;
        self.calibrator.update(
            &calibration::key(mv, "win"),
            t,
            snap.win_probability,
            rec.won,
        )?;
        for f in &snap.forecasts {
            if let Some(a) = rec.outcome.move_at(f.horizon_s) {
                self.calibrator.update(
                    &calibration::key(mv, &format!("up_{}s", f.horizon_s)),
                    t,
                    f.p_up,
                    a > 0.0,
                )?;
            }
        }
        if let Some(z) = report.forecast_z {
            self.regime_mut(&snap.instrument)
                .observe(t, Signal::Residual, z)?;
        }
        rec.seq = self.outcomes.len() as u64;
        rec.sha256 = outcome_hash(&rec);
        self.last_outcome_ms = t;
        self.by_trade
            .insert(snap.trade_id.clone(), self.outcomes.len());
        self.mistakes.insert(snap.trade_id.clone(), report);
        self.outcomes.push(rec.clone());
        Ok(rec)
    }

    /// §6.2: append the outcome (separately; the snapshot is untouched),
    /// classify it (§6.3) and feed online calibration and regime tracking.
    pub fn record_outcome(&mut self, o: TradeOutcome) -> Result<OutcomeRecord> {
        let rec = self.apply_outcome(o)?;
        self.write(&JournalLine::Outcome(rec.clone()))?;
        Ok(rec)
    }

    fn regime_mut(&mut self, instrument: &str) -> &mut RegimeMonitor {
        let cfg = self.cfg.regime.clone();
        self.regimes
            .entry(instrument.to_string())
            .or_insert_with(|| RegimeMonitor::new(cfg))
    }

    pub fn observe_market(
        &mut self,
        instrument: &str,
        ts_ms: i64,
        m: MarketObs,
    ) -> Result<RegimeState> {
        let r = self.regime_mut(instrument);
        for (sig, v) in [
            (Signal::Volatility, m.volatility),
            (Signal::Spread, m.spread_bps),
            (Signal::Liquidity, m.depth),
        ] {
            if let Some(v) = v {
                r.observe(ts_ms, sig, v)?;
            }
        }
        Ok(r.state())
    }

    /// Shrink-only adjustment for the next proposal on `instrument`.
    pub fn signal(&self, instrument: &str) -> LearningSignal {
        match self.regimes.get(instrument) {
            None => LearningSignal::neutral(),
            Some(r) => {
                let s = r.state();
                let mut reasons: Vec<String> = s
                    .shifted
                    .iter()
                    .map(|x| format!("regime shift: {x:?}"))
                    .collect();
                if s.confidence_multiplier < 1.0 && reasons.is_empty() {
                    reasons.push(format!(
                        "recovering after regime shift ({} obs)",
                        s.obs_since_shift.unwrap_or(0)
                    ));
                }
                LearningSignal::new(s.confidence_multiplier, s.confidence_multiplier, reasons)
            }
        }
    }

    pub fn calibrate(&self, model_version: &str, target: &str, p: f64) -> Calibrated {
        self.calibrator
            .recalibrate(&calibration::key(model_version, target), p)
    }

    pub fn decision(&self, trade_id: &str) -> Option<&SealedDecision> {
        self.decisions.get(trade_id)
    }
    pub fn outcome(&self, trade_id: &str) -> Option<&OutcomeRecord> {
        self.by_trade.get(trade_id).map(|&i| &self.outcomes[i])
    }
    pub fn classify(&self, trade_id: &str) -> Result<&MistakeReport> {
        self.mistakes
            .get(trade_id)
            .ok_or_else(|| LearnError::UnknownTrade(trade_id.to_string()))
    }

    pub fn lesson(&self, trade_id: &str) -> Result<LessonReport> {
        let d = self
            .decisions
            .get(trade_id)
            .ok_or_else(|| LearnError::UnknownTrade(trade_id.to_string()))?;
        let o = self
            .outcome(trade_id)
            .ok_or_else(|| LearnError::UnknownTrade(trade_id.to_string()))?;
        Ok(lesson(
            &d.record,
            o,
            self.classify(trade_id)?,
            self.registry.criteria(),
        ))
    }

    /// most recent first
    pub fn lessons(&self, filter: &Filter, limit: usize) -> Vec<LessonReport> {
        self.outcomes
            .iter()
            .rev()
            .filter(|o| {
                self.decisions
                    .get(&o.outcome.trade_id)
                    .is_some_and(|d| filter.matches(&d.record))
            })
            .take(limit)
            .filter_map(|o| self.lesson(&o.outcome.trade_id).ok())
            .collect()
    }

    fn pairs(&self) -> impl Iterator<Item = (&DecisionSnapshot, &OutcomeRecord)> {
        self.outcomes.iter().filter_map(|o| {
            self.decisions
                .get(&o.outcome.trade_id)
                .map(|d| (&d.record, o))
        })
    }

    pub fn metrics(&self, as_of_ms: i64, filter: &Filter) -> MetricsReport {
        compute(self.pairs(), as_of_ms, filter)
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }
    pub fn registry_mut(&mut self) -> &mut Registry {
        &mut self.registry
    }
    /// replace the registry (e.g. loaded from its JSON file); criteria travel with it
    pub fn set_registry(&mut self, r: Registry) {
        self.registry = r;
    }

    pub fn dashboard(&self, as_of_ms: i64, mode: Option<Mode>) -> LearningDashboard {
        let filter = Filter {
            mode,
            ..Default::default()
        };
        let decisions: Vec<&DecisionSnapshot> = self
            .decisions
            .iter()
            .map(|d| &d.record)
            .filter(|d| d.ts_ms <= as_of_ms && filter.matches(d))
            .collect();
        let pairs: Vec<_> = self
            .pairs()
            .filter(|(d, o)| o.outcome.recorded_ms <= as_of_ms && filter.matches(d))
            .collect();
        let mut mistake_counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut verdicts: BTreeMap<String, usize> = BTreeMap::new();
        let mut by_strategy: BTreeMap<(String, String), usize> = BTreeMap::new();
        for (d, _) in &pairs {
            if let Some(m) = self.mistakes.get(&d.trade_id) {
                *verdicts.entry(format!("{:?}", m.verdict)).or_default() += 1;
                for f in &m.findings {
                    *mistake_counts.entry(format!("{:?}", f.cause)).or_default() += 1;
                    *by_strategy
                        .entry((d.strategy_version.clone(), format!("{:?}", f.cause)))
                        .or_default() += 1;
                }
            }
        }
        let recent_errors = pairs
            .iter()
            .rev()
            .take(20)
            .filter_map(|(d, o)| {
                let f = d.trade_forecast()?;
                let a = o.outcome.move_at(f.horizon_s)?;
                Some(RecentError {
                    trade_id: d.trade_id.clone(),
                    instrument: d.instrument.clone(),
                    horizon_s: f.horizon_s,
                    predicted: f.expected_return,
                    actual: a,
                    error: a - f.expected_return,
                    z: (f.return_sd > 0.0).then(|| (a - f.expected_return) / f.return_sd),
                    verdict: self.mistakes.get(&d.trade_id).map(|m| m.verdict),
                })
            })
            .collect();
        let review_threshold = 3;
        let strategies_under_review = by_strategy
            .iter()
            .filter(|(_, &n)| n >= review_threshold)
            .map(|((s, c), n)| format!("{s}: {c} x{n}"))
            .collect();
        let won = pairs.iter().filter(|(_, o)| o.won).count();
        LearningDashboard {
            as_of_ms,
            mode_label: match mode {
                Some(Mode::Paper) => "PAPER",
                Some(Mode::Live) => "LIVE",
                None => "PAPER+LIVE",
            }
            .to_string(),
            n_decisions: decisions.len(),
            n_approved: decisions.iter().filter(|d| d.risk.approved).count(),
            n_rejected: decisions.iter().filter(|d| !d.risk.approved).count(),
            n_outcomes: pairs.len(),
            wins: won,
            losses: pairs.len() - won,
            metrics: compute(pairs.iter().copied(), as_of_ms, &filter),
            recent_errors,
            mistake_counts,
            verdicts,
            calibration: self
                .calibrator
                .keys()
                .filter_map(|k| self.calibrator.view(k))
                .collect(),
            champion: self.registry.champion().map(|e| e.spec.version.clone()),
            registry: self.registry.clone(),
            recent_lessons: self.lessons(&filter, 10),
            regimes: self
                .regimes
                .iter()
                .map(|(k, r)| (k.clone(), r.state()))
                .collect(),
            strategies_under_review,
        }
    }

    pub fn n_decisions(&self) -> usize {
        self.decisions.len()
    }
    pub fn n_outcomes(&self) -> usize {
        self.outcomes.len()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecentError {
    pub trade_id: String,
    pub instrument: String,
    pub horizon_s: u32,
    pub predicted: f64,
    pub actual: f64,
    pub error: f64,
    pub z: Option<f64>,
    pub verdict: Option<Verdict>,
}

/// JSON payload for the dashboard's mandatory learning-and-mistakes section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LearningDashboard {
    pub as_of_ms: i64,
    pub mode_label: String,
    pub n_decisions: usize,
    pub n_approved: usize,
    pub n_rejected: usize,
    pub n_outcomes: usize,
    pub wins: usize,
    pub losses: usize,
    /// prediction vs actual per horizon, Brier/log loss/ECE, MFE/MAE, cost errors
    pub metrics: MetricsReport,
    pub recent_errors: Vec<RecentError>,
    pub mistake_counts: BTreeMap<String, usize>,
    pub verdicts: BTreeMap<String, usize>,
    pub calibration: Vec<TrackerView>,
    pub champion: Option<String>,
    /// versions, evals, promotion/rollback history, criteria
    pub registry: Registry,
    pub recent_lessons: Vec<LessonReport>,
    pub regimes: BTreeMap<String, RegimeState>,
    /// strategy versions with >= 3 findings of one cause
    pub strategies_under_review: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outcome::tests::outcome;
    use crate::snapshot::tests::snap;

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("qe-learn-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&d);
        d
    }

    fn run(e: &mut LearningEngine, n: i64) {
        for i in 0..n {
            let ts = 1_000 + i * 200_000;
            let id = format!("t{i}");
            e.record_decision(snap(&id, ts)).unwrap();
            let (px, why, mv) = if i % 3 == 0 {
                ("99.5", ExitReason::StopLoss, -0.004)
            } else {
                ("100.6", ExitReason::TakeProfit, 0.006)
            };
            e.record_outcome(outcome(&id, ts, px, why, mv)).unwrap();
        }
    }

    #[test]
    fn end_to_end_record_classify_metrics_lessons_dashboard() {
        let mut e = LearningEngine::new(EngineConfig::default());
        run(&mut e, 9);
        assert_eq!(e.n_outcomes(), 9);
        assert!(e.classify("t0").unwrap().verdict == Verdict::Classified);
        let m = e.metrics(i64::MAX, &Filter::default());
        assert_eq!(m.n_trades, 9);
        let l = e.lessons(&Filter::default(), 3);
        assert_eq!(l.len(), 3);
        assert_eq!(l[0].trade_id, "t8");
        let d = e.dashboard(i64::MAX, Some(Mode::Paper));
        assert_eq!(d.mode_label, "PAPER");
        assert_eq!(d.losses, 3);
        assert_eq!(d.mistake_counts.get("IncorrectDirection"), Some(&3));
        assert!(d
            .strategies_under_review
            .iter()
            .any(|s| s.contains("IncorrectDirection")));
        let j = serde_json::to_string(&d).unwrap();
        assert!(j.contains("\"recent_errors\""));
        assert_eq!(e.dashboard(i64::MAX, Some(Mode::Live)).n_outcomes, 0);
    }

    #[test]
    fn outcome_rules_unknown_duplicate_out_of_order() {
        let mut e = LearningEngine::new(EngineConfig::default());
        assert!(matches!(
            e.record_outcome(outcome("x", 1_000, "100.6", ExitReason::TakeProfit, 0.0)),
            Err(LearnError::UnknownTrade(_))
        ));
        e.record_decision(snap("a", 1_000)).unwrap();
        e.record_decision(snap("b", 2_000)).unwrap();
        e.record_outcome(outcome("b", 2_000, "100.6", ExitReason::TakeProfit, 0.0))
            .unwrap();
        assert!(matches!(
            e.record_outcome(outcome("b", 2_000, "100.6", ExitReason::TakeProfit, 0.0)),
            Err(LearnError::Duplicate(_))
        ));
        // a's record time (122 000) precedes b's (123 000): refused
        assert!(matches!(
            e.record_outcome(outcome("a", 1_000, "100.6", ExitReason::TakeProfit, 0.0)),
            Err(LearnError::NotChronological { .. })
        ));
        // the pre-trade record is untouched by outcomes
        assert!(e.decision("b").unwrap().verify());
    }

    #[test]
    fn journal_replays_identically_and_detects_tampering() {
        let p = scratch("journal.jsonl");
        {
            let mut e = LearningEngine::open(EngineConfig::default(), &p).unwrap();
            run(&mut e, 5);
        }
        let e = LearningEngine::open(EngineConfig::default(), &p).unwrap();
        assert_eq!((e.n_decisions(), e.n_outcomes()), (5, 5));
        assert_eq!(e.classify("t0").unwrap().verdict, Verdict::Classified);
        let text = std::fs::read_to_string(&p).unwrap();
        std::fs::write(
            &p,
            text.replacen("\"p_tp_first\":0.55", "\"p_tp_first\":0.75", 1),
        )
        .unwrap();
        assert!(matches!(
            LearningEngine::open(EngineConfig::default(), &p),
            Err(LearnError::Tampered(_))
        ));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn regime_shift_shrinks_the_signal_only() {
        let mut e = LearningEngine::new(EngineConfig::default());
        assert_eq!(e.signal("X").confidence_multiplier(), 1.0);
        for i in 0..300 {
            e.observe_market(
                "X",
                i,
                MarketObs {
                    spread_bps: Some(1.0 + (i % 7) as f64 * 0.01),
                    ..Default::default()
                },
            )
            .unwrap();
        }
        assert_eq!(e.signal("X").confidence_multiplier(), 1.0);
        for i in 300..320 {
            e.observe_market(
                "X",
                i,
                MarketObs {
                    spread_bps: Some(8.0),
                    ..Default::default()
                },
            )
            .unwrap();
        }
        let s = e.signal("X");
        assert!(s.confidence_multiplier() < 1.0 && s.size_multiplier() < 1.0);
        assert!(!s.reasons.is_empty());
    }
}
