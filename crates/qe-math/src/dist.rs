//! Normal-distribution primitives with good relative accuracy in the tails.
//!
//! `erfc` uses the Chebyshev fit from Numerical Recipes (`erfcc`), whose
//! *fractional* error is < 1.2e-7 everywhere, so tail probabilities keep
//! relative precision (needed when they are multiplied by large exponentials
//! in first-passage formulas). `norm_ppf` is Acklam's rational approximation
//! (relative error < 1.15e-9) refined by one Halley step.

/// ln(erfc(z)) for z >= 0 computed without underflow.
fn ln_erfc_pos(z: f64) -> f64 {
    let t = 1.0 / (1.0 + 0.5 * z);
    let poly = -z * z - 1.265_512_23
        + t * (1.000_023_68
            + t * (0.374_091_96
                + t * (0.096_784_18
                    + t * (-0.186_288_06
                        + t * (0.278_868_07
                            + t * (-1.135_203_98
                                + t * (1.488_515_87 + t * (-0.822_152_23 + t * 0.170_872_77))))))));
    t.ln() + poly
}

/// Complementary error function.
pub fn erfc(x: f64) -> f64 {
    if x >= 0.0 {
        ln_erfc_pos(x).exp()
    } else {
        2.0 - ln_erfc_pos(-x).exp()
    }
}

/// Standard normal CDF Phi(x) = erfc(-x / sqrt 2) / 2.
pub fn norm_cdf(x: f64) -> f64 {
    0.5 * erfc(-x / std::f64::consts::SQRT_2)
}

/// ln Phi(x), accurate for very negative x.
pub fn ln_norm_cdf(x: f64) -> f64 {
    if x < 0.0 {
        ln_erfc_pos(-x / std::f64::consts::SQRT_2) - std::f64::consts::LN_2
    } else {
        norm_cdf(x).ln()
    }
}

/// Standard normal density.
pub fn norm_pdf(x: f64) -> f64 {
    (-0.5 * x * x).exp() / (2.0 * std::f64::consts::PI).sqrt()
}

/// Inverse standard normal CDF (quantile). p is clamped to [1e-12, 1-1e-12].
pub fn norm_ppf(p: f64) -> f64 {
    let p = p.clamp(1e-12, 1.0 - 1e-12);
    const A: [f64; 6] = [
        -3.969_683_028_665_376e1,
        2.209_460_984_245_205e2,
        -2.759_285_104_469_687e2,
        1.383_577_518_672_69e2,
        -3.066_479_806_614_716e1,
        2.506_628_277_459_239,
    ];
    const B: [f64; 5] = [
        -5.447_609_879_822_406e1,
        1.615_858_368_580_409e2,
        -1.556_989_798_598_866e2,
        6.680_131_188_771_972e1,
        -1.328_068_155_288_572e1,
    ];
    const C: [f64; 6] = [
        -7.784_894_002_430_293e-3,
        -3.223_964_580_411_365e-1,
        -2.400_758_277_161_838,
        -2.549_732_539_343_734,
        4.374_664_141_464_968,
        2.938_163_982_698_783,
    ];
    const D: [f64; 4] = [
        7.784_695_709_041_462e-3,
        3.224_671_290_700_398e-1,
        2.445_134_137_142_996,
        3.754_408_661_907_416,
    ];
    let pl = 0.024_25;
    let x = if p < pl {
        let q = (-2.0 * p.ln()).sqrt();
        (((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    } else if p <= 1.0 - pl {
        let q = p - 0.5;
        let r = q * q;
        (((((A[0] * r + A[1]) * r + A[2]) * r + A[3]) * r + A[4]) * r + A[5]) * q
            / (((((B[0] * r + B[1]) * r + B[2]) * r + B[3]) * r + B[4]) * r + 1.0)
    } else {
        let q = (-2.0 * (1.0 - p).ln()).sqrt();
        -(((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    };
    // one Halley refinement step
    let e = norm_cdf(x) - p;
    let u = e * (2.0 * std::f64::consts::PI).sqrt() * (x * x / 2.0).exp();
    x - u / (1.0 + x * u / 2.0)
}

/// Logistic sigmoid.
pub fn sigmoid(z: f64) -> f64 {
    if z >= 0.0 {
        1.0 / (1.0 + (-z).exp())
    } else {
        let e = z.exp();
        e / (1.0 + e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cdf_and_tails() {
        assert!((norm_cdf(0.0) - 0.5).abs() < 1e-7);
        assert!((norm_cdf(1.96) - 0.975_002_104_851_78).abs() < 1e-7);
        // Phi(-10) = 7.6198530241605e-24: relative accuracy in the far tail
        let v = norm_cdf(-10.0);
        assert!((v / 7.619_853_024_160_5e-24 - 1.0).abs() < 1e-6, "{v}");
        assert!((ln_norm_cdf(-40.0) - (-804.608_442_013_754)).abs() < 1e-3);
    }
    #[test]
    fn ppf_inverts_cdf() {
        for &p in &[1e-6, 0.01, 0.2, 0.5, 0.8, 0.975, 0.999_999] {
            let x = norm_ppf(p);
            assert!(
                (norm_cdf(x) - p).abs() < 1e-9 * p.max(1e-3) * 1e3,
                "{p} {x}"
            );
        }
        assert!((norm_ppf(0.975) - 1.959_963_984_540_054).abs() < 1e-6);
    }
    #[test]
    fn sigmoid_stable() {
        assert_eq!(sigmoid(0.0), 0.5);
        assert!(sigmoid(-800.0) >= 0.0 && sigmoid(800.0) <= 1.0);
    }
}
