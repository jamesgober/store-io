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
