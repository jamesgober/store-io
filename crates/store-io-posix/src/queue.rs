//! The synchronous queue (tier T3): each operation is one system call made
//! inside `submit`, and its completion is pushed into a fixed ready ring.

use core::ffi::c_int;

use store_io_platform::{
    Completion, CompletionBuf, IoBuf, IoOp, Queue, QueueConfig, RawResult, Rejected,
};

use crate::file::PosixFile;
use crate::sys;

/// A per-thread synchronous queue.
///
/// `submit` performs the system call inline (`pwritev2`, `preadv2` or
/// `fdatasync`) and stores the completion in a ring allocated once, sized by
/// [`QueueConfig::depth`]. When the ring is full, `submit` refuses the
/// operation and returns its buffer. `reap` drains the ring; `wait` does the
/// same, since nothing ever completes later.
///
/// A write asked for durability at completion uses `RWF_DSYNC`, except on a
/// raw block device, where the synchronous `RWF_DSYNC` path issues both a
/// FUA write and a cache flush: there the write is plain and one `fdatasync`
/// follows it. Short transfers are returned as `Ok(n)` with `n` below the
/// buffer length and never retried; `EINTR` from a data call is returned as
/// the raw result, since a direct I/O on a regular file is not interrupted
/// before the device sees it in practice.
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
}

impl PosixQueue {
    /// Creates a queue whose ready ring holds `cfg.depth` completions.
    #[must_use]
    pub fn new(cfg: QueueConfig) -> Self {
        let depth = usize::try_from(cfg.depth).unwrap_or(usize::MAX).max(1);
        Self {
            ready: Vec::with_capacity(depth),
            depth,
        }
    }

    /// Ring capacity.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.depth
    }

    fn drain_into(&mut self, out: &mut CompletionBuf) -> usize {
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

fn write_inline(
    file: &PosixFile,
    offset: libc::off_t,
    buf: &IoBuf,
    dsync: bool,
) -> RawResult<usize> {
    let bdev = file.is_block_device();
    let flags: c_int = if dsync && !bdev { libc::RWF_DSYNC } else { 0 };
    let n = sys::pwritev2(file.raw(), buf.as_ptr(), buf.len(), offset, flags)?;
    if dsync && bdev && n == buf.len() {
        sys::fdatasync(file.raw())?;
    }
    Ok(n)
}

fn read_inline(file: &PosixFile, offset: libc::off_t, buf: &mut IoBuf) -> RawResult<usize> {
    let len = buf.len();
    sys::preadv2(file.raw(), buf.as_mut_ptr(), len, offset, 0)
}

impl Queue for PosixQueue {
    type File = PosixFile;

    fn submit(&mut self, op: IoOp<'_, PosixFile>, tag: u64) -> Result<(), Rejected> {
        if self.ready.len() >= self.depth {
            let buf = match op {
                IoOp::Write { buf, .. } | IoOp::Read { buf, .. } => Some(buf),
                IoOp::FlushData { .. } => None,
            };
            return Err(Rejected { buf, raw: None });
        }
        let completion = match op {
            IoOp::Write {
                file,
                offset,
                buf,
                dsync,
            } => {
                if !file.is_writable() {
                    return Err(Rejected {
                        buf: Some(buf),
                        raw: Some(sys::errno(libc::EBADF)),
                    });
                }
                let Ok(off) = libc::off_t::try_from(offset) else {
                    return Err(Rejected {
                        buf: Some(buf),
                        raw: Some(sys::errno(libc::EINVAL)),
                    });
                };
                let result = write_inline(file, off, &buf, dsync);
                Completion {
                    tag,
                    buf: Some(buf),
                    result,
                }
            }
            IoOp::Read {
                file,
                offset,
                mut buf,
            } => {
                let Ok(off) = libc::off_t::try_from(offset) else {
                    return Err(Rejected {
                        buf: Some(buf),
                        raw: Some(sys::errno(libc::EINVAL)),
                    });
                };
                let result = read_inline(file, off, &mut buf);
                Completion {
                    tag,
                    buf: Some(buf),
                    result,
                }
            }
            IoOp::FlushData { file } => {
                if !file.is_writable() {
                    return Err(Rejected {
                        buf: None,
                        raw: Some(sys::errno(libc::EBADF)),
                    });
                }
                let result = sys::fdatasync(file.raw()).map(|()| 0);
                Completion {
                    tag,
                    buf: None,
                    result,
                }
            }
        };
        // Capacity is `depth` and the length is below it, so this never grows.
        self.ready.push(completion);
        Ok(())
    }

    fn reap(&mut self, out: &mut CompletionBuf) -> usize {
        self.drain_into(out)
    }

    fn wait(&mut self, _min: usize, out: &mut CompletionBuf) -> RawResult<usize> {
        // Everything completed inside `submit`; there is nothing to wait for.
        Ok(self.drain_into(out))
    }

    fn in_flight(&self) -> usize {
        self.ready.len()
    }
}
