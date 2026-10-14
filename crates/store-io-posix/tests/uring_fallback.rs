//! Under `LinuxTier::Auto`, a queue whose ring the kernel cannot set up for
//! want of resources is a synchronous queue, counted in the report; under
//! an explicit `Uring` it is an error. Its own test binary: it lowers the
//! process's descriptor limit, which would disturb other tests.

#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stderr,
    unused_results
)]

use store_io_buf::{BufPool, PoolConfig};
use store_io_platform::{
    CompletionBuf, FileName, IoInterface, IoOp, LinuxTier, Platform, Queue, QueueConfig,
};
use store_io_posix::PosixPlatform;

/// The soft descriptor limit, lowered for the test and restored on drop.
struct Limit(libc::rlimit);

impl Limit {
    /// Lowers the soft limit to `n`.
    fn lower_to(n: u64) -> Self {
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: getrlimit(2) and setrlimit(2) with a valid `rlimit`; only
        // this test's own process is affected.
        unsafe {
            assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut lim), 0);
            let saved = lim;
            lim.rlim_cur = n.min(lim.rlim_max);
            assert_eq!(libc::setrlimit(libc::RLIMIT_NOFILE, &raw const lim), 0);
            Self(saved)
        }
    }
}

impl Drop for Limit {
    fn drop(&mut self) {
        // SAFETY: as in `lower_to`, restoring the saved limit.
        let _restored = unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raw const self.0) };
    }
}

#[test]
fn test_auto_falls_back_to_synchronous_queues_without_descriptors() {
    let auto = PosixPlatform::with_tier(LinuxTier::Auto).unwrap();
    if auto.io_path().interface != IoInterface::IoUring {
        assert!(
            std::env::var_os("STORE_IO_EXPECT_URING").is_none(),
            "io_uring refused where STORE_IO_EXPECT_URING says it works"
        );
        eprintln!("io_uring refused here: nothing to fall back from");
        return;
    }
    let explicit = PosixPlatform::with_tier(LinuxTier::Uring).unwrap();
    // A file and buffers made before the limit drops.
    let dir_path = std::path::PathBuf::from(std::env::var_os("HOME").unwrap())
        .join(format!(".store-io-uring-fallback-{}", std::process::id()));
    let dir = auto.open_dir(&dir_path, true).unwrap();
    let file = auto
        .create_file(&dir, &FileName::new("f").unwrap())
        .unwrap();
    auto.allocate(&file, 4096).unwrap();
    let pool = BufPool::new(&PoolConfig::uniform(4096, 4096, 4096, 2)).unwrap();
    let open = std::fs::read_dir("/proc/self/fd").unwrap().count() as u64;
    let limit = Limit::lower_to(open + 4);
    let mut queues = Vec::new();
    for _ in 0..8 {
        queues.push(auto.queue(QueueConfig::default()).unwrap());
    }
    let reason = auto.io_path().reason;
    assert!(reason.contains("synchronous"), "{reason}");
    assert_eq!(
        explicit
            .queue(QueueConfig::default())
            .map(|_| ())
            .map_err(|e| e.code),
        Err(libc::EMFILE),
        "an explicit io_uring choice never falls back"
    );
    // The last queue fell back; it writes and reads back.
    let q = queues.last_mut().unwrap();
    let mut buf = pool.take(4096).unwrap();
    buf.as_mut_slice().fill(0x77);
    q.submit(
        IoOp::Write {
            file: &file,
            offset: 0,
            buf,
            dsync: true,
        },
        1,
    )
    .unwrap();
    let mut out = CompletionBuf::with_capacity(1);
    assert_eq!(q.wait(1, &mut out).unwrap(), 1);
    assert_eq!(out.drain().next().unwrap().result, Ok(4096));
    q.submit(
        IoOp::Read {
            file: &file,
            offset: 0,
            buf: pool.take(4096).unwrap(),
        },
        2,
    )
    .unwrap();
    assert_eq!(q.wait(1, &mut out).unwrap(), 1);
    let c = out.drain().next().unwrap();
    assert_eq!(c.result, Ok(4096));
    assert!(c.buf.unwrap().as_slice().iter().all(|&b| b == 0x77));
    drop(queues);
    drop(limit);
    drop(file);
    let _cleanup = std::fs::remove_dir_all(&dir_path);
}
