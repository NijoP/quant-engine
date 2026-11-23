//! Interpretable, deterministic learners for forecast challengers.
//!
//! # API (narrow, stable)
//! * [`Logistic::fit`]`(x, y, l2)` → standardized ridge logistic regression fitted by
//!   Newton/IRLS; [`Logistic::predict`]`(&row)` → P(y = 1). Coefficients are on the
//!   standardized scale and named by the caller, so they stay inspectable.
//! * [`Isotonic::fit`]`(p, y, prior_weight)` → pool-adjacent-violators monotone map
//!   from a raw probability to a calibrated one, each block shrunk toward the base
//!   rate with `prior_weight` pseudo-observations; [`Isotonic::apply`]`(p)`.
//! * [`logit`]`(p)` → the single feature used for Platt scaling with [`Logistic`].
//!
//! No randomness, no hidden state: identical inputs give identical outputs.
use crate::dist::sigmoid;
use serde::{Deserialize, Serialize};

const EPS: f64 = 1e-6;

/// logit(p) with p clamped to [1e-6, 1 - 1e-6].
pub fn logit(p: f64) -> f64 {
    let p = p.clamp(EPS, 1.0 - EPS);
    (p / (1.0 - p)).ln()
}

/// Standardized ridge logistic regression.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Logistic {
    pub mean: Vec<f64>,
    pub sd: Vec<f64>,
    /// intercept followed by one coefficient per standardized feature
    pub beta: Vec<f64>,
    pub n_train: usize,
    pub iterations: usize,
}

/// Solve A x = b (small, well conditioned) by Gaussian elimination with
/// partial pivoting. Returns None when singular.
fn solve(mut a: Vec<Vec<f64>>, mut b: Vec<f64>) -> Option<Vec<f64>> {
    let n = b.len();
    for c in 0..n {
        let piv = (c..n).max_by(|&i, &j| a[i][c].abs().total_cmp(&a[j][c].abs()))?;
        if a[piv][c].abs() < 1e-12 {
            return None;
        }
        a.swap(c, piv);
        b.swap(c, piv);
        for r in c + 1..n {
            let f = a[r][c] / a[c][c];
            for k in c..n {
                a[r][k] -= f * a[c][k];
            }
            b[r] -= f * b[c];
        }
    }
    let mut x = vec![0.0; n];
    for r in (0..n).rev() {
        let s: f64 = (r + 1..n).map(|k| a[r][k] * x[k]).sum();
        x[r] = (b[r] - s) / a[r][r];
    }
    Some(x)
}

impl Logistic {
    /// Fit on rows `x` (all the same width) and labels `y`. `l2` penalises the
    /// standardized coefficients (not the intercept). Rows with non-finite values
    /// are skipped. Returns None with fewer than 20 usable rows or one class only.
    pub fn fit(x: &[Vec<f64>], y: &[bool], l2: f64) -> Option<Logistic> {
        let d = x.first()?.len();
        let rows: Vec<(&Vec<f64>, bool)> = x
            .iter()
            .zip(y)
            .filter(|(r, _)| r.len() == d && r.iter().all(|v| v.is_finite()))
            .map(|(r, &y)| (r, y))
            .collect();
        let n = rows.len();
        let pos = rows.iter().filter(|r| r.1).count();
        if n < 20 || pos == 0 || pos == n {
            return None;
        }
        let mut mean = vec![0.0; d];
        for (r, _) in &rows {
            for j in 0..d {
                mean[j] += r[j] / n as f64;
            }
        }
        let mut sd = vec![0.0; d];
        for (r, _) in &rows {
            for j in 0..d {
                sd[j] += (r[j] - mean[j]).powi(2) / n as f64;
            }
        }
        for s in sd.iter_mut() {
            *s = if *s > 1e-18 { s.sqrt() } else { 1.0 };
        }
        let z: Vec<Vec<f64>> = rows
            .iter()
            .map(|(r, _)| {
                std::iter::once(1.0)
                    .chain((0..d).map(|j| (r[j] - mean[j]) / sd[j]))
                    .collect()
            })
            .collect();
        let mut beta = vec![0.0; d + 1];
        beta[0] = logit(pos as f64 / n as f64);
        let mut it = 0;
        for _ in 0..50 {
            it += 1;
            let mut g = vec![0.0; d + 1];
            let mut h = vec![vec![0.0; d + 1]; d + 1];
            for (zi, (_, yi)) in z.iter().zip(&rows) {
                let eta: f64 = zi.iter().zip(&beta).map(|(a, b)| a * b).sum();
                let p = sigmoid(eta);
                let w = (p * (1.0 - p)).max(1e-9);
                let e = if *yi { 1.0 } else { 0.0 } - p;
                for a in 0..=d {
                    g[a] += e * zi[a];
                    for b in a..=d {
                        h[a][b] += w * zi[a] * zi[b];
                    }
                }
            }
            for a in 0..=d {
                for b in 0..a {
                    h[a][b] = h[b][a];
                }
            }
            for j in 1..=d {
                g[j] -= l2 * beta[j];
                h[j][j] += l2;
            }
            let step = solve(h, g)?;
            let mx = step.iter().fold(0.0f64, |m, s| m.max(s.abs()));
            for (b, s) in beta.iter_mut().zip(&step) {
                *b += s;
            }
            if mx < 1e-8 {
                break;
            }
        }
        beta.iter().all(|b| b.is_finite()).then_some(Logistic {
            mean,
            sd,
            beta,
            n_train: n,
            iterations: it,
        })
    }

    /// P(y = 1 | row). A non-finite feature is replaced by its training mean.
    pub fn predict(&self, row: &[f64]) -> f64 {
        let mut eta = self.beta[0];
        for (j, v) in row.iter().enumerate().take(self.mean.len()) {
            let v = if v.is_finite() { *v } else { self.mean[j] };
            eta += self.beta[j + 1] * (v - self.mean[j]) / self.sd[j];
        }
        sigmoid(eta).clamp(EPS, 1.0 - EPS)
    }
}

/// Monotone (isotonic) recalibration map.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Isotonic {
    /// upper raw-probability bound of each block (ascending)
    pub hi: Vec<f64>,
    /// calibrated probability of each block (non-decreasing)
    pub p_cal: Vec<f64>,
    pub base_rate: f64,
    pub n: usize,
}

impl Isotonic {
    /// PAV fit. Each block's frequency is shrunk toward the base rate with
    /// `prior_weight` pseudo-observations, then monotonicity is re-imposed.
    pub fn fit(p: &[f64], y: &[bool], prior_weight: f64) -> Option<Isotonic> {
        let mut v: Vec<(f64, f64)> = p
            .iter()
            .zip(y)
            .filter(|(p, _)| p.is_finite())
            .map(|(&p, &y)| (p, if y { 1.0 } else { 0.0 }))
            .collect();
        if v.len() < 20 {
            return None;
        }
        v.sort_by(|a, b| a.0.total_cmp(&b.0));
        let n = v.len();
        let base = v.iter().map(|x| x.1).sum::<f64>() / n as f64;
        // blocks: (sum_y, count, max_p)
        let mut bl: Vec<(f64, f64, f64)> = vec![];
        for (pi, yi) in v {
            bl.push((yi, 1.0, pi));
            while bl.len() >= 2 {
                let (a, b) = (bl[bl.len() - 2], bl[bl.len() - 1]);
                if a.0 / a.1 >= b.0 / b.1 {
                    bl.pop();
                    *bl.last_mut().unwrap() = (a.0 + b.0, a.1 + b.1, b.2);
                } else {
                    break;
                }
            }
        }
        let mut p_cal: Vec<f64> = bl
            .iter()
            .map(|b| (b.0 + prior_weight * base) / (b.1 + prior_weight))
            .collect();
        for i in 1..p_cal.len() {
            p_cal[i] = p_cal[i].max(p_cal[i - 1]);
        }
        Some(Isotonic {
            hi: bl.iter().map(|b| b.2).collect(),
            p_cal,
            base_rate: base,
            n,
        })
    }

    pub fn apply(&self, p: f64) -> f64 {
        if !p.is_finite() || self.hi.is_empty() {
            return self.base_rate;
        }
        let k = self.hi.partition_point(|h| *h < p).min(self.hi.len() - 1);
        self.p_cal[k].clamp(EPS, 1.0 - EPS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::SplitMix64;

    #[test]
    fn logistic_recovers_known_coefficients() {
        let mut rng = SplitMix64(7);
        let (mut x, mut y) = (vec![], vec![]);
        for _ in 0..4000 {
            let a = rng.next_f64() * 2.0 - 1.0;
            let b = rng.next_f64() * 2.0 - 1.0;
            let p = sigmoid(-0.5 + 2.0 * a - 1.0 * b);
            x.push(vec![a, b]);
            y.push(rng.next_f64() < p);
        }
        let m = Logistic::fit(&x, &y, 0.0).unwrap();
        // raw-scale coefficient = beta / sd
        let ca = m.beta[1] / m.sd[0];
        let cb = m.beta[2] / m.sd[1];
        assert!((ca - 2.0).abs() < 0.3, "{ca}");
        assert!((cb + 1.0).abs() < 0.3, "{cb}");
        assert!(m.predict(&[1.0, -1.0]) > m.predict(&[-1.0, 1.0]));
    }

    #[test]
    fn logistic_rejects_degenerate_input() {
        let x = vec![vec![1.0]; 50];
        assert!(Logistic::fit(&x, &[true; 50], 1.0).is_none());
        assert!(Logistic::fit(&x[..5], &[true, false, true, false, true], 1.0).is_none());
    }

    #[test]
    fn isotonic_is_monotone_and_calibrates() {
        // raw p is overconfident: true freq = 0.5 * p + 0.25
        let mut rng = SplitMix64(3);
        let (mut p, mut y) = (vec![], vec![]);
        for _ in 0..5000 {
            let q = rng.next_f64();
            p.push(q);
            y.push(rng.next_f64() < 0.5 * q + 0.25);
        }
        let m = Isotonic::fit(&p, &y, 5.0).unwrap();
        assert!(m.p_cal.windows(2).all(|w| w[1] >= w[0]));
        assert!((m.apply(0.9) - 0.70).abs() < 0.08, "{}", m.apply(0.9));
        assert!((m.apply(0.1) - 0.30).abs() < 0.08, "{}", m.apply(0.1));
        assert!(m.apply(f64::NAN) == m.base_rate);
    }
}
