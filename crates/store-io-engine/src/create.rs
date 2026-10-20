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
    /// [`Error::UnsafeDevice`] / [`Error::Unverified`] on refusal;
    /// [`Error::Io`] / [`Error::DurabilityUnknown`] on platform failures.
    pub fn create(platform: P, path: &Path, opts: StoreOptions) -> Result<Self, Error> {
        let dir = platform
            .open_dir(path, true)
            .map_err(|e| io(Op::Create, e))?;
        let name = container_name()?;
        let (file, lock) = match platform.create_file(&dir, &name) {
            Ok(file) => {
                let lock = platform.lock_exclusive(&file).map_err(|_| Error::Locked)?;
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
    /// header whose header CRC verifies (D-068). A read error, an empty pool
    /// or any verified header (a damaged store, or one of a newer format)
    /// proves nothing, so the container is left untouched.
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
        let lock = platform.lock_exclusive(&file).map_err(|_| exists())?;
        let largest_metadata = 4u64 << crate::layout::MAX_LOG2_BLOCK;
        if platform.size(&file).map_err(|e| io(Op::Open, e))? > largest_metadata {
            return Err(exists());
        }
        let evidence = platform.probe(dir, &file).map_err(|e| io(Op::Probe, e))?;
        let boot_pool = make_pool(
            &evidence,
            &Layout {
                log2_block: crate::layout::MAX_LOG2_BLOCK,
                log2_table: crate::layout::MAX_LOG2_BLOCK,
            },
            opts,
            false,
        )?;
        let queues = make_queues(platform, opts)?;
        let len = platform.size(&file).map_err(|e| io(Op::Open, e))?;
        let holds = {
            let mut q = queues.get(0);
            holds_slot_header(&mut q, &file, &boot_pool, len)
        };
        if holds != Some(false) {
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
        let layout = Layout {
            log2_block: choose_log2_block(&evidence, opts.block_size),
            log2_table: opts
                .log2_table_slot
                .clamp(choose_log2_block(&evidence, opts.block_size), 16),
        };
        let policy = policy_of(&decision);
        let pool = make_pool(&evidence, &layout, opts, true)?;
        let queues = make_queues(platform, opts)?;
        let volume = VolumeId::from_bytes(random_uuid());
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
}

/// Whether some 4 KiB block of the first `len` bytes starts with a slot
/// header whose header CRC verifies (any status past the header-CRC check:
/// valid, foreign, misplaced, unsupported version, and so on). `None` when a
/// block could not be read in full: nothing is proven then (D-068). Slot
/// headers sit at block starts and blocks are at least 4 KiB, so every header
/// starts on a 4 KiB boundary.
fn holds_slot_header<Q: Queue>(
    q: &mut Lane<Q>,
    file: &Q::File,
    pool: &BufPool,
    len: u64,
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
            _ => return Some(true),
        }
        at += SCAN;
    }
    Some(false)
}
