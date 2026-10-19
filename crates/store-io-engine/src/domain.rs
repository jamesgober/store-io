//! The flush domain: one per device (one per container file in the default
//! layout). It decides when a barrier needs a new device flush and lets
//! concurrent barriers share one, with no timer, no window and no waiting
//! for company.
//!
//! # The join rule
//!
//! A barrier covering writes `W` (all already completed) loads
//! `e = issued` **after** the last write in `W` completed. Flush number
//! `e + 1` is issued only after `issued` is incremented to `e + 1`, which the
//! load preceded, so flush `e + 1` was issued after every write in `W`
//! completed and covers them (a device flush persists every write that
//! completed before it was submitted). The barrier therefore needs
//! `done >= e + 1`, where `done` is the highest `k` such that flushes `1..=k`
//! all succeeded.
//!
//! # Leader-inline flushing
//!
//! At most one flush is in flight per domain. A barrier that needs a flush and
//! finds none running becomes the **leader**: it increments `issued` *before*
//! calling the flush, runs the flush on its own thread (no hand-off: measured
//! at +7.7% p50 on Windows when handed to a dedicated thread), publishes
//! `done`, and wakes the **followers**. A follower that needed a later flush
//! than the one that just finished loops and may lead the next one. A lone
//! writer therefore pays exactly one flush; concurrent writers share flushes.
//!
//! # Poison
//!
//! The first failed flush poisons the domain forever: `done` never advances
//! again, the leader gets the failure, and every waiting or later barrier gets
//! [`Error::Poisoned`]. A barrier that was already satisfied before it
//! observed the poison keeps its result. A flush is never retried.

use std::sync::OnceLock;

#[cfg(loom)]
use loom::sync::{Condvar, Mutex};
#[cfg(not(loom))]
use std::sync::{Condvar, Mutex};

use store_io_core::error::{Error, ErrorContext, FirstCause, Op, OsError};

use crate::sync::{AtomicBool, AtomicU64, Ordering};

/// Counters a domain exposes for observability.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DomainStats {
    /// Flushes issued.
    pub flushes: u64,
    /// Barriers satisfied by a flush another barrier issued.
    pub joined: u64,
    /// Barriers that issued the flush themselves.
    pub led: u64,
    /// Receipts returned without a barrier because the target was already
    /// durable (SIO-TYP-02: a barrier is never a silent no-op).
    pub already_durable: u64,
}

/// Observability counters, on a cache line of their own: incrementing them
/// must not evict the line every barrier reads (`issued`, `done`, `leader`).
#[derive(Debug)]
#[repr(align(128))]
struct Counters {
    joined: AtomicU64,
    led: AtomicU64,
    already_durable: AtomicU64,
}

/// A flush domain.
#[derive(Debug)]
pub struct Domain {
    issued: AtomicU64,
    done: AtomicU64,
    leader: AtomicBool,
    counters: Counters,
    poison: OnceLock<FirstCause>,
    /// Followers inside `follow` (registered under `wait`): a leader
    /// notifies only when there is one, so a lone flush makes no futex call.
    waiters: AtomicU64,
    wait: Mutex<()>,
    wake: Condvar,
}

impl Default for Domain {
    fn default() -> Self {
        Self::new()
    }
}

impl Domain {
    /// A fresh, healthy domain.
    #[must_use]
    pub fn new() -> Self {
        Self {
            issued: AtomicU64::new(0),
            done: AtomicU64::new(0),
            leader: AtomicBool::new(false),
            counters: Counters {
                joined: AtomicU64::new(0),
                led: AtomicU64::new(0),
                already_durable: AtomicU64::new(0),
            },
            poison: OnceLock::new(),
            waiters: AtomicU64::new(0),
            wait: Mutex::new(()),
            wake: Condvar::new(),
        }
    }

    /// The failure that poisoned the domain, if any.
    #[must_use]
    pub fn poisoned(&self) -> Option<FirstCause> {
        self.poison.get().copied()
    }

    /// Poisons the domain (explicitly by a caller's watchdog, or after a
    /// failed write). Wakes every follower. Idempotent: the first cause wins.
    pub fn poison(&self, cause: FirstCause) {
        let _first_cause_kept = self.poison.set(cause);
        self.notify();
    }

    /// Observability counters.
    #[must_use]
    pub fn stats(&self) -> DomainStats {
        DomainStats {
            flushes: self.issued.load(Ordering::Acquire),
            joined: self.counters.joined.load(Ordering::Relaxed),
            led: self.counters.led.load(Ordering::Relaxed),
            already_durable: self.counters.already_durable.load(Ordering::Relaxed),
        }
    }

    /// Counts a receipt returned without a barrier because its target was
    /// already durable.
    #[inline]
    pub fn count_already_durable(&self) {
        let _previous = self
            .counters
            .already_durable
            .fetch_add(1, Ordering::Relaxed);
    }

    /// The ticket a barrier needs, to be taken **after** every write it must
    /// cover has completed (and its completion has been observed).
    #[inline]
    #[must_use]
    pub fn ticket(&self) -> u64 {
        self.issued.load(Ordering::SeqCst).wrapping_add(1)
    }

    /// Whether a flush `>= need` has already succeeded (a barrier for it
    /// would return at once); counted as a joined barrier when it has.
    pub(crate) fn join_if_done(&self, need: u64) -> bool {
        let done = self.done.load(Ordering::SeqCst) >= need;
        if done {
            let _count = self.counters.joined.fetch_add(1, Ordering::Relaxed);
        }
        done
    }

    /// Blocks until a flush issued after `need - 1` has succeeded, issuing it
    /// on this thread if no covering flush is running.
    ///
    /// `flush` performs one device flush and returns the raw outcome; it is
    /// called at most once per call to `barrier`, and only by the leader.
    ///
    /// # Errors
    ///
    /// [`Error::DurabilityUnknown`] if this call led a flush that failed;
    /// [`Error::Poisoned`] if the domain is poisoned.
    pub fn barrier(
        &self,
        need: u64,
        mut flush: impl FnMut() -> Result<(), Option<OsError>>,
    ) -> Result<(), Error> {
        let mut led_flush = false;
        loop {
            if self.done.load(Ordering::SeqCst) >= need {
                if !led_flush {
                    let _count = self.counters.joined.fetch_add(1, Ordering::Relaxed);
                }
                return Ok(());
            }
            if let Some(first) = self.poison.get() {
                return Err(Error::Poisoned { first: *first });
            }
            if self
                .leader
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                // Leading. Re-check under leadership: a previous leader may
                // have satisfied us or poisoned the domain in between.
                if self.done.load(Ordering::SeqCst) >= need || self.poison.get().is_some() {
                    self.release_leadership();
                    continue;
                }
                // `issued == done` here (one flush in flight at most, and none
                // is), so k = issued + 1 >= need.
                let k = self.issued.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
                led_flush = true;
                let _count = self.counters.led.fetch_add(1, Ordering::Relaxed);
                // A flush that panics leaves the device's state unknown:
                // the guard poisons the domain and gives leadership back, so
                // no later barrier waits for a leader that is gone.
                let unwinding = LeaderGuard { domain: self };
                let outcome = flush();
                core::mem::forget(unwinding);
                match outcome {
                    Ok(()) => {
                        if self.poison.get().is_none() {
                            self.done.store(k, Ordering::SeqCst);
                        }
                        self.release_leadership();
                    }
                    Err(raw) => {
                        let _first_cause_kept = self.poison.set(FirstCause {
                            op: Some(Op::FlushData),
                            raw,
                        });
                        self.release_leadership();
                        return Err(Error::DurabilityUnknown {
                            op: Op::FlushData,
                            raw,
                            ctx: ErrorContext::default(),
                        });
                    }
                }
            } else {
                self.follow(need);
            }
        }
    }

    /// Takes flush leadership, as a leader flushing would (tests: every
    /// barrier then waits until [`Self::drop_leadership`]).
    #[cfg(test)]
    pub(crate) fn hold_leadership(&self) -> bool {
        self.leader
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    /// Gives back leadership taken by [`Self::hold_leadership`].
    #[cfg(test)]
    pub(crate) fn drop_leadership(&self) {
        self.release_leadership();
    }

    /// Gives leadership back and wakes the followers, if any. Lost-wake
    /// free without relying on sequentially consistent loads (as
    /// `QueueSet`'s waiters): a follower registers (`waiters` read-modify-
    /// write) and then reads `leader` with a read-modify-write, all under
    /// `wait`; the leader clears `leader` (read-modify-write) and then reads
    /// `waiters` (read-modify-write). Read-modify-writes of one location are
    /// totally ordered and synchronise, so either the follower's read sees
    /// the leader gone (it does not wait), or the leader's read sees the
    /// follower and notifies under `wait`, which the follower holds until
    /// it is waiting.
    /// Poisons the domain (a flush whose outcome is unknown) and wakes
    /// every waiter.
    fn poison_unknown_flush(&self) {
        let _first_cause_kept = self.poison.set(FirstCause {
            op: Some(Op::FlushData),
            raw: None,
        });
        self.release_leadership();
        self.notify();
    }

    fn release_leadership(&self) {
        let _was = self.leader.swap(false, Ordering::AcqRel);
        if self.waiters.fetch_add(0, Ordering::AcqRel) > 0 {
            self.notify();
        }
    }

    fn notify(&self) {
        let _guard = self
            .wait
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.wake.notify_all();
    }

    /// Waits while a leader is flushing and the need is unmet. The checks are
    /// repeated under the wait mutex, so a leader that releases between the
    /// check and the wait cannot be missed (it notifies under the same mutex).
    fn follow(&self, need: u64) {
        let mut guard = self
            .wait
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _registered = self.waiters.fetch_add(1, Ordering::AcqRel);
        while self.leader.fetch_or(false, Ordering::AcqRel)
            && self.done.load(Ordering::SeqCst) < need
            && self.poison.get().is_none()
        {
            guard = self
                .wake
                .wait(guard)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        let _unregistered = self.waiters.fetch_sub(1, Ordering::AcqRel);
        drop(guard);
    }
}

/// Held by a flush leader while its flush runs; dropped only if the flush
/// panics (the leader forgets it otherwise).
struct LeaderGuard<'a> {
    domain: &'a Domain,
}

impl Drop for LeaderGuard<'_> {
    fn drop(&mut self) {
        self.domain.poison_unknown_flush();
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    use store_io_core::error::OsErrorSource;

    const EIO: OsError = OsError {
        code: 5,
        source: OsErrorSource::Sim,
    };

    /// A follower waiting behind a leader is woken when leadership is given
    /// back (the leader notifies only when a follower is registered), and a
    /// lone barrier registers nobody.
    #[test]
    fn test_a_waiting_follower_is_woken_when_the_leader_returns() {
        let d = Arc::new(Domain::new());
        assert!(d.hold_leadership());
        let need = d.ticket();
        let (tx, rx) = std::sync::mpsc::channel();
        let follower = {
            let d = Arc::clone(&d);
            std::thread::spawn(move || {
                let r = d.barrier(need, || Ok(()));
                let _sent = tx.send(r.is_ok());
            })
        };
        // Until the follower is registered and waiting.
        let start = std::time::Instant::now();
        while d.waiters.load(Ordering::SeqCst) == 0 {
            assert!(
                start.elapsed() < std::time::Duration::from_secs(10),
                "never registered"
            );
            std::thread::yield_now();
        }
        // The follower holds `wait` from registering until it is parked in
        // the condvar (it cannot leave while leadership is held): taking it
        // proves the follower is waiting, so only a notify can wake it.
        drop(
            d.wait
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        d.drop_leadership();
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_secs(10)),
            Ok(true),
            "the follower was never woken"
        );
        assert!(follower.join().is_ok(), "the follower panicked");
        assert_eq!(d.waiters.load(Ordering::SeqCst), 0);
        // A lone barrier leads and registers nobody.
        let need = d.ticket();
        assert!(d.barrier(need, || Ok(())).is_ok());
        assert_eq!(d.waiters.load(Ordering::SeqCst), 0);
    }

    /// A flush that panics poisons the domain and gives leadership back:
    /// later barriers fail at once instead of waiting for a leader that is
    /// gone.
    #[test]
    fn test_a_panicking_flush_poisons_and_releases_leadership() {
        let d = Domain::new();
        let need = d.ticket();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _r = d.barrier(need, || panic!("the flush panicked"));
        }));
        assert!(r.is_err());
        assert!(
            d.poisoned().is_some(),
            "a flush of unknown outcome did not poison"
        );
        let need = d.ticket();
        assert!(matches!(
            d.barrier(need, || Ok(())),
            Err(Error::Poisoned { .. })
        ));
    }

    #[test]
    fn test_lone_barrier_issues_exactly_one_flush() {
        let d = Domain::new();
        let mut calls = 0;
        let need = d.ticket();
        assert!(
            d.barrier(need, || {
                calls += 1;
                Ok(())
            })
            .is_ok()
        );
        assert_eq!(calls, 1);
        // The same need is now satisfied without another flush.
        assert!(d.barrier(need, || Err(Some(EIO))).is_ok());
        assert_eq!(
            d.stats(),
            DomainStats {
                flushes: 1,
                joined: 1,
                led: 1,
                already_durable: 0
            }
        );
    }

    #[test]
    fn test_ticket_taken_after_a_flush_needs_the_next_flush() {
        let d = Domain::new();
        assert!(d.barrier(d.ticket(), || Ok(())).is_ok());
        let mut calls = 0;
        assert!(
            d.barrier(d.ticket(), || {
                calls += 1;
                Ok(())
            })
            .is_ok()
        );
        assert_eq!(calls, 1, "a write completing after flush 1 needs flush 2");
    }

    #[test]
    fn test_failed_flush_poisons_and_is_never_retried() {
        let d = Domain::new();
        let r = d.barrier(d.ticket(), || Err(Some(EIO)));
        assert!(matches!(
            r,
            Err(Error::DurabilityUnknown {
                op: Op::FlushData,
                ..
            })
        ));
        let mut calls = 0;
        let r = d.barrier(d.ticket(), || {
            calls += 1;
            Ok(())
        });
        assert!(matches!(r, Err(Error::Poisoned { .. })));
        assert_eq!(calls, 0, "a poisoned domain must never flush again");
    }

    #[test]
    fn test_satisfied_barrier_keeps_its_result_after_later_poison() {
        let d = Domain::new();
        let need = d.ticket();
        assert!(d.barrier(need, || Ok(())).is_ok());
        let _ = d.barrier(d.ticket(), || Err(Some(EIO)));
        assert!(
            d.barrier(need, || Ok(())).is_ok(),
            "already-durable data stays durable"
        );
    }

    #[test]
    fn test_explicit_poison_wakes_and_fails_followers() {
        let d = Arc::new(Domain::new());
        d.poison(FirstCause {
            op: None,
            raw: None,
        });
        assert!(matches!(
            d.barrier(d.ticket(), || Ok(())),
            Err(Error::Poisoned { .. })
        ));
    }

    #[test]
    fn test_concurrent_barriers_share_flushes_and_never_cover_late_writes() {
        // Model: a "write" completes at a moment; a flush covers every write
        // completed before the flush started. Each thread checks that the
        // flush that satisfied it started after its write completed.
        let d = Arc::new(Domain::new());
        let clock = Arc::new(AtomicUsize::new(0));
        let flush_starts = Arc::new(Mutex::new(Vec::<(u64, usize)>::new()));
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let (d, clock, starts) = (
                    Arc::clone(&d),
                    Arc::clone(&clock),
                    Arc::clone(&flush_starts),
                );
                std::thread::spawn(move || {
                    for _ in 0..200 {
                        let completed_at = clock.fetch_add(1, Ordering::SeqCst);
                        let need = d.ticket();
                        let r = d.barrier(need, || {
                            let k = d.issued.load(Ordering::SeqCst);
                            starts
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .push((k, clock.fetch_add(1, Ordering::SeqCst)));
                            // A flush takes time, as a real one does, so the
                            // barriers overlap even on a loaded machine (an
                            // instant one lets them run one after another,
                            // sharing nothing).
                            std::thread::sleep(std::time::Duration::from_micros(100));
                            Ok(())
                        });
                        assert!(r.is_ok());
                        let starts = starts
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        let covering = starts.iter().find(|(k, _)| *k >= need);
                        let Some((_, started)) = covering else {
                            panic!("no flush with index ≥ {need}");
                        };
                        assert!(
                            *started > completed_at,
                            "covering flush started before the write completed"
                        );
                    }
                })
            })
            .collect();
        for h in handles {
            assert!(h.join().is_ok());
        }
        let s = d.stats();
        assert_eq!(s.led as usize + s.joined as usize, 16 * 200);
        assert!(
            s.flushes < 16 * 200,
            "concurrent barriers must share flushes ({} flushes)",
            s.flushes
        );
    }
}

#[cfg(all(test, loom))]
mod loom_tests {
    use super::*;
    use loom::sync::Arc;

    use store_io_core::error::OsErrorSource;

    /// Two barriers race; each must observe a flush issued after its ticket,
    /// and a failing flush must poison the other.
    #[test]
    fn loom_two_barriers_share_or_issue_and_poison_propagates() {
        loom::model(|| {
            let d = Arc::new(Domain::new());
            let flushes = Arc::new(loom::sync::atomic::AtomicUsize::new(0));
            let spawn = |fail: bool| {
                let (d, flushes) = (d.clone(), flushes.clone());
                loom::thread::spawn(move || {
                    let need = d.ticket();
                    let r = d.barrier(need, || {
                        let _n = flushes.fetch_add(1, loom::sync::atomic::Ordering::SeqCst);
                        if fail {
                            Err(Some(OsError {
                                code: 5,
                                source: OsErrorSource::Sim,
                            }))
                        } else {
                            Ok(())
                        }
                    });
                    (need, r.is_ok())
                })
            };
            let a = spawn(false);
            let b = spawn(true);
            let (need_a, ok_a) = a.join().unwrap_or((0, false));
            let (_need_b, _ok_b) = b.join().unwrap_or((0, false));
            if ok_a {
                assert!(d.done.load(Ordering::SeqCst) >= need_a);
            }
            assert!(flushes.load(loom::sync::atomic::Ordering::SeqCst) <= 2);
        });
    }
}
