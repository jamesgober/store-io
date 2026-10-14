//! A safe io_uring (D-076, D-081, D-093): set up per flavour, restricted to
//! an opcode allowlist, with submission and reaping that never lose sight
//! of an entry or a completion.

use std::cell::Cell;
use std::os::fd::{AsRawFd, OwnedFd};
use std::thread::ThreadId;

use libc::c_int;
use store_io_platform::{OsError, RawResult};

use super::abi::{
    self, Cqe, FEAT_NODROP, Params, Restriction, SQ_CQ_OVERFLOW, SQ_TASKRUN, Sqe, op, reg,
    restriction, setup,
};
use super::sys::{self, Rings};
use crate::sys::errno;

/// Who submits to a ring (D-081).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flavour {
    /// One thread for the ring's whole life (a `Submitter`): completions run
    /// only when that thread asks (`DEFER_TASKRUN`), with no interrupts.
    Owned,
    /// Any thread, one at a time, each waiting for what it submitted (the
    /// blocking layers' lanes). Completions interrupt the submitting thread
    /// (no `COOP_TASKRUN`: with it, a completion could wait on a thread that
    /// is asleep elsewhere, D-076).
    Shared,
}

/// Every opcode a ring accepts (D-073, D-076): reads and writes, fixed and
/// vectored, NOP and cancel. Nothing that always punts to io-wq (`FSYNC`,
/// `FALLOCATE`, `OPENAT`, ...) can be submitted.
const ALLOWED: [u8; 8] = [
    op::NOP,
    op::READ,
    op::WRITE,
    op::READ_FIXED,
    op::WRITE_FIXED,
    op::READV,
    op::WRITEV,
    op::ASYNC_CANCEL,
];

/// Registrations allowed after the ring is enabled (the restrictions are set
/// once, before; anything registered whole, such as a file table or the
/// buffers of D-078, is registered while the ring is still disabled): the
/// file and buffer updates, and releasing the ring descriptor. Oldest first:
/// a kernel that does not know one refuses the list (`EINVAL`), and the
/// newest are then dropped in order.
const ALLOWED_REGISTER: [libc::c_uint; 4] = [
    reg::FILES_UPDATE,
    reg::FILES_UPDATE2,
    reg::BUFFERS_UPDATE,
    reg::UNREGISTER_RING_FDS,
];

/// The setup flags to try for a flavour, best first; the next is tried
/// only when the kernel does not know a flag (`EINVAL`). An owned ring
/// never runs without `SINGLE_ISSUER | DEFER_TASKRUN` (6.1 and later).
fn tiers(flavour: Flavour) -> [u32; 2] {
    match flavour {
        Flavour::Owned => {
            let t1 = setup::SINGLE_ISSUER
                | setup::DEFER_TASKRUN
                | setup::TASKRUN_FLAG
                | setup::SUBMIT_ALL;
            [t1 | setup::NO_SQARRAY, t1]
        }
        Flavour::Shared => [setup::SUBMIT_ALL | setup::NO_SQARRAY, setup::SUBMIT_ALL],
    }
}

/// Why reaping stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reap {
    /// The kernel could not keep a completion (`EBADR`, or the overflow
    /// counter moved): everything in flight has an unknown outcome.
    Lost,
    /// Entering the kernel to run its deferred work failed.
    Os(OsError),
}

/// An io_uring ring. Dropping it with operations in flight leaves the kernel
/// writing to their buffers: its user drains it first (D-103 3b).
#[derive(Debug)]
pub(crate) struct Ring {
    rings: Rings,
    /// What `io_uring_enter` is given: the registered index, or the fd.
    enter_fd: c_int,
    /// `ENTER_REGISTERED_RING` when `enter_fd` is a registered index.
    enter_flags: u32,
    /// The registered index of an owned ring, valid only on `owner`.
    registered: Option<u32>,
    /// The only thread that may use an owned ring.
    owner: Option<ThreadId>,
    /// Next submission position.
    tail: u32,
    /// The setup flags in effect.
    pub(crate) flags: u32,
    /// Opcodes the kernel supports (`REGISTER_PROBE`).
    supported: [bool; 256],
    flavour: Flavour,
    // Declared last: closed after the rings are unmapped.
    fd: OwnedFd,
}

impl Ring {
    /// A ring of `entries` submissions (rounded up to a power of two, at
    /// most 4096), enabled for the calling thread, which for an
    /// [`Flavour::Owned`] ring is its only user.
    ///
    /// # Errors
    ///
    /// The kernel's refusal (a setup tier it does not know is skipped; any
    /// other error is returned), or a kernel without `NODROP`, the opcode
    /// probe, or NOP, READ and WRITE (`ENOSYS`).
    pub(crate) fn new(flavour: Flavour, entries: u32) -> RawResult<Self> {
        let entries = entries.clamp(1, 4096).next_power_of_two();
        let mut last = errno(libc::EINVAL);
        for flags in tiers(flavour) {
            let mut p = Params {
                cq_entries: entries * 2,
                flags: flags | setup::CQSIZE | setup::R_DISABLED,
                ..Params::default()
            };
            match sys::setup(entries, &mut p) {
                Ok(fd) => return Self::finish(flavour, fd, &p),
                Err(e) if e.code == libc::EINVAL => last = e,
                Err(e) => return Err(e),
            }
        }
        Err(last)
    }

    fn finish(flavour: Flavour, fd: OwnedFd, p: &Params) -> RawResult<Self> {
        if p.features & FEAT_NODROP == 0 {
            return Err(errno(libc::ENOSYS));
        }
        let rings = Rings::new(&fd, p)?;
        // D-073 (nothing on io-wq) is kept by the allowlist and NOWAIT, not
        // by `IOWQ_MAX_WORKERS`: that limit belongs to the calling thread,
        // so it would also cap the application's own rings on it.
        // A kernel without the probe (`EINVAL`) is unsupported; any other
        // refusal (memory, say) is passed on as it is.
        let supported = sys::probe(&fd).map_err(|e| {
            if e.code == libc::EINVAL {
                errno(libc::ENOSYS)
            } else {
                e
            }
        })?;
        if ![op::NOP, op::READ, op::WRITE]
            .iter()
            .all(|o| supported[usize::from(*o)])
        {
            return Err(errno(libc::ENOSYS));
        }
        let can_release = restrict(&fd, &supported)?;
        // Whole registrations (D-078's sparse file table and fixed buffers,
        // step 3d) go here, while the ring is disabled: the restrictions
        // allow only updates once it is enabled.
        // Registered only if it can be released again (else the thread's
        // table would keep the ring alive until the thread exits).
        let registered = if flavour == Flavour::Owned && can_release {
            sys::register_ring_fd(&fd).ok()
        } else {
            None
        };
        if let Err(e) = sys::enable(&fd) {
            if let Some(index) = registered {
                let _released = sys::unregister_ring_fd(&fd, index);
            }
            return Err(e);
        }
        Ok(Self {
            rings,
            enter_fd: registered.map_or(fd.as_raw_fd(), |i| i as c_int),
            enter_flags: registered.map_or(0, |_| abi::ENTER_REGISTERED_RING),
            registered,
            owner: (flavour == Flavour::Owned).then(this_thread),
            tail: 0,
            flags: p.flags,
            supported,
            flavour,
            fd,
        })
    }

    /// Whether the kernel supports `opcode`.
    pub(crate) fn supports(&self, opcode: u8) -> bool {
        self.supported[usize::from(opcode)]
    }

    /// The ring's flavour.
    pub(crate) fn flavour(&self) -> Flavour {
        self.flavour
    }

    /// Refuses an owned ring's use from another thread (its registered
    /// index and its single issuer belong to the owner).
    pub(crate) fn check_thread(&self) -> RawResult<()> {
        match self.owner {
            Some(id) if !is_this_thread(id) => Err(errno(libc::EXDEV)),
            _ => Ok(()),
        }
    }

    /// Entries queued and not yet consumed by the kernel.
    pub(crate) fn queued(&self) -> u32 {
        self.tail.wrapping_sub(self.rings.sq_head())
    }

    /// Submission slots free now.
    pub(crate) fn space(&self) -> u32 {
        self.rings.sq_entries() - self.queued()
    }

    /// Queues `sqe` for the next submission; false (nothing queued) when the
    /// submission ring is full.
    ///
    /// # Safety
    ///
    /// Every buffer `sqe` names stays valid, and is not otherwise accessed,
    /// until its completion is reaped; the ring is not dropped before.
    pub(crate) unsafe fn push(&mut self, sqe: &Sqe) -> bool {
        if self.space() == 0 {
            return false;
        }
        // SAFETY: `space() > 0`: the slot at `tail` is past every published
        // entry and within the ring size of the kernel's head; the buffers
        // are the caller's contract.
        unsafe { self.rings.write_sqe(self.tail, sqe) };
        self.tail = self.tail.wrapping_add(1);
        true
    }

    fn enter(&mut self, min_complete: u32, flags: u32) -> RawResult<u32> {
        self.check_thread()?;
        // Everything the kernel has not consumed, including entries an
        // earlier call published but the kernel did not take.
        let to_submit = self.queued();
        // SAFETY: every position before `tail` was written by `push`.
        unsafe { self.rings.publish(self.tail) };
        loop {
            match sys::enter(
                self.enter_fd,
                to_submit,
                min_complete,
                flags | self.enter_flags,
            ) {
                // A signal before the kernel looked at anything: nothing was
                // decided, so the call is made again.
                Err(e) if e.code == libc::EINTR => {}
                other => return other,
            }
        }
    }

    /// Submits every queued entry without waiting. Returns how many the
    /// kernel took; any it did not stay queued for the next call.
    pub(crate) fn submit(&mut self) -> RawResult<u32> {
        if self.queued() == 0 {
            return Ok(0);
        }
        self.enter(0, 0)
    }

    /// Submits every queued entry and waits for at least `want`
    /// completions. It may return before (a signal during the wait): the
    /// caller reaps and waits again.
    pub(crate) fn submit_and_wait(&mut self, want: u32) -> RawResult<u32> {
        self.enter(want, abi::ENTER_GETEVENTS)
    }

    /// Hands every ready completion to `f`, in the order posted. Runs the
    /// ring's deferred work first when the kernel flags some (an owned ring
    /// posts completions only then), and moves a backlog the kernel kept
    /// into the ring. Each completion is released to the kernel before `f`
    /// sees it, so a panic in `f` never hands one over twice. Entering the
    /// kernel also submits anything queued.
    ///
    /// # Errors
    ///
    /// [`Reap::Lost`] if the kernel could not keep a completion;
    /// [`Reap::Os`] if entering the kernel failed otherwise.
    pub(crate) fn reap(&mut self, mut f: impl FnMut(Cqe)) -> Result<usize, Reap> {
        let mut n = 0;
        loop {
            if self.rings.sq_flags() & (SQ_TASKRUN | SQ_CQ_OVERFLOW) != 0 {
                match self.enter(0, abi::ENTER_GETEVENTS) {
                    Err(e) if e.code == libc::EBADR => return Err(Reap::Lost),
                    // A backlog still waiting for room, or no memory for the
                    // submission path: the ring is drained below, then again.
                    Err(e) if e.code == libc::EBUSY || e.code == libc::EAGAIN => {}
                    Err(e) => return Err(Reap::Os(e)),
                    Ok(_) => {}
                }
            }
            let (head, tail) = self.rings.cq_window();
            if head == tail {
                break;
            }
            let mut pos = head;
            while pos != tail {
                // SAFETY: `pos` is inside the window just read and not yet
                // consumed; it is released before `f` runs.
                let cqe = unsafe { self.rings.read_cqe(pos) };
                pos = pos.wrapping_add(1);
                // SAFETY: every completion before `pos` has been read.
                unsafe { self.rings.consume(pos) };
                n += 1;
                f(cqe);
            }
        }
        if self.rings.cq_overflow() != 0 {
            return Err(Reap::Lost);
        }
        Ok(n)
    }
}

thread_local! {
    /// The thread's id, once `this_thread` has asked for it.
    static ID: Cell<Option<ThreadId>> = const { Cell::new(None) };
}

/// The calling thread's id, without the `Arc` clone of
/// `std::thread::current()` on every call.
fn this_thread() -> ThreadId {
    ID.with(|c| {
        c.get().unwrap_or_else(|| {
            let id = std::thread::current().id();
            c.set(Some(id));
            id
        })
    })
}

/// Whether the calling thread is `id`, without ever asking std for the
/// thread's id: safe in a thread-local destructor (where that can panic),
/// and one thread-local load on the submit path.
/// An owner has always cached its id (`finish`), so a thread that has not
/// is not the owner.
fn is_this_thread(id: ThreadId) -> bool {
    ID.try_with(|c| c.get() == Some(id)).unwrap_or(false)
}

/// The restrictions: the allowlisted opcodes this kernel has, fixed files,
/// and the registrations allowed after enabling (the newest dropped while
/// the kernel refuses the list). Returns whether releasing a ring
/// descriptor is among them.
fn restrict(fd: &OwnedFd, supported: &[bool; 256]) -> RawResult<bool> {
    let mut list = [Restriction::default(); ALLOWED.len() + 1 + ALLOWED_REGISTER.len()];
    let mut n = 0;
    let mut add = |opcode: u16, arg: u8| {
        list[n] = Restriction {
            opcode,
            arg,
            ..Restriction::default()
        };
        n += 1;
    };
    for o in ALLOWED.iter().filter(|o| supported[usize::from(**o)]) {
        add(restriction::SQE_OP, *o);
    }
    add(restriction::SQE_FLAGS_ALLOWED, abi::SQE_FIXED_FILE);
    for r in ALLOWED_REGISTER {
        add(restriction::REGISTER_OP, r as u8);
    }
    let base = n - ALLOWED_REGISTER.len();
    for kept in (0..=ALLOWED_REGISTER.len()).rev() {
        match sys::restrict(fd, &mut list[..base + kept]) {
            Ok(()) => return Ok(kept == ALLOWED_REGISTER.len()),
            // A kernel that does not know the newest: the list without it.
            Err(e) if e.code == libc::EINVAL && kept > 0 => {}
            Err(e) => return Err(e),
        }
    }
    Err(errno(libc::EINVAL))
}

impl Drop for Ring {
    fn drop(&mut self) {
        // The registered index lives in the owner's table: released there.
        // (An owned ring dropped on another thread cannot release it; the
        // owner's table then keeps the ring until that thread exits. Its
        // only user, a `Submitter`, cannot leave its thread.)
        if let Some(index) = self.registered {
            if self.owner.is_some_and(is_this_thread) {
                let _released = sys::unregister_ring_fd(&self.fd, index);
            }
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn nop(user_data: u64) -> Sqe {
        Sqe {
            opcode: op::NOP,
            user_data,
            ..Sqe::default()
        }
    }

    /// Pushes a NOP (no buffer, so no contract to keep).
    fn push_nop(r: &mut Ring, user_data: u64) -> bool {
        // SAFETY: a NOP names no buffer.
        unsafe { r.push(&nop(user_data)) }
    }

    fn rings() -> [Ring; 2] {
        [Flavour::Owned, Flavour::Shared].map(|f| Ring::new(f, 8).unwrap())
    }

    #[test]
    fn test_a_nop_round_trips_on_both_flavours() {
        for mut r in rings() {
            assert!(r.supports(op::READ) && r.supports(op::WRITE));
            assert!(push_nop(&mut r, 7));
            assert_eq!(r.submit_and_wait(1).unwrap(), 1);
            let mut got = Vec::new();
            assert_eq!(r.reap(|c| got.push(c)), Ok(1));
            assert_eq!(
                got,
                [Cqe {
                    user_data: 7,
                    res: 0,
                    flags: 0
                }],
                "{:?}",
                r.flavour()
            );
        }
    }

    /// E12 regression (D-076): a completion posted later, as task work (a
    /// read of an eventfd that becomes readable after the submit), is
    /// reaped without any wait: the reap runs the work the kernel flags.
    /// An owned ring posts it only then.
    #[test]
    fn test_a_deferred_completion_is_reaped_without_waiting() {
        for mut r in rings() {
            let efd = sys::eventfd().unwrap();
            // Leaked: if an assertion fails with the read pending, the kernel
            // may still write here after the test ends.
            let word: &'static mut [u8; 8] = Box::leak(Box::new([0u8; 8]));
            let read = Sqe {
                opcode: op::READ,
                fd: efd.as_raw_fd(),
                addr: word.as_mut_ptr() as u64,
                len: 8,
                user_data: 5,
                ..Sqe::default()
            };
            // SAFETY: `word` is never freed; `efd` outlives the completion,
            // reaped below.
            assert!(unsafe { r.push(&read) });
            assert_eq!(r.submit().unwrap(), 1);
            let mut got = Vec::new();
            assert_eq!(r.reap(|c| got.push(c)), Ok(0), "completed before it could");
            sys::eventfd_signal(&efd);
            let mut tries = 0;
            while got.is_empty() {
                tries += 1;
                assert!(
                    tries < 100_000,
                    "{:?}: a deferred completion was stranded",
                    r.flavour()
                );
                let _n = r.reap(|c| got.push(c)).unwrap();
            }
            assert_eq!((got[0].user_data, got[0].res), (5, 8));
            assert_eq!(u64::from_ne_bytes(*word), 1);
        }
    }

    /// The allowlist refuses an opcode that always punts (D-073).
    #[test]
    fn test_the_allowlist_refuses_fsync() {
        for mut r in rings() {
            let sqe = Sqe {
                opcode: op::FSYNC,
                fd: -1,
                user_data: 9,
                ..Sqe::default()
            };
            // SAFETY: no buffer.
            assert!(unsafe { r.push(&sqe) });
            let _taken = r.submit_and_wait(1);
            let mut got = Vec::new();
            assert_eq!(r.reap(|c| got.push(c)), Ok(1));
            assert_eq!(got[0].user_data, 9);
            assert_eq!(got[0].res, -libc::EACCES, "{:?}", r.flavour());
        }
    }

    /// Many rounds through a small ring: every completion arrives exactly
    /// once; a full ring refuses a push.
    #[test]
    fn test_many_rounds_lose_and_repeat_nothing() {
        for mut r in rings() {
            let size = r.space();
            let mut seen = vec![0u32; 2000];
            let mut next = 0u64;
            while next < 2000 {
                while r.space() > 0 && next < 2000 {
                    assert!(push_nop(&mut r, next));
                    next += 1;
                }
                if next < 2000 {
                    assert!(!push_nop(&mut r, u64::MAX), "pushed into a full ring");
                }
                let q = r.queued();
                let _taken = r.submit_and_wait(q).unwrap();
                let _n = r.reap(|c| seen[c.user_data as usize] += 1).unwrap();
            }
            assert!(
                seen.iter().all(|&n| n == 1),
                "{:?}: lost or repeated",
                r.flavour()
            );
            assert_eq!(r.space(), size);
        }
    }

    /// More completions than the completion ring holds, none reaped in
    /// between: the kernel keeps the backlog and the reap collects all of
    /// it, nothing lost.
    #[test]
    fn test_a_completion_backlog_is_collected() {
        for mut r in rings() {
            let size = r.space();
            let total = 3 * size;
            for round in 0..3 {
                for i in 0..size {
                    assert!(push_nop(&mut r, u64::from(round * size + i)));
                }
                let _taken = r.submit_and_wait(0).unwrap();
            }
            let mut seen = vec![0u32; total as usize];
            let mut tries = 0;
            while seen.contains(&0) {
                tries += 1;
                assert!(tries < 1000, "{:?}: backlog not collected", r.flavour());
                let _n = r.reap(|c| seen[c.user_data as usize] += 1).unwrap();
            }
            assert!(seen.iter().all(|&n| n == 1), "{:?}", r.flavour());
        }
    }

    /// An owned ring refuses use from another thread; a shared ring works
    /// from a thread that owns a ring of its own (no registered-index mix-up).
    #[test]
    fn test_thread_rules() {
        let owned = Ring::new(Flavour::Owned, 8).unwrap();
        let shared = Ring::new(Flavour::Shared, 8).unwrap();
        std::thread::spawn(move || {
            let (mut owned, mut shared) = (owned, shared);
            assert!(push_nop(&mut owned, 1));
            assert_eq!(owned.submit().map_err(|e| e.code), Err(libc::EXDEV));
            let mut mine = Ring::new(Flavour::Owned, 8).unwrap();
            // Both registered, each in its own thread's table (likely at
            // the same index).
            assert!(owned.registered.is_some() && mine.registered.is_some());
            assert!(push_nop(&mut mine, 2));
            assert!(push_nop(&mut shared, 3));
            assert_eq!(shared.submit_and_wait(1).unwrap(), 1);
            let mut got = Vec::new();
            assert_eq!(shared.reap(|c| got.push(c)), Ok(1));
            assert_eq!(got[0].user_data, 3);
            assert_eq!(mine.submit_and_wait(1).unwrap(), 1);
            got.clear();
            assert_eq!(mine.reap(|c| got.push(c)), Ok(1));
            assert_eq!(got[0].user_data, 2);
            // Dropped off its owner: it must not release this thread's
            // index (which may be `mine`'s).
            drop(owned);
            assert!(push_nop(&mut mine, 4));
            assert_eq!(mine.submit_and_wait(1).unwrap(), 1);
            got.clear();
            assert_eq!(mine.reap(|c| got.push(c)), Ok(1));
            assert_eq!(got[0].user_data, 4);
        })
        .join()
        .unwrap();
        // Dropped on its owner: its registered index is released there.
        let owned = Ring::new(Flavour::Owned, 8).unwrap();
        assert!(owned.registered.is_some());
        drop(owned);
    }
}
