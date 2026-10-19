//! The `Queue` on an io_uring ring (D-103 3b).
//!
//! Each read or write is one entry, submitted at once with `RWF_NOWAIT` (and
//! `RWF_DSYNC` when durability at completion is asked, on block devices
//! too: the asynchronous path makes that a FUA write, which is the durable
//! write the flag promises, without the synchronous path's extra cache
//! flush). Nothing ever runs on io-wq (D-073): when the kernel would block
//! (`EAGAIN`), cannot honour NOWAIT for the file (`EOPNOTSUPP`) or transfers
//! short, the rest is done by one blocking system call where the completion
//! is observed (in `reap` or `wait`), and counted. A `FlushData` is the
//! synchronous `fdatasync`, completed inside `submit`.
//!
//! The op table holds each operation's buffer from `submit` until its
//! completion is observed; `user_data` is the slot index and the slot's
//! generation. A completion the table does not expect, or one the kernel
//! could not keep, ends the queue: everything in flight fails with `EBADR`
//! and its buffer is never freed, since the kernel may still use it.
//!
//! The waker is an eventfd. Once a waker has been handed out, a blocking
//! `wait` arms a read of it on the ring, whose completion ends the wait. On
//! a shared ring the read's completion is delivered through the thread that
//! armed it, so that thread completes it (by signalling the eventfd itself)
//! before its wait returns: the next waiter, on any thread, arms its own.
//! Dropping the queue completes the read the same way and waits for every
//! operation, so no buffer is released while the kernel owns it; it never
//! starts a fallback call, since nothing will reap the result.

use std::cell::Cell;
use std::collections::VecDeque;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::ptr::NonNull;
use std::sync::Arc;

use store_io_platform::{Completion, CompletionBuf, IoBuf, IoOp, OsError, RawResult, Rejected};

use super::abi::{Cqe, SQE_FIXED_FILE, Sqe, op};
use super::ring::{Fixed, Flavour, Reap, Ring, Want};
use super::sys;
use crate::file::PosixFile;
use crate::sys::{errno, fdatasync, preadv2, pwritev2};

/// `user_data` of the armed eventfd read. Never a table tag: a tag's low
/// half is a slot index, below the depth.
const WAKE: u64 = u64::MAX;

/// Operations, process-wide, that a queue finished with a blocking call
/// after NOWAIT refused them or cut them short (D-073's counted fallbacks;
/// `crate::io_uring_fallbacks`).
pub(crate) static FALLBACKS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// The most operations a queue holds: with the wake read, within the
/// completion ring (twice the largest submission ring, 4096), so the kernel
/// never needs its overflow list.
const MAX_DEPTH: usize = 8191;

/// An operation the kernel holds.
#[derive(Debug)]
struct InKernel {
    tag: u64,
    /// `Some(dsync)` for a write.
    write: Option<bool>,
    /// The caller's descriptor (platform contract 2: open until reaped).
    fd: RawFd,
    offset: libc::off_t,
    buf: IoBuf,
}

#[derive(Debug, Default)]
struct Slot {
    generation: u32,
    op: Option<InKernel>,
}

/// A queue on one ring (see the module docs).
#[derive(Debug)]
pub(crate) struct UringQueue {
    ring: Ring,
    slots: Box<[Slot]>,
    free: Vec<u32>,
    /// Completions observed and not yet reaped. Never more than the depth.
    ready: VecDeque<Completion>,
    /// Operations in the kernel (the wake read is not one).
    kernel: usize,
    depth: usize,
    wake: Arc<OwnedFd>,
    /// The wake read's 8-byte target: a heap allocation freed only in
    /// `Drop`, after the read has completed.
    word: NonNull<u64>,
    /// The wake read is in the kernel.
    armed: bool,
    /// A waker has been handed out: blocking waits arm the wake read.
    wakeable: Cell<bool>,
    /// The queue is dropping: completions are only accounted, never
    /// finished by a fallback call.
    dropping: bool,
    /// The wake read completed: the current or next `wait` returns.
    woken: bool,
    /// The kernel lost a completion, or posted one the table did not
    /// expect: nothing more is accepted.
    lost: bool,
    /// Entering the kernel failed during `reap`: the next `wait` says so.
    failed: Option<OsError>,
    /// Operations NOWAIT refused or cut short, finished by a blocking call
    /// (reads that met the end of the file included).
    fallbacks: u64,
    /// The file the ring registered as direct descriptor 0 (kept open, so
    /// its identity is exact). An operation uses it only on that very file.
    fixed_file: Option<Arc<OwnedFd>>,
    /// Operations submitted with the direct descriptor.
    direct_ops: u64,
}

/// What a thread-owned ring may register (D-078, D-107: the file only).
pub(crate) struct Registered {
    pub(crate) file: Arc<OwnedFd>,
}

// SAFETY: `word` is owned by the queue alone (the kernel aside, which never
// cares which thread owns it); every other field is `Send`.
unsafe impl Send for UringQueue {}

impl UringQueue {
    /// A queue of `depth` operations on a new ring of `flavour`.
    pub(crate) fn new(flavour: Flavour, depth: usize) -> RawResult<Self> {
        Self::registered(flavour, depth, None)
    }

    /// As [`Self::new`], registering `reg`'s file on an owned ring (best
    /// effort: see [`Self::fixed`]).
    pub(crate) fn registered(
        flavour: Flavour,
        depth: usize,
        reg: Option<Registered>,
    ) -> RawResult<Self> {
        let depth = depth.clamp(1, MAX_DEPTH);
        // Room for every operation and the wake read, should the kernel not
        // take entries at once; at most 4096 (more in flight is fine).
        let entries = u32::try_from(depth + 1).unwrap_or(u32::MAX);
        let want = Want {
            file: reg.as_ref().map(|r| r.file.as_raw_fd()),
        };
        // The eventfd first: nothing after the ring can fail, so what the
        // ring registered is always kept as long as the ring lives.
        let wake = Arc::new(sys::eventfd()?);
        // The file is open (`reg.file`, kept by the queue).
        let ring = Ring::with_registration(flavour, entries, want)?;
        let fixed_file = reg.and_then(|r| ring.fixed().file.then_some(r.file));
        let word = NonNull::from(Box::leak(Box::new(0u64)));
        Ok(Self {
            ring,
            slots: (0..depth).map(|_| Slot::default()).collect(),
            free: (0..depth as u32).rev().collect(),
            ready: VecDeque::with_capacity(depth),
            kernel: 0,
            depth,
            wake,
            word,
            armed: false,
            wakeable: Cell::new(false),
            dropping: false,
            woken: false,
            lost: false,
            failed: None,
            fallbacks: 0,
            fixed_file,
            direct_ops: 0,
        })
    }

    /// What the ring registered (D-078), and how many operations used the
    /// direct descriptor.
    pub(crate) fn fixed(&self) -> (Fixed, u64) {
        (self.ring.fixed(), self.direct_ops)
    }

    /// Operations finished by a blocking call after NOWAIT refused them.
    pub(crate) fn fallbacks(&self) -> u64 {
        self.fallbacks
    }

    /// The eventfd [`UringQueue::wait`] watches.
    pub(crate) fn waker(&self) -> Arc<OwnedFd> {
        self.wakeable.set(true);
        Arc::clone(&self.wake)
    }

    pub(crate) fn in_flight(&self) -> usize {
        self.kernel + self.ready.len()
    }

    pub(crate) fn submit(&mut self, op: IoOp<'_, PosixFile>, tag: u64) -> Result<(), Rejected> {
        let refuse = |buf: Option<IoBuf>, code: Option<libc::c_int>| Rejected {
            buf,
            raw: code.map(errno),
        };
        let (file, offset, buf, write) = match op {
            IoOp::Write {
                file,
                offset,
                buf,
                dsync,
            } => (file, offset, buf, Some(dsync)),
            IoOp::Read { file, offset, buf } => (file, offset, buf, None),
            IoOp::FlushData { file } => {
                if self.lost {
                    return Err(refuse(None, Some(libc::EBADR)));
                }
                if let Err(e) = self.ring.check_thread() {
                    return Err(Rejected {
                        buf: None,
                        raw: Some(e),
                    });
                }
                if self.in_flight() >= self.depth {
                    return Err(refuse(None, None));
                }
                if !file.is_writable() {
                    return Err(refuse(None, Some(libc::EBADF)));
                }
                let result = fdatasync(file.raw()).map(|()| 0);
                self.ready.push_back(Completion {
                    tag,
                    buf: None,
                    result,
                });
                return Ok(());
            }
        };
        if self.lost {
            return Err(refuse(Some(buf), Some(libc::EBADR)));
        }
        if let Err(e) = self.ring.check_thread() {
            return Err(Rejected {
                buf: Some(buf),
                raw: Some(e),
            });
        }
        if write.is_some() && !file.is_writable() {
            return Err(refuse(Some(buf), Some(libc::EBADF)));
        }
        let (Ok(offset), Ok(len)) = (libc::off_t::try_from(offset), u32::try_from(buf.len()))
        else {
            return Err(refuse(Some(buf), Some(libc::EINVAL)));
        };
        if self.in_flight() >= self.depth || self.ring.space() == 0 {
            return Err(refuse(Some(buf), None));
        }
        let Some(idx) = self.free.pop() else {
            return Err(refuse(Some(buf), None));
        };
        let slot = &mut self.slots[idx as usize];
        slot.generation = slot.generation.wrapping_add(1);
        let user_data = u64::from(idx) | (u64::from(slot.generation) << 32);
        let op = slot.op.insert(InKernel {
            tag,
            write,
            fd: file.raw(),
            offset,
            buf,
        });
        // The direct descriptor only for the registered file itself (D-078).
        // Never a fixed buffer (D-107).
        let fixed_file = self.fixed_file.as_ref().is_some_and(|f| file.same_fd(f));
        let (opcode, addr) = match write {
            Some(_) => (op::WRITE, op.buf.as_ptr() as u64),
            None => (op::READ, op.buf.as_mut_ptr() as u64),
        };
        let sqe = Sqe {
            opcode,
            flags: if fixed_file { SQE_FIXED_FILE } else { 0 },
            fd: if fixed_file { 0 } else { op.fd },
            off: offset as u64,
            addr,
            len,
            rw_flags: rw_flags(write, true),
            user_data,
            ..Sqe::default()
        };
        self.direct_ops += u64::from(fixed_file);
        // SAFETY: the buffer lives in the slot, untouched, until this
        // entry's completion is observed (`observe`), or forever if it never
        // is (`lose`, `Drop`); the queue drains the ring before it drops it.
        if !unsafe { self.ring.push(&sqe) } {
            // Not reached (space was checked); never leave an operation the
            // kernel cannot see.
            let buf = self.slots[idx as usize].op.take().map(|o| o.buf);
            self.free.push(idx);
            return Err(refuse(buf, None));
        }
        self.kernel += 1;
        // Taken now, or at the next entry into the kernel (a transient
        // refusal keeps it queued); either way it may reach the device, so
        // `submit` succeeds. A lasting failure is reported by `wait`.
        if let Err(e) = self.ring.submit() {
            if !transient(e) {
                self.failed = Some(e);
            }
        }
        Ok(())
    }

    pub(crate) fn reap(&mut self, out: &mut CompletionBuf) -> usize {
        // Entries a transient refusal left queued go in now: a caller that
        // only polls still makes progress.
        if self.ring.queued() > 0 {
            if let Err(e) = self.ring.submit() {
                if !transient(e) {
                    self.failed = Some(e);
                }
            }
        }
        let mut added = self.drain(out);
        if (self.kernel > 0 || self.armed) && out.room() > 0 {
            self.collect();
            added += self.drain(out);
        }
        added
    }

    pub(crate) fn wait(&mut self, min: usize, out: &mut CompletionBuf) -> RawResult<usize> {
        let mut added = self.drain(out);
        loop {
            if self.kernel > 0 || self.armed {
                self.collect();
                added += self.drain(out);
            }
            if let Some(e) = self.failed.take() {
                // Completions already moved are returned first.
                if added > 0 {
                    self.failed = Some(e);
                    break;
                }
                self.disarm_shared();
                // A wake is spent on the failed wait too.
                self.woken = false;
                return Err(e);
            }
            if self.woken || added >= min || out.room() == 0 || self.kernel == 0 {
                break;
            }
            self.arm();
            match self.ring.submit_and_wait(1) {
                Ok(_) => {}
                Err(e) if transient(e) => {}
                Err(e) => self.failed = Some(e),
            }
        }
        self.disarm_shared();
        // A wake that arrived before or during this wait is spent on it.
        self.woken = false;
        Ok(added)
    }

    /// On a shared ring with a waker, completes an armed wake read before
    /// the wait returns: its completion is delivered through this thread,
    /// which may not be the next to wait. Signalling the eventfd completes
    /// it; then the counter is emptied, so neither this signal nor a wake
    /// sent before this wait returns (spent on it) can end a later wait.
    /// If entering the kernel fails here, the read stays armed by this
    /// thread: a later waiter elsewhere then depends on this thread running
    /// its completion (the kernel interrupts it to; its exit cancels it).
    fn disarm_shared(&mut self) {
        if self.ring.flavour() != Flavour::Shared || !self.wakeable.get() {
            return;
        }
        if self.armed {
            // A wake may have completed the read since the last look.
            self.collect();
        }
        if self.armed {
            sys::eventfd_signal(&self.wake);
        }
        while self.armed && !self.lost {
            match self.ring.submit_and_wait(1) {
                Ok(_) => {}
                Err(e) if transient(e) => {}
                Err(e) => {
                    self.failed = Some(e);
                    return;
                }
            }
            self.collect();
        }
        sys::eventfd_drain(&self.wake);
    }

    fn drain(&mut self, out: &mut CompletionBuf) -> usize {
        let n = out.room().min(self.ready.len());
        let mut added = 0;
        for c in self.ready.drain(..n) {
            // `n` is at most the room, so no push is refused.
            if out.push(c).is_ok() {
                added += 1;
            }
        }
        added
    }

    /// Arms the wake read, if it is not in the kernel already.
    fn arm(&mut self) {
        if self.armed || self.lost || !self.wakeable.get() {
            return;
        }
        let sqe = Sqe {
            opcode: op::READ,
            fd: self.wake.as_raw_fd(),
            // The eventfd has no position: -1 reads it as a stream.
            off: u64::MAX,
            addr: self.word.as_ptr() as u64,
            len: 8,
            user_data: WAKE,
            ..Sqe::default()
        };
        // SAFETY: `word` is freed only in `Drop`, after this read completed.
        self.armed = unsafe { self.ring.push(&sqe) };
    }

    /// Moves every completion the kernel posted into `ready`.
    fn collect(&mut self) {
        let before = self.fallbacks;
        self.collect_inner();
        if self.fallbacks != before {
            let _n = FALLBACKS.fetch_add(
                self.fallbacks - before,
                core::sync::atomic::Ordering::Relaxed,
            );
        }
    }

    fn collect_inner(&mut self) {
        let Self {
            ring,
            slots,
            free,
            ready,
            kernel,
            armed,
            woken,
            fallbacks,
            dropping,
            ..
        } = self;
        let mut unexpected = false;
        let mut fallbacks = (!*dropping).then_some(fallbacks);
        let r = ring.reap(|cqe| match cqe.user_data {
            WAKE => {
                *armed = false;
                // Only a read of the counter is a wake (not a cancellation,
                // say at the arming thread's exit).
                *woken |= cqe.res == 8;
            }
            ud => match observe(slots, ud, cqe, fallbacks.as_deref_mut()) {
                Some((idx, c)) => {
                    free.push(idx);
                    *kernel -= 1;
                    ready.push_back(c);
                }
                None => unexpected = true,
            },
        });
        match r {
            Ok(_) if !unexpected => {}
            Ok(_) | Err(Reap::Lost) => self.lose(),
            Err(Reap::Os(e)) => self.failed = Some(e),
        }
    }

    /// The kernel can no longer be trusted to report this queue's
    /// operations: each fails with `EBADR` and keeps its buffer from ever
    /// being freed (the kernel may still use it).
    fn lose(&mut self) {
        self.lost = true;
        for slot in self.slots.iter_mut() {
            if let Some(op) = slot.op.take() {
                core::mem::forget(op.buf);
                self.ready.push_back(Completion {
                    tag: op.tag,
                    buf: None,
                    result: Err(errno(libc::EBADR)),
                });
            }
        }
        self.kernel = 0;
    }
}

/// Whether a refusal to enter the kernel passes by itself (a completion
/// backlog, or no memory for the submission path just now).
fn transient(e: OsError) -> bool {
    e.code == libc::EBUSY || e.code == libc::EAGAIN
}

/// The completion of the table's operation `ud`, and its freed slot; `None`
/// if the table holds no such operation.
fn observe(
    slots: &mut [Slot],
    ud: u64,
    cqe: Cqe,
    fallbacks: Option<&mut u64>,
) -> Option<(u32, Completion)> {
    let idx = (ud & u64::from(u32::MAX)) as u32;
    let slot = slots.get_mut(idx as usize)?;
    if slot.generation != (ud >> 32) as u32 {
        return None;
    }
    let mut op = slot.op.take()?;
    let result = finish(&mut op, cqe.res, fallbacks);
    Some((
        idx,
        Completion {
            tag: op.tag,
            buf: Some(op.buf),
            result,
        },
    ))
}

/// The `RWF_*` flags of a read (`write` is `None`) or write: durability at
/// completion when asked, and NOWAIT on the ring (never on a fallback).
fn rw_flags(write: Option<bool>, nowait: bool) -> libc::c_int {
    let dsync = if write == Some(true) {
        libc::RWF_DSYNC
    } else {
        0
    };
    dsync | if nowait { libc::RWF_NOWAIT } else { 0 }
}

/// The outcome of an operation whose entry completed with `res`. What NOWAIT
/// refused (`EAGAIN`, or `EOPNOTSUPP` from a file that cannot honour it) or
/// left undone (a short transfer) is finished with one blocking call at the
/// right offset, counted in `fallbacks`; with no counter (a dropping queue)
/// nothing is started and the partial outcome stands. A read that reached
/// the end of the file (0 bytes) is complete.
fn finish(op: &mut InKernel, res: i32, fallbacks: Option<&mut u64>) -> RawResult<usize> {
    let len = op.buf.len();
    let done = match res {
        r if r == -libc::EAGAIN || r == -libc::EOPNOTSUPP => 0,
        r if r < 0 => return Err(errno(-r)),
        r => r as usize,
    };
    if done >= len || (op.write.is_none() && res == 0) {
        return Ok(done);
    }
    let Some(fallbacks) = fallbacks else {
        return if res < 0 { Err(errno(-res)) } else { Ok(done) };
    };
    *fallbacks += 1;
    let offset = op.offset + done as libc::off_t;
    let flags = rw_flags(op.write, false);
    let rest = match op.write {
        Some(_) => pwritev2(
            op.fd,
            op.buf.as_ptr().wrapping_add(done),
            len - done,
            offset,
            flags,
        ),
        None => preadv2(
            op.fd,
            op.buf.as_mut_ptr().wrapping_add(done),
            len - done,
            offset,
            flags,
        ),
    };
    rest.map(|n| done + n)
}

impl Drop for UringQueue {
    /// Waits, untimed, for every operation in the kernel, after completing
    /// the wake read: when the drop returns, the kernel holds no buffer of
    /// this queue. If the kernel can no longer be entered or trusted, the
    /// buffers and the wake word are leaked instead of freed.
    fn drop(&mut self) {
        self.dropping = true;
        if self.armed {
            sys::eventfd_signal(&self.wake);
        }
        while !self.lost && (self.kernel > 0 || self.armed) {
            match self.ring.submit_and_wait(1) {
                Ok(_) => {}
                Err(e) if transient(e) => {}
                Err(_) => break,
            }
            self.collect();
        }
        if self.lost || self.kernel > 0 || self.armed {
            for slot in self.slots.iter_mut() {
                if let Some(op) = slot.op.take() {
                    core::mem::forget(op.buf);
                }
            }
            return;
        }
        // SAFETY: `word` came from `Box::leak` in `new` and the read that
        // targeted it has completed; nothing else refers to it.
        drop(unsafe { Box::from_raw(self.word.as_ptr()) });
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use store_io_buf::{BufPool, PoolConfig};

    use super::*;

    /// A queue holding one operation that never reached the kernel, and the
    /// one-buffer pool its buffer came from.
    fn with_one_op(flavour: Flavour) -> (UringQueue, BufPool) {
        let pool = BufPool::new(&PoolConfig::uniform(4096, 4096, 4096, 1)).unwrap();
        let mut q = UringQueue::new(flavour, 4).unwrap();
        let idx = q.free.pop().unwrap();
        q.slots[idx as usize].op = Some(InKernel {
            tag: 9,
            write: Some(true),
            fd: -1,
            offset: 0,
            buf: pool.take(4096).unwrap(),
        });
        q.kernel = 1;
        (q, pool)
    }

    /// A completion the table does not expect (here a NOP with a slot's
    /// index and a stale generation) ends the queue: the operation in
    /// flight fails with `EBADR`, its buffer is never freed (the kernel
    /// might still use it), and nothing more is accepted.
    #[test]
    fn test_an_unexpected_completion_ends_the_queue() {
        if crate::uring::skip() {
            return;
        }
        use store_io_platform::{FileName, Platform};
        let path = std::path::PathBuf::from(std::env::var_os("HOME").unwrap())
            .join(format!(".store-io-uring-unit-{}", std::process::id()));
        let p = crate::PosixPlatform::new();
        let dir = p.open_dir(&path, true).unwrap();
        let file = p.create_file(&dir, &FileName::new("f").unwrap()).unwrap();
        let spare = BufPool::new(&PoolConfig::uniform(4096, 4096, 4096, 2)).unwrap();
        for flavour in [Flavour::Shared, Flavour::Owned] {
            let (mut q, pool) = with_one_op(flavour);
            let stray = Sqe {
                opcode: op::NOP,
                user_data: u64::from(q.slots[0].generation.wrapping_add(7)) << 32,
                ..Sqe::default()
            };
            // SAFETY: a NOP names no buffer.
            assert!(unsafe { q.ring.push(&stray) });
            let _taken = q.ring.submit().unwrap();
            let mut out = CompletionBuf::with_capacity(4);
            let mut got = 0;
            for _ in 0..1_000_000 {
                got += q.reap(&mut out);
                if got > 0 {
                    break;
                }
                std::thread::yield_now();
            }
            let c = out.drain().next().unwrap();
            assert_eq!(c.tag, 9);
            assert_eq!(c.result.map_err(|e| e.code), Err(libc::EBADR));
            assert!(c.buf.is_none(), "{flavour:?}");
            assert!(pool.take(4096).is_err(), "the buffer must never return");
            assert_eq!(q.in_flight(), 0);
            assert_eq!(q.wait(1, &mut out).unwrap(), 0);
            let r = q.submit(
                IoOp::Read {
                    file: &file,
                    offset: 0,
                    buf: spare.take(4096).unwrap(),
                },
                10,
            );
            let rejected = r.unwrap_err();
            assert_eq!(rejected.raw.map(|e| e.code), Some(libc::EBADR));
            assert!(rejected.buf.is_some());
        }
        drop(file);
        let _cleanup = std::fs::remove_dir_all(&path);
    }

    /// Only the table's own generation completes a slot.
    #[test]
    fn test_observe_matches_index_and_generation() {
        if crate::uring::skip() {
            return;
        }
        let (mut q, _pool) = with_one_op(Flavour::Shared);
        let idx = q.slots.iter().position(|s| s.op.is_some()).unwrap() as u64;
        let generation = u64::from(q.slots[idx as usize].generation);
        let cqe = |ud| Cqe {
            user_data: ud,
            res: 4096,
            flags: 0,
        };
        let mut n = 0;
        let stale = idx | ((generation + 1) << 32);
        assert!(observe(&mut q.slots, stale, cqe(stale), Some(&mut n)).is_none());
        let other = (idx + 1) | (generation << 32);
        assert!(observe(&mut q.slots, other, cqe(other), Some(&mut n)).is_none());
        let right = idx | (generation << 32);
        let (i, c) = observe(&mut q.slots, right, cqe(right), Some(&mut n)).unwrap();
        assert_eq!((u64::from(i), c.tag, c.result), (idx, 9, Ok(4096)));
        q.kernel = 0;
    }

    /// The flags of each kind of operation, on the ring and on a fallback.
    #[test]
    fn test_rw_flags() {
        let (d, n) = (libc::RWF_DSYNC, libc::RWF_NOWAIT);
        assert_eq!(rw_flags(Some(true), true), d | n);
        assert_eq!(rw_flags(Some(true), false), d);
        assert_eq!(rw_flags(Some(false), true), n);
        assert_eq!(rw_flags(Some(false), false), 0);
        assert_eq!(rw_flags(None, true), n);
        assert_eq!(rw_flags(None, false), 0);
    }

    /// A buffered scratch file under `$HOME` (fallbacks need no direct I/O).
    fn scratch(name: &str, len: usize) -> (std::fs::File, std::path::PathBuf) {
        let path = std::path::PathBuf::from(std::env::var_os("HOME").unwrap())
            .join(format!(".store-io-uring-{name}-{}", std::process::id()));
        let content: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &content).unwrap();
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        (f, path)
    }

    /// A short transfer is finished at the right offset from the right part
    /// of the buffer; `EAGAIN` and `EOPNOTSUPP` redo the whole operation; a
    /// dropping queue (no counter) starts nothing.
    #[test]
    fn test_finish_completes_short_and_refused_transfers() {
        use std::os::fd::AsRawFd;
        let pool = BufPool::new(&PoolConfig::uniform(4096, 4096, 4096, 8)).unwrap();
        let (file, path) = scratch("finish", 16384);
        let op = |write, offset, byte: u8| {
            let mut buf = pool.take(4096).unwrap();
            buf.as_mut_slice()
                .iter_mut()
                .enumerate()
                .for_each(|(i, b)| *b = byte.wrapping_add(i as u8));
            InKernel {
                tag: 1,
                write,
                fd: file.as_raw_fd(),
                offset,
                buf,
            }
        };
        let mut n = 0;
        // A write cut short at 1000 bytes: the rest lands at 8192 + 1000.
        let mut w = op(Some(true), 8192, 0x40);
        assert_eq!(finish(&mut w, 1000, Some(&mut n)), Ok(4096));
        let on_disk = std::fs::read(&path).unwrap();
        assert_eq!(&on_disk[9192..12288], &w.buf.as_slice()[1000..]);
        let untouched: Vec<u8> = (8192..9192).map(|i| (i % 251) as u8).collect();
        assert_eq!(&on_disk[8192..9192], &untouched[..]);
        assert_eq!(n, 1);
        // A read cut short: the rest of the buffer comes from the file.
        drop(w);
        let mut r = op(None, 0, 0);
        assert_eq!(finish(&mut r, 1000, Some(&mut n)), Ok(4096));
        let want: Vec<u8> = (1000..4096).map(|i| (i % 251) as u8).collect();
        assert_eq!(&r.buf.as_slice()[1000..], &want[..]);
        assert_eq!(r.buf.as_slice()[0], 0);
        // Refused outright: the whole read is redone, from its offset.
        for code in [libc::EAGAIN, libc::EOPNOTSUPP] {
            let mut r = op(None, 4096, 0);
            assert_eq!(finish(&mut r, -code, Some(&mut n)), Ok(4096));
            let want: Vec<u8> = (4096..8192).map(|i| (i % 251) as u8).collect();
            assert_eq!(r.buf.as_slice(), &want[..]);
        }
        assert_eq!(n, 4);
        // Dropping: nothing is started, the partial outcome stands.
        let mut r = op(None, 0, 9);
        assert_eq!(finish(&mut r, 1000, None), Ok(1000));
        assert_eq!(r.buf.as_slice()[2000], 9u8.wrapping_add(2000u32 as u8));
        let mut r2 = op(None, 0, 9);
        drop(r);
        assert_eq!(
            finish(&mut r2, -libc::EAGAIN, None).map_err(|e| e.code),
            Err(libc::EAGAIN)
        );
        // Any other error is the outcome.
        assert_eq!(
            finish(&mut r2, -libc::EIO, Some(&mut n)).map_err(|e| e.code),
            Err(libc::EIO)
        );
        assert_eq!(n, 4);
        drop(file);
        let _cleanup = std::fs::remove_file(&path);
    }

    /// A queue dropped with its wake read armed completes the read before
    /// it frees the read's target; without that the drop waits for ever (a
    /// timeout fails the test instead of hanging it).
    #[test]
    fn test_a_drop_completes_an_armed_wake_read() {
        if crate::uring::skip() {
            return;
        }
        for flavour in [Flavour::Owned, Flavour::Shared] {
            let (tx, rx) = std::sync::mpsc::channel();
            let _detached = std::thread::spawn(move || {
                let mut q = UringQueue::new(flavour, 4).unwrap();
                let _w = q.waker();
                q.arm();
                let _taken = q.ring.submit().unwrap();
                q.collect();
                assert!(q.armed, "{flavour:?}: the read is waiting in the kernel");
                drop(q);
                tx.send(()).unwrap();
            });
            rx.recv_timeout(std::time::Duration::from_secs(10))
                .unwrap_or_else(|_| panic!("{flavour:?}: the drop did not return"));
        }
    }
}
