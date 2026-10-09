//! The append frontier of one append region: where the next append goes, and
//! how far the region is contiguously written and contiguously durable.
//!
//! Appends reserve space from an atomic tail, so concurrent appenders never
//! overlap and a written block is never rewritten. Writes may complete out of
//! order; [`AppendFrontier::complete`] records each completion in a bounded
//! lock-free ring indexed by the reservation's first block, and whoever
//! completes the next range in order advances the **completed prefix**. A
//! barrier waits for the prefix to pass its ticket, then flushes; the
//! **durable frontier** is published after the flush succeeds.
//!
//! Space is reserved only after the write's buffer is ready and every check
//! has passed, immediately before submission. Any failure after a reservation
//! poisons the device domain (the reserved range is a hole that later appends
//! must never be acknowledged past), so the frontier never needs to undo a
//! reservation.

#[cfg(loom)]
use loom::sync::{Condvar, Mutex};
#[cfg(not(loom))]
use std::sync::{Condvar, Mutex};

use store_io_core::error::NotWrittenCause;

use crate::sync::{AtomicU64, Ordering};

/// Bounded, lock-free append frontier.
#[derive(Debug)]
pub struct AppendFrontier {
    shift: u32,
    limit: u64,
    tail: AtomicU64,
    prefix: AtomicU64,
    durable: AtomicU64,
    ring: Box<[AtomicU64]>,
    mask: u64,
    waiters: AtomicU64,
    wait: Mutex<()>,
    wake: Condvar,
}

/// A reserved append range `[start, end)` in region bytes; `end` is block
/// aligned and `start + len` is where the caller's bytes end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reservation {
    /// First byte.
    pub start: u64,
    /// One past the caller's last byte.
    pub data_end: u64,
    /// One past the reserved range (next block boundary).
    pub end: u64,
}

fn pack(start_block: u64, end_block: u64) -> u64 {
    (start_block << 32) | (end_block & 0xFFFF_FFFF)
}

fn unpack(v: u64) -> (u64, u64) {
    (v >> 32, v & 0xFFFF_FFFF)
}

impl AppendFrontier {
    /// A frontier starting at `resume` (block aligned) in a region of `limit`
    /// bytes, with `block = 1 << shift`, allowing at most `ring` (a power of
    /// two) blocks between the completed prefix and a new reservation's start.
    ///
    /// Regions are limited to `2^32 - 1` blocks (16 TiB at 4 KiB).
    #[must_use]
    pub fn new(resume: u64, limit: u64, shift: u32, ring: usize) -> Self {
        let ring = ring.max(2).next_power_of_two();
        Self {
            shift,
            limit,
            tail: AtomicU64::new(resume),
            prefix: AtomicU64::new(resume),
            durable: AtomicU64::new(resume),
            ring: (0..ring).map(|_| AtomicU64::new(0)).collect(),
            mask: ring as u64 - 1,
            waiters: AtomicU64::new(0),
            wait: Mutex::new(()),
            wake: Condvar::new(),
        }
    }

    /// Reserves space for `len` caller bytes.
    ///
    /// # Errors
    ///
    /// [`NotWrittenCause::Empty`] for `len == 0`, [`NotWrittenCause::RegionFull`]
    /// if the region cannot hold it, [`NotWrittenCause::TooManyInFlight`] if the
    /// reservation would start further than the ring allows past the completed
    /// prefix. Nothing is reserved on error.
    pub fn reserve(&self, len: u64) -> Result<Reservation, NotWrittenCause> {
        if len == 0 {
            return Err(NotWrittenCause::Empty);
        }
        let block = 1u64 << self.shift;
        let mut start = self.tail.load(Ordering::SeqCst);
        loop {
            let data_end = start.checked_add(len).ok_or(NotWrittenCause::RegionFull)?;
            let end = data_end
                .checked_add(block - 1)
                .ok_or(NotWrittenCause::RegionFull)?
                & !(block - 1);
            if end > self.limit {
                return Err(NotWrittenCause::RegionFull);
            }
            let prefix = self.prefix.load(Ordering::SeqCst);
            if (start >> self.shift).saturating_sub(prefix >> self.shift) > self.mask {
                return Err(NotWrittenCause::TooManyInFlight);
            }
            match self
                .tail
                .compare_exchange_weak(start, end, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => {
                    return Ok(Reservation {
                        start,
                        data_end,
                        end,
                    });
                }
                Err(current) => start = current,
            }
        }
    }

    /// Reserves like [`Self::reserve`], but while too many appends are in
    /// flight ahead of the new one it waits for the completed prefix to
    /// advance instead of failing. A completion always wakes it (there is an
    /// in-flight reservation whenever the ring is full), and `give_up` (the
    /// domain was poisoned, which also wakes waiters) ends the wait.
    ///
    /// # Errors
    ///
    /// As [`Self::reserve`]; [`NotWrittenCause::TooManyInFlight`] only when
    /// `give_up` returned true.
    pub fn reserve_wait(
        &self,
        len: u64,
        give_up: impl Fn() -> bool,
    ) -> Result<Reservation, NotWrittenCause> {
        loop {
            match self.reserve(len) {
                Err(NotWrittenCause::TooManyInFlight) => {
                    let p = self.prefix.load(Ordering::SeqCst);
                    if !self.wait_completed(p + 1, &give_up) {
                        return Err(NotWrittenCause::TooManyInFlight);
                    }
                }
                other => return other,
            }
        }
    }

    /// Records that the write of `r` completed, and advances the completed
    /// prefix as far as contiguous completions allow.
    ///
    /// Every access to a ring slot is a read-modify-write with acquire-release
    /// ordering. A marker publishes its slot and then reads the prefix; the
    /// advancer publishes the prefix and then reads the next slot. Because
    /// read-modify-writes on one slot are totally ordered and synchronise,
    /// either the advancer's slot read sees the mark (and advances past it),
    /// or the marker's slot write follows the advancer's slot read and so the
    /// marker sees the advanced prefix (and advances itself). No completion is
    /// ever stranded, without relying on sequentially consistent fences.
    pub fn complete(&self, r: Reservation) {
        let sb = r.start >> self.shift;
        let eb = r.end >> self.shift;
        let _previous = self.ring[(sb & self.mask) as usize].swap(pack(sb, eb), Ordering::AcqRel);
        let mut advanced = false;
        loop {
            let p = self.prefix.load(Ordering::Acquire);
            let pb = p >> self.shift;
            let slot = &self.ring[(pb & self.mask) as usize];
            let v = slot.fetch_or(0, Ordering::AcqRel);
            if v == 0 {
                break;
            }
            let (s, e) = unpack(v);
            if s != pb {
                break;
            }
            // Take the slot first so only one thread advances past it and a
            // new reservation reusing the slot index is never erased.
            if slot
                .compare_exchange(v, 0, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                self.prefix.store(e << self.shift, Ordering::Release);
                advanced = true;
            }
        }
        if advanced && self.waiters.load(Ordering::SeqCst) > 0 {
            let _guard = self
                .wait
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.wake.notify_all();
        }
    }

    /// Blocks until the completed prefix reaches `end`, or `give_up` returns
    /// true (the domain was poisoned). Returns whether the prefix was reached.
    pub fn wait_completed(&self, end: u64, give_up: impl Fn() -> bool) -> bool {
        if self.prefix.load(Ordering::SeqCst) >= end {
            return true;
        }
        let _registered = self.waiters.fetch_add(1, Ordering::SeqCst);
        let mut guard = self
            .wait
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let reached = loop {
            if self.prefix.load(Ordering::SeqCst) >= end {
                break true;
            }
            if give_up() {
                break false;
            }
            guard = self
                .wake
                .wait(guard)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        };
        drop(guard);
        let _unregistered = self.waiters.fetch_sub(1, Ordering::SeqCst);
        reached
    }

    /// Wakes every waiter (used when the domain is poisoned).
    pub fn wake_all(&self) {
        let _guard = self
            .wait
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.wake.notify_all();
    }

    /// Contiguously completed bytes.
    #[must_use]
    pub fn completed(&self) -> u64 {
        self.prefix.load(Ordering::SeqCst)
    }

    /// Contiguously durable bytes.
    #[must_use]
    pub fn durable(&self) -> u64 {
        self.durable.load(Ordering::SeqCst)
    }

    /// Publishes a durable frontier after a successful barrier. Never moves
    /// backwards.
    pub fn publish_durable(&self, through: u64) {
        let _previous = self.durable.fetch_max(through, Ordering::SeqCst);
    }

    /// The next reservation's start.
    #[must_use]
    pub fn tail(&self) -> u64 {
        self.tail.load(Ordering::SeqCst)
    }

    /// Moves the tail forward to `offset` rounded up to a block, when nothing
    /// is in flight. Never moves it backwards.
    ///
    /// # Errors
    ///
    /// [`NotWrittenCause::TooManyInFlight`] if appends are in flight,
    /// [`NotWrittenCause::OutOfBounds`] past the region end.
    pub fn resume_at(&self, offset: u64) -> Result<u64, NotWrittenCause> {
        let block = 1u64 << self.shift;
        let target = offset
            .checked_add(block - 1)
            .ok_or(NotWrittenCause::OutOfBounds)?
            & !(block - 1);
        if target > self.limit {
            return Err(NotWrittenCause::OutOfBounds);
        }
        let tail = self.tail.load(Ordering::SeqCst);
        if self.prefix.load(Ordering::SeqCst) != tail {
            return Err(NotWrittenCause::TooManyInFlight);
        }
        let new = target.max(tail);
        if self
            .tail
            .compare_exchange(tail, new, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(NotWrittenCause::TooManyInFlight);
        }
        self.prefix.store(new, Ordering::SeqCst);
        let _previous = self.durable.fetch_max(new, Ordering::SeqCst);
        Ok(new)
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn f() -> AppendFrontier {
        AppendFrontier::new(0, 1 << 20, 12, 64)
    }

    #[test]
    fn test_reserve_rounds_to_blocks_and_never_overlaps() {
        let fr = f();
        let a = fr.reserve(100).unwrap_or_else(|e| panic!("{e:?}"));
        let b = fr.reserve(5000).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(
            a,
            Reservation {
                start: 0,
                data_end: 100,
                end: 4096
            }
        );
        assert_eq!(
            b,
            Reservation {
                start: 4096,
                data_end: 9096,
                end: 12288
            }
        );
        assert_eq!(fr.reserve(0), Err(NotWrittenCause::Empty));
        assert_eq!(fr.reserve(1 << 20), Err(NotWrittenCause::RegionFull));
    }

    #[test]
    fn test_out_of_order_completion_advances_prefix_only_when_contiguous() {
        let fr = f();
        let r: Vec<_> = (0..4)
            .map(|_| fr.reserve(4096).unwrap_or_else(|e| panic!("{e:?}")))
            .collect();
        fr.complete(r[2]);
        fr.complete(r[1]);
        assert_eq!(fr.completed(), 0);
        fr.complete(r[0]);
        assert_eq!(fr.completed(), 3 * 4096);
        fr.complete(r[3]);
        assert_eq!(fr.completed(), 4 * 4096);
    }

    #[test]
    fn test_ring_bound_refuses_runaway_reservations() {
        let fr = AppendFrontier::new(0, 1 << 30, 12, 4);
        for _ in 0..4 {
            assert!(fr.reserve(4096).is_ok());
        }
        assert_eq!(fr.reserve(4096), Err(NotWrittenCause::TooManyInFlight));
    }

    #[test]
    fn test_resume_only_moves_forward_and_only_when_idle() {
        let fr = f();
        assert_eq!(fr.resume_at(5000), Ok(8192));
        assert_eq!(fr.resume_at(100), Ok(8192));
        let r = fr.reserve(1).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(r.start, 8192);
        assert_eq!(fr.resume_at(1 << 19), Err(NotWrittenCause::TooManyInFlight));
        fr.complete(r);
        assert_eq!(fr.durable(), 8192);
        fr.publish_durable(r.end);
        fr.publish_durable(0);
        assert_eq!(fr.durable(), r.end);
    }

    #[test]
    fn test_concurrent_appenders_reach_a_gapless_prefix() {
        let fr = Arc::new(AppendFrontier::new(0, 1 << 30, 12, 1024));
        let handles: Vec<_> = (0..8)
            .map(|t| {
                let fr = Arc::clone(&fr);
                std::thread::spawn(move || {
                    let mut seed = 0x9E37_79B9u64 + t;
                    for _ in 0..2_000 {
                        seed ^= seed << 13;
                        seed ^= seed >> 7;
                        seed ^= seed << 17;
                        let len = 1 + seed % 20_000;
                        loop {
                            match fr.reserve(len) {
                                Ok(r) => {
                                    fr.complete(r);
                                    break;
                                }
                                Err(NotWrittenCause::TooManyInFlight) => std::hint::spin_loop(),
                                Err(e) => panic!("{e:?}"),
                            }
                        }
                    }
                })
            })
            .collect();
        for h in handles {
            assert!(h.join().is_ok());
        }
        assert_eq!(
            fr.completed(),
            fr.tail(),
            "every reservation completed, so the prefix reaches the tail"
        );
    }

    #[test]
    fn test_waiter_wakes_when_prefix_arrives() {
        let fr = Arc::new(f());
        let a = fr.reserve(4096).unwrap_or_else(|e| panic!("{e:?}"));
        let b = fr.reserve(4096).unwrap_or_else(|e| panic!("{e:?}"));
        fr.complete(b);
        let waiter = {
            let fr = Arc::clone(&fr);
            std::thread::spawn(move || fr.wait_completed(b.end, || false))
        };
        std::thread::yield_now();
        fr.complete(a);
        assert_eq!(waiter.join().ok(), Some(true));
    }

    #[test]
    fn test_reserve_wait_blocks_until_the_ring_drains() {
        let fr = Arc::new(AppendFrontier::new(0, 1 << 30, 12, 4));
        let held: Vec<_> = (0..4)
            .map(|_| fr.reserve(4096).unwrap_or_else(|e| panic!("{e:?}")))
            .collect();
        assert_eq!(fr.reserve(4096), Err(NotWrittenCause::TooManyInFlight));
        let started = Arc::new(std::sync::Barrier::new(2));
        let waiter = {
            let fr = Arc::clone(&fr);
            let started = Arc::clone(&started);
            std::thread::spawn(move || {
                let _leader = started.wait();
                fr.reserve_wait(4096, || false)
            })
        };
        let _leader = started.wait();
        // The waiter cannot proceed while the ring is full; completing the
        // oldest reservation frees it.
        for r in held {
            fr.complete(r);
        }
        let got = waiter.join().ok().and_then(Result::ok);
        assert_eq!(got.map(|r| r.start), Some(4 * 4096));
    }

    #[test]
    fn test_reserve_wait_gives_up_when_told() {
        let fr = Arc::new(AppendFrontier::new(0, 1 << 30, 12, 4));
        for _ in 0..4 {
            assert!(fr.reserve(4096).is_ok());
        }
        let give_up = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let waiter = {
            let fr = Arc::clone(&fr);
            let give_up = Arc::clone(&give_up);
            std::thread::spawn(move || {
                fr.reserve_wait(4096, || give_up.load(std::sync::atomic::Ordering::SeqCst))
            })
        };
        give_up.store(true, std::sync::atomic::Ordering::SeqCst);
        fr.wake_all();
        // A waiter that registered after the wake still sees `give_up` on
        // its first check.
        assert_eq!(
            waiter.join().ok(),
            Some(Err(NotWrittenCause::TooManyInFlight))
        );
    }
}

#[cfg(all(test, loom))]
mod loom_tests {
    use super::*;
    use loom::sync::Arc;

    /// Two appenders complete in either order; the prefix ends at the tail
    /// with no slot left behind.
    #[test]
    fn loom_two_appenders_reach_the_tail() {
        loom::model(|| {
            let fr = Arc::new(AppendFrontier::new(0, 1 << 20, 12, 4));
            let h: Vec<_> = (0..2)
                .map(|_| {
                    let fr = fr.clone();
                    loom::thread::spawn(move || {
                        if let Ok(r) = fr.reserve(4096) {
                            fr.complete(r);
                        }
                    })
                })
                .collect();
            for t in h {
                let _joined = t.join();
            }
            assert_eq!(fr.completed(), fr.tail());
        });
    }
}
