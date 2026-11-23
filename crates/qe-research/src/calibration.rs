//! Chronological walk-forward scoring and the calibration-status registry.
//!
//! # No-lookahead protocol
//! Points `(t_ms, p, y)` are sorted by decision time `t`. The time span is cut
//! into `folds` equal chronological slices. Fold 0 is training only. For fold
//! k >= 1 the recalibration map and the climatology baseline are fitted on
//! points with `t + label_horizon_ms < fold_k_start` (a purge gap, so no label
//! that overlaps the test period is ever used for fitting) and evaluated on
//! fold k. Out-of-sample (OOS) metrics aggregate folds 1..K only.
//!
//! # Status rule (per model id / target)
//! * `uncalibrated` — no OOS observations.
//! * `calibrating`  — OOS observations exist but at least one gate fails.
//! * `calibrated`   — n_oos >= [`MIN_OOS`], >= 3 OOS folds, ECE of the
//!   published (recalibrated) mapping <= [`MAX_ECE`], and positive Brier skill
//!   against the training-fold climatology.
//!
//! Only `calibrated` grants forecast authority ([`CalibrationReport::grants_authority`]).
use qe_math::prob::{brier, ece, log_loss};
use serde::{Deserialize, Serialize};

pub const MIN_OOS: usize = 1_000;
pub const MAX_ECE: f64 = 0.03;
pub const BINS: usize = 10;
/// pseudo-count shrinking each recalibration bin toward the training base rate
pub const SHRINK: f64 = 5.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub t_ms: i64,
    pub p: f64,
    pub y: bool,
}

/// Histogram-binning recalibration: p -> smoothed empirical frequency of its bin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BinMap {
    /// calibrated probability per equal-width bin of the raw probability
    pub p_cal: Vec<f64>,
    pub n: Vec<usize>,
    pub base_rate: f64,
}

impl BinMap {
    pub fn fit(pts: &[Point]) -> Option<BinMap> {
        if pts.is_empty() {
            return None;
        }
        let base = pts.iter().filter(|x| x.y).count() as f64 / pts.len() as f64;
        let mut sy = vec![0.0; BINS];
        let mut n = vec![0usize; BINS];
        for x in pts {
            let b = bin(x.p);
            n[b] += 1;
            if x.y {
                sy[b] += 1.0;
            }
        }
        let p_cal = (0..BINS)
            .map(|b| (sy[b] + SHRINK * base) / (n[b] as f64 + SHRINK))
            .collect();
        Some(BinMap {
            p_cal,
            n,
            base_rate: base,
        })
    }
    pub fn apply(&self, p: f64) -> f64 {
        self.p_cal[bin(p)]
    }
}

fn bin(p: f64) -> usize {
    ((p.clamp(0.0, 1.0) * BINS as f64) as usize).min(BINS - 1)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FoldMetrics {
    pub start_ms: i64,
    pub end_ms: i64,
    pub n_train: usize,
    pub n_test: usize,
    pub base_rate_test: f64,
    pub brier_raw: f64,
    pub brier_recal: f64,
    pub brier_clim: f64,
    pub log_loss_raw: f64,
    pub log_loss_recal: f64,
    pub ece_raw: f64,
    pub ece_recal: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReliabilityBin {
    pub lo: f64,
    pub hi: f64,
    pub n: usize,
    pub mean_p: f64,
    pub freq_y: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WalkForward {
    pub n_total: usize,
    pub folds: Vec<FoldMetrics>,
    pub n_oos: usize,
    pub oos_folds: usize,
    pub base_rate_oos: f64,
    pub mean_p_raw_oos: f64,
    pub brier_raw: f64,
    pub brier_recal: f64,
    pub brier_clim: f64,
    /// 1 - brier_recal / brier_clim (> 0 means better than climatology)
    pub brier_skill_recal: f64,
    pub brier_skill_raw: f64,
    pub log_loss_raw: f64,
    pub log_loss_recal: f64,
    pub ece_raw: f64,
    pub ece_recal: f64,
    pub reliability_raw: Vec<ReliabilityBin>,
}

fn reliability(p: &[f64], y: &[bool]) -> Vec<ReliabilityBin> {
    let mut sp = vec![0.0; BINS];
    let mut sy = vec![0.0; BINS];
    let mut n = vec![0usize; BINS];
    for (&pi, &yi) in p.iter().zip(y) {
        let b = bin(pi);
        sp[b] += pi;
        sy[b] += if yi { 1.0 } else { 0.0 };
        n[b] += 1;
    }
    (0..BINS)
        .filter(|&b| n[b] > 0)
        .map(|b| ReliabilityBin {
            lo: b as f64 / BINS as f64,
            hi: (b + 1) as f64 / BINS as f64,
            n: n[b],
            mean_p: sp[b] / n[b] as f64,
            freq_y: sy[b] / n[b] as f64,
        })
        .collect()
}

/// Chronological walk-forward evaluation (see module docs).
pub fn walk_forward(points: &[Point], folds: usize, label_horizon_ms: i64) -> Option<WalkForward> {
    let mut pts: Vec<Point> = points.iter().copied().filter(|x| x.p.is_finite()).collect();
    if pts.len() < 2 || folds < 2 {
        return None;
    }
    pts.sort_by_key(|x| x.t_ms);
    let (t0, t1) = (pts[0].t_ms, pts[pts.len() - 1].t_ms + 1);
    let span = (t1 - t0) as f64 / folds as f64;
    let mut out_folds = vec![];
    let (mut op, mut oc, mut oy, mut ocl) = (vec![], vec![], vec![], vec![]);
    for k in 1..folds {
        let a = t0 + (span * k as f64) as i64;
        let b = if k + 1 == folds {
            t1
        } else {
            t0 + (span * (k + 1) as f64) as i64
        };
        let train: Vec<Point> = pts
            .iter()
            .copied()
            .filter(|x| x.t_ms + label_horizon_ms < a)
            .collect();
        let test: Vec<Point> = pts
            .iter()
            .copied()
            .filter(|x| x.t_ms >= a && x.t_ms < b)
            .collect();
        let Some(map) = BinMap::fit(&train) else {
            continue;
        };
        if test.is_empty() {
            continue;
        }
        let p: Vec<f64> = test.iter().map(|x| x.p).collect();
        let c: Vec<f64> = p.iter().map(|&x| map.apply(x)).collect();
        let cl: Vec<f64> = vec![map.base_rate; test.len()];
        let y: Vec<bool> = test.iter().map(|x| x.y).collect();
        out_folds.push(FoldMetrics {
            start_ms: a,
            end_ms: b,
            n_train: train.len(),
            n_test: test.len(),
            base_rate_test: y.iter().filter(|v| **v).count() as f64 / y.len() as f64,
            brier_raw: brier(&p, &y)?,
            brier_recal: brier(&c, &y)?,
            brier_clim: brier(&cl, &y)?,
            log_loss_raw: log_loss(&p, &y)?,
            log_loss_recal: log_loss(&c, &y)?,
            ece_raw: ece(&p, &y, BINS)?,
            ece_recal: ece(&c, &y, BINS)?,
        });
        op.extend(p);
        oc.extend(c);
        oy.extend(y);
        ocl.extend(cl);
    }
    let n_oos = oy.len();
    let (br, bc, bcl) = if n_oos > 0 {
        (brier(&op, &oy)?, brier(&oc, &oy)?, brier(&ocl, &oy)?)
    } else {
        (f64::NAN, f64::NAN, f64::NAN)
    };
    let skill = |b: f64| if bcl > 0.0 { 1.0 - b / bcl } else { f64::NAN };
    Some(WalkForward {
        n_total: pts.len(),
        oos_folds: out_folds.len(),
        folds: out_folds,
        n_oos,
        base_rate_oos: if n_oos > 0 {
            oy.iter().filter(|v| **v).count() as f64 / n_oos as f64
        } else {
            f64::NAN
        },
        mean_p_raw_oos: if n_oos > 0 {
            op.iter().sum::<f64>() / n_oos as f64
        } else {
            f64::NAN
        },
        brier_raw: br,
        brier_recal: bc,
        brier_clim: bcl,
        brier_skill_recal: skill(bc),
        brier_skill_raw: skill(br),
        log_loss_raw: log_loss(&op, &oy).unwrap_or(f64::NAN),
        log_loss_recal: log_loss(&oc, &oy).unwrap_or(f64::NAN),
        ece_raw: ece(&op, &oy, BINS).unwrap_or(f64::NAN),
        ece_recal: ece(&oc, &oy, BINS).unwrap_or(f64::NAN),
        reliability_raw: reliability(&op, &oy),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Uncalibrated,
    Calibrating,
    Calibrated,
}

/// One registry row: a model's probability for one target/horizon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalibrationEntry {
    pub model_id: String,
    /// e.g. `tp_first_long`, `up_60s`
    pub target: String,
    pub horizon_s: f64,
    pub dataset: String,
    pub status: Status,
    pub n_oos: usize,
    pub reasons: Vec<String>,
    pub walk_forward: Option<WalkForward>,
    /// recalibration map fitted on ALL evaluated data, for forward use only
    pub recalibration: Option<BinMap>,
}

impl CalibrationEntry {
    pub fn from_points(
        model_id: &str,
        target: &str,
        horizon_s: f64,
        dataset: &str,
        pts: &[Point],
        folds: usize,
    ) -> Self {
        let wf = walk_forward(pts, folds, (horizon_s * 1000.0) as i64);
        let mut reasons = vec![];
        let status = match &wf {
            None => {
                reasons.push("no observations".into());
                Status::Uncalibrated
            }
            Some(w) if w.n_oos == 0 => {
                reasons.push("no out-of-sample observations".into());
                Status::Uncalibrated
            }
            Some(w) => {
                if w.n_oos < MIN_OOS {
                    reasons.push(format!("n_oos {} < {MIN_OOS}", w.n_oos));
                }
                if w.oos_folds < 3 {
                    reasons.push(format!("oos folds {} < 3", w.oos_folds));
                }
                if !(w.ece_recal <= MAX_ECE) {
                    reasons.push(format!("ECE(recal) {:.4} > {MAX_ECE}", w.ece_recal));
                }
                if !(w.brier_skill_recal > 0.0) {
                    reasons.push(format!(
                        "Brier skill vs climatology {:.5} <= 0 (no information beyond base rate)",
                        w.brier_skill_recal
                    ));
                }
                if reasons.is_empty() {
                    Status::Calibrated
                } else {
                    Status::Calibrating
                }
            }
        };
        CalibrationEntry {
            model_id: model_id.into(),
            target: target.into(),
            horizon_s,
            dataset: dataset.into(),
            status,
            n_oos: wf.as_ref().map_or(0, |w| w.n_oos),
            reasons,
            walk_forward: wf,
            recalibration: BinMap::fit(pts),
        }
    }
}

/// The artefact s2-desk reads (`calibration.json`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CalibrationReport {
    pub generated_ms: i64,
    pub entries: Vec<CalibrationEntry>,
    /// dataset descriptions, trading metrics and cost sensitivity (free-form JSON)
    #[serde(default)]
    pub evaluation: serde_json::Value,
}

impl CalibrationReport {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        Ok(serde_json::from_slice(&std::fs::read(path)?)?)
    }
    pub fn entry(&self, model_id: &str, target: &str) -> Option<&CalibrationEntry> {
        self.entries
            .iter()
            .find(|e| e.model_id == model_id && e.target == target)
    }
    /// Status for a model/target; unknown pairs are `Uncalibrated`.
    pub fn status(&self, model_id: &str, target: &str) -> Status {
        self.entry(model_id, target)
            .map_or(Status::Uncalibrated, |e| e.status)
    }
    /// True only for `Calibrated`. Everything else must be treated as advisory.
    pub fn grants_authority(&self, model_id: &str, target: &str) -> bool {
        self.status(model_id, target) == Status::Calibrated
    }
    /// Recalibrated probability, only when the entry is `Calibrated`.
    pub fn recalibrate(&self, model_id: &str, target: &str, p: f64) -> Option<f64> {
        let e = self.entry(model_id, target)?;
        (e.status == Status::Calibrated)
            .then(|| e.recalibration.as_ref().map(|m| m.apply(p)))
            .flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcg(seed: &mut u64) -> f64 {
        *seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (*seed >> 11) as f64 / (1u64 << 53) as f64
    }

    #[test]
    fn calibrated_informative_model_earns_status() {
        let mut s = 7u64;
        let pts: Vec<Point> = (0..20_000)
            .map(|i| {
                let p = lcg(&mut s);
                Point {
                    t_ms: i * 1000,
                    p,
                    y: lcg(&mut s) < p,
                }
            })
            .collect();
        let e = CalibrationEntry::from_points("m", "t", 1.0, "synthetic", &pts, 5);
        assert_eq!(e.status, Status::Calibrated, "{:?}", e.reasons);
        let w = e.walk_forward.unwrap();
        assert!(w.brier_skill_recal > 0.2 && w.ece_raw < 0.03);
    }

    #[test]
    fn uninformative_model_is_not_calibrated() {
        let mut s = 9u64;
        let pts: Vec<Point> = (0..20_000)
            .map(|i| Point {
                t_ms: i * 1000,
                p: lcg(&mut s),
                y: lcg(&mut s) < 0.3,
            })
            .collect();
        let e = CalibrationEntry::from_points("m", "t", 1.0, "synthetic", &pts, 5);
        assert_eq!(e.status, Status::Calibrating);
        assert!(e.reasons.iter().any(|r| r.contains("Brier skill")));
        let r = CalibrationReport {
            entries: vec![e],
            ..Default::default()
        };
        assert!(!r.grants_authority("m", "t"));
        assert_eq!(r.status("other", "t"), Status::Uncalibrated);
        assert!(r.recalibrate("m", "t", 0.5).is_none());
    }

    #[test]
    fn training_never_sees_test_labels() {
        // labels flip regime exactly at fold boundaries: a leaky fit would look perfect
        let pts: Vec<Point> = (0..1000)
            .map(|i| Point {
                t_ms: i * 1000,
                p: 0.5,
                y: (i / 200) % 2 == 0,
            })
            .collect();
        let w = walk_forward(&pts, 5, 10_000).unwrap();
        for f in &w.folds {
            // purge: every training point ends its label window before the fold starts
            // the latest training point is t = (n_train - 1) s; its label ends before the fold
            assert!((f.n_train as i64 - 1) * 1000 + 10_000 < f.start_ms);
        }
        // regime flip means climatology from the past is wrong out of sample
        assert!(w.brier_clim > 0.2);
    }

    #[test]
    fn small_sample_is_calibrating() {
        let pts: Vec<Point> = (0..100)
            .map(|i| Point {
                t_ms: i,
                p: 0.5,
                y: i % 2 == 0,
            })
            .collect();
        let e = CalibrationEntry::from_points("m", "t", 0.0, "x", &pts, 5);
        assert_eq!(e.status, Status::Calibrating);
        assert!(e.reasons.iter().any(|r| r.starts_with("n_oos")));
        let none = CalibrationEntry::from_points("m", "t", 0.0, "x", &[], 5);
        assert_eq!(none.status, Status::Uncalibrated);
    }
}
