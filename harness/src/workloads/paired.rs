//! Alternated rounds judged on their per-round ratios (G7, G20): each round
//! measures store-io and its baseline one after the other, the side first
//! alternating, and the gate is judged as G1's is, on the median of the
//! rounds' ratios and the pooled one (the medians' ratio), the worse of the
//! two, with the median's interval deciding between NOT RESOLVABLE and
//! FAIL. A PASS still needs the pooled ratio, so pairing never makes one;
//! what it changes is the line between NOT RESOLVABLE and FAIL: a runner
//! whose speed drifts between rounds moves both sides of a round together,
//! so each round's ratio, and the interval drawn from them, judges
//! store-io and not the drift. (Three unpaired runs a side failed G20 at
//! 84% on one CI run of code another run passed at 94%; a separate run on
//! CI's memory-backed device, 6 paired rounds, read 94 to 100% in each.)

use super::gates::Verdict;
use crate::report::Point;

/// Rounds (even: each side first in half of them).
pub const ROUNDS: usize = 10;
/// The order statistic (1-based, from either end) bounding the median's
/// interval: the 2nd of 10 holds the true median about 98% of the time
/// (1 - 2 P(Binomial(10, 1/2) <= 1)).
pub const BOUND: usize = 2;

/// Whether round `i` measures store-io first.
#[must_use]
pub fn ours_first(i: usize) -> bool {
    i % 2 == 1
}

/// The median of a non-empty set (the mean of the middle two for an even
/// count; NaN when empty).
#[must_use]
pub fn median(v: &[f64]) -> f64 {
    let mut v = v.to_vec();
    v.sort_by(f64::total_cmp);
    let n = v.len();
    match n {
        0 => f64::NAN,
        _ if n % 2 == 1 => v[n / 2],
        _ => (v[n / 2 - 1] + v[n / 2]) / 2.0,
    }
}

/// The per-round ratios' median, its interval and the pooled ratio, as
/// `ratio`, `ratio_lo`, `ratio_hi` and `pooled_ratio` on `p`: none, and a
/// note saying why, when the two sides did not measure every round or a
/// baseline was not a positive number.
pub fn record(p: &mut Point, ours: &[f64], theirs: &[f64]) {
    let usable = ours.len() == ROUNDS
        && theirs.len() == ROUNDS
        && ours.iter().chain(theirs).all(|x| x.is_finite())
        && theirs.iter().all(|t| *t > 0.0);
    if !usable {
        p.notes.push(format!(
            "the rounds' ratios not taken: {} store-io and {} baseline rounds of {ROUNDS}, or a baseline not a positive number",
            ours.len(),
            theirs.len()
        ));
        return;
    }
    let mut r: Vec<f64> = ours.iter().zip(theirs).map(|(o, t)| o / t).collect();
    r.sort_by(f64::total_cmp);
    p.metric("ratio", Some(median(&r)));
    p.metric("ratio_lo", r.get(BOUND - 1).copied());
    p.metric("ratio_hi", r.len().checked_sub(BOUND).map(|i| r[i]));
    p.metric("pooled_ratio", Some(median(ours) / median(theirs)));
}

/// The verdict on `gate` (a ratio): PASS when the worse of the median and
/// pooled ratios reaches it; NOT RESOLVABLE when it does not but the
/// median's interval does; FAIL otherwise, or when a figure is missing.
#[must_use]
pub(super) fn judge(
    ratio: Option<f64>,
    lo: Option<f64>,
    hi: Option<f64>,
    pooled: Option<f64>,
    gate: f64,
) -> Verdict {
    match (ratio.zip(pooled).map(|(m, p)| m.min(p)), lo, hi) {
        (Some(w), Some(_), Some(_)) if w >= gate => Verdict::Pass,
        (Some(_), Some(_), Some(h)) if h >= gate => Verdict::NotResolvable,
        _ => Verdict::Fail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::System;

    fn recorded(ours: &[f64], theirs: &[f64]) -> Point {
        let mut p = Point::new(System::StoreIo, "paired");
        record(&mut p, ours, theirs);
        p
    }

    fn get(p: &Point, k: &str) -> Option<f64> {
        p.get(k)
    }

    fn judged(p: &Point, gate: f64) -> Verdict {
        judge(
            get(p, "ratio"),
            get(p, "ratio_lo"),
            get(p, "ratio_hi"),
            get(p, "pooled_ratio"),
            gate,
        )
    }

    /// Each round's ratio is its own: a drift both sides share (a runner
    /// slowing from round to round) leaves every round's ratio the same,
    /// so the median's interval is that one ratio, not the drift's width.
    #[test]
    fn test_a_drift_both_sides_share_leaves_the_interval_tight() {
        let theirs: Vec<f64> = (0..ROUNDS).map(|i| 11_400.0 - 600.0 * i as f64).collect();
        let ours: Vec<f64> = theirs.iter().map(|t| 0.85 * t).collect();
        let p = recorded(&ours, &theirs);
        for k in ["ratio", "ratio_lo", "ratio_hi", "pooled_ratio"] {
            assert!((get(&p, k).unwrap() - 0.85).abs() < 1e-9, "{k}");
        }
        // Below a 90% gate by more than the (empty) interval: FAIL, though
        // the drift alone spans almost a factor of two.
        assert_eq!(judged(&p, 0.9), Verdict::Fail);
        assert_eq!(judged(&p, 0.84), Verdict::Pass);
    }

    /// Below the gate with the interval reaching it: NOT RESOLVABLE; with
    /// the whole interval below it: FAIL; the median above but the pooled
    /// ratio below: not a pass.
    #[test]
    fn test_the_interval_decides_between_not_resolvable_and_fail() {
        let theirs = vec![10_000.0; ROUNDS];
        // Ratios 0.80 .. 0.98, median 0.89, 2nd largest 0.96.
        let ours: Vec<f64> = (0..ROUNDS).map(|i| 8000.0 + 200.0 * i as f64).collect();
        assert_eq!(
            judged(&recorded(&ours, &theirs), 0.9),
            Verdict::NotResolvable
        );
        let low: Vec<f64> = (0..ROUNDS).map(|i| 7000.0 + 100.0 * i as f64).collect();
        assert_eq!(judged(&recorded(&low, &theirs), 0.9), Verdict::Fail);
        // The rounds' ratios 0.95 but for two middle rounds at 0.80: the
        // median ratio 0.95, the pooled one (the medians') 0.80, so the
        // worse of the two is below the gate: not a pass.
        let t2 = [
            8000.0, 8000.0, 8000.0, 8000.0, 10_000.0, 10_000.0, 12_000.0, 12_000.0, 12_000.0,
            12_000.0,
        ];
        let o2 = [
            7600.0, 7600.0, 7600.0, 7600.0, 8000.0, 8000.0, 11_400.0, 11_400.0, 11_400.0, 11_400.0,
        ];
        let p = recorded(&o2, &t2);
        assert!((get(&p, "ratio").unwrap() - 0.95).abs() < 1e-9);
        assert!((get(&p, "pooled_ratio").unwrap() - 0.8).abs() < 1e-9);
        assert_eq!(judged(&p, 0.9), Verdict::NotResolvable);
    }

    /// A round missing on either side, or a baseline of zero, records
    /// nothing but a note, and nothing recorded fails.
    #[test]
    fn test_a_missing_round_or_baseline_fails() {
        let theirs = vec![10_000.0; ROUNDS];
        let ours = vec![9_900.0; ROUNDS];
        let short = recorded(&ours[1..], &theirs);
        assert_eq!(judged(&short, 0.9), Verdict::Fail);
        assert!(short.notes.iter().any(|n| n.contains("not taken")));
        let mut zero = theirs.clone();
        zero[3] = 0.0;
        assert_eq!(judged(&recorded(&ours, &zero), 0.9), Verdict::Fail);
        assert_eq!(judged(&recorded(&ours, &theirs), 0.9), Verdict::Pass);
    }

    /// The side first alternates, each first in half the rounds.
    #[test]
    fn test_each_side_goes_first_in_half_the_rounds() {
        let first = (0..ROUNDS).filter(|i| ours_first(*i)).count();
        assert_eq!(first * 2, ROUNDS);
    }
}
