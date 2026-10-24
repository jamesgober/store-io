//! Creating a store: probing the device, reclaiming an abandoned create,
//! and writing the first volume record and region table.

use std::path::Path;

use store_io_buf::BufPool;
use store_io_core::decide::decide;
use store_io_core::error::{Error, ErrorContext, Named, Op};
use store_io_core::id::VolumeId;
use store_io_format::meta::{VOLUME_META_LEN, VolumeMeta, encode_table};
use store_io_platform::{FileMode, FileName, IoOp, Platform, Queue};

use crate::exec::{Lane, QueueSet, Ran, run};
use crate::layout::{Layout, choose_log2_block};
use crate::slots::{PairRead, plan_next, write_slot};
use crate::store::*;

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
    /// [`Error::Locked`] if another handle holds the new container's lock;
    /// [`Error::UnsafeDevice`] / [`Error::Unverified`] on refusal;
    /// [`Error::Io`] / [`Error::DurabilityUnknown`] on platform failures
    /// (`Io(Lock)` on a file system that cannot lock).
    pub fn create(platform: P, path: &Path, opts: StoreOptions) -> Result<Self, Error> {
        Self::create_named(platform, path, opts, &container_name()?)
    }

    /// [`Self::create`] with the container under `name` (`copy_store`
    /// builds its target under a temporary one).
    pub(crate) fn create_named(
        platform: P,
        path: &Path,
        opts: StoreOptions,
        name: &store_io_platform::FileName,
    ) -> Result<Self, Error> {
        let dir = platform
            .open_dir(path, true)
            .map_err(|e| io(Op::Create, e))?;
        let name = name.clone();
        let (file, lock) = match platform.create_file(&dir, &name) {
            Ok(file) => {
                let lock = platform.lock_exclusive(&file).map_err(|raw| {
                    if store_io_core::errno::is_lock_held(raw) {
                        Error::Locked
                    } else {
                        io(Op::Lock, raw)
                    }
                })?;
                (file, lock)
            }
            Err(e) if store_io_core::errno::is_already_exists(e) => {
                Self::reclaim_abandoned(&platform, &dir, &name, &opts)?
            }
            Err(e) => return Err(io(Op::Create, e)),
        };
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

    /// A container left by a create that never finished (a crash before its
    /// metadata was durable) is taken over by the next create; anything else
    /// is a store and stays untouched (`AlreadyExists`). Abandoned means: no
    /// one holds its lock, it is no longer than the largest metadata area,
    /// and every block of it was read in full without finding a single slot
    /// header whose header CRC verifies (D-068), apart from, for a format-2
    /// create, what a crashed format-2 create leaves (see
    /// [`holds_slot_header`]; a file longer than format 1's bound must show
    /// at least one such leftover). A read error, an empty pool or any
    /// other verified header (a damaged store, or one of a newer format)
    /// proves nothing, so the container is left untouched. The bound is
    /// this create's geometry: a crashed create with a larger table is
    /// taken over only by a create asking for at least as much.
    fn reclaim_abandoned(
        platform: &P,
        dir: &P::Dir,
        name: &FileName,
        opts: &StoreOptions,
    ) -> Result<(P::File, P::Lock), Error> {
        let exists = || Error::AlreadyExists { what: Named::Store };
        let file = platform
            .open_file(dir, name, FileMode::ReadWrite)
            .map_err(|_| exists())?;
        // Another holder: a live store, left alone. A file system that
        // cannot lock is a plain failure, as at the first create.
        let lock = platform.lock_exclusive(&file).map_err(|raw| {
            if store_io_core::errno::is_lock_held(raw) {
                exists()
            } else {
                io(Op::Lock, raw)
            }
        })?;
        // A crashed create never wrote past its metadata: format 1's four
        // largest slots, or format 2's table area for this create's geometry
        // (bounded here over every block size, before the probe, so a large
        // store is refused without one).
        let v2 = opts.container_format == store_io_format::slot::FORMAT_2;
        let v1_metadata = 4u64 << crate::layout::MAX_LOG2_BLOCK;
        let bound = if v2 {
            (crate::layout::MIN_LOG2_BLOCK..=crate::layout::MAX_LOG2_BLOCK)
                .map(|b| {
                    let t = opts.log2_table_slot.clamp(b, 16);
                    crate::v2::layout_for(b, t, opts.region_capacity).data_start()
                })
                .max()
                .unwrap_or(v1_metadata)
        } else {
            v1_metadata
        };
        if platform.size(&file).map_err(|e| io(Op::Open, e))? > bound {
            return Err(exists());
        }
        let evidence = platform.probe(dir, &file).map_err(|e| io(Op::Probe, e))?;
        let largest_metadata = if v2 {
            let log2_block = choose_log2_block(&evidence, opts.block_size);
            let log2_table = opts.log2_table_slot.clamp(log2_block, 16);
            crate::v2::layout_for(log2_block, log2_table, opts.region_capacity).data_start()
        } else {
            v1_metadata
        };
        if platform.size(&file).map_err(|e| io(Op::Open, e))? > largest_metadata {
            return Err(exists());
        }
        let boot_pool = make_pool(
            &evidence,
            &Layout::v1(crate::layout::MAX_LOG2_BLOCK, crate::layout::MAX_LOG2_BLOCK),
            opts,
            false,
        )?;
        let queues = make_queues(platform, opts, 1)?;
        let len = platform.size(&file).map_err(|e| io(Op::Open, e))?;
        let mut leftovers = false;
        let holds = {
            let mut q = queues.get(0);
            holds_slot_header(
                &mut q,
                &file,
                &boot_pool,
                len,
                v2.then_some(opts.log2_table_slot),
                &mut leftovers,
            )
        };
        // Past format 1's bound, only a format-2 create's own remains
        // justify a takeover: a larger file with nothing that verifies may
        // be a damaged store of either format.
        if holds != Some(false) || (len > v1_metadata && !leftovers) {
            return Err(exists());
        }
        platform.set_len(&file, 0).map_err(|e| io(Op::Create, e))?;
        Ok((file, lock))
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
        let log2_block = choose_log2_block(&evidence, opts.block_size);
        let log2_table = opts.log2_table_slot.clamp(log2_block, 16);
        let v2 = opts.container_format == store_io_format::slot::FORMAT_2;
        let layout = if v2 {
            crate::v2::layout_for(log2_block, log2_table, opts.region_capacity)
        } else {
            Layout::v1(log2_block, log2_table)
        };
        let policy = policy_of(&decision);
        let pool = make_pool(&evidence, &layout, opts, true)?;
        let lanes = if policy.flush {
            flush_slots(platform, opts)
        } else {
            1
        };
        let queues = make_queues(platform, opts, lanes)?;
        let volume = VolumeId::from_bytes(random_uuid());
        let created = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        if v2 {
            let meta = {
                let mut q = queues.get(0);
                crate::v2::create_metadata(platform, file, &mut q, &pool, &layout, volume, created)?
            };
            let report = StoreReport {
                evidence,
                decision,
                block_size: layout.block(),
                table_capacity: layout.pages as usize
                    * usize::from(store_io_format::meta2::entries_per_page(layout.log2_table)),
                volume,
                owner_generation: 1,
                read_only: false,
                io: platform.io_path(),
                unavailable: Vec::new(),
                format: store_io_format::slot::FORMAT_2,
                metadata: MetadataReport::default(),
                flushes_in_flight: 1,
            };
            return Ok((meta, report, layout, policy, pool, queues));
        }
        let header_len = layout.data_start();
        platform
            .allocate(file, header_len)
            .map_err(|e| space_err(Op::Allocate, e, 0))?;
        let vmeta = VolumeMeta {
            log2_block_size: layout.log2_block,
            log2_table_slot_size: layout.log2_table,
            device_index: 0,
            owner_generation: 1,
            table_offset: layout.table_slots().0,
            created_unix_s: created,
            features: 0,
        };
        let mut vbuf = [0u8; VOLUME_META_LEN];
        let _len = vmeta.encode(&mut vbuf).map_err(|_| meta_err())?;
        let mut tbuf = vec![0u8; layout.table() as usize];
        let tlen = encode_table(&mut tbuf, 0, &[]).map_err(|_| meta_err())?;
        let vat = pair_volume(volume, &layout);
        let tat = pair_table(volume, &layout);
        let empty = PairRead::blank();
        let mut q = queues.get(0);
        let next_v = plan_next(&empty, &vbuf, 1)?;
        let volume_slot = write_slot(&mut q, file, &pool, &vat, &next_v, false)?;
        let next_t = plan_next(&empty, &tbuf[..tlen], 1)?;
        let table_slot = write_slot(&mut q, file, &pool, &tat, &next_t, false)?;
        drop(q);
        platform
            .flush_all(file)
            .map_err(|e| Error::DurabilityUnknown {
                op: Op::FlushAll,
                raw: Some(e),
                ctx: ErrorContext::default(),
            })?;
        // The in-memory state is what was written, not a read-back (D-069).
        let volume_pair = empty.after(volume_slot);
        let table_pair = empty.after(table_slot);
        let report = StoreReport {
            evidence,
            decision,
            block_size: layout.block(),
            table_capacity: layout.table_capacity(),
            volume,
            owner_generation: 1,
            read_only: false,
            io: platform.io_path(),
            unavailable: Vec::new(),
            format: store_io_format::slot::FORMAT_1,
            metadata: MetadataReport::default(),
            flushes_in_flight: 1,
        };
        let meta = Meta {
            volume: vmeta,
            volume_pair,
            table_pair,
            next_region_id: 0,
            entries: Vec::new(),
            container_len: header_len,
            v2: None,
            unavailable: Vec::new(),
            repairs: Vec::new(),
        };
        Ok((meta, report, layout, policy, pool, queues))
    }
}

/// Whether some 4 KiB block of the first `len` bytes starts with a slot
/// header whose header CRC verifies (any status past the header-CRC check:
/// valid, foreign, misplaced, unsupported version, and so on). `None` when a
/// block could not be read in full: nothing is proven then (D-068). Slot
/// headers sit at block starts and blocks are at least 4 KiB, so every header
/// starts on a 4 KiB boundary.
///
/// With `v2` (a format-2 create, its `log2_table_slot`), a crashed format-2
/// create's own leftovers do not count (spec §12): a generation-1 table
/// page that is empty or torn, and a generation-1 volume record whose
/// payload is torn (a valid one would make the file a store), each only
/// where a create of this table slot size puts it at some block size (an
/// image elsewhere, in a damaged store's data, proves nothing). Of any
/// volume ids: an earlier crashed create may have left another.
/// `leftovers` is set when one was seen.
fn holds_slot_header<Q: Queue>(
    q: &mut Lane<Q>,
    file: &Q::File,
    pool: &BufPool,
    len: u64,
    v2: Option<u8>,
    leftovers: &mut bool,
) -> Option<bool> {
    use store_io_format::slot::{Expect, SlotIndex, SlotStatus, validate};
    const SCAN: u64 = 4096;
    let mut at = 0u64;
    while at < len {
        let want = SCAN.min(len - at) as usize;
        // The boot pool, which has no large buffers.
        let buf = pool.take(SCAN as usize).ok()?;
        let Ran::Done { buf, result } = run(
            q,
            IoOp::Read {
                file,
                offset: at,
                buf,
            },
        )
        .ok()?
        else {
            return None;
        };
        let mut b = buf?;
        if result.ok()? != want {
            return None;
        }
        b.as_mut_slice()[want..].fill(0);
        let expect = Expect {
            volume_uuid: [0; 16],
            object_id: 0,
            slot_offset: at,
            slot_index: SlotIndex::A,
            log2_slot_size: 12,
            format_major: store_io_format::slot::FORMAT_1,
            numbered: false,
        };
        match validate(b.as_slice(), &expect) {
            SlotStatus::Blank
            | SlotStatus::BadMagic
            | SlotStatus::BadHeaderCrc
            | SlotStatus::BadLength
            | SlotStatus::ReadError => {}
            _ if v2.is_some_and(|t| crashed_create_leftover(b.as_slice(), at, t)) => {
                *leftovers = true;
            }
            _ => return Some(true),
        }
        at += SCAN;
    }
    Some(false)
}

/// Whether a 4 KiB block whose slot header verifies, at `at`, is what a
/// crashed format-2 create of table slots of `log2_table_slot` leaves
/// behind (see [`holds_slot_header`]): format 2, generation 1, slot A, and
/// either a table page whose payload is torn or an empty page, or a volume
/// record whose payload is torn; written where a create writes that page's
/// (or the volume record's) first copy at some block size, and saying so
/// (its own slot offset).
fn crashed_create_leftover(b: &[u8], at: u64, log2_table_slot: u8) -> bool {
    let u16_at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
    let u32_at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    let u64_at = |i: usize| {
        let mut x = [0u8; 8];
        x.copy_from_slice(&b[i..i + 8]);
        u64::from_le_bytes(x)
    };
    let header_len = usize::from(u16_at(12));
    // Format 2, generation 1, written at `at` as slot A (a create writes
    // only the first copy of each pair).
    if u16_at(14) != store_io_format::slot::FORMAT_2
        || u64_at(56) != 1
        || header_len != 128
        || u64_at(64) != at
        || b[76] != 0
    {
        return false;
    }
    let object = u64_at(48);
    let payload_len = u32_at(72) as usize;
    let Some(payload) = header_len
        .checked_add(payload_len)
        .and_then(|end| b.get(header_len..end))
    else {
        return false;
    };
    let torn = store_io_format::crc32c::crc32c(payload) != u32_at(28);
    if object == 0 {
        // The volume record's first copy: block 0.
        return torn && at == 0;
    }
    let page = object & !(1u64 << 61);
    if object >> 61 != 1 || page >= u64::from(store_io_format::meta2::MAX_TABLE_PAGES) {
        return false;
    }
    // Page `p`'s first copy: from block 2, two table slots a page.
    let placed = (crate::layout::MIN_LOG2_BLOCK..=crate::layout::MAX_LOG2_BLOCK).any(|lb| {
        let t = 1u64 << log2_table_slot.clamp(lb, 16);
        at == (2u64 << lb) + 2 * page * t
    });
    placed
        && (torn
            || store_io_format::meta2::decode_page(payload, page as u32, u16::MAX)
                .is_ok_and(|p| p.header.count == 0))
}
