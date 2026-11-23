//! §6.5 B Regime adaptation: detect shifts in volatility, liquidity, spread and
//! forecast residuals, and lower confidence until enough new observations.
//!
//! Each signal keeps a slow EW mean/variance (the established regime) and a
//! fast EW mean (recent behaviour). After warm-up, a shift is declared when
//! `|fast - slow| / slow_sd > z_threshold`. Confidence then drops to `floor` and
//! recovers linearly over `recovery_obs` further observations (of any signal). The output is a
//! multiplier in `[floor, 1]`: it can only *lower* confidence, never raise it.
use crate::types::{finite, LearnError, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Signal {
    Volatility,
    Liquidity,
    Spread,
    /// standardised forecast error (actual - predicted) / sd
    Residual,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegimeConfig {
    pub slow_half_life: f64,
    pub fast_half_life: f64,
    pub warmup_obs: usize,
    pub z_threshold: f64,
    pub floor: f64,
    pub recovery_obs: usize,
}

impl Default for RegimeConfig {
    fn default() -> Self {
        RegimeConfig {
            slow_half_life: 300.0,
            fast_half_life: 20.0,
            warmup_obs: 60,
            z_threshold: 2.0,
            floor: 0.5,
            recovery_obs: 50,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Track {
    n: usize,
    last_ms: i64,
    slow_mean: f64,
    slow_var: f64,
    fast_mean: f64,
    z: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegimeState {
    pub confidence_multiplier: f64,
    /// signals currently beyond the threshold
    pub shifted: Vec<Signal>,
    pub z: BTreeMap<Signal, f64>,
    pub obs_since_shift: Option<usize>,
    pub warmed_up: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegimeMonitor {
    pub cfg: RegimeConfig,
    tracks: BTreeMap<Signal, Track>,
    obs_since_shift: Option<usize>,
}

fn alpha(h: f64) -> f64 {
    1.0 - 0.5f64.powf(1.0 / h.max(1.0))
}

impl RegimeMonitor {
    pub fn new(cfg: RegimeConfig) -> Self {
        RegimeMonitor {
            cfg,
            tracks: BTreeMap::new(),
            obs_since_shift: None,
        }
    }

    pub fn observe(&mut self, ts_ms: i64, sig: Signal, x: f64) -> Result<RegimeState> {
        finite("regime signal", x)?;
        let (a_s, a_f) = (
            alpha(self.cfg.slow_half_life),
            alpha(self.cfg.fast_half_life),
        );
        let warm = self.cfg.warmup_obs;
        let t = self.tracks.entry(sig).or_default();
        // chronological per signal (market data and trade residuals arrive on
        // different clocks)
        if t.n > 0 && ts_ms < t.last_ms {
            return Err(LearnError::NotChronological {
                last_ms: t.last_ms,
                got_ms: ts_ms,
            });
        }
        t.last_ms = ts_ms;
        if t.n == 0 {
            t.slow_mean = x;
            t.fast_mean = x;
        } else {
            // z against the established regime, measured before absorbing x
            t.fast_mean += a_f * (x - t.fast_mean);
            let sd = t.slow_var.sqrt();
            t.z = if t.n >= warm && sd > 0.0 {
                (t.fast_mean - t.slow_mean) / sd
            } else {
                0.0
            };
            let d = x - t.slow_mean;
            t.slow_mean += a_s * d;
            t.slow_var = (1.0 - a_s) * (t.slow_var + a_s * d * d);
        }
        t.n += 1;
        let shifted_now = t.z.abs() > self.cfg.z_threshold;
        if shifted_now {
            self.obs_since_shift = Some(0);
        } else if let Some(k) = self.obs_since_shift.as_mut() {
            *k += 1;
            if *k >= self.cfg.recovery_obs {
                self.obs_since_shift = None;
            }
        }
        Ok(self.state())
    }

    pub fn state(&self) -> RegimeState {
        let shifted: Vec<Signal> = self
            .tracks
            .iter()
            .filter(|(_, t)| t.z.abs() > self.cfg.z_threshold)
            .map(|(s, _)| *s)
            .collect();
        let conf = match self.obs_since_shift {
            None => 1.0,
            Some(k) => {
                let f = self.cfg.floor.clamp(0.0, 1.0);
                f + (1.0 - f) * (k as f64 / self.cfg.recovery_obs.max(1) as f64).min(1.0)
            }
        };
        RegimeState {
            confidence_multiplier: if shifted.is_empty() {
                conf
            } else {
                self.cfg.floor.clamp(0.0, 1.0)
            },
            z: self.tracks.iter().map(|(s, t)| (*s, t.z)).collect(),
            shifted,
            obs_since_shift: self.obs_since_shift,
            warmed_up: self.tracks.values().any(|t| t.n >= self.cfg.warmup_obs),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(i: usize) -> f64 {
        // deterministic pseudo-noise in [-0.5, 0.5)
        ((i as u64).wrapping_mul(2654435761) % 1000) as f64 / 1000.0 - 0.5
    }

    #[test]
    fn steady_signal_keeps_full_confidence() {
        let mut m = RegimeMonitor::new(RegimeConfig::default());
        let mut st = None;
        for i in 0..500 {
            st = Some(
                m.observe(i as i64, Signal::Spread, 2.0 + 0.2 * noise(i))
                    .unwrap(),
            );
        }
        let st = st.unwrap();
        assert!(st.warmed_up);
        assert_eq!(st.confidence_multiplier, 1.0, "{st:?}");
    }

    #[test]
    fn volatility_jump_lowers_confidence_then_recovers() {
        let cfg = RegimeConfig::default();
        let mut m = RegimeMonitor::new(cfg.clone());
        for i in 0..400 {
            m.observe(i as i64, Signal::Volatility, 1.0 + 0.1 * noise(i))
                .unwrap();
        }
        let mut low = 1.0f64;
        for i in 400..430 {
            let s = m
                .observe(i as i64, Signal::Volatility, 3.0 + 0.1 * noise(i))
                .unwrap();
            low = low.min(s.confidence_multiplier);
        }
        assert_eq!(low, cfg.floor);
        // the new level becomes the regime; confidence returns only after recovery_obs
        let mut last = None;
        for i in 430..2000 {
            last = Some(
                m.observe(i as i64, Signal::Volatility, 3.0 + 0.1 * noise(i))
                    .unwrap(),
            );
        }
        assert_eq!(last.unwrap().confidence_multiplier, 1.0);
    }

    #[test]
    fn multiplier_never_exceeds_one_and_rejects_time_travel() {
        let mut m = RegimeMonitor::new(RegimeConfig::default());
        for i in 0..100 {
            let s = m
                .observe(i, Signal::Residual, noise(i as usize) * 10.0)
                .unwrap();
            assert!(s.confidence_multiplier <= 1.0);
        }
        assert!(m.observe(5, Signal::Residual, 0.0).is_err());
        assert!(m.observe(200, Signal::Residual, f64::NAN).is_err());
    }
}
