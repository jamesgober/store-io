//! The flush domain: one per device (one per container file in the default
//! layout). It decides when a barrier needs a new device flush and lets
//! concurrent barriers share one, with no timer and no waiting for company.
//!
//! # The join rule
//!
//! A barrier covering writes `W` (all already completed) loads
//! `e = issued` **after** the last write in `W` completed. Flush number
//! `e + 1` is issued only after `issued` is incremented to `e + 1`, which the
//! load preceded, so flush `e + 1` (and every later one) was issued after
//! every write in `W` completed and covers them (a device flush persists
//! every write that completed before it was submitted). The barrier
//! therefore needs `done >= e + 1`, where `done` is the highest `k` such that
//! flushes `1..=k` **all** succeeded.
//!
//! # Leader-inline flushing, on up to `slots` handles at once
//!
//! A barrier that needs a flush, finds none issued since its ticket, and
//! finds a free **slot** becomes a **leader**: it takes the slot, increments
//! `issued` (under the slot lock) *before* calling the flush, runs the flush
//! on its own thread (no hand-off: measured at +7.7% p50 on Windows when
//! handed to a dedicated thread), records the success, and wakes the
//! **followers**. A follower that needed a later flush than the ones that
//! finished loops and may lead the next one. A lone writer therefore pays
//! exactly one flush; concurrent writers share flushes.
//!
//! With one slot (the default), at most one flush is in flight. With more
//! (D-124), up to that many run at once, each on the slot's own file handle
//! and queue (the caller maps a slot to them): flushes may then finish out of
//! order, and `done` advances only over an unbroken run of successes, so a
//! barrier is never satisfied while an earlier flush is unfinished or
//! failed. Separate handles keep each flush's failure report its own (on
//! Linux the write-back error sequence is per open file description: two
//! flushes on one handle could see one failure between them, and the other
//! report success). Slot 0 is the caller's main handle; a barrier that can
//! only flush through it (metadata writes) leads on slot 0 alone, and any
//! other flush of the main handle runs holding slot 0
//! ([`Domain::exclusive_main`]). The platform must also persist, through a
//! flush on any handle, the writes completed through every handle of the
//! file (`Platform::flush_errors_per_handle`).
//!
//! # Waking (D-125, D-126)
//!
//! A barrier that must wait registers under the slot lock, then sleeps on a
//! [`WaitWord`] without it, in one of three places:
//! - the **bucket** of the flush it needs (`need % BUCKETS`) when a flush
//!   covering it is in flight, or will be led by the designate; a bucket
//!   holds the barriers of one need at a time;
//! - as the **designate**, on a word of its own, when it needs the next
//!   flush (the only one it may lead) and no barrier waits to lead it yet;
//! - on the **shared** word otherwise: its bucket held another need (rare),
//!   it may lead on fewer slots than the domain has (`barrier_main`), or it
//!   is an `exclusive_main` caller waiting for slot 0.
//!
//! A leader whose flush finished, still under the lock, deregisters every
//! bucket its flush covered, the designate (a slot is free) and the shared
//! sleepers, and bumps their words; after releasing the lock it wakes each
//! word with one call. The kernel wakes all of a bucket's barriers at once,
//! and each returns on reading `done`, never re-taking the lock: on a
//! virtual machine every wake is an interrupt to another processor, and a
//! chain of wakes, each woken barrier re-taking the lock to wake the next,
//! cost the last barrier one interrupt and one lock per barrier ahead of it
//! (G15, D-125, D-126). The designate re-takes the lock and leads the next
//! flush, or, if a new barrier led it first, sleeps in its bucket. Barriers
//! needing a later flush are not woken by an earlier one. Poison, a failed
//! flush and a panicked leader wake everyone; slot 0 freed by
//! `exclusive_main` wakes the designate and the shared sleepers (no bucket
//! sleeper waits for a slot: each waits for `done` or for the designate).
//!
//! A sleeper reads its word under the lock when it registers, and a waker
//! bumps it under the lock when it deregisters: so a waker's bump always
//! follows the registration it ends, a sleeper registered later reads the
//! bumped value, and a wake sent after the lock is released either finds the
//! sleeper asleep or finds the word already moved. A sleeper leaves only
//! when its word has moved, so a spurious return of the OS's wait changes
//! nothing.
//!
//! # Poison
//!
//! The first failed flush poisons the domain forever: `done` never advances
//! again, the leader gets the failure, and every waiting or later barrier gets
//! [`Error::Poisoned`]. A barrier that was already satisfied before it
//! observed the poison keeps its result. A flush is never retried.

use std::sync::OnceLock;

#[cfg(loom)]
use loom::sync::{Mutex, MutexGuard};
#[cfg(not(loom))]
use std::sync::{Mutex, MutexGuard};

use store_io_buf::{CachePadded, WaitWord};
use store_io_core::error::{Error, ErrorContext, FirstCause, Op, OsError};

use crate::sync::{AtomicU64, Ordering};

/// The most flushes a domain runs at once.
pub const MAX_FLUSH_SLOTS: usize = 4;

/// Flushes issued past `done` (in flight, or finished and waiting for an
/// earlier one) beyond which no new flush is issued: the successes kept
/// above `done` fit a 64-bit mask.
const WINDOW: u64 = 32;

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
    /// The most flushes that were in flight at once (at most the domain's
    /// slots, D-124).
    pub peak_in_flight: u64,
    /// Flushes that succeeded while an earlier one was still unfinished
    /// (kept until it finished, D-124).
    pub out_of_order: u64,
}

/// Wait buckets, one word each. Needs that can wait in a bucket lie in
/// `(done, issued + 1]`, at most `WINDOW + 1` values, so with 64 a bucket
/// rarely meets a second need; tests use 2, so it often does.
#[cfg(not(any(test, loom)))]
const BUCKETS: usize = 64;
#[cfg(any(test, loom))]
const BUCKETS: usize = 2;

// Bucket indices are kept in a byte.
const _: () = assert!(BUCKETS <= 256);

/// The barriers registered in one bucket, all of one need.
#[derive(Debug, Default, Clone, Copy)]
struct Bucket {
    /// Barriers registered here (until a waker deregisters them).
    waiting: u32,
    /// Their need, while `waiting > 0`.
    need: u64,
}

/// Where a waiting barrier sleeps.
#[derive(Debug, Clone, Copy)]
enum Bed {
    Bucket(usize),
    Designate,
    Shared,
}

/// Wakes decided (and their words bumped) under the slot lock, sent after
/// it is released.
struct Wakes {
    /// Buckets to wake, each at most once.
    buckets: [u8; BUCKETS],
    len: usize,
    designate: bool,
    shared: bool,
}

impl Wakes {
    fn new() -> Self {
        Self {
            buckets: [0; BUCKETS],
            len: 0,
            designate: false,
            shared: false,
        }
    }

    fn send(self, d: &Domain) {
        // The designate first: the next flush waits for it, and waking a
        // bucket of many barriers takes the kernel longest.
        if self.designate {
            d.designate_word.wake_all();
        }
        for &b in &self.buckets[..self.len] {
            d.bucket_words[usize::from(b)].wake_all();
        }
        if self.shared {
            d.shared_word.wake_all();
        }
    }
}

/// Observability counters counted without the slot lock, on a cache line of
/// their own: incrementing them must not evict the line every barrier reads
/// (`issued`, `done`). Those counted under the lock live in [`Slots`], on
/// the line a leader holds anyway (no locked add, no line of its own, per
/// flush).
#[derive(Debug)]
#[repr(align(128))]
struct Counters {
    joined: AtomicU64,
    already_durable: AtomicU64,
}

/// What the slot lock guards. `done` changes only under it.
#[derive(Debug)]
struct Slots {
    /// Bit `s`: slot `s` has a flush in flight.
    busy: u32,
    /// Bit `i`: flush `done + 1 + i` succeeded (waiting for an earlier one).
    ahead: u64,
    /// The wait buckets (`BUCKETS` of them).
    buckets: [Bucket; BUCKETS],
    /// The need of the barrier registered as the designate, if any.
    designate: Option<u64>,
    /// Sleepers registered on the shared word.
    shared: u32,
    /// Callers of `exclusive_main` waiting for slot 0: leaders leave it to
    /// them, so a full flush waits at most for the flush running on slot 0
    /// (and any full flush ahead of it).
    main_waiting: u32,
    /// Flushes led, the most in flight at once, and those that finished
    /// before an earlier one (observability).
    led: u64,
    peak: u64,
    out_of_order: u64,
}

/// A flush domain.
#[derive(Debug)]
pub struct Domain {
    issued: AtomicU64,
    done: AtomicU64,
    counters: Counters,
    poison: OnceLock<FirstCause>,
    slots: u32,
    state: Mutex<Slots>,
    /// One per wait bucket, each on its own cache line.
    bucket_words: Box<[CachePadded<WaitWord>]>,
    designate_word: CachePadded<WaitWord>,
    shared_word: CachePadded<WaitWord>,
}

impl Default for Domain {
    fn default() -> Self {
        Self::new()
    }
}

impl Domain {
    /// A fresh, healthy domain with one flush slot.
    #[must_use]
    pub fn new() -> Self {
        Self::with_slots(1)
    }

    /// A fresh, healthy domain running up to `slots` flushes at once
    /// (clamped to `1..=MAX_FLUSH_SLOTS`). The caller maps each slot to a
    /// handle and queue of its own.
    #[must_use]
    pub fn with_slots(slots: usize) -> Self {
        Self {
            issued: AtomicU64::new(0),
            done: AtomicU64::new(0),
            counters: Counters {
                joined: AtomicU64::new(0),
                already_durable: AtomicU64::new(0),
            },
            poison: OnceLock::new(),
            slots: slots.clamp(1, MAX_FLUSH_SLOTS) as u32,
            state: Mutex::new(Slots {
                busy: 0,
                ahead: 0,
                buckets: [Bucket {
                    waiting: 0,
                    need: 0,
                }; BUCKETS],
                designate: None,
                shared: 0,
                main_waiting: 0,
                led: 0,
                peak: 0,
                out_of_order: 0,
            }),
            bucket_words: (0..BUCKETS)
                .map(|_| CachePadded::new(WaitWord::new()))
                .collect(),
            designate_word: CachePadded::new(WaitWord::new()),
            shared_word: CachePadded::new(WaitWord::new()),
        }
    }

    /// The flush slots: how many flushes run at once at most.
    #[must_use]
    pub fn slots(&self) -> usize {
        self.slots as usize
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

    /// Observability counters: a consistent snapshot, read under the slot
    /// lock (briefly; every barrier takes it), so not for a hot loop.
    #[must_use]
    pub fn stats(&self) -> DomainStats {
        let st = self.lock();
        DomainStats {
            flushes: self.issued.load(Ordering::Acquire),
            joined: self.counters.joined.load(Ordering::Relaxed),
            led: st.led,
            already_durable: self.counters.already_durable.load(Ordering::Relaxed),
            peak_in_flight: st.peak,
            out_of_order: st.out_of_order,
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

    /// Blocks until flushes `1..=need` have all succeeded, leading a flush on
    /// this thread, on any free slot, if none has been issued since `need`'s
    /// ticket.
    ///
    /// `flush(slot, k)` performs flush number `k` through slot `slot`'s
    /// handle and returns the raw outcome; it is called at most once per
    /// call to `barrier`, and only by the leader.
    ///
    /// # Errors
    ///
    /// [`Error::DurabilityUnknown`] if this call led a flush that failed;
    /// [`Error::Poisoned`] if the domain is poisoned.
    pub fn barrier(
        &self,
        need: u64,
        flush: impl FnMut(usize, u64) -> Result<(), Option<OsError>>,
    ) -> Result<(), Error> {
        self.barrier_in(need, self.slots, flush)
    }

    /// As [`Self::barrier`], leading only on slot 0 (the caller's main
    /// handle), for a caller that can flush through no other.
    ///
    /// # Errors
    ///
    /// As [`Self::barrier`].
    pub fn barrier_main(
        &self,
        need: u64,
        mut flush: impl FnMut() -> Result<(), Option<OsError>>,
    ) -> Result<(), Error> {
        self.barrier_in(need, 1, |_slot, _k| flush())
    }

    fn barrier_in(
        &self,
        need: u64,
        usable: u32,
        mut flush: impl FnMut(usize, u64) -> Result<(), Option<OsError>>,
    ) -> Result<(), Error> {
        let mut led_flush = false;
        loop {
            // A woken barrier comes back here and, covered, returns without
            // the lock.
            if self.done.load(Ordering::SeqCst) >= need {
                if !led_flush {
                    let _count = self.counters.joined.fetch_add(1, Ordering::Relaxed);
                }
                return Ok(());
            }
            if let Some(first) = self.poison.get() {
                return Err(Error::Poisoned { first: *first });
            }
            let mut st = self.lock();
            // Re-checked under the lock: every change to `done`, `issued`
            // and the slots happens under it, so nothing is missed between
            // these checks and the registration.
            let done = self.done.load(Ordering::SeqCst);
            if done >= need || self.poison.get().is_some() {
                continue;
            }
            let issued = self.issued.load(Ordering::SeqCst);
            // A flush issued since the ticket covers `need`: wait for it
            // (and every earlier one) to finish. Otherwise lead, given a
            // free slot and room in the window.
            let mut free = (!st.busy) & ((1u32 << usable) - 1);
            // A full flush waiting for slot 0 has it next: every leader
            // leaves it (none holds what that flush needs: it takes no lock).
            if st.main_waiting > 0 {
                free &= !1;
            }
            if issued >= need || free == 0 || issued - done >= WINDOW {
                // A barrier that would lead but may use fewer slots than
                // the domain has must not take the designate's wake.
                let narrow = need > issued && usable < self.slots;
                let bed = Self::bed(&st, need, issued, narrow);
                // `sleep` releases the lock; the loop re-checks.
                self.sleep(st, need, bed);
                continue;
            }
            // The highest free slot: slot 0, the only one a main-handle
            // flush can use, is taken by data leaders last.
            let slot = 31 - free.leading_zeros();
            st.busy |= 1 << slot;
            st.peak = st.peak.max(u64::from(st.busy.count_ones()));
            st.led += 1;
            // k = issued + 1 >= need: issued only grows, and the ticket
            // read need - 1 before this.
            let k = self.issued.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
            drop(st);
            led_flush = true;
            // A flush that panics leaves the device's state unknown: the
            // guard poisons the domain and frees the slot, so no later
            // barrier waits for a leader that is gone.
            let unwinding = LeaderGuard { domain: self, slot };
            let outcome = flush(slot as usize, k);
            core::mem::forget(unwinding);
            let mut st = self.lock();
            st.busy &= !(1 << slot);
            let failed = outcome.err();
            let before = self.done.load(Ordering::SeqCst);
            match failed {
                None if self.poison.get().is_none() => self.record(&mut st, k),
                None => {}
                Some(raw) => {
                    let _first_cause_kept = self.poison.set(FirstCause {
                        op: Some(Op::FlushData),
                        raw,
                    });
                }
            }
            let mut wakes = Wakes::new();
            if self.poison.get().is_some() {
                self.take_everyone(&mut st, &mut wakes);
            } else {
                self.take_covered(&mut st, before, &mut wakes);
                // A slot is free: the designate may lead on it, and the
                // shared sleepers include those that wait for a slot.
                self.take_designate(&mut st, &mut wakes);
                self.take_shared(&mut st, &mut wakes);
            }
            drop(st);
            wakes.send(self);
            if let Some(raw) = failed {
                return Err(Error::DurabilityUnknown {
                    op: Op::FlushData,
                    raw,
                    ctx: ErrorContext::default(),
                });
            }
        }
    }

    /// Records flush `k`'s success and advances `done` over the unbroken
    /// run of successes that follows it.
    fn record(&self, st: &mut Slots, k: u64) {
        let mut done = self.done.load(Ordering::SeqCst);
        // k > done: flush k has not been counted, and done only covers
        // finished flushes; k - done <= WINDOW by the issue rule.
        if k > done + 1 {
            st.out_of_order += 1;
        }
        st.ahead |= 1u64 << (k - done - 1);
        while st.ahead & 1 == 1 {
            st.ahead >>= 1;
            done += 1;
        }
        self.done.store(done, Ordering::SeqCst);
    }

    /// Runs `f` holding slot 0, the main handle's (waiting until it is
    /// free; leaders leave it to a waiting caller): for a flush of the
    /// main handle outside a barrier (a full flush), which must not run
    /// beside a data flush on the same handle. Records nothing; the caller
    /// handles `f`'s outcome. Runs `f` even on a poisoned domain.
    ///
    /// Deadlock: while a caller waits here, every barrier that would lead on
    /// slot 0 waits for it, so `f` must not block on anything a barrier's
    /// caller may hold (a lock, a lane), and the caller must hold nothing a
    /// slot-0 leader's flush needs.
    pub fn exclusive_main<R>(&self, f: impl FnOnce() -> R) -> R {
        let mut st = self.lock();
        st.main_waiting += 1;
        while st.busy & 1 == 1 {
            // Woken with the shared sleepers whenever a slot is freed.
            self.sleep(st, 0, Bed::Shared);
            st = self.lock();
        }
        st.main_waiting -= 1;
        st.busy |= 1;
        drop(st);
        // Slot 0 is given back however `f` ends, a panic included.
        let _held = MainGuard { domain: self };
        f()
    }

    /// Takes every flush slot, as leaders flushing would (tests: every
    /// barrier that needs a flush then waits until [`Self::drop_leadership`]).
    #[cfg(test)]
    pub(crate) fn hold_leadership(&self) -> bool {
        let mut st = self.lock();
        if st.busy != 0 {
            return false;
        }
        st.busy = (1u32 << self.slots) - 1;
        true
    }

    /// Gives back the slots taken by [`Self::hold_leadership`].
    #[cfg(test)]
    pub(crate) fn drop_leadership(&self) {
        let mut st = self.lock();
        st.busy = 0;
        let mut wakes = Wakes::new();
        self.take_everyone(&mut st, &mut wakes);
        drop(st);
        wakes.send(self);
    }

    /// Barriers and `exclusive_main` callers registered asleep (tests).
    #[cfg(test)]
    pub(crate) fn waiting(&self) -> u32 {
        let st = self.lock();
        st.buckets.iter().map(|b| b.waiting).sum::<u32>()
            + u32::from(st.designate.is_some())
            + st.shared
    }

    /// Poisons the domain (a flush whose outcome is unknown), frees its slot
    /// and wakes every waiter.
    fn poison_unknown_flush(&self, slot: u32) {
        let _first_cause_kept = self.poison.set(FirstCause {
            op: Some(Op::FlushData),
            raw: None,
        });
        let mut st = self.lock();
        st.busy &= !(1 << slot);
        let mut wakes = Wakes::new();
        self.take_everyone(&mut st, &mut wakes);
        drop(st);
        wakes.send(self);
    }

    /// Where a barrier for `need` that must wait sleeps (see the module
    /// documentation): `issued` and the registrations as read under the
    /// lock.
    fn bed(st: &Slots, need: u64, issued: u64, narrow: bool) -> Bed {
        if narrow {
            return Bed::Shared;
        }
        // Needs the next flush and nobody waits to lead it: this barrier will.
        if need > issued && st.designate.is_none() {
            return Bed::Designate;
        }
        // Covered by a flush in flight, or by the one the designate will
        // lead: in its bucket, unless the bucket holds another need.
        let b = (need % BUCKETS as u64) as usize;
        let bucket = st.buckets[b];
        let covered_later = need <= issued || st.designate == Some(need);
        if covered_later && (bucket.waiting == 0 || bucket.need == need) {
            Bed::Bucket(b)
        } else {
            Bed::Shared
        }
    }

    /// Registers in `bed`, reading its word, releases the lock and sleeps
    /// until a waker has deregistered this sleeper (the word moved). The
    /// caller re-checks; a woken barrier re-takes the lock only if it is
    /// not yet satisfied.
    fn sleep(&self, mut st: MutexGuard<'_, Slots>, need: u64, bed: Bed) {
        let word: &WaitWord = match bed {
            Bed::Bucket(b) => {
                st.buckets[b].need = need;
                st.buckets[b].waiting += 1;
                &self.bucket_words[b]
            }
            Bed::Designate => {
                st.designate = Some(need);
                &self.designate_word
            }
            Bed::Shared => {
                st.shared += 1;
                &self.shared_word
            }
        };
        let seen = word.load();
        drop(st);
        // The word moves only when a waker deregisters this bed (and every
        // sleeper in it): a return of the wait with the word unmoved is
        // spurious. (A sleeper would need 2^32 bumps between its read and
        // its wait to miss one.)
        while word.load() == seen {
            word.wait(seen);
        }
    }

    /// After `done` moved from `before`: every bucket whose need it now
    /// covers.
    fn take_covered(&self, st: &mut Slots, before: u64, wakes: &mut Wakes) {
        let after = self.done.load(Ordering::SeqCst);
        // Registered needs exceed `done` when registered, so a covered one
        // lies in (before, after]; a bucket is visited once.
        for d in before + 1..=after.min(before + BUCKETS as u64) {
            let b = (d % BUCKETS as u64) as usize;
            let w = st.buckets[b];
            if w.waiting > 0 && w.need > before && w.need <= after {
                self.take_bucket(st, b, wakes);
            }
        }
    }

    fn take_bucket(&self, st: &mut Slots, b: usize, wakes: &mut Wakes) {
        st.buckets[b].waiting = 0;
        self.bucket_words[b].bump();
        // Each bucket is taken at most once per decision: never full.
        debug_assert!(wakes.len < wakes.buckets.len(), "wake list full");
        if let Some(slot) = wakes.buckets.get_mut(wakes.len) {
            *slot = b as u8;
            wakes.len += 1;
        }
    }

    fn take_designate(&self, st: &mut Slots, wakes: &mut Wakes) {
        if st.designate.take().is_some() {
            self.designate_word.bump();
            wakes.designate = true;
        }
    }

    fn take_shared(&self, st: &mut Slots, wakes: &mut Wakes) {
        if st.shared > 0 {
            st.shared = 0;
            self.shared_word.bump();
            wakes.shared = true;
        }
    }

    /// Every sleeper: poison, a failed flush, a panicked leader, and the
    /// tests' `drop_leadership`.
    fn take_everyone(&self, st: &mut Slots, wakes: &mut Wakes) {
        for b in 0..BUCKETS {
            if st.buckets[b].waiting > 0 {
                self.take_bucket(st, b, wakes);
            }
        }
        self.take_designate(st, wakes);
        self.take_shared(st, wakes);
    }

    fn lock(&self) -> MutexGuard<'_, Slots> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn notify(&self) {
        let mut st = self.lock();
        let mut wakes = Wakes::new();
        self.take_everyone(&mut st, &mut wakes);
        drop(st);
        wakes.send(self);
    }
}

/// Holds slot 0 for [`Domain::exclusive_main`]; gives it back on drop.
struct MainGuard<'a> {
    domain: &'a Domain,
}

impl Drop for MainGuard<'_> {
    fn drop(&mut self) {
        let d = self.domain;
        let mut st = d.lock();
        st.busy &= !1;
        // Slot 0 is free: whoever waits for a slot.
        let mut wakes = Wakes::new();
        d.take_designate(&mut st, &mut wakes);
        d.take_shared(&mut st, &mut wakes);
        drop(st);
        wakes.send(d);
    }
}

/// Held by a flush leader while its flush runs; dropped only if the flush
/// panics (the leader forgets it otherwise).
struct LeaderGuard<'a> {
    domain: &'a Domain,
    slot: u32,
}

impl Drop for LeaderGuard<'_> {
    fn drop(&mut self) {
        self.domain.poison_unknown_flush(self.slot);
    }
}

#[cfg(all(test, not(loom)))]
mod tests;
#[cfg(all(test, not(loom)))]
mod wake_tests;

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
                    let r = d.barrier(need, |_, _| {
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

    /// One leader and two barriers of the same need: whoever sleeps is woken
    /// (the leader wakes their bucket at once), and all three return
    /// satisfied by one or more flushes.
    #[test]
    fn loom_every_barrier_of_one_need_is_woken() {
        loom::model(|| {
            let d = Arc::new(Domain::new());
            let need = d.ticket();
            let spawn = || {
                let d = d.clone();
                loom::thread::spawn(move || d.barrier(need, |_, _| Ok(())).is_ok())
            };
            let all = [spawn(), spawn(), spawn()];
            for t in all {
                assert!(t.join().unwrap_or(false));
            }
            assert!(d.done.load(Ordering::SeqCst) >= need);
            assert_eq!(d.waiting(), 0);
        });
    }

    /// Three barriers take their tickets as they run, so some need the
    /// next flush while one is in flight: whoever sleeps as the designate,
    /// in a bucket or on the shared word is woken, and every barrier
    /// returns satisfied with nobody left registered.
    #[test]
    fn loom_barriers_needing_the_next_flush_are_led_and_woken() {
        loom::model(|| {
            let d = Arc::new(Domain::new());
            let spawn = || {
                let d = d.clone();
                loom::thread::spawn(move || {
                    let need = d.ticket();
                    (need, d.barrier(need, |_, _| Ok(())).is_ok())
                })
            };
            let all = [spawn(), spawn(), spawn()];
            for t in all {
                let (need, ok) = t.join().unwrap_or((0, false));
                assert!(ok);
                assert!(d.done.load(Ordering::SeqCst) >= need);
            }
            assert_eq!(d.waiting(), 0);
        });
    }

    /// One slot: a barrier, a barrier taking its ticket as it runs (it may
    /// become the designate) and a full flush: the designate parked behind
    /// the full flush's claim on slot 0 is woken when slot 0 is freed, and
    /// everyone returns.
    #[test]
    fn loom_a_full_flush_and_the_designate() {
        loom::model(|| {
            let d = Arc::new(Domain::new());
            let spawn = || {
                let d = d.clone();
                loom::thread::spawn(move || {
                    let need = d.ticket();
                    (need, d.barrier(need, |_, _| Ok(())).is_ok())
                })
            };
            let a = spawn();
            let b = spawn();
            d.exclusive_main(|| {});
            for t in [a, b] {
                let (need, ok) = t.join().unwrap_or((0, false));
                assert!(ok);
                assert!(d.done.load(Ordering::SeqCst) >= need);
            }
            assert_eq!(d.waiting(), 0);
        });
    }

    /// Two slots: a barrier that may lead only on slot 0 and two that may
    /// lead on either, tickets taken as they run: the narrow one never takes
    /// the designate's place, and all return.
    #[test]
    fn loom_narrow_and_wide_barriers_on_two_slots() {
        loom::model(|| {
            let d = Arc::new(Domain::with_slots(2));
            let main = {
                let d = d.clone();
                loom::thread::spawn(move || {
                    let need = d.ticket();
                    (need, d.barrier_main(need, || Ok(())).is_ok())
                })
            };
            let wide = |d: &Arc<Domain>| {
                let d = d.clone();
                loom::thread::spawn(move || {
                    let need = d.ticket();
                    (need, d.barrier(need, |_, _| Ok(())).is_ok())
                })
            };
            let a = wide(&d);
            let b = wide(&d);
            for t in [main, a, b] {
                let (need, ok) = t.join().unwrap_or((0, false));
                assert!(ok);
                assert!(d.done.load(Ordering::SeqCst) >= need);
            }
            assert_eq!(d.waiting(), 0);
        });
    }

    /// Poison from outside races two barriers on one slot (one may sleep as
    /// the designate, one in a bucket): each either returned satisfied
    /// before the poison or fails, and nobody is left registered.
    #[test]
    fn loom_outside_poison_wakes_the_designate_and_the_bucket() {
        loom::model(|| {
            let d = Arc::new(Domain::new());
            let spawn = || {
                let d = d.clone();
                loom::thread::spawn(move || {
                    let need = d.ticket();
                    (need, d.barrier(need, |_, _| Ok(())).is_ok())
                })
            };
            let a = spawn();
            let b = spawn();
            d.poison(FirstCause {
                op: None,
                raw: None,
            });
            for t in [a, b] {
                let (need, ok) = t.join().unwrap_or((0, false));
                if ok {
                    assert!(d.done.load(Ordering::SeqCst) >= need);
                }
            }
            assert_eq!(d.waiting(), 0);
        });
    }

    /// Two slots, two barriers, one failing flush: a barrier that returns
    /// success saw every flush up to its need succeed. With at most two
    /// flushes, success beside the other's failure means its own flush was
    /// flush 1 and `done` stopped there.
    #[test]
    fn loom_two_slots_never_satisfy_past_a_failure() {
        loom::model(|| {
            let d = Arc::new(Domain::with_slots(2));
            let spawn = |fail: bool| {
                let d = d.clone();
                loom::thread::spawn(move || {
                    let need = d.ticket();
                    let r = d.barrier(need, |_, _| {
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
            let (_need_b, ok_b) = b.join().unwrap_or((0, true));
            let done = d.done.load(Ordering::SeqCst);
            if ok_a {
                assert!(done >= need_a);
            }
            if ok_a && !ok_b {
                assert_eq!(done, 1, "satisfied past a failed flush");
            }
            assert!(d.stats().flushes <= 2);
        });
    }

    /// A main-handle barrier and a data barrier race on two slots: both are
    /// satisfied, by flushes issued after their tickets, and the data
    /// leader never shares slot 0 with the main one.
    #[test]
    fn loom_a_main_barrier_and_a_data_barrier() {
        loom::model(|| {
            let d = Arc::new(Domain::with_slots(2));
            let in_main = Arc::new(loom::sync::atomic::AtomicBool::new(false));
            let main = {
                let (d, in_main) = (d.clone(), in_main.clone());
                loom::thread::spawn(move || {
                    let need = d.ticket();
                    let r = d.barrier_main(need, || {
                        in_main.store(true, loom::sync::atomic::Ordering::SeqCst);
                        in_main.store(false, loom::sync::atomic::Ordering::SeqCst);
                        Ok(())
                    });
                    (need, r.is_ok())
                })
            };
            let data = {
                let (d, in_main) = (d.clone(), in_main.clone());
                loom::thread::spawn(move || {
                    let need = d.ticket();
                    let r = d.barrier(need, |slot, _| {
                        if slot == 0 {
                            assert!(!in_main.load(loom::sync::atomic::Ordering::SeqCst));
                        }
                        Ok(())
                    });
                    (need, r.is_ok())
                })
            };
            let (need_m, ok_m) = main.join().unwrap_or((0, false));
            let (need_d, ok_d) = data.join().unwrap_or((0, false));
            assert!(ok_m && ok_d);
            let done = d.done.load(Ordering::SeqCst);
            assert!(done >= need_m && done >= need_d);
        });
    }

    /// Poison from outside races a leader: a barrier that returns success
    /// had its flush recorded before the poison, and nobody is left waiting.
    #[test]
    fn loom_outside_poison_races_a_leader() {
        loom::model(|| {
            let d = Arc::new(Domain::with_slots(2));
            let leader = {
                let d = d.clone();
                loom::thread::spawn(move || {
                    let need = d.ticket();
                    (need, d.barrier(need, |_, _| Ok(())).is_ok())
                })
            };
            let poisoner = {
                let d = d.clone();
                loom::thread::spawn(move || {
                    d.poison(FirstCause {
                        op: None,
                        raw: None,
                    });
                })
            };
            let (need, ok) = leader.join().unwrap_or((0, false));
            let _p = poisoner.join();
            if ok {
                assert!(d.done.load(Ordering::SeqCst) >= need);
            }
            assert!(d.poisoned().is_some());
        });
    }
}
