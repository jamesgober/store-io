//! [`Store`]: one container on one device.
//!
//! A store owns the container file, the ownership lock, the device's flush
//! domain, the buffer pool and the queues, and the in-memory copy of the
//! volume record and region table. Region handles share it through an `Arc`.
//!
//! The metadata lock (`meta`) is taken only by lifecycle operations
//! (provision, release, lookup by name); reads and writes of region data never
//! take it.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use store_io_buf::{BufPool, PoolConfig};
use store_io_core::class::{DurabilityClass, ReceiptLabel};
use store_io_core::decide::{ClassDecision, DurableOpen, Trust};
use store_io_core::errno::{Stage, classify};
use store_io_core::error::{
    CorruptionKind, Error, ErrorContext, FirstCause, Named, NotWrittenCause, Op, OsError,
    ReserveTag,
};
use store_io_core::evidence::Evidence;
use store_io_core::id::{Generation, RegionId, VolumeId};
use store_io_core::observe::{Observer, Snapshot};
use store_io_format::meta::{RegionKind, RegionName, TableEntry, VolumeMeta};
use store_io_platform::{FileName, IoOp, IoPath, LinuxTier, Platform, Queue, QueueConfig};

use crate::domain::Domain;
use crate::exec::{Lane, QueueSet, Ran, run};
use crate::frontier::AppendFrontier;
use crate::gate::Gate;
use crate::layout::{Layout, TABLE_OBJECT, VOLUME_OBJECT, region_object};
use crate::slots::{NextSlot, PairAt, PairRead, write_slot};
use crate::space::Accounting;

/// The container file name inside the store directory.
pub const CONTAINER: &str = "store.sio";

/// Explicit, reported choices for a store. Every durability-affecting field
/// appears in [`Store::report`].
#[derive(Debug, Clone)]
pub struct StoreOptions {
    /// Certificate, attestation and override inputs to the class decision.
    pub trust: Trust,
    /// Minimum block (slot) size in bytes; the device may require more.
    pub block_size: Option<u32>,
    /// Region-table slot size as a power of two (14 = 16 KiB: 253 regions).
    pub log2_table_slot: u8,
    /// Largest single I/O of the blocking layers, and the batch buffer size
    /// (bytes; rounded up to a power of two, 128 KiB to 1 GiB).
    pub max_io: usize,
    /// Buffers per size class in the pool, up to `max_io`.
    pub buffers_per_class: u32,
    /// Largest single write or read a `Submitter` takes, from one buffer, as
    /// one OS write or read operation (bytes; rounded up to a power of two,
    /// at most 1 GiB): the pool holds `large_buffers` buffers of that size,
    /// used by submitters only (D-101). Ignored when not above `max_io`.
    pub max_write: usize,
    /// Buffers of `max_write` bytes in the pool (0: none, so a submitter's
    /// largest operation is `max_io`). The defaults add 32 MiB on top of
    /// about 128 MiB of ordinary buffers per store: Windows charges the
    /// whole pool to the commit limit at creation; on both systems a buffer
    /// becomes resident when first used, and buffers are used in turn. A
    /// pool too large to map fails `create` and `open` with
    /// `NotWritten(PoolExhausted)`.
    pub large_buffers: u32,
    /// Queues shared by the blocking layers (0 = number of CPUs). On Linux
    /// they perform their operations inline on either tier; the parallel
    /// lanes, as many again and created on first use, are io_uring rings on
    /// the io_uring tier, each holding two file descriptors (its ring and
    /// its eventfd), as does each `Submitter`.
    pub queues: usize,
    /// Operations in flight per queue.
    pub queue_depth: u32,
    /// Worker threads for the platform's submit pool, which runs the
    /// independent operations of a page batch or a large read concurrently
    /// on the Linux synchronous tier (D-051, D-096); 0 = one per CPU, at most
    /// 32. The pool is shared by every store on the platform and sized by the
    /// first store that uses it (at most 256); other backends ignore this.
    pub t3_pool_threads: u32,
    /// Append-frontier ring size in blocks (bounds how far concurrent appends
    /// may run ahead of the oldest incomplete one).
    pub append_ring: usize,
    /// Fill new regions with the keyed incompressible pattern instead of
    /// zeros (for thin, virtual, deduplicating or zero-detecting stacks).
    pub keyed_fill: bool,
    /// The kernel interface on Linux (D-082): `Auto` (io_uring where the
    /// kernel accepts it), `Sync` or `Uring`. Read by the native API when
    /// it makes the platform; the choice is in [`StoreReport::io`]. Other
    /// systems ignore it.
    pub linux_tier: LinuxTier,
    /// Register the store's container file with each `Submitter`'s queue
    /// where the backend can (D-078, D-107: on Linux's io_uring tier, a
    /// read-only twin of it as a direct descriptor on the submitter's ring),
    /// so its reads skip the per-operation file lookup in the kernel; writes
    /// stay on the store's own descriptor, so a write in flight keeps the
    /// store locked until it completes. Best effort: a
    /// registration the kernel refuses is counted in `Store::io_path`, never
    /// an error. On by default: the gates measured reads 1.03x to 1.08x of
    /// an unregistered ring (G3d, CI); writes take the unregistered path.
    /// Buffers
    /// are never registered: reads into registered buffers hang forever on
    /// some kernels (at least Linux 6.5.0 and 6.6.0), which no probe
    /// detects (D-107).
    pub register_file: bool,
}

impl Default for StoreOptions {
    fn default() -> Self {
        Self {
            trust: Trust::default(),
            block_size: None,
            log2_table_slot: 14,
            max_io: 1 << 20,
            buffers_per_class: 64,
            max_write: 4 << 20,
            large_buffers: 8,
            queues: 0,
            queue_depth: 64,
            t3_pool_threads: 0,
            append_ring: 4096,
            keyed_fill: false,
            linux_tier: LinuxTier::Auto,
            register_file: true,
        }
    }
}

/// How durable writes are made durable on this store.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Policy {
    pub(crate) class: DurabilityClass,
    pub(crate) label: ReceiptLabel,
    /// Write with `dsync` (durable at completion): power-safe stores.
    pub(crate) dsync: bool,
    /// Barriers issue a device flush: every class except power-safe (where
    /// only regions forcing data sync flush, D-102).
    pub(crate) flush: bool,
}

/// Why a region the region table names could not be opened (D-090).
///
/// Such a region is never used as ready and its name stays reserved; a lookup
/// returns [`Error::Corruption`] naming the region, never `NotFound`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Unavailable {
    /// The container is shorter than the region's extent.
    ContainerShort {
        /// Bytes the container must hold for this region.
        needed: u64,
        /// Bytes it holds.
        actual: u64,
    },
    /// Neither copy of the region's header is valid.
    HeaderUnreadable,
    /// The header passes its checksum but holds impossible values.
    HeaderInvalid,
    /// The header belongs to another region or another kind of region.
    HeaderMismatch,
    /// The header says the region was released while the table says ready.
    HeaderReleased,
}

impl Unavailable {
    /// The corruption kind a lookup of such a region reports.
    #[must_use]
    pub fn corruption_kind(&self) -> CorruptionKind {
        match self {
            Self::ContainerShort { .. } => CorruptionKind::Truncated,
            Self::HeaderUnreadable => CorruptionKind::HeaderCrc,
            Self::HeaderInvalid | Self::HeaderMismatch | Self::HeaderReleased => {
                CorruptionKind::Metadata
            }
        }
    }
}

impl core::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ContainerShort { needed, actual } => write!(
                f,
                "the container holds {actual} bytes but the region needs {needed}"
            ),
            Self::HeaderUnreadable => f.write_str("neither copy of its header is valid"),
            Self::HeaderInvalid => f.write_str("its header holds impossible values"),
            Self::HeaderMismatch => f.write_str("its header belongs to another region or kind"),
            Self::HeaderReleased => {
                f.write_str("its header says released while the table says ready")
            }
        }
    }
}

/// A region the region table names that could not be opened (D-090).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnavailableRegion {
    /// The region's name.
    pub name: String,
    /// The region's id.
    pub id: RegionId,
    /// The region's kind.
    pub kind: RegionKind,
    /// Why it could not be opened.
    pub reason: Unavailable,
}

/// What the store learned about its device, and what it decided.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct StoreReport {
    /// Raw evidence from the platform.
    pub evidence: Evidence,
    /// The class decision.
    pub decision: ClassDecision,
    /// Block (slot) size in bytes.
    pub block_size: u64,
    /// Region-table capacity.
    pub table_capacity: usize,
    /// The volume identity.
    pub volume: VolumeId,
    /// Owner generation of this open (0 for read-only).
    pub owner_generation: u64,
    /// Whether the store was opened read-only.
    pub read_only: bool,
    /// The kernel interface the store's I/O uses, and why, as of create or
    /// open. Queues made later (a `Submitter`'s, the parallel lanes, created
    /// on first use) are counted live in [`Store::io_path`].
    pub io: IoPath,
    /// Regions the region table names that could not be opened, with the
    /// reason (D-090). Empty for a healthy store.
    pub unavailable: Vec<UnavailableRegion>,
}

impl core::fmt::Display for StoreReport {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let d = &self.decision;
        writeln!(f, "volume           {}", self.volume)?;
        writeln!(
            f,
            "io               {} ({})",
            self.io.interface, self.io.reason
        )?;
        writeln!(f, "class            {}", d.class)?;
        writeln!(f, "label            {}", d.label)?;
        writeln!(f, "durable open     {:?}", d.durable_open)?;
        if d.power_safe_candidate {
            writeln!(
                f,
                "note             device reports no volatile cache; health unreadable (power-safe candidate)"
            )?;
        }
        for r in d.reasons.iter() {
            writeln!(f, "reason           {r}")?;
        }
        for m in d.missing.iter() {
            writeln!(f, "missing          {m}")?;
        }
        writeln!(f, "flush execution  inline (leader)")?;
        writeln!(f, "block size       {}", self.block_size)?;
        writeln!(f, "table capacity   {} regions", self.table_capacity)?;
        writeln!(f, "filesystem       {:?}", self.evidence.fs.kind)?;
        writeln!(f, "bus              {:?}", self.evidence.device.bus)?;
        if let Some(m) = &self.evidence.device.model {
            writeln!(f, "model            {m}")?;
        }
        for u in &self.unavailable {
            writeln!(
                f,
                "unavailable      region {:?} (id {}, {:?}): {}",
                u.name,
                u.id.get(),
                u.kind,
                u.reason
            )?;
        }
        write!(
            f,
            "mode             {}",
            if self.read_only {
                "read-only"
            } else {
                "read-write"
            }
        )
    }
}

/// In-memory metadata, guarded by the metadata lock.
pub(crate) struct Meta {
    pub(crate) volume: VolumeMeta,
    pub(crate) volume_pair: PairRead,
    pub(crate) table_pair: PairRead,
    pub(crate) next_region_id: u32,
    pub(crate) entries: Vec<TableEntry>,
    pub(crate) container_len: u64,
}

/// One live region.
pub(crate) struct LiveRegion {
    pub(crate) id: RegionId,
    pub(crate) name: RegionName,
    pub(crate) kind: RegionKind,
    pub(crate) generation: Generation,
    pub(crate) data_offset: u64,
    pub(crate) data_size: u64,
    pub(crate) header: Mutex<PairRead>,
    pub(crate) frontier: Option<AppendFrontier>,
    /// Append regions: whether the append position is known (true when
    /// provisioned in this process; after a reopen, only after `resume_at`).
    pub(crate) positioned: std::sync::atomic::AtomicBool,
    /// Admission of data operations; closed by recycle and release.
    pub(crate) gate: Gate,
    /// Every barrier on the region issues a data flush, even on a
    /// power-safe store (D-102). Runtime only: never on disk. Shared by
    /// every generation of the region, so a setting made through any handle
    /// holds after a recycle.
    pub(crate) force_sync: Arc<std::sync::atomic::AtomicBool>,
}

impl LiveRegion {
    /// Whether barriers on this region always flush (D-102).
    pub(crate) fn forced(&self) -> bool {
        self.force_sync.load(std::sync::atomic::Ordering::Acquire)
    }
}

pub(crate) struct Inner<P: Platform> {
    /// Declared first, so the queues drop first (fields drop in declaration
    /// order): a queue whose operations run on worker threads waits for them
    /// in its drop, and none may land after the ownership lock is released.
    pub(crate) queues: QueueSet<P::Queue>,
    /// The flush thread (D-099), started by the first submitter request
    /// that needs a flush (a flush-required store, or a region forcing data
    /// sync); `None` inside if the OS refused it. Declared
    /// early too: it is stopped and joined before the file and the ownership
    /// lock are released.
    pub(crate) flusher: std::sync::OnceLock<Option<crate::flusher::Flusher<P>>>,
    pub(crate) platform: P,
    /// The store directory, held open for directory syncs.
    pub(crate) _dir: P::Dir,
    pub(crate) file: P::File,
    pub(crate) _lock: P::Lock,
    pub(crate) volume: VolumeId,
    pub(crate) layout: Layout,
    pub(crate) policy: Policy,
    pub(crate) domain: Domain,
    pub(crate) pool: BufPool,
    pub(crate) meta: Mutex<Meta>,
    pub(crate) regions: Mutex<Vec<Arc<LiveRegion>>>,
    pub(crate) report: StoreReport,
    pub(crate) read_only: bool,
    pub(crate) owner_generation: u64,
    pub(crate) opts: StoreOptions,
    /// Space accounting (lock order: `meta`, then `acct`).
    pub(crate) acct: Mutex<Accounting>,
    /// Unique per open store in this process: tickets and receipts of one
    /// open never vouch for another's writes.
    pub(crate) epoch: u64,
    /// Per-stage timing and counters (zero-sized without feature `observe`).
    pub(crate) observer: Observer,
    /// What `oldest_outstanding` queries have seen (D-095).
    pub(crate) seen: Mutex<crate::stats::Seen>,
    /// Live submitters' busy marks (D-098).
    pub(crate) submitters: Mutex<crate::stats::Submitters>,
}

/// A store: one container file on one device.
pub struct Store<P: Platform> {
    pub(crate) inner: Arc<Inner<P>>,
}

impl<P: Platform> core::fmt::Debug for Store<P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Store")
            .field("volume", &self.inner.volume)
            .field("class", &self.inner.policy.class)
            .field("read_only", &self.inner.read_only)
            .field("poisoned", &self.inner.domain.poisoned().is_some())
            .finish()
    }
}

impl<P: Platform> Clone for Store<P> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

pub(crate) fn io(op: Op, raw: OsError) -> Error {
    Error::Io {
        op,
        raw,
        ctx: ErrorContext::default(),
    }
}

pub(crate) fn lock_meta(m: &Mutex<Meta>) -> MutexGuard<'_, Meta> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A random 128-bit identity. Uniqueness matters, secrecy does not: the
/// standard library's OS-seeded hasher keys, mixed with the clock and the
/// process id through SplitMix64.
pub(crate) fn random_uuid() -> [u8; 16] {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    h.write_u32(std::process::id());
    let mut z = h.finish();
    let mut out = [0u8; 16];
    for chunk in out.chunks_exact_mut(8) {
        z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut x = z;
        x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        chunk.copy_from_slice(&(x ^ (x >> 31)).to_le_bytes());
    }
    out
}

impl<P: Platform> Store<P>
where
    P::Queue: Send,
{
    /// The failure that poisoned the store, if one has: every write and
    /// barrier fails with `Poisoned` from then on. A cheap check, for a
    /// health probe or a monitor.
    #[must_use]
    pub fn poisoned(&self) -> Option<FirstCause> {
        self.inner.domain.poisoned()
    }

    /// Event counters and sampled per-stage latencies since this handle was
    /// opened. With feature `observe` off (the default) nothing is recorded
    /// and the snapshot is all zeros.
    #[must_use]
    pub fn observe(&self) -> Snapshot {
        self.inner.observer.snapshot()
    }

    /// What the store learned about its device and what it decided.
    #[must_use]
    pub fn report(&self) -> &StoreReport {
        &self.inner.report
    }

    /// The kernel interface the store's I/O uses now, and why: as
    /// [`StoreReport::io`], with every queue made since counted (a ring the
    /// kernel refused for a `Submitter` or a parallel lane, and what
    /// served instead).
    #[must_use]
    pub fn io_path(&self) -> IoPath {
        self.inner.platform.io_path()
    }

    /// The volume identity.
    #[must_use]
    pub fn volume(&self) -> VolumeId {
        self.inner.volume
    }

    /// Poisons the device domain (for a caller's watchdog that decided the
    /// device is hung). Every later write and barrier fails with
    /// [`Error::Poisoned`]; reads keep working.
    pub fn poison(&self) {
        self.inner.domain.poison(FirstCause {
            op: None,
            raw: None,
        });
        for r in self.regions_snapshot() {
            if let Some(f) = &r.frontier {
                f.wake_all();
            }
        }
    }

    /// Flush-sharing counters of the device domain.
    #[must_use]
    pub fn domain_stats(&self) -> crate::domain::DomainStats {
        self.inner.domain.stats()
    }

    /// Worker threads in the submit pool that runs this store's page batches
    /// and large reads concurrently, once this store's first such call has
    /// started its parallel queues (the pool may have been started earlier by
    /// another store on the same platform, and stays `None` if they could
    /// not be started);
    /// `None` before that, or on a backend that needs no pool (IOCP, the
    /// simulator).
    #[must_use]
    pub fn submit_pool_threads(&self) -> Option<usize> {
        self.inner.queues.parallel_workers()
    }

    pub(crate) fn regions_snapshot(&self) -> Vec<Arc<LiveRegion>> {
        self.inner
            .regions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Names and kinds of the ready regions.
    #[must_use]
    pub fn regions(&self) -> Vec<(String, RegionKind)> {
        self.regions_snapshot()
            .iter()
            .map(|r| (r.name.as_str().to_owned(), r.kind))
            .collect()
    }

    pub(crate) fn find(
        &self,
        name: &str,
        kind: RegionKind,
        what: Named,
    ) -> Result<Arc<LiveRegion>, Error> {
        let name = RegionName::new(name).map_err(|_| Error::NotWritten {
            cause: NotWrittenCause::InvalidName,
            ctx: ErrorContext::default(),
        })?;
        if let Some(r) = self
            .regions_snapshot()
            .into_iter()
            .find(|r| r.name == name && r.kind == kind)
        {
            return Ok(r);
        }
        // A region the table names but open could not use is corrupt, not
        // missing: a caller must never mistake it for a fresh start (D-090).
        match self
            .inner
            .report
            .unavailable
            .iter()
            .find(|u| u.name == name.as_str() && u.kind == kind)
        {
            Some(u) => Err(Error::Corruption {
                kind: u.reason.corruption_kind(),
                ctx: ErrorContext {
                    region: Some(u.id),
                    range: None,
                },
            }),
            None => Err(Error::NotFound { what }),
        }
    }

    /// Regions the region table names that open could not use, with the
    /// reason (D-090). Empty for a healthy store. Their names stay reserved.
    #[must_use]
    pub fn unavailable_regions(&self) -> Vec<UnavailableRegion> {
        self.inner.report.unavailable.clone()
    }
}

/// Writes one slot durably according to the store's policy: a `dsync` write
/// on power-safe devices; a plain write plus a domain barrier otherwise.
/// Returns the pair's new state, derived from `prev` and the bytes written;
/// the device is never read back (D-069), so once the write is durable no
/// later step can fail or report the old version as current.
// The pair (location, previous state), the slot to write and the store's I/O
// context are all needed and none derives from another.
#[allow(clippy::too_many_arguments)]
pub(crate) fn durable_slot<P: Platform>(
    q: &mut Lane<P::Queue>,
    file: &P::File,
    pool: &BufPool,
    at: &PairAt,
    prev: &PairRead,
    next: &NextSlot<'_>,
    policy: &Policy,
    domain: &Domain,
) -> Result<PairRead, Error> {
    if let Some(first) = domain.poisoned() {
        return Err(Error::Poisoned { first });
    }
    let written = write_slot(q, file, pool, at, next, policy.dsync).inspect_err(|e| {
        if matches!(e, Error::DurabilityUnknown { .. }) {
            domain.poison(FirstCause {
                op: Some(Op::Write),
                raw: None,
            });
        }
    })?;
    if policy.flush {
        let need = domain.ticket();
        domain.barrier(need, || flush_data(q, file))?;
    }
    Ok(prev.after(written))
}

/// One device flush through a queue. A flush refused without an OS error
/// (a full queue) carries no code: none is invented.
pub(crate) fn flush_data<Q: Queue>(q: &mut Lane<Q>, file: &Q::File) -> Result<(), Option<OsError>> {
    match run(q, IoOp::FlushData { file }) {
        Ok(Ran::Done { result, .. }) => result.map(|_| ()).map_err(Some),
        Ok(Ran::Abandoned(raw)) => Err(Some(raw)),
        Err((_b, raw)) => Err(raw),
    }
}

pub(crate) fn container_name() -> Result<FileName, Error> {
    FileName::new(CONTAINER).map_err(|_| Error::NotWritten {
        cause: NotWrittenCause::InvalidName,
        ctx: ErrorContext::default(),
    })
}

pub(crate) fn refuse(d: &ClassDecision) -> Result<(), Error> {
    match d.durable_open {
        DurableOpen::Allowed | DurableOpen::Overridden => Ok(()),
        DurableOpen::RefusedUnsafe => Err(Error::UnsafeDevice { reasons: d.reasons }),
        DurableOpen::RefusedUnverified => Err(Error::Unverified {
            missing: d.missing,
            reasons: d.reasons,
        }),
    }
}

pub(crate) fn policy_of(d: &ClassDecision) -> Policy {
    let power_safe = d.class == DurabilityClass::PowerSafe;
    Policy {
        class: d.class,
        label: d.label,
        dsync: power_safe,
        flush: !power_safe,
    }
}

pub(crate) fn make_pool(
    ev: &Evidence,
    layout: &Layout,
    opts: &StoreOptions,
    large: bool,
) -> Result<BufPool, Error> {
    let align = [
        4096usize,
        ev.fs.dio_mem_align as usize,
        ev.device.logical_block as usize,
    ]
    .into_iter()
    .max()
    .unwrap_or(4096)
    .next_power_of_two();
    let min = (layout.block() as usize).max(align);
    let max = io_max(opts).max(min);
    let mut cfg = PoolConfig::uniform(align, min, max, opts.buffers_per_class.max(4));
    // One class of larger buffers for single large submitter operations
    // (D-101), fewer of them; a request between the two sizes takes one.
    // Never in the short-lived pools that read the metadata at open.
    let size = large_size(opts);
    if large && opts.large_buffers > 0 && size > max {
        cfg.classes.push(store_io_buf::ClassSpec {
            size,
            count: opts.large_buffers,
        });
    }
    BufPool::new(&cfg).map_err(|_| Error::NotWritten {
        cause: NotWrittenCause::PoolExhausted,
        ctx: ErrorContext::default(),
    })
}

/// The largest single I/O anything may ask for: 1 GiB (above 2 GiB a single
/// `pwrite` transfers short, above 4 GiB `WriteFile` refuses).
const LARGEST_IO: usize = 1 << 30;

/// The largest ordinary pool buffer, which the blocking layers never exceed:
/// `max_io` rounded up to a power of two, between 128 KiB and 1 GiB.
pub(crate) fn io_max(opts: &StoreOptions) -> usize {
    opts.max_io.clamp(2 << 16, LARGEST_IO).next_power_of_two()
}

/// The large class's buffer size: `max_write` rounded up to a power of two,
/// at most 1 GiB (no large class when that is not above [`io_max`]).
pub(crate) fn large_size(opts: &StoreOptions) -> usize {
    opts.max_write.clamp(1, LARGEST_IO).next_power_of_two()
}

pub(crate) fn make_queues<P: Platform>(
    platform: &P,
    opts: &StoreOptions,
) -> Result<QueueSet<P::Queue>, Error> {
    let n = if opts.queues == 0 {
        std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get)
    } else {
        opts.queues
    };
    let cfg = QueueConfig {
        depth: opts.queue_depth.max(1),
        ..QueueConfig::default()
    };
    let mut qs = Vec::with_capacity(n);
    for _ in 0..n {
        qs.push(platform.queue(cfg).map_err(|e| io(Op::Queue, e))?);
    }
    // One more queue, used only by the flush leader (D-070).
    let flush = platform.queue(cfg).map_err(|e| io(Op::Queue, e))?;
    Ok(QueueSet::with_flush_lane(qs, flush, cfg.depth as usize))
}

pub(crate) fn space_err(op: Op, raw: OsError, tag: u32) -> Error {
    match classify(Stage::Space, raw) {
        store_io_core::errno::ErrorKind::NoSpace => Error::NoSpace {
            tag: ReserveTag::Caller(tag),
            ctx: ErrorContext::default(),
        },
        _ => io(op, raw),
    }
}

pub(crate) fn meta_err() -> Error {
    Error::Corruption {
        kind: store_io_core::error::CorruptionKind::Metadata,
        ctx: ErrorContext::default(),
    }
}

pub(crate) fn corrupt() -> Error {
    meta_err()
}

pub(crate) fn pair_volume(volume: VolumeId, l: &Layout) -> PairAt {
    let (a, b) = l.volume_slots();
    PairAt {
        volume: *volume.as_bytes(),
        object: VOLUME_OBJECT,
        a,
        b,
        log2: l.log2_block,
    }
}

pub(crate) fn pair_table(volume: VolumeId, l: &Layout) -> PairAt {
    let (a, b) = l.table_slots();
    PairAt {
        volume: *volume.as_bytes(),
        object: TABLE_OBJECT,
        a,
        b,
        log2: l.log2_table,
    }
}

pub(crate) fn pair_region(volume: VolumeId, l: &Layout, e: &TableEntry) -> PairAt {
    PairAt {
        volume: *volume.as_bytes(),
        object: region_object(e.region_id),
        a: e.offset,
        b: e.offset + l.block(),
        log2: l.log2_block,
    }
}
