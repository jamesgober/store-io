//! Stores on several devices (R5.3). Dependent writes across regions,
//! stores and opens: a dependency on an append region must be a prefix
//! receipt, whichever store it is from; a dependency on a reachable store
//! must name a live generation of its region, and one from an earlier open
//! must lie below where this open resumed it. Every refusal comes before
//! anything is reserved or written. `StoreSet`: one flush per store per
//! barrier, receipts by each store's kind, a store's failure its own.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_results,
    unused_must_use
)]

use std::path::{Path, PathBuf};

use store_io_core::error::{Error, NotWrittenCause};
use store_io_engine::completion::Done;
use store_io_engine::{
    DurableReceipt, Store, StoreOptions, StoreSet, SubmitterConfig, WriteMode, WriteTicket,
};
use store_io_sim::{CrashMode, DeviceKind, SimConfig, SimPlatform};

const DIR: &str = "/db";
const B: usize = 4096;
const MIB: u64 = 1 << 20;

fn create(seed: u64) -> (SimPlatform, Store<SimPlatform>) {
    let p = SimPlatform::new(SimConfig::volatile(seed));
    let s = Store::create(p.clone(), Path::new(DIR), StoreOptions::default()).unwrap();
    (p, s)
}

fn reopen(p: &SimPlatform) -> Store<SimPlatform> {
    Store::open(p.clone(), Path::new(DIR), StoreOptions::default()).unwrap()
}

/// Copies the container in `DIR` to `to` byte for byte, as a tool other
/// than store-io would: the copy has the same volume id.
fn copy_container(p: &SimPlatform, to: &str) {
    use store_io_buf::{BufPool, PoolConfig};
    use store_io_platform::{CompletionBuf, FileName, IoOp, Platform, Queue, QueueConfig};
    let bytes = p.with_world(|w| {
        w.media_of(&PathBuf::from(DIR), store_io_engine::CONTAINER)
            .unwrap()
            .to_vec()
    });
    let dir = p.open_dir(Path::new(to), true).unwrap();
    let file = p
        .create_file(&dir, &FileName::new(store_io_engine::CONTAINER).unwrap())
        .unwrap();
    let pool = BufPool::new(&PoolConfig::uniform(B, B, B, 2)).unwrap();
    let mut q = p.queue(QueueConfig::default()).unwrap();
    for (i, chunk) in bytes.chunks(B).enumerate() {
        let mut buf = pool.take(B).unwrap();
        buf.as_mut_slice()[..chunk.len()].copy_from_slice(chunk);
        q.submit(
            IoOp::Write {
                file: &file,
                offset: (i * B) as u64,
                buf,
                dsync: false,
            },
            1,
        )
        .unwrap();
        let mut out = CompletionBuf::with_capacity(2);
        q.wait(1, &mut out).unwrap();
    }
    p.flush_all(&file).unwrap();
    p.sync_dir(&dir).unwrap();
}

fn cause(r: Result<impl core::fmt::Debug, Error>) -> NotWrittenCause {
    match r {
        Err(Error::NotWritten { cause, .. }) => cause,
        other => panic!("expected NotWritten, got {other:?}"),
    }
}

/// A receipt for one append's range, not a prefix: a `Submitter` barrier's.
fn range_receipt(s: &Store<SimPlatform>, name: &str) -> DurableReceipt {
    let data = s.provision_append_region(name, MIB).unwrap();
    let t = data.append(b"range").unwrap();
    let mut sub = s.submitter(SubmitterConfig::default()).unwrap();
    sub.barrier(&t, 7).unwrap();
    let c = sub.wait(1).unwrap().next().unwrap();
    match c.outcome {
        Ok(Done::Synced(r)) => {
            assert!(!r.is_prefix());
            r
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn test_a_dependent_write_with_prefix_receipts_is_written_and_durable() {
    let (_p, s) = create(1);
    let data = s.provision_append_region("data", MIB).unwrap();
    let pages = s.provision_page_region("pages", MIB).unwrap();
    let log = s.provision_append_region("log", MIB).unwrap();
    let (_at, d1) = data.append_durable(b"record").unwrap();
    let d2 = pages
        .write_durable(pages.pos(0).unwrap(), &[3u8; B])
        .unwrap();
    let t = log.append_after(&[d1, d2], b"pointer").unwrap();
    let r = log.sync_through(&t).unwrap();
    assert!(r.covers(&t));
    let t = pages
        .write_after(&[d1, r], pages.pos(B as u64).unwrap(), &[4u8; B])
        .unwrap();
    assert!(pages.sync_through(&t).unwrap().covers(&t));
    // No dependency is a plain write.
    let t = log.append_after(&[], b"none").unwrap();
    assert!(log.sync_through(&t).unwrap().covers(&t));
    let mut got = [0u8; 7];
    log.read(0, &mut got).unwrap();
    assert_eq!(&got, b"pointer");
}

/// The receipt for one append's range is refused on its own store and on
/// any other (the receipt carries its region's kind); a page receipt of
/// another store is accepted.
#[test]
fn test_a_range_receipt_of_an_append_is_refused_from_any_store() {
    let (_pa, a) = create(2);
    let (_pb, b) = create(3);
    let r = range_receipt(&a, "data");
    let a_log = a.provision_append_region("log", MIB).unwrap();
    let b_log = b.provision_append_region("log", MIB).unwrap();
    let b_pages = b.provision_page_region("pages", MIB).unwrap();
    assert_eq!(
        cause(a_log.append_after(&[r], b"p")),
        NotWrittenCause::NotPrefix
    );
    assert_eq!(
        cause(b_log.append_after(&[r], b"p")),
        NotWrittenCause::NotPrefix
    );
    assert_eq!(
        cause(b_pages.write_after(&[r], b_pages.pos(0).unwrap(), &[1u8; B])),
        NotWrittenCause::NotPrefix
    );
    assert_eq!((a_log.tail(), b_log.tail()), (0, 0), "nothing reserved");
    // A page receipt is never a prefix and needs none.
    let a_pages = a.provision_page_region("pages", MIB).unwrap();
    let pr = a_pages
        .write_durable(a_pages.pos(0).unwrap(), &[2u8; B])
        .unwrap();
    assert!(!pr.is_prefix());
    let t = b_log.append_after(&[pr], b"p").unwrap();
    assert!(b_log.sync_through(&t).unwrap().covers(&t));
}

/// A dependency's region recycled since is refused, before anything is
/// reserved; so is one released (its region is gone).
#[test]
fn test_a_dependency_recycled_or_released_since_is_refused() {
    let (_p, s) = create(4);
    let data = s.provision_append_region("data", MIB).unwrap();
    let pages = s.provision_page_region("pages", MIB).unwrap();
    let gone = s.provision_page_region("gone", MIB).unwrap();
    let log = s.provision_append_region("log", MIB).unwrap();
    let (_at, d) = data.append_durable(b"old").unwrap();
    let pr = pages
        .write_durable(pages.pos(0).unwrap(), &[5u8; B])
        .unwrap();
    let g = gone.write_durable(gone.pos(0).unwrap(), &[6u8; B]).unwrap();
    let _data = data.recycle().unwrap();
    let pages = pages.recycle().unwrap();
    gone.release().unwrap();
    assert_eq!(
        cause(log.append_after(&[d], b"p")),
        NotWrittenCause::StaleGeneration
    );
    assert_eq!(
        cause(log.append_after(&[pr], b"p")),
        NotWrittenCause::StaleGeneration
    );
    assert_eq!(
        cause(log.append_after(&[g], b"p")),
        NotWrittenCause::NotReady
    );
    assert_eq!(
        cause(pages.write_after(&[d], pages.pos(0).unwrap(), &[7u8; B])),
        NotWrittenCause::StaleGeneration
    );
    assert_eq!(log.tail(), 0, "nothing reserved");
    assert!(s.poisoned().is_none());
}

/// A receipt of an earlier open is still this store's: refused while its
/// append region is not positioned, and once any open since resumed it below
/// the receipt's end (that open's appends overwrite from there), however
/// far later opens resume; accepted while every open since resumed at or
/// past it.
#[test]
fn test_an_earlier_opens_receipt_is_checked_against_where_its_region_resumed() {
    let (p, s) = create(5);
    let data = s.provision_append_region("data", MIB).unwrap();
    s.provision_page_region("ptr", MIB).unwrap();
    data.append(&[1u8; B]).unwrap();
    let (_at, r) = data.append_durable(&[2u8; B]).unwrap();
    assert_eq!(r.durable_through(), 2 * B as u64);
    drop((data, s));

    let s = reopen(&p);
    let ptr = s.page_region("ptr").unwrap();
    let data = s.append_region("data").unwrap();
    let at = ptr.pos(0).unwrap();
    assert_eq!(
        cause(ptr.write_after(&[r], at, &[9u8; B])),
        NotWrittenCause::NotPositioned
    );
    assert_eq!(data.resume_at(B as u64).unwrap(), B as u64);
    // Moving on later changes nothing: the bytes between were appended
    // over.
    data.append(&[3u8; B]).unwrap();
    assert_eq!(data.resume_at(4 * B as u64).unwrap(), 4 * B as u64);
    assert_eq!(
        cause(ptr.write_after(&[r], at, &[9u8; B])),
        NotWrittenCause::StaleGeneration
    );
    drop((data, ptr, s));

    // An open after the one that appended over the receipt's bytes,
    // resuming past them, still refuses it.
    let s = reopen(&p);
    let ptr = s.page_region("ptr").unwrap();
    let data = s.append_region("data").unwrap();
    data.resume_at(r.durable_through()).unwrap();
    assert_eq!(
        cause(ptr.write_after(&[r], at, &[9u8; B])),
        NotWrittenCause::StaleGeneration
    );
}

/// Opens that each resumed at or past a receipt's end keep it good, two
/// opens on; the receipt's own open's resumes do not count against it.
#[test]
fn test_an_earlier_opens_receipt_survives_opens_that_resumed_past_it() {
    let (p, s) = create(11);
    let data = s.provision_append_region("data", MIB).unwrap();
    s.provision_page_region("ptr", MIB).unwrap();
    // Resumed in its own open, below where its receipt ends.
    assert_eq!(data.resume_at(B as u64).unwrap(), B as u64);
    let (_at, r) = data.append_durable(&[2u8; B]).unwrap();
    assert_eq!(r.durable_through(), 2 * B as u64);
    drop((data, s));
    for _ in 0..2 {
        let s = reopen(&p);
        let data = s.append_region("data").unwrap();
        data.resume_at(r.durable_through()).unwrap();
        let ptr = s.page_region("ptr").unwrap();
        let t = ptr
            .write_after(&[r], ptr.pos(0).unwrap(), &[9u8; B])
            .unwrap();
        assert!(ptr.sync_through(&t).unwrap().covers(&t));
    }
}

/// An open this process did not see (another process's, made here with
/// `test_hook::unseen`) fences off every append-region receipt of earlier
/// opens, even one that resumed past them: it left the file's metadata
/// sequence where this process did not. Page receipts stand (a recycle is
/// on disk).
#[test]
fn test_an_open_this_process_did_not_see_fences_earlier_append_receipts() {
    let (p, s) = create(13);
    let data = s.provision_append_region("data", MIB).unwrap();
    let pages = s.provision_page_region("pages", MIB).unwrap();
    let (_at, r) = data.append_durable(&[1u8; B]).unwrap();
    let pr = pages
        .write_durable(pages.pos(0).unwrap(), &[2u8; B])
        .unwrap();
    drop((data, pages, s));
    store_io_engine::test_hook::unseen(|| {
        let s = reopen(&p);
        let data = s.append_region("data").unwrap();
        data.resume_at(0).unwrap();
        data.append(&[3u8; B]).unwrap();
    });
    let s = reopen(&p);
    let data = s.append_region("data").unwrap();
    data.resume_at(r.durable_through()).unwrap();
    let pages = s.page_region("pages").unwrap();
    let at = pages.pos(B as u64).unwrap();
    assert_eq!(
        cause(pages.write_after(&[r], at, &[4u8; B])),
        NotWrittenCause::StaleGeneration
    );
    let t = pages.write_after(&[pr], at, &[4u8; B]).unwrap();
    assert!(pages.sync_through(&t).unwrap().covers(&t));
    // The fence holds for later opens too; receipts of opens after it are
    // vouched for again.
    let (_at, r2) = data.append_durable(&[5u8; B]).unwrap();
    drop((data, pages, s));
    let s = reopen(&p);
    s.append_region("data")
        .unwrap()
        .resume_at(r2.durable_through())
        .unwrap();
    let pages = s.page_region("pages").unwrap();
    assert_eq!(
        cause(pages.write_after(&[r], at, &[4u8; B])),
        NotWrittenCause::StaleGeneration
    );
    let t = pages.write_after(&[r2], at, &[4u8; B]).unwrap();
    assert!(pages.sync_through(&t).unwrap().covers(&t));
}

/// A container copied by another tool has the same volume id: a receipt
/// of the original is the copy's only if the open it came from was of the
/// copy's file. It is not, so to the copy it is another store's receipt,
/// checked only for being a prefix (here accepted although the copy's
/// region is not positioned); the original's own later open still knows it.
#[test]
fn test_a_receipt_of_a_copied_containers_original_is_another_stores() {
    let (p, s) = create(15);
    let data = s.provision_append_region("data", MIB).unwrap();
    s.provision_page_region("ptr", MIB).unwrap();
    let (_at, r) = data.append_durable(&[1u8; B]).unwrap();
    drop((data, s));
    copy_container(&p, "/copy");
    let copy = Store::open(p.clone(), Path::new("/copy"), StoreOptions::default()).unwrap();
    assert_eq!(copy.volume(), r.volume());
    let ptr = copy.page_region("ptr").unwrap();
    let t = ptr
        .write_after(&[r], ptr.pos(0).unwrap(), &[9u8; B])
        .unwrap();
    assert!(ptr.sync_through(&t).unwrap().covers(&t));
    drop((ptr, copy));
    let s = reopen(&p);
    let ptr = s.page_region("ptr").unwrap();
    assert_eq!(
        cause(ptr.write_after(&[r], ptr.pos(0).unwrap(), &[9u8; B])),
        NotWrittenCause::NotPositioned
    );
}

/// A receipt of an open this process has no record of (made here unseen)
/// cannot be placed: refused, never taken for another store's.
#[test]
fn test_a_receipt_of_an_unrecorded_open_is_refused() {
    let p = SimPlatform::new(SimConfig::volatile(16));
    let r = store_io_engine::test_hook::unseen(|| {
        let s = Store::create(p.clone(), Path::new(DIR), StoreOptions::default()).unwrap();
        s.provision_page_region("ptr", MIB).unwrap();
        let pages = s.provision_page_region("pages", MIB).unwrap();
        pages
            .write_durable(pages.pos(0).unwrap(), &[1u8; B])
            .unwrap()
    });
    let s = reopen(&p);
    let ptr = s.page_region("ptr").unwrap();
    assert_eq!(
        cause(ptr.write_after(&[r], ptr.pos(0).unwrap(), &[9u8; B])),
        NotWrittenCause::StaleGeneration
    );
}

/// A close that wrote no clean-close record (a poisoned store) leaves the
/// file's state unknown to this process: the next open fences off the
/// earlier receipts.
#[test]
fn test_a_poisoned_close_fences_earlier_append_receipts() {
    let (p, s) = create(14);
    let data = s.provision_append_region("data", MIB).unwrap();
    s.provision_page_region("ptr", MIB).unwrap();
    let (_at, r) = data.append_durable(&[1u8; B]).unwrap();
    s.poison();
    drop((data, s));
    let s = reopen(&p);
    s.append_region("data")
        .unwrap()
        .resume_at(r.durable_through())
        .unwrap();
    let ptr = s.page_region("ptr").unwrap();
    assert_eq!(
        cause(ptr.write_after(&[r], ptr.pos(0).unwrap(), &[9u8; B])),
        NotWrittenCause::StaleGeneration
    );
}

/// An earlier open's receipt whose region was recycled in this open is
/// refused: matched to the store by its volume alone, its region's
/// generation read from the region as it stands.
#[test]
fn test_an_earlier_opens_receipt_of_a_region_recycled_since_is_refused() {
    let (p, s) = create(6);
    let data = s.provision_append_region("data", MIB).unwrap();
    s.provision_page_region("ptr", MIB).unwrap();
    let (_at, r) = data.append_durable(b"old").unwrap();
    drop((data, s));
    let s = reopen(&p);
    let data = s.append_region("data").unwrap();
    data.resume_at(r.durable_through()).unwrap();
    let _next = data.recycle().unwrap();
    let ptr = s.page_region("ptr").unwrap();
    assert_eq!(
        cause(ptr.write_after(&[r], ptr.pos(0).unwrap(), &[1u8; B])),
        NotWrittenCause::StaleGeneration
    );
}

/// An earlier open's page receipt needs no resume: page regions are not
/// appended over.
#[test]
fn test_an_earlier_opens_page_receipt_is_accepted() {
    let (p, s) = create(7);
    let pages = s.provision_page_region("pages", MIB).unwrap();
    s.provision_append_region("log", MIB).unwrap();
    let pr = pages
        .write_durable(pages.pos(0).unwrap(), &[8u8; B])
        .unwrap();
    drop((pages, s));
    let s = reopen(&p);
    let log = s.append_region("log").unwrap();
    log.resume_at(0).unwrap();
    let t = log.append_after(&[pr], b"p").unwrap();
    assert!(log.sync_through(&t).unwrap().covers(&t));
}

/// A receipt from a store poisoned since still proves its write (R5.15):
/// the dependent write on another store proceeds.
#[test]
fn test_a_receipt_from_a_store_poisoned_since_still_counts() {
    let (_pa, a) = create(8);
    let (_pb, b) = create(9);
    let data = a.provision_append_region("data", MIB).unwrap();
    let (_at, r) = data.append_durable(b"kept").unwrap();
    a.poison();
    let log = b.provision_append_region("log", MIB).unwrap();
    let t = log.append_after(&[r], b"p").unwrap();
    assert!(log.sync_through(&t).unwrap().covers(&t));
}

/// A log pointing into its own durable prefix (the usual write-ahead log
/// pattern: its own region's gate entered twice), the same receipt named
/// twice, and a slot's commit receipt (whose generation is the slot's
/// commit count) are all accepted.
#[test]
fn test_self_duplicate_and_slot_dependencies() {
    let (_p, s) = create(12);
    let log = s.provision_append_region("log", MIB).unwrap();
    let (_at, own) = log.append_durable(b"first").unwrap();
    let t = log.append_after(&[own, own], b"second").unwrap();
    let r = log.sync_through(&t).unwrap();
    assert!(r.covers(&t));
    let manifest = s.provision_slot("manifest").unwrap();
    manifest.commit(b"v1").unwrap();
    let m = manifest.commit(b"v2").unwrap();
    let t = log.append_after(&[m, r], b"third").unwrap();
    assert!(log.sync_through(&t).unwrap().covers(&t));
}

/// The `Submitter` calls check as the blocking ones do, refusing with the
/// whole buffer and nothing reserved; accepted, they complete as usual.
#[test]
fn test_submitter_dependent_writes() {
    let (_p, s) = create(10);
    let range = range_receipt(&s, "ranged");
    let data = s.provision_append_region("data", MIB).unwrap();
    let pages = s.provision_page_region("pages", MIB).unwrap();
    let log = s.provision_append_region("log", MIB).unwrap();
    let (_at, d) = data.append_durable(b"rec").unwrap();
    let mut sub = s.submitter(SubmitterConfig::default()).unwrap();

    let mut buf = sub.take_buf(B).unwrap();
    buf.as_mut_slice().fill(4);
    let e = sub
        .append_after(&log, &[range], buf, WriteMode::Plain, 1)
        .unwrap_err();
    assert!(
        matches!(
            e.error,
            Error::NotWritten {
                cause: NotWrittenCause::NotPrefix,
                ..
            }
        ),
        "{:?}",
        e.error
    );
    let buf = e.buf.unwrap();
    assert_eq!(buf.as_slice(), &[4u8; B][..]);
    assert_eq!(log.tail(), 0, "nothing reserved");

    let pos = sub
        .append_after(&log, &[d], buf, WriteMode::Durable, 2)
        .unwrap();
    assert_eq!(pos.offset(), 0);
    let c = sub.wait(1).unwrap().next().unwrap();
    assert!(matches!(c.outcome, Ok(Done::Written(_))), "{:?}", c.outcome);

    let stale = d;
    let _data = data.recycle().unwrap();
    let mut buf = c.buf.unwrap();
    buf.as_mut_slice().fill(5);
    let e = sub
        .write_after(
            &pages,
            &[stale],
            pages.pos(0).unwrap(),
            buf,
            WriteMode::Plain,
            3,
        )
        .unwrap_err();
    assert!(
        matches!(
            e.error,
            Error::NotWritten {
                cause: NotWrittenCause::StaleGeneration,
                ..
            }
        ),
        "{:?}",
        e.error
    );
    let buf = e.buf.unwrap();
    assert_eq!(buf.as_slice(), &[5u8; B][..]);
    let pr = pages
        .write_durable(pages.pos(B as u64).unwrap(), &[6u8; B])
        .unwrap();
    sub.write_after(
        &pages,
        &[pr],
        pages.pos(0).unwrap(),
        buf,
        WriteMode::Plain,
        4,
    )
    .unwrap();
    let c = sub.wait(1).unwrap().next().unwrap();
    match c.outcome {
        Ok(Done::Written(w)) => assert!(pages.sync_through(&w.ticket).unwrap().covers(&w.ticket)),
        other => panic!("{other:?}"),
    }
    assert!(s.poisoned().is_none());
}

// ---------------------------------------------------------------- StoreSet

/// A store in `dir` of `p` (on the device `dir` is placed on).
fn create_in(p: &SimPlatform, dir: &str) -> Store<SimPlatform> {
    Store::create(p.clone(), Path::new(dir), StoreOptions::default()).unwrap()
}

/// A world of two devices, the second `second`, with `/b` placed on it.
fn two_devices(seed: u64, second: DeviceKind) -> SimPlatform {
    let p = SimPlatform::new(SimConfig::volatile(seed).with_device(second));
    p.with_world(|w| w.place("/b", 1));
    p
}

fn flushes(s: &Store<SimPlatform>) -> u64 {
    s.domain_stats().flushes
}

/// One set barrier over a flush-required store and a power-safe one, an
/// append and a page write on each: the flush-required store is flushed
/// once and its receipts are synced; the power-safe store is not flushed
/// and its receipts are durable at completion (never read as synced);
/// every receipt covers its ticket.
#[test]
fn test_a_set_barrier_flushes_each_store_once_and_answers_by_its_kind() {
    let p = two_devices(101, DeviceKind::PowerSafe);
    let (a, b) = (create_in(&p, "/a"), create_in(&p, "/b"));
    let set = StoreSet::new([a.clone(), b.clone()]).unwrap();
    assert_eq!(set.len(), 2);
    let (wa, wb) = (
        a.provision_append_region("wal", MIB).unwrap(),
        b.provision_append_region("wal", MIB).unwrap(),
    );
    let (pa, pb) = (
        a.provision_page_region("pages", MIB).unwrap(),
        b.provision_page_region("pages", MIB).unwrap(),
    );
    let tickets = [
        wa.append(&[1u8; B]).unwrap(),
        wb.append(&[2u8; B]).unwrap(),
        pa.write(pa.pos(0).unwrap(), &[3u8; B]).unwrap(),
        pb.write(pb.pos(0).unwrap(), &[4u8; B]).unwrap(),
    ];
    let (fa, fb) = (flushes(&a), flushes(&b));
    let refs: Vec<&WriteTicket> = tickets.iter().collect();
    let receipts = set.barrier(&refs).unwrap();
    assert_eq!(flushes(&a), fa + 1, "one flush of the flush-required store");
    assert_eq!(flushes(&b), fb, "a power-safe store is not flushed");
    for (r, t) in receipts.iter().zip(&tickets) {
        assert!(r.covers(t));
    }
    assert!(receipts[0].is_prefix() && receipts[0].data_synced());
    assert!(receipts[1].is_prefix() && !receipts[1].data_synced());
    assert!(receipts[2].data_synced());
    assert!(!receipts[3].data_synced());
    let stats = set.stats();
    assert_eq!((stats.barriers, stats.stores), (1, 2));
    // Already durable (appends) or covered by a finished flush (pages): a
    // second barrier flushes nothing.
    let again = set.barrier(&refs).unwrap();
    assert_eq!((flushes(&a), flushes(&b)), (fa + 1, fb));
    for (r, t) in again.iter().zip(&tickets) {
        assert!(r.covers(t));
    }
    // Published: the region's own sync needs no flush now.
    assert!(wa.sync_through(&tickets[0]).unwrap().covers(&tickets[0]));
    assert_eq!(flushes(&a), fa + 1);
}

/// Two stores on one device, one ticket each: each store is flushed (a
/// flush is its own file's: the simulator's flushes are not shared), and
/// both writes survive a crash.
#[test]
fn test_two_stores_on_one_device_are_each_flushed() {
    let p = SimPlatform::new(SimConfig::volatile(102));
    let (a, b) = (create_in(&p, "/a"), create_in(&p, "/b"));
    let set = StoreSet::new([a.clone(), b.clone()]).unwrap();
    let (pa, pb) = (
        a.provision_page_region("pages", MIB).unwrap(),
        b.provision_page_region("pages", MIB).unwrap(),
    );
    let ta = pa.write(pa.pos(0).unwrap(), &[5u8; B]).unwrap();
    let tb = pb.write(pb.pos(0).unwrap(), &[6u8; B]).unwrap();
    let (fa, fb) = (flushes(&a), flushes(&b));
    set.barrier(&[&ta, &tb]).unwrap();
    assert_eq!((flushes(&a), flushes(&b)), (fa + 1, fb + 1));
    p.with_world(|w| w.crash(&CrashMode::LoseAll));
    drop((pa, pb, set, a, b));
    for (dir, byte) in [("/a", 5u8), ("/b", 6u8)] {
        let s = Store::open(p.clone(), Path::new(dir), StoreOptions::default()).unwrap();
        let pages = s.page_region("pages").unwrap();
        let mut out = vec![0u8; B];
        pages.read(pages.pos(0).unwrap(), &mut out).unwrap();
        assert_eq!(out, vec![byte; B], "{dir}");
    }
}

/// Tickets are matched to their stores: one of no store in the set fails
/// the call before anything is flushed; a released region's ticket is
/// `NotReady` and a recycled one's `StaleGeneration`, the others' receipts
/// standing.
#[test]
fn test_a_set_barrier_matches_each_ticket_to_its_store() {
    let p = two_devices(103, DeviceKind::VolatileCache);
    let (a, b) = (create_in(&p, "/a"), create_in(&p, "/b"));
    let outside = create_in(&p, "/c");
    let set = StoreSet::new([a.clone(), b.clone()]).unwrap();
    let wa = a.provision_append_region("wal", MIB).unwrap();
    let gone = b.provision_page_region("gone", MIB).unwrap();
    let old = b.provision_append_region("old", MIB).unwrap();
    let stranger = outside.provision_page_region("p", MIB).unwrap();
    let ta = wa.append(&[1u8; B]).unwrap();
    let tg = gone.write(gone.pos(0).unwrap(), &[2u8; B]).unwrap();
    let to = old.append(&[3u8; B]).unwrap();
    let ts = stranger.write(stranger.pos(0).unwrap(), &[4u8; B]).unwrap();
    let (fa, fb) = (flushes(&a), flushes(&b));
    assert_eq!(
        cause(set.barrier_each(&[&ta, &ts])),
        NotWrittenCause::ForeignPosition
    );
    assert_eq!(
        (flushes(&a), flushes(&b)),
        (fa, fb),
        "flushed for a refused call"
    );
    gone.release().unwrap();
    let old = old.recycle().unwrap();
    // The new generation reaches past the old ticket: matched wrongly, the
    // ticket would get a receipt at once.
    old.append(&[9u8; B]).unwrap();
    let each = set.barrier_each(&[&ta, &tg, &to]).unwrap();
    assert!(each[0].as_ref().unwrap().covers(&ta));
    assert_eq!(cause(each[1].clone()), NotWrittenCause::NotReady);
    assert_eq!(cause(each[2].clone()), NotWrittenCause::StaleGeneration);
    assert_eq!(
        cause(set.barrier(&[&ta, &to])),
        NotWrittenCause::StaleGeneration
    );
    drop(old);
}

/// A ticket is its open's: one from before a reopen of a store in the set
/// matches no store of it.
#[test]
fn test_a_ticket_from_before_a_reopen_is_foreign_to_the_set() {
    let (p, a) = create(116);
    let wal = a.provision_append_region("wal", MIB).unwrap();
    let t = wal.append(&[1u8; B]).unwrap();
    wal.sync_through(&t).unwrap();
    drop((wal, a));
    let a = reopen(&p);
    // Resumed past the ticket: matched wrongly, it would get a receipt at
    // once (never wait on bytes this open has not written).
    let wal = a.append_region("wal").unwrap();
    wal.resume_at(t.end()).unwrap();
    let set = StoreSet::new([a]).unwrap();
    assert_eq!(cause(set.barrier(&[&t])), NotWrittenCause::ForeignPosition);
}

/// A set takes each open store once, never two stores with one volume id
/// (a container copied by another tool), and no read-only store.
#[test]
fn test_a_set_refuses_a_store_twice_a_copy_and_a_read_only_store() {
    let (p, a) = create(104);
    a.provision_append_region("wal", MIB).unwrap();
    assert_eq!(
        cause(StoreSet::new([a.clone(), a.clone()])),
        NotWrittenCause::Overlap
    );
    copy_container(&p, "/copy");
    let copy = Store::open(p.clone(), Path::new("/copy"), StoreOptions::default()).unwrap();
    assert_eq!(copy.volume(), a.volume());
    assert_eq!(
        cause(StoreSet::new([a.clone(), copy.clone()])),
        NotWrittenCause::Overlap
    );
    drop(copy);
    let other = create_in(&p, "/other");
    drop(other);
    let ro = Store::open_readonly(p.clone(), Path::new("/other"), StoreOptions::default()).unwrap();
    assert_eq!(
        cause(StoreSet::new([a.clone(), ro])),
        NotWrittenCause::ReadOnly
    );
    let set = StoreSet::new(Vec::<Store<SimPlatform>>::new()).unwrap();
    assert!(set.is_empty());
    assert!(set.barrier(&[]).unwrap().is_empty());
}

/// The set's dependent writes check dependencies on every store of the
/// set: a recycled one is refused, where the region's own `append_after`
/// checks another store's receipt only for being a prefix. A region of a
/// store outside the set is refused before any work.
#[test]
fn test_set_wide_dependent_writes() {
    let p = two_devices(105, DeviceKind::VolatileCache);
    let (a, b) = (create_in(&p, "/a"), create_in(&p, "/b"));
    let outside = create_in(&p, "/c");
    let set = StoreSet::new([a.clone(), b.clone()]).unwrap();
    let data = a.provision_append_region("data", MIB).unwrap();
    let ptr = b.provision_append_region("ptr", MIB).unwrap();
    let pages = b.provision_page_region("pages", MIB).unwrap();
    let (_at, r) = data.append_durable(&[1u8; B]).unwrap();
    let t = set.append_after(&ptr, &[r], &[2u8; B]).unwrap();
    let t2 = set
        .write_after(&pages, &[r], pages.pos(0).unwrap(), &[3u8; B])
        .unwrap();
    for r in set.barrier(&[&t, &t2]).unwrap().iter().zip([&t, &t2]) {
        assert!(r.0.covers(r.1));
    }
    assert_eq!(
        cause(set.append_after(&ptr, &[range_receipt(&a, "range")], &[2u8; B])),
        NotWrittenCause::NotPrefix
    );
    let foreign = outside.provision_append_region("x", MIB).unwrap();
    assert_eq!(
        cause(set.append_after(&foreign, &[r], &[2u8; B])),
        NotWrittenCause::ForeignPosition
    );
    let foreign_pages = outside.provision_page_region("y", MIB).unwrap();
    assert_eq!(
        cause(set.write_after(
            &foreign_pages,
            &[r],
            foreign_pages.pos(0).unwrap(),
            &[3u8; B]
        )),
        NotWrittenCause::ForeignPosition
    );
    let _data = data.recycle().unwrap();
    assert_eq!(
        cause(set.append_after(&ptr, &[r], &[2u8; B])),
        NotWrittenCause::StaleGeneration
    );
    assert_eq!(
        cause(set.write_after(&pages, &[r], pages.pos(0).unwrap(), &[3u8; B])),
        NotWrittenCause::StaleGeneration
    );
    // Without the set, store a is out of reach: only the prefix is checked.
    ptr.append_after(&[r], &[2u8; B]).unwrap();
}

/// A receipt from before a reopen of a store in the set is still that
/// store's (matched by volume) and checked: refused once its region was
/// recycled.
#[test]
fn test_a_set_checks_a_receipt_from_before_a_reopen() {
    let p = two_devices(106, DeviceKind::VolatileCache);
    let a = create_in(&p, "/a");
    let data = a.provision_append_region("data", MIB).unwrap();
    let (_at, r) = data.append_durable(&[1u8; B]).unwrap();
    drop((data, a));
    let a = Store::open(p.clone(), Path::new("/a"), StoreOptions::default()).unwrap();
    let b = create_in(&p, "/b");
    let ptr = b.provision_append_region("ptr", MIB).unwrap();
    let set = StoreSet::new([a.clone(), b.clone()]).unwrap();
    let data = a.append_region("data").unwrap();
    assert_eq!(
        cause(set.append_after(&ptr, &[r], &[2u8; B])),
        NotWrittenCause::NotPositioned
    );
    data.resume_at(r.durable_through()).unwrap();
    set.append_after(&ptr, &[r], &[2u8; B]).unwrap();
    let _data = data.recycle().unwrap();
    assert_eq!(
        cause(set.append_after(&ptr, &[r], &[2u8; B])),
        NotWrittenCause::StaleGeneration
    );
}

/// A submitter's dependent writes through a set check dependencies on every
/// store of it, and are refused unless the submitter's own store is in it.
#[test]
fn test_submitter_dependent_writes_through_a_set() {
    let p = two_devices(107, DeviceKind::VolatileCache);
    let (a, b) = (create_in(&p, "/a"), create_in(&p, "/b"));
    let outside = create_in(&p, "/c");
    let set = StoreSet::new([a.clone(), b.clone()]).unwrap();
    let data = a.provision_append_region("data", MIB).unwrap();
    let ptr = b.provision_append_region("ptr", MIB).unwrap();
    let pages = b.provision_page_region("pages", MIB).unwrap();
    let (_at, r) = data.append_durable(&[1u8; B]).unwrap();
    let mut sub = b.submitter(SubmitterConfig::default()).unwrap();
    let buf = sub.take_buf(B).unwrap();
    sub.append_after_in(&set, &ptr, &[r], buf, WriteMode::Durable, 1)
        .unwrap();
    let buf = sub.take_buf(B).unwrap();
    sub.write_after_in(
        &set,
        &pages,
        &[r],
        pages.pos(0).unwrap(),
        buf,
        WriteMode::Durable,
        2,
    )
    .unwrap();
    let mut done = 0;
    while done < 2 {
        for c in sub.wait(1).unwrap() {
            assert!(matches!(c.outcome, Ok(Done::Written(_))), "{:?}", c.outcome);
            done += 1;
        }
    }
    let _data = data.recycle().unwrap();
    let buf = sub.take_buf(B).unwrap();
    let refused = sub
        .append_after_in(&set, &ptr, &[r], buf, WriteMode::Durable, 3)
        .unwrap_err();
    assert_eq!(
        cause(Err::<(), _>(refused.error)),
        NotWrittenCause::StaleGeneration
    );
    assert!(refused.buf.is_some());
    let buf = sub.take_buf(B).unwrap();
    let refused = sub
        .write_after_in(
            &set,
            &pages,
            &[r],
            pages.pos(0).unwrap(),
            buf,
            WriteMode::Durable,
            5,
        )
        .unwrap_err();
    assert_eq!(
        cause(Err::<(), _>(refused.error)),
        NotWrittenCause::StaleGeneration
    );
    assert!(refused.buf.is_some());
    let other = outside.provision_append_region("x", MIB).unwrap();
    let mut foreign = outside.submitter(SubmitterConfig::default()).unwrap();
    let buf = foreign.take_buf(B).unwrap();
    let refused = foreign
        .append_after_in(&set, &other, &[], buf, WriteMode::Durable, 4)
        .unwrap_err();
    assert_eq!(
        cause(Err::<(), _>(refused.error)),
        NotWrittenCause::ForeignPosition
    );
    assert!(refused.buf.is_some());
    let other_pages = outside.provision_page_region("y", MIB).unwrap();
    let buf = foreign.take_buf(B).unwrap();
    let refused = foreign
        .write_after_in(
            &set,
            &other_pages,
            &[],
            other_pages.pos(0).unwrap(),
            buf,
            WriteMode::Durable,
            6,
        )
        .unwrap_err();
    assert_eq!(
        cause(Err::<(), _>(refused.error)),
        NotWrittenCause::ForeignPosition
    );
    assert!(refused.buf.is_some());
}

/// Distinctness is read at each call, never cached: a device whose identity
/// changes between `new` and `distinct` is seen as changed. Incomplete
/// identities are never distinct.
#[test]
fn test_distinct_is_read_afresh() {
    let p = two_devices(108, DeviceKind::VolatileCache);
    let (a, b) = (create_in(&p, "/a"), create_in(&p, "/b"));
    let set = StoreSet::new([a.clone(), b.clone()]).unwrap();
    assert!(
        !set.distinct(0, 1),
        "incomplete identities are not distinct"
    );
    let identity = |d: u32| store_io_core::identity::DeviceIdentity {
        devices: vec![store_io_core::identity::DeviceId::node(
            store_io_core::identity::DeviceNode::Sim { device: d },
        )],
        complete: true,
    };
    let with = |d: usize, id| {
        let mut ev = a.report().evidence.clone();
        ev.device.identity = id;
        p.with_world(|w| w.set_device_evidence(d, Some(ev)));
    };
    with(0, identity(0));
    with(1, identity(1));
    assert!(set.distinct(0, 1));
    with(1, identity(0));
    assert!(!set.distinct(0, 1), "a changed identity was not read");
}

/// A power-safe store with a region forcing data sync (D-102): that
/// region's tickets are flushed and get synced receipts; another region's
/// are durable at completion.
#[test]
fn test_a_forced_region_on_a_power_safe_store_in_a_set() {
    let p = SimPlatform::new(SimConfig::power_safe(109));
    let a = create_in(&p, "/a");
    let set = StoreSet::new([a.clone()]).unwrap();
    let forced = a.provision_append_region("forced", MIB).unwrap();
    forced.set_force_data_sync(true);
    let plain = a.provision_page_region("plain", MIB).unwrap();
    let tf = forced.append(&[1u8; B]).unwrap();
    let tp = plain.write(plain.pos(0).unwrap(), &[2u8; B]).unwrap();
    let f0 = flushes(&a);
    let r = set.barrier(&[&tf, &tp]).unwrap();
    assert_eq!(flushes(&a), f0 + 1);
    assert!(r[0].covers(&tf) && r[0].data_synced() && r[0].is_prefix());
    assert!(r[1].covers(&tp) && !r[1].data_synced());
    // A forced page region alone is flushed too.
    let forced_pages = a.provision_page_region("forced-pages", MIB).unwrap();
    forced_pages.set_force_data_sync(true);
    let tq = forced_pages
        .write(forced_pages.pos(0).unwrap(), &[3u8; B])
        .unwrap();
    let f1 = flushes(&a);
    let r = set.barrier(&[&tq]).unwrap();
    assert_eq!(flushes(&a), f1 + 1);
    assert!(r[0].covers(&tq) && r[0].data_synced());
}

/// Two threads calling one set at once each get receipts for their own
/// tickets only, every one covering its ticket.
#[test]
fn test_two_callers_of_one_set() {
    let p = two_devices(110, DeviceKind::VolatileCache);
    let (a, b) = (create_in(&p, "/a"), create_in(&p, "/b"));
    let set = StoreSet::new([a.clone(), b.clone()]).unwrap();
    let regions = [
        (
            a.provision_append_region("w0", MIB).unwrap(),
            b.provision_append_region("w0", MIB).unwrap(),
        ),
        (
            a.provision_append_region("w1", MIB).unwrap(),
            b.provision_append_region("w1", MIB).unwrap(),
        ),
    ];
    std::thread::scope(|scope| {
        for (ra, rb) in &regions {
            let set = &set;
            scope.spawn(move || {
                for i in 0..20u8 {
                    let ta = ra.append(&[i; B]).unwrap();
                    let tb = rb.append(&[i; B]).unwrap();
                    let r = set.barrier(&[&ta, &tb]).unwrap();
                    assert!(r[0].covers(&ta) && r[1].covers(&tb));
                }
            });
        }
    });
    assert_eq!(set.stats().barriers, 40);
}

/// With no flush thread (the OS refused it), every store is flushed on the
/// calling thread.
#[test]
fn test_a_store_whose_flush_thread_cannot_start_is_flushed_inline() {
    let p = two_devices(111, DeviceKind::VolatileCache);
    let (a, b) = (create_in(&p, "/a"), create_in(&p, "/b"));
    let set = StoreSet::new([a.clone(), b.clone()]).unwrap();
    let (wa, wb) = (
        a.provision_append_region("wal", MIB).unwrap(),
        b.provision_append_region("wal", MIB).unwrap(),
    );
    let ta = wa.append(&[1u8; B]).unwrap();
    let tb = wb.append(&[2u8; B]).unwrap();
    let (fa, fb) = (flushes(&a), flushes(&b));
    let r = store_io_engine::test_hook::without_flush_threads(|| set.barrier(&[&ta, &tb])).unwrap();
    assert!(r[0].covers(&ta) && r[1].covers(&tb));
    assert_eq!((flushes(&a), flushes(&b)), (fa + 1, fb + 1));
    let stats = set.stats();
    assert_eq!(stats.fallbacks, 1, "the store it would hand off");
    assert_eq!(stats.sleeps, 0, "an inline flush sleeps on no answer");
}

/// Parks the next step of `p`'s world until released, reporting when
/// parked.
struct Park {
    state: std::sync::Arc<(std::sync::Mutex<(bool, bool)>, std::sync::Condvar)>,
}

impl Park {
    fn arm(p: &SimPlatform) -> Self {
        let state = std::sync::Arc::new((
            std::sync::Mutex::new((false, false)),
            std::sync::Condvar::new(),
        ));
        let s = std::sync::Arc::clone(&state);
        p.with_world(|w| {
            w.set_step_probe(Some(std::sync::Arc::new(move || {
                let (m, cv) = &*s;
                let mut g = m.lock().unwrap();
                g.0 = true;
                cv.notify_all();
                while !g.1 {
                    g = cv.wait(g).unwrap();
                }
                false
            })));
        });
        Self { state }
    }

    fn until_parked(&self) {
        let (m, cv) = &*self.state;
        let mut g = m.lock().unwrap();
        while !g.0 {
            g = cv.wait(g).unwrap();
        }
    }

    fn release(&self) {
        let (m, cv) = &*self.state;
        m.lock().unwrap().1 = true;
        cv.notify_all();
    }
}

/// A store whose handed-off flush hangs: its poison (a caller's watchdog)
/// ends the call, its tickets failing `Poisoned` and the other store's
/// receipts standing.
#[test]
fn test_the_poison_of_a_handed_off_store_ends_the_call() {
    let (pa, pb) = (
        SimPlatform::new(SimConfig::volatile(112)),
        SimPlatform::new(SimConfig::volatile(113)),
    );
    let (a, b) = (create_in(&pa, "/a"), create_in(&pb, "/b"));
    // b first: the caller leads the last ready store (a) itself and hands
    // b to its flush thread.
    let set = StoreSet::new([b.clone(), a.clone()]).unwrap();
    let (wa, wb) = (
        a.provision_append_region("wal", MIB).unwrap(),
        b.provision_append_region("wal", MIB).unwrap(),
    );
    let ta = wa.append(&[1u8; B]).unwrap();
    let tb = wb.append(&[2u8; B]).unwrap();
    let park = Park::arm(&pb);
    std::thread::scope(|scope| {
        let (done, call) = std::sync::mpsc::channel();
        let (set, ta, tb) = (&set, &ta, &tb);
        scope.spawn(move || done.send(set.barrier_each(&[ta, tb]).unwrap()));
        park.until_parked();
        b.poison();
        let got = call.recv_timeout(std::time::Duration::from_secs(10));
        // Released before judging, so a failure ends the test, never hangs
        // it (the parked flush holds the call's scope).
        park.release();
        let each = got.expect("the poison did not end the call");
        assert!(each[0].as_ref().unwrap().covers(ta));
        assert!(
            matches!(each[1], Err(Error::Poisoned { .. })),
            "{:?}",
            each[1]
        );
    });
    pb.with_world(|w| w.set_step_probe(None));
}

/// A store whose flush fails fails only its own tickets: the other store's
/// receipts stand, and `barrier` returns the failing ticket's error. A
/// ticket of the failed store already durable keeps its receipt.
#[test]
fn test_a_failed_store_fails_only_its_own_tickets() {
    let (pa, pb) = (
        SimPlatform::new(SimConfig::volatile(114)),
        SimPlatform::new(SimConfig::volatile(115)),
    );
    let (a, b) = (create_in(&pa, "/a"), create_in(&pb, "/b"));
    let set = StoreSet::new([a.clone(), b.clone()]).unwrap();
    let (wa, wb) = (
        a.provision_append_region("wal", MIB).unwrap(),
        b.provision_append_region("wal", MIB).unwrap(),
    );
    let pages = b.provision_page_region("pages", MIB).unwrap();
    let early = wb.append(&[3u8; B]).unwrap();
    wb.sync_through(&early).unwrap();
    let covered = pages.write(pages.pos(0).unwrap(), &[6u8; B]).unwrap();
    pages.sync_through(&covered).unwrap();
    let ta = wa.append(&[1u8; B]).unwrap();
    let tb = wb.append(&[2u8; B]).unwrap();
    pb.with_world(|w| w.faults_mut().fail_flush = Some(w.flushes() + 1));
    let each = set.barrier_each(&[&ta, &tb, &early, &covered]).unwrap();
    assert!(
        each[3].as_ref().unwrap().covers(&covered),
        "a finished flush covered it"
    );
    assert!(each[0].as_ref().unwrap().covers(&ta));
    assert!(
        matches!(each[1], Err(Error::DurabilityUnknown { .. })),
        "{:?}",
        each[1]
    );
    assert!(each[2].as_ref().unwrap().covers(&early));
    assert!(a.poisoned().is_none() && b.poisoned().is_some());
    let ta = wa.append(&[4u8; B]).unwrap();
    let tb = wb.append(&[5u8; B]);
    assert!(tb.is_err(), "a poisoned store takes no write");
    assert!(set.barrier(&[&ta]).unwrap()[0].covers(&ta));
}

/// Three stores on three devices, a ticket each.
fn three(seed: u64) -> (Vec<SimPlatform>, StoreSet<SimPlatform>, Vec<WriteTicket>) {
    let ps: Vec<SimPlatform> = (0..3)
        .map(|i| SimPlatform::new(SimConfig::volatile(seed + i)))
        .collect();
    let stores: Vec<Store<SimPlatform>> = ps
        .iter()
        .enumerate()
        .map(|(i, p)| create_in(p, &format!("/s{i}")))
        .collect();
    let set = StoreSet::new(stores.clone()).unwrap();
    let tickets = stores
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let w = s.provision_append_region("wal", MIB).unwrap();
            w.append(&[i as u8; B]).unwrap()
        })
        .collect();
    (ps, set, tickets)
}

/// Three stores, the first two handed to their flush threads (the caller
/// leads the last): each answer reaches its own store, so the call ends,
/// every receipt covering its ticket. (An answer sent to another store's
/// slot leaves one store waiting for ever: the call is bounded here.)
#[test]
fn test_answers_reach_each_handed_off_store() {
    let (_ps, set, tickets) = three(117);
    let (done, call) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let refs: Vec<&WriteTicket> = tickets.iter().collect();
        let each = set.barrier_each(&refs).unwrap();
        let ok = each
            .iter()
            .zip(&tickets)
            .all(|(r, t)| r.as_ref().is_ok_and(|r| r.covers(t)));
        let _sent = done.send((ok, set.stats()));
    });
    let (ok, stats) = call
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("an answer never reached its store");
    assert!(ok, "a receipt did not cover its ticket");
    assert_eq!((stats.stores, stats.fallbacks), (3, 0));
}

/// Three stores, the second's handed-off flush failing: its ticket fails
/// (`DurabilityUnknown` as its flush thread answers, or `Poisoned` with the
/// flush's cause when its poison is seen first), the others' receipts
/// stand.
#[test]
fn test_a_handed_off_store_whose_flush_fails() {
    let (ps, set, tickets) = three(120);
    ps[1].with_world(|w| w.faults_mut().fail_flush = Some(w.flushes() + 1));
    let refs: Vec<&WriteTicket> = tickets.iter().collect();
    let each = set.barrier_each(&refs).unwrap();
    assert!(each[0].as_ref().unwrap().covers(&tickets[0]));
    match &each[1] {
        Err(Error::DurabilityUnknown { .. }) => {}
        Err(Error::Poisoned { first }) => {
            assert_eq!(first.op, Some(store_io_core::error::Op::FlushData));
        }
        other => panic!("{other:?}"),
    }
    assert!(each[2].as_ref().unwrap().covers(&tickets[2]));
    assert!(set[0].poisoned().is_none() && set[2].poisoned().is_none());
}

/// Gives device `d` of `p` a complete identity of its own (`Sim { device:
/// node }`), the evidence otherwise `s`'s.
fn identify(p: &SimPlatform, s: &Store<SimPlatform>, d: usize, node: u32) {
    let mut ev = s.report().evidence.clone();
    ev.device.identity = store_io_core::identity::DeviceIdentity {
        devices: vec![store_io_core::identity::DeviceId::node(
            store_io_core::identity::DeviceNode::Sim { device: node },
        )],
        complete: true,
    };
    p.with_world(|w| w.set_device_evidence(d, Some(ev)));
}

/// The writes of `dir`'s container in `p`'s trace so far, as indices.
fn writes_of(p: &SimPlatform, dir: &str) -> Vec<usize> {
    p.with_world(|w| {
        let id = w
            .file_id(&PathBuf::from(dir), store_io_engine::CONTAINER)
            .unwrap();
        w.trace()
            .iter()
            .enumerate()
            .filter(|(_, e)| matches!(e, store_io_sim::world::TraceEvent::Write { file, .. } if *file == id))
            .map(|(i, _)| i)
            .collect()
    })
}

/// Ditto writes: a copy on each of two distinct devices, written in set
/// order whatever the order named, one result per copy in the order named,
/// each made durable by a set barrier.
#[test]
fn test_ditto_writes_a_copy_on_each_device_in_set_order() {
    let p = two_devices(120, DeviceKind::VolatileCache);
    let (a, b) = (create_in(&p, "/a"), create_in(&p, "/b"));
    identify(&p, &a, 0, 0);
    identify(&p, &a, 1, 1);
    let set = StoreSet::new([a.clone(), b.clone()]).unwrap();
    let (ra, rb) = (
        a.provision_append_region("log", MIB).unwrap(),
        b.provision_append_region("log", MIB).unwrap(),
    );
    let (wa, wb) = (writes_of(&p, "/a").len(), writes_of(&p, "/b").len());
    let t = set.ditto(&[&rb, &ra], &[7u8; B]).unwrap();
    let (tb, ta) = (t[0].as_ref().unwrap(), t[1].as_ref().unwrap());
    let (na, nb) = (writes_of(&p, "/a"), writes_of(&p, "/b"));
    assert_eq!((na.len(), nb.len()), (wa + 1, wb + 1));
    assert!(na[wa] < nb[wb], "written in set order: a's copy first");
    let r = set.barrier(&[ta, tb]).unwrap();
    assert!(r[0].covers(ta) && r[1].covers(tb));
    // Each result is its named region's: each syncs through it.
    assert!(rb.sync_through(tb).unwrap().covers(tb));
    assert!(ra.sync_through(ta).unwrap().covers(ta));
    // Pages too.
    let (pa, pb) = (
        a.provision_page_region("pages", MIB).unwrap(),
        b.provision_page_region("pages", MIB).unwrap(),
    );
    let t = set
        .ditto_pages(
            &[(&pb, pb.pos(B as u64).unwrap()), (&pa, pa.pos(0).unwrap())],
            &[8u8; B],
        )
        .unwrap();
    assert!(pb.sync_through(t[0].as_ref().unwrap()).is_ok());
    assert!(pa.sync_through(t[1].as_ref().unwrap()).is_ok());
    let r = set
        .barrier(&[t[0].as_ref().unwrap(), t[1].as_ref().unwrap()])
        .unwrap();
    assert!(r[0].covers(t[0].as_ref().unwrap()) && r[1].covers(t[1].as_ref().unwrap()));
    let mut out = vec![0u8; B];
    pb.read(pb.pos(B as u64).unwrap(), &mut out).unwrap();
    assert_eq!(out, vec![8u8; B]);
}

/// A ditto is refused before any write unless every pair of its stores is
/// on devices known to be distinct (two regions of one store, incomplete
/// identities, one identity on both), and unless every region's store is
/// in the set.
#[test]
fn test_ditto_refusals_come_before_any_write() {
    let p = two_devices(121, DeviceKind::VolatileCache);
    let (a, b) = (create_in(&p, "/a"), create_in(&p, "/b"));
    let outside = create_in(&p, "/c");
    let set = StoreSet::new([a.clone(), b.clone()]).unwrap();
    let (ra, ra2, rb) = (
        a.provision_append_region("log", MIB).unwrap(),
        a.provision_append_region("log2", MIB).unwrap(),
        b.provision_append_region("log", MIB).unwrap(),
    );
    let pa = a.provision_page_region("pages", MIB).unwrap();
    let pb = b.provision_page_region("pages", MIB).unwrap();
    let rc = outside.provision_append_region("log", MIB).unwrap();
    let writes = || p.with_world(|w| w.writes());
    let before = writes();
    // Incomplete identities.
    assert_eq!(
        cause(set.ditto(&[&ra, &rb], &[1u8; B])),
        NotWrittenCause::SameDevice
    );
    assert_eq!(
        cause(set.ditto_pages(
            &[(&pa, pa.pos(0).unwrap()), (&pb, pb.pos(0).unwrap())],
            &[1u8; B]
        )),
        NotWrittenCause::SameDevice
    );
    identify(&p, &a, 0, 0);
    identify(&p, &a, 1, 0);
    // One identity on both devices.
    assert_eq!(
        cause(set.ditto(&[&ra, &rb], &[1u8; B])),
        NotWrittenCause::SameDevice
    );
    identify(&p, &a, 1, 1);
    // Two regions of one store.
    assert_eq!(
        cause(set.ditto(&[&ra, &ra2, &rb], &[1u8; B])),
        NotWrittenCause::SameDevice
    );
    // A region of no store in the set, whatever the devices.
    assert_eq!(
        cause(set.ditto(&[&ra, &rc], &[1u8; B])),
        NotWrittenCause::ForeignPosition
    );
    assert_eq!(writes(), before, "a refused ditto wrote");
    // Distinct now: written.
    let t = set.ditto(&[&ra, &rb], &[1u8; B]).unwrap();
    assert!(t.iter().all(Result::is_ok));
    assert_eq!(writes(), before + 2);
}

/// A copy failing after submission is its store's: that store is poisoned
/// and its copy's result is the error; the other copy is written and made
/// durable.
#[test]
fn test_a_failing_ditto_copy_is_its_stores_failure() {
    let p = two_devices(122, DeviceKind::VolatileCache);
    let (a, b) = (create_in(&p, "/a"), create_in(&p, "/b"));
    identify(&p, &a, 0, 0);
    identify(&p, &a, 1, 1);
    let set = StoreSet::new([a.clone(), b.clone()]).unwrap();
    let (ra, rb) = (
        a.provision_append_region("log", MIB).unwrap(),
        b.provision_append_region("log", MIB).unwrap(),
    );
    // Set order: a's copy is written first, b's second, and fails.
    p.with_world(|w| w.faults_mut().fail_write = Some(w.writes() + 2));
    let t = set.ditto(&[&ra, &rb], &[3u8; B]).unwrap();
    let ta = t[0].as_ref().unwrap();
    assert!(t[1].is_err(), "{:?}", t[1]);
    assert!(b.poisoned().is_some() && a.poisoned().is_none());
    assert!(set.barrier(&[ta]).unwrap()[0].covers(ta));
}

/// The first copy in set order failing stops nothing: the later copy is
/// written and made durable, and only the first's store is poisoned.
#[test]
fn test_a_first_ditto_copy_failing_leaves_the_later_ones() {
    let p = two_devices(123, DeviceKind::VolatileCache);
    let (a, b) = (create_in(&p, "/a"), create_in(&p, "/b"));
    identify(&p, &a, 0, 0);
    identify(&p, &a, 1, 1);
    let set = StoreSet::new([a.clone(), b.clone()]).unwrap();
    let (ra, rb) = (
        a.provision_append_region("log", MIB).unwrap(),
        b.provision_append_region("log", MIB).unwrap(),
    );
    // a's copy is written first (set order), and fails.
    p.with_world(|w| w.faults_mut().fail_write = Some(w.writes() + 1));
    let t = set.ditto(&[&rb, &ra], &[4u8; B]).unwrap();
    assert!(t[1].is_err(), "{:?}", t[1]);
    let tb = t[0].as_ref().unwrap();
    assert!(a.poisoned().is_some() && b.poisoned().is_none());
    assert!(set.barrier(&[tb]).unwrap()[0].covers(tb));
}

/// A page position its region refuses fails the whole ditto before any
/// write, whichever copy it is.
#[test]
fn test_a_refused_ditto_page_position_comes_before_any_write() {
    let p = two_devices(124, DeviceKind::VolatileCache);
    let (a, b) = (create_in(&p, "/a"), create_in(&p, "/b"));
    identify(&p, &a, 0, 0);
    identify(&p, &a, 1, 1);
    let set = StoreSet::new([a.clone(), b.clone()]).unwrap();
    let (pa, pb) = (
        a.provision_page_region("pages", MIB).unwrap(),
        b.provision_page_region("pages", MIB).unwrap(),
    );
    let other = b.provision_page_region("other", MIB).unwrap();
    let before = p.with_world(|w| w.writes());
    // The second copy's position is another region's.
    assert_eq!(
        cause(set.ditto_pages(
            &[(&pa, pa.pos(0).unwrap()), (&pb, other.pos(0).unwrap())],
            &[1u8; B]
        )),
        NotWrittenCause::ForeignPosition
    );
    assert_eq!(
        cause(set.ditto_pages(
            &[
                (&pa, pa.pos(0).unwrap()),
                (&pb, pb.pos(MIB - B as u64).unwrap())
            ],
            &[1u8; 2 * B]
        )),
        NotWrittenCause::OutOfBounds
    );
    assert_eq!(
        p.with_world(|w| w.writes()),
        before,
        "a refused ditto wrote"
    );
}
