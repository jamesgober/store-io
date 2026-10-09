//! Fault injection plan.
//!
//! Faults are either targeted ("the 3rd flush fails") or probabilistic
//! (per-million rates drawn from the world's seeded generator), so every run
//! is reproducible from its seed and plan.

/// How a failed flush behaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FlushFailure {
    /// The flush returns an error and the cached writes stay cached.
    #[default]
    Keep,
    /// The flush returns an error and the cached writes are lost (the
    /// fsyncgate behaviour: a later flush then succeeds with nothing to do).
    Drop,
}

/// What the simulated device and filesystem do wrong, and when.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FaultPlan {
    /// The n-th write completion (1-based) fails with EIO.
    pub fail_write: Option<u64>,
    /// The n-th flush (data or full, 1-based) fails with EIO.
    pub fail_flush: Option<u64>,
    /// What a failed flush does to cached writes.
    pub flush_failure: FlushFailure,
    /// Every flush reports success without persisting anything.
    pub lying_flush: bool,
    /// The n-th write completion transfers only half its bytes (rounded down
    /// to a logical block) and reports the short count.
    pub short_write: Option<u64>,
    /// The n-th read completion transfers only half its bytes (rounded down
    /// to a logical block) and reports the short count.
    pub short_read: Option<u64>,
    /// The n-th write lands `delta` bytes away from its offset (a misdirected
    /// write) and reports success.
    pub misdirect_write: Option<(u64, i64)>,
    /// The n-th write is silently lost: reports success, persists nothing.
    pub lose_write: Option<u64>,
    /// Reads overlapping these `(file, start, end)` byte ranges fail with EIO
    /// (latent sector errors).
    pub bad_ranges: Vec<(u32, u64, u64)>,
    /// Allocation beyond this many total bytes fails with ENOSPC.
    pub capacity: Option<u64>,
    /// The file has pages in the OS page cache (a foreign buffered reader).
    pub cached_pages: bool,
    /// Per-million probability that any write completion fails with EIO.
    pub write_eio_ppm: u32,
    /// Per-million probability that any flush fails with EIO.
    pub flush_eio_ppm: u32,
}
