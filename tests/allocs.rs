//! Gate G8a as a test: a blocking durable write allocates nothing once the
//! store is warm, on Linux and Windows (the harness measures it too; this
//! keeps a regression out of every test run). A counting allocator counts every
//! allocation in the process, on any thread, while 100 durable page writes
//! run after 200 warm-up writes.

#![cfg(any(windows, target_os = "linux"))]
#![allow(clippy::unwrap_used, unused_results, unused_must_use)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use store_io::{Store, StoreOptions, Trust};

static ARMED: AtomicBool = AtomicBool::new(false);
static COUNT: AtomicU64 = AtomicU64::new(0);

struct Counting;

// SAFETY: every call is passed to the system allocator unchanged.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            COUNT.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: as the caller's contract.
        unsafe { System.alloc(l) }
    }

    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            COUNT.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: as the caller's contract.
        unsafe { System.realloc(p, l, new) }
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: as the caller's contract.
        unsafe { System.dealloc(p, l) }
    }
}

#[global_allocator]
static A: Counting = Counting;

#[test]
fn test_a_warm_durable_write_allocates_nothing() {
    let base = std::env::var_os("STORE_IO_TEST_DIR")
        .or_else(|| std::env::var_os("HOME"))
        .map_or_else(std::env::temp_dir, std::path::PathBuf::from)
        .join(format!(".store-io-allocs-{}", std::process::id()));
    let _gone = std::fs::remove_dir_all(&base);
    let opts = StoreOptions {
        trust: Trust {
            override_refusal: true,
            ..Trust::default()
        },
        ..StoreOptions::default()
    };
    let store = Store::create(&base, opts).unwrap();
    let pages = store.provision_page_region("p", 1 << 20).unwrap();
    let blk = pages.block_size();
    let data = vec![7u8; blk as usize];
    let write = |i: u64| {
        let _receipt = pages
            .write_durable(pages.pos((i % 64) * blk).unwrap(), &data)
            .unwrap();
    };
    (0..200).for_each(write);
    ARMED.store(true, Ordering::Relaxed);
    (0..100).for_each(write);
    ARMED.store(false, Ordering::Relaxed);
    let n = COUNT.load(Ordering::Relaxed);
    drop((pages, store));
    let _cleanup = std::fs::remove_dir_all(&base);
    assert_eq!(n, 0, "allocations in 100 warm durable writes");
}
