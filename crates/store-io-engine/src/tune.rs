//! Opt-in auto-tuning of a device group's background budget (PLANNING
//! R5.4, D-146): the rule alone, with no clock, sample or atomic of its
//! own. The group's tuning state (V3f3) closes a window of leader commit
//! samples, hands this its p99 and runs the next window at the budget it
//! returns.
//!
//! There is no idle anchor: each window is compared with the **floor's own
//! p99 in the same conditions** (a window run at the floor, budget 1,
//! commits and background work as they are).
//! - The first window runs at the floor (nothing to compare with before
//!   it); so does every eighth (a probe, which costs only background
//!   throughput).
//! - So does the window after any window over 1.4x the latest floor
//!   window's p99, and the window after any floor window over 1.25x the
//!   floor window before it: a floor taken in a drive's slow stretch is
//!   re-measured at once, not trusted for seven windows.
//! - The budget rises by one, to its ceiling (`background_depth_max`),
//!   only after three windows at the budget whose p99 is at most 1.25x the
//!   latest floor window's; it halves at once, never below the floor, on a
//!   window over 1.4x.
//!
//! Its bounds (documented, not gated; G19 publishes them): commits may run
//! at up to 1.4x the floor's own ratio to idle indefinitely; transiently,
//! until a re-probe, at whatever a floor taken in a slow stretch allows;
//! and within the one window that first goes over 1.4x, at any ratio until
//! that window closes and the budget halves.

/// The floor: one piece beyond each call's own while commits are active.
pub(crate) const FLOOR: u64 = 1;
/// Every this many windows, one runs at the floor.
const PROBE_EVERY: u64 = 8;
/// Windows at the budget within [`RISE_PCT`] of the floor before a rise.
const RISE_AFTER: u32 = 3;
/// A rise needs windows at most this percentage of the floor's p99; a
/// floor window over it of the previous floor's is re-measured.
const RISE_PCT: u64 = 125;
/// A window over this percentage of the floor's p99 halves the budget.
const HALVE_PCT: u64 = 140;

/// Whether `p99` is over `pct` percent of `of`.
fn over(p99: u64, of: u64, pct: u64) -> bool {
    u128::from(p99) * 100 > u128::from(of) * u128::from(pct)
}

/// The rule's state for one device group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Tuner {
    /// The budget a window not at the floor runs at (at least [`FLOOR`]).
    budget: u64,
    /// Its ceiling (`background_depth_max`).
    ceiling: u64,
    /// Windows closed so far.
    closed: u64,
    /// The latest floor window's p99 (`None` before the first closes).
    floor: Option<u64>,
    /// Consecutive windows at the budget within [`RISE_PCT`] of the floor.
    streak: u32,
    /// The current window runs at the floor.
    at_floor: bool,
}

impl Tuner {
    /// A group's rule, its budget at the floor and its first window at the
    /// floor; `ceiling` at least [`FLOOR`].
    pub(crate) fn new(ceiling: u64) -> Self {
        Self {
            budget: FLOOR,
            ceiling: ceiling.max(FLOOR),
            closed: 0,
            floor: None,
            streak: 0,
            at_floor: true,
        }
    }

    /// The budget the current window runs at.
    pub(crate) fn current(&self) -> u64 {
        if self.at_floor { FLOOR } else { self.budget }
    }

    /// The budget windows not at the floor run at.
    pub(crate) fn budget(&self) -> u64 {
        self.budget
    }

    /// Whether the current window runs at the floor.
    pub(crate) fn at_floor(&self) -> bool {
        self.at_floor
    }

    /// The latest floor window's p99.
    pub(crate) fn floor_p99(&self) -> Option<u64> {
        self.floor
    }

    /// Closes the current window, whose commits' p99 was `p99`: applies
    /// the rule and returns the budget the next window runs at.
    pub(crate) fn close(&mut self, p99: u64) -> u64 {
        let mut reprobe = false;
        match (self.at_floor, self.floor) {
            // A floor window: the new reference, re-measured at once if it
            // is slower than the one before by more than the rise line.
            (true, prev) => {
                reprobe = prev.is_some_and(|f| over(p99, f, RISE_PCT));
                self.floor = Some(p99);
            }
            (false, Some(f)) => {
                if over(p99, f, HALVE_PCT) {
                    self.budget = (self.budget / 2).max(FLOOR);
                    self.streak = 0;
                    reprobe = true;
                } else if over(p99, f, RISE_PCT) {
                    self.streak = 0;
                } else {
                    self.streak += 1;
                    if self.streak >= RISE_AFTER {
                        self.budget = (self.budget + 1).min(self.ceiling);
                        self.streak = 0;
                    }
                }
            }
            // Unreachable (the first window is at the floor), but safe:
            // nothing to compare with, so measure the floor next.
            (false, None) => reprobe = true,
        }
        self.closed += 1;
        self.at_floor = reprobe || self.closed % PROBE_EVERY == 0;
        self.current()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Windows are numbered from 1 as they run: window 1 is the first
    // floor window, the probes are 9, 17, 25, ...; each `close` ends the
    // current window and returns the budget the next one runs at.

    /// Closes windows with `p99s` in turn, returning the budget each next
    /// window runs at.
    fn run(t: &mut Tuner, p99s: &[u64]) -> Vec<u64> {
        p99s.iter().map(|&p| t.close(p)).collect()
    }

    /// Closes `n` windows, each as fast as the floor of 1000.
    fn good(t: &mut Tuner, n: usize) {
        for _ in 0..n {
            let _ = t.close(1000);
        }
    }

    #[test]
    fn test_the_first_window_runs_at_the_floor() {
        let t = Tuner::new(8);
        assert!(t.at_floor());
        assert_eq!(
            (t.current(), t.budget(), t.floor_p99()),
            (FLOOR, FLOOR, None)
        );
    }

    #[test]
    fn test_three_windows_within_the_rise_line_raise_the_budget_by_one() {
        let mut t = Tuner::new(8);
        assert_eq!(t.close(1000), 1, "window 1, the floor, closes");
        assert!(!t.at_floor(), "window 2 runs at the budget");
        // Windows 2 and 3; exactly 1.25x is within the line.
        assert_eq!(run(&mut t, &[1000, 1250]), [1, 1]);
        assert_eq!(t.close(1100), 2, "window 4, the third within the line");
        assert_eq!(run(&mut t, &[1000, 1000]), [2, 2]);
        assert_eq!(t.close(1000), 3, "window 7");
    }

    #[test]
    fn test_a_window_between_the_lines_holds_the_budget_and_restarts_the_count() {
        let mut t = Tuner::new(8);
        let _ = t.close(1000);
        // Window 4 at 1.251x: held, and the count starts again.
        assert_eq!(run(&mut t, &[1000, 1000, 1251]), [1, 1, 1]);
        assert_eq!(run(&mut t, &[1000, 1000]), [1, 1]);
        assert_eq!(t.close(1000), 2, "window 7, the third since window 4");
        // Window 8 at exactly 1.4x holds too (the halving line is "over
        // 1.4x"); window 9 is a probe.
        assert_eq!(t.close(1400), FLOOR);
        assert!(t.at_floor());
        assert_eq!(t.budget(), 2, "held, not halved");
    }

    #[test]
    fn test_a_window_over_the_halving_line_halves_at_once_and_reprobes_the_floor() {
        let mut t = Tuner::new(8);
        let _ = t.close(1000);
        // Windows 2-11: rises after 4, 7 and 11 (the probe, window 9, in
        // between).
        good(&mut t, 10);
        assert_eq!((t.budget(), t.at_floor()), (4, false), "{t:?}");
        assert_eq!(
            t.close(1401),
            FLOOR,
            "window 12 over 1.4x: window 13 at the floor"
        );
        assert!(t.at_floor());
        assert_eq!(t.budget(), 2);
        // Window 13, no slower than the floor before: no second re-probe.
        assert_eq!(t.close(1000), 2);
    }

    #[test]
    fn test_an_odd_budget_halves_down() {
        let mut t = Tuner::new(8);
        let _ = t.close(1000);
        // Rises after windows 4, 7, 11 and 14.
        good(&mut t, 13);
        assert_eq!((t.budget(), t.at_floor()), (5, false), "{t:?}");
        let _ = t.close(1500);
        assert_eq!(t.budget(), 2);
    }

    #[test]
    fn test_a_halving_restarts_the_count_toward_a_rise() {
        let mut t = Tuner::new(8);
        let _ = t.close(1000);
        // Up to 2 after window 4, then windows 5 and 6 good.
        assert_eq!(run(&mut t, &[1000, 1000, 1000]), [1, 1, 2]);
        assert_eq!(run(&mut t, &[1000, 1000]), [2, 2]);
        // Window 7 over the halving line: 2 halves to 1, window 8 at the
        // floor.
        assert_eq!(t.close(1500), FLOOR);
        assert_eq!(t.budget(), 1);
        // Window 8 (the re-measured floor) closes; window 9 is a probe.
        assert_eq!(t.close(1000), FLOOR);
        assert!(t.at_floor());
        assert_eq!(t.close(1000), 1, "the probe closes");
        // Windows 10 and 11 good: two since the halving, no rise (without
        // the restart, windows 5 and 6 would make window 10 the third).
        assert_eq!(run(&mut t, &[1000, 1000]), [1, 1]);
        assert_eq!(t.close(1000), 2, "window 12, the third since the halving");
    }

    #[test]
    fn test_a_count_toward_a_rise_carries_across_a_probe() {
        let mut t = Tuner::new(8);
        let _ = t.close(1000);
        // Windows 2-8: rises after 4 and 7, window 8 one toward the next.
        good(&mut t, 7);
        assert_eq!((t.budget(), t.at_floor()), (3, true), "window 9 is a probe");
        assert_eq!(t.close(1000), 3, "the probe closes");
        assert_eq!(t.close(1000), 3, "window 10: two toward the rise");
        assert_eq!(t.close(1000), 4, "window 11: three (window 8 counted)");
    }

    #[test]
    fn test_a_slow_floor_window_is_measured_again_and_never_halves() {
        let mut t = Tuner::new(8);
        let _ = t.close(1000);
        good(&mut t, 7);
        assert!(t.at_floor(), "window 9: a probe");
        // The probe at 1.5x the floor before it: over the rise line, so the
        // floor is measured again; over 1.4x too, but a floor window says
        // nothing of the budget.
        assert_eq!(t.close(1500), FLOOR);
        assert!(t.at_floor());
        assert_eq!((t.budget(), t.floor_p99()), (3, Some(1500)));
        // Window 10, the floor again, not slower than window 9's: back to
        // the budget, judged against the new floor.
        assert_eq!(t.close(1000), 3);
        assert_eq!(t.floor_p99(), Some(1000));
        // Exactly 1.25x the floor before it is no re-probe.
        let mut u = Tuner::new(8);
        let _ = u.close(1000);
        good(&mut u, 7);
        assert_eq!(u.close(1250), u.budget());
        assert!(!u.at_floor());
    }

    #[test]
    fn test_windows_are_judged_against_the_latest_floor() {
        let mut t = Tuner::new(8);
        let _ = t.close(2000);
        // 2400 is within 1.25x of a 2000 floor: rises after windows 4 and
        // 7 (a slow floor makes the line slack: the documented transient
        // bound).
        assert_eq!(
            run(&mut t, &[2400, 2400, 2400, 2400, 2400, 2400]),
            [1, 1, 2, 2, 2, 3]
        );
        let _ = t.close(2400);
        assert!(t.at_floor(), "window 9: a probe");
        // The probe finds the floor at 1000: 2400 is now over 1.4x of it.
        assert_eq!(t.close(1000), 3);
        assert_eq!(t.close(2400), FLOOR, "window 10 halves");
        assert_eq!(t.budget(), 1);
    }

    #[test]
    fn test_every_eighth_window_runs_at_the_floor() {
        let mut t = Tuner::new(8);
        // Every window as fast as the floor: only window 1 and the probes
        // are at it.
        for closed in 1..=24u64 {
            let _ = t.close(1000);
            let next = closed + 1;
            assert_eq!(t.at_floor(), next % 8 == 1, "window {next}");
        }
    }

    #[test]
    fn test_the_budget_never_halves_below_the_floor_nor_rises_past_its_ceiling() {
        let mut t = Tuner::new(2);
        let _ = t.close(1000);
        assert_eq!(t.close(5000), FLOOR);
        assert!(t.at_floor(), "a halving at the floor still re-measures it");
        assert_eq!(t.budget(), FLOOR);
        good(&mut t, 21);
        assert_eq!(t.budget(), 2);
        assert_eq!(
            Tuner::new(0).current(),
            FLOOR,
            "a ceiling of 0 is the floor"
        );
        let mut z = Tuner::new(0);
        good(&mut z, 11);
        assert_eq!(z.budget(), FLOOR);
    }

    #[test]
    fn test_over_compares_exactly_and_never_overflows() {
        assert!(!over(1250, 1000, 125));
        assert!(over(1251, 1000, 125));
        assert!(!over(1400, 1000, 140));
        assert!(over(1401, 1000, 140));
        assert!(over(u64::MAX, u64::MAX / 2, 140));
        assert!(!over(u64::MAX, u64::MAX, 125));
    }
}
