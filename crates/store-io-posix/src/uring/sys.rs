//! The io_uring kernel interface (D-093): the three system calls, the ring
//! mappings and the shared-memory index protocol. The only `unsafe` of the
//! io_uring module.
//!
//! The submission ring has one producer (the ring's user, one thread at a
//! time) and one consumer (the kernel); the completion ring the reverse. A
//! producer publishes with a release store of its tail after writing the
//! entries; a consumer reads the other side's tail with an acquire load
//! before reading entries, and releases them with a release store of its
//! head. Functions whose misuse could make the kernel or this process touch
//! memory wrongly are `unsafe fn`, each with its contract.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, Ordering};

use libc::{c_int, c_long, c_uint, c_void};
use store_io_platform::{OsError, RawResult};

use super::abi::{
    Cqe, FEAT_SINGLE_MMAP, OFF_CQ_RING, OFF_SQ_RING, OFF_SQES, OP_SUPPORTED, Params, Probe,
    ProbeOp, Restriction, RsrcUpdate, Sqe, reg, setup,
};
use crate::sys::{errno, last_errno};

fn einval() -> OsError {
    errno(libc::EINVAL)
}

// ----- system calls -------------------------------------------------------------

/// `io_uring_setup(2)`: a new ring for `entries` submissions; the kernel
/// fills `p` with the actual sizes and the ring offsets.
pub(crate) fn setup(entries: u32, p: &mut Params) -> RawResult<OwnedFd> {
    // SAFETY: io_uring_setup(2) reads and writes one `io_uring_params`,
    // which `p` is (`repr(C)`, size checked in `abi`), exclusively borrowed.
    let fd = unsafe { libc::syscall(libc::SYS_io_uring_setup, entries as c_long, &raw mut *p) };
    let fd = c_int::try_from(fd).map_err(|_| last_errno())?;
    if fd < 0 {
        return Err(last_errno());
    }
    // SAFETY: the kernel returned a new descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// `io_uring_enter(2)` without an argument structure: submits `to_submit`
/// entries and, with `ENTER_GETEVENTS`, waits for `min_complete`
/// completions. `fd` is the ring descriptor, or its registered index with
/// `ENTER_REGISTERED_RING`. Returns the entries consumed.
pub(crate) fn enter(fd: c_int, to_submit: u32, min_complete: u32, flags: u32) -> RawResult<u32> {
    // SAFETY: io_uring_enter(2) with no signal mask (null, size 0): the
    // kernel reads only the ring's shared memory, which stays mapped.
    let n = unsafe {
        libc::syscall(
            libc::SYS_io_uring_enter,
            fd as c_long,
            to_submit as c_long,
            min_complete as c_long,
            flags as c_long,
            std::ptr::null::<c_void>(),
            0 as c_long,
        )
    };
    u32::try_from(n).map_err(|_| last_errno())
}

/// `io_uring_register(2)`.
///
/// # Safety
///
/// `arg` must point to what `opcode` takes (null if nothing), `nr` of them
/// as the opcode counts, valid for reads (and writes where the kernel
/// writes back) for the call.
unsafe fn register(fd: &OwnedFd, opcode: c_uint, arg: *mut c_void, nr: u32) -> RawResult<i32> {
    // SAFETY: the caller's contract above.
    let r = unsafe {
        libc::syscall(
            libc::SYS_io_uring_register,
            fd.as_raw_fd() as c_long,
            opcode as c_long,
            arg,
            nr as c_long,
        )
    };
    i32::try_from(r)
        .ok()
        .filter(|v| *v >= 0)
        .ok_or_else(last_errno)
}

/// The kernel's opcode support (`REGISTER_PROBE`): `supported[op]`.
pub(crate) fn probe(fd: &OwnedFd) -> RawResult<[bool; 256]> {
    let mut p = Probe {
        last_op: 0,
        ops_len: 0,
        resv: 0,
        resv2: [0; 3],
        ops: [ProbeOp::default(); 256],
    };
    // SAFETY: a probe with room for 256 ops, `nr` = 256.
    let _ok = unsafe { register(fd, reg::PROBE, (&raw mut p).cast(), 256)? };
    let mut supported = [false; 256];
    for o in p.ops.iter().take(usize::from(p.ops_len)) {
        supported[usize::from(o.op)] = o.flags & OP_SUPPORTED != 0;
    }
    Ok(supported)
}

/// Registers the ring's restrictions (once, before it is enabled).
pub(crate) fn restrict(fd: &OwnedFd, list: &mut [Restriction]) -> RawResult<()> {
    let nr = u32::try_from(list.len()).map_err(|_| einval())?;
    // SAFETY: an array of `nr` restrictions.
    unsafe { register(fd, reg::RESTRICTIONS, list.as_mut_ptr().cast(), nr) }.map(|_| ())
}

/// Registers `fd` in the calling thread's ring table (`REGISTER_RING_FDS`);
/// returns its index. Valid only on this thread.
pub(crate) fn register_ring_fd(fd: &OwnedFd) -> RawResult<u32> {
    let mut u = RsrcUpdate {
        offset: u32::MAX,
        data: fd.as_raw_fd() as u64,
        ..RsrcUpdate::default()
    };
    // SAFETY: one update entry; the kernel writes the chosen index back.
    let n = unsafe { register(fd, reg::RING_FDS, (&raw mut u).cast(), 1)? };
    if n == 1 { Ok(u.offset) } else { Err(einval()) }
}

/// Removes index `index` from the calling thread's ring table.
pub(crate) fn unregister_ring_fd(fd: &OwnedFd, index: u32) -> RawResult<()> {
    let mut u = RsrcUpdate {
        offset: index,
        ..RsrcUpdate::default()
    };
    // SAFETY: one update entry naming the index to remove.
    unsafe { register(fd, reg::UNREGISTER_RING_FDS, (&raw mut u).cast(), 1) }.map(|_| ())
}

/// Registers `file` as the ring's direct descriptor 0 (`REGISTER_FILES`, a
/// table of one). The kernel holds its own reference to the open file.
pub(crate) fn register_file(fd: &OwnedFd, file: RawFd) -> RawResult<()> {
    let mut f: i32 = file;
    // SAFETY: an array of one descriptor, valid for the call.
    unsafe { register(fd, reg::FILES, (&raw mut f).cast(), 1) }.map(|_| ())
}

/// Enables a ring created with `R_DISABLED` (on the calling thread: an
/// owned ring's only submitter).
pub(crate) fn enable(fd: &OwnedFd) -> RawResult<()> {
    // SAFETY: the opcode takes no argument.
    unsafe { register(fd, reg::ENABLE_RINGS, std::ptr::null_mut(), 0) }.map(|_| ())
}

/// A non-blocking `eventfd(2)`, counter at 0.
pub(crate) fn eventfd() -> RawResult<OwnedFd> {
    // SAFETY: eventfd(2) takes an initial value and flags.
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if fd < 0 {
        return Err(last_errno());
    }
    // SAFETY: a new descriptor nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Adds 1 to an eventfd's counter (wakes a read armed on it). A full
/// counter (`EAGAIN`) already wakes any reader, so it is not an error.
pub(crate) fn eventfd_signal(fd: &OwnedFd) {
    let one = 1u64;
    // SAFETY: write(2) of the 8 bytes of `one`, as an eventfd requires.
    let _n = unsafe { libc::write(fd.as_raw_fd(), (&raw const one).cast(), 8) };
}

/// Empties a (non-blocking) eventfd's counter; an empty one (`EAGAIN`) is
/// already empty.
pub(crate) fn eventfd_drain(fd: &OwnedFd) {
    let mut value = 0u64;
    // SAFETY: read(2) of 8 bytes into `value`, as an eventfd requires; the
    // descriptor is non-blocking, so the call never waits.
    let _n = unsafe { libc::read(fd.as_raw_fd(), (&raw mut value).cast(), 8) };
}

// ----- the shared rings ---------------------------------------------------------

/// One `mmap` of a ring region, unmapped on drop (in tests, heap memory
/// standing in for it).
#[derive(Debug)]
struct Map {
    ptr: NonNull<u8>,
    len: usize,
    /// A test ring's heap memory, a raw allocation (no move can invalidate
    /// the pointers formed from it).
    #[cfg(test)]
    heap: Option<NonNull<[u64]>>,
}

impl Map {
    fn new(fd: &OwnedFd, offset: libc::off_t, len: usize) -> RawResult<Self> {
        // SAFETY: a shared read-write mapping of `len` bytes of the ring at
        // the region offset the kernel defines; the kernel validates both.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_POPULATE,
                fd.as_raw_fd(),
                offset,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(last_errno());
        }
        // A forked child must not share the rings (its copy would race this
        // process's indices).
        // SAFETY: madvise on the mapping just created, of its exact length.
        let _ignored = unsafe { libc::madvise(p, len, libc::MADV_DONTFORK) };
        let ptr = NonNull::new(p.cast::<u8>()).ok_or_else(einval)?;
        Ok(Self {
            ptr,
            len,
            #[cfg(test)]
            heap: None,
        })
    }

    fn at(&self, offset: u32) -> *mut u8 {
        // Callers check the offset against the length (`field`).
        self.ptr.as_ptr().wrapping_add(offset as usize)
    }
}

impl Drop for Map {
    fn drop(&mut self) {
        #[cfg(test)]
        if let Some(raw) = self.heap {
            // SAFETY: `raw` came from `Box::into_raw` (the test helper) and is
            // freed exactly once, after every view of it is gone.
            drop(unsafe { Box::from_raw(raw.as_ptr()) });
            return;
        }
        // SAFETY: unmaps exactly the region `new` mapped; nothing refers to
        // it once its `Rings` is dropped.
        let _ignored = unsafe { libc::munmap(self.ptr.as_ptr().cast(), self.len) };
    }
}

/// A pointer at `offset` into `map`, checked to lie inside it, aligned,
/// with room for `count` values of `T`.
fn field<T>(map: &Map, offset: u32, count: usize) -> RawResult<NonNull<T>> {
    let bytes = count.checked_mul(size_of::<T>()).ok_or_else(einval)?;
    let end = (offset as usize).checked_add(bytes);
    if end.is_none_or(|e| e > map.len) || offset as usize % align_of::<T>() != 0 {
        return Err(einval());
    }
    NonNull::new(map.at(offset).cast::<T>()).ok_or_else(einval)
}

/// The mapped submission and completion rings of one io_uring.
#[derive(Debug)]
pub(crate) struct Rings {
    sq_head: NonNull<AtomicU32>,
    sq_tail: NonNull<AtomicU32>,
    sq_flags: NonNull<AtomicU32>,
    sqes: NonNull<Sqe>,
    cq_head: NonNull<AtomicU32>,
    cq_tail: NonNull<AtomicU32>,
    cq_overflow: NonNull<AtomicU32>,
    cqes: NonNull<Cqe>,
    /// The bounds every index is masked to: private, checked at creation.
    sq_mask: u32,
    sq_entries: u32,
    cq_mask: u32,
    // Declared last: unmapped after nothing above can be used.
    _sq: Map,
    _cq: Option<Map>,
    _sqes: Map,
}

// SAFETY: the rings are shared memory with no thread affinity; the owning
// `Ring` is used by one thread at a time (`&mut` for every producer step,
// and an owned ring checks its thread).
unsafe impl Send for Rings {}

impl Rings {
    /// Maps the rings of `fd`, set up with `p`.
    pub(crate) fn new(fd: &OwnedFd, p: &Params) -> RawResult<Self> {
        let sq_len = p.sq_off.array as usize + p.sq_entries as usize * size_of::<u32>();
        let cq_len = p.cq_off.cqes as usize + p.cq_entries as usize * size_of::<Cqe>();
        let single = p.features & FEAT_SINGLE_MMAP != 0;
        let sq = Map::new(
            fd,
            OFF_SQ_RING,
            if single { sq_len.max(cq_len) } else { sq_len },
        )?;
        let cq = if single {
            None
        } else {
            Some(Map::new(fd, OFF_CQ_RING, cq_len)?)
        };
        let sqes = Map::new(fd, OFF_SQES, p.sq_entries as usize * size_of::<Sqe>())?;
        Self::from_maps(sq, cq, sqes, p)
    }

    /// The rings over their mappings (`cq` is `None` when one mapping holds
    /// both), with every offset checked against its mapping and the sizes in
    /// shared memory checked against those reported at setup.
    fn from_maps(sq: Map, cq: Option<Map>, sqes: Map, p: &Params) -> RawResult<Self> {
        let cqm = cq.as_ref().unwrap_or(&sq);
        let (s, c) = (&p.sq_off, &p.cq_off);
        let read = |m: &Map, off: u32| -> RawResult<u32> {
            // SAFETY: `field` checked the offset lies inside the mapping,
            // aligned; the kernel initialised it before setup returned.
            Ok(unsafe { field::<u32>(m, off, 1)?.as_ptr().read() })
        };
        let (sq_mask, sq_entries) = (read(&sq, s.ring_mask)?, read(&sq, s.ring_entries)?);
        let (cq_mask, cq_entries) = (read(cqm, c.ring_mask)?, read(cqm, c.ring_entries)?);
        // The sizes in shared memory must be those reported at setup (the
        // bounds below are checked against), powers of two with their masks:
        // a masked index can then never leave its array.
        let pow2 = |n: u32, mask: u32| n.is_power_of_two() && mask == n - 1;
        if sq_entries != p.sq_entries
            || cq_entries != p.cq_entries
            || !pow2(sq_entries, sq_mask)
            || !pow2(cq_entries, cq_mask)
        {
            return Err(einval());
        }
        if p.flags & setup::NO_SQARRAY == 0 {
            // The index array maps slot i to entry i, once for the ring's
            // life (entries are written in slot order).
            let array = field::<u32>(&sq, s.array, sq_entries as usize)?;
            for i in 0..sq_entries {
                // SAFETY: `i < sq_entries`, the length `field` checked; the
                // ring is disabled, so the kernel reads nothing yet.
                unsafe { array.as_ptr().add(i as usize).write(i) };
            }
        }
        Ok(Self {
            sq_head: field(&sq, s.head, 1)?,
            sq_tail: field(&sq, s.tail, 1)?,
            sq_flags: field(&sq, s.flags, 1)?,
            sqes: field(&sqes, 0, sq_entries as usize)?,
            cq_head: field(cqm, c.head, 1)?,
            cq_tail: field(cqm, c.tail, 1)?,
            cq_overflow: field(cqm, c.overflow, 1)?,
            cqes: field(cqm, c.cqes, cq_entries as usize)?,
            sq_mask,
            sq_entries,
            cq_mask,
            _sq: sq,
            _cq: cq,
            _sqes: sqes,
        })
    }

    fn atomic(&self, p: NonNull<AtomicU32>) -> &AtomicU32 {
        // SAFETY: `p` is one of `self`'s index fields, inside a live mapping
        // `self` owns, aligned, and shared with the kernel only through atomic
        // accesses; the reference lives no longer than `self`.
        unsafe { p.as_ref() }
    }

    /// Submission slots.
    pub(crate) fn sq_entries(&self) -> u32 {
        self.sq_entries
    }

    /// The kernel's SQ head: entries before it are consumed (acquire: the
    /// kernel is done reading them).
    pub(crate) fn sq_head(&self) -> u32 {
        self.atomic(self.sq_head).load(Ordering::Acquire)
    }

    /// Writes submission entry `sqe` at ring position `pos`.
    ///
    /// # Safety
    ///
    /// `pos` is at or after the last published tail and less than the
    /// kernel's head plus the ring size (a slot the kernel is not reading);
    /// and every buffer `sqe` names stays valid, and is not otherwise
    /// accessed, until its completion is reaped.
    pub(crate) unsafe fn write_sqe(&mut self, pos: u32, sqe: &Sqe) {
        let i = pos & self.sq_mask;
        // SAFETY: `i <= sq_mask < sq_entries`, the length `field` checked;
        // the slot is free by the caller's contract.
        unsafe { self.sqes.as_ptr().add(i as usize).write(*sqe) };
    }

    /// Publishes every entry before `tail` (release: the entries are
    /// written before the kernel can see the new tail).
    ///
    /// # Safety
    ///
    /// Every position before `tail` and after the previous published tail
    /// holds an entry written with [`Self::write_sqe`].
    pub(crate) unsafe fn publish(&self, tail: u32) {
        self.atomic(self.sq_tail).store(tail, Ordering::Release);
    }

    /// The SQ ring's flags (`SQ_CQ_OVERFLOW`, `SQ_TASKRUN`).
    pub(crate) fn sq_flags(&self) -> u32 {
        self.atomic(self.sq_flags).load(Ordering::Acquire)
    }

    /// Completions not yet consumed: `(head, tail)` (acquire on the tail: the
    /// entries are written before the kernel published it).
    pub(crate) fn cq_window(&self) -> (u32, u32) {
        let head = self.atomic(self.cq_head).load(Ordering::Relaxed);
        (head, self.atomic(self.cq_tail).load(Ordering::Acquire))
    }

    /// Reads the completion at ring position `pos`.
    ///
    /// # Safety
    ///
    /// `pos` lies in the window [`Self::cq_window`] returned, not yet
    /// consumed (the kernel will not write it).
    pub(crate) unsafe fn read_cqe(&self, pos: u32) -> Cqe {
        let i = pos & self.cq_mask;
        // SAFETY: `i <= cq_mask < cq_entries`, the length `field` checked;
        // the entry is the caller's by the contract above.
        unsafe { self.cqes.as_ptr().add(i as usize).read() }
    }

    /// Releases every completion before `head` back to the kernel.
    ///
    /// # Safety
    ///
    /// `head` is at most the window's tail, and every completion before it
    /// has been read (the kernel may overwrite them now).
    pub(crate) unsafe fn consume(&self, head: u32) {
        self.atomic(self.cq_head).store(head, Ordering::Release);
    }

    /// Completions the kernel could not keep (0 with `NODROP` unless it ran
    /// out of memory).
    pub(crate) fn cq_overflow(&self) -> u32 {
        self.atomic(self.cq_overflow).load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod fake {
    //! A simulated kernel over heap memory, for Miri: it checks every
    //! pointer the rings form and, with the kernel on another thread, that
    //! the index protocol publishes entries before the indices that cover
    //! them (a data race otherwise). Counters start just below `u32::MAX`,
    //! so they wrap.
    #![allow(clippy::unwrap_used)]

    use std::ptr::NonNull;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::{Map, Rings};
    use crate::uring::abi::{CqOffsets, Cqe, Params, SqOffsets, Sqe};

    /// Heap memory for one ring region of `len` bytes.
    fn heap(len: usize) -> Map {
        let raw = NonNull::new(Box::into_raw(
            vec![0u64; len.div_ceil(8)].into_boxed_slice(),
        ))
        .unwrap();
        Map {
            ptr: raw.cast::<u8>(),
            len,
            heap: Some(raw),
        }
    }

    const START: u32 = u32::MAX - 20;

    fn offsets() -> (SqOffsets, CqOffsets) {
        (
            SqOffsets {
                head: 0,
                tail: 4,
                ring_mask: 8,
                ring_entries: 12,
                flags: 16,
                dropped: 20,
                array: 24,
                ..SqOffsets::default()
            },
            CqOffsets {
                head: 0,
                tail: 4,
                ring_mask: 8,
                ring_entries: 12,
                overflow: 16,
                cqes: 32,
                ..CqOffsets::default()
            },
        )
    }

    /// Writes the sizes (and the starting counters) into fresh regions.
    fn init(sq: &Map, cq: &Map, sq_n: u32, cq_n: u32) {
        // SAFETY: fresh, exclusively owned heap regions, written at the
        // offsets `offsets` declares (inside them by construction).
        unsafe {
            for (m, n) in [(sq, sq_n), (cq, cq_n)] {
                m.at(0).cast::<u32>().write(START);
                m.at(4).cast::<u32>().write(START);
                m.at(8).cast::<u32>().write(n - 1);
                m.at(12).cast::<u32>().write(n);
            }
        }
    }

    /// The kernel side: raw views of the same memory.
    struct Kernel {
        sq_head: *const AtomicU32,
        sq_tail: *const AtomicU32,
        array: *const u32,
        sqes: *const Sqe,
        cq_tail: *const AtomicU32,
        cq_head: *const AtomicU32,
        cqes: *mut Cqe,
        sq_mask: u32,
        cq_mask: u32,
    }

    // SAFETY: the views point into heap memory the test keeps alive until
    // the kernel thread is joined.
    unsafe impl Send for Kernel {}

    impl Kernel {
        /// Consumes every published submission, completing each with its
        /// `user_data` and `res = len`. Returns how many.
        fn run_once(&self) -> u32 {
            // SAFETY: every pointer is inside its region; the kernel reads
            // entries only below the acquired tail, writes completions only
            // within the ring size past the consumer's acquired head, and
            // publishes with release stores, as the real kernel does.
            unsafe {
                let tail = (*self.sq_tail).load(Ordering::Acquire);
                let mut head = (*self.sq_head).load(Ordering::Relaxed);
                let mut n = 0;
                while head != tail {
                    let i = self.array.add((head & self.sq_mask) as usize).read();
                    let sqe = self.sqes.add(i as usize).read();
                    let ct = (*self.cq_tail).load(Ordering::Relaxed);
                    while ct.wrapping_sub((*self.cq_head).load(Ordering::Acquire)) > self.cq_mask {
                        std::thread::yield_now();
                    }
                    self.cqes.add((ct & self.cq_mask) as usize).write(Cqe {
                        user_data: sqe.user_data,
                        res: sqe.len as i32,
                        flags: 0,
                    });
                    (*self.cq_tail).store(ct.wrapping_add(1), Ordering::Release);
                    head = head.wrapping_add(1);
                    (*self.sq_head).store(head, Ordering::Release);
                    n += 1;
                }
                n
            }
        }
    }

    /// Rings of 4 submissions and 8 completions over heap memory, and the
    /// kernel's view of them.
    fn fake() -> (Rings, Kernel) {
        let (sq_n, cq_n) = (4u32, 8u32);
        let (sq_off, cq_off) = offsets();
        let (sq, cq, sqes) = (
            heap(24 + 4 * sq_n as usize),
            heap(32 + 16 * cq_n as usize),
            heap(64 * sq_n as usize),
        );
        init(&sq, &cq, sq_n, cq_n);
        let kernel = Kernel {
            sq_head: sq.at(0).cast(),
            sq_tail: sq.at(4).cast(),
            array: sq.at(24).cast(),
            sqes: sqes.at(0).cast(),
            cq_head: cq.at(0).cast(),
            cq_tail: cq.at(4).cast(),
            cqes: cq.at(32).cast(),
            sq_mask: sq_n - 1,
            cq_mask: cq_n - 1,
        };
        let p = Params {
            sq_entries: sq_n,
            cq_entries: cq_n,
            sq_off,
            cq_off,
            ..Params::default()
        };
        (Rings::from_maps(sq, Some(cq), sqes, &p).unwrap(), kernel)
    }

    /// The submission side and a kernel thread exchange 64 entries through
    /// a 4-entry ring, the counters wrapping past `u32::MAX`: every
    /// completion arrives once, with its entry's data.
    #[test]
    fn test_fake_kernel_round_trips_through_a_wrapping_ring() {
        let (mut r, kernel) = fake();
        let total = 64u32;
        let k = std::thread::spawn(move || {
            let mut done = 0;
            while done < total {
                done += kernel.run_once();
                std::thread::yield_now();
            }
        });
        let (mut tail, mut sent, mut got) = (START, 0u32, Vec::new());
        while got.len() < total as usize {
            while sent < total && tail.wrapping_sub(r.sq_head()) < r.sq_entries() {
                let sqe = Sqe {
                    user_data: u64::from(sent),
                    len: sent * 3,
                    ..Sqe::default()
                };
                // SAFETY: `tail` is a free slot (checked against the head
                // above); the entry names no buffer.
                unsafe { r.write_sqe(tail, &sqe) };
                tail = tail.wrapping_add(1);
                sent += 1;
            }
            // SAFETY: every position before `tail` was just written.
            unsafe { r.publish(tail) };
            let (head, ctail) = r.cq_window();
            let mut pos = head;
            while pos != ctail {
                // SAFETY: `pos` is inside the window just read.
                got.push(unsafe { r.read_cqe(pos) });
                pos = pos.wrapping_add(1);
            }
            // SAFETY: every completion before `ctail` was read.
            unsafe { r.consume(ctail) };
            std::thread::yield_now();
        }
        k.join().unwrap();
        for (i, c) in got.iter().enumerate() {
            assert_eq!((c.user_data, c.res), (i as u64, (i * 3) as i32));
        }
        assert_eq!(r.cq_overflow(), 0);
    }

    /// Sizes in shared memory that disagree with setup are refused (each
    /// region otherwise well-formed).
    #[test]
    fn test_mismatched_ring_sizes_are_refused() {
        let (sq_off, cq_off) = offsets();
        for (sq_says, cq_says) in [(8u32, 8u32), (4, 16)] {
            let (sq, cq, sqes) = (heap(24 + 4 * 8), heap(32 + 16 * 16), heap(64 * 8));
            init(&sq, &cq, sq_says, cq_says);
            let p = Params {
                sq_entries: 4,
                cq_entries: 8,
                sq_off,
                cq_off,
                ..Params::default()
            };
            assert!(
                Rings::from_maps(sq, Some(cq), sqes, &p).is_err(),
                "{sq_says} / {cq_says}"
            );
        }
    }
}
