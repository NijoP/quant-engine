//! Seeded paper fill model: exchange rejections, partial entry fills and simulated
//! order latency, plus latency statistics (decision -> simulated fill).
//!
//! Exits always fill in full in this model (the lifecycle's reconcile path is what
//! handles an unfilled exit); entry partial fills round down to the quantity
//! increment and are rejected when that falls below the minimum quantity.

use crate::frequency::Rng;
use serde::Serialize;
use std::collections::VecDeque;

#[derive(Debug, Clone, Serialize)]
pub struct FillModelParams {
    pub p_reject: f64,
    pub p_partial: f64,
    /// Filled fraction on a partial fill is uniform in [min_partial_frac, 1).
    pub min_partial_frac: f64,
    pub latency_ms_min: u64,
    pub latency_ms_max: u64,
    pub seed: u64,
}

impl Default for FillModelParams {
    fn default() -> Self {
        Self {
            p_reject: 0.03,
            p_partial: 0.15,
            min_partial_frac: 0.3,
            latency_ms_min: 40,
            latency_ms_max: 250,
            seed: 0x5133_4445_534b,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum SimFill {
    Rejected {
        reason: String,
        latency_ms: u64,
    },
    Filled {
        qty: f64,
        partial: bool,
        latency_ms: u64,
    },
}

#[derive(Debug, Clone)]
pub struct FillModel {
    pub params: FillModelParams,
    rng: Rng,
    pub entries: u64,
    pub rejected: u64,
    pub partial: u64,
}

impl FillModel {
    pub fn new(params: FillModelParams) -> Self {
        let rng = Rng(params.seed);
        Self {
            params,
            rng,
            entries: 0,
            rejected: 0,
            partial: 0,
        }
    }

    fn latency(&mut self) -> u64 {
        let (a, b) = (
            self.params.latency_ms_min,
            self.params.latency_ms_max.max(self.params.latency_ms_min),
        );
        a + ((b - a) as f64 * self.rng.f64()) as u64
    }

    /// Outcome of an entry order for `qty` (already on the quantity increment).
    pub fn entry(&mut self, qty: f64, qty_increment: f64, min_qty: f64) -> SimFill {
        self.entries += 1;
        let latency_ms = self.latency();
        if self.rng.f64() < self.params.p_reject {
            self.rejected += 1;
            return SimFill::Rejected {
                reason: "simulated exchange rejection".into(),
                latency_ms,
            };
        }
        if self.rng.f64() < self.params.p_partial {
            let lo = self.params.min_partial_frac.clamp(0.0, 1.0);
            let frac = lo + (1.0 - lo) * self.rng.f64();
            let inc = if qty_increment > 0.0 {
                qty_increment
            } else {
                qty
            };
            let filled = ((qty * frac / inc).floor() * inc * 1e12).round() / 1e12;
            if filled + 1e-12 < min_qty.max(inc) || filled <= 0.0 {
                self.rejected += 1;
                return SimFill::Rejected {
                    reason: format!("simulated partial fill {filled} below minimum quantity; remainder cancelled"),
                    latency_ms,
                };
            }
            if filled + 1e-12 < qty {
                self.partial += 1;
                return SimFill::Filled {
                    qty: filled,
                    partial: true,
                    latency_ms,
                };
            }
        }
        SimFill::Filled {
            qty,
            partial: false,
            latency_ms,
        }
    }

    pub fn exit_latency(&mut self) -> u64 {
        self.latency()
    }
}

/// Rolling latency samples in ms.
#[derive(Debug, Clone, Default)]
pub struct LatencyStats {
    samples: VecDeque<f64>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LatencySummary {
    pub n: usize,
    pub mean_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub max_ms: f64,
}

impl LatencyStats {
    pub fn push(&mut self, ms: f64) {
        self.samples.push_back(ms);
        if self.samples.len() > 2_000 {
            self.samples.pop_front();
        }
    }
    pub fn summary(&self) -> Option<LatencySummary> {
        if self.samples.is_empty() {
            return None;
        }
        let mut v: Vec<f64> = self.samples.iter().copied().collect();
        v.sort_by(|a, b| a.total_cmp(b));
        let q = |f: f64| v[((v.len() - 1) as f64 * f).round() as usize];
        Some(LatencySummary {
            n: v.len(),
            mean_ms: v.iter().sum::<f64>() / v.len() as f64,
            p50_ms: q(0.5),
            p95_ms: q(0.95),
            max_ms: v[v.len() - 1],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeded_rates_and_increments() {
        let mut m = FillModel::new(FillModelParams::default());
        let mut parts = 0;
        for _ in 0..5_000 {
            match m.entry(1.0, 0.001, 0.001) {
                SimFill::Filled {
                    qty,
                    partial,
                    latency_ms,
                } => {
                    assert!(qty <= 1.0 && qty > 0.0);
                    assert!((qty / 0.001 - (qty / 0.001).round()).abs() < 1e-6);
                    assert!((40..=250).contains(&latency_ms));
                    parts += partial as u32;
                }
                SimFill::Rejected { .. } => {}
            }
        }
        let rej = m.rejected as f64 / 5_000.0;
        assert!((0.015..0.05).contains(&rej), "{rej}");
        assert!(parts > 500, "{parts}");
    }

    #[test]
    fn partial_below_min_qty_is_rejected() {
        let mut m = FillModel::new(FillModelParams {
            p_reject: 0.0,
            p_partial: 1.0,
            min_partial_frac: 0.3,
            ..Default::default()
        });
        // qty equals the minimum: any partial is below it
        assert!(matches!(
            m.entry(0.001, 0.001, 0.001),
            SimFill::Rejected { .. }
        ));
    }

    #[test]
    fn latency_summary() {
        let mut s = LatencyStats::default();
        assert!(s.summary().is_none());
        for x in 1..=100 {
            s.push(x as f64);
        }
        let r = s.summary().unwrap();
        assert_eq!(r.n, 100);
        assert_eq!(r.max_ms, 100.0);
        assert!((r.mean_ms - 50.5).abs() < 1e-9);
    }
}
