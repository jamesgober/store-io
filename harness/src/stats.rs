//! Latency samples and their summary: hand-rolled, sort-based percentiles.
//!
//! Percentiles use the **nearest-rank** method on the sorted samples: the
//! q-quantile is the sample at 1-based rank `ceil(q * n)`. A percentile is
//! reported only when the sample count supports it (p99 needs at least 100
//! samples, p99.9 at least 1000); otherwise it is `None` and printed `n/a`,
//! never extrapolated.

use std::time::Duration;

/// Latency samples in nanoseconds.
#[derive(Debug, Default, Clone)]
pub struct Samples {
    ns: Vec<u64>,
}

/// The summary of a sample set, in microseconds.
#[derive(Debug, Clone, Copy)]
pub struct Summary {
    /// Sample count.
    pub n: usize,
    /// Median.
    pub p50: f64,
    /// 90th percentile.
    pub p90: f64,
    /// 99th percentile (`None` below 100 samples).
    pub p99: Option<f64>,
    /// 99.9th percentile (`None` below 1000 samples).
    pub p999: Option<f64>,
    /// Largest sample.
    pub max: f64,
    /// Arithmetic mean.
    pub mean: f64,
}

impl Samples {
    /// An empty set with room for `n` samples.
    #[must_use]
    pub fn with_capacity(n: usize) -> Self {
        Self {
            ns: Vec::with_capacity(n),
        }
    }

    /// Adds one sample.
    pub fn push(&mut self, d: Duration) {
        self.ns
            .push(u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
    }

    /// Sum of all samples.
    #[must_use]
    pub fn total(&self) -> Duration {
        Duration::from_nanos(self.ns.iter().sum())
    }

    /// The summary, or `None` for an empty set.
    #[must_use]
    pub fn summary(&self) -> Option<Summary> {
        if self.ns.is_empty() {
            return None;
        }
        let mut v = self.ns.clone();
        v.sort_unstable();
        let n = v.len();
        let us = |x: u64| x as f64 / 1000.0;
        let rank = |q: f64| {
            // Nearest rank: 1-based ceil(q * n), clamped to [1, n].
            let r = (q * n as f64).ceil() as usize;
            v[r.clamp(1, n) - 1]
        };
        let mean = v.iter().map(|&x| x as f64).sum::<f64>() / n as f64 / 1000.0;
        Some(Summary {
            n,
            p50: us(rank(0.50)),
            p90: us(rank(0.90)),
            p99: (n >= 100).then(|| us(rank(0.99))),
            p999: (n >= 1000).then(|| us(rank(0.999))),
            max: us(v[n - 1]),
            mean,
        })
    }
}

/// Formats an optional number with `decimals` places, `n/a` when absent or
/// not finite.
#[must_use]
pub fn fmt(x: Option<f64>, decimals: usize) -> String {
    match x {
        Some(v) if v.is_finite() => format!("{v:.decimals$}"),
        _ => "n/a".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nearest_rank_percentiles() {
        let mut s = Samples::default();
        for i in 1..=1000u64 {
            s.push(Duration::from_micros(i));
        }
        let m = s.summary().unwrap_or_else(|| panic!("summary"));
        assert_eq!(m.n, 1000);
        assert!((m.p50 - 500.0).abs() < 1e-9);
        assert!((m.p90 - 900.0).abs() < 1e-9);
        assert_eq!(m.p99, Some(990.0));
        assert_eq!(m.p999, Some(999.0));
        assert!((m.max - 1000.0).abs() < 1e-9);
        assert!((m.mean - 500.5).abs() < 1e-9);
    }

    #[test]
    fn test_small_sets_do_not_report_tail_percentiles() {
        let mut s = Samples::default();
        for i in 0..50u64 {
            s.push(Duration::from_micros(i));
        }
        let m = s.summary().unwrap_or_else(|| panic!("summary"));
        assert_eq!(m.p99, None);
        assert_eq!(m.p999, None);
        assert!(Samples::default().summary().is_none());
    }

    #[test]
    fn test_formatting() {
        assert_eq!(fmt(Some(1.23456), 2), "1.23");
        assert_eq!(fmt(None, 2), "n/a");
        assert_eq!(fmt(Some(f64::NAN), 1), "n/a");
    }
}
