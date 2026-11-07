//! Dependent writes across regions, stores and opens (R5.3): a dependency
//! on an append region must be a prefix receipt, whichever store it is
//! from; a dependency on a reachable store must name a live generation of
//! its region, and one from an earlier open must lie below where this open
//! resumed it. Every refusal comes before anything is reserved or written.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_results,
    unused_must_use
)]

use std::path::{Path, PathBuf};

use store_io_core::error::{Error, NotWrittenCause};
use store_io_engine::completion::Done;
use store_io_engine::{DurableReceipt, Store, StoreOptions, SubmitterConfig, WriteMode};
use store_io_sim::{SimConfig, SimPlatform};

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
