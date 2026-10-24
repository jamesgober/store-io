//! The Linux queue. On the synchronous tier (T3), inline, each operation is
//! one system call made inside `submit`, and its completion is pushed into a
//! fixed ready ring; pooled (`QueueConfig::parallel`, D-096), `submit` hands
//! the operation to the platform's submit pool and its completion arrives in
//! the queue's inbox. On the io_uring tier the queue is a ring
//! (`crate::uring::queue`).

use core::ffi::c_int;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;

use store_io_platform::{
    Completion, CompletionBuf, IoBuf, IoOp, OsError, Queue, QueueConfig, RawResult, Rejected, Wake,
};

use crate::file::PosixFile;
use crate::pool::{Inbox, Job, SubmitPool};
use crate::sys;
use crate::uring::queue::UringQueue;
use crate::uring::ring::Flavour;

/// A per-thread synchronous queue.
///
/// **Inline** (the default): `submit` performs the system call (`pwritev2`,
/// `preadv2` or `fdatasync`) and stores the completion in a ring allocated
/// once, sized by [`QueueConfig::depth`]; `reap` drains the ring, and `wait`
/// does the same, since nothing ever completes later. Zero thread hops.
/// Holding no completion, it also performs an operation directly
/// (`Queue::perform`): the same system call and checks, no completion.
///
/// **Pooled** ([`QueueConfig::parallel`]): `submit` makes the same checks,
/// then queues the operation on the platform's submit pool and returns; the
/// operations run concurrently on the pool's workers and complete in any
/// order, and `wait` blocks, untimed, until enough of them have completed. A
/// `FlushData` covers only writes that completed before it was submitted, as
/// on every asynchronous backend. Dropping a pooled queue waits for every
/// operation still in flight, so none lands after the drop (drop it before
/// the file's ownership lock), and every buffer is back in its pool when the
/// drop returns. [`Queue::workers`] reports the pool's size.
///
/// In both modes at most `depth` operations are submitted and not yet
/// reaped; beyond that `submit` refuses the operation and returns its buffer.
///
/// **io_uring** (a platform on [`crate::LinuxTier::Uring`]), for a queue
/// that keeps several operations in flight (`parallel`, or
/// [`store_io_platform::Owner::Thread`]): each read and write is submitted
/// to a ring without blocking and completes asynchronously. A queue for one
/// thread gets a ring only that thread may use (any other gets `EXDEV`); a
/// parallel shared queue gets a ring any thread may use, one at a time. A
/// queue for one operation at a time (neither) is an inline queue on this
/// tier too (D-104). Dropping it waits for every operation in
/// flight. What the kernel cannot do without blocking, or transfers short,
/// is finished by one blocking call in `reap` or `wait` (counted:
/// [`PosixQueue::fallbacks`]). A durable write uses `RWF_DSYNC` on every
/// file, raw block devices included (there the asynchronous path makes it a
/// FUA write).
///
/// On the synchronous tier, a write asked for durability at completion uses
/// `RWF_DSYNC`, except on a raw block device, where the synchronous
/// `RWF_DSYNC` path issues both a FUA write and a cache flush: there the
/// write is plain and one `fdatasync` follows it. Short transfers are
/// returned as `Ok(n)` with `n` below the buffer length and never retried;
/// `EINTR` from a data call is returned as the raw result, since a direct
/// I/O on a regular file is not interrupted before the device sees it in
/// practice.
///
/// # Examples
///
/// ```no_run
/// use std::path::Path;
/// use store_io_buf::{BufPool, PoolConfig};
/// use store_io_platform::{CompletionBuf, FileName, IoOp, Platform, Queue, QueueConfig};
/// use store_io_posix::PosixPlatform;
///
/// let p = PosixPlatform::new();
/// let dir = p.open_dir(Path::new("/var/lib/example"), true)?;
/// let name = FileName::new("data")?;
/// let file = p.create_file(&dir, &name)?;
/// p.allocate(&file, 4096)?;
/// let pool = BufPool::new(&PoolConfig::uniform(4096, 4096, 4096, 4))?;
/// let mut q = p.queue(QueueConfig::default())?;
/// let mut buf = pool.take(4096).map_err(|_| "pool exhausted")?;
/// buf.as_mut_slice().fill(0xAB);
/// q.submit(IoOp::Write { file: &file, offset: 0, buf, dsync: true }, 7)
///     .map_err(|_| "rejected")?;
/// let mut out = CompletionBuf::with_capacity(8);
/// assert_eq!(q.wait(1, &mut out)?, 1);
/// let c = out.drain().next().ok_or("no completion")?;
/// assert_eq!((c.tag, c.result), (7, Ok(4096)));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug)]
pub struct PosixQueue {
    ready: Vec<Completion>,
    depth: usize,
    pooled: Option<Pooled>,
    uring: Option<UringQueue>,
}

/// What an io_uring queue registered (D-078), from
/// [`PosixQueue::registered`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Registration {
    /// The file is the ring's direct descriptor.
    pub file: bool,
    /// The kernel's refusal, if it refused.
    pub refused: Option<OsError>,
    /// Operations submitted with the direct descriptor.
    pub direct_ops: u64,
}

/// The pooled mode's state.
struct Pooled {
    pool: Arc<SubmitPool>,
    inbox: Arc<Inbox>,
    /// Submitted and not yet reaped (some may be posted in the inbox).
    in_flight: usize,
}

impl core::fmt::Debug for Pooled {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Pooled")
            .field("pool", &self.pool)
            .field("in_flight", &self.in_flight)
            .finish_non_exhaustive()
    }
}

impl PosixQueue {
    /// Creates an inline queue whose ready ring holds `cfg.depth`
    /// completions. [`QueueConfig::parallel`] is not looked at: a pooled
    /// queue comes from [`store_io_platform::Platform::queue`], which owns
    /// the pool.
    #[must_use]
    pub fn new(cfg: QueueConfig) -> Self {
        let depth = depth_of(cfg);
        Self {
            ready: Vec::with_capacity(depth),
            depth,
            pooled: None,
            uring: None,
        }
    }

    /// A queue on an io_uring ring, thread-owned if `owned`, registering
    /// `reg` on an owned ring (D-078, best effort).
    pub(crate) fn uring(
        cfg: QueueConfig,
        owned: bool,
        reg: Option<crate::uring::queue::Registered>,
    ) -> RawResult<Self> {
        let depth = depth_of(cfg);
        let flavour = if owned {
            Flavour::Owned
        } else {
            Flavour::Shared
        };
        Ok(Self {
            ready: Vec::new(),
            depth,
            pooled: None,
            uring: Some(UringQueue::registered(flavour, depth, reg)?),
        })
    }

    /// What this queue's ring registered (D-078), and how many operations
    /// used it; `None` off a ring.
    #[must_use]
    pub fn registered(&self) -> Option<Registration> {
        self.uring.as_ref().map(|u| {
            let (f, direct_ops) = u.fixed();
            Registration {
                file: f.file,
                refused: f.refused,
                direct_ops,
            }
        })
    }

    /// Operations the io_uring tier finished with a blocking call because
    /// the kernel would have blocked (0 on the synchronous tier).
    #[must_use]
    pub fn fallbacks(&self) -> u64 {
        self.uring.as_ref().map_or(0, UringQueue::fallbacks)
    }

    /// A pooled queue on `pool`, reserving room for its depth there.
    pub(crate) fn pooled(cfg: QueueConfig, pool: Arc<SubmitPool>) -> Self {
        let depth = depth_of(cfg);
        pool.reserve(depth);
        Self {
            ready: Vec::new(),
            depth,
            pooled: Some(Pooled {
                pool,
                inbox: Arc::new(Inbox::new(depth)),
                in_flight: 0,
            }),
            uring: None,
        }
    }

    /// Ring capacity: the most operations submitted and not yet reaped.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.depth
    }

    fn drain_ready(&mut self, out: &mut CompletionBuf) -> usize {
        let n = out.room().min(self.ready.len());
        let mut added = 0;
        for c in self.ready.drain(..n) {
            // `n ≤ room()`, so no push is refused.
            if out.push(c).is_ok() {
                added += 1;
            }
        }
        added
    }
}

fn depth_of(cfg: QueueConfig) -> usize {
    usize::try_from(cfg.depth).unwrap_or(usize::MAX).max(1)
}

/// A checked operation, ready to perform: on a borrowed descriptor inline
/// (`RawFd`), on a shared one in the pool (`Arc<OwnedFd>`).
pub(crate) enum Work<F> {
    Write {
        fd: F,
        bdev: bool,
        offset: libc::off_t,
        buf: IoBuf,
        dsync: bool,
    },
    Read {
        fd: F,
        offset: libc::off_t,
        buf: IoBuf,
    },
    Flush {
        fd: F,
    },
}

impl<F: AsRawFd> Work<F> {
    /// Performs the operation: one system call, or two for a durable write
    /// to a raw block device. Returns the buffer and the raw outcome; the
    /// descriptor is dropped before this returns.
    pub(crate) fn perform(self) -> (Option<IoBuf>, RawResult<usize>) {
        match self {
            Self::Write {
                fd,
                bdev,
                offset,
                buf,
                dsync,
            } => {
                let result =
                    sys::write_data(fd.as_raw_fd(), buf.as_ptr(), buf.len(), offset, dsync, bdev);
                drop(fd);
                (Some(buf), result)
            }
            Self::Read {
                fd,
                offset,
                mut buf,
            } => {
                let len = buf.len();
                let result = sys::read_data(fd.as_raw_fd(), buf.as_mut_ptr(), len, offset);
                drop(fd);
                (Some(buf), result)
            }
            Self::Flush { fd } => {
                let result = sys::flush_data(fd.as_raw_fd()).map(|()| 0);
                drop(fd);
                (None, result)
            }
        }
    }
}

fn refuse(buf: Option<IoBuf>, code: c_int) -> Rejected {
    Rejected {
        buf,
        raw: Some(sys::errno(code)),
    }
}

/// The checks every mode makes before any I/O: a write or flush needs a
/// writable handle, and the offset must fit `off_t`. `fd` picks how the
/// descriptor travels with the work.
fn check<F>(op: IoOp<'_, PosixFile>, fd: impl Fn(&PosixFile) -> F) -> Result<Work<F>, Rejected> {
    match op {
        IoOp::Write {
            file,
            offset,
            buf,
            dsync,
        } => {
            if !file.is_writable() {
                return Err(refuse(Some(buf), libc::EBADF));
            }
            let Ok(offset) = libc::off_t::try_from(offset) else {
                return Err(refuse(Some(buf), libc::EINVAL));
            };
            Ok(Work::Write {
                fd: fd(file),
                bdev: file.is_block_device(),
                offset,
                buf,
                dsync,
            })
        }
        IoOp::Read { file, offset, buf } => {
            let Ok(offset) = libc::off_t::try_from(offset) else {
                return Err(refuse(Some(buf), libc::EINVAL));
            };
            Ok(Work::Read {
                fd: fd(file),
                offset,
                buf,
            })
        }
        IoOp::FlushData { file } => {
            if !file.is_writable() {
                return Err(refuse(None, libc::EBADF));
            }
            Ok(Work::Flush { fd: fd(file) })
        }
    }
}

/// Ends a [`PosixQueue`]'s wait from another thread (D-083). An inline
/// queue never blocks in `wait`, so its waker has nothing to do.
#[derive(Clone)]
pub struct PosixWaker(Wakes);

#[derive(Clone)]
enum Wakes {
    Inline,
    Pooled(Arc<Inbox>),
    /// The eventfd an io_uring queue's wait watches.
    Ring(Arc<OwnedFd>),
}

impl core::fmt::Debug for PosixWaker {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let kind = match self.0 {
            Wakes::Inline => "inline",
            Wakes::Pooled(_) => "pooled",
            Wakes::Ring(_) => "ring",
        };
        f.debug_tuple("PosixWaker").field(&kind).finish()
    }
}

impl Wake for PosixWaker {
    fn wake(&self) {
        match &self.0 {
            Wakes::Inline => {}
            Wakes::Pooled(inbox) => inbox.wake(),
            Wakes::Ring(fd) => crate::uring::sys::eventfd_signal(fd),
        }
    }
}

impl Queue for PosixQueue {
    type File = PosixFile;
    type Waker = PosixWaker;

    fn submit(&mut self, op: IoOp<'_, PosixFile>, tag: u64) -> Result<(), Rejected> {
        if let Some(u) = &mut self.uring {
            return u.submit(op, tag);
        }
        if self.in_flight() >= self.depth {
            let buf = match op {
                IoOp::Write { buf, .. } | IoOp::Read { buf, .. } => Some(buf),
                IoOp::FlushData { .. } => None,
            };
            return Err(Rejected { buf, raw: None });
        }
        match &mut self.pooled {
            None => {
                let (buf, result) = check(op, PosixFile::raw)?.perform();
                // Capacity is `depth` and the length is below it, so this
                // never grows.
                self.ready.push(Completion { tag, buf, result });
            }
            Some(p) => {
                let work = check(op, PosixFile::share_fd)?;
                p.in_flight += 1;
                p.pool.push(Job {
                    work,
                    tag,
                    inbox: Arc::clone(&p.inbox),
                });
            }
        }
        Ok(())
    }

    fn reap(&mut self, out: &mut CompletionBuf) -> usize {
        if let Some(u) = &mut self.uring {
            return u.reap(out);
        }
        match &mut self.pooled {
            None => self.drain_ready(out),
            Some(p) => {
                let n = p.inbox.reap(out);
                p.in_flight -= n;
                n
            }
        }
    }

    fn wait(&mut self, min: usize, out: &mut CompletionBuf) -> RawResult<usize> {
        if let Some(u) = &mut self.uring {
            return u.wait(min, out);
        }
        match &mut self.pooled {
            // Everything completed inside `submit`; nothing to wait for.
            None => Ok(self.drain_ready(out)),
            Some(p) => {
                // Never wait for more than is in flight or fits in `out`:
                // the same rule as the asynchronous backends, so a wait
                // always ends.
                let target = min.min(p.in_flight).min(out.room());
                let n = if target == 0 {
                    // Returns at once: a pending wake is spent here.
                    p.inbox.reap_spending_wake(out)
                } else {
                    p.inbox.wait(target, out)
                };
                p.in_flight -= n;
                Ok(n)
            }
        }
    }

    /// The inline synchronous queue, holding no completion.
    fn performs(&self) -> bool {
        self.uring.is_none() && self.pooled.is_none() && self.ready.is_empty()
    }

    fn perform<'a>(
        &mut self,
        op: IoOp<'a, PosixFile>,
    ) -> Result<Result<(Option<IoBuf>, RawResult<usize>), IoOp<'a, PosixFile>>, Rejected> {
        if !self.performs() {
            return Ok(Err(op));
        }
        Ok(Ok(check(op, PosixFile::raw)?.perform()))
    }

    fn in_flight(&self) -> usize {
        self.ready.len()
            + self.pooled.as_ref().map_or(0, |p| p.in_flight)
            + self.uring.as_ref().map_or(0, UringQueue::in_flight)
    }

    fn waker(&self) -> PosixWaker {
        PosixWaker(match (&self.uring, &self.pooled) {
            (Some(u), _) => Wakes::Ring(u.waker()),
            (None, Some(p)) => Wakes::Pooled(Arc::clone(&p.inbox)),
            (None, None) => Wakes::Inline,
        })
    }

    fn workers(&self) -> Option<usize> {
        self.pooled.as_ref().map(|p| p.pool.threads())
    }
}

impl Drop for PosixQueue {
    /// A pooled queue dropped with operations in flight waits, untimed, for
    /// each completion: when the drop returns, none of its operations can
    /// still land on the device (the ownership lock may be released next),
    /// and every buffer is back in its pool.
    fn drop(&mut self) {
        if let Some(p) = &self.pooled {
            if p.in_flight > 0 {
                p.inbox.settle(p.in_flight);
            }
            p.pool.release(self.depth);
        }
    }
}
