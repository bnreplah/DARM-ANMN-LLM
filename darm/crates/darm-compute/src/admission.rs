//! The CDCP admission model (DARM-ANN Paper 1 §6, Paper 2 §3, Volume 1
//! Proof P39 as corrected in §41).
//!
//! A claim (a memory, or a shard output) is admitted when at least
//! `quorum = ⌈τ_c·N⌉` of `N` validators accept it. If each validator fails to
//! catch a bad claim with probability `p_node`, and failures are correlated
//! with pairwise correlation `ρ` through a one-factor Gaussian copula, the
//! probability a bad claim is admitted is
//!
//! ```text
//! P(admit) = ∫ BinomSF(quorum−1; N, p(z)) φ(z) dz,
//! p(z)     = 1 − Φ((Φ⁻¹(1−p_node) − √ρ·z) / √(1−ρ))
//! ```
//!
//! **Epistemic status: modeled, not measured.** `ρ` is an input, not a
//! measurement of real validators (Paper 2 §7). The numbers this module
//! produces are what the model predicts; they are labelled as such wherever
//! they are reported. The papers' central design finding, reproduced by the
//! tests below, is that under correlation the threshold `τ_c` is a much
//! stronger lever than the number of validators.

use darm_core::{DarmError, Result};
use serde::{Deserialize, Serialize};

/// An admission threshold `τ_c = num/den`, kept as an exact fraction so that
/// `τ_c = 2/3` gives the papers' quorum sizes (14 of 21, not 15).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Threshold {
    pub num: u16,
    pub den: u16,
}

impl Threshold {
    /// The BFT-derived default (Proof P46).
    pub const TWO_THIRDS: Self = Self { num: 2, den: 3 };
    /// Recommended floor for high-risk domains, and whenever correlation
    /// cannot be ruled out (Volume 1 §11 revised guidance).
    pub const HIGH_RISK: Self = Self { num: 4, den: 5 };
    pub const STRICT: Self = Self { num: 9, den: 10 };

    /// Thresholds must exceed one half, so no two different outputs can both
    /// reach quorum, and be at most one.
    pub fn validate(&self) -> Result<()> {
        if self.den == 0 || self.num > self.den || u32::from(self.num) * 2 <= u32::from(self.den) {
            return Err(DarmError::malformed(
                "threshold",
                "must satisfy 1/2 < num/den <= 1",
            ));
        }
        Ok(())
    }

    /// `⌈τ_c·n⌉`, exactly.
    pub fn quorum(&self, n: u32) -> u32 {
        (u64::from(self.num) * u64::from(n)).div_ceil(u64::from(self.den.max(1))) as u32
    }

    pub fn as_f64(&self) -> f64 {
        f64::from(self.num) / f64::from(self.den.max(1))
    }
}

impl Default for Threshold {
    fn default() -> Self {
        Self::TWO_THIRDS
    }
}

/// Upper normal tail `Q(x) = 1 − Φ(x)`, accurate in relative terms far into
/// the tail (the model needs values like 1e-40).
pub fn normal_sf(x: f64) -> f64 {
    if x < 0.0 {
        return 1.0 - normal_sf(-x);
    }
    0.5 * erfc(x / std::f64::consts::SQRT_2)
}

pub fn normal_cdf(x: f64) -> f64 {
    normal_sf(-x)
}

fn erfc(y: f64) -> f64 {
    if y < 2.0 {
        // erf series: 2/√π Σ (-1)^n y^(2n+1) / (n! (2n+1)).
        let mut term = y;
        let mut sum = y;
        let y2 = y * y;
        for n in 1..200 {
            term *= -y2 / f64::from(n);
            let add = term / f64::from(2 * n + 1);
            sum += add;
            if add.abs() < 1e-17 * sum.abs() {
                break;
            }
        }
        1.0 - sum * 2.0 / std::f64::consts::PI.sqrt()
    } else {
        // Continued fraction (modified Lentz) for erfc, y ≥ 2:
        // erfc(y) = exp(−y²)/√π · 1/(y + (1/2)/(y + 1/(y + (3/2)/(y + …)))).
        let tiny = 1e-300;
        let mut f = y;
        let mut c = y;
        let mut d = 0.0;
        for k in 1..500 {
            let a = f64::from(k) / 2.0;
            d = y + a * d;
            d = if d.abs() < tiny { tiny } else { d };
            c = y + a / c;
            c = if c.abs() < tiny { tiny } else { c };
            d = 1.0 / d;
            let delta = c * d;
            f *= delta;
            if (delta - 1.0).abs() < 1e-16 {
                break;
            }
        }
        (-y * y).exp() / (f * std::f64::consts::PI.sqrt())
    }
}

/// Inverse normal CDF (Acklam's rational approximation, then two Halley
/// refinements against [`normal_cdf`]).
pub fn normal_inv_cdf(p: f64) -> f64 {
    assert!(p > 0.0 && p < 1.0, "probability must be in (0, 1)");
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
    let low = 0.024_25;
    let mut x = if p < low {
        let q = (-2.0 * p.ln()).sqrt();
        (((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    } else if p <= 1.0 - low {
        let q = p - 0.5;
        let r = q * q;
        (((((A[0] * r + A[1]) * r + A[2]) * r + A[3]) * r + A[4]) * r + A[5]) * q
            / (((((B[0] * r + B[1]) * r + B[2]) * r + B[3]) * r + B[4]) * r + 1.0)
    } else {
        let q = (-2.0 * (1.0 - p).ln()).sqrt();
        -(((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    };
    for _ in 0..2 {
        let e = normal_cdf(x) - p;
        let u = e * (2.0 * std::f64::consts::PI).sqrt() * (x * x / 2.0).exp();
        x -= u / (1.0 + x * u / 2.0);
    }
    x
}

fn ln_choose(n: u32, k: u32) -> f64 {
    (1..=k)
        .map(|i| (f64::from(n - k + i) / f64::from(i)).ln())
        .sum()
}

/// `P(X ≥ k)` for `X ~ Binomial(n, p)`, summed in log space.
pub fn binomial_tail(k: u32, n: u32, p: f64) -> f64 {
    if k == 0 {
        return 1.0;
    }
    if k > n || p <= 0.0 {
        return 0.0;
    }
    if p >= 1.0 {
        return 1.0;
    }
    let (lp, lq) = (p.ln(), (-p).ln_1p());
    (k..=n)
        .map(|j| (ln_choose(n, j) + f64::from(j) * lp + f64::from(n - j) * lq).exp())
        .sum::<f64>()
        .min(1.0)
}

/// Modeled probability that a bad claim reaches quorum.
pub fn admission_probability(n: u32, p_node: f64, rho: f64, threshold: Threshold) -> f64 {
    let quorum = threshold.quorum(n);
    if !(p_node > 0.0 && p_node < 1.0) {
        return if p_node >= 1.0 { 1.0 } else { 0.0 };
    }
    let rho = rho.clamp(0.0, 0.999);
    if rho == 0.0 {
        return binomial_tail(quorum, n, p_node);
    }
    let z_thr = normal_inv_cdf(1.0 - p_node);
    let (sr, s1r) = (rho.sqrt(), (1.0 - rho).sqrt());
    let integrand = |z: f64| {
        let pz = normal_sf((z_thr - sr * z) / s1r);
        binomial_tail(quorum, n, pz) * (-z * z / 2.0).exp() / (2.0 * std::f64::consts::PI).sqrt()
    };
    // Composite Simpson over [-8, 8]; the integrand is smooth and the step
    // is far below its narrowest feature for ρ ≤ 0.999.
    let steps = 4_000;
    let (a, b) = (-8.0, 8.0);
    let h = (b - a) / f64::from(steps);
    let mut sum = integrand(a) + integrand(b);
    for i in 1..steps {
        let w = if i % 2 == 1 { 4.0 } else { 2.0 };
        sum += w * integrand(a + h * f64::from(i));
    }
    (sum * h / 3.0).min(1.0)
}

/// Proof obligation P67 (v7.2 §4.3): with spot-check probability `q`, the
/// chance that `k` fraudulent single-replica shards all go unchecked.
pub fn undetected_fraud_bound(q: f64, k: u32) -> f64 {
    (1.0 - q.clamp(0.0, 1.0)).powi(i32::try_from(k).unwrap_or(i32::MAX))
}

/// A labelled modeled estimate for status and plan responses.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModeledAdmission {
    pub label: &'static str,
    pub replicas: u32,
    pub quorum: u32,
    pub threshold: f64,
    pub p_node: f64,
    /// `(ρ, P(admit))` for the correlation levels the papers sweep.
    pub by_correlation: Vec<(f64, f64)>,
}

impl ModeledAdmission {
    pub fn new(replicas: u32, threshold: Threshold, p_node: f64) -> Self {
        Self {
            label: "modeled (engineering estimate, P39 copula model); rho is assumed, not measured",
            replicas,
            quorum: threshold.quorum(replicas),
            threshold: threshold.as_f64(),
            p_node,
            by_correlation: [0.0, 0.3, 0.5]
                .into_iter()
                .map(|rho| (rho, admission_probability(replicas, p_node, rho, threshold)))
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(actual: f64, expected: f64, rel: f64) {
        let err = ((actual - expected) / expected).abs();
        assert!(
            err <= rel,
            "got {actual:e}, paper says {expected:e} ({err:.3} rel)"
        );
    }

    #[test]
    fn normal_functions_are_accurate_in_the_tails() {
        close(normal_sf(0.0), 0.5, 1e-15);
        close(normal_sf(1.959_963_984_540_054), 0.025, 1e-12);
        close(normal_sf(5.0), 2.866_515_718_791_939e-7, 1e-10);
        close(normal_sf(10.0), 7.619_853_024_160_525e-24, 1e-9);
        close(normal_sf(20.0), 2.753_624_118_606_233_7e-89, 1e-8);
        close(normal_inv_cdf(0.99), 2.326_347_874_040_841, 1e-12);
        close(normal_inv_cdf(1.0 - 5e-4), 3.290_526_731_491_926, 1e-12);
        close(binomial_tail(5, 7, 0.01), 2.065_2e-9, 1e-3);
    }

    #[test]
    fn quorum_sizes_match_the_papers() {
        assert_eq!(Threshold::TWO_THIRDS.quorum(7), 5);
        assert_eq!(Threshold::TWO_THIRDS.quorum(21), 14);
        assert_eq!(Threshold::HIGH_RISK.quorum(7), 6);
        assert_eq!(Threshold::STRICT.quorum(7), 7);
        assert_eq!(Threshold::TWO_THIRDS.quorum(3), 2);
        assert!(Threshold { num: 1, den: 2 }.validate().is_err());
        assert!(Threshold { num: 3, den: 2 }.validate().is_err());
        assert!(Threshold { num: 1, den: 0 }.validate().is_err());
        Threshold::STRICT.validate().unwrap();
    }

    /// Paper 1 §6 / Volume 1 P39 table (corrected).
    #[test]
    fn reproduces_the_p39_table() {
        let t = Threshold::TWO_THIRDS;
        close(admission_probability(7, 0.01, 0.0, t), 2.1e-9, 0.03);
        close(admission_probability(7, 0.01, 0.5, t), 4.740e-4, 0.01);
        close(admission_probability(7, 5e-4, 0.0, t), 6.6e-16, 0.03);
        close(admission_probability(7, 5e-4, 0.5, t), 2.7e-6, 0.03);
        close(admission_probability(21, 0.01, 0.0, t), 1.1e-23, 0.05);
        close(admission_probability(21, 0.01, 0.5, t), 2.151e-4, 0.01);
        close(admission_probability(21, 5e-4, 0.0, t), 7.1e-42, 0.03);
        close(admission_probability(21, 5e-4, 0.5, t), 6.6e-7, 0.03);
    }

    /// Paper 2 §3 Table 1 (node-count sweep at ρ = 0.3 and 0.5).
    #[test]
    fn reproduces_the_node_count_sweep() {
        let t = Threshold::TWO_THIRDS;
        for (n, at_03, at_05) in [
            (7, 4.5e-5, 4.7e-4),
            (13, 9.3e-6, 2.6e-4),
            (21, 4.8e-6, 2.2e-4),
            (35, 1.5e-6, 1.3e-4),
            (51, 1.4e-6, 1.4e-4),
        ] {
            close(admission_probability(n, 0.01, 0.3, t), at_03, 0.04);
            close(admission_probability(n, 0.01, 0.5, t), at_05, 0.04);
        }
    }

    /// Hypothesis H1 (Paper 2 Appendix A): under ρ = 0.5 raising τ_c from
    /// 2/3 to 0.9 helps ~17.6x, tripling the validators only ~2.2x.
    #[test]
    fn threshold_beats_node_count_under_correlation() {
        let n7 = admission_probability(7, 0.01, 0.5, Threshold::TWO_THIRDS);
        let n21 = admission_probability(21, 0.01, 0.5, Threshold::TWO_THIRDS);
        let strict = admission_probability(7, 0.01, 0.5, Threshold::STRICT);
        close(
            admission_probability(7, 0.01, 0.5, Threshold::HIGH_RISK),
            1.4e-4,
            0.04,
        );
        close(strict, 2.697e-5, 0.01);
        close(n7 / n21, 2.20, 0.02);
        close(n7 / strict, 17.57, 0.02);
    }

    #[test]
    fn spot_check_bound() {
        close(undetected_fraud_bound(0.1, 10), 0.9f64.powi(10), 1e-12);
        assert_eq!(undetected_fraud_bound(1.0, 3), 0.0);
        assert_eq!(undetected_fraud_bound(0.0, 3), 1.0);
    }
}
