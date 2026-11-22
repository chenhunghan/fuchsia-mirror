// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Small statistics helpers: quantiles, summaries and bootstrap tests.
//!
//! The bootstrap draws its resamples from a fixed-seed generator, so the same
//! samples always give the same intervals (the regression gate's decisions
//! depend on them).

use serde::Serialize;

/// The linear-interpolation quantile of `xs` (NaN when empty).
pub fn quantile(xs: &[f64], q: f64) -> f64 {
    if xs.is_empty() {
        return f64::NAN;
    }
    let mut s = xs.to_vec();
    s.sort_by(f64::total_cmp);
    quantile_sorted(&s, q)
}

/// [`quantile`] for an already sorted, non-empty slice.
pub fn quantile_sorted(s: &[f64], q: f64) -> f64 {
    if s.len() == 1 {
        return s[0];
    }
    let q = q.clamp(0.0, 1.0);
    let pos = q * (s.len() - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = (lo + 1).min(s.len() - 1);
    s[lo] + (s[hi] - s[lo]) * (pos - lo as f64)
}

/// [`quantile`] of integer samples.
pub fn quantile_i64(xs: &[i64], q: f64) -> f64 {
    quantile(&to_f64(xs), q)
}

/// Converts integer samples to floats.
pub fn to_f64(xs: &[i64]) -> Vec<f64> {
    xs.iter().map(|&x| x as f64).collect()
}

/// n, min, p50, p90, p99, max and mean of a sample. Empty samples only have n.
#[derive(Clone, Debug, Default, PartialEq, Serialize, serde::Deserialize)]
pub struct Summary {
    pub n: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p50: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p90: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p99: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mean: Option<f64>,
}

impl Summary {
    /// p50, or 0 when the sample is empty.
    pub fn p50(&self) -> f64 {
        self.p50.unwrap_or(0.0)
    }
    /// p90, or 0 when the sample is empty.
    pub fn p90(&self) -> f64 {
        self.p90.unwrap_or(0.0)
    }
    /// p99, or 0 when the sample is empty.
    pub fn p99(&self) -> f64 {
        self.p99.unwrap_or(0.0)
    }
    /// max, or 0 when the sample is empty.
    pub fn max(&self) -> f64 {
        self.max.unwrap_or(0.0)
    }
}

/// Sums floats with Neumaier compensation, so the result does not depend on
/// the order rounding errors build up in.
pub fn compensated_sum(xs: &[f64]) -> f64 {
    let mut f = 0.0f64;
    let mut c = 0.0f64;
    for &x in xs {
        let t = f + x;
        if f.abs() >= x.abs() {
            c += (f - t) + x;
        } else {
            c += (x - t) + f;
        }
        f = t;
    }
    if c != 0.0 && c.is_finite() {
        f += c;
    }
    f
}

/// Summarizes a sample.
pub fn summary(xs: &[f64]) -> Summary {
    if xs.is_empty() {
        return Summary::default();
    }
    let mut s = xs.to_vec();
    s.sort_by(f64::total_cmp);
    let mean = compensated_sum(xs) / xs.len() as f64;
    Summary {
        n: xs.len(),
        min: Some(s[0]),
        p50: Some(quantile_sorted(&s, 0.50)),
        p90: Some(quantile_sorted(&s, 0.90)),
        p99: Some(quantile_sorted(&s, 0.99)),
        max: Some(s[s.len() - 1]),
        mean: Some(mean),
    }
}

/// [`summary`] of integer samples.
pub fn summary_i64(xs: &[i64]) -> Summary {
    let mut out = summary(&to_f64(xs));
    if !xs.is_empty() {
        // Sum exactly; only the division rounds.
        out.mean = Some(xs.iter().map(|&x| x as i128).sum::<i128>() as f64 / xs.len() as f64);
    }
    out
}

/// A small deterministic pseudo-random generator (SplitMix64).
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A uniform integer in [0, n) for n > 0.
    fn below(&mut self, n: u64) -> u64 {
        let mask = n.next_power_of_two().wrapping_sub(1);
        loop {
            let r = self.next_u64() & mask;
            if r < n {
                return r;
            }
        }
    }
}

/// Bootstrap resamples per interval. 2000 resamples keep the 2.5% and 97.5%
/// order statistics within about 0.5 points of their limit for p99; 200 left
/// them several points apart.
const BOOTSTRAP_ITERS: usize = 2000;

/// The (lo, hi) bounds of the 95% interval of bootstrap values. Conventional
/// 95% interval.
fn interval_95(mut vals: Vec<f64>) -> (f64, f64) {
    vals.sort_by(f64::total_cmp);
    let n = vals.len();
    (vals[(0.025 * n as f64) as usize], vals[(0.975 * n as f64) as usize - 1])
}

/// The (lo, hi) 95% bootstrap confidence interval of a quantile.
///
/// The quantile of a resample only depends on its order statistics, so the
/// sample is sorted once and each resample draws indices and selects the two
/// it needs. Every resample has the full sample size: a p99 interval is
/// skewed, so it cannot be scaled down from a smaller resample.
pub fn bootstrap_quantile_ci(xs: &[f64], q: f64) -> (f64, f64) {
    let q = q.clamp(0.0, 1.0);
    if xs.len() < 5 {
        let v = quantile(xs, q);
        return (v, v);
    }
    let mut s = xs.to_vec();
    s.sort_by(f64::total_cmp);
    let n = s.len();
    let mut rng = Rng(1);
    let pos = q * (n - 1) as f64;
    let lo = pos.floor() as usize;
    let mut idx: Vec<usize> = vec![0; n];
    let mut vals = Vec::with_capacity(BOOTSTRAP_ITERS);
    for _ in 0..BOOTSTRAP_ITERS {
        for i in idx.iter_mut() {
            *i = rng.below(n as u64) as usize;
        }
        let (_, &mut at_lo, above) = idx.select_nth_unstable(lo);
        // The next order statistic is the smallest index above position lo.
        let at_hi = above.iter().copied().min().unwrap_or(at_lo);
        vals.push(s[at_lo] + (s[at_hi] - s[at_lo]) * (pos - lo as f64));
    }
    interval_95(vals)
}

/// The (lo, hi) 95% bootstrap confidence interval of `sum(num) / sum(den)`,
/// resampling (num, den) pairs (for example one bucket's time and the total
/// time of each tail job). Returns (r, r) for a single pair, which has no
/// spread to resample; from two pairs on the interval reflects how little
/// data there is.
pub fn bootstrap_ratio_ci(num: &[f64], den: &[f64]) -> (f64, f64) {
    let n = num.len().min(den.len());
    let ratio = |a: f64, b: f64| if b != 0.0 { a / b } else { 0.0 };
    if n < 2 {
        let r = ratio(num[..n].iter().sum(), den[..n].iter().sum());
        return (r, r);
    }
    let mut rng = Rng(1);
    let mut vals = Vec::with_capacity(BOOTSTRAP_ITERS);
    for _ in 0..BOOTSTRAP_ITERS {
        let (mut a, mut b) = (0.0, 0.0);
        for _ in 0..n {
            let i = rng.below(n as u64) as usize;
            a += num[i];
            b += den[i];
        }
        vals.push(ratio(a, b));
    }
    interval_95(vals)
}

/// The fewest samples per side [`improved`] and [`regressed`] decide on:
/// [`bootstrap_quantile_ci`] gives no interval below this.
const MIN_COMPARE: usize = 5;

/// True if quantile q of `after` is lower than `before` beyond bootstrap noise.
pub fn improved(before: &[f64], after: &[f64], q: f64, min_rel: f64) -> bool {
    if before.len() < MIN_COMPARE || after.len() < MIN_COMPARE {
        return false;
    }
    let b = quantile(before, q);
    let a = quantile(after, q);
    let rel = (b - a) / b.max(1e-9);
    if rel.is_nan() || rel < min_rel {
        return false;
    }
    let (blo, _) = bootstrap_quantile_ci(before, q);
    let (_, ahi) = bootstrap_quantile_ci(after, q);
    ahi < blo
}

/// True if quantile q of `after` is higher than `before` beyond bootstrap noise.
pub fn regressed(before: &[f64], after: &[f64], q: f64, min_rel: f64) -> bool {
    if before.len() < MIN_COMPARE || after.len() < MIN_COMPARE {
        return false;
    }
    let b = quantile(before, q);
    let a = quantile(after, q);
    let rel = (a - b) / b.max(1e-9);
    if rel.is_nan() || rel < min_rel {
        return false;
    }
    let (_, bhi) = bootstrap_quantile_ci(before, q);
    let (alo, _) = bootstrap_quantile_ci(after, q);
    alo > bhi
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_is_deterministic_and_in_range() {
        let draw = || {
            let mut r = Rng(1);
            (0..1000).map(|_| r.below(7)).collect::<Vec<_>>()
        };
        let a = draw();
        assert_eq!(a, draw());
        assert!(a.iter().all(|&x| x < 7));
        assert!((0..7).all(|v| a.contains(&v)));
    }

    #[test]
    fn quantiles() {
        let ys: Vec<f64> = (0..5000).map(|i| ((i * 104729) % 100003) as f64).collect();
        assert_eq!(quantile(&ys, 0.99), 98999.21);
        assert_eq!(quantile(&[5.0], 0.5), 5.0);
        assert!(quantile(&[], 0.5).is_nan());
        assert_eq!(quantile(&[1.0, 10.0], -0.5), 1.0);
        assert_eq!(quantile(&[1.0, 10.0], 1.5), 10.0);
        let s = [1.0, 2.0, 3.0, 4.0, 5.0];
        assert_eq!(bootstrap_quantile_ci(&s, -0.5), bootstrap_quantile_ci(&s, 0.0));
        assert_eq!(bootstrap_quantile_ci(&s, 1.5), bootstrap_quantile_ci(&s, 1.0));
    }

    #[test]
    fn bootstrap_intervals() {
        let xs: Vec<f64> = (0..300).map(|i| ((i * 7919) % 1013) as f64 + i as f64 * 0.5).collect();
        let (lo, hi) = bootstrap_quantile_ci(&xs, 0.9);
        let p90 = quantile(&xs, 0.9);
        assert!(lo <= p90 && p90 <= hi && lo < hi, "{lo} {p90} {hi}");
        assert_eq!(bootstrap_quantile_ci(&xs, 0.9), (lo, hi));
        let ys: Vec<f64> = (0..5000).map(|i| ((i * 104729) % 100003) as f64).collect();
        let (lo, hi) = bootstrap_quantile_ci(&ys, 0.99);
        let p99 = quantile(&ys, 0.99);
        assert!(lo <= p99 && p99 <= hi && hi - lo < 0.02 * p99, "{lo} {p99} {hi}");
        let faster: Vec<f64> = ys.iter().map(|y| y * 0.8).collect();
        let slower: Vec<f64> = ys.iter().map(|y| y * 1.3).collect();
        assert!(improved(&ys, &faster, 0.9, 0.05));
        assert!(regressed(&ys, &slower, 0.5, 0.05));
    }

    #[test]
    fn small_samples_keep_their_uncertainty() {
        // Under 5 samples a side, nothing is called better or worse.
        assert!(!improved(&[100.0; 4], &[1.0; 4], 0.9, 0.05));
        assert!(!regressed(&[1.0; 4], &[100.0; 4], 0.9, 0.05));
        assert!(!improved(&[f64::NAN; 6], &[1.0; 6], 0.9, 0.05));
        // A ratio over a few uneven pairs has a real interval, not a point.
        let (lo, hi) = bootstrap_ratio_ci(&[1.0, 9.0, 1.0], &[10.0, 10.0, 10.0]);
        assert!(lo < hi, "{lo} {hi}");
        assert_eq!(bootstrap_ratio_ci(&[1.0], &[4.0]), (0.25, 0.25));
    }

    #[test]
    fn bootstrap_matches_sorting_every_resample() {
        // Selecting the two order statistics gives what sorting would.
        let xs: Vec<f64> = (0..400).map(|i| ((i * 7919) % 1013) as f64).collect();
        let mut s = xs.clone();
        s.sort_by(f64::total_cmp);
        let q = 0.9;
        let pos = q * (s.len() - 1) as f64;
        let lo = pos.floor() as usize;
        let mut rng = Rng(1);
        let mut vals = Vec::new();
        for _ in 0..BOOTSTRAP_ITERS {
            let mut idx: Vec<usize> =
                (0..s.len()).map(|_| rng.below(s.len() as u64) as usize).collect();
            idx.sort_unstable();
            vals.push(s[idx[lo]] + (s[idx[lo + 1]] - s[idx[lo]]) * (pos - lo as f64));
        }
        assert_eq!(bootstrap_quantile_ci(&xs, q), interval_95(vals));
    }

    #[test]
    fn ratio_intervals() {
        let num: Vec<f64> = (0..150).map(|i| (i % 4) as f64).collect();
        let den = vec![4.0; 150];
        let (lo, hi) = bootstrap_ratio_ci(&num, &den);
        let r = num.iter().sum::<f64>() / den.iter().sum::<f64>();
        assert!(lo < r && r < hi, "{lo} {r} {hi}");
        assert_eq!(bootstrap_ratio_ci(&num, &den), (lo, hi));
        assert_eq!(bootstrap_ratio_ci(&[1.0, 1.0], &[2.0, 2.0]), (0.5, 0.5));
        assert_eq!(bootstrap_ratio_ci(&[], &[]), (0.0, 0.0));
    }

    #[test]
    fn summary_of_empty_has_only_n() {
        let s = summary(&[]);
        assert_eq!(serde_json::to_string(&s).unwrap(), r#"{"n":0}"#);
        let s = summary_i64(&[1, 2, 3, 4]);
        assert_eq!(s.n, 4);
        assert_eq!(s.p50, Some(2.5));
        assert_eq!(s.mean, Some(2.5));
    }
}
