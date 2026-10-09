//! [`Store`]: one container on one device.
//!
//! A store owns the container file, the ownership lock, the device's flush
//! domain, the buffer pool and the queues, and the in-memory copy of the
//! volume record and region table. Region handles share it through an `Arc`.
//!
//! The metadata lock (`meta`) is taken only by lifecycle operations
//! (provision, release, lookup by name); reads and writes of region data never
//! take it.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use store_io_buf::{BufPool, PoolConfig};
use store_io_core::class::{DurabilityClass, ReceiptLabel};
use store_io_core::decide::{ClassDecision, DurableOpen, Trust, decide};
use store_io_core::errno::{Stage, classify};
use store_io_core::error::{
    Error, ErrorContext, FirstCause, Named, NotWrittenCause, Op, OsError, ReserveTag,
};
use store_io_core::evidence::Evidence;
use store_io_core::id::{Generation, RegionId, VolumeId};
use store_io_format::fill::fill_keyed_v1;
use store_io_format::meta::{
    FillPattern, REGION_META_LEN, RegionKind, RegionMeta, RegionName, RegionState, TableEntry,
    VOLUME_META_LEN, VolumeMeta, decode_table, encode_table,
};
use store_io_format::pair::Winner;
use store_io_platform::{FileMode, FileName, IoOp, Platform, Queue, QueueConfig};

use crate::domain::Domain;
use crate::exec::{Lane, QueueSet, run};
use crate::frontier::AppendFrontier;
use crate::layout::{Layout, TABLE_OBJECT, VOLUME_OBJECT, choose_log2_block, region_object};
use crate::slots::{NextSlot, PairAt, PairRead, plan_next, read_pair, write_slot};

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
    /// Largest single I/O the buffer pool serves (bytes, power of two).
    pub max_io: usize,
    /// Buffers per size class in the pool.
    pub buffers_per_class: u32,
    /// Queues shared by the blocking layers (0 = number of CPUs).
    pub queues: usize,
    /// Operations in flight per queue.
    pub queue_depth: u32,
    /// Append-frontier ring size in blocks (bounds how far concurrent appends
    /// may run ahead of the oldest incomplete one).
    pub append_ring: usize,
    /// Fill new regions with the keyed incompressible pattern instead of
    /// zeros (for thin, virtual, deduplicating or zero-detecting stacks).
    pub keyed_fill: bool,
}

impl Default for StoreOptions {
    fn default() -> Self {
        Self {
            trust: Trust::default(),
            block_size: None,
            log2_table_slot: 14,
            max_io: 1 << 20,
            buffers_per_class: 64,
            queues: 0,
            queue_depth: 64,
            append_ring: 4096,
            keyed_fill: false,
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
    /// Barriers issue a device flush: every class except power-safe.
    pub(crate) flush: bool,
}

/// What the store learned about its device, and what it decided.
#[derive(Debug, Clone)]
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
}

impl core::fmt::Display for StoreReport {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let d = &self.decision;
        writeln!(f, "volume           {}", self.volume)?;
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
}

pub(crate) struct Inner<P: Platform> {
    pub(crate) platform: P,
    /// The store directory, held open for directory syncs.
    pub(crate) _dir: P::Dir,
    pub(crate) file: P::File,
    pub(crate) _lock: P::Lock,
    pub(crate) _lock_file: Option<P::File>,
    pub(crate) volume: VolumeId,
    pub(crate) layout: Layout,
    pub(crate) policy: Policy,
    pub(crate) domain: Domain,
    pub(crate) pool: BufPool,
    pub(crate) queues: QueueSet<P::Queue>,
    pub(crate) meta: Mutex<Meta>,
    pub(crate) regions: Mutex<Vec<Arc<LiveRegion>>>,
    pub(crate) report: StoreReport,
    pub(crate) read_only: bool,
    pub(crate) owner_generation: u64,
    pub(crate) opts: StoreOptions,
    /// Unique per open store in this process: tickets and receipts of one
    /// open never vouch for another's writes.
    pub(crate) epoch: u64,
}

/// Source of [`Inner::epoch`].
static EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// A store: one container file on one device.
pub struct Store<P: Platform> {
    pub(crate) inner: Arc<Inner<P>>,
}

impl<P: Platform> Clone for Store<P> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

fn io(op: Op, raw: OsError) -> Error {
    Error::Io {
        op,
        raw,
        ctx: ErrorContext::default(),
    }
}

fn not_found_or(op: Op, raw: OsError, what: Named) -> Error {
    // ENOENT on every platform the engine supports (errno 2, Win32 2/3).
    if raw.code == 2 || raw.code == 3 {
        Error::NotFound { what }
    } else {
        io(op, raw)
    }
}

pub(crate) fn lock_meta(m: &Mutex<Meta>) -> MutexGuard<'_, Meta> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A random 128-bit identity. Uniqueness matters, secrecy does not: the
/// standard library's OS-seeded hasher keys, mixed with the clock and the
/// process id through SplitMix64.
fn random_uuid() -> [u8; 16] {
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
    /// Creates a new store in `path` (the directory is created if missing).
    ///
    /// The device is probed first; a durable open on an unsafe or unverified
    /// device is refused (and nothing is left behind) unless `opts.trust`
    /// attests or overrides it.
    ///
    /// # Errors
    ///
    /// [`Error::AlreadyExists`] if a store is already there;
    /// [`Error::UnsafeDevice`] / [`Error::Unverified`] on refusal;
    /// [`Error::Io`] / [`Error::DurabilityUnknown`] on platform failures.
    pub fn create(platform: P, path: &Path, opts: StoreOptions) -> Result<Self, Error> {
        let dir = platform
            .open_dir(path, true)
            .map_err(|e| io(Op::Create, e))?;
        let name = container_name()?;
        let file = platform.create_file(&dir, &name).map_err(|e| {
            if e.code == 17 || e.code == 80 {
                Error::AlreadyExists { what: Named::Store }
            } else {
                io(Op::Create, e)
            }
        })?;
        let lock = platform.lock_exclusive(&file).map_err(|_| Error::Locked)?;
        let built = Self::build_new(&platform, &dir, &file, &opts);
        match built {
            Ok((meta, report, layout, policy, pool, queues)) => {
                if let Err(e) = platform.sync_dir(&dir) {
                    return Err(Error::DurabilityUnknown {
                        op: Op::SyncDir,
                        raw: Some(e),
                        ctx: ErrorContext::default(),
                    });
                }
                let volume = report.volume;
                Ok(Self::assemble(
                    platform,
                    dir,
                    file,
                    lock,
                    None,
                    volume,
                    layout,
                    policy,
                    pool,
                    queues,
                    meta,
                    Vec::new(),
                    report,
                    false,
                    1,
                    opts,
                ))
            }
            Err(e) => {
                drop(lock);
                drop(file);
                let _removed = platform.unlink(&dir, &name);
                let _synced = platform.sync_dir(&dir);
                Err(e)
            }
        }
    }

    #[allow(clippy::type_complexity)]
    fn build_new(
        platform: &P,
        dir: &P::Dir,
        file: &P::File,
        opts: &StoreOptions,
    ) -> Result<
        (
            Meta,
            StoreReport,
            Layout,
            Policy,
            BufPool,
            QueueSet<P::Queue>,
        ),
        Error,
    > {
        let evidence = platform.probe(dir, file).map_err(|e| io(Op::Probe, e))?;
        let decision = decide(&evidence, &opts.trust);
        refuse(&decision)?;
        let layout = Layout {
            log2_block: choose_log2_block(&evidence, opts.block_size),
            log2_table: opts
                .log2_table_slot
                .clamp(choose_log2_block(&evidence, opts.block_size), 16),
        };
        let policy = policy_of(&decision);
        let pool = make_pool(&evidence, &layout, opts)?;
        let queues = make_queues(platform, opts)?;
        let volume = VolumeId::from_bytes(random_uuid());
        let header_len = layout.data_start();
        platform
            .allocate(file, header_len)
            .map_err(|e| space_err(Op::Allocate, e))?;
        let vmeta = VolumeMeta {
            log2_block_size: layout.log2_block,
            log2_table_slot_size: layout.log2_table,
            device_index: 0,
            owner_generation: 1,
            table_offset: layout.table_slots().0,
            created_unix_s: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
            features: 0,
        };
        let mut vbuf = [0u8; VOLUME_META_LEN];
        let _len = vmeta.encode(&mut vbuf).map_err(|_| meta_err())?;
        let mut tbuf = vec![0u8; layout.table() as usize];
        let tlen = encode_table(&mut tbuf, 0, &[]).map_err(|_| meta_err())?;
        let vat = pair_volume(volume, &layout);
        let tat = pair_table(volume, &layout);
        let empty = PairRead {
            report: store_io_format::pair::decide(
                store_io_format::slot::SlotStatus::Blank,
                store_io_format::slot::SlotStatus::Blank,
            ),
            payload: None,
        };
        let mut q = queues.get(0);
        let next_v = plan_next(&empty, &vbuf, 1).ok_or_else(meta_err)?;
        let _crc = write_slot(&mut q, file, &pool, &vat, &next_v, false)?;
        let next_t = plan_next(&empty, &tbuf[..tlen], 1).ok_or_else(meta_err)?;
        let _crc = write_slot(&mut q, file, &pool, &tat, &next_t, false)?;
        drop(q);
        platform
            .flush_all(file)
            .map_err(|e| Error::DurabilityUnknown {
                op: Op::FlushAll,
                raw: Some(e),
                ctx: ErrorContext::default(),
            })?;
        // Re-read what was written so the in-memory state matches the disk.
        let mut q = queues.get(0);
        let volume_pair = read_pair(&mut q, file, &pool, &vat)?;
        let table_pair = read_pair(&mut q, file, &pool, &tat)?;
        drop(q);
        let report = StoreReport {
            evidence,
            decision,
            block_size: layout.block(),
            table_capacity: layout.table_capacity(),
            volume,
            owner_generation: 1,
            read_only: false,
        };
        let meta = Meta {
            volume: vmeta,
            volume_pair,
            table_pair,
            next_region_id: 0,
            entries: Vec::new(),
            container_len: header_len,
        };
        Ok((meta, report, layout, policy, pool, queues))
    }

    /// Opens an existing store for reading and writing.
    ///
    /// Takes the ownership lock, makes a dead owner's completed writes
    /// durable (one full flush), reads the metadata, verifies every ready
    /// region, and bumps the owner generation. Modifies no caller bytes.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`], [`Error::Locked`], refusal errors,
    /// [`Error::Corruption`] for unreadable or foreign metadata,
    /// [`Error::Unsupported`] for a newer format.
    pub fn open(platform: P, path: &Path, opts: StoreOptions) -> Result<Self, Error> {
        Self::open_inner(platform, path, opts, false)
    }

    /// Opens an existing store read-only: no write rights, nothing modified,
    /// no refusal on unsafe or unverified devices (reading is always allowed).
    ///
    /// # Errors
    ///
    /// As [`Store::open`], without the refusal errors.
    pub fn open_readonly(platform: P, path: &Path, opts: StoreOptions) -> Result<Self, Error> {
        Self::open_inner(platform, path, opts, true)
    }

    fn open_inner(
        platform: P,
        path: &Path,
        opts: StoreOptions,
        read_only: bool,
    ) -> Result<Self, Error> {
        let dir = platform
            .open_dir(path, false)
            .map_err(|e| not_found_or(Op::Open, e, Named::Store))?;
        let name = container_name()?;
        // The lock needs write access; read-only stores keep a separate
        // handle for it and do all I/O through a read-only handle.
        let lock_file = platform
            .open_file(&dir, &name, FileMode::ReadWrite)
            .map_err(|e| not_found_or(Op::Open, e, Named::Store))?;
        let lock = platform
            .lock_exclusive(&lock_file)
            .map_err(|_| Error::Locked)?;
        let (file, lock_file) = if read_only {
            (
                platform
                    .open_file(&dir, &name, FileMode::ReadOnly)
                    .map_err(|e| io(Op::Open, e))?,
                Some(lock_file),
            )
        } else {
            (lock_file, None)
        };
        let evidence = platform.probe(&dir, &file).map_err(|e| io(Op::Probe, e))?;
        let decision = decide(&evidence, &opts.trust);
        if !read_only {
            refuse(&decision)?;
            // Drain a dead owner's completed-but-unflushed writes before
            // reading any slot (a later write to the other slot could
            // otherwise lose both copies at the next power cut).
            platform
                .flush_all(&file)
                .map_err(|e| Error::DurabilityUnknown {
                    op: Op::FlushAll,
                    raw: Some(e),
                    ctx: ErrorContext::default(),
                })?;
        }
        let policy = policy_of(&decision);
        let queues = make_queues(&platform, &opts)?;
        let container_len = platform.size(&file).map_err(|e| io(Op::Open, e))?;
        // Geometry is in the volume record; the record's own location depends
        // on it, so try every legal block size.
        let boot_pool = make_pool(
            &evidence,
            &Layout {
                log2_block: 16,
                log2_table: 16,
            },
            &opts,
        )?;
        let (volume, layout, vmeta, volume_pair) = {
            let mut q = queues.get(0);
            find_volume(&mut q, &file, &boot_pool)?
        };
        let pool = make_pool(&evidence, &layout, &opts)?;
        let tat = pair_table(volume, &layout);
        let mut q = queues.get(0);
        let table_pair = read_pair(&mut q, &file, &pool, &tat)?;
        let table_bytes = winner_payload(&table_pair)?;
        let table = decode_table(&table_bytes, layout.block()).map_err(|_| corrupt())?;
        let next_region_id = table.next_region_id;
        let entries: Vec<TableEntry> = table.iter().collect();
        let mut regions = Vec::new();
        for e in entries.iter().filter(|e| e.state == RegionState::Ready) {
            let end = layout
                .extent(e.data_size)
                .and_then(|l| e.offset.checked_add(l))
                .ok_or_else(corrupt)?;
            if end > container_len {
                // The table names space the container does not hold: never
                // presented as ready.
                continue;
            }
            let at = pair_region(volume, &layout, e);
            let header = read_pair(&mut q, &file, &pool, &at)?;
            let Some(bytes) = header.payload.clone() else {
                continue;
            };
            let Ok(rm) = RegionMeta::decode(&bytes) else {
                continue;
            };
            if rm.region_id != e.region_id || rm.state != RegionState::Ready || rm.kind != e.kind {
                continue;
            }
            let Some((_, info)) = header.report.winner_info() else {
                continue;
            };
            let Some(generation) = Generation::from_raw(info.generation) else {
                continue;
            };
            let frontier = (e.kind == RegionKind::Append).then(|| {
                AppendFrontier::new(
                    0,
                    e.data_size,
                    u32::from(layout.log2_block),
                    opts.append_ring,
                )
            });
            regions.push(Arc::new(LiveRegion {
                id: RegionId::from_raw(e.region_id),
                name: e.name,
                kind: e.kind,
                generation,
                data_offset: e.offset + 2 * layout.block(),
                data_size: e.data_size,
                header: Mutex::new(header),
                frontier,
                positioned: std::sync::atomic::AtomicBool::new(false),
            }));
        }
        drop(q);
        let mut meta = Meta {
            volume: vmeta,
            volume_pair,
            table_pair,
            next_region_id,
            entries,
            container_len,
        };
        let owner_generation = if read_only {
            0
        } else {
            let og = meta
                .volume
                .owner_generation
                .checked_add(1)
                .ok_or_else(corrupt)?;
            let mut vm = meta.volume;
            vm.owner_generation = og;
            let mut vbuf = [0u8; VOLUME_META_LEN];
            let _len = vm.encode(&mut vbuf).map_err(|_| meta_err())?;
            let vat = pair_volume(volume, &layout);
            let mut q = queues.get(0);
            let next = plan_next(&meta.volume_pair, &vbuf, og).ok_or_else(corrupt)?;
            let domain = Domain::new();
            durable_slot::<P>(&mut q, &file, &pool, &vat, &next, &policy, &domain)?;
            meta.volume_pair = read_pair(&mut q, &file, &pool, &vat)?;
            meta.volume = vm;
            og
        };
        let report = StoreReport {
            evidence,
            decision,
            block_size: layout.block(),
            table_capacity: layout.table_capacity(),
            volume,
            owner_generation,
            read_only,
        };
        Ok(Self::assemble(
            platform,
            dir,
            file,
            lock,
            lock_file,
            volume,
            layout,
            policy,
            pool,
            queues,
            meta,
            regions,
            report,
            read_only,
            owner_generation,
            opts,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn assemble(
        platform: P,
        dir: P::Dir,
        file: P::File,
        lock: P::Lock,
        lock_file: Option<P::File>,
        volume: VolumeId,
        layout: Layout,
        policy: Policy,
        pool: BufPool,
        queues: QueueSet<P::Queue>,
        meta: Meta,
        regions: Vec<Arc<LiveRegion>>,
        report: StoreReport,
        read_only: bool,
        owner_generation: u64,
        opts: StoreOptions,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                platform,
                _dir: dir,
                file,
                _lock: lock,
                _lock_file: lock_file,
                volume,
                layout,
                policy,
                domain: Domain::new(),
                pool,
                queues,
                meta: Mutex::new(meta),
                regions: Mutex::new(regions),
                report,
                read_only,
                owner_generation,
                opts,
                epoch: EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            }),
        }
    }

    /// What the store learned about its device and what it decided.
    #[must_use]
    pub fn report(&self) -> &StoreReport {
        &self.inner.report
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
        self.regions_snapshot()
            .into_iter()
            .find(|r| r.name == name && r.kind == kind)
            .ok_or(Error::NotFound { what })
    }

    /// Provisions a region of `data_size` bytes (rounded up to the block size)
    /// and returns its state.
    pub(crate) fn provision(
        &self,
        name: &str,
        kind: RegionKind,
        data_size: u64,
    ) -> Result<Arc<LiveRegion>, Error> {
        let inner = &self.inner;
        if inner.read_only {
            return Err(Error::NotWritten {
                cause: NotWrittenCause::ReadOnly,
                ctx: ErrorContext::default(),
            });
        }
        if let Some(first) = inner.domain.poisoned() {
            return Err(Error::Poisoned { first });
        }
        let rname = RegionName::new(name).map_err(|_| Error::NotWritten {
            cause: NotWrittenCause::InvalidName,
            ctx: ErrorContext::default(),
        })?;
        let block = inner.layout.block();
        let data_size =
            store_io_core::align::align_up(data_size, block).ok_or(Error::NotWritten {
                cause: NotWrittenCause::TooLarge,
                ctx: ErrorContext::default(),
            })?;
        if data_size / block >= u64::from(u32::MAX) {
            return Err(Error::NotWritten {
                cause: NotWrittenCause::TooLarge,
                ctx: ErrorContext::default(),
            });
        }
        let mut meta = lock_meta(&inner.meta);
        if self.regions_snapshot().iter().any(|r| r.name == rname) {
            return Err(Error::AlreadyExists {
                what: Named::Region,
            });
        }
        let ready_entries = meta
            .entries
            .iter()
            .filter(|e| e.state == RegionState::Ready)
            .count();
        if ready_entries >= inner.layout.table_capacity() {
            return Err(Error::NoSpace {
                tag: ReserveTag::Table,
                ctx: ErrorContext::default(),
            });
        }
        let extent = inner.layout.extent(data_size).ok_or(Error::NotWritten {
            cause: NotWrittenCause::TooLarge,
            ctx: ErrorContext::default(),
        })?;
        let (offset, reuse, grow) = inner
            .layout
            .place(&meta.entries, meta.container_len, extent)
            .ok_or(Error::NotWritten {
                cause: NotWrittenCause::TooLarge,
                ctx: ErrorContext::default(),
            })?;
        let end = offset.checked_add(extent).ok_or(Error::NotWritten {
            cause: NotWrittenCause::TooLarge,
            ctx: ErrorContext::default(),
        })?;
        if grow {
            inner
                .platform
                .allocate(&inner.file, end)
                .map_err(|e| space_err(Op::Allocate, e))?;
            meta.container_len = end;
        }
        let id = meta.next_region_id;
        let next_id = id.checked_add(1).ok_or(Error::NoSpace {
            tag: ReserveTag::Table,
            ctx: ErrorContext::default(),
        })?;
        let fill = if inner.opts.keyed_fill {
            FillPattern::KeyedV1
        } else {
            FillPattern::Zeros
        };
        let fill_key = if fill == FillPattern::KeyedV1 {
            random_uuid()
        } else {
            [0; 16]
        };
        // 1. Fill the header blocks and the data area with direct writes,
        //    blanking any old header pair at this location.
        self.fill(offset, extent, block, fill, &fill_key)?;
        // 2. Make the fill and the size durable.
        self.flush_all_accounted()?;
        // 3. Verify the extent is really written and out of the page cache.
        self.verify_ready(offset, extent)?;
        // 4. Region header slot A, generation 1, Ready.
        let rmeta = RegionMeta {
            region_id: id,
            kind,
            state: RegionState::Ready,
            fill,
            data_offset: offset + 2 * block,
            data_size,
            fill_key,
        };
        let mut rbuf = [0u8; REGION_META_LEN];
        let _len = rmeta.encode(&mut rbuf).map_err(|_| meta_err())?;
        let entry = TableEntry {
            region_id: id,
            kind,
            state: RegionState::Ready,
            name: rname,
            offset,
            data_size,
            fill,
        };
        let at = pair_region(inner.volume, &inner.layout, &entry);
        let blank = PairRead {
            report: store_io_format::pair::decide(
                store_io_format::slot::SlotStatus::Blank,
                store_io_format::slot::SlotStatus::Blank,
            ),
            payload: None,
        };
        let next = plan_next(&blank, &rbuf, inner.owner_generation).ok_or_else(meta_err)?;
        {
            let mut q = inner.queues.get(id as usize);
            durable_slot::<P>(
                &mut q,
                &inner.file,
                &inner.pool,
                &at,
                &next,
                &inner.policy,
                &inner.domain,
            )
            .inspect_err(|_| self.poison())?;
        }
        // 5. Table flip adding the entry (replacing a reused released entry).
        let mut entries = meta.entries.clone();
        match reuse {
            Some(i) => entries[i] = entry,
            None => entries.push(entry),
        }
        self.flip_table(&mut meta, next_id, &entries)?;
        meta.entries = entries;
        meta.next_region_id = next_id;
        let header = {
            let mut q = inner.queues.get(id as usize);
            read_pair(&mut q, &inner.file, &inner.pool, &at)?
        };
        let state = Arc::new(LiveRegion {
            id: RegionId::from_raw(id),
            name: rname,
            kind,
            generation: Generation::FIRST,
            data_offset: offset + 2 * block,
            data_size,
            header: Mutex::new(header),
            frontier: (kind == RegionKind::Append).then(|| {
                AppendFrontier::new(
                    0,
                    data_size,
                    u32::from(inner.layout.log2_block),
                    inner.opts.append_ring,
                )
            }),
            positioned: std::sync::atomic::AtomicBool::new(true),
        });
        inner
            .regions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Arc::clone(&state));
        Ok(state)
    }

    fn fill(
        &self,
        offset: u64,
        extent: u64,
        block: u64,
        fill: FillPattern,
        key: &[u8; 16],
    ) -> Result<(), Error> {
        let inner = &self.inner;
        let chunk = (inner.pool.max_len() as u64).min(1 << 20).max(block);
        let mut q = inner.queues.get(0);
        let mut at = 0u64;
        while at < extent {
            let len = chunk.min(extent - at);
            let mut buf = inner
                .pool
                .take(len as usize)
                .map_err(|_| Error::NotWritten {
                    cause: NotWrittenCause::PoolExhausted,
                    ctx: ErrorContext::default(),
                })?;
            match fill {
                // The two header blocks are always zero so no old header
                // survives; the data area gets the pattern.
                FillPattern::KeyedV1 if at >= 2 * block => {
                    fill_keyed_v1(key, at - 2 * block, buf.as_mut_slice())
                }
                _ => buf.as_mut_slice().fill(0),
            }
            let done = run(
                &mut q,
                IoOp::Write {
                    file: &inner.file,
                    offset: offset + at,
                    buf,
                    dsync: false,
                },
            )
            .map_err(|(_, raw)| {
                raw.map_or(
                    Error::NotWritten {
                        cause: NotWrittenCause::QueueFull,
                        ctx: ErrorContext::default(),
                    },
                    |r| space_err(Op::Write, r),
                )
            })?;
            match done.result {
                Ok(n) if n as u64 == len => {}
                Ok(_) => {
                    return Err(Error::DurabilityUnknown {
                        op: Op::Write,
                        raw: None,
                        ctx: ErrorContext::default(),
                    });
                }
                Err(raw) => return Err(space_err(Op::Write, raw)),
            }
            at += len;
        }
        Ok(())
    }

    fn flush_all_accounted(&self) -> Result<(), Error> {
        let inner = &self.inner;
        inner.platform.flush_all(&inner.file).map_err(|raw| {
            inner.domain.poison(FirstCause {
                op: Some(Op::FlushAll),
                raw: Some(raw),
            });
            Error::DurabilityUnknown {
                op: Op::FlushAll,
                raw: Some(raw),
                ctx: ErrorContext::default(),
            }
        })
    }

    fn verify_ready(&self, offset: u64, extent: u64) -> Result<(), Error> {
        let inner = &self.inner;
        let st = inner
            .platform
            .range_state(&inner.file, offset, extent)
            .map_err(|e| io(Op::Probe, e))?;
        use store_io_core::evidence::Tri;
        let bad = st.unwritten == Tri::Yes
            || st.shared == Tri::Yes
            || st.cached_pages.is_some_and(|n| n > 0)
            || st.valid_data == Tri::No;
        if bad {
            return Err(Error::NotWritten {
                cause: NotWrittenCause::NotReady,
                ctx: ErrorContext::default(),
            });
        }
        Ok(())
    }

    fn flip_table(
        &self,
        meta: &mut Meta,
        next_id: u32,
        entries: &[TableEntry],
    ) -> Result<(), Error> {
        let inner = &self.inner;
        let mut tbuf = vec![0u8; inner.layout.table() as usize];
        let len = encode_table(&mut tbuf, next_id, entries).map_err(|_| Error::NoSpace {
            tag: ReserveTag::Table,
            ctx: ErrorContext::default(),
        })?;
        let tat = pair_table(inner.volume, &inner.layout);
        let next = plan_next(&meta.table_pair, &tbuf[..len], inner.owner_generation)
            .ok_or_else(corrupt)?;
        let mut q = inner.queues.get(1);
        durable_slot::<P>(
            &mut q,
            &inner.file,
            &inner.pool,
            &tat,
            &next,
            &inner.policy,
            &inner.domain,
        )
        .inspect_err(|_| self.poison())?;
        meta.table_pair = read_pair(&mut q, &inner.file, &inner.pool, &tat)?;
        Ok(())
    }
}

/// Writes one slot durably according to the store's policy: a `dsync` write
/// on power-safe devices; a plain write plus a domain barrier otherwise.
pub(crate) fn durable_slot<P: Platform>(
    q: &mut Lane<P::Queue>,
    file: &P::File,
    pool: &BufPool,
    at: &PairAt,
    next: &NextSlot<'_>,
    policy: &Policy,
    domain: &Domain,
) -> Result<(), Error> {
    if let Some(first) = domain.poisoned() {
        return Err(Error::Poisoned { first });
    }
    let _crc = write_slot(q, file, pool, at, next, policy.dsync).inspect_err(|e| {
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
    Ok(())
}

/// One device flush through a queue.
pub(crate) fn flush_data<Q: Queue>(q: &mut Lane<Q>, file: &Q::File) -> Result<(), OsError> {
    match run(q, IoOp::FlushData { file }) {
        Ok(done) => done.result.map(|_| ()),
        Err((_b, raw)) => Err(raw.unwrap_or(OsError {
            code: 11,
            source: store_io_core::error::OsErrorSource::Sim,
        })),
    }
}

fn container_name() -> Result<FileName, Error> {
    FileName::new(CONTAINER).map_err(|_| Error::NotWritten {
        cause: NotWrittenCause::InvalidName,
        ctx: ErrorContext::default(),
    })
}

fn refuse(d: &ClassDecision) -> Result<(), Error> {
    match d.durable_open {
        DurableOpen::Allowed | DurableOpen::Overridden => Ok(()),
        DurableOpen::RefusedUnsafe => Err(Error::UnsafeDevice { reasons: d.reasons }),
        DurableOpen::RefusedUnverified => Err(Error::Unverified {
            missing: d.missing,
            reasons: d.reasons,
        }),
    }
}

fn policy_of(d: &ClassDecision) -> Policy {
    let power_safe = d.class == DurabilityClass::PowerSafe;
    Policy {
        class: d.class,
        label: d.label,
        dsync: power_safe,
        flush: !power_safe,
    }
}

fn make_pool(ev: &Evidence, layout: &Layout, opts: &StoreOptions) -> Result<BufPool, Error> {
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
    let max = opts.max_io.max(min).max(2 * (1 << 16)).next_power_of_two();
    BufPool::new(&PoolConfig::uniform(
        align,
        min,
        max,
        opts.buffers_per_class.max(4),
    ))
    .map_err(|_| Error::NotWritten {
        cause: NotWrittenCause::PoolExhausted,
        ctx: ErrorContext::default(),
    })
}

fn make_queues<P: Platform>(
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
    };
    let mut qs = Vec::with_capacity(n);
    for _ in 0..n {
        qs.push(platform.queue(cfg).map_err(|e| io(Op::Open, e))?);
    }
    Ok(QueueSet::new(qs, cfg.depth as usize))
}

fn space_err(op: Op, raw: OsError) -> Error {
    match classify(Stage::Space, raw) {
        store_io_core::errno::ErrorKind::NoSpace => Error::NoSpace {
            tag: ReserveTag::Caller(0),
            ctx: ErrorContext::default(),
        },
        _ => io(op, raw),
    }
}

fn meta_err() -> Error {
    Error::Corruption {
        kind: store_io_core::error::CorruptionKind::Metadata,
        ctx: ErrorContext::default(),
    }
}

fn corrupt() -> Error {
    meta_err()
}

fn winner_payload(p: &PairRead) -> Result<Vec<u8>, Error> {
    match p.report.winner {
        Winner::Confirmed(_) | Winner::Unconfirmed(_) => p.payload.clone().ok_or_else(corrupt),
        Winner::Refused => Err(Error::Unsupported {
            what: store_io_core::error::Capability::FormatVersion,
        }),
        Winner::Fork => Err(Error::Corruption {
            kind: store_io_core::error::CorruptionKind::Fork,
            ctx: ErrorContext::default(),
        }),
        Winner::Empty | Winner::Lost => Err(Error::Corruption {
            kind: store_io_core::error::CorruptionKind::HeaderCrc,
            ctx: ErrorContext::default(),
        }),
    }
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

/// Finds the volume record by trying every legal block size.
fn find_volume<Q: Queue>(
    q: &mut Lane<Q>,
    file: &Q::File,
    pool: &BufPool,
) -> Result<(VolumeId, Layout, VolumeMeta, PairRead), Error> {
    // Read the first slot header at the smallest size to learn the volume id;
    // either copy may be damaged, so try both locations per size.
    for log2 in crate::layout::MIN_LOG2_BLOCK..=crate::layout::MAX_LOG2_BLOCK {
        let size = 1u64 << log2;
        for probe_at in [0, size] {
            let buf = pool.take(4096).map_err(|_| Error::NotWritten {
                cause: NotWrittenCause::PoolExhausted,
                ctx: ErrorContext::default(),
            })?;
            let done = match run(
                q,
                IoOp::Read {
                    file,
                    offset: probe_at,
                    buf,
                },
            ) {
                Ok(d) => d,
                Err(_) => continue,
            };
            let Some(b) = done.buf else { continue };
            if done.result.ok() != Some(4096) || b.as_slice()[..8] != store_io_format::slot::MAGIC {
                continue;
            }
            let mut uuid = [0u8; 16];
            uuid.copy_from_slice(&b.as_slice()[32..48]);
            if b.as_slice()[78] != log2 {
                continue;
            }
            let layout0 = Layout {
                log2_block: log2,
                log2_table: log2,
            };
            let at = pair_volume(VolumeId::from_bytes(uuid), &layout0);
            let pair = read_pair(q, file, pool, &at)?;
            let bytes = match winner_payload(&pair) {
                Ok(b) => b,
                Err(Error::Unsupported { what }) => return Err(Error::Unsupported { what }),
                Err(_) => continue,
            };
            let vm = VolumeMeta::decode(&bytes).map_err(|_| corrupt())?;
            if vm.log2_block_size != log2 {
                continue;
            }
            let layout = Layout {
                log2_block: log2,
                log2_table: vm.log2_table_slot_size,
            };
            if vm.table_offset != layout.table_slots().0 {
                return Err(corrupt());
            }
            return Ok((VolumeId::from_bytes(uuid), layout, vm, pair));
        }
    }
    Err(Error::Corruption {
        kind: store_io_core::error::CorruptionKind::HeaderCrc,
        ctx: ErrorContext::default(),
    })
}
