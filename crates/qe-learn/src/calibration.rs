//! §6.5 A Online calibration: exponentially weighted reliability bins plus an
//! exact rolling window, with minimum-sample guards.
//!
//! * Updates must arrive in time order (per key); an older timestamp is refused.
//! * `recalibrate(p)` returns the raw probability unchanged until the key has
//!   `min_total` observations *and* p's bin has an effective count of at least
//!   `min_bin_n_eff`: small samples are not stable and are never used to move a
//!   probability. Once stable, the bin's EW hit rate is shrunk toward `p` with
//!   `prior_strength` pseudo-observations.
//! * The calibrator never touches size, leverage or stops (see [`crate::safety`]).
use crate::types::{LearnError, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalibConfig {
    pub bins: usize,
    /// EW half-life in observations
    pub half_life_obs: f64,
    /// exact rolling window length
    pub window: usize,
    pub min_total: usize,
    pub min_bin_n_eff: f64,
    pub prior_strength: f64,
}

impl Default for CalibConfig {
    fn default() -> Self {
        CalibConfig {
            bins: 10,
            half_life_obs: 200.0,
            window: 500,
            min_total: 100,
            min_bin_n_eff: 30.0,
            prior_strength: 20.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalStatus {
    /// fewer than `min_total` observations: raw probability, no authority from calibration
    Insufficient,
    /// enough in total, but this bin is below `min_bin_n_eff`
    Calibrating,
    Stable,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Calibrated {
    pub raw: f64,
    pub calibrated: f64,
    pub status: CalStatus,
    pub bin_n_eff: f64,
    pub bin_rate: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BinView {
    pub lo: f64,
    pub hi: f64,
    pub n_eff: f64,
    pub mean_p: Option<f64>,
    pub hit_rate: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackerView {
    pub key: String,
    pub n_total: usize,
    pub rolling_n: usize,
    pub rolling_brier: Option<f64>,
    pub rolling_ece: Option<f64>,
    pub bins: Vec<BinView>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Tracker {
    last_ms: i64,
    n_total: usize,
    w: Vec<f64>,
    hit: Vec<f64>,
    psum: Vec<f64>,
    window: VecDeque<(f64, bool)>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OnlineCalibrator {
    pub cfg: CalibConfig,
    trackers: BTreeMap<String, Tracker>,
}

/// key convention: `"{model_version}/{target}"`, target e.g. `tp_first`, `win`, `up_120s`
pub fn key(model_version: &str, target: &str) -> String {
    format!("{model_version}/{target}")
}

impl OnlineCalibrator {
    pub fn new(cfg: CalibConfig) -> Self {
        OnlineCalibrator {
            cfg,
            trackers: BTreeMap::new(),
        }
    }
    fn bin(&self, p: f64) -> usize {
        ((p.clamp(0.0, 1.0) * self.cfg.bins as f64) as usize).min(self.cfg.bins - 1)
    }

    pub fn update(&mut self, key: &str, ts_ms: i64, p: f64, y: bool) -> Result<()> {
        crate::types::prob("p", p)?;
        let b = self.bin(p);
        let decay = 0.5f64.powf(1.0 / self.cfg.half_life_obs.max(1.0));
        let (bins, window) = (self.cfg.bins, self.cfg.window);
        let t = self
            .trackers
            .entry(key.to_string())
            .or_insert_with(|| Tracker {
                w: vec![0.0; bins],
                hit: vec![0.0; bins],
                psum: vec![0.0; bins],
                ..Default::default()
            });
        if t.n_total > 0 && ts_ms < t.last_ms {
            return Err(LearnError::NotChronological {
                last_ms: t.last_ms,
                got_ms: ts_ms,
            });
        }
        for i in 0..bins {
            t.w[i] *= decay;
            t.hit[i] *= decay;
            t.psum[i] *= decay;
        }
        t.w[b] += 1.0;
        t.psum[b] += p;
        if y {
            t.hit[b] += 1.0;
        }
        t.window.push_back((p, y));
        while t.window.len() > window {
            t.window.pop_front();
        }
        t.n_total += 1;
        t.last_ms = ts_ms;
        Ok(())
    }

    pub fn recalibrate(&self, key: &str, p: f64) -> Calibrated {
        let b = self.bin(p);
        let raw = Calibrated {
            raw: p,
            calibrated: p,
            status: CalStatus::Insufficient,
            bin_n_eff: 0.0,
            bin_rate: None,
        };
        let Some(t) = self.trackers.get(key) else {
            return raw;
        };
        let n = t.w[b];
        let rate = (n > 0.0).then(|| t.hit[b] / n);
        if t.n_total < self.cfg.min_total {
            return Calibrated {
                bin_n_eff: n,
                bin_rate: rate,
                ..raw
            };
        }
        if n < self.cfg.min_bin_n_eff {
            return Calibrated {
                status: CalStatus::Calibrating,
                bin_n_eff: n,
                bin_rate: rate,
                ..raw
            };
        }
        let k = self.cfg.prior_strength;
        Calibrated {
            raw: p,
            calibrated: (t.hit[b] + k * p) / (n + k),
            status: CalStatus::Stable,
            bin_n_eff: n,
            bin_rate: rate,
        }
    }

    pub fn view(&self, key: &str) -> Option<TrackerView> {
        let t = self.trackers.get(key)?;
        let (p, y): (Vec<f64>, Vec<bool>) = t.window.iter().copied().unzip();
        let width = 1.0 / self.cfg.bins as f64;
        Some(TrackerView {
            key: key.to_string(),
            n_total: t.n_total,
            rolling_n: p.len(),
            rolling_brier: qe_math::prob::brier(&p, &y),
            rolling_ece: qe_math::prob::ece(&p, &y, self.cfg.bins),
            bins: (0..self.cfg.bins)
                .map(|i| BinView {
                    lo: i as f64 * width,
                    hi: (i + 1) as f64 * width,
                    n_eff: t.w[i],
                    mean_p: (t.w[i] > 0.0).then(|| t.psum[i] / t.w[i]),
                    hit_rate: (t.w[i] > 0.0).then(|| t.hit[i] / t.w[i]),
                })
                .collect(),
        })
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.trackers.keys()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_samples_never_move_the_probability() {
        let mut c = OnlineCalibrator::new(CalibConfig::default());
        for i in 0..50 {
            c.update("m/tp", i, 0.7, false).unwrap();
        }
        let r = c.recalibrate("m/tp", 0.7);
        assert_eq!(r.status, CalStatus::Insufficient);
        assert_eq!(r.calibrated, 0.7);
    }

    #[test]
    fn stable_bin_moves_toward_empirical_rate() {
        let mut c = OnlineCalibrator::new(CalibConfig::default());
        // p = 0.7 is over-confident: true rate 0.4
        for i in 0..300 {
            c.update("m/tp", i, 0.72, i % 5 < 2).unwrap();
        }
        let r = c.recalibrate("m/tp", 0.72);
        assert_eq!(r.status, CalStatus::Stable);
        assert!(r.calibrated < 0.5 && r.calibrated > 0.38, "{r:?}");
        // an empty bin stays raw even when the key is mature
        let other = c.recalibrate("m/tp", 0.15);
        assert_eq!(other.status, CalStatus::Calibrating);
        assert_eq!(other.calibrated, 0.15);
        let v = c.view("m/tp").unwrap();
        assert_eq!(v.rolling_n, 300);
        assert!(v.rolling_brier.unwrap() > 0.0);
    }

    #[test]
    fn rejects_out_of_order_updates() {
        let mut c = OnlineCalibrator::new(CalibConfig::default());
        c.update("k", 10, 0.5, true).unwrap();
        assert!(c.update("k", 9, 0.5, true).is_err());
        assert!(c.update("k", 11, 1.5, true).is_err());
    }

    #[test]
    fn ew_forgets_old_regime() {
        let cfg = CalibConfig {
            half_life_obs: 50.0,
            ..Default::default()
        };
        let mut c = OnlineCalibrator::new(cfg);
        for i in 0..400 {
            c.update("k", i, 0.55, true).unwrap();
        }
        for i in 400..800 {
            c.update("k", i, 0.55, false).unwrap();
        }
        let r = c.recalibrate("k", 0.55);
        assert!(r.bin_rate.unwrap() < 0.01, "{r:?}");
    }
}
