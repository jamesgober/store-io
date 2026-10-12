//! One-off diagnostics run before the workloads.
//!
//! Windows: `NtFlushBuffersFileEx(FLUSH_FLAGS_FILE_DATA_SYNC_ONLY)` on a
//! handle opened with `FILE_FLAG_OVERLAPPED` may return `STATUS_PENDING`
//! (0x103, non-negative) and complete the flush later. store-io-win
//! therefore never flushes through its overlapped queue handles: every flush
//! goes through each file's separate synchronous handle
//! (`crates/store-io-win/src/file.rs`, `flush_data`), and
//! `crates/store-io-win/src/sys.rs` (`flush_data_sync_only`) treats
//! `STATUS_PENDING` as a failure and leaks the `IO_STATUS_BLOCK` the kernel
//! may still write. This diagnostic issues the call on an overlapped handle,
//! once idle and once while another thread writes the same file, and counts
//! how often it pends on this machine: the evidence for that design.

use crate::workloads::Ctx;

/// Flushes per phase.
#[cfg(windows)]
const FLUSHES: usize = 400;

/// Runs the diagnostics; returns one line per finding.
#[cfg(windows)]
#[must_use]
pub fn run(ctx: &Ctx) -> Vec<String> {
    use std::sync::atomic::{AtomicBool, Ordering};

    use crate::aligned::AlignedBuf;
    use crate::raw::{OverlappedFile, RawFile};

    let path = ctx.path("diag-flush.dat");
    let r = (|| -> std::io::Result<(u64, u64)> {
        let (writer, _) = RawFile::create_ready(&path, 16 << 20)?;
        let mut o = OverlappedFile::open(&path, 1)?;
        let mut b = AlignedBuf::zeroed(4096);
        let mut phase = |o: &mut OverlappedFile| -> std::io::Result<u64> {
            let before = o.flush_pending;
            for i in 0..FLUSHES {
                b.as_mut_slice()[..8].copy_from_slice(&(i as u64).to_le_bytes());
                o.write_all_at(&[(((i % 2048) * 4096) as u64, b.as_slice())])?;
                o.flush_data()?;
            }
            Ok(o.flush_pending - before)
        };
        let idle = phase(&mut o)?;
        let stop = AtomicBool::new(false);
        let contended = std::thread::scope(|s| {
            let w = s.spawn(|| {
                let mut wb = AlignedBuf::zeroed(4096);
                let mut i = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    wb.as_mut_slice()[..8].copy_from_slice(&i.to_le_bytes());
                    let _ = writer.write_at(wb.as_slice(), (2048 + i % 2048) * 4096);
                    i += 1;
                }
            });
            let r = phase(&mut o);
            stop.store(true, Ordering::Relaxed);
            let _ = w.join();
            r
        })?;
        Ok((idle, contended))
    })();
    let _ = crate::workloads::remove(&path);
    match r {
        Ok((0, 0)) => vec![format!(
            "NtFlushBuffersFileEx(DATA_SYNC_ONLY) on a FILE_FLAG_OVERLAPPED | NO_BUFFERING handle after a dirty 4 KiB write never returned STATUS_PENDING: 0 of {FLUSHES} idle and 0 of {FLUSHES} while a second thread wrote the same file [measured]. That is not proof it cannot; store-io-win flushes only through synchronous handles and treats STATUS_PENDING as a failure either way"
        )],
        Ok((a, b)) => vec![format!(
            "NtFlushBuffersFileEx(DATA_SYNC_ONLY) on a FILE_FLAG_OVERLAPPED handle returned STATUS_PENDING {a} of {FLUSHES} times idle and {b} of {FLUSHES} times under concurrent writes [measured]. This is why store-io-win flushes only through synchronous handles and treats STATUS_PENDING as a failure"
        )],
        Err(e) => vec![format!("flush-pending diagnostic could not run: {e}")],
    }
}

/// Runs the diagnostics; returns one line per finding.
#[cfg(not(windows))]
#[must_use]
pub fn run(_ctx: &Ctx) -> Vec<String> {
    Vec::new()
}
