//! `copy_store` (v0.5 E5 C1): a store copied into a fresh format-2 one,
//! every region with its name, kind, data, size, cap, tag, region
//! generation, fill pattern and key; the source never written; refusals
//! before anything is created; a crash leaves the target name empty or
//! holding the whole copy.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_results,
    unused_must_use
)]

mod format2_util;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use format2_util::audit::{audit, audit_in, header_in};
use format2_util::craft::{region, tear_newer_copy_of, write_regions};
use format2_util::*;
use store_io_core::error::{Error, NotWrittenCause, ReserveTag};
use store_io_engine::{
    CONTAINER, COPY_LOCK, COPYING, CopyOptions, RegionOptions, Store, StoreOptions, copy_store,
};
use store_io_format::meta::{FillPattern, RegionKind};
use store_io_platform::{FileMode, FileName, Platform};
use store_io_sim::{CrashMode, SimConfig, SimPlatform};

const B: u64 = 4096;
/// The source's directory in the copying tests (the target is `DIR`, which
/// the audit reads); the target's in the refusals (whose crafted sources
/// are in `DIR`).
const OTHER: &str = "/other";

fn src_opts(format: u16) -> StoreOptions {
    StoreOptions {
        container_format: format,
        keyed_fill: true,
        ..opts()
    }
}

fn copy_opts() -> CopyOptions {
    CopyOptions {
        store: opts(),
        ..CopyOptions::default()
    }
}

/// The container names in `dir` (durable entries).
fn files(p: &SimPlatform, dir: &str) -> Vec<&'static str> {
    [CONTAINER, COPYING]
        .into_iter()
        .filter(|n| p.with_world(|w| w.media_of(&PathBuf::from(dir), n).is_some()))
        .collect()
}

fn copy(p: &SimPlatform, from: &str, to: &str) -> Result<store_io_engine::CopyReport, Error> {
    copy_store(p.clone(), Path::new(from), Path::new(to), copy_opts())
}

/// A source store in `OTHER` of `format`: an append region of records, a
/// page region with a page written, a slot with an object, a region from a
/// tagged reservation, one recycled (region generation 2).
fn source(p: &SimPlatform, format: u16) -> Store<SimPlatform> {
    let s = Store::create(p.clone(), Path::new(OTHER), src_opts(format)).unwrap();
    let a = s.provision_append_region("log", 64 * B).unwrap();
    for i in 0..10u8 {
        a.append_durable(&[i + 1; B as usize]).unwrap();
    }
    let pages = s.provision_page_region("pages", 32 * B).unwrap();
    pages
        .write_durable(pages.pos(5 * B).unwrap(), &[0xAB; B as usize])
        .unwrap();
    let slot = s.provision_slot("manifest").unwrap();
    slot.commit(b"the manifest").unwrap();
    let mut r = s.reserve(64 * B, 7).unwrap();
    r.provision_page_region("tagged", 16 * B).unwrap();
    drop(r);
    let old = s.provision_page_region("old", 8 * B).unwrap();
    old.write_durable(old.pos(0).unwrap(), &[3; B as usize])
        .unwrap();
    let again = old.recycle().unwrap();
    again
        .write_durable(again.pos(B).unwrap(), &[4; B as usize])
        .unwrap();
    s
}

/// Every source region is in the target in `DIR` with the same kind, size,
/// data, generation and tag; the slot's object too. Returns the target.
fn same(p: &SimPlatform, src: &Store<SimPlatform>) -> Store<SimPlatform> {
    let dst = open(p);
    assert_eq!(dst.report().format, 2);
    let mut want = src.regions();
    let mut got = dst.regions();
    want.sort_by(|a, b| a.0.cmp(&b.0));
    got.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(got, want, "the same regions");
    for (name, kind) in &want {
        match kind {
            RegionKind::Append => {
                let (a, b) = (
                    src.append_region(name).unwrap(),
                    dst.append_region(name).unwrap(),
                );
                assert_eq!(a.len(), b.len(), "{name}");
                assert_eq!(a.generation(), b.generation(), "{name}");
                let (mut x, mut y) = (vec![0u8; a.len() as usize], vec![1u8; b.len() as usize]);
                a.read(0, &mut x).unwrap();
                b.read(0, &mut y).unwrap();
                assert!(x == y, "{name}: the data");
            }
            RegionKind::Page => {
                let (a, b) = (
                    src.page_region(name).unwrap(),
                    dst.page_region(name).unwrap(),
                );
                assert_eq!(a.len(), b.len(), "{name}");
                assert_eq!(a.generation(), b.generation(), "{name}");
                if !a.is_empty() {
                    let (mut x, mut y) = (vec![0u8; a.len() as usize], vec![1u8; b.len() as usize]);
                    a.read(a.pos(0).unwrap(), &mut x).unwrap();
                    b.read(b.pos(0).unwrap(), &mut y).unwrap();
                    assert!(x == y, "{name}: the data");
                }
            }
            RegionKind::Slot => {
                let (a, b) = (src.slot(name).unwrap(), dst.slot(name).unwrap());
                assert_eq!(a.read(), b.read(), "{name}");
            }
        }
    }
    let tags = |s: &Store<SimPlatform>| {
        let mut t: Vec<_> = s
            .space()
            .tags
            .into_iter()
            .filter(|t| t.logical > 0)
            .map(|t| (t.tag, t.logical))
            .collect();
        t.sort_unstable();
        t
    };
    assert_eq!(tags(&dst), tags(src), "the tags' data");
    assert!(tags(src).iter().any(|t| t.0 == 7));
    dst
}

/// A format-1 store copied: format 2, everything kept (zero-size regions
/// too), keyed fills still keyed (slots too), a slot's region generation 1
/// however often it was committed, the source never written.
#[test]
fn test_a_format_1_store_is_copied_whole() {
    let p = SimPlatform::new(SimConfig::volatile(1001));
    let s = source(&p, 1);
    s.provision_page_region("empty pages", 0).unwrap();
    s.provision_append_region("empty log", 0).unwrap();
    let slot = s.slot("manifest").unwrap();
    slot.commit(b"the manifest, again").unwrap();
    drop((slot, s));
    let media_of = || {
        p.with_world(|w| {
            w.media_of(&PathBuf::from(OTHER), CONTAINER)
                .unwrap()
                .to_vec()
        })
    };
    let before = media_of();
    let report = copy(&p, OTHER, DIR).unwrap();
    assert_eq!((report.regions, report.source_format), (7, 1));
    assert!(media_of() == before, "the source never written");
    let src = Store::open_readonly(p.clone(), Path::new(OTHER), src_opts(1)).unwrap();
    assert_eq!(src.page_region("old").unwrap().generation().get(), 2);
    assert_eq!(src.page_region("empty pages").unwrap().len(), 0);
    let dst = same(&p, &src);
    assert!(dst.report().metadata.clean_close, "closed cleanly");
    assert_eq!(dst.page_region("empty pages").unwrap().len(), 0);
    assert_eq!(dst.append_region("empty log").unwrap().len(), 0);
    drop(dst);
    assert_eq!(files(&p, DIR), [CONTAINER], "the temporary name gone");
    let mut keys = Vec::new();
    for f in audit(&p) {
        let h = header_in(&p, DIR, f.id, f.header_at).unwrap().header;
        assert_eq!(h.fill, FillPattern::KeyedV1, "{}", f.name);
        assert_ne!(h.fill_key, [0; 16], "{}", f.name);
        keys.push(h.fill_key);
        let want = if f.name == "old" { 2 } else { 1 };
        assert_eq!(h.region_generation, want, "{}", f.name);
    }
    keys.sort_unstable();
    keys.dedup();
    assert_eq!(keys.len(), 7, "each region its own key");
}

/// A format-2 store copied: a capped region stays capped at its cap, a
/// grown region of several extents comes out in one, every region keeps
/// its fill key and flags.
#[test]
fn test_a_format_2_store_is_copied_whole() {
    let p = SimPlatform::new(SimConfig::volatile(1002));
    let s = source(&p, 2);
    let capped = s
        .provision_page_region_with(
            "capped",
            RegionOptions::default().size(8 * B).max_size(64 * B),
        )
        .unwrap();
    capped.grow(16 * B).unwrap();
    let grown = s.provision_page_region("grown", 8 * B).unwrap();
    s.provision_page_region("between", 8 * B).unwrap();
    grown.grow(12 * B).unwrap();
    grown
        .write_durable(grown.pos(10 * B).unwrap(), &[9; B as usize])
        .unwrap();
    drop((capped, grown, s));
    let want: Vec<_> = audit_in(&p, OTHER)
        .into_iter()
        .map(|f| {
            let h = header_in(&p, OTHER, f.id, f.header_at).unwrap().header;
            (f.name, f.extents.len(), h.fill, h.fill_key, h.flags)
        })
        .collect();
    assert_eq!(want.iter().find(|w| w.0 == "grown").unwrap().1, 2);
    let before = media_in(&p, OTHER);
    let report = copy(&p, OTHER, DIR).unwrap();
    assert!(media_in(&p, OTHER) == before, "the source never written");
    assert_eq!((report.regions, report.source_format), (8, 2));
    let src = Store::open_readonly(p.clone(), Path::new(OTHER), src_opts(2)).unwrap();
    let dst = same(&p, &src);
    let c = dst.page_region("capped").unwrap();
    assert_eq!((c.len(), c.capacity()), (16 * B, 64 * B), "the cap kept");
    let e = c.grow(65 * B).unwrap_err();
    assert!(
        matches!(
            e,
            Error::NoSpace {
                tag: ReserveTag::RegionLimit,
                ..
            }
        ),
        "{e:?}"
    );
    drop((c, dst));
    for f in audit(&p) {
        let h = header_in(&p, DIR, f.id, f.header_at).unwrap().header;
        let w = want.iter().find(|w| w.0 == f.name).unwrap();
        assert_eq!(f.extents.len(), w.1.min(1), "{}: laid out anew", f.name);
        assert_eq!((h.fill, h.fill_key, h.flags), (w.2, w.3, w.4), "{}", f.name);
    }
}

/// Refused before anything is created: a source with an unavailable
/// region, one with a stale page, one with a slot object larger than a
/// format-2 slot holds, a target that holds a store.
#[test]
fn test_refusals_create_nothing() {
    // An unavailable region (a header never written).
    let p = SimPlatform::new(SimConfig::volatile(1003));
    drop(create(&p));
    let mut r = region(
        0,
        RegionKind::Page,
        "broken",
        2 * B + 2 * u64::from(PAGES) * TABLE,
        8 * B,
    );
    r.write_header = false;
    write_regions(&p, 0, &[r]);
    let e = copy(&p, DIR, OTHER).unwrap_err();
    assert!(
        matches!(e, Error::NotWritten { cause: NotWrittenCause::NotReady, ctx } if ctx.region.is_some()),
        "{e:?}"
    );
    assert!(files(&p, OTHER).is_empty());
    // An entry of a kind from a newer release (only in the report's
    // invalid entries).
    let p = SimPlatform::new(SimConfig::volatile(1009));
    drop(create(&p));
    let base = 2 * B + 2 * u64::from(PAGES) * TABLE;
    let fine = region(0, RegionKind::Page, "fine", base, 16 * B);
    let mut newer = region(1, RegionKind::Append, "newer", base + 18 * B, 16 * B);
    newer.kind_raw = Some(9);
    write_regions(&p, 0, &[fine, newer]);
    let e = copy(&p, DIR, OTHER).unwrap_err();
    assert!(
        matches!(e, Error::NotWritten { cause: NotWrittenCause::NotReady, ctx } if ctx.region.is_some()),
        "{e:?}"
    );
    assert!(files(&p, OTHER).is_empty());
    // A stale page.
    let p = SimPlatform::new(SimConfig::volatile(1004));
    let s = create(&p);
    s.provision_page_region("a", 8 * B).unwrap();
    drop(s);
    tear_newer_copy_of(&p, 0);
    let e = copy(&p, DIR, OTHER).unwrap_err();
    assert!(
        matches!(
            e,
            Error::NotWritten {
                cause: NotWrittenCause::Frozen,
                ..
            }
        ),
        "{e:?}"
    );
    assert!(files(&p, OTHER).is_empty());
    // A format-1 slot object larger than format 2 holds.
    let p = SimPlatform::new(SimConfig::volatile(1005));
    let s = Store::create(p.clone(), Path::new(DIR), src_opts(1)).unwrap();
    let slot = s.provision_slot("big").unwrap();
    let max2 = store_io_format::meta2::max_object(12);
    assert!(slot.capacity() > max2, "format 1's slots are larger");
    slot.commit(&vec![1u8; max2 + 1]).unwrap();
    drop((slot, s));
    let e = copy(&p, DIR, OTHER).unwrap_err();
    assert!(
        matches!(e, Error::NotWritten { cause: NotWrittenCause::TooLarge, ctx } if ctx.region.is_some()),
        "{e:?}"
    );
    assert!(files(&p, OTHER).is_empty());
    // A target holding a store.
    let p = SimPlatform::new(SimConfig::volatile(1006));
    drop(source(&p, 2));
    drop(create(&p));
    let before = media(&p);
    let e = copy(&p, OTHER, DIR).unwrap_err();
    assert!(matches!(e, Error::AlreadyExists { .. }), "{e:?}");
    assert!(media(&p) == before);
    assert_eq!(files(&p, DIR), [CONTAINER]);
    let dir = p.open_dir(Path::new(DIR), false).unwrap();
    for n in [COPY_LOCK, COPYING] {
        let e = p.open_file(&dir, &FileName::new(n).unwrap(), FileMode::ReadOnly);
        assert!(e.is_err(), "{n} created");
    }
}

/// Another copy into the directory holds the copy lock: `Locked`, and the
/// other copy's temporary store untouched.
#[test]
fn test_a_copy_waits_for_no_other() {
    let p = SimPlatform::new(SimConfig::volatile(1010));
    drop(source(&p, 1));
    let dir = p.open_dir(Path::new(DIR), true).unwrap();
    let lock_file = p
        .create_file(&dir, &FileName::new(COPY_LOCK).unwrap())
        .unwrap();
    let held = p.lock_exclusive(&lock_file).unwrap();
    p.create_file(&dir, &FileName::new(COPYING).unwrap())
        .unwrap();
    p.sync_dir(&dir).unwrap();
    let e = copy(&p, OTHER, DIR).unwrap_err();
    assert!(matches!(e, Error::Locked), "{e:?}");
    assert_eq!(files(&p, DIR), [COPYING], "the other copy's file kept");
    drop(held);
    copy(&p, OTHER, DIR).unwrap();
    assert_eq!(files(&p, DIR), [CONTAINER]);
    let src = Store::open_readonly(p.clone(), Path::new(OTHER), src_opts(1)).unwrap();
    drop(same(&p, &src));
}

/// A second copy into the directory started while one runs (from inside
/// its writes) is `Locked`, before touching anything; the first finishes
/// whole.
#[test]
fn test_a_second_copy_meanwhile_is_locked() {
    let p = SimPlatform::new(SimConfig::volatile(1012));
    drop(source(&p, 1));
    let second: Arc<Mutex<Option<Result<(), Error>>>> = Arc::new(Mutex::new(None));
    let started = Arc::new(AtomicBool::new(false));
    p.with_world(|w| {
        let (p, second, started) = (p.clone(), Arc::clone(&second), Arc::clone(&started));
        let writes = w.write_submits();
        w.set_submit_probe(Some(Arc::new(move || {
            // Well into the first copy (its temporary store exists), once.
            if p.with_world(|w| w.write_submits()) > writes + 8
                && !started.swap(true, Ordering::SeqCst)
            {
                let r = copy(&p, OTHER, DIR).map(|_| ());
                *second.lock().unwrap() = Some(r);
            }
        })));
    });
    let first = copy(&p, OTHER, DIR);
    p.with_world(|w| w.set_submit_probe(None));
    let second = second.lock().unwrap().take().expect("the second copy ran");
    assert!(matches!(second, Err(Error::Locked)), "{second:?}");
    first.unwrap();
    assert_eq!(files(&p, DIR), [CONTAINER]);
    let src = Store::open_readonly(p.clone(), Path::new(OTHER), src_opts(1)).unwrap();
    drop(same(&p, &src));
}

/// Whether the target holds a store is decided only by "not found": any
/// other failure to look (here a refused open) is an error, never "no
/// store", and nothing is created.
#[test]
fn test_a_target_that_cannot_be_looked_at_is_refused() {
    let p = SimPlatform::new(SimConfig::volatile(1013));
    drop(source(&p, 1));
    p.with_world(|w| {
        w.faults_mut().fail_open = Some((PathBuf::from(DIR), CONTAINER.to_owned(), 13));
    });
    let e = copy(&p, OTHER, DIR).unwrap_err();
    p.with_world(|w| w.faults_mut().fail_open = None);
    assert!(
        matches!(e, Error::Io { raw, .. } if raw.code == 13),
        "{e:?}"
    );
    let dir = p.open_dir(Path::new(DIR), false).unwrap();
    for n in [COPY_LOCK, COPYING, CONTAINER] {
        let e = p.open_file(&dir, &FileName::new(n).unwrap(), FileMode::ReadOnly);
        assert!(e.is_err(), "{n} created");
    }
}

/// A target device that needs larger blocks than the source's is refused
/// (every size would round up), as is a source region whose data size is
/// not whole blocks (a format-2 append region may end on 512 bytes).
#[test]
fn test_sizes_that_would_change_are_refused() {
    let p = SimPlatform::new(SimConfig::volatile(1014));
    drop(source(&p, 1));
    let dir = p.open_dir(Path::new(OTHER), false).unwrap();
    let f = p
        .open_file(&dir, &FileName::new(CONTAINER).unwrap(), FileMode::ReadOnly)
        .unwrap();
    let mut ev = p.probe(&dir, &f).unwrap();
    ev.device.optimal_write = 8192;
    p.with_world(|w| w.set_evidence(Some(ev)));
    let e = copy(&p, OTHER, DIR).unwrap_err();
    assert!(
        matches!(e, Error::NotWritten { cause: NotWrittenCause::Misaligned, ctx } if ctx.region.is_none()),
        "{e:?}"
    );
    assert_eq!(files(&p, DIR), [COPYING], "left for the next copy");
    // An append region of 512-byte blocks holding three.
    let p = SimPlatform::new(SimConfig::volatile(1015));
    drop(create(&p));
    let base = 2 * B + 2 * u64::from(PAGES) * TABLE;
    let mut odd = region(0, RegionKind::Append, "odd", base, 8 * B);
    odd.entry.data_size = 3 * 512;
    odd.header.log2_block = 9;
    write_regions(&p, 0, &[odd]);
    let s = Store::open_readonly(p.clone(), Path::new(DIR), opts()).unwrap();
    assert_eq!(
        s.append_region("odd").unwrap().len(),
        1536,
        "the crafted size"
    );
    drop(s);
    let e = copy(&p, DIR, OTHER).unwrap_err();
    assert!(
        matches!(e, Error::NotWritten { cause: NotWrittenCause::Misaligned, ctx } if ctx.region.is_some()),
        "{e:?}"
    );
}

/// A store created in the target directory while the copy runs is never
/// replaced: the copy is refused, `AlreadyExists`, its temporary store
/// removed, the other store whole.
#[test]
fn test_a_store_created_meanwhile_is_kept() {
    let p = SimPlatform::new(SimConfig::volatile(1011));
    drop(source(&p, 1));
    let done = Arc::new(AtomicBool::new(false));
    p.with_world(|w| {
        let (p, done) = (p.clone(), Arc::clone(&done));
        let writes = w.write_submits();
        w.set_submit_probe(Some(Arc::new(move || {
            // Well into the copy (its target exists), once.
            if p.with_world(|w| w.write_submits()) > writes + 8
                && !done.swap(true, Ordering::SeqCst)
            {
                let s = create(&p);
                let v = s.provision_page_region("victim", 8 * B).unwrap();
                v.write_durable(v.pos(0).unwrap(), &[5; B as usize])
                    .unwrap();
            }
        })));
    });
    let e = copy(&p, OTHER, DIR).unwrap_err();
    p.with_world(|w| w.set_submit_probe(None));
    assert!(done.load(Ordering::SeqCst));
    assert!(matches!(e, Error::AlreadyExists { .. }), "{e:?}");
    p.with_world(|w| w.crash(&CrashMode::Random));
    assert_eq!(
        files(&p, DIR),
        [CONTAINER],
        "the copy's temporary store removed"
    );
    let s = open(&p);
    assert_eq!(s.regions().len(), 1);
    let v = s.page_region("victim").unwrap();
    let mut b = vec![0u8; B as usize];
    v.read(v.pos(0).unwrap(), &mut b).unwrap();
    assert!(b == [5; B as usize], "the other store whole");
}

/// After a failed copy, what power leaves: the target name holds nothing or
/// the whole copy, the source is unchanged, and a copy run again replaces
/// any leftover under the temporary name. Returns whether the copy was
/// there.
fn after_a_failure(p: &SimPlatform, what: &str) -> bool {
    p.with_world(|w| w.crash(&CrashMode::Random));
    let src = Store::open_readonly(p.clone(), Path::new(OTHER), src_opts(1)).unwrap();
    assert_eq!(src.regions().len(), 5, "{what}: the source");
    let whole = files(p, DIR).contains(&CONTAINER);
    if !whole {
        copy(p, OTHER, DIR).unwrap();
        assert_eq!(files(p, DIR), [CONTAINER], "{what}");
    }
    drop(same(p, &src));
    audit(p);
    whole
}

/// Power fails at each write submission of a copy, and at its directory
/// flush (after the rename).
#[test]
fn test_a_power_failure_at_any_point_of_a_copy() {
    let mut k = 1u64;
    let mut left = 0;
    loop {
        let p = SimPlatform::new(SimConfig::volatile(1007 + k));
        drop(source(&p, 1));
        p.with_world(|w| w.faults_mut().fail_submit = Some((w.write_submits() + k, 5)));
        let r = copy(&p, OTHER, DIR);
        p.with_world(|w| w.faults_mut().fail_submit = None);
        left += usize::from(files(&p, DIR).contains(&COPYING));
        after_a_failure(&p, &format!("submission {k}"));
        if r.is_ok() {
            break;
        }
        k += 1;
        assert!(k < 400, "a copy never finished");
    }
    assert!(k > 20 && left > 0, "{k} submissions, {left} leftovers");
    // The directory flush after the rename fails: the rename may be lost.
    let p = SimPlatform::new(SimConfig::volatile(1006));
    drop(source(&p, 1));
    p.with_world(|w| w.faults_mut().fail_dir_sync = Some(w.dir_syncs() + 2));
    let e = copy(&p, OTHER, DIR);
    p.with_world(|w| w.faults_mut().fail_dir_sync = None);
    assert!(matches!(e, Err(Error::DurabilityUnknown { .. })), "{e:?}");
    assert!(
        !after_a_failure(&p, "the directory flush"),
        "the rename lost"
    );
}

/// The clean close's flush fails (the close is best effort, silent): the
/// copy's own barrier has already made every byte durable, so the renamed
/// store holds the whole copy, opened as not closed cleanly.
#[test]
fn test_a_failed_clean_close_loses_nothing() {
    let flushes = |p: &SimPlatform| p.with_world(|w| w.flushes());
    let p = SimPlatform::new(SimConfig::volatile(1008));
    drop(source(&p, 1));
    let before = flushes(&p);
    copy(&p, OTHER, DIR).unwrap();
    let last = flushes(&p) - before;
    let p = SimPlatform::new(SimConfig::volatile(1008));
    drop(source(&p, 1));
    p.with_world(|w| w.faults_mut().fail_flush = Some(w.flushes() + last));
    copy(&p, OTHER, DIR).unwrap();
    p.with_world(|w| w.faults_mut().fail_flush = None);
    p.with_world(|w| w.crash(&CrashMode::Random));
    assert!(!open(&p).report().metadata.clean_close);
    assert!(after_a_failure(&p, "the clean close"), "the copy in place");
}
