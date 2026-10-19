//! The simple API end to end on the real file system of the machine running
//! the tests (NTFS on Windows, ext4/XFS on Linux).
//!
//! Where the device is refused (a virtual disk with unverifiable durability,
//! or a file system mounted `nobarrier` as on CI runners), the store is opened
//! with the documented labelled override, and the test asserts that every
//! receipt says so. Nothing here weakens the class decision.

#![cfg(any(windows, target_os = "linux"))]
// Test setup unwraps (a failure is the test failing) and ignores values a
// given test does not inspect.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_results,
    unused_must_use
)]

use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use store_io::{
    Directory, DurabilityClass, Error, ReceiptLabel, ScanItem, Store, StoreOptions, Trust,
};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        // $STORE_IO_TEST_DIR when set (CI: loop-mounted ext4 and XFS);
        // else $HOME on Linux (/tmp may be tmpfs) and %TEMP% on Windows.
        let base = std::env::var_os("STORE_IO_TEST_DIR")
            .or_else(|| std::env::var_os("HOME").filter(|_| cfg!(target_os = "linux")))
            .map_or_else(std::env::temp_dir, PathBuf::from);
        let d = base.join(format!(
            ".store-io-native-{}-{}-{tag}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        Self(d)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _cleanup = std::fs::remove_dir_all(&self.0);
    }
}

/// Options for this machine: default trust where the device is accepted,
/// the labelled override where it is refused. Returns whether the override
/// was needed.
fn options_for(dir: &Path) -> (StoreOptions, bool) {
    let probe = store_io::probe(dir).unwrap();
    let refused = !matches!(probe.decision.durable_open, store_io::DurableOpen::Allowed);
    let opts = StoreOptions {
        trust: Trust {
            override_refusal: refused,
            ..Trust::default()
        },
        ..StoreOptions::default()
    };
    (opts, refused)
}

#[test]
fn test_store_round_trip_on_the_real_file_system() {
    let t = TempDir::new("store");
    let (opts, overridden) = options_for(t.path());
    let path = t.path().join("orders");
    let store = Store::create(&path, opts.clone()).unwrap();
    let class = store.report().decision.class;
    // An unsafe store exists only through the explicit, labelled override.
    if class == DurabilityClass::Unsafe {
        assert!(overridden, "an unsafe store opened without the override");
    }

    let wal = store.provision_append_region("wal", 8 << 20).unwrap();
    let (pos, receipt) = wal.append_durable(b"order 1001").unwrap();
    assert_eq!(receipt.class(), class);
    assert_eq!(
        matches!(receipt.label(), ReceiptLabel::Overridden(_)),
        overridden
    );
    let mut batch = wal.batch();
    for i in 0..100u32 {
        batch
            .append(format!("order {}", 2000 + i).as_bytes())
            .unwrap();
    }
    let (bpos, _) = batch.commit().unwrap();
    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
    let (big_pos, _) = wal.append_durable(&big).unwrap();

    let pages = store.provision_page_region("pages", 1 << 20).unwrap();
    let bs = pages.block_size() as usize;
    pages
        .write_durable(pages.pos(0).unwrap(), &vec![7u8; bs])
        .unwrap();
    let manifest = store.provision_slot("manifest").unwrap();
    manifest.commit(b"wal@0").unwrap();
    let mut reservation = store.reserve(2 << 20, 9).unwrap();
    reservation
        .provision_page_region("tenant9", 1 << 20)
        .unwrap();
    drop((reservation, batch, wal, pages, manifest, store));

    // Read-only recovery: scan finds every record; nothing changes.
    let ro = Store::open_readonly(&path).unwrap();
    let wal = ro.append_region("wal").unwrap();
    let mut first = vec![0u8; 10];
    wal.read(pos.offset(), &mut first).unwrap();
    assert_eq!(&first, b"order 1001");
    let mut seen = Vec::new();
    let summary = wal
        .scan(0, |item| {
            if let ScanItem::Data { offset, bytes } = item {
                seen.push((offset, bytes.len()));
            }
            ControlFlow::Continue(())
        })
        .unwrap();
    assert_eq!(summary.unreadable, 0);
    assert_eq!(summary.end, wal.len());
    let mut back = vec![0u8; big.len()];
    wal.read(big_pos.offset(), &mut back).unwrap();
    assert!(back == big, "large append round trip");
    let mut rec = vec![0u8; 10];
    wal.read(bpos.offset(), &mut rec).unwrap();
    assert_eq!(&rec, b"order 2000");
    assert!(matches!(wal.append(b"x"), Err(Error::NotWritten { .. })));
    drop((wal, ro));

    // Writable reopen: positions resume where the caller says.
    let store = Store::open_with(&path, opts).unwrap();
    assert_eq!(store.slot("manifest").unwrap().read().unwrap(), b"wal@0");
    let wal = store.append_region("wal").unwrap();
    assert!(matches!(wal.append(b"x"), Err(Error::NotWritten { .. })));
    let end = big_pos.offset() + big.len() as u64;
    wal.resume_at(end).unwrap();
    let (next, _) = wal.append_durable(b"after reopen").unwrap();
    assert!(next.offset() >= end);
    assert!(
        store
            .space()
            .tags
            .iter()
            .any(|t| t.tag == 9 && t.physical > 0)
    );
    // A second writable open of the same store is refused.
    assert!(matches!(Store::open(&path), Err(Error::Locked)));
}

#[test]
fn test_directory_replace_on_the_real_file_system() {
    let t = TempDir::new("dir");
    let (opts, _) = options_for(t.path());
    let dir = Directory::open_with(t.path().join("meta"), true, opts.trust).unwrap();
    for (i, len) in [0usize, 15, 4096, 10_000].into_iter().enumerate() {
        let data: Vec<u8> = (0..len).map(|j| (i + j) as u8).collect();
        dir.replace("CURRENT", &data).unwrap();
        assert_eq!(dir.read("CURRENT").unwrap(), data);
        // Ordinary tools see the exact bytes too.
        assert_eq!(
            std::fs::read(t.path().join("meta").join("CURRENT")).unwrap(),
            data
        );
    }
    dir.remove("CURRENT").unwrap();
    assert!(matches!(dir.read("CURRENT"), Err(Error::NotFound { .. })));
    assert!(dir.class().is_some());
}

/// Releasing a region on the real file system gives its space back without
/// touching anything else: the regions on either side, the slots and the
/// table read back exactly, before and after a reopen, and a region
/// provisioned into the freed space works. On Windows the release trims in
/// place, so the container keeps its length and never becomes sparse (a
/// sparse file may allocate on overwrite, which would cost the store its
/// class).
#[test]
fn test_releasing_a_region_leaves_its_neighbours_and_the_metadata_intact() {
    let t = TempDir::new("release");
    let (opts, _overridden) = options_for(t.path());
    let path = t.path().join("columns");
    let store = Store::create(&path, opts.clone()).unwrap();
    let fill =
        |tag: u8, len: usize| -> Vec<u8> { (0..len).map(|i| tag ^ (i % 253) as u8).collect() };
    let size = 1u64 << 20;
    let names = ["left", "middle", "right"];
    for (i, name) in names.iter().enumerate() {
        let r = store.provision_page_region(name, size).unwrap();
        r.write_durable(r.pos(0).unwrap(), &fill(i as u8 + 1, size as usize))
            .unwrap();
    }
    let manifest = store.provision_slot("manifest").unwrap();
    manifest.commit(b"left,middle,right").unwrap();
    let container = path.join(store_io::CONTAINER);
    let len_before = std::fs::metadata(&container).unwrap().len();

    store.page_region("middle").unwrap().release().unwrap();

    let check = |s: &Store| {
        for (i, name) in names.iter().enumerate().filter(|(_, n)| **n != "middle") {
            let r = s.page_region(name).unwrap();
            let mut back = vec![0u8; size as usize];
            r.read(r.pos(0).unwrap(), &mut back).unwrap();
            assert!(back == fill(i as u8 + 1, size as usize), "{name} changed");
        }
        assert!(matches!(
            s.page_region("middle"),
            Err(Error::NotFound { .. })
        ));
        assert_eq!(
            s.slot("manifest").unwrap().read().unwrap(),
            b"left,middle,right"
        );
        assert!(s.unavailable_regions().is_empty());
    };
    check(&store);
    assert_eq!(std::fs::metadata(&container).unwrap().len(), len_before);
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_SPARSE_FILE: u32 = 0x200;
        let attrs = std::fs::metadata(&container).unwrap().file_attributes();
        assert_eq!(
            attrs & FILE_ATTRIBUTE_SPARSE_FILE,
            0,
            "the container became sparse"
        );
    }
    drop((manifest, store));

    let store = Store::open_with(&path, opts).unwrap();
    check(&store);
    // The freed space is reused and reads back what was written.
    let again = store.provision_page_region("again", size).unwrap();
    let data = fill(9, size as usize);
    again.write_durable(again.pos(0).unwrap(), &data).unwrap();
    let mut back = vec![0u8; size as usize];
    again.read(again.pos(0).unwrap(), &mut back).unwrap();
    assert!(back == data, "the reused space did not read back");
    check(&store);
}

/// D-092: a read-only open works on a container the process may not write
/// (a read-only file, as on a read-only mount or a backup copy), and two
/// readers open it at once.
#[test]
fn test_a_write_protected_store_opens_read_only_for_two_readers() {
    let t = TempDir::new("ro");
    let (opts, _overridden) = options_for(t.path());
    let path = t.path().join("backup");
    let store = Store::create(&path, opts).unwrap();
    let wal = store.provision_append_region("wal", 1 << 20).unwrap();
    let (pos, _) = wal.append_durable(b"kept in the backup").unwrap();
    drop((wal, store));

    let container = path.join(store_io::CONTAINER);
    let set_read_only = |on: bool| {
        let mut perms = std::fs::metadata(&container).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(on);
        std::fs::set_permissions(&container, perms).unwrap();
    };
    set_read_only(true);
    let first = Store::open_readonly(&path);
    let second = Store::open_readonly(&path);
    // Writable opens need write rights (unless the tests run as root).
    let writable = Store::open(&path);
    set_read_only(false);
    let (first, second) = (first.unwrap(), second.unwrap());
    for s in [&first, &second] {
        let mut out = vec![0u8; 18];
        s.append_region("wal")
            .unwrap()
            .read(pos.offset(), &mut out)
            .unwrap();
        assert_eq!(out, b"kept in the backup");
    }
    if cfg!(windows) {
        assert!(writable.is_err(), "a writable open of a read-only file");
    }
}

/// The platform's coarse clock (`GetTickCount64`, `CLOCK_MONOTONIC_COARSE`)
/// advances with real time: `oldest_outstanding` ages depend on it.
#[test]
fn test_the_coarse_clock_advances_with_real_time() {
    use store_io_platform::Platform;
    let p = store_io::NativePlatform::new();
    let t0 = p.monotonic_ms();
    std::thread::sleep(std::time::Duration::from_millis(250));
    let t1 = p.monotonic_ms();
    // Coarse resolution is at most 16 ms; allow a generous margin.
    let moved = t1.wrapping_sub(t0);
    assert!(
        t1 >= t0 + 200,
        "the clock advanced only {moved} ms in 250 ms"
    );
    assert!(moved < 10_000, "the clock jumped {moved} ms in 250 ms");
}

/// Page batches of several writes and reads larger than one buffer run on
/// the parallel lanes (D-096): on the Linux synchronous tier through the
/// submit pool, on io_uring and on Windows (IOCP) in the kernel. Every byte
/// comes back as written, one-page batches stay on the caller's thread, and
/// the pool is reported where there is one.
#[test]
fn test_parallel_page_batches_and_large_reads_round_trip() {
    let t = TempDir::new("parallel");
    let (opts, _) = options_for(t.path());
    // A store opened through the simple API has a platform, and so a pool,
    // of its own: its size is this store's choice.
    let opts = StoreOptions {
        t3_pool_threads: 3,
        ..opts
    };
    let store = Store::create(t.path().join("pages"), opts).unwrap();
    let pages = store.provision_page_region("pages", 16 << 20).unwrap();
    let bs = pages.block_size() as usize;
    let page = |round: u8, i: usize| -> Vec<u8> {
        (0..bs)
            .map(|k| (k as u8) ^ (i as u8).wrapping_mul(31) ^ round)
            .collect()
    };
    // 48 blocks spread over the first 8 MiB (a batch stages each write in a
    // pool buffer: 64 per size class by default).
    let stride = (8 << 20) / 48 / bs * bs;
    let mut one = pages.batch();
    one.write(pages.pos(0).unwrap(), &page(0, 0)).unwrap();
    one.commit().unwrap();
    assert_eq!(
        store.submit_pool_threads(),
        None,
        "a one-write batch started the pool"
    );
    for round in 1..=3u8 {
        let mut batch = pages.batch();
        // Staged in reverse order: the batch sorts them.
        for i in (0..48).rev() {
            batch
                .write(pages.pos((i * stride) as u64).unwrap(), &page(round, i))
                .unwrap();
        }
        batch.commit().unwrap();
        let mut back = vec![0u8; 8 << 20];
        assert_eq!(
            pages.read(pages.pos(0).unwrap(), &mut back).unwrap(),
            back.len()
        );
        for i in 0..48 {
            let at = i * stride;
            assert!(
                back[at..at + bs] == page(round, i),
                "round {round}: block {i} differs"
            );
        }
    }
    // Only the Linux synchronous tier runs operations on a pool; io_uring
    // runs them in the kernel.
    if store.report().io.interface == store_io::IoInterface::Synchronous {
        assert_eq!(store.submit_pool_threads(), Some(3));
    } else {
        assert_eq!(store.submit_pool_threads(), None);
    }
    assert_eq!(store.oldest_outstanding(), None);
}

/// The engine-layer submitter end to end on the real file system (D-098):
/// its operations run on the submit pool on Linux and on IOCP on Windows.
/// Appends and page writes complete in any order, land exactly where
/// submission said, every ticket syncs, and reads through the submitter
/// return every byte.
#[test]
fn test_submitter_round_trips_on_the_real_file_system() {
    use store_io::submit::{Done, SubmitterConfig};
    let full = |e: &Error| {
        matches!(
            e,
            Error::NotWritten {
                cause: store_io::NotWrittenCause::QueueFull,
                ..
            }
        )
    };
    let t = TempDir::new("submitter");
    let (opts, _) = options_for(t.path());
    let store = Store::create(t.path().join("sub"), opts).unwrap();
    let wal = store.provision_append_region("wal", 16 << 20).unwrap();
    let pages = store.provision_page_region("pages", 16 << 20).unwrap();
    let bs = pages.block_size() as usize;
    let record = |i: u64, len: usize| -> Vec<u8> {
        (0..len)
            .map(|k| (k as u8).wrapping_mul(13) ^ (i as u8).wrapping_mul(29) ^ 0xA5)
            .collect()
    };
    let mut sub = store
        .submitter(SubmitterConfig::default().with_depth(32))
        .unwrap();
    let mut placed = Vec::new();
    let mut done = Vec::new();
    for i in 0..64u64 {
        let len = 1 + (i as usize * 1531) % (3 * bs);
        let mut buf = sub.take_buf(len).unwrap();
        buf.as_mut_slice().copy_from_slice(&record(i, len));
        let at = loop {
            match sub.append(&wal, buf, store_io::submit::WriteMode::Plain, i) {
                Ok(pos) => break pos.offset(),
                Err(r) => {
                    assert!(full(&r.error), "{:?}", r.error);
                    buf = r.buf.unwrap();
                    done.extend(sub.wait(1).unwrap());
                }
            }
        };
        placed.push((at, len));
        let mut page = sub.take_buf(bs).unwrap();
        page.as_mut_slice().copy_from_slice(&record(1000 + i, bs));
        let pos = pages.pos(i * 3 * bs as u64).unwrap();
        while let Err(r) = sub.write(
            &pages,
            pos,
            page,
            store_io::submit::WriteMode::Plain,
            1000 + i,
        ) {
            assert!(full(&r.error), "{:?}", r.error);
            page = r.buf.unwrap();
            done.extend(sub.wait(1).unwrap());
        }
    }
    while sub.in_flight() > 0 {
        done.extend(sub.wait(sub.in_flight()).unwrap());
    }
    done.extend(sub.reap());
    assert_eq!(done.len(), 128);
    let mut last = None;
    for c in done {
        let Ok(Done::Written(w)) = c.outcome else {
            panic!("operation {} failed: {:?}", c.tag, c.outcome);
        };
        if c.tag >= 1000 {
            assert_eq!(w.ticket.pos().offset(), (c.tag - 1000) * 3 * bs as u64);
            pages.sync_through(&w.ticket).unwrap();
        } else {
            assert_eq!(w.ticket.pos().offset(), placed[c.tag as usize].0);
            if c.tag == 63 {
                last = Some(w.ticket);
            }
        }
    }
    wal.sync_through(&last.unwrap()).unwrap();
    for (i, &(at, len)) in placed.iter().enumerate() {
        let buf = sub.take_buf(len.div_ceil(bs) * bs).unwrap();
        sub.read_append(&wal, at, buf, i as u64).unwrap();
        let buf = sub.take_buf(bs).unwrap();
        let pos = pages.pos(i as u64 * 3 * bs as u64).unwrap();
        sub.read_page(&pages, pos, buf, 1000 + i as u64).unwrap();
        for c in sub.wait(2).unwrap() {
            let Ok(Done::Read { len: n }) = c.outcome else {
                panic!("read {} failed: {:?}", c.tag, c.outcome);
            };
            let got = c.buf.unwrap();
            if c.tag >= 1000 {
                assert_eq!(n, bs);
                assert!(got.as_slice() == record(c.tag, bs), "page {i} differs");
            } else {
                assert!(
                    got.as_slice()[..len] == record(i as u64, len),
                    "append {i} differs"
                );
                assert!(got.as_slice()[len..].iter().all(|&b| b == 0));
            }
        }
    }
    assert_eq!(sub.in_flight(), 0);
    drop(sub);
    assert_eq!(store.oldest_outstanding(), None);
}

/// Durable submitter writes and barriers on the real file system: the flush
/// thread's wake reaches a submitter blocked in its queue (IOCP on Windows,
/// the submit pool on Linux), and every receipt covers its write.
#[test]
fn test_durable_submitter_writes_on_the_real_file_system() {
    use store_io::submit::{Done, SubmitterConfig, WriteMode};
    let t = TempDir::new("durable");
    let (opts, _) = options_for(t.path());
    let store = Store::create(t.path().join("d"), opts).unwrap();
    let wal = store.provision_append_region("wal", 8 << 20).unwrap();
    let pages = store.provision_page_region("pages", 8 << 20).unwrap();
    let bs = pages.block_size() as usize;
    let mut sub = store
        .submitter(SubmitterConfig::default().with_depth(128))
        .unwrap();
    let mut done = Vec::new();
    for i in 0..32u64 {
        let mut buf = sub.take_buf(300 + i as usize).unwrap();
        buf.as_mut_slice().fill(i as u8);
        sub.append(&wal, buf, WriteMode::Durable, i).unwrap();
        // Reads in flight alongside, so waits block in the queue.
        let buf = sub.take_buf(bs).unwrap();
        sub.read_page(&pages, pages.pos(0).unwrap(), buf, 1000 + i)
            .unwrap();
    }
    sub.barrier_all(5000).unwrap();
    while sub.in_flight() > 0 {
        done.extend(sub.wait(1).unwrap());
    }
    done.extend(sub.reap());
    assert_eq!(done.len(), 65);
    for c in done {
        match (c.tag, c.outcome) {
            (5000, Ok(Done::Flushed)) => {}
            (t, Ok(Done::Read { .. })) if t >= 1000 => {}
            (t, Ok(Done::Written(w))) if t < 32 => {
                assert!(w.receipt.unwrap().covers(&w.ticket), "write {t}");
            }
            (t, other) => panic!("operation {t}: {other:?}"),
        }
    }
    drop(sub);
    assert_eq!(store.oldest_outstanding(), None);
}

/// A 4 MiB durable append from one buffer on the real file system: one OS
/// write (D-101), read back byte for byte.
#[test]
fn test_a_4_mib_submitter_append_on_the_real_file_system() {
    use store_io::submit::{Done, SubmitterConfig, WriteMode};
    let t = TempDir::new("large");
    let (opts, _) = options_for(t.path());
    let store = Store::create(t.path().join("l"), opts).unwrap();
    let wal = store.provision_append_region("wal", 16 << 20).unwrap();
    let mut sub = store.submitter(SubmitterConfig::default()).unwrap();
    let len = sub.max_buf();
    assert_eq!(len, 4 << 20);
    let bytes: Vec<u8> = (0..len).map(|k| (k as u8) ^ (k >> 13) as u8).collect();
    let mut buf = sub.take_buf(len).unwrap();
    buf.as_mut_slice().copy_from_slice(&bytes);
    let at = sub
        .append(&wal, buf, WriteMode::Durable, 1)
        .unwrap()
        .offset();
    let c = sub.wait(1).unwrap().next().unwrap();
    let Ok(Done::Written(w)) = c.outcome else {
        panic!("{:?}", c.outcome)
    };
    assert!(w.receipt.unwrap().covers(&w.ticket));
    let buf = sub.take_buf(len).unwrap();
    sub.read_append(&wal, at, buf, 2).unwrap();
    let c = sub.wait(1).unwrap().next().unwrap();
    assert!(
        c.buf.unwrap().as_slice() == bytes,
        "4 MiB read back differs"
    );
}

/// The store reports the kernel interface its I/O uses and why (D-082).
/// On Linux, `Auto` takes io_uring exactly when an explicit `Uring` choice
/// is accepted, `Sync` is always honoured, and a store written on one tier
/// reads back on the other.
#[test]
fn test_the_io_tier_is_chosen_and_reported() {
    use store_io::{IoInterface, LinuxTier, Op};
    let t = TempDir::new("tier");
    let (opts, _) = options_for(t.path());
    // Two lanes: on io_uring each ring holds two descriptors (a store's
    // parallel lanes and submitters; its lone-operation lanes are inline,
    // D-104), and tests running beside this one could otherwise exhaust the
    // process's limit and make `Auto` fall back (correctly, but not what is
    // checked here).
    let at = |tier| StoreOptions {
        linux_tier: tier,
        queues: 2,
        ..opts.clone()
    };
    let path = t.path().join("tiers");
    let store = Store::create(&path, at(LinuxTier::Sync)).unwrap();
    let io = store.report().io.clone();
    let line = format!("io               {} ({})", io.interface, io.reason);
    assert!(store.report().to_string().contains(&line));
    let wal = store.provision_append_region("wal", 1 << 20).unwrap();
    let ticket = wal.append(b"written on one tier").unwrap();
    wal.sync_through(&ticket).unwrap();
    drop((wal, store));
    if cfg!(windows) {
        assert_eq!(io.interface, IoInterface::Iocp);
        return;
    }
    assert_eq!(
        (io.interface, io.reason.as_str()),
        (IoInterface::Synchronous, "chosen")
    );
    let auto = Store::open_with(&path, at(LinuxTier::Auto)).unwrap();
    // A submitter's queue is a ring on the io_uring tier: the live path then
    // counts it (the report is as of the open, before any ring exists).
    let sub = auto
        .submitter(store_io::submit::SubmitterConfig::default())
        .unwrap();
    let auto_io = auto.io_path();
    drop(sub);
    let wal = auto.append_region("wal").unwrap();
    let mut back = vec![0u8; 19];
    wal.read(ticket.pos().offset(), &mut back).unwrap();
    assert_eq!(back, b"written on one tier");
    drop((wal, auto));
    let ro = Store::open_readonly_with(&path, at(LinuxTier::Sync)).unwrap();
    assert_eq!(ro.report().io.interface, IoInterface::Synchronous);
    drop(ro);
    match Store::open_with(&path, at(LinuxTier::Uring)) {
        Ok(s) => {
            assert_eq!(s.report().io.interface, IoInterface::IoUring);
            assert_eq!(auto_io.interface, IoInterface::IoUring);
            assert!(auto_io.reason.starts_with("auto"), "{auto_io:?}");
            assert!(!auto_io.reason.contains("synchronous"), "{auto_io:?}");
        }
        Err(Error::Io { op: Op::Queue, .. }) => {
            assert!(
                std::env::var_os("STORE_IO_EXPECT_URING").is_none(),
                "io_uring refused where STORE_IO_EXPECT_URING says it works"
            );
            assert_eq!(auto_io.interface, IoInterface::Synchronous);
            assert!(auto_io.reason.contains("io_uring refused"), "{auto_io:?}");
        }
        Err(e) => panic!("{e}"),
    }
}
