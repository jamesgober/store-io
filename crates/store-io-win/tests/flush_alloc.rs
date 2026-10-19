//! A queue's data flushes allocate nothing once warm: a `dsync` write (the
//! flush after its write) and a `FlushData`, both through the reused status
//! block of `NtFlushBuffersFileEx` (gate G8a). A counting allocator counts
//! every allocation in the process while they run.

#![cfg(windows)]
#![allow(clippy::unwrap_used, unused_results)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use store_io_buf::{BufPool, PoolConfig};
use store_io_platform::{CompletionBuf, FileName, IoOp, Platform, Queue, QueueConfig};
use store_io_win::{FlushMode, WinPlatform};

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

const BLOCK: usize = 4096;

#[test]
fn test_warm_data_flushes_allocate_nothing() {
    let path =
        std::env::temp_dir().join(format!("store-io-win-{}-flush-alloc", std::process::id()));
    let p = WinPlatform::new();
    let dir = p.open_dir(&path, true).unwrap();
    let pool = BufPool::new(&PoolConfig::uniform(BLOCK, BLOCK, BLOCK, 8)).unwrap();
    let f = p
        .create_file(&dir, &FileName::new("a.sio").unwrap())
        .unwrap();
    assert_eq!(f.flush_mode(), FlushMode::DataSyncOnly);
    p.allocate(&f, 1 << 20).unwrap();
    let mut q = p
        .queue(QueueConfig {
            depth: 4,
            ..QueueConfig::default()
        })
        .unwrap();
    let mut out = CompletionBuf::with_capacity(4);
    let mut round = |i: u64| {
        let buf = pool.take(BLOCK).unwrap();
        q.submit(
            IoOp::Write {
                file: &f,
                offset: (i % 64) * BLOCK as u64,
                buf,
                dsync: true,
            },
            1,
        )
        .unwrap();
        q.submit(IoOp::FlushData { file: &f }, 2).unwrap();
        let mut got = 0;
        while got < 2 {
            got += q.wait(1, &mut out).unwrap();
        }
        for c in out.drain() {
            assert!(c.result.is_ok(), "{c:?}");
        }
    };
    (0..20).for_each(&mut round);
    ARMED.store(true, Ordering::Relaxed);
    (0..50).for_each(&mut round);
    ARMED.store(false, Ordering::Relaxed);
    let n = COUNT.load(Ordering::Relaxed);
    drop((q, f, dir));
    let _cleanup = std::fs::remove_dir_all(&path);
    assert_eq!(n, 0, "allocations in 50 dsync writes and 50 data flushes");
}
