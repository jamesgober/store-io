//! The store's flush thread (D-074, D-099): makes `Submitter` writes and
//! barriers durable without blocking the submitting thread, on
//! flush-required stores and for regions forcing data sync (D-102).
//!
//! One thread per store, started by the first request and parked untimed
//! when idle (no wakeups). It takes every pending request, leads or joins one
//! barrier covering them all through the domain's join rule (shared with the
//! blocking layers' barriers), and posts each outcome to its submitter's
//! [`Inbox`], waking that submitter once per batch, and only if it is
//! blocked. It reaches the store through a weak reference, upgraded only
//! while it flushes and answers, so it never keeps the store alive; the
//! store's drop stops and joins it.
//!
//! Read-only stores never start it, nor do power-safe stores unless a region
//! forces data sync.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::thread::JoinHandle;

use store_io_core::error::{Error, FirstCause, Op};
use store_io_platform::{Platform, Queue, Wake};

use crate::store::{Inner, Store};

/// The outcome of a flush request: the flush it waited for, or why none
/// covers it.
pub(crate) type Flushed = Result<u64, Error>;

/// Where the submitter is blocked, if anywhere ([`Inbox::blocked`]).
const NOWHERE: u8 = 0;
const IN_QUEUE: u8 = 1;
const ON_INBOX: u8 = 2;

/// Where the flush thread posts a submitter's outcomes, and how it (and the
/// submitter's own waker) reaches a blocked submitter.
pub(crate) struct Inbox<W> {
    /// `(queue tag, outcome)`; one entry per row at most, so it never grows
    /// past the submitter's depth.
    posted: Mutex<VecDeque<(u64, Flushed)>>,
    /// Entries in `posted`: lets the submitter skip the lock when none.
    count: AtomicUsize,
    ready: Condvar,
    /// Where the submitter blocks: in its queue's wait (the flush thread
    /// wakes it through the queue's waker) or on `ready`. Set before the
    /// submitter re-checks `count`, read after a post: one of the two always
    /// sees the other (both sequentially consistent).
    blocked: AtomicU8,
    /// A post since the flush thread last woke this submitter.
    due: AtomicBool,
    /// The submitter's waker fired (D-083): its current or next wait
    /// returns.
    woken: AtomicBool,
    queue: W,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl<W: Wake> Inbox<W> {
    pub(crate) fn new(depth: usize, queue: W) -> Self {
        Self {
            posted: Mutex::new(VecDeque::with_capacity(depth)),
            count: AtomicUsize::new(0),
            ready: Condvar::new(),
            blocked: AtomicU8::new(NOWHERE),
            due: AtomicBool::new(false),
            woken: AtomicBool::new(false),
            queue,
        }
    }

    /// Posts an outcome without waking anyone (see [`Self::signal`]).
    fn post(&self, qtag: u64, outcome: Flushed) {
        lock(&self.posted).push_back((qtag, outcome));
        let _was = self.count.fetch_add(1, Ordering::SeqCst);
        self.due.store(true, Ordering::Relaxed);
    }

    /// Wakes the submitter once for everything posted since the last call,
    /// and only where it is blocked (no wake left behind to cut a later
    /// wait short).
    fn signal(&self) {
        if !self.due.swap(false, Ordering::Relaxed) {
            return;
        }
        match self.blocked.load(Ordering::SeqCst) {
            IN_QUEUE => self.queue.wake(),
            ON_INBOX => {
                let _sync = lock(&self.posted);
                self.ready.notify_one();
            }
            _ => {}
        }
    }

    /// Takes the oldest posted outcome, if any.
    pub(crate) fn pop(&self) -> Option<(u64, Flushed)> {
        if self.count.load(Ordering::SeqCst) == 0 {
            return None;
        }
        let got = lock(&self.posted).pop_front();
        if got.is_some() {
            let _was = self.count.fetch_sub(1, Ordering::SeqCst);
        }
        got
    }

    /// Before the submitter blocks in its queue's wait: whether it may (no
    /// outcome posted). If it returns true, call [`Self::unblock`] after.
    pub(crate) fn block_in_queue(&self) -> bool {
        self.blocked.store(IN_QUEUE, Ordering::SeqCst);
        if self.count.load(Ordering::SeqCst) > 0 || self.woken.load(Ordering::SeqCst) {
            self.unblock();
            return false;
        }
        true
    }

    pub(crate) fn unblock(&self) {
        self.blocked.store(NOWHERE, Ordering::SeqCst);
    }

    /// Blocks until an outcome is posted (untimed: the flush thread answers
    /// every request it accepted) or, with `wakes`, the submitter's waker
    /// fires.
    pub(crate) fn wait(&self, wakes: bool) {
        let mut posted = lock(&self.posted);
        while posted.is_empty() && !(wakes && self.woken.load(Ordering::SeqCst)) {
            self.blocked.store(ON_INBOX, Ordering::SeqCst);
            posted = self
                .ready
                .wait(posted)
                .unwrap_or_else(PoisonError::into_inner);
        }
        self.blocked.store(NOWHERE, Ordering::SeqCst);
    }

    /// Consumes a wake from the submitter's waker.
    pub(crate) fn take_woken(&self) -> bool {
        self.woken.swap(false, Ordering::SeqCst)
    }

    /// The submitter's waker: ends its current or next wait, wherever it
    /// blocks.
    fn wake(&self) {
        self.woken.store(true, Ordering::SeqCst);
        self.queue.wake();
        let _sync = lock(&self.posted);
        self.ready.notify_one();
    }
}

/// Ends a [`crate::Submitter`]'s current or next `wait` from another thread
/// (D-083), whether it waits for I/O or for a flush.
pub struct SubmitterWaker<W>(Arc<Inbox<W>>);

impl<W> SubmitterWaker<W> {
    pub(crate) fn new(inbox: &Arc<Inbox<W>>) -> Self {
        Self(Arc::clone(inbox))
    }
}

impl<W> Clone for SubmitterWaker<W> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<W> core::fmt::Debug for SubmitterWaker<W> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("SubmitterWaker")
    }
}

impl<W: Wake> Wake for SubmitterWaker<W> {
    fn wake(&self) {
        self.0.wake();
    }
}

impl<P: Platform> Store<P>
where
    P::Queue: Send,
{
    /// The flush thread, started on first use; `None` if it could not be.
    pub(crate) fn flusher(&self) -> Option<&Flusher<P>> {
        self.inner
            .flusher
            .get_or_init(|| Flusher::start(Arc::downgrade(&self.inner)))
            .as_ref()
    }
}

/// One submitter's request: a flush `>= need`, answered into `inbox`.
struct Request<W> {
    need: u64,
    qtag: u64,
    inbox: Arc<Inbox<W>>,
    /// Its outcome, decided while the thread still holds the store and
    /// posted after it let go of it.
    outcome: Option<Flushed>,
}

struct State<W> {
    requests: Vec<Request<W>>,
    /// The store is gone, or the thread stopped: no request is accepted.
    stop: bool,
    /// The thread is parked waiting for requests (notify only then).
    idle: bool,
}

struct Shared<W> {
    state: Mutex<State<W>>,
    wake: Condvar,
}

/// The waker type of a platform's queues.
pub(crate) type WakerOf<P> = <<P as Platform>::Queue as Queue>::Waker;

/// A store's flush thread.
pub(crate) struct Flusher<P: Platform> {
    shared: Arc<Shared<WakerOf<P>>>,
    thread: Option<JoinHandle<()>>,
}

impl<P: Platform> Flusher<P>
where
    P::Queue: Send,
{
    /// Starts the thread; `None` if the OS refused it (the caller then
    /// flushes on its own thread).
    pub(crate) fn start(store: Weak<Inner<P>>) -> Option<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                requests: Vec::new(),
                stop: false,
                idle: false,
            }),
            wake: Condvar::new(),
        });
        let worker = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("store-io-flush".into())
            .spawn(move || run(&store, &worker))
            .ok()?;
        Some(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// Asks for a flush `>= need`, answered into `inbox` under `qtag`.
    /// Returns false if the thread no longer takes requests (the caller then
    /// flushes on its own thread).
    pub(crate) fn request(&self, need: u64, qtag: u64, inbox: &Arc<Inbox<WakerOf<P>>>) -> bool {
        let mut st = lock(&self.shared.state);
        if st.stop {
            return false;
        }
        st.requests.push(Request {
            need,
            qtag,
            inbox: Arc::clone(inbox),
            outcome: None,
        });
        let idle = st.idle;
        drop(st);
        if idle {
            self.shared.wake.notify_one();
        }
        true
    }
}

impl<P: Platform> Drop for Flusher<P> {
    fn drop(&mut self) {
        lock(&self.shared.state).stop = true;
        self.shared.wake.notify_one();
        if let Some(t) = self.thread.take() {
            // The thread drops the store's last handle itself when a store
            // is dropped while it answers: it then exits on its own.
            if t.thread().id() != std::thread::current().id() {
                let _ended = t.join();
            }
        }
    }
}

fn run<P: Platform>(store: &Weak<Inner<P>>, shared: &Shared<WakerOf<P>>)
where
    P::Queue: Send,
{
    let mut batch: Vec<Request<WakerOf<P>>> = Vec::new();
    let mut sent: Vec<Request<WakerOf<P>>> = Vec::new();
    loop {
        {
            let mut st = lock(&shared.state);
            while st.requests.is_empty() && !st.stop {
                st.idle = true;
                st = shared.wake.wait(st).unwrap_or_else(PoisonError::into_inner);
            }
            st.idle = false;
            if st.requests.is_empty() {
                return;
            }
            std::mem::swap(&mut st.requests, &mut batch);
        }
        let mut guard = Unwind {
            store,
            shared,
            batch: &mut batch,
            sent: &mut sent,
            answered: false,
        };
        let need = guard.batch.iter().map(|r| r.need).max().unwrap_or(0);
        let Some(inner) = store.upgrade() else {
            // Cannot happen while a request exists; answered all the same.
            return;
        };
        let held = Store { inner };
        let outcome = held.barrier_to(need).map(|()| need);
        guard.decide(&held, &outcome);
        // Let go of the store before anything is posted: once a submitter
        // sees its outcome it may drop the store's last other handle, and
        // the store must then close on that thread, not this one (its file
        // and lock released before that drop returns).
        drop(held);
        guard.answer();
    }
}

/// Answers a batch. If the thread unwinds instead (a panic while flushing
/// or answering), its drop poisons the store (those writes' durability is
/// unknown), answers every request of the batch and every request queued
/// since with `Poisoned`, and stops the thread taking more.
struct Unwind<'a, P: Platform>
where
    P::Queue: Send,
{
    store: &'a Weak<Inner<P>>,
    shared: &'a Shared<WakerOf<P>>,
    batch: &'a mut Vec<Request<WakerOf<P>>>,
    /// Requests already posted to (each is posted exactly once).
    sent: &'a mut Vec<Request<WakerOf<P>>>,
    answered: bool,
}

impl<P: Platform> Unwind<'_, P>
where
    P::Queue: Send,
{
    /// Decides every request's outcome (the batch's flush, except that a
    /// request an earlier flush already covered keeps its success when this
    /// flush failed).
    fn decide(&mut self, store: &Store<P>, outcome: &Flushed) {
        let domain = &store.inner.domain;
        for r in self.batch.iter_mut() {
            r.outcome = Some(match outcome {
                Err(_) if domain.join_if_done(r.need) => Ok(r.need),
                other => other.clone(),
            });
        }
    }

    /// Posts every decided outcome, then wakes each submitter once.
    fn answer(&mut self) {
        while let Some(mut r) = self.batch.pop() {
            let outcome = r.outcome.take().unwrap_or(Err(Error::Poisoned {
                first: FirstCause {
                    op: Some(Op::FlushData),
                    raw: None,
                },
            }));
            r.inbox.post(r.qtag, outcome);
            self.sent.push(r);
        }
        // One wake per submitter for the whole batch.
        for r in self.sent.drain(..) {
            r.inbox.signal();
        }
        self.answered = true;
    }
}

impl<P: Platform> Drop for Unwind<'_, P>
where
    P::Queue: Send,
{
    fn drop(&mut self) {
        if self.answered {
            return;
        }
        let first = FirstCause {
            op: Some(Op::FlushData),
            raw: None,
        };
        if let Some(inner) = self.store.upgrade() {
            let store = Store { inner };
            store.inner.domain.poison(first);
            store.wake_frontiers();
        }
        let late = {
            let mut st = lock(&self.shared.state);
            st.stop = true;
            std::mem::take(&mut st.requests)
        };
        for r in self.batch.drain(..).chain(late) {
            r.inbox.post(r.qtag, Err(Error::Poisoned { first }));
            r.inbox.signal();
        }
        for r in self.sent.drain(..) {
            r.inbox.signal();
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, unused_results, unused_must_use)]

    use std::path::Path;
    use std::sync::Arc;

    use store_io_platform::{Platform, Queue, QueueConfig};
    use store_io_sim::{SimConfig, SimPlatform};

    use std::sync::atomic::{AtomicUsize, Ordering};

    use store_io_platform::Wake;

    use super::{Inbox, Request, lock};
    use crate::{Store, StoreOptions};

    /// A queue waker that counts its wakes.
    #[derive(Clone, Default)]
    struct Count(Arc<AtomicUsize>);

    impl Wake for Count {
        fn wake(&self) {
            let _was = self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl Count {
        fn get(&self) -> usize {
            self.0.load(Ordering::SeqCst)
        }
    }

    /// The flush thread wakes a submitter's queue only while it is blocked
    /// there, once for any number of posts; a submitter about to block sees
    /// a post or a wake and does not.
    #[test]
    fn test_the_inbox_wakes_only_a_blocked_queue_once() {
        let n = Count::default();
        let inbox = Inbox::new(4, n.clone());
        // Not blocked: no wake.
        inbox.post(1, Ok(1));
        inbox.signal();
        assert_eq!(n.get(), 0);
        // Posted already: the submitter does not block.
        assert!(!inbox.block_in_queue());
        assert!(inbox.pop().is_some());
        // Blocked in its queue: two posts, one wake.
        assert!(inbox.block_in_queue());
        inbox.post(2, Ok(2));
        inbox.post(3, Ok(3));
        inbox.signal();
        inbox.signal();
        assert_eq!(n.get(), 1);
        inbox.unblock();
        while inbox.pop().is_some() {}
        // Its own waker: the queue is woken, and it does not block after.
        inbox.wake();
        assert_eq!(n.get(), 2);
        assert!(!inbox.block_in_queue());
        assert!(inbox.take_woken());
        assert!(inbox.block_in_queue());
        inbox.unblock();
    }

    /// Requests taken together are answered by one flush for the largest
    /// need: one already covered and one that is not both get that flush.
    #[test]
    fn test_a_batch_flushes_for_its_largest_need() {
        let p = SimPlatform::new(SimConfig::volatile(1200));
        let s = Store::create(p.clone(), Path::new("/db"), StoreOptions::default()).unwrap();
        let wal = s.provision_append_region("wal", 1 << 20).unwrap();
        let t1 = wal.append(b"a").unwrap();
        wal.sync_through(&t1).unwrap();
        let t2 = wal.append(b"b").unwrap();
        assert!(t2.need > t1.need);
        let inbox = Arc::new(Inbox::new(
            4,
            p.queue(QueueConfig::default()).unwrap().waker(),
        ));
        let f = s.flusher().unwrap();
        {
            let mut st = lock(&f.shared.state);
            for (need, qtag) in [(t1.need, 1), (t2.need, 2)] {
                st.requests.push(Request {
                    need,
                    qtag,
                    inbox: Arc::clone(&inbox),
                    outcome: None,
                });
            }
        }
        f.shared.wake.notify_one();
        let mut got = Vec::new();
        while got.len() < 2 {
            inbox.wait(false);
            while let Some(x) = inbox.pop() {
                got.push(x);
            }
        }
        got.sort_by_key(|g| g.0);
        assert_eq!(
            got[1],
            (2, Ok(t2.need)),
            "the batch flushed for a smaller need"
        );
        assert!(got[0].1.is_ok());
    }
}
