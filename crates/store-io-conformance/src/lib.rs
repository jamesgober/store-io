//! # store-io-conformance
//!
//! The checks every store-io platform backend must pass, on every file system
//! it supports. A [`Suite`] runs each scenario in a fresh directory under a
//! scratch root and returns a [`Report`] naming every scenario and what, if
//! anything, went wrong; one failure never hides the others.
//!
//! Two kinds of scenario:
//!
//! - **The platform contract** (the `Platform` and `Queue` traits): exclusive
//!   create and not-found mapping, sizes, many operations in flight completing
//!   once each with their buffers, durable writes and flushes, queue limits,
//!   read-only handles, rename and unlink, directory sync, the ownership lock,
//!   range state after a full write, release, and the probe's honesty about
//!   what it could not read.
//! - **Store behaviour on that platform**: a round trip through a reopen, a
//!   read-only open that changes nothing, ownership, and directory-mode
//!   replace.
//!
//! The suite cannot cut power: crash behaviour is tested on the simulator
//! (`store-io-sim`) and, for certification, on a power-cut rig.

#![deny(warnings)]
#![forbid(unsafe_code)]

use std::fmt;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use store_io_buf::{BufPool, IoBuf, PoolConfig};
use store_io_core::class::Missing;
use store_io_core::decide::Trust;
use store_io_core::errno::{is_already_exists, is_not_found};
use store_io_core::error::Error;
use store_io_core::evidence::Tri;
use store_io_engine::{CONTAINER, Directory, ScanItem, Store, StoreOptions};
use store_io_platform::{
    Completion, CompletionBuf, FileMode, FileName, IoOp, Platform, Queue, QueueConfig, RawResult,
    ReleaseHow,
};

/// Bytes per block in the scenarios: a multiple of every direct-I/O
/// alignment store-io supports.
const BLOCK: usize = 4096;

/// One scenario's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// The scenario's name.
    pub name: &'static str,
    /// `None` if it passed, else what went wrong.
    pub failure: Option<String>,
}

/// The outcome of every scenario.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Report {
    /// One entry per scenario, in run order.
    pub outcomes: Vec<Outcome>,
}

impl Report {
    /// Whether every scenario passed.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.outcomes.iter().all(|o| o.failure.is_none())
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for o in &self.outcomes {
            match &o.failure {
                None => writeln!(f, "pass  {}", o.name)?,
                Some(why) => writeln!(f, "FAIL  {}: {why}", o.name)?,
            }
        }
        let failed = self.outcomes.iter().filter(|o| o.failure.is_some()).count();
        write!(f, "{} scenarios, {failed} failed", self.outcomes.len())
    }
}

/// A conformance run over one platform and one scratch directory.
#[derive(Debug)]
pub struct Suite<'a, P> {
    platform: &'a P,
    root: PathBuf,
    trust: Trust,
}

/// What a scenario gets: the platform, a fresh directory, buffers, trust.
struct Ctx<'a, P> {
    p: &'a P,
    path: PathBuf,
    pool: BufPool,
    trust: Trust,
}

type Check = Result<(), String>;
type Scenario<P> = (&'static str, fn(&Ctx<'_, P>) -> Check);

/// Fails the scenario with a message unless `$cond` holds.
macro_rules! check {
    ($cond:expr, $($msg:tt)+) => {
        if !$cond {
            return Err(format!($($msg)+));
        }
    };
}

/// Unwraps a platform result or fails the scenario naming the step.
fn ok<T>(r: RawResult<T>, step: &str) -> Result<T, String> {
    r.map_err(|e| format!("{step}: {e}"))
}

/// Unwraps a store result or fails the scenario naming the step.
fn done<T>(r: Result<T, Error>, step: &str) -> Result<T, String> {
    r.map_err(|e| format!("{step}: {e}"))
}

fn name(s: &str) -> Result<FileName, String> {
    FileName::new(s).map_err(|_| format!("invalid name {s:?}"))
}

fn pattern(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| seed.wrapping_mul(31).wrapping_add((i * 7 + (i >> 9)) as u8))
        .collect()
}

impl<'a, P> Suite<'a, P>
where
    P: Platform + Clone,
    P::Queue: Send,
{
    /// A suite over `platform`, running each scenario in a fresh directory
    /// under `root` (which must exist and should be empty). `trust` is used
    /// for the store scenarios: pass an override only for devices that are
    /// refused (virtual disks, `nobarrier` mounts), and say so in your
    /// report.
    pub fn new(platform: &'a P, root: impl AsRef<Path>, trust: Trust) -> Self {
        Self {
            platform,
            root: root.as_ref().to_path_buf(),
            trust,
        }
    }

    /// The scenarios, in run order.
    fn scenarios() -> Vec<Scenario<P>> {
        vec![
            (
                "create is exclusive and a missing file is not found",
                create_is_exclusive,
            ),
            ("allocate only grows and set_len is exact", sizes),
            (
                "many operations in flight complete once each, with their buffers",
                many_in_flight,
            ),
            (
                "durable writes, data flushes and full flushes complete",
                durable_and_flushes,
            ),
            (
                "a full queue refuses with the buffer and no OS error",
                queue_full,
            ),
            (
                "a read-only handle reads and never writes",
                read_only_handle,
            ),
            (
                "rename replaces, unlink removes, the directory syncs",
                rename_unlink_sync,
            ),
            (
                "the ownership lock admits one holder at a time",
                ownership_lock,
            ),
            (
                "a fully written range is allocated, unshared and uncached",
                written_range_state,
            ),
            ("a released range keeps the file's size", release_keeps_size),
            (
                "the probe reports alignment and lists what it could not read",
                probe_is_honest,
            ),
            ("a store round-trips through a reopen", store_round_trip),
            (
                "a read-only open changes nothing",
                read_only_open_changes_nothing,
            ),
            ("a second writable open is refused", second_open_is_locked),
            (
                "a directory replace round-trips with exact bytes",
                directory_replace,
            ),
        ]
    }

    /// The scenario names, in run order.
    #[must_use]
    pub fn names() -> Vec<&'static str> {
        Self::scenarios().into_iter().map(|(n, _)| n).collect()
    }

    /// Runs every scenario.
    #[must_use]
    pub fn run(&self) -> Report {
        static RUN: AtomicU32 = AtomicU32::new(0);
        let run = RUN.fetch_add(1, Ordering::Relaxed);
        let outcomes = Self::scenarios()
            .into_iter()
            .enumerate()
            .map(|(i, (name, scenario))| {
                let path = self
                    .root
                    .join(format!("sio-conf-{}-{run}-{i:02}", std::process::id()));
                let failure = match BufPool::new(&PoolConfig::uniform(BLOCK, BLOCK, 1 << 17, 16)) {
                    Ok(pool) => scenario(&Ctx {
                        p: self.platform,
                        path,
                        pool,
                        trust: self.trust,
                    })
                    .err(),
                    Err(e) => Some(format!("buffer pool: {e:?}")),
                };
                Outcome { name, failure }
            })
            .collect();
        Report { outcomes }
    }
}

impl<P: Platform> Ctx<'_, P> {
    fn dir(&self) -> Result<P::Dir, String> {
        ok(self.p.open_dir(&self.path, true), "open_dir")
    }

    fn queue(&self, depth: u32) -> Result<P::Queue, String> {
        ok(self.p.queue(QueueConfig { depth }), "queue")
    }

    fn buf(&self, bytes: &[u8]) -> Result<IoBuf, String> {
        let mut b = self
            .pool
            .take(bytes.len())
            .map_err(|_| "buffer pool exhausted".to_owned())?;
        b.as_mut_slice().copy_from_slice(bytes);
        Ok(b)
    }

    /// A new file of `len` bytes, allocated.
    fn file(&self, dir: &P::Dir, file: &str, len: u64) -> Result<P::File, String> {
        let f = ok(self.p.create_file(dir, &name(file)?), "create_file")?;
        ok(self.p.allocate(&f, len), "allocate")?;
        Ok(f)
    }
}

/// Waits for `n` completions.
fn drain<Q: Queue>(q: &mut Q, n: usize) -> Result<Vec<Completion>, String> {
    let mut out = CompletionBuf::with_capacity(n.max(1));
    let mut got = Vec::new();
    while got.len() < n {
        let _added = ok(q.wait(1, &mut out), "wait")?;
        got.extend(out.drain());
    }
    Ok(got)
}

fn write_one<Q: Queue>(
    q: &mut Q,
    file: &Q::File,
    offset: u64,
    buf: IoBuf,
    dsync: bool,
) -> Result<RawResult<usize>, String> {
    q.submit(
        IoOp::Write {
            file,
            offset,
            buf,
            dsync,
        },
        offset,
    )
    .map_err(|r| format!("write at {offset} refused: {:?}", r.raw))?;
    let c = drain(q, 1)?.pop().ok_or("no completion")?;
    Ok(c.result)
}

fn read_one<Q: Queue>(
    q: &mut Q,
    file: &Q::File,
    offset: u64,
    buf: IoBuf,
) -> Result<(RawResult<usize>, Option<IoBuf>), String> {
    q.submit(IoOp::Read { file, offset, buf }, offset)
        .map_err(|r| format!("read at {offset} refused: {:?}", r.raw))?;
    let c = drain(q, 1)?.pop().ok_or("no completion")?;
    Ok((c.result, c.buf))
}

// ---------------------------------------------------------------------------
// The platform contract
// ---------------------------------------------------------------------------

fn create_is_exclusive<P: Platform>(c: &Ctx<'_, P>) -> Check {
    let dir = c.dir()?;
    let _a = c.file(&dir, "a", BLOCK as u64)?;
    match c.p.create_file(&dir, &name("a")?) {
        Ok(_) => return Err("a second create of the same name succeeded".into()),
        Err(e) => check!(
            is_already_exists(e),
            "second create: {e} is not 'already exists'"
        ),
    }
    match c.p.open_file(&dir, &name("missing")?, FileMode::ReadOnly) {
        Ok(_) => Err("opening a missing file succeeded".into()),
        Err(e) => {
            check!(is_not_found(e), "missing file: {e} is not 'not found'");
            Ok(())
        }
    }
}

fn sizes<P: Platform>(c: &Ctx<'_, P>) -> Check {
    let dir = c.dir()?;
    let f = c.file(&dir, "s", 16 * BLOCK as u64)?;
    check!(
        ok(c.p.size(&f), "size")? == 16 * BLOCK as u64,
        "allocate did not set the size"
    );
    ok(c.p.allocate(&f, BLOCK as u64), "allocate smaller")?;
    let after = ok(c.p.size(&f), "size")?;
    check!(
        after == 16 * BLOCK as u64,
        "allocate shrank the file to {after}"
    );
    ok(c.p.set_len(&f, 100), "set_len 100")?;
    check!(ok(c.p.size(&f), "size")? == 100, "set_len did not truncate");
    ok(c.p.set_len(&f, 3 * BLOCK as u64), "set_len grow")?;
    check!(
        ok(c.p.size(&f), "size")? == 3 * BLOCK as u64,
        "set_len did not extend"
    );
    Ok(())
}

fn many_in_flight<P: Platform>(c: &Ctx<'_, P>) -> Check {
    let dir = c.dir()?;
    let f = c.file(&dir, "m", 1 << 20)?;
    let mut q = c.queue(16)?;
    let mut expect = Vec::new();
    let mut at = 0u64;
    for i in 0..16u8 {
        let len = BLOCK * (1 + usize::from(i % 4));
        let bytes = pattern(i, len);
        let buf = c.buf(&bytes)?;
        q.submit(
            IoOp::Write {
                file: &f,
                offset: at,
                buf,
                dsync: false,
            },
            u64::from(i),
        )
        .map_err(|r| format!("write {i} refused: {:?}", r.raw))?;
        expect.push((at, bytes));
        at += len as u64;
    }
    let mut seen = [false; 16];
    for comp in drain(&mut q, 16)? {
        let i = usize::try_from(comp.tag).unwrap_or(usize::MAX);
        check!(
            i < 16 && !seen[i],
            "completion tag {} unknown or repeated",
            comp.tag
        );
        seen[i] = true;
        let len = expect[i].1.len();
        check!(
            comp.result == Ok(len),
            "write {i}: {:?}, want {len}",
            comp.result
        );
        check!(
            comp.buf.as_ref().is_some_and(|b| b.len() == len),
            "write {i} did not return its buffer"
        );
    }
    for (i, (at, bytes)) in expect.iter().enumerate() {
        let buf = c.buf(&vec![0; bytes.len()])?;
        let (r, back) = read_one(&mut q, &f, *at, buf)?;
        check!(r == Ok(bytes.len()), "read {i}: {r:?}");
        check!(
            back.is_some_and(|b| b.as_slice() == bytes.as_slice()),
            "read {i} returned other bytes"
        );
    }
    Ok(())
}

fn durable_and_flushes<P: Platform>(c: &Ctx<'_, P>) -> Check {
    let dir = c.dir()?;
    let f = c.file(&dir, "d", 4 * BLOCK as u64)?;
    let mut q = c.queue(4)?;
    let r = write_one(&mut q, &f, 0, c.buf(&pattern(1, BLOCK))?, true)?;
    check!(r == Ok(BLOCK), "durable write: {r:?}");
    q.submit(IoOp::FlushData { file: &f }, 7)
        .map_err(|r| format!("data flush refused: {:?}", r.raw))?;
    let comp = drain(&mut q, 1)?.pop().ok_or("no flush completion")?;
    check!(
        comp.tag == 7 && comp.result.is_ok(),
        "data flush: {:?}",
        comp.result
    );
    check!(comp.buf.is_none(), "a flush completion carried a buffer");
    ok(c.p.flush_all(&f), "flush_all")
}

fn queue_full<P: Platform>(c: &Ctx<'_, P>) -> Check {
    let dir = c.dir()?;
    let f = c.file(&dir, "q", 4 * BLOCK as u64)?;
    let mut q = c.queue(2)?;
    for i in 0..2u64 {
        let buf = c.buf(&pattern(2, BLOCK))?;
        q.submit(
            IoOp::Write {
                file: &f,
                offset: i * BLOCK as u64,
                buf,
                dsync: false,
            },
            i,
        )
        .map_err(|r| format!("write {i} refused: {:?}", r.raw))?;
    }
    let third = q.submit(
        IoOp::Write {
            file: &f,
            offset: 2 * BLOCK as u64,
            buf: c.buf(&pattern(3, BLOCK))?,
            dsync: false,
        },
        2,
    );
    match third {
        Ok(()) => return Err("a third operation on a queue of depth 2 was accepted".into()),
        Err(r) => {
            check!(
                r.raw.is_none(),
                "a full queue reported an OS error {:?}",
                r.raw
            );
            check!(
                r.buf.is_some_and(|b| b.len() == BLOCK),
                "the refused buffer was not returned"
            );
        }
    }
    let _two = drain(&mut q, 2)?;
    check!(
        q.in_flight() == 0,
        "operations left in flight after draining"
    );
    Ok(())
}

fn read_only_handle<P: Platform>(c: &Ctx<'_, P>) -> Check {
    let dir = c.dir()?;
    let w = c.file(&dir, "r", 4 * BLOCK as u64)?;
    let mut q = c.queue(4)?;
    let _written = write_one(&mut q, &w, 0, c.buf(&pattern(4, BLOCK))?, false)?;
    ok(c.p.flush_all(&w), "flush_all")?;
    let r = ok(
        c.p.open_file(&dir, &name("r")?, FileMode::ReadOnly),
        "open read-only",
    )?;
    let (got, back) = read_one(&mut q, &r, 0, c.buf(&[0; BLOCK])?)?;
    check!(got == Ok(BLOCK), "read-only read: {got:?}");
    check!(
        back.is_some_and(|b| b.as_slice() == pattern(4, BLOCK).as_slice()),
        "read-only read returned other bytes"
    );
    let attempt = q.submit(
        IoOp::Write {
            file: &r,
            offset: 0,
            buf: c.buf(&pattern(5, BLOCK))?,
            dsync: false,
        },
        9,
    );
    if attempt.is_ok() {
        let comp = drain(&mut q, 1)?.pop().ok_or("no completion")?;
        check!(
            comp.result.is_err(),
            "a write through a read-only handle succeeded"
        );
    }
    check!(
        c.p.allocate(&r, 64 * BLOCK as u64).is_err(),
        "allocate through a read-only handle succeeded"
    );
    check!(
        c.p.set_len(&r, 0).is_err(),
        "set_len through a read-only handle succeeded"
    );
    let (again, back) = read_one(&mut q, &r, 0, c.buf(&[0; BLOCK])?)?;
    check!(
        again == Ok(BLOCK) && back.is_some_and(|b| b.as_slice() == pattern(4, BLOCK).as_slice()),
        "the file changed through a read-only handle"
    );
    Ok(())
}

fn rename_unlink_sync<P: Platform>(c: &Ctx<'_, P>) -> Check {
    let dir = c.dir()?;
    let mut q = c.queue(4)?;
    let a = c.file(&dir, "a", BLOCK as u64)?;
    let b = c.file(&dir, "b", BLOCK as u64)?;
    let _x = write_one(&mut q, &a, 0, c.buf(&pattern(6, BLOCK))?, false)?;
    let _y = write_one(&mut q, &b, 0, c.buf(&pattern(7, BLOCK))?, false)?;
    ok(c.p.flush_all(&a), "flush a")?;
    ok(c.p.flush_all(&b), "flush b")?;
    ok(
        c.p.rename_replace(&dir, &name("a")?, &name("b")?, &a),
        "rename_replace",
    )?;
    ok(c.p.sync_dir(&dir), "sync_dir")?;
    drop((a, b));
    let now_b = ok(
        c.p.open_file(&dir, &name("b")?, FileMode::ReadOnly),
        "open b",
    )?;
    let (r, back) = read_one(&mut q, &now_b, 0, c.buf(&[0; BLOCK])?)?;
    check!(
        r == Ok(BLOCK) && back.is_some_and(|x| x.as_slice() == pattern(6, BLOCK).as_slice()),
        "the target does not hold the renamed file's bytes"
    );
    match c.p.open_file(&dir, &name("a")?, FileMode::ReadOnly) {
        Ok(_) => return Err("the renamed name still exists".into()),
        Err(e) => check!(is_not_found(e), "old name: {e}"),
    }
    drop(now_b);
    ok(c.p.unlink(&dir, &name("b")?), "unlink")?;
    ok(c.p.sync_dir(&dir), "sync_dir after unlink")?;
    match c.p.open_file(&dir, &name("b")?, FileMode::ReadOnly) {
        Ok(_) => Err("an unlinked file still opens".into()),
        Err(e) => {
            check!(is_not_found(e), "unlinked: {e}");
            Ok(())
        }
    }
}

fn ownership_lock<P: Platform>(c: &Ctx<'_, P>) -> Check {
    let dir = c.dir()?;
    let f = c.file(&dir, "l", BLOCK as u64)?;
    let held = ok(c.p.lock_exclusive(&f), "first lock")?;
    let other = ok(
        c.p.open_file(&dir, &name("l")?, FileMode::ReadWrite),
        "second open",
    )?;
    check!(
        c.p.lock_exclusive(&other).is_err(),
        "a second holder took the lock"
    );
    drop(held);
    drop(f);
    ok(c.p.lock_exclusive(&other), "lock after release").map(|_| ())
}

fn written_range_state<P: Platform>(c: &Ctx<'_, P>) -> Check {
    let dir = c.dir()?;
    let len = 16 * BLOCK;
    let f = c.file(&dir, "w", len as u64)?;
    let mut q = c.queue(4)?;
    let r = write_one(&mut q, &f, 0, c.buf(&pattern(8, len))?, false)?;
    check!(r == Ok(len), "full write: {r:?}");
    ok(c.p.flush_all(&f), "flush_all")?;
    let s = ok(c.p.range_state(&f, 0, len as u64), "range_state")?;
    check!(
        s.unwritten != Tri::Yes,
        "a written range reports unwritten extents"
    );
    check!(s.shared != Tri::Yes, "a fresh file reports shared extents");
    check!(
        s.cached_pages.is_none_or(|n| n == 0),
        "direct writes left {:?} cached pages",
        s.cached_pages
    );
    check!(
        s.valid_data != Tri::No,
        "the valid data length does not cover a written range"
    );
    Ok(())
}

fn release_keeps_size<P: Platform>(c: &Ctx<'_, P>) -> Check {
    let dir = c.dir()?;
    let len = 32 * BLOCK as u64;
    let f = c.file(&dir, "x", len)?;
    let mut q = c.queue(4)?;
    let _w = write_one(&mut q, &f, 0, c.buf(&pattern(9, 32 * BLOCK))?, false)?;
    ok(c.p.flush_all(&f), "flush_all")?;
    ok(
        c.p.release_range(
            &f,
            8 * BLOCK as u64,
            8 * BLOCK as u64,
            ReleaseHow::Deallocate,
        ),
        "release",
    )?;
    check!(
        ok(c.p.size(&f), "size")? == len,
        "release changed the file size"
    );
    let (r, back) = read_one(&mut q, &f, 0, c.buf(&[0; BLOCK])?)?;
    check!(
        r == Ok(BLOCK) && back.is_some_and(|b| b.as_slice() == &pattern(9, 32 * BLOCK)[..BLOCK]),
        "release changed bytes outside its range"
    );
    Ok(())
}

fn probe_is_honest<P: Platform>(c: &Ctx<'_, P>) -> Check {
    let dir = c.dir()?;
    let f = c.file(&dir, "p", BLOCK as u64)?;
    let ev = ok(c.p.probe(&dir, &f), "probe")?;
    let a = ev.fs.dio_offset_align;
    let m = ev.fs.dio_mem_align;
    check!(
        a.is_power_of_two() && a as usize <= BLOCK,
        "direct-I/O offset alignment {a}"
    );
    check!(
        m.is_power_of_two() && m as usize <= BLOCK,
        "direct-I/O memory alignment {m}"
    );
    if ev.device.cache_present == Tri::Unknown {
        check!(
            ev.missing.contains(Missing::CachePresence),
            "an unknown write cache is not listed as missing"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Store behaviour on the platform
// ---------------------------------------------------------------------------

fn opts(trust: Trust) -> StoreOptions {
    StoreOptions {
        trust,
        queues: 2,
        buffers_per_class: 16,
        max_io: 1 << 17,
        ..StoreOptions::default()
    }
}

fn store_round_trip<P: Platform + Clone>(c: &Ctx<'_, P>) -> Check
where
    P::Queue: Send,
{
    let s = done(Store::create(c.p.clone(), &c.path, opts(c.trust)), "create")?;
    let wal = done(s.provision_append_region("wal", 4 << 20), "provision wal")?;
    let big = pattern(10, 600_000);
    let (pos, receipt) = done(wal.append_durable(&big), "append")?;
    check!(
        receipt.durable_through() >= pos.offset() + big.len() as u64,
        "receipt short"
    );
    let mut batch = wal.batch();
    for i in 0..50u8 {
        let _at = done(batch.append(&pattern(i, 100)), "batch append")?;
    }
    let (bpos, _r) = done(batch.commit(), "batch commit")?;
    let pages = done(s.provision_page_region("pages", 1 << 20), "provision pages")?;
    let _p = done(
        pages.write_durable(
            done(pages.pos(4 * BLOCK as u64), "pos")?,
            &pattern(11, BLOCK),
        ),
        "page write",
    )?;
    let slot = done(s.provision_slot("manifest"), "provision slot")?;
    let _c = done(slot.commit(b"v1"), "slot commit")?;
    drop((batch, wal, pages, slot, s));
    let s = done(Store::open(c.p.clone(), &c.path, opts(c.trust)), "reopen")?;
    let wal = done(s.append_region("wal"), "wal lookup")?;
    let mut back = vec![0; big.len()];
    let _n = done(wal.read(pos.offset(), &mut back), "read")?;
    check!(back == big, "append did not round-trip");
    let mut first = vec![0; 100];
    let _n = done(wal.read(bpos.offset(), &mut first), "read batch")?;
    check!(first == pattern(0, 100), "batch did not round-trip");
    check!(
        done(s.slot("manifest"), "slot lookup")?.read().as_deref() == Some(b"v1".as_slice()),
        "slot did not round-trip"
    );
    let pages = done(s.page_region("pages"), "pages lookup")?;
    let mut page = vec![0; BLOCK];
    let _n = done(
        pages.read(done(pages.pos(4 * BLOCK as u64), "pos")?, &mut page),
        "page read",
    )?;
    check!(page == pattern(11, BLOCK), "page did not round-trip");
    check!(
        wal.append(b"x").is_err(),
        "a reopened append region accepted an unpositioned append"
    );
    let _at = done(wal.resume_at(bpos.offset() + 50 * 100), "resume_at")?;
    let _t = done(wal.append_durable(b"after"), "append after resume")?;
    Ok(())
}

/// Every byte of the container, read through the platform.
fn container_bytes<P: Platform>(c: &Ctx<'_, P>) -> Result<Vec<u8>, String> {
    let dir = c.dir()?;
    let f = ok(
        c.p.open_file(&dir, &name(CONTAINER)?, FileMode::ReadOnly),
        "open container",
    )?;
    let len = usize::try_from(ok(c.p.size(&f), "size")?).map_err(|_| "container too large")?;
    let mut q = c.queue(2)?;
    let mut out = Vec::with_capacity(len);
    let chunk = 1 << 16;
    while out.len() < len {
        let want = (len - out.len()).min(chunk);
        let padded = want.div_ceil(BLOCK) * BLOCK;
        let (r, back) = read_one(&mut q, &f, out.len() as u64, c.buf(&vec![0; padded])?)?;
        let n = ok(r, "read container")?.min(want);
        let b = back.ok_or("no buffer")?;
        out.extend_from_slice(&b.as_slice()[..n]);
        check!(n > 0, "short read of the container at {}", out.len());
    }
    Ok(out)
}

fn read_only_open_changes_nothing<P: Platform + Clone>(c: &Ctx<'_, P>) -> Check
where
    P::Queue: Send,
{
    let s = done(Store::create(c.p.clone(), &c.path, opts(c.trust)), "create")?;
    let wal = done(s.provision_append_region("wal", 1 << 20), "provision")?;
    let _r = done(wal.append_durable(&pattern(12, 70_000)), "append")?;
    drop((wal, s));
    let before = container_bytes(c)?;
    let ro = done(
        Store::open_readonly(c.p.clone(), &c.path, opts(c.trust)),
        "open_readonly",
    )?;
    let wal = done(ro.append_region("wal"), "lookup")?;
    let _s = done(
        wal.scan(0, |_: ScanItem<'_>| ControlFlow::Continue(())),
        "scan",
    )?;
    check!(
        wal.append(b"x").is_err(),
        "a read-only store accepted an append"
    );
    drop((wal, ro));
    let after = container_bytes(c)?;
    check!(
        before.len() == after.len(),
        "a read-only open changed the size"
    );
    check!(before == after, "a read-only open changed the bytes");
    Ok(())
}

fn second_open_is_locked<P: Platform + Clone>(c: &Ctx<'_, P>) -> Check
where
    P::Queue: Send,
{
    let s = done(Store::create(c.p.clone(), &c.path, opts(c.trust)), "create")?;
    match Store::open(c.p.clone(), &c.path, opts(c.trust)) {
        Ok(_) => return Err("a second writable open succeeded".into()),
        Err(e) => check!(matches!(e, Error::Locked), "second open: {e}"),
    }
    drop(s);
    done(
        Store::open(c.p.clone(), &c.path, opts(c.trust)),
        "open after close",
    )
    .map(|_| ())
}

fn directory_replace<P: Platform + Clone>(c: &Ctx<'_, P>) -> Check {
    let d = done(Directory::open(c.p.clone(), &c.path, true, c.trust), "open")?;
    for (i, len) in [0usize, 1, 4095, 4096, 70_000].into_iter().enumerate() {
        let bytes = pattern(i as u8, len);
        done(d.replace("CURRENT", &bytes), "replace")?;
        check!(
            done(d.read("CURRENT"), "read")? == bytes,
            "replace of {len} bytes did not round-trip"
        );
    }
    done(d.remove("CURRENT"), "remove")?;
    check!(
        matches!(d.read("CURRENT"), Err(Error::NotFound { .. })),
        "a removed file still reads"
    );
    Ok(())
}
