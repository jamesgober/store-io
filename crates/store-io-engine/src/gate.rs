//! Per-region admission, so recycle and release never race a write.
//!
//! Every data operation on a region holds a [`Pass`] for its whole I/O.
//! Recycling or releasing the region first *retires* it (no new pass is
//! issued) and then waits until every pass in flight has been returned, so
//! no write of an old generation can land after the region's header says the
//! generation is over, and no read returns bytes of a generation it does not
//! belong to.
//!
//! An operation announces itself (increments `active`) and then reads the
//! state; retiring sets the state and then reads `active`. Every one of these
//! accesses is a read-modify-write (a read is `fetch_add(0)`): the
//! read-modify-writes of one location are totally ordered and each
//! synchronises with the one it reads from, so either the operation sees the
//! retirement and backs out, or the retirer sees the operation and waits for
//! it. This holds under acquire-release alone, without relying on
//! sequentially consistent loads (which loom does not model). The wait is on
//! a condition variable signalled by the last operation out; there is no
//! timer.

use std::sync::PoisonError;

#[cfg(loom)]
use loom::sync::{Condvar, Mutex};
#[cfg(not(loom))]
use std::sync::{Condvar, Mutex};

use crate::sync::{AtomicU64, Ordering};

/// The region is live.
const LIVE: u64 = 0;
/// The region was recycled: this state belongs to an older generation.
const RECYCLED: u64 = 1;
/// The region was released.
const RELEASED: u64 = 2;

/// Why a retired region refused an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Retired {
    /// A newer generation exists.
    Recycled,
    /// The region was released.
    Released,
}

/// What a retirement turns the region into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Retire {
    Recycle,
    Release,
}

/// Admission control for one region.
#[derive(Debug)]
pub(crate) struct Gate {
    active: AtomicU64,
    state: AtomicU64,
    lock: Mutex<()>,
    idle: Condvar,
}

/// Proof that an operation was admitted; returned on drop.
#[must_use]
pub(crate) struct Pass<'a> {
    gate: &'a Gate,
}

impl Drop for Pass<'_> {
    fn drop(&mut self) {
        self.gate.leave();
    }
}

fn retired(state: u64) -> Retired {
    if state == RELEASED {
        Retired::Released
    } else {
        Retired::Recycled
    }
}

impl Gate {
    pub(crate) fn new() -> Self {
        Self {
            active: AtomicU64::new(0),
            state: AtomicU64::new(LIVE),
            lock: Mutex::new(()),
            idle: Condvar::new(),
        }
    }

    /// Admits one operation, or says why the region no longer takes any.
    pub(crate) fn enter(&self) -> Result<Pass<'_>, Retired> {
        let _was = self.active.fetch_add(1, Ordering::AcqRel);
        match self.state.fetch_add(0, Ordering::AcqRel) {
            LIVE => Ok(Pass { gate: self }),
            s => {
                self.leave();
                Err(retired(s))
            }
        }
    }

    /// Whether the region is retired, and how (no admission).
    pub(crate) fn check(&self) -> Result<(), Retired> {
        match self.state.load(Ordering::SeqCst) {
            LIVE => Ok(()),
            s => Err(retired(s)),
        }
    }

    fn leave(&self) {
        if self.active.fetch_sub(1, Ordering::AcqRel) == 1
            && self.state.fetch_add(0, Ordering::AcqRel) != LIVE
        {
            let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
            self.idle.notify_all();
        }
    }

    /// Refuses new operations and waits for every one in flight to finish.
    /// Must not be called while the caller holds a [`Pass`] of this gate.
    ///
    /// # Errors
    ///
    /// The region was already retired (by another recycle or release).
    pub(crate) fn retire(&self, how: Retire) -> Result<(), Retired> {
        let to = match how {
            Retire::Recycle => RECYCLED,
            Retire::Release => RELEASED,
        };
        let _was_live = self
            .state
            .compare_exchange(LIVE, to, Ordering::AcqRel, Ordering::Acquire)
            .map_err(retired)?;
        let mut guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        while self.active.fetch_add(0, Ordering::AcqRel) != 0 {
            guard = self
                .idle
                .wait(guard)
                .unwrap_or_else(PoisonError::into_inner);
        }
        Ok(())
    }

    /// Takes back a retirement that failed before anything changed on disk.
    pub(crate) fn restore(&self) {
        self.state.store(LIVE, Ordering::SeqCst);
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn test_retire_waits_for_every_pass_and_refuses_new_ones() {
        let gate = Arc::new(Gate::new());
        let pass = gate.enter();
        assert!(pass.is_ok());
        let retirer = {
            let gate = Arc::clone(&gate);
            std::thread::spawn(move || gate.retire(Retire::Recycle))
        };
        // The retirer cannot finish while the pass is held.
        while gate.check().is_ok() {
            std::thread::yield_now();
        }
        assert_eq!(gate.enter().err(), Some(Retired::Recycled));
        // Correct code can never finish while the pass is held; a retirer
        // that does not wait finishes within microseconds.
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!retirer.is_finished(), "retire returned with a pass held");
        drop(pass);
        assert_eq!(retirer.join().ok(), Some(Ok(())));
        assert_eq!(gate.retire(Retire::Release), Err(Retired::Recycled));
        gate.restore();
        assert!(gate.enter().is_ok());
    }

    #[test]
    fn test_released_gate_reports_release() {
        let gate = Gate::new();
        assert_eq!(gate.retire(Retire::Release), Ok(()));
        assert_eq!(gate.enter().err(), Some(Retired::Released));
        assert_eq!(gate.check(), Err(Retired::Released));
    }
}

#[cfg(all(test, loom))]
mod loom_tests {
    use super::*;
    use loom::sync::Arc;
    use loom::sync::atomic::AtomicBool;

    /// An operation racing a retirement either backs out or is waited for:
    /// it never runs after the retirement completes.
    #[test]
    fn loom_no_operation_outlives_a_retirement() {
        loom::model(|| {
            let gate = Arc::new(Gate::new());
            let retired = Arc::new(AtomicBool::new(false));
            let op = {
                let gate = Arc::clone(&gate);
                let retired = Arc::clone(&retired);
                loom::thread::spawn(move || {
                    if let Ok(pass) = gate.enter() {
                        assert!(!retired.load(Ordering::SeqCst), "ran after retirement");
                        drop(pass);
                    }
                })
            };
            if gate.retire(Retire::Recycle).is_ok() {
                retired.store(true, Ordering::SeqCst);
            }
            let _joined = op.join();
        });
    }
}
