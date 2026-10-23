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
//! The ring is allocated at the region's first completed append (or a
//! `resume_at` that moves its tail), so a store of many append regions
//! (65,536 in format 2) pays for none until each is used: at most 8 MiB of
//! ring per region (`MAX_RING` slots); at the default window, 32 KiB for one
//! of 16 MiB or more at 4 KiB blocks (256 KiB at 512 B).
//!
//! Space is reserved only after the write's buffer is ready and every check
//! has passed, immediately before submission. Any failure after a reservation
//! poisons the device domain (the reserved range is a hole that later appends
//! must never be acknowledged past), so the frontier never needs to undo a
//! reservation.

use std::sync::OnceLock;

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
    /// Contiguously durable bytes made so by a data flush (a forced region
    /// on a power-safe store reads its prefix from here, D-102).
    synced: AtomicU64,
    /// The completion ring, `mask + 1` slots, allocated at the first
    /// completion (at construction under loom, which does not model the
    /// cell).
    ring: OnceLock<Box<[AtomicU64]>>,
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

/// A completion ring of `n` empty slots.
fn slots(n: usize) -> Box<[AtomicU64]> {
    (0..n).map(|_| AtomicU64::new(0)).collect()
}

fn pack(start_block: u64, end_block: u64) -> u64 {
    (start_block << 32) | (end_block & 0xFFFF_FFFF)
}

fn unpack(v: u64) -> (u64, u64) {
    (v >> 32, v & 0xFFFF_FFFF)
}

/// The most ring slots a frontier allocates (8 MiB of slots): a window that
/// asks for more is cut to it.
const MAX_RING: u64 = 1 << 20;

/// Ring slots for a window of `window` bytes in a region of `limit` bytes at
/// blocks of `1 << shift`, at least 2 and at most [`MAX_RING`]. When the
/// window is the bound, its blocks rounded down to a power of two, so the
/// window never holds more than was asked; when the region is no larger, the
/// region's blocks rounded up, so a region never limits its own appends and
/// a small region never pays for a large ring.
#[must_use]
pub(crate) fn ring_for(window: u64, limit: u64, shift: u32) -> usize {
    let (w, r) = (window >> shift, limit.div_ceil(1 << shift));
    let blocks = if r <= w {
        r.max(2).next_power_of_two()
    } else {
        1u64 << (63 - w.max(1).leading_zeros())
    };
    // At most MAX_RING, so the cast is lossless.
    #[allow(clippy::cast_possible_truncation)]
    let slots = blocks.clamp(2, MAX_RING) as usize;
    slots
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
        let cell = OnceLock::new();
        #[cfg(loom)]
        let _set = cell.set(slots(ring));
        Self {
            shift,
            limit,
            tail: AtomicU64::new(resume),
            prefix: AtomicU64::new(resume),
            durable: AtomicU64::new(resume),
            synced: AtomicU64::new(resume),
            ring: cell,
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
    /// domain was poisoned, which also wakes waiters) ends the wait and
    /// refuses any later reservation.
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
            // Never reserve on a poisoned store, even after a wake that made
            // room.
            if give_up() {
                return Err(NotWrittenCause::TooManyInFlight);
            }
            // The prefix a refusal is judged against is at least this one,
            // read before the reservation: waiting for it to move past this
            // never outlasts the appends in flight (read after, every
            // in-flight append could finish first, leaving nothing to wake
            // the wait).
            let p = self.prefix.load(Ordering::SeqCst);
            match self.reserve(len) {
                Err(NotWrittenCause::TooManyInFlight) => {
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
        let ring = self.ring();
        let _previous = ring[(sb & self.mask) as usize].swap(pack(sb, eb), Ordering::AcqRel);
        if self.advance(ring) {
            self.wake_if_waiting();
        }
    }

    /// The completion ring, allocated on first use (once, whoever asks
    /// first: every other caller waits for it and sees it whole).
    fn ring(&self) -> &[AtomicU64] {
        self.ring.get_or_init(|| slots(self.mask as usize + 1))
    }

    /// Slots of the completion ring allocated so far: 0 before the first
    /// completion.
    #[cfg(all(test, not(loom)))]
    pub(crate) fn ring_slots(&self) -> usize {
        self.ring.get().map_or(0, |r| r.len())
    }

    /// Advances the completed prefix over the contiguous marks from where it
    /// stands; whether it moved.
    fn advance(&self, ring: &[AtomicU64]) -> bool {
        let mut advanced = false;
        loop {
            let p = self.prefix.load(Ordering::Acquire);
            let pb = p >> self.shift;
            let slot = &ring[(pb & self.mask) as usize];
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
        advanced
    }

    /// Wakes every waiter, if one is registered, after the prefix moved.
    fn wake_if_waiting(&self) {
        // A read-modify-write, after the prefix store: the waiter registers
        // with one too and then reads the prefix, and read-modify-writes of
        // one location are totally ordered and synchronise, so either this
        // sees the waiter (and notifies under `wait`) or the waiter sees the
        // new prefix. A plain load could miss the waiter while the waiter
        // misses the prefix (store buffering), losing the last wake.
        if self.waiters.fetch_or(0, Ordering::AcqRel) > 0 {
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

    /// Contiguously durable bytes made so by a data flush.
    #[must_use]
    pub fn synced(&self) -> u64 {
        self.synced.load(Ordering::SeqCst)
    }

    /// Publishes a durable frontier after a successful barrier that issued
    /// a data flush (also the durable frontier). Never moves backwards.
    pub fn publish_synced(&self, through: u64) {
        self.publish_durable(through);
        let _previous = self.synced.fetch_max(through, Ordering::SeqCst);
    }

    /// The next reservation's start.
    #[must_use]
    pub fn tail(&self) -> u64 {
        self.tail.load(Ordering::SeqCst)
    }

    /// Moves the tail forward to `offset` rounded up to a block, when nothing
    /// is in flight. Never moves it backwards. The skipped range counts as
    /// durable (a writable open flushed the device first) only when
    /// everything appended in this open is durable too; otherwise the
    /// durable point stays, and the next sync that flushes covers both (so
    /// `durable_through` is conservative after it).
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
        if new == tail {
            // Already there: nothing moves (an append may be reserving at the
            // tail right now; touching the tail or prefix could only race it).
            // The prefix compare-exchange below is enough under the memory
            // model even then, but loom (0.7) does not order a
            // read-modify-write against a racing plain store and reports a
            // false failure for that race; not running it at all keeps the
            // model exact.
            return Ok(tail);
        }
        if self
            .tail
            .compare_exchange(tail, new, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(NotWrittenCause::TooManyInFlight);
        }
        // From the old tail only: with `new == tail` an append may already
        // have reserved at the tail, completed and advanced the prefix past
        // it; a plain store would move the prefix back below that append
        // and strand everything after it. (With `new > tail` nothing can
        // move it first: no reservation starts below `new`.)
        let _moved = self
            .prefix
            .compare_exchange(tail, new, Ordering::SeqCst, Ordering::SeqCst);
        // An append that reserved between the tail's move and this store
        // read the old prefix: its completion may be marked and stranded, or
        // a writer may wait for room. Advance over it and wake them, as a
        // completion would. The ring is had through `get_or_init`, never a
        // plain look that may miss one a first completion is creating (both
        // threads would then miss each other's write: a store-buffering
        // race the slot protocol cannot see); so a `resume_at` that moves
        // the tail allocates it.
        let _moved = self.advance(self.ring());
        self.wake_if_waiting();
        // A writable open flushed the device before reading anything, so the
        // skipped range is on media; but bytes appended in this open and
        // not yet flushed are not, and the durable point is a prefix: it
        // moves only from the old tail (nothing waiting for a flush).
        let _moved = self
            .durable
            .compare_exchange(tail, new, Ordering::SeqCst, Ordering::SeqCst);
        let _moved = self
            .synced
            .compare_exchange(tail, new, Ordering::SeqCst, Ordering::SeqCst);
        Ok(new)
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::sync::Arc;

    /// The ring is allocated at the first completion, or a `resume_at` that
    /// moves the tail (its advance must order against a first completion):
    /// not at construction, a reservation or a `resume_at` at the tail.
    #[test]
    fn test_the_ring_is_allocated_at_the_first_completion() {
        let ring = ring_for(16 << 20, 64 << 20, 12);
        let fr = AppendFrontier::new(0, 64 << 20, 12, ring);
        assert_eq!(fr.ring_slots(), 0);
        assert_eq!(fr.resume_at(0), Ok(0));
        let r = fr.reserve(4096).unwrap();
        assert_eq!(fr.ring_slots(), 0);
        fr.complete(r);
        assert_eq!(fr.ring_slots(), 4096);
        assert_eq!(fr.completed(), 4096);
        let moved = AppendFrontier::new(0, 64 << 20, 12, ring);
        assert_eq!(moved.resume_at(8192), Ok(8192));
        assert_eq!(moved.ring_slots(), 4096);
    }

    /// Sixteen appenders making their first appends at once share one ring,
    /// and every append completes.
    #[test]
    fn test_first_appends_at_once_share_one_ring() {
        let fr = Arc::new(AppendFrontier::new(0, 64 << 20, 12, 64));
        let threads: Vec<_> = (0..16)
            .map(|_| {
                let fr = Arc::clone(&fr);
                std::thread::spawn(move || {
                    for _ in 0..4 {
                        let r = fr.reserve_wait(4096, || false).unwrap();
                        fr.complete(r);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(fr.ring_slots(), 64);
        assert_eq!(fr.completed(), 64 * 4096);
    }

    /// A store's append regions, reopened, have no ring until one of them is
    /// appended to (then that one alone).
    #[test]
    fn test_reopened_append_regions_allocate_no_ring_until_used() {
        use std::path::Path;

        use store_io_sim::{SimConfig, SimPlatform};

        use crate::store::{Store, StoreOptions};

        let p = SimPlatform::new(SimConfig::volatile(61));
        let s = Store::create(p.clone(), Path::new("/db"), StoreOptions::default()).unwrap();
        for i in 0..64 {
            let _r = s
                .provision_append_region(&format!("a{i}"), 1 << 20)
                .unwrap();
        }
        drop(s);
        let s = Store::open(p, Path::new("/db"), StoreOptions::default()).unwrap();
        let rings = |s: &Store<SimPlatform>| -> Vec<(String, usize)> {
            s.regions_snapshot()
                .iter()
                .map(|r| {
                    let f = r.frontier.as_ref().unwrap();
                    (r.name.as_str().to_owned(), f.ring_slots())
                })
                .collect()
        };
        assert!(rings(&s).iter().all(|r| r.1 == 0), "{:?}", rings(&s));
        let a7 = s.append_region("a7").unwrap();
        let _at = a7.resume_at(0).unwrap();
        assert!(
            rings(&s).iter().all(|r| r.1 == 0),
            "resume_at allocated one"
        );
        let _t = a7.append(b"x").unwrap();
        let used: Vec<_> = rings(&s).into_iter().filter(|r| r.1 > 0).collect();
        assert_eq!(used.len(), 1, "{used:?}");
        assert_eq!(used[0].0, "a7");
    }

    fn f() -> AppendFrontier {
        AppendFrontier::new(0, 1 << 20, 12, 64)
    }

    /// The window is bytes: the same reach at every block size, within
    /// bounds.
    #[test]
    fn test_ring_for_counts_the_window_in_blocks_of_the_region() {
        let big = 1 << 40;
        assert_eq!(ring_for(16 << 20, big, 12), 4096);
        assert_eq!(
            ring_for(16 << 20, big, 9),
            32_768,
            "512 B blocks hold the same bytes"
        );
        assert_eq!(ring_for(0, big, 12), 2);
        assert_eq!(ring_for(4096, big, 12), 2);
        assert_eq!(ring_for(u64::MAX, u64::MAX, 9), 1 << 20);
        assert_eq!(
            ring_for(10 << 20, big, 12),
            2048,
            "rounded down, never more"
        );
        assert_eq!(
            ring_for(16 << 20, 64 << 10, 12),
            16,
            "bounded by the region"
        );
        assert_eq!(
            ring_for(16 << 20, 12 << 20, 12),
            4096,
            "a region below the window never limits itself"
        );
        assert_eq!(
            ring_for(12 << 20, 12 << 20, 12),
            4096,
            "nor one the window's size"
        );
    }

    /// The default window at 512 B blocks still holds eight 1 MiB appends in
    /// flight, none completed (counted in blocks, it held only 2 MiB there:
    /// the third was refused as too far ahead).
    #[test]
    fn test_the_default_window_holds_eight_mib_in_flight_at_512_byte_blocks() {
        let limit = 64 << 20;
        let window = crate::StoreOptions::default().append_window;
        let fr = AppendFrontier::new(0, limit, 9, ring_for(window, limit, 9));
        for i in 0..8 {
            let r = fr.reserve(1 << 20);
            assert!(r.is_ok(), "append {i} refused: {r:?}");
        }
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

    /// A poisoned store reserves nothing, even with room in the ring (a
    /// wake that made room never lets an append through after the poison).
    #[test]
    fn test_reserve_wait_refuses_once_told_to_give_up() {
        let fr = AppendFrontier::new(0, 1 << 30, 12, 4);
        assert_eq!(
            fr.reserve_wait(4096, || true),
            Err(NotWrittenCause::TooManyInFlight)
        );
        assert_eq!(fr.tail(), 0, "reserved on a poisoned store");
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

    /// A waiter for room in a full ring, racing the completions that drain
    /// it: however they interleave, the waiter reserves (the snapshot it
    /// waits on is taken before its refusal).
    #[test]
    fn loom_a_waiter_for_room_sees_the_ring_drain() {
        loom::model(|| {
            let fr = Arc::new(AppendFrontier::new(0, 1 << 20, 12, 2));
            let a = fr.reserve(4096).unwrap_or_else(|_| unreachable!());
            let b = fr.reserve(4096).unwrap_or_else(|_| unreachable!());
            let done = {
                let fr = fr.clone();
                loom::thread::spawn(move || {
                    fr.complete(a);
                    fr.complete(b);
                })
            };
            let waiter = {
                let fr = fr.clone();
                loom::thread::spawn(move || fr.reserve_wait(4096, || false).is_ok())
            };
            let _joined = done.join();
            assert_eq!(waiter.join().ok(), Some(true));
        });
    }

    /// A waiter for the completed prefix, racing the one completion that
    /// reaches it: the completion's wake is never lost.
    #[test]
    fn loom_the_last_completion_wakes_its_waiter() {
        loom::model(|| {
            let fr = Arc::new(AppendFrontier::new(0, 1 << 20, 12, 4));
            let a = fr.reserve(4096).unwrap_or_else(|_| unreachable!());
            let done = {
                let fr = fr.clone();
                loom::thread::spawn(move || fr.complete(a))
            };
            let waiter = {
                let fr = fr.clone();
                loom::thread::spawn(move || fr.wait_completed(a.end, || false))
            };
            let _joined = done.join();
            assert_eq!(waiter.join().ok(), Some(true));
        });
    }

    /// An append racing `resume_at`: a reservation made between the tail's
    /// move and the prefix's is never stranded once both are done.
    #[test]
    fn loom_an_append_racing_resume_is_never_stranded() {
        loom::model(|| {
            let fr = Arc::new(AppendFrontier::new(0, 1 << 20, 12, 4));
            let resume = {
                let fr = fr.clone();
                loom::thread::spawn(move || {
                    let _r = fr.resume_at(4096);
                })
            };
            let append = {
                let fr = fr.clone();
                loom::thread::spawn(move || {
                    if let Ok(r) = fr.reserve(4096) {
                        fr.complete(r);
                    }
                })
            };
            let _a = resume.join();
            let _b = append.join();
            assert_eq!(fr.completed(), fr.tail(), "a completion was stranded");
        });
    }

    /// A writer waiting for room while `resume_at` skips past the ring is
    /// woken by the skip.
    #[test]
    fn loom_a_waiter_racing_resume_is_woken() {
        loom::model(|| {
            let fr = Arc::new(AppendFrontier::new(0, 1 << 20, 12, 2));
            let resume = {
                let fr = fr.clone();
                loom::thread::spawn(move || {
                    let _r = fr.resume_at(4 * 4096);
                })
            };
            let waiter = {
                let fr = fr.clone();
                loom::thread::spawn(move || fr.reserve_wait(4096, || false).is_ok())
            };
            let _a = resume.join();
            assert_eq!(waiter.join().ok(), Some(true));
        });
    }

    /// `resume_at` to the tail itself, racing an append there: the prefix
    /// never moves back below the append's completion.
    #[test]
    fn loom_resume_to_the_tail_never_moves_the_prefix_back() {
        loom::model(|| {
            let fr = Arc::new(AppendFrontier::new(0, 1 << 20, 12, 4));
            let resume = {
                let fr = fr.clone();
                loom::thread::spawn(move || {
                    let _r = fr.resume_at(0);
                })
            };
            let append = {
                let fr = fr.clone();
                loom::thread::spawn(move || {
                    if let Ok(r) = fr.reserve(4096) {
                        fr.complete(r);
                    }
                })
            };
            let _a = resume.join();
            let _b = append.join();
            assert_eq!(fr.completed(), fr.tail(), "the prefix moved back");
        });
    }
}
