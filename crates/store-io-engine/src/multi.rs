//! `StoreSet` (R5.3, D-144): stores on several devices made durable
//! together, one flush per store and the flushes in parallel, with no format
//! change and no lock across stores (MDEV-02): each store keeps its own
//! domain, flush thread, poison and receipts.
//!
//! # A set barrier
//!
//! 1. Each ticket is matched to its store by its volume and open, and its
//!    region found among the store's live regions (released: `NotReady`;
//!    recycled or retiring: `StaleGeneration`).
//! 2. A store is **ready** once every append ticket's region has completed
//!    through the ticket's end. Each append ticket's completed prefix is
//!    snapshotted then, and the store's `need` is a domain ticket taken
//!    **after** the last snapshot, so its flush covers every snapshotted
//!    byte; a store with only page tickets is ready at once, `need` the
//!    largest of theirs. Ready stores are handed off as each is found ready:
//!    the call waits on one remaining store at a time and sweeps them all at
//!    every wake.
//! 3. A store needs **no flush** when it is power-safe with no ticket on a
//!    region forcing data sync (D-102), when a finished flush already covers
//!    `need`, or when every ticket is already durable; such a store is
//!    answered only once ready (a prefix receipt short of a ticket's end
//!    would still read as covering it).
//! 4. Every store needing a flush but the last is handed to its own flush
//!    thread, the request answered into the call's [`Answers`]; the calling
//!    thread leads the last store's flush itself, then yields for its
//!    answers (under the set's `Patience`, D-139), then sleeps. A store whose
//!    flush thread cannot start is flushed on the calling thread, and a hung
//!    flush the caller leads holds the call (no poison releases a system
//!    call).
//! 5. The call's `Answers` is registered as a poison waker on every store it
//!    handed off, and the poison of each read after the registration, so a
//!    store poisoned while its flush thread hangs ends the wait.
//! 6. Each ticket's receipt is built by the code that builds it for a single
//!    store (`receipt_for`, `region::sync_tail`), never from a flush number
//!    below the store's `need` (checked in release builds: a violation
//!    poisons that store and fails its tickets). A failed store's tickets
//!    keep a receipt only where what is already published proves them.
//!
//! Concurrent callers of one set are the normal case (a commit layer's
//! committers). Every call has its own answers, from a per-thread pool, and
//! tags its requests with a sequence number from a process-wide counter that
//! is never reset: an outcome of another call (a late one, on a reused
//! `Answers`) is dropped, never taken as this call's flush.

use std::cell::RefCell;
use std::sync::{Arc, PoisonError};

#[cfg(loom)]
use loom::sync::{Condvar, Mutex, MutexGuard};
#[cfg(not(loom))]
use std::sync::{Condvar, Mutex, MutexGuard};

use crate::sync::{AtomicU64, Ordering};

use store_io_core::error::{Error, ErrorContext, FirstCause, NotWrittenCause};
use store_io_platform::{IoBuf, Platform};

use crate::completion::{Rejected, WriteMode};
use crate::domain::{Patience, PoisonWaker};
use crate::flusher::{Answer, Flushed};
use crate::io::not_written;
use crate::receipt::{DurableReceipt, RegionPos, WriteTicket};
use crate::region::{AppendRegion, PageRegion, sync_tail};
use crate::settle::in_range;
use crate::store::{LiveRegion, Store};
use crate::submitter::Submitter;

/// The most stores in a set: a request's tag keeps the store's index in 16
/// bits.
pub const MAX_SET_STORES: usize = 1 << 16;

/// Bits of a tag that hold the store's index; the call's sequence is above.
const INDEX_BITS: u32 = 16;

/// The bits of a call's sequence a tag keeps (the answers compare those
/// alone, so the sequence may wrap without a call missing its outcomes).
const SEQ_MASK: u64 = (1 << (64 - INDEX_BITS)) - 1;

/// A spare grown past this many stores (or tickets) is dropped, not kept:
/// one call on a huge set does not pin its memory to the thread.
const SPARE_KEEP: usize = 1024;

/// The sequence of every set barrier call in the process, never reset: a
/// late outcome of an earlier call never matches a later one.
static CALLS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);

/// Stores on several devices, made durable together (R5.3).
///
/// ```ignore
/// let set = StoreSet::new([a, b])?;
/// let t1 = set[0].append_region("data")?.append(bytes)?;
/// let t2 = set[1].append_region("data")?.append(bytes)?;
/// let receipts = set.barrier(&[&t1, &t2])?; // one flush per store, in parallel
/// ```
pub struct StoreSet<P: Platform> {
    stores: Vec<Store<P>>,
    /// Whether a call yields for its answers before it sleeps (D-139).
    patience: Patience,
    barriers: core::sync::atomic::AtomicU64,
    stores_flushed: core::sync::atomic::AtomicU64,
    sleeps: core::sync::atomic::AtomicU64,
    fallbacks: core::sync::atomic::AtomicU64,
}

impl<P: Platform> core::fmt::Debug for StoreSet<P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StoreSet")
            .field("stores", &self.stores.len())
            .finish_non_exhaustive()
    }
}

/// What a set's barriers did ([`StoreSet::stats`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SetStats {
    /// Set barrier calls.
    pub barriers: u64,
    /// Stores the calls waited on: each call counts once each store with a
    /// ticket that became ready, whether it was flushed, needed no flush,
    /// or failed.
    pub stores: u64,
    /// Times a call slept waiting for a flush thread's answer.
    pub sleeps: u64,
    /// Stores flushed on the calling thread because no flush thread took
    /// them (one could not be started): besides the one store each call
    /// flushes itself.
    pub fallbacks: u64,
}

impl<P: Platform> StoreSet<P>
where
    P::Queue: Send,
{
    /// A set of writable stores (each a handle the caller keeps too). Stores
    /// on one device are allowed, and flushed separately.
    ///
    /// # Errors
    ///
    /// `NotWritten`: `InvalidOptions` for more than [`MAX_SET_STORES`]
    /// stores; `ReadOnly` for a read-only store; `Overlap` for the same open
    /// store twice, or two stores with one volume id (a container copied by
    /// another tool: a receipt could not name which).
    pub fn new<S: Into<Store<P>>>(stores: impl IntoIterator<Item = S>) -> Result<Self, Error> {
        let stores: Vec<Store<P>> = stores.into_iter().map(Into::into).collect();
        let refuse = |cause| not_written(cause, ErrorContext::default());
        if stores.len() > MAX_SET_STORES {
            return Err(refuse(NotWrittenCause::InvalidOptions));
        }
        for (i, s) in stores.iter().enumerate() {
            if s.inner.read_only {
                return Err(refuse(NotWrittenCause::ReadOnly));
            }
            let twice = stores[..i]
                .iter()
                .any(|t| Arc::ptr_eq(&t.inner, &s.inner) || t.inner.volume == s.inner.volume);
            if twice {
                return Err(refuse(NotWrittenCause::Overlap));
            }
        }
        Ok(Self {
            stores,
            patience: Patience::default(),
            barriers: core::sync::atomic::AtomicU64::new(0),
            stores_flushed: core::sync::atomic::AtomicU64::new(0),
            sleeps: core::sync::atomic::AtomicU64::new(0),
            fallbacks: core::sync::atomic::AtomicU64::new(0),
        })
    }

    /// The number of stores.
    #[must_use]
    pub fn len(&self) -> usize {
        self.stores.len()
    }

    /// Whether the set has no store.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.stores.is_empty()
    }

    /// The stores, in set order.
    pub fn iter(&self) -> core::slice::Iter<'_, Store<P>> {
        self.stores.iter()
    }

    /// Whether stores `i` and `j` share no physical device, read afresh
    /// (`Platform::device_identity`: a volume moved online changes it): false
    /// when either identity is incomplete.
    ///
    /// # Panics
    ///
    /// If `i` or `j` is not a store of the set.
    #[must_use]
    pub fn distinct(&self, i: usize, j: usize) -> bool {
        let identity = |s: &Store<P>| {
            let inner = &s.inner;
            inner.platform.device_identity(&inner._dir, &inner.file)
        };
        identity(&self.stores[i]).distinct(&identity(&self.stores[j]))
    }

    /// What the set's barriers did so far.
    #[must_use]
    pub fn stats(&self) -> SetStats {
        use core::sync::atomic::Ordering::Relaxed;
        SetStats {
            barriers: self.barriers.load(Relaxed),
            stores: self.stores_flushed.load(Relaxed),
            sleeps: self.sleeps.load(Relaxed),
            fallbacks: self.fallbacks.load(Relaxed),
        }
    }

    /// Makes every ticket durable, each store flushed at most once and the
    /// flushes in parallel; every store is flushed even if one fails.
    /// Returns the receipts in ticket order.
    ///
    /// Like [`AppendRegion::sync_through`], a ticket behind the calling
    /// thread's own unreaped [`Submitter`] appends waits for completions only
    /// that thread can reap.
    ///
    /// # Errors
    ///
    /// The first failing ticket's error (see [`Self::barrier_each`]).
    pub fn barrier(&self, tickets: &[&WriteTicket]) -> Result<Vec<DurableReceipt>, Error> {
        self.barrier_each(tickets)?.into_iter().collect()
    }

    /// [`Self::barrier`] with one result per ticket, for a caller deciding a
    /// quorum: a store's failure is its own (its flush failing poisons that
    /// store only), and the others' receipts stand.
    ///
    /// # Errors
    ///
    /// The outer error only for a ticket of no store in the set
    /// (`NotWritten(ForeignPosition)`), before anything is flushed. Per
    /// ticket: `NotWritten(NotReady)` for a region since released,
    /// `NotWritten(StaleGeneration)` for one since recycled, `Poisoned`,
    /// `DurabilityUnknown` if its store's flush failed (a flush handed to
    /// its store's flush thread may instead answer `Poisoned`, the failed
    /// flush as its first cause, when the call sees the poison first).
    pub fn barrier_each(
        &self,
        tickets: &[&WriteTicket],
    ) -> Result<Vec<Result<DurableReceipt, Error>>, Error> {
        use core::sync::atomic::Ordering::Relaxed;
        let mut call = Call::new(self, tickets)?;
        let _ = self.barriers.fetch_add(1, Relaxed);
        call.run();
        let _ = self.stores_flushed.fetch_add(call.stores_waited(), Relaxed);
        let _ = self.sleeps.fetch_add(call.sleeps, Relaxed);
        let _ = self.fallbacks.fetch_add(call.fallbacks, Relaxed);
        Ok(call.finish())
    }

    /// Whether `store` is one of the set's: `ForeignPosition` if not.
    fn member(&self, store: &Store<P>) -> Result<(), Error> {
        if self
            .stores
            .iter()
            .any(|s| Arc::ptr_eq(&s.inner, &store.inner))
        {
            Ok(())
        } else {
            Err(not_written(
                NotWrittenCause::ForeignPosition,
                ErrorContext::default(),
            ))
        }
    }

    /// [`AppendRegion::append_after`] with dependencies on any store of the
    /// set checked as that region's own store's are (each matched to its
    /// store by volume, its region held from the append's lane through its
    /// reservation, the holds taken in set order).
    ///
    /// # Errors
    ///
    /// `NotWritten(ForeignPosition)` before any work if `region` is of no
    /// store in the set; else as [`AppendRegion::append_after`].
    pub fn append_after(
        &self,
        region: &AppendRegion<P>,
        deps: &[DurableReceipt],
        bytes: &[u8],
    ) -> Result<WriteTicket, Error> {
        self.member(region.store())?;
        region.append_after_in(deps, bytes, &self.stores)
    }

    /// [`PageRegion::write_after`] with dependencies on any store of the set
    /// (as [`Self::append_after`]).
    ///
    /// # Errors
    ///
    /// `NotWritten(ForeignPosition)` before any work if `region` is of no
    /// store in the set; else as [`PageRegion::write_after`].
    pub fn write_after(
        &self,
        region: &PageRegion<P>,
        deps: &[DurableReceipt],
        pos: RegionPos,
        bytes: &[u8],
    ) -> Result<WriteTicket, Error> {
        self.member(region.store())?;
        region.write_after_in(deps, pos, bytes, &self.stores)
    }
}

impl<P: Platform> core::ops::Index<usize> for StoreSet<P> {
    type Output = Store<P>;

    fn index(&self, i: usize) -> &Store<P> {
        &self.stores[i]
    }
}

impl<'a, P: Platform> IntoIterator for &'a StoreSet<P> {
    type Item = &'a Store<P>;
    type IntoIter = core::slice::Iter<'a, Store<P>>;

    fn into_iter(self) -> Self::IntoIter {
        self.stores.iter()
    }
}

impl<P: Platform> Submitter<P>
where
    P::Queue: Send,
{
    /// [`Self::append_after`] with dependencies on any store of `set` (data
    /// through a submitter on one device, the pointer through one on
    /// another).
    ///
    /// # Errors
    ///
    /// `NotWritten(ForeignPosition)`, with the buffer and before any work,
    /// unless this submitter's store is in the set (so its own store's
    /// dependencies are always checked); else as [`Self::append_after`].
    pub fn append_after_in(
        &mut self,
        set: &StoreSet<P>,
        region: &AppendRegion<P>,
        deps: &[DurableReceipt],
        buf: IoBuf,
        mode: WriteMode,
        tag: u64,
    ) -> Result<RegionPos, Rejected> {
        if let Err(error) = set.member(&self.store) {
            return Err(Rejected {
                error,
                buf: Some(buf),
            });
        }
        self.append_after_with(region, deps, buf, mode, tag, &set.stores)
    }

    /// [`Self::write_after`] with dependencies on any store of `set`.
    ///
    /// # Errors
    ///
    /// As [`Self::append_after_in`], else as [`Self::write_after`].
    #[allow(clippy::too_many_arguments)]
    pub fn write_after_in(
        &mut self,
        set: &StoreSet<P>,
        region: &PageRegion<P>,
        deps: &[DurableReceipt],
        pos: RegionPos,
        buf: IoBuf,
        mode: WriteMode,
        tag: u64,
    ) -> Result<(), Rejected> {
        if let Err(error) = set.member(&self.store) {
            return Err(Rejected {
                error,
                buf: Some(buf),
            });
        }
        self.write_after_with(region, deps, pos, buf, mode, tag, &set.stores)
    }
}

// ---------------------------------------------------------------- answers

/// One call's answer channel: the flush threads it handed stores to post
/// here, and a store's poison wakes it (R5.3 steps 4-5).
pub(crate) struct Answers {
    state: Mutex<AnswerState>,
    ready: Condvar,
    /// Outcomes of the current call posted: read without the lock while
    /// the call yields.
    posted: AtomicU64,
}

struct AnswerState {
    /// The call these answers are for; outcomes of any other are dropped.
    seq: u64,
    /// Per store of the set, its flush thread's outcome once posted.
    slots: Vec<Option<Flushed>>,
}

/// What a call needs besides its result, kept by each thread for its next
/// call: no allocation per call once warm.
struct Spare {
    answers: Arc<Answers>,
    states: Vec<Ticket>,
    store_of: Vec<usize>,
    parts: Vec<Part>,
}

thread_local! {
    /// Each thread's spare: a call takes it and gives it back.
    static SPARE: RefCell<Option<Spare>> = const { RefCell::new(None) };
}

fn lock_answers(a: &Answers) -> MutexGuard<'_, AnswerState> {
    a.state.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Spare {
    /// This thread's spare (or a new one), emptied.
    fn take() -> Self {
        let mut s = SPARE
            .try_with(|s| s.try_borrow_mut().ok().and_then(|mut s| s.take()))
            .ok()
            .flatten()
            .unwrap_or_else(|| Self {
                answers: Arc::new(Answers {
                    state: Mutex::new(AnswerState {
                        seq: 0,
                        slots: Vec::new(),
                    }),
                    ready: Condvar::new(),
                    posted: AtomicU64::new(0),
                }),
                states: Vec::new(),
                store_of: Vec::new(),
                parts: Vec::new(),
            });
        s.states.clear();
        s.store_of.clear();
        s.parts.clear();
        s
    }

    /// Back to this thread (a flush thread may still hold the answers, to
    /// post a late outcome the next call drops).
    fn give_back(mut self) {
        if self.parts.capacity() > SPARE_KEEP || self.states.capacity() > SPARE_KEEP {
            return;
        }
        // No region is kept alive by a spare.
        self.states.clear();
        // From a thread-local's destructor the spare is just dropped (never
        // a panic in a drop).
        let _kept = SPARE.try_with(|s| {
            if let Ok(mut slot) = s.try_borrow_mut() {
                *slot = Some(self);
            }
        });
    }
}

impl Answers {
    /// Sets these answers up for call `seq` of a set of `stores`: every
    /// slot empty, and any other call's outcome dropped from now on.
    fn reset(&self, seq: u64, stores: usize) {
        let mut st = lock_answers(self);
        st.seq = seq & SEQ_MASK;
        st.slots.clear();
        st.slots.resize_with(stores, || None);
        self.posted.store(0, Ordering::SeqCst);
    }
}

/// Sleeps on `a` until `settled` holds of its slots, read under its mutex
/// (so neither a post nor a poison wake after a check is missed). Returns
/// how many times it slept.
fn wait_settled(a: &Answers, settled: impl Fn(&AnswerState) -> bool) -> u64 {
    let mut st = lock_answers(a);
    let mut sleeps = 0;
    while !settled(&st) {
        sleeps += 1;
        st = a.ready.wait(st).unwrap_or_else(PoisonError::into_inner);
    }
    sleeps
}

impl Answer for Answers {
    fn post(&self, tag: u64, outcome: Flushed) {
        let (seq, i) = (tag >> INDEX_BITS, (tag & ((1 << INDEX_BITS) - 1)) as usize);
        let mut st = lock_answers(self);
        if st.seq != seq {
            return;
        }
        if let Some(slot) = st.slots.get_mut(i) {
            if slot.is_none() {
                *slot = Some(outcome);
                let _ = self.posted.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    fn signal(&self) {
        // The slot was set under the mutex, which the caller holds while it
        // checks the slots: notified here, it is either waiting or has not
        // checked yet.
        self.ready.notify_one();
    }
}

impl PoisonWaker for Answers {
    fn poisoned(&self) {
        // Under the mutex: the caller reads the poison under it before it
        // waits, so this wake is never missed.
        let _sync = lock_answers(self);
        self.ready.notify_one();
    }
}

// ---------------------------------------------------------------- one call

/// A ticket's progress through a call.
enum Ticket {
    /// Answered.
    Done(Result<DurableReceipt, Error>),
    /// An append ticket: its region, whether a data flush backs its receipt
    /// (the store flushes, or the region forces data sync, read once), and
    /// its completed prefix once its store is ready.
    Append {
        region: Arc<LiveRegion>,
        synced: bool,
        snapshot: Option<u64>,
    },
    /// A page (or slot) ticket: whether a data flush backs its receipt.
    Page { synced: bool },
}

/// One store's part in a call.
#[derive(Default)]
struct Part {
    /// Tickets of this store not yet answered.
    open: usize,
    /// Every append ticket has completed through its end.
    ready: bool,
    /// The flush that covers its tickets.
    need: u64,
    /// The store's flush, its flush thread's or this call's.
    outcome: Option<Flushed>,
    /// Handed to its flush thread; the poison waker's key.
    handed: Option<u64>,
}

struct Call<'a, P: Platform>
where
    P::Queue: Send,
{
    set: &'a StoreSet<P>,
    tickets: &'a [&'a WriteTicket],
    states: Vec<Ticket>,
    /// Per ticket, its store's index.
    store_of: Vec<usize>,
    parts: Vec<Part>,
    answers: Arc<Answers>,
    seq: u64,
    sleeps: u64,
    fallbacks: u64,
}

impl<'a, P: Platform> Call<'a, P>
where
    P::Queue: Send,
{
    /// Matches every ticket (R5.3 step 1); answers what needs no flush.
    fn new(set: &'a StoreSet<P>, tickets: &'a [&'a WriteTicket]) -> Result<Self, Error> {
        let Spare {
            answers,
            mut states,
            mut store_of,
            mut parts,
        } = Spare::take();
        for t in tickets {
            let at = set
                .stores
                .iter()
                .position(|s| s.inner.volume == t.volume && s.inner.epoch == t.epoch);
            let Some(i) = at else {
                Spare {
                    answers,
                    states,
                    store_of,
                    parts,
                }
                .give_back();
                return Err(not_written(NotWrittenCause::ForeignPosition, ctx(t)));
            };
            store_of.push(i);
        }
        parts.resize_with(set.stores.len(), Part::default);
        for (t, &i) in tickets.iter().zip(&store_of) {
            let state = Self::matched(&set.stores[i], t);
            if !matches!(state, Ticket::Done(_)) {
                parts[i].open += 1;
            }
            states.push(state);
        }
        let seq = CALLS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        answers.reset(seq, set.stores.len());
        Ok(Self {
            set,
            tickets,
            states,
            store_of,
            parts,
            answers,
            seq,
            sleeps: 0,
            fallbacks: 0,
        })
    }

    /// A ticket matched to its store's live region.
    fn matched(store: &Store<P>, t: &WriteTicket) -> Ticket {
        let c = ctx(t);
        let found = store
            .inner
            .regions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|r| r.id == t.region)
            .map(Arc::clone);
        let Some(region) = found else {
            return Ticket::Done(Err(not_written(NotWrittenCause::NotReady, c)));
        };
        if region.generation != t.generation || region.gate.check().is_err() {
            return Ticket::Done(Err(not_written(NotWrittenCause::StaleGeneration, c)));
        }
        let flushes = store.inner.policy.flush;
        let synced = flushes || region.forced();
        let Some(f) = region.frontier.as_ref() else {
            return Ticket::Page { synced };
        };
        // Already durable: no flush (as `sync_through`). A forced region on
        // a power-safe store counts only what a data flush made durable.
        let durable = if synced && !flushes {
            f.synced()
        } else {
            f.durable()
        };
        if durable >= t.end {
            store.inner.domain.count_already_durable();
            return Ticket::Done(Ok(store.receipt(&region, 0, durable, 0, true, synced)));
        }
        Ticket::Append {
            region,
            synced,
            snapshot: None,
        }
    }

    fn run(&mut self) {
        let mut lead: Option<usize> = None;
        loop {
            // Every store found ready is decided; each one needing a flush
            // is handed off, except one kept back while nothing remains to
            // wait for, which this thread leads.
            for i in 0..self.parts.len() {
                if self.parts[i].open == 0 || self.parts[i].ready || !self.is_ready(i) {
                    continue;
                }
                if self.ready(i) {
                    if let Some(prev) = lead.replace(i) {
                        self.hand_off(prev);
                    }
                }
            }
            if self.parts.iter().all(|p| p.open == 0 || p.ready) {
                break;
            }
            // More to wait for: nothing is kept back meanwhile.
            if let Some(prev) = lead.take() {
                self.hand_off(prev);
            }
            // A store whose last ticket completed since the sweep has none
            // to wait on: the next sweep finds it ready.
            if let Some(waiting) = self.first_unready() {
                self.wait_ready(waiting);
            }
        }
        if let Some(i) = lead {
            let need = self.parts[i].need;
            self.parts[i].outcome = Some(self.set.stores[i].barrier_to(need).map(|()| need));
        }
        self.collect();
    }

    /// Whether every append ticket of store `i` has completed through its
    /// end (or its store is poisoned: then nothing more completes).
    fn is_ready(&self, i: usize) -> bool {
        if self.set.stores[i].inner.domain.poisoned().is_some() {
            return true;
        }
        self.states
            .iter()
            .zip(self.tickets)
            .zip(&self.store_of)
            .all(|((s, t), &j)| match s {
                Ticket::Append { region, .. } if j == i => region
                    .frontier
                    .as_ref()
                    .is_none_or(|f| f.completed() >= t.end),
                _ => true,
            })
    }

    /// Store `i` is ready: snapshots its append tickets' prefixes, then
    /// takes its `need` (after the last snapshot), and decides whether it
    /// needs a flush. Returns true if it does.
    fn ready(&mut self, i: usize) -> bool {
        let store = &self.set.stores[i];
        let domain = &store.inner.domain;
        let mut need = 0;
        let mut appends = false;
        let mut flush = store.inner.policy.flush;
        for ((s, t), &j) in self.states.iter_mut().zip(self.tickets).zip(&self.store_of) {
            if j != i {
                continue;
            }
            match s {
                Ticket::Append {
                    region,
                    synced,
                    snapshot,
                } => {
                    *snapshot = region.frontier.as_ref().map(|f| f.completed());
                    appends = true;
                    flush |= *synced;
                }
                Ticket::Page { synced } => {
                    need = need.max(t.need);
                    flush |= *synced;
                }
                Ticket::Done(_) => {}
            }
        }
        if appends {
            // After the last snapshot: the flush covers every byte of them.
            need = need.max(domain.ticket());
        }
        let part = &mut self.parts[i];
        part.ready = true;
        part.need = need;
        if let Some(first) = domain.poisoned() {
            part.outcome = Some(Err(Error::Poisoned { first }));
            return false;
        }
        if !flush {
            // Power-safe, no forced region: durable at completion.
            part.outcome = Some(Ok(u64::MAX));
            return false;
        }
        if domain.join_if_done(need) {
            part.outcome = Some(Ok(need));
            return false;
        }
        true
    }

    /// The first store not yet ready, and the ticket to wait on.
    fn first_unready(&self) -> Option<(usize, usize)> {
        self.states.iter().enumerate().find_map(|(k, s)| match s {
            Ticket::Append { region, .. } => {
                let i = self.store_of[k];
                let done = region
                    .frontier
                    .as_ref()
                    .is_none_or(|f| f.completed() >= self.tickets[k].end);
                (!self.parts[i].ready && !done).then_some((i, k))
            }
            _ => None,
        })
    }

    /// Waits until ticket `k` of store `i` has completed (or the store is
    /// poisoned).
    fn wait_ready(&self, (i, k): (usize, usize)) {
        let Ticket::Append { region, .. } = &self.states[k] else {
            return;
        };
        let domain = &self.set.stores[i].inner.domain;
        if let Some(f) = region.frontier.as_ref() {
            let _reached = f.wait_completed(self.tickets[k].end, || domain.poisoned().is_some());
        }
    }

    /// Hands store `i`'s flush to its flush thread, registered for its
    /// poison first; flushes it on this thread if no flush thread takes it.
    fn hand_off(&mut self, i: usize) {
        let store = &self.set.stores[i];
        let need = self.parts[i].need;
        if let Some(f) = store.flusher() {
            let waker: Arc<dyn PoisonWaker> = Arc::clone(&self.answers) as Arc<dyn PoisonWaker>;
            let key = store.inner.domain.on_poison(waker);
            let tag = (self.seq << INDEX_BITS) | i as u64;
            if f.request(need, tag, Arc::clone(&self.answers) as Arc<dyn Answer>) {
                self.parts[i].handed = Some(key);
                return;
            }
            store.inner.domain.forget_poison_waker(key);
        }
        self.fallbacks += 1;
        self.parts[i].outcome = Some(store.barrier_to(need).map(|()| need));
    }

    /// Waits for every handed-off store's answer, or its poison: yielding
    /// first, then asleep on the answers.
    fn collect(&mut self) {
        let want = self.parts.iter().filter(|p| p.handed.is_some()).count() as u64;
        if want == 0 {
            return;
        }
        let answers = Arc::clone(&self.answers);
        let all_posted = || answers.posted.load(Ordering::SeqCst) >= want;
        if !self.set.patience.yield_until(all_posted) {
            // The poison is read after the registration (in `hand_off`) and
            // under the mutex the poison waker takes: a poison before it is
            // seen here, one after it wakes the wait.
            let slept = wait_settled(&answers, |st| self.settled(st));
            self.sleeps += slept;
        }
        let mut st = lock_answers(&answers);
        for (i, part) in self.parts.iter_mut().enumerate() {
            if part.handed.is_none() {
                continue;
            }
            let store = &self.set.stores[i];
            part.outcome = Some(st.slots[i].take().unwrap_or_else(|| {
                Err(Error::Poisoned {
                    first: store.inner.domain.poisoned().unwrap_or(FirstCause {
                        op: None,
                        raw: None,
                    }),
                })
            }));
        }
        drop(st);
    }

    /// Whether every handed-off store has answered or is poisoned.
    fn settled(&self, st: &AnswerState) -> bool {
        self.parts.iter().enumerate().all(|(i, p)| {
            p.handed.is_none()
                || st.slots[i].is_some()
                || self.set.stores[i].inner.domain.poisoned().is_some()
        })
    }

    /// Stores this call waited on (each once).
    fn stores_waited(&self) -> u64 {
        self.parts.iter().filter(|p| p.ready).count() as u64
    }

    /// Every ticket's receipt or error (R5.3 step 6).
    fn finish(mut self) -> Vec<Result<DurableReceipt, Error>> {
        // No receipt below a store's need: a violation poisons that store
        // (with no lock of the call's held) and fails its tickets.
        for (i, part) in self.parts.iter_mut().enumerate() {
            if let Some(Ok(n)) = part.outcome {
                if n < part.need {
                    let store = &self.set.stores[i];
                    store.inner.domain.poison(FirstCause {
                        op: None,
                        raw: None,
                    });
                    store.wake_frontiers();
                    part.outcome = Some(Err(Error::Poisoned {
                        first: store.inner.domain.poisoned().unwrap_or(FirstCause {
                            op: None,
                            raw: None,
                        }),
                    }));
                }
            }
        }
        let mut out = Vec::with_capacity(self.tickets.len());
        let mut states = std::mem::take(&mut self.states);
        for ((s, t), &i) in states.drain(..).zip(self.tickets).zip(&self.store_of) {
            let store = &self.set.stores[i];
            let part = &self.parts[i];
            out.push(match s {
                Ticket::Done(r) => r,
                Ticket::Append {
                    region,
                    synced,
                    snapshot,
                } => match &part.outcome {
                    Some(Ok(_)) => {
                        let need = if synced { part.need } else { u64::MAX };
                        let through = snapshot.unwrap_or(0);
                        Ok(sync_tail(store, &region, through, need, synced))
                    }
                    Some(Err(e)) => failed_append(store, &region, t, synced, e),
                    None => Err(Error::Poisoned {
                        first: FirstCause {
                            op: None,
                            raw: None,
                        },
                    }),
                },
                Ticket::Page { synced } => match &part.outcome {
                    Some(Ok(_)) => {
                        let need = if synced { part.need } else { u64::MAX };
                        Ok(store.receipt_for(t, need))
                    }
                    Some(Err(e)) => failed_page(store, t, e),
                    None => Err(Error::Poisoned {
                        first: FirstCause {
                            op: None,
                            raw: None,
                        },
                    }),
                },
            });
        }
        self.states = states;
        out
    }
}

impl<P: Platform> Drop for Call<'_, P>
where
    P::Queue: Send,
{
    fn drop(&mut self) {
        // Unregistered on every exit, with no lock of the answers held.
        for (i, part) in self.parts.iter_mut().enumerate() {
            if let Some(key) = part.handed.take() {
                self.set.stores[i].inner.domain.forget_poison_waker(key);
            }
        }
        Spare {
            answers: Arc::clone(&self.answers),
            states: std::mem::take(&mut self.states),
            store_of: std::mem::take(&mut self.store_of),
            parts: std::mem::take(&mut self.parts),
        }
        .give_back();
    }
}

/// A failed store's append ticket (R5.3 step 6): a prefix receipt if what
/// is already published reaches its end, else a range receipt if a finished
/// flush covers it, else the store's error.
fn failed_append<P: Platform>(
    store: &Store<P>,
    region: &LiveRegion,
    t: &WriteTicket,
    synced: bool,
    e: &Error,
) -> Result<DurableReceipt, Error>
where
    P::Queue: Send,
{
    let flushes = store.inner.policy.flush;
    if let Some(f) = region.frontier.as_ref() {
        let durable = if synced && !flushes {
            f.synced()
        } else {
            f.durable()
        };
        if durable >= t.end {
            return Ok(store.receipt(region, 0, durable, 0, true, synced));
        }
    }
    failed_page(store, t, e)
}

/// A failed store's page ticket: a range receipt if a finished flush covers
/// it, else the store's error.
fn failed_page<P: Platform>(
    store: &Store<P>,
    t: &WriteTicket,
    e: &Error,
) -> Result<DurableReceipt, Error>
where
    P::Queue: Send,
{
    if store.inner.domain.join_if_done(t.need) {
        Ok(store.receipt_for(t, t.need))
    } else {
        Err(in_range(e.clone(), t))
    }
}

fn ctx(t: &WriteTicket) -> ErrorContext {
    ErrorContext {
        region: Some(t.region),
        range: Some(store_io_core::error::ByteRange {
            start: t.start,
            end: t.end,
        }),
    }
}

#[cfg(all(test, loom))]
mod loom_tests {
    use super::*;

    use crate::domain::Domain;

    fn answers(seq: u64) -> Arc<Answers> {
        let a = Arc::new(Answers {
            state: Mutex::new(AnswerState {
                seq: 0,
                slots: Vec::new(),
            }),
            ready: Condvar::new(),
            posted: AtomicU64::new(0),
        });
        a.reset(seq, 1);
        a
    }

    /// Waits as a call does (`wait_settled`), until store 0 answered or
    /// `d` is poisoned.
    fn wait(a: &Answers, d: Option<&Domain>) -> Option<Flushed> {
        let _slept = wait_settled(a, |st| {
            st.slots[0].is_some() || d.is_some_and(|d| d.poisoned().is_some())
        });
        lock_answers(a).slots[0].take()
    }

    /// Answers reused by the next call: a late outcome of the earlier call,
    /// posted before or after the reuse, is never taken as the next call's.
    #[test]
    fn loom_reused_answers_drop_a_late_outcome() {
        loom::model(|| {
            let a = answers(1);
            let late = {
                let a = Arc::clone(&a);
                loom::thread::spawn(move || {
                    a.post(1 << INDEX_BITS, Ok(7));
                    a.signal();
                })
            };
            a.reset(2, 1);
            let own = {
                let a = Arc::clone(&a);
                loom::thread::spawn(move || {
                    a.post(2 << INDEX_BITS, Ok(9));
                    a.signal();
                })
            };
            assert!(matches!(wait(&a, None), Some(Ok(9))));
            let _l = late.join();
            let _o = own.join();
        });
    }

    /// A poison landing anywhere around the call's registration ends its
    /// wait: before it, the call's read sees it; after it, the waker wakes
    /// the call.
    #[test]
    fn loom_a_poison_around_the_registration_ends_the_wait() {
        loom::model(|| {
            let d = Arc::new(Domain::new());
            let a = answers(1);
            let poisoner = {
                let d = Arc::clone(&d);
                loom::thread::spawn(move || {
                    d.poison(FirstCause {
                        op: None,
                        raw: None,
                    });
                })
            };
            let key = d.on_poison(Arc::clone(&a) as Arc<dyn PoisonWaker>);
            assert!(wait(&a, Some(&d)).is_none());
            d.forget_poison_waker(key);
            let _p = poisoner.join();
        });
    }
}
