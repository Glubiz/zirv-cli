//! Pure statistics primitives for the promotion gate (issue #801): a
//! deterministic PRNG, a paired bootstrap confidence interval, and the small
//! descriptive-stat helpers `promote.rs` composes into a verdict. No fs,
//! clock, env or network: identical inputs (including the seed) always give
//! identical outputs.

use serde::{Deserialize, Serialize};

/// SplitMix64, seeded by a single `u64`. Chosen over the platform RNG
/// because the promotion gate must reproduce the same bootstrap interval for
/// the same seed on every run (#801: "everything deterministic for a given
/// seed").
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// The next raw 64-bit output, advancing the internal state.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A uniform index in `0..bound`. `bound == 0` returns 0 rather than
    /// panicking, since a resample of an empty population never occurs on
    /// any caller's path but should not be able to crash the gate either.
    pub fn next_index(&mut self, bound: usize) -> usize {
        if bound == 0 {
            return 0;
        }
        (self.next_u64() % bound as u64) as usize
    }
}

/// A point estimate with a two-sided confidence interval, as produced by
/// [`paired_bootstrap`].
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Interval {
    pub point: f64,
    pub lo: f64,
    pub hi: f64,
}

/// The arithmetic mean of `xs`, or `0.0` for an empty slice.
pub fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.iter().sum::<f64>() / xs.len() as f64
}

/// The median of `xs` (the 50th nearest-rank percentile). Unused in
/// production today (`promote.rs` calls `percentile` directly for its own
/// median needs); kept as public API and exercised by its own test below.
#[cfg_attr(not(test), allow(dead_code))]
pub fn median(xs: &[f64]) -> f64 {
    percentile(xs, 50.0)
}

/// The nearest-rank percentile `p` (in `[0, 100]`) of `xs`, sorted ascending
/// on a copy; `0.0` for an empty slice. Nearest-rank (rather than
/// interpolated) so the returned value is always an observed data point,
/// matching the rest of the gate's preference for values a reader can trace
/// back to a real trial.
pub fn percentile(xs: &[f64], p: f64) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    let mut sorted = xs.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[idx]
}

/// `mean(candidate - baseline)` over paired `(baseline, candidate)` values.
/// `None` for an empty slice, matching [`paired_bootstrap`]'s convention
/// that a stat function signals "not computable" rather than a nonsense
/// zero.
pub fn mean_diff(pairs: &[(f64, f64)]) -> Option<f64> {
    if pairs.is_empty() {
        return None;
    }
    let diffs: Vec<f64> = pairs.iter().map(|(base, cand)| cand - base).collect();
    Some(mean(&diffs))
}

/// `mean(candidate - baseline) / mean(baseline)` over paired values. `None`
/// when there are no pairs, or when `mean(baseline) <= 0.0` (a
/// non-positive baseline makes a relative change meaningless -- there is
/// nothing to be a fraction of).
pub fn relative_diff(pairs: &[(f64, f64)]) -> Option<f64> {
    if pairs.is_empty() {
        return None;
    }
    let base_mean = mean(&pairs.iter().map(|(base, _)| *base).collect::<Vec<_>>());
    if base_mean <= 0.0 {
        return None;
    }
    let diff_mean = mean_diff(pairs)?;
    Some(diff_mean / base_mean)
}

/// A percentile-bootstrap confidence interval for `stat` over paired
/// `(baseline, candidate)` observations: `resamples` times, draw `pairs.len()`
/// pairs with replacement (each pair resampled as a unit, preserving the
/// pairing) using a `SplitMix64` seeded by `seed`, apply `stat`, then take
/// the `(1-confidence)/2` and `1-(1-confidence)/2` nearest-rank percentiles
/// of the resulting distribution as the interval bounds. The point estimate
/// is `stat` applied to the original (unresampled) pairs.
///
/// `None` when `pairs` is empty or `stat(pairs)` is `None`. Resamples for
/// which `stat` returns `None` are dropped from the interval rather than
/// failing the whole call; if every resample is dropped, the interval
/// collapses to the point estimate.
pub fn paired_bootstrap(
    pairs: &[(f64, f64)],
    stat: impl Fn(&[(f64, f64)]) -> Option<f64>,
    resamples: usize,
    confidence: f64,
    seed: u64,
) -> Option<Interval> {
    if pairs.is_empty() {
        return None;
    }
    let point = stat(pairs)?;
    if resamples == 0 {
        return Some(Interval {
            point,
            lo: point,
            hi: point,
        });
    }

    let mut rng = SplitMix64::new(seed);
    let n = pairs.len();
    let mut samples = Vec::with_capacity(resamples);
    let mut scratch = Vec::with_capacity(n);
    for _ in 0..resamples {
        scratch.clear();
        for _ in 0..n {
            scratch.push(pairs[rng.next_index(n)]);
        }
        if let Some(v) = stat(&scratch) {
            samples.push(v);
        }
    }

    if samples.is_empty() {
        return Some(Interval {
            point,
            lo: point,
            hi: point,
        });
    }

    let alpha = (1.0 - confidence).clamp(0.0, 1.0);
    let lo = percentile(&samples, alpha / 2.0 * 100.0);
    let hi = percentile(&samples, (1.0 - alpha / 2.0) * 100.0);
    Some(Interval { point, lo, hi })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitmix64_is_deterministic_for_seed() {
        let mut a = SplitMix64::new(42);
        let mut b = SplitMix64::new(42);
        let seq_a: Vec<u64> = (0..10).map(|_| a.next_u64()).collect();
        let seq_b: Vec<u64> = (0..10).map(|_| b.next_u64()).collect();
        assert_eq!(seq_a, seq_b);
    }

    #[test]
    fn splitmix64_different_seeds_diverge() {
        let mut a = SplitMix64::new(1);
        let mut b = SplitMix64::new(2);
        assert_ne!(a.next_u64(), b.next_u64());
    }

    #[test]
    fn mean_and_median_basic() {
        assert_eq!(mean(&[1.0, 2.0, 3.0]), 2.0);
        assert_eq!(mean(&[]), 0.0);
        assert_eq!(median(&[1.0, 2.0, 3.0]), 2.0);
        assert_eq!(percentile(&[1.0, 2.0, 3.0, 4.0], 90.0), 4.0);
    }

    #[test]
    fn mean_diff_is_candidate_minus_baseline() {
        let pairs = [(1.0, 3.0), (2.0, 5.0)];
        assert_eq!(mean_diff(&pairs), Some(2.5));
        assert_eq!(mean_diff(&[]), None);
    }

    #[test]
    fn relative_diff_is_none_for_nonpositive_baseline() {
        let pairs = [(0.0, 1.0), (0.0, 2.0)];
        assert_eq!(relative_diff(&pairs), None);
        let pairs = [(-1.0, 1.0)];
        assert_eq!(relative_diff(&pairs), None);
    }

    #[test]
    fn relative_diff_basic() {
        let pairs = [(10.0, 5.0), (10.0, 5.0)];
        assert_eq!(relative_diff(&pairs), Some(-0.5));
    }

    #[test]
    fn paired_bootstrap_is_deterministic_for_seed() {
        let pairs = [(1.0, 2.0), (2.0, 1.0), (3.0, 5.0), (4.0, 2.0), (5.0, 9.0)];
        let a = paired_bootstrap(&pairs, mean_diff, 500, 0.95, 7).unwrap();
        let b = paired_bootstrap(&pairs, mean_diff, 500, 0.95, 7).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn paired_bootstrap_known_data_sanity_check() {
        // Every pair has the exact same candidate - baseline diff (2.0), so
        // every possible bootstrap resample (with replacement) also has a
        // mean diff of exactly 2.0: the interval must collapse to a point.
        let pairs: Vec<(f64, f64)> = (0..20).map(|i| (i as f64, i as f64 + 2.0)).collect();
        let interval = paired_bootstrap(&pairs, mean_diff, 1000, 0.95, 99).unwrap();
        assert_eq!(interval.point, 2.0);
        assert_eq!(interval.lo, 2.0);
        assert_eq!(interval.hi, 2.0);
    }

    #[test]
    fn paired_bootstrap_empty_pairs_is_none() {
        assert_eq!(paired_bootstrap(&[], mean_diff, 100, 0.95, 1), None);
    }
}
