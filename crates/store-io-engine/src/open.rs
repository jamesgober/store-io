//! Opening a store: finding the volume record, reading the region table,
//! verifying every region against its header, and assembling the handle.

use std::path::Path;
use std::sync::{Arc, Mutex};

use store_io_buf::BufPool;
use store_io_core::decide::decide;
use store_io_core::error::{Error, ErrorContext, Named, NotWrittenCause, Op, OsError};
use store_io_core::id::{Generation, RegionId, VolumeId};
use store_io_core::observe::Observer;
use store_io_format::meta::{
    RegionKind, RegionMeta, RegionState, TableEntry, VolumeMeta, decode_table,
};
use store_io_format::pair::Winner;
use store_io_platform::{FileMode, IoOp, Platform, Queue, QueueConfig};

use crate::domain::Domain;
use crate::exec::{Lane, QueueSet, Ran, run};
use crate::frontier::AppendFrontier;
use crate::gate::Gate;
use crate::layout::Layout;
use crate::slots::{PairRead, read_pair};
use crate::space::Accounting;
use crate::store::*;

impl<P: Platform> Store<P>
where
    P::Queue: Send,
{
    /// Opens an existing store for reading and writing.
    ///
    /// Takes the ownership lock, makes a dead owner's completed writes
    /// durable (one full flush), reads the metadata, verifies every ready
    /// region, and bumps the owner generation. Modifies no caller bytes.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`], [`Error::Locked`] (another handle, in this or
    /// another process, holds the store), refusal errors,
    /// [`Error::Corruption`] for unreadable or foreign metadata,
    /// [`Error::Unsupported`] (`FormatVersion`) for a newer format and for
    /// a format-1 store (read-only in 0.5: open it with
    /// [`Store::open_readonly`], or copy it to format 2 with
    /// [`copy_store`](crate::copy_store)) before anything is written,
    /// [`Error::DurabilityUnknown`] when a dead owner's writes may not be
    /// durable: the drain's flush failed, or the probe did (on macOS its
    /// flush check is a flush; elsewhere it reads, and fails only on an I/O
    /// error),
    /// [`Error::Io`] (`Io(Lock)` on a file system that cannot lock).
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
        // A read-only open needs no write rights (a read-only mount, a
        // backup copy): one read-only handle and the shared lock, which
        // other readers may hold too and which excludes the writer (D-092).
        let mode = if read_only {
            FileMode::ReadOnly
        } else {
            FileMode::ReadWrite
        };
        let file = platform
            .open_file(&dir, &name, mode)
            .map_err(|e| not_found_or(Op::Open, e, Named::Store))?;
        let lock = if read_only {
            platform.lock_shared(&file)
        } else {
            platform.lock_exclusive(&file)
        }
        .map_err(|raw| {
            // Another owner holds it; anything else (a file system that
            // cannot lock) is a plain failure, not "locked".
            if store_io_core::errno::is_lock_held(raw) {
                Error::Locked
            } else {
                io(Op::Lock, raw)
            }
        })?;
        // A writable open's probe may flush (the macOS flush check): its
        // failure leaves a dead owner's writes unknown, as the drain's below
        // does. A reader's probe never flushes.
        let evidence = platform.probe(&dir, &file).map_err(|e| {
            if read_only {
                io(Op::Probe, e)
            } else {
                Error::DurabilityUnknown {
                    op: Op::Probe,
                    raw: Some(e),
                    ctx: ErrorContext::default(),
                }
            }
        })?;
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
        let lanes = if read_only || !policy.flush {
            1
        } else {
            flush_slots(&platform, &opts)
        };
        let queues = make_queues(&platform, &opts, lanes)?;
        let container_len = platform.size(&file).map_err(|e| io(Op::Open, e))?;
        // Geometry is in the volume record; the record's own location depends
        // on it, so try every legal block size.
        let boot_pool = make_pool(&evidence, &Layout::v1(16, 16), &opts, false)?;
        let (volume, layout, found, volume_pair) = {
            let mut q = queues.get(0);
            find_volume(&mut q, &file, &boot_pool)?
        };
        let pool = make_pool(&evidence, &layout, &opts, true)?;
        let vmeta = match found {
            // Format 1 is read-only from 0.5 (D-112, D-116): refused before
            // any write (nothing above wrote).
            Found::V1(_) if !read_only => {
                return Err(Error::Unsupported {
                    what: store_io_core::error::Capability::FormatVersion,
                });
            }
            Found::V1(vm) => vm,
            Found::V2(vm) => {
                let opened = {
                    let mut q = queues.get(0);
                    crate::v2::open_metadata::<P>(
                        &file,
                        &mut q,
                        &pool,
                        &layout,
                        volume,
                        vm,
                        volume_pair,
                        container_len,
                        read_only,
                        &policy,
                        opts.append_window,
                        &mut || {
                            let cfg = QueueConfig {
                                depth: opts.queue_depth.max(1),
                                parallel: true,
                                pool_threads: opts.t3_pool_threads,
                                ..QueueConfig::default()
                            };
                            platform.queue(cfg).ok().map(|q| (q, cfg.depth as usize))
                        },
                    )?
                };
                let report = StoreReport {
                    evidence,
                    decision,
                    block_size: layout.block(),
                    table_capacity: layout.pages as usize
                        * usize::from(store_io_format::meta2::entries_per_page(layout.log2_table)),
                    volume,
                    owner_generation: opened.owner_generation,
                    read_only: opened.read_only,
                    io: platform.io_path(),
                    unavailable: opened.unavailable,
                    format: store_io_format::slot::FORMAT_2,
                    metadata: opened.report,
                    flushes_in_flight: 1,
                };
                return Ok(Self::assemble(
                    platform,
                    dir,
                    file,
                    lock,
                    volume,
                    layout,
                    policy,
                    pool,
                    queues,
                    opened.meta,
                    opened.regions,
                    report,
                    opened.read_only,
                    opened.owner_generation,
                    opts,
                ));
            }
        };
        let tat = pair_table(volume, &layout);
        let mut q = queues.get(0);
        let table_pair = read_pair(&mut q, &file, &pool, &tat)?;
        let table_bytes = winner_payload(&table_pair)?;
        let table = decode_table(&table_bytes, layout.block()).map_err(|_| corrupt())?;
        let next_region_id = table.next_region_id;
        let entries: Vec<TableEntry> = table.iter().collect();
        let mut regions = Vec::new();
        // A region that fails any check below is reported, with the reason,
        // and never used as ready (D-090).
        let mut unavailable = Vec::new();
        for e in entries.iter().filter(|e| e.state == RegionState::Ready) {
            let mut skip = |reason| {
                unavailable.push(UnavailableRegion {
                    name: e.name.as_str().to_owned(),
                    id: RegionId::from_raw(u64::from(e.region_id)),
                    kind: e.kind,
                    reason,
                });
            };
            let end = layout
                .extent(e.data_size)
                .and_then(|l| e.offset.checked_add(l))
                .ok_or_else(corrupt)?;
            if end > container_len {
                // The table names space the container does not hold.
                skip(Unavailable::ContainerShort {
                    needed: end,
                    actual: container_len,
                });
                continue;
            }
            let at = pair_region(volume, &layout, e);
            let header = read_pair(&mut q, &file, &pool, &at)?;
            let (Some(bytes), Some((_, info))) =
                (header.payload.clone(), header.report.winner_info())
            else {
                skip(Unavailable::HeaderUnreadable);
                continue;
            };
            let Ok(rm) = RegionMeta::decode(&bytes) else {
                skip(Unavailable::HeaderInvalid);
                continue;
            };
            if rm.region_id != e.region_id || rm.kind != e.kind {
                skip(Unavailable::HeaderMismatch);
                continue;
            }
            if rm.state != RegionState::Ready {
                skip(Unavailable::HeaderReleased);
                continue;
            }
            let Some(generation) = Generation::from_raw(info.generation) else {
                skip(Unavailable::HeaderInvalid);
                continue;
            };
            let frontier = (e.kind == RegionKind::Append).then(|| {
                AppendFrontier::new(
                    0,
                    e.data_size,
                    u32::from(layout.log2_block),
                    crate::frontier::ring_for(
                        opts.append_window,
                        e.data_size,
                        u32::from(layout.log2_block),
                    ),
                )
            });
            regions.push(Arc::new(LiveRegion {
                id: RegionId::from_raw(u64::from(e.region_id)),
                name: e.name,
                kind: e.kind,
                generation,
                map: crate::regionmap::RegionMap::single(
                    e.offset + 2 * layout.block(),
                    e.data_size,
                ),
                header_at: [e.offset, e.offset + layout.block()],
                header: Mutex::new(header),
                frontier,
                positioned: std::sync::atomic::AtomicBool::new(false),
                force_sync: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                read_only: std::sync::atomic::AtomicBool::new(false),
                superseded: false,
                foreign: false,
                gate: Gate::new(),
            }));
        }
        drop(q);
        let meta = Meta {
            volume: vmeta,
            volume_pair,
            table_pair,
            next_region_id,
            entries,
            container_len,
            v2: None,
            unavailable: Vec::new(),
            repairs: Vec::new(),
        };
        // Format 1 opens for reading only (a writable open was refused
        // above): no owner generation is taken, nothing is written.
        debug_assert!(read_only, "a writable format-1 open was refused");
        let owner_generation = 0;
        let report = StoreReport {
            evidence,
            decision,
            block_size: layout.block(),
            table_capacity: layout.table_capacity(),
            volume,
            owner_generation,
            read_only,
            io: platform.io_path(),
            unavailable,
            format: store_io_format::slot::FORMAT_1,
            metadata: MetadataReport::default(),
            flushes_in_flight: 1,
        };
        Ok(Self::assemble(
            platform,
            dir,
            file,
            lock,
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
    pub(crate) fn assemble(
        platform: P,
        dir: P::Dir,
        file: P::File,
        lock: P::Lock,
        volume: VolumeId,
        layout: Layout,
        policy: Policy,
        pool: BufPool,
        mut queues: QueueSet<P::Queue>,
        meta: Meta,
        regions: Vec<Arc<LiveRegion>>,
        report: StoreReport,
        read_only: bool,
        owner_generation: u64,
        opts: StoreOptions,
    ) -> Self {
        let acct = match meta.v2.as_deref() {
            Some(v2) => Accounting::from_usage(&v2.usage),
            None => Accounting::from_entries(&meta.entries, layout.block()),
        };
        // Every flush lane past the first needs a handle of its own. A store
        // that flushes nothing (read-only: asked for, or found so by a
        // format-2 open after its queues were made; or on a device that
        // needs no flush) opens none, and a lane whose handle could not be
        // made is closed.
        let lanes = if read_only || !policy.flush {
            1
        } else {
            queues.flush_lanes()
        };
        let flush_files = flush_handles(&platform, &file, lanes);
        queues.keep_flush_lanes(1 + flush_files.len());
        // A read-only store changes no metadata: no metadata lane (D-135).
        if read_only {
            queues.drop_meta_lane();
        }
        let mut report = report;
        report.flushes_in_flight = 1 + flush_files.len() as u8;
        let mut meta = meta;
        meta.unavailable = report.unavailable.clone();
        let frozen = std::sync::atomic::AtomicBool::new(report.metadata.frozen);
        let block = layout.block() as usize;
        let ordinary = crate::store::io_max(&opts).max(pool.min_len());
        let chunk = (ordinary / block * block).max(block);
        Self {
            inner: Arc::new(Inner {
                domain: Domain::with_slots(1 + flush_files.len()),
                flush_files: flush_files.into_boxed_slice(),
                platform,
                _dir: dir,
                file,
                _lock: lock,
                volume,
                layout,
                policy,
                pool,
                queues,
                meta: Mutex::new(meta),
                regions: Mutex::new(regions),
                report,
                frozen,
                read_only,
                owner_generation,
                opts,
                ordinary,
                chunk,
                acct: Mutex::new(acct),
                epoch: EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                observer: Observer::new(),
                seen: Mutex::new(crate::stats::Seen::default()),
                submitters: Mutex::new(crate::stats::Submitters::default()),
                flusher: std::sync::OnceLock::new(),
            }),
        }
    }
}

/// Handles for flush slots 1.. (D-124): the container itself reopened
/// (`Platform::reopen`), once per slot past the first, each its own open
/// file description (so each flush reports its own failures; the ownership
/// lock stays on slot 0's, and closing these never releases it). Best
/// effort: a handle that cannot be made leaves fewer slots, which the report
/// states.
fn flush_handles<P: Platform>(platform: &P, file: &P::File, lanes: usize) -> Vec<P::File> {
    (1..lanes).map_while(|_| platform.reopen(file)).collect()
}

/// The volume record found, by format.
enum Found {
    V1(VolumeMeta),
    V2(store_io_format::meta2::VolumeMeta2),
}

/// Finds the volume record by trying every legal block size; the slot's
/// `format_major` says which format to read it as.
fn find_volume<Q: Queue>(
    q: &mut Lane<Q>,
    file: &Q::File,
    pool: &BufPool,
) -> Result<(VolumeId, Layout, Found, PairRead), Error> {
    // Read the first slot header at the smallest size to learn the volume id;
    // either copy may be damaged, so try both locations per size.
    let mut media_error = false;
    for log2 in crate::layout::MIN_LOG2_BLOCK..=crate::layout::MAX_LOG2_BLOCK {
        let size = 1u64 << log2;
        for probe_at in [0, size] {
            // The boot pool, which has no large buffers.
            let buf = pool.take(4096).map_err(|_| Error::NotWritten {
                cause: NotWrittenCause::PoolExhausted,
                ctx: ErrorContext::default(),
            })?;
            // Only an unreadable location (a media error, a short read, the
            // end of the file) moves on to the next candidate. Any other
            // failure is returned: treating it as "no volume here" would
            // report a healthy store as corrupt (found by review).
            let io = |raw| Error::Io {
                op: Op::Read,
                raw,
                ctx: ErrorContext::default(),
            };
            let (got, result) = match run(
                q,
                IoOp::Read {
                    file,
                    offset: probe_at,
                    buf,
                },
            ) {
                Ok(Ran::Done { buf, result }) => (buf, result),
                // A failed wait says nothing about this candidate: stop.
                Ok(Ran::Abandoned(raw)) | Err((_, Some(raw))) => return Err(io(raw)),
                Err((_, None)) => {
                    return Err(Error::NotWritten {
                        cause: NotWrittenCause::QueueFull,
                        ctx: ErrorContext::default(),
                    });
                }
            };
            match result {
                Err(raw) if !crate::slots::unreadable(raw) => return Err(io(raw)),
                Ok(4096) => {}
                Err(raw) => {
                    media_error |= store_io_core::errno::is_media_error(raw);
                    continue;
                }
                Ok(_) => continue,
            }
            let Some(b) = got else { continue };
            if b.as_slice()[..8] != store_io_format::slot::MAGIC {
                continue;
            }
            let mut uuid = [0u8; 16];
            uuid.copy_from_slice(&b.as_slice()[32..48]);
            if b.as_slice()[78] != log2 {
                continue;
            }
            // The format comes only from a header whose CRC verifies (spec
            // §2): a bit flip there must not decide how the pair is read.
            if !header_verifies(b.as_slice()) {
                continue;
            }
            let format = u16::from_le_bytes([b.as_slice()[14], b.as_slice()[15]]);
            let v2 = format == store_io_format::slot::FORMAT_2;
            let layout0 = Layout {
                format: if v2 {
                    format
                } else {
                    store_io_format::slot::FORMAT_1
                },
                ..Layout::v1(log2, log2)
            };
            let at = pair_volume(VolumeId::from_bytes(uuid), &layout0);
            let pair = read_pair(q, file, pool, &at)?;
            media_error |= pair.media_error;
            let bytes = match winner_payload(&pair) {
                Ok(b) => b,
                Err(Error::Unsupported { what }) => return Err(Error::Unsupported { what }),
                Err(_) => continue,
            };
            // A longer record, or a feature this version does not know, is a
            // newer format: refused as unsupported (and never rewritten),
            // not reported as corruption.
            let newer = Error::Unsupported {
                what: store_io_core::error::Capability::FormatVersion,
            };
            if v2 {
                let vm = crate::v2::decode_volume(&bytes)?;
                if vm.base.log2_block_size != log2 {
                    continue;
                }
                let layout = Layout {
                    log2_block: log2,
                    log2_table: vm.base.log2_table_slot_size,
                    format,
                    pages: vm.table_pages,
                };
                return Ok((VolumeId::from_bytes(uuid), layout, Found::V2(vm), pair));
            }
            if bytes.len() > store_io_format::meta::VOLUME_META_LEN {
                return Err(newer);
            }
            let vm = VolumeMeta::decode(&bytes).map_err(|e| match e {
                store_io_format::meta::MetaError::Field("features") => newer,
                _ => corrupt(),
            })?;
            if vm.log2_block_size != log2 {
                continue;
            }
            let layout = Layout::v1(log2, vm.log2_table_slot_size);
            if vm.table_offset != layout.table_slots().0 {
                return Err(corrupt());
            }
            return Ok((VolumeId::from_bytes(uuid), layout, Found::V1(vm), pair));
        }
    }
    // No volume record found. If a read failed on the media on the way (a
    // probe or a slot of a candidate pair), that is the answer: it may be
    // transient, and "corrupt" would invite a caller to recreate the store
    // over its data. A media error at a candidate that was never the record
    // also ends here; erring toward "do not recreate" is deliberate.
    Err(Error::Corruption {
        kind: if media_error {
            store_io_core::error::CorruptionKind::MediaError
        } else {
            store_io_core::error::CorruptionKind::HeaderCrc
        },
        ctx: ErrorContext::default(),
    })
}

/// Source of [`Inner::epoch`].
static EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn not_found_or(op: Op, raw: OsError, what: Named) -> Error {
    if store_io_core::errno::is_not_found(raw) {
        Error::NotFound { what }
    } else {
        io(op, raw)
    }
}

pub(crate) fn winner_payload(p: &PairRead) -> Result<Vec<u8>, Error> {
    match p.report.winner {
        Winner::Confirmed(_) | Winner::Unconfirmed(_) => p.payload.clone().ok_or_else(corrupt),
        Winner::Refused => Err(Error::Unsupported {
            what: store_io_core::error::Capability::FormatVersion,
        }),
        Winner::Fork => Err(Error::Corruption {
            kind: store_io_core::error::CorruptionKind::Fork,
            ctx: ErrorContext::default(),
        }),
        // No version left, and a slot was lost to a media error: report that
        // (it may be transient) rather than corruption, which would invite a
        // caller to recreate the store over its data.
        Winner::Lost if p.media_error => Err(Error::Corruption {
            kind: store_io_core::error::CorruptionKind::MediaError,
            ctx: ErrorContext::default(),
        }),
        Winner::Empty | Winner::Lost => Err(Error::Corruption {
            kind: store_io_core::error::CorruptionKind::HeaderCrc,
            ctx: ErrorContext::default(),
        }),
    }
}

/// Whether the slot header at the start of `b` carries a header CRC that
/// verifies. A header longer than `b` (format 1 allows one; format 2's is
/// exactly 128 bytes) is not judged here: true, unless it claims format 2.
fn header_verifies(b: &[u8]) -> bool {
    let len = usize::from(u16::from_le_bytes([b[12], b[13]]));
    if len > b.len() {
        return u16::from_le_bytes([b[14], b[15]]) != store_io_format::slot::FORMAT_2;
    }
    let Some(h) = b.get(..len.max(store_io_format::slot::HEADER_LEN)) else {
        return false;
    };
    let stored = u32::from_le_bytes([h[8], h[9], h[10], h[11]]);
    let mut h = h.to_vec();
    h[8..12].fill(0);
    store_io_format::crc32c::crc32c(&h) == stored
}
