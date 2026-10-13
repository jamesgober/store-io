//! The T3 submit pool (D-051, D-096): worker threads that run the
//! operations of parallel queues concurrently.
//!
//! One pool per [`PosixPlatform`](crate::PosixPlatform), shared by every
//! store on it, started when the first parallel queue is created. Each worker
//! parks untimed on a condvar (no timer, no idle wakeup), takes a job,
//! performs it (one system call; a durable write to a raw block device adds
//! an `fdatasync`) and posts the completion to the submitting queue's
//! [`Inbox`]. Jobs carry a clone of the file's descriptor (`Arc<OwnedFd>`),
//! so the descriptor stays open, and its number unused by any other file,
//! until the call has returned; the job owns its buffer from `submit` until
//! the completion is posted (platform contracts 1 and 2).
//!
//! Nothing here allocates per job: each parallel queue reserves room for its
//! depth in the job queue when it is created, and its inbox holds its depth.
//! Wakes are signalled after the lock is released, and only as many as there
//! are parked workers not already signalled.
//!
//! Workers block every signal except those the kernel forces on the thread
//! that caused them (faults, traps), so a process's handlers for any other
//! signal never run on them. A worker that panicked would leave its job's
//! completion unposted and its queue's `wait` and drop blocked forever, so a
//! panic aborts the process instead (fail-stop). After `fork()` the child has
//! no workers: a pooled queue must not be used there.

use std::collections::VecDeque;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::thread::JoinHandle;

use store_io_platform::{Completion, CompletionBuf, RawResult};

use crate::queue::Work;
use crate::sync::{Condvar, Mutex, lock, wait};

/// Completions posted by the pool for one parallel queue, and its waiter.
pub(crate) struct Inbox {
    state: Mutex<InboxState>,
    ready: Condvar,
}

struct InboxState {
    /// Posted, not yet reaped. Capacity is the queue's depth and the queue
    /// never has more than that in flight, so a post never allocates.
    done: Vec<Completion>,
    /// Completions the queue's waiting thread needs; 0 when none waits. The
    /// waiter is woken once, when its target is reached, not per post.
    want: usize,
}

impl Inbox {
    pub(crate) fn new(depth: usize) -> Self {
        Self {
            state: Mutex::new(InboxState {
                done: Vec::with_capacity(depth),
                want: 0,
            }),
            ready: Condvar::new(),
        }
    }

    /// Posts a finished operation (a worker).
    pub(crate) fn post(&self, c: Completion) {
        let mut st = lock(&self.state);
        st.done.push(c);
        let reached = st.want != 0 && st.done.len() >= st.want;
        if reached {
            st.want = 0;
        }
        drop(st);
        // After the unlock, so the woken waiter does not block on the lock.
        if reached {
            self.ready.notify_one();
        }
    }

    /// Moves posted completions into `out`, as many as fit; never blocks.
    pub(crate) fn reap(&self, out: &mut CompletionBuf) -> usize {
        move_into(&mut lock(&self.state).done, out)
    }

    /// Blocks, untimed, until `target` completions are posted, then moves as
    /// many as fit into `out`. The caller keeps `target` at most what it has
    /// in flight and at most `out`'s room, so the wait always ends.
    pub(crate) fn wait(&self, target: usize, out: &mut CompletionBuf) -> usize {
        let mut st = lock(&self.state);
        while st.done.len() < target {
            st.want = target;
            st = wait(&self.ready, st);
        }
        st.want = 0;
        move_into(&mut st.done, out)
    }

    /// Blocks, untimed, until `n` completions are posted, then drops them.
    /// For a queue dropped with `n` operations in flight: when it returns,
    /// every one of the queue's operations has finished (none can land later)
    /// and every buffer is back in its pool.
    pub(crate) fn settle(&self, n: usize) {
        let mut st = lock(&self.state);
        while st.done.len() < n {
            st.want = n;
            st = wait(&self.ready, st);
        }
        st.want = 0;
        st.done.clear();
    }
}

fn move_into(done: &mut Vec<Completion>, out: &mut CompletionBuf) -> usize {
    let n = out.room().min(done.len());
    let mut added = 0;
    for c in done.drain(..n) {
        // `n` is at most the room, so no push is refused.
        if out.push(c).is_ok() {
            added += 1;
        }
    }
    added
}

/// The pool's job queue: a deque guarded by a mutex, and a condvar the idle
/// workers park on.
pub(crate) struct Jobs<J> {
    state: Mutex<JobState<J>>,
    work: Condvar,
}

struct JobState<J> {
    queue: VecDeque<J>,
    /// Sum of the live parallel queues' depths: the most jobs that can be
    /// queued at once, and the capacity kept reserved.
    reserved: usize,
    /// Workers parked on `work`.
    idle: usize,
    /// Parked workers already signalled and not yet awake: a push wakes
    /// another only while `idle > signalled`.
    signalled: usize,
    stop: bool,
}

impl<J> Jobs<J> {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(JobState {
                queue: VecDeque::new(),
                reserved: 0,
                idle: 0,
                signalled: 0,
                stop: false,
            }),
            work: Condvar::new(),
        }
    }

    /// Makes room for `depth` more queued jobs (a parallel queue was
    /// created), so that `push` never allocates.
    pub(crate) fn reserve(&self, depth: usize) {
        let mut st = lock(&self.state);
        st.reserved = st.reserved.saturating_add(depth);
        let more = st.reserved.saturating_sub(st.queue.len());
        st.queue.reserve(more);
    }

    /// Gives back `depth` (a parallel queue was dropped, with nothing in
    /// flight). The capacity is kept for the next queue.
    pub(crate) fn release(&self, depth: usize) {
        let mut st = lock(&self.state);
        st.reserved = st.reserved.saturating_sub(depth);
    }

    /// Queues a job and wakes one parked worker, if one is parked and not
    /// already signalled.
    pub(crate) fn push(&self, job: J) {
        let mut st = lock(&self.state);
        st.queue.push_back(job);
        let wake = st.idle > st.signalled;
        if wake {
            st.signalled += 1;
        }
        drop(st);
        // After the unlock, so the woken worker does not block on the lock.
        // A signal that finds no waiter (it woke early) is harmless: every
        // awake worker looks at the queue before it parks again.
        if wake {
            self.work.notify_one();
        }
    }

    /// The next job, parking untimed while there is none; `None` once the
    /// pool is stopping and every queued job has been taken.
    pub(crate) fn pop(&self) -> Option<J> {
        let mut st = lock(&self.state);
        loop {
            if let Some(job) = st.queue.pop_front() {
                return Some(job);
            }
            if st.stop {
                return None;
            }
            st.idle += 1;
            st = wait(&self.work, st);
            st.idle -= 1;
            st.signalled = st.signalled.saturating_sub(1);
        }
    }

    /// Tells every worker to exit once the queue is empty.
    pub(crate) fn stop(&self) {
        lock(&self.state).stop = true;
        self.work.notify_all();
    }
}

/// One pooled operation: the work, its tag, and the inbox to post it to.
pub(crate) struct Job {
    pub(crate) work: Work<Arc<OwnedFd>>,
    pub(crate) tag: u64,
    pub(crate) inbox: Arc<Inbox>,
}

impl Job {
    fn run(self) {
        let Self { work, tag, inbox } = self;
        // `perform` drops the descriptor clone before returning: once the
        // completion is visible, the job holds nothing of the file.
        let (buf, result) = work.perform();
        inbox.post(Completion { tag, buf, result });
    }
}

/// The worker threads and their job queue.
pub(crate) struct SubmitPool {
    jobs: Arc<Jobs<Job>>,
    workers: Vec<JoinHandle<()>>,
}

impl core::fmt::Debug for SubmitPool {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SubmitPool")
            .field("threads", &self.workers.len())
            .finish()
    }
}

/// The default size: one worker per CPU, at most 32 (D-052).
pub(crate) fn default_threads() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get().min(32))
}

/// The most workers a pool starts, whatever is asked: the pool is bounded.
pub(crate) const MAX_THREADS: usize = 256;

/// A worker's stack: it makes one system call and posts a completion, so a
/// small fixed stack keeps a large pool cheap in address space. The panic
/// path (hook and backtrace) was measured to need under 32 KiB; glibc counts
/// the process's static TLS against it, and a process-wide custom panic hook
/// runs on it too, hence the margin.
const WORKER_STACK: usize = 256 * 1024;

/// Aborts the process if dropped while its thread unwinds: a worker that died
/// holding a job would leave that job's queue waiting forever.
struct AbortOnPanic;

impl Drop for AbortOnPanic {
    fn drop(&mut self) {
        if std::thread::panicking() {
            std::process::abort();
        }
    }
}

impl SubmitPool {
    /// Starts `threads` workers (at least 1, at most [`MAX_THREADS`]). If
    /// the system refuses a thread after the first, the pool runs with those
    /// it has, and [`Self::threads`] says how many.
    ///
    /// # Errors
    ///
    /// The raw error when not even one worker could be started.
    pub(crate) fn start(threads: usize) -> RawResult<Self> {
        let jobs: Arc<Jobs<Job>> = Arc::new(Jobs::new());
        let threads = threads.clamp(1, MAX_THREADS);
        let mut workers = Vec::with_capacity(threads);
        for i in 0..threads {
            let mine = Arc::clone(&jobs);
            // Blocked around the spawn, so the worker starts with every
            // signal blocked but the ones the kernel forces on a faulting
            // thread (a thread inherits its creator's mask), and opens those
            // too in case the caller had blocked one; the caller's mask is
            // restored right after the spawn.
            let blocked = crate::sys::SignalsBlocked::new();
            let spawned = std::thread::Builder::new()
                .name(format!("sio-t3-{i}"))
                .stack_size(WORKER_STACK)
                .spawn(move || {
                    crate::sys::unblock_fault_signals();
                    let _fail_stop = AbortOnPanic;
                    while let Some(job) = mine.pop() {
                        job.run();
                    }
                });
            drop(blocked);
            match spawned {
                Ok(h) => workers.push(h),
                Err(e) if workers.is_empty() => {
                    return Err(crate::sys::errno(e.raw_os_error().unwrap_or(libc::EAGAIN)));
                }
                Err(_) => break,
            }
        }
        Ok(Self { jobs, workers })
    }

    /// Worker threads running.
    pub(crate) fn threads(&self) -> usize {
        self.workers.len()
    }

    pub(crate) fn reserve(&self, depth: usize) {
        self.jobs.reserve(depth);
    }

    pub(crate) fn release(&self, depth: usize) {
        self.jobs.release(depth);
    }

    pub(crate) fn push(&self, job: Job) {
        self.jobs.push(job);
    }
}

impl Drop for SubmitPool {
    /// The last reference is gone, so every parallel queue has been dropped,
    /// and each waited for its jobs: the queue is empty and the workers exit
    /// at once.
    fn drop(&mut self) {
        self.jobs.stop();
        for h in self.workers.drain(..) {
            let _joined = h.join();
        }
    }
}

#[cfg(all(test, loom))]
#[allow(clippy::unwrap_used)]
mod loom_tests {
    use super::*;
    use loom::sync::Arc as LoomArc;

    /// Runs a model with `LOOM_MAX_PREEMPTIONS`, or a bound of 3 (as CI
    /// sets) when that is not set, so every model finishes either way.
    fn model<F: Fn() + Sync + Send + 'static>(f: F) {
        let mut b = loom::model::Builder::new();
        if b.preemption_bound.is_none() {
            b.preemption_bound = Some(3);
        }
        b.check(f);
    }

    fn completion(tag: u64) -> Completion {
        Completion {
            tag,
            buf: None,
            result: Ok(0),
        }
    }

    /// Two workers post while the queue's thread waits for both: the waiter
    /// is never left parked with its target reached, and gets each once.
    #[test]
    fn loom_inbox_wait_sees_every_post_once() {
        model(|| {
            let inbox = LoomArc::new(Inbox::new(2));
            let posters: Vec<_> = (0..2u64)
                .map(|t| {
                    let ib = LoomArc::clone(&inbox);
                    loom::thread::spawn(move || ib.post(completion(t)))
                })
                .collect();
            let mut out = CompletionBuf::with_capacity(2);
            let mut got = inbox.wait(1, &mut out);
            if got < 2 {
                got += inbox.wait(1, &mut out);
            }
            for p in posters {
                p.join().unwrap();
            }
            assert_eq!(got, 2);
            let mut tags: Vec<u64> = out.drain().map(|c| c.tag).collect();
            tags.sort_unstable();
            assert_eq!(tags, vec![0, 1]);
        });
    }

    /// A target above one: neither post may wake the waiter early or be
    /// missed, whichever lands first.
    #[test]
    fn loom_inbox_wait_for_two_with_two_posters() {
        model(|| {
            let inbox = LoomArc::new(Inbox::new(2));
            let posters: Vec<_> = (0..2u64)
                .map(|t| {
                    let ib = LoomArc::clone(&inbox);
                    loom::thread::spawn(move || ib.post(completion(t)))
                })
                .collect();
            // A stray wake (loom's condvar never wakes spuriously by itself):
            // the waiter must re-check, never return short.
            let ib = LoomArc::clone(&inbox);
            let noise = loom::thread::spawn(move || ib.ready.notify_one());
            let mut out = CompletionBuf::with_capacity(2);
            assert_eq!(inbox.wait(2, &mut out), 2);
            for p in posters {
                p.join().unwrap();
            }
            noise.join().unwrap();
        });
    }

    /// A dropped queue settles: it returns only after every post.
    #[test]
    fn loom_inbox_settle_waits_for_every_post() {
        model(|| {
            let inbox = LoomArc::new(Inbox::new(2));
            let ib = LoomArc::clone(&inbox);
            let poster = loom::thread::spawn(move || {
                ib.post(completion(0));
                ib.post(completion(1));
            });
            inbox.settle(2);
            let mut out = CompletionBuf::with_capacity(2);
            assert_eq!(inbox.reap(&mut out), 0);
            poster.join().unwrap();
        });
    }

    /// A worker parked before the jobs arrive, or busy when they do, takes
    /// every job exactly once, and exits only after the stop and an empty
    /// queue.
    #[test]
    fn loom_jobs_handoff_and_stop() {
        model(|| {
            let jobs = LoomArc::new(Jobs::<u64>::new());
            jobs.reserve(2);
            let w = LoomArc::clone(&jobs);
            let worker = loom::thread::spawn(move || {
                let mut seen = Vec::new();
                while let Some(j) = w.pop() {
                    seen.push(j);
                }
                seen
            });
            jobs.push(1);
            jobs.push(2);
            jobs.stop();
            assert_eq!(worker.join().unwrap(), vec![1, 2]);
        });
    }

    /// One worker (the smallest pool): the pushes alone must wake it, since
    /// the stop comes only after both jobs completed. A lost wake deadlocks.
    #[test]
    fn loom_jobs_one_worker_woken_by_push() {
        model(|| {
            let jobs = LoomArc::new(Jobs::<u64>::new());
            jobs.reserve(2);
            let done = LoomArc::new(Inbox::new(2));
            let (w, d) = (LoomArc::clone(&jobs), LoomArc::clone(&done));
            let worker = loom::thread::spawn(move || {
                while let Some(j) = w.pop() {
                    d.post(completion(j));
                }
            });
            jobs.push(1);
            jobs.push(2);
            let mut out = CompletionBuf::with_capacity(2);
            assert_eq!(done.wait(2, &mut out), 2);
            jobs.stop();
            worker.join().unwrap();
        });
    }

    /// Two workers, parked or not, three jobs pushed together, and a stray
    /// wake: the wake accounting never leaves a job queued with every worker
    /// parked, and each job runs once. The stop comes only after all three
    /// ran, so a lost wake deadlocks the model.
    #[test]
    fn loom_jobs_two_workers_three_jobs() {
        model(|| {
            let jobs = LoomArc::new(Jobs::<u64>::new());
            jobs.reserve(3);
            let done = LoomArc::new(Inbox::new(3));
            let workers: Vec<_> = (0..2)
                .map(|_| {
                    let w = LoomArc::clone(&jobs);
                    let d = LoomArc::clone(&done);
                    loom::thread::spawn(move || {
                        while let Some(j) = w.pop() {
                            d.post(completion(j));
                        }
                    })
                })
                .collect();
            // A stray wake: a worker woken by it consumes a signal it was
            // not sent, and the accounting must still never strand a job.
            let j = LoomArc::clone(&jobs);
            let noise = loom::thread::spawn(move || j.work.notify_all());
            jobs.push(1);
            jobs.push(2);
            jobs.push(3);
            let mut out = CompletionBuf::with_capacity(3);
            assert_eq!(done.wait(3, &mut out), 3);
            jobs.stop();
            noise.join().unwrap();
            for w in workers {
                w.join().unwrap();
            }
            let mut tags: Vec<u64> = out.drain().map(|c| c.tag).collect();
            tags.sort_unstable();
            assert_eq!(tags, vec![1, 2, 3]);
            // Nothing ran twice: no fourth completion was posted.
            assert_eq!(done.reap(&mut CompletionBuf::with_capacity(1)), 0);
        });
    }
}
