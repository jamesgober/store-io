//! How long a format-2 open takes with N regions (v0.5 E2b/E3 C8, the
//! measure for pipelining the header reads). Creates a format-2 store on
//! this OS's backend, writes N page regions straight into its file with the
//! format crate's encoders (one table entry and one header each:
//! provisioning N regions would take minutes of flushes), then times the
//! open.
//!
//! Usage: `open_scale <dir> <regions> [reps]` (the directory is emptied, if
//! it is missing, empty or holds only a store's container).
//! `DENSE` set writes every region's blocks (no holes); `RUNS=k` gives each
//! region k one-block extents apart from each other (k runs, v0.5 E4 C12:
//! the cost of multi-run maps); on Linux, `SYNC` set uses the synchronous
//! tier. Each open prints the heap the open store holds (a counting
//! allocator).

// `clone_on_copy`: the Windows platform is `Copy`, the Linux one is not.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::clone_on_copy
)]

/// Counts the heap in use, so an open store's share can be printed.
struct Counting;

static HEAP: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

// SAFETY: every call forwards to the system allocator unchanged; the
// counter only records sizes.
unsafe impl std::alloc::GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: std::alloc::Layout) -> *mut u8 {
        HEAP.fetch_add(l.size(), std::sync::atomic::Ordering::Relaxed);
        // SAFETY: as the caller's contract.
        unsafe { std::alloc::System.alloc(l) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, l: std::alloc::Layout) {
        HEAP.fetch_sub(l.size(), std::sync::atomic::Ordering::Relaxed);
        // SAFETY: as the caller's contract.
        unsafe { std::alloc::System.dealloc(ptr, l) }
    }
    unsafe fn alloc_zeroed(&self, l: std::alloc::Layout) -> *mut u8 {
        HEAP.fetch_add(l.size(), std::sync::atomic::Ordering::Relaxed);
        // SAFETY: as the caller's contract.
        unsafe { std::alloc::System.alloc_zeroed(l) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, l: std::alloc::Layout, new: usize) -> *mut u8 {
        // SAFETY: as the caller's contract.
        let out = unsafe { std::alloc::System.realloc(ptr, l, new) };
        if !out.is_null() {
            if new >= l.size() {
                HEAP.fetch_add(new - l.size(), std::sync::atomic::Ordering::Relaxed);
            } else {
                HEAP.fetch_sub(l.size() - new, std::sync::atomic::Ordering::Relaxed);
            }
        }
        out
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
mod scale {
    use std::fs::OpenOptions;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::path::Path;
    use std::time::Instant;

    use store_io_engine::{CONTAINER, Store, StoreOptions};
    use store_io_format::meta::{FillPattern, RegionKind, RegionName, RegionState};
    use store_io_format::meta2::{
        Entry2, PAGE_HEADER_LEN, PageHeader, RegionHeader2, encode_page, encode_region_header,
        entries_per_page,
    };
    use store_io_format::slot::{FORMAT_2, SlotFields, SlotIndex, encode};

    #[cfg(windows)]
    fn platform() -> store_io_win::WinPlatform {
        store_io_win::WinPlatform::new()
    }

    /// As the native API makes it: io_uring where the kernel accepts it (the
    /// synchronous tier with `SYNC` set).
    #[cfg(target_os = "linux")]
    fn platform() -> store_io_posix::PosixPlatform {
        let tier = if std::env::var_os("SYNC").is_some() {
            store_io_platform::LinuxTier::Sync
        } else {
            store_io_platform::LinuxTier::Auto
        };
        store_io_posix::PosixPlatform::with_tier(tier).unwrap()
    }

    const LOG2_TABLE: u8 = 14;

    fn u32_at(b: &[u8], i: usize) -> u32 {
        u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
    }

    fn u64_at(b: &[u8], i: usize) -> u64 {
        u64::from_le_bytes(b[i..i + 8].try_into().unwrap())
    }

    /// Writes `n` page regions of one data block each into the closed store's
    /// container: every page's next copy in slot B, every header in its slot A.
    fn craft(path: &Path, n: u32, block: u64, dense: bool, runs: u64) {
        let table = 1u64 << LOG2_TABLE;
        let per_page = u32::from(entries_per_page(LOG2_TABLE));
        let pages = n.div_ceil(per_page);
        let mut f = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        let mut meta = vec![0u8; (2 * block + 2 * u64::from(pages) * table) as usize];
        f.read_exact(&mut meta).unwrap();
        let volume: [u8; 16] = meta[32..48].try_into().unwrap();
        // The table's end: create leaves the file exactly that long.
        let data_start = f.metadata().unwrap().len();
        let last_seq = (0..2 + 2 * u64::from(pages))
            .map(|i| {
                if i < 2 {
                    i * block
                } else {
                    2 * block + (i - 2) * table
                }
            })
            .filter(|&at| meta[at as usize..at as usize + 8] == *b"SIOSLOT\0")
            .map(|at| u64_at(&meta, at as usize + 96))
            .max()
            .unwrap();
        // k data blocks, apart from each other (a gap after each).
        let region = if runs == 1 {
            3 * block
        } else {
            2 * block + 2 * runs * block
        };
        let mut buf = vec![0u8; block as usize];
        for p in 0..pages {
            // Each page write takes the next sequence.
            let meta_seq = last_seq + 1 + u64::from(p);
            let count = (n - p * per_page).min(per_page);
            let mut raws = Vec::with_capacity(count as usize);
            for seq in 0..count {
                let i = u64::from(p * per_page + seq);
                let off = data_start + i * region;
                let id = (u64::from(p) << 32) | u64::from(seq);
                raws.push(
                    Entry2 {
                        seq,
                        kind: RegionKind::Page,
                        flags: 0,
                        compat_flags: 0,
                        name: RegionName::new(&format!("r{i}")).unwrap(),
                        tag: 0,
                        extent_count: runs as u16,
                        header_a: off,
                        header_b: off + block,
                        data_size: runs * block,
                        capacity: runs * block,
                        span_start: off,
                        span_end: off + 2 * block + (2 * runs - 1) * block,
                        footprint_seq: meta_seq,
                    }
                    .to_raw(),
                );
                let h = RegionHeader2 {
                    region_id: id,
                    kind: RegionKind::Page,
                    state: RegionState::Ready,
                    fill: FillPattern::Zeros,
                    flags: 0,
                    compat_flags: 0,
                    log2_block: block.trailing_zeros() as u8,
                    extent_count: runs as u16,
                    region_generation: 1,
                    fill_key: [0; 16],
                    object_len: 0,
                };
                let mut payload = vec![0u8; block as usize];
                let extents: Vec<(u64, u64)> = (0..runs)
                    .map(|k| (off + 2 * block + 2 * k * block, block))
                    .collect();
                let len = encode_region_header(&mut payload, &h, &extents, &[]).unwrap();
                buf.fill(0);
                let _crc = encode(
                    &mut buf,
                    &SlotFields {
                        volume_uuid: volume,
                        object_id: (1 << 62) | id,
                        generation: 1,
                        owner_generation: 1,
                        slot_offset: off,
                        slot_index: SlotIndex::A,
                        log2_slot_size: block.trailing_zeros() as u8,
                        prev_header_crc32c: 0,
                        format_major: FORMAT_2,
                        meta_seq: 0,
                        compat_flags: 0,
                    },
                    &payload[..len],
                )
                .unwrap();
                f.seek(SeekFrom::Start(off)).unwrap();
                f.write_all(&buf).unwrap();
                if dense {
                    // The rest of the region written too: no holes.
                    f.write_all(&vec![0u8; (region - block) as usize]).unwrap();
                }
            }
            let mut payload = vec![0u8; PAGE_HEADER_LEN + 96 * raws.len()];
            let len = encode_page(
                &mut payload,
                &PageHeader {
                    page_index: p,
                    next_seq: count,
                    count: count as u16,
                },
                &raws,
            )
            .unwrap();
            let a = (2 * block + 2 * u64::from(p) * table) as usize;
            let mut slot = vec![0u8; table as usize];
            let _crc = encode(
                &mut slot,
                &SlotFields {
                    volume_uuid: volume,
                    object_id: (1 << 61) | u64::from(p),
                    generation: u64_at(&meta, a + 56) + 1,
                    owner_generation: 1,
                    slot_offset: a as u64 + table,
                    slot_index: SlotIndex::B,
                    log2_slot_size: LOG2_TABLE,
                    prev_header_crc32c: u32_at(&meta, a + 8),
                    format_major: FORMAT_2,
                    meta_seq,
                    compat_flags: 0,
                },
                &payload[..len],
            )
            .unwrap();
            f.seek(SeekFrom::Start(a as u64 + table)).unwrap();
            f.write_all(&slot).unwrap();
        }
        f.set_len(data_start + u64::from(n) * region).unwrap();
        f.sync_all().unwrap();
    }

    pub(crate) fn main() {
        let args: Vec<String> = std::env::args().collect();
        let dir = Path::new(&args[1]);
        let n: u32 = args[2].parse().unwrap();
        let reps: u32 = args.get(3).map_or(3, |r| r.parse().unwrap());
        // Emptied only if it is missing, empty, or a store's directory.
        if let Ok(mut entries) = std::fs::read_dir(dir) {
            let ours = entries.all(|e| e.is_ok_and(|e| e.file_name() == CONTAINER));
            assert!(
                ours,
                "{} holds more than a store: not emptied",
                dir.display()
            );
        }
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).unwrap();
        let mut opts = StoreOptions {
            container_format: FORMAT_2,
            region_capacity: n.max(1),
            log2_table_slot: LOG2_TABLE,
            ..StoreOptions::default()
        };
        let p = platform();
        let s = match Store::create(p.clone(), dir, opts.clone()) {
            Ok(s) => s,
            Err(_) => {
                // A device without the evidence for default trust (a VM disk).
                opts.trust.override_refusal = true;
                Store::create(p.clone(), dir, opts.clone()).unwrap()
            }
        };
        let block = s.report().block_size;
        drop(s);
        let t = Instant::now();
        let runs: u64 = std::env::var("RUNS").map_or(1, |r| r.parse().unwrap());
        assert!(
            (1..=store_io_format::meta2::max_extents(12) as u64).contains(&runs),
            "RUNS from 1 to a header's extents"
        );
        craft(
            &dir.join(CONTAINER),
            n,
            block,
            std::env::var_os("DENSE").is_some(),
            runs,
        );
        println!("crafted {n} regions in {:.2} s", t.elapsed().as_secs_f64());
        {
            let s = Store::open(p.clone(), dir, opts.clone()).unwrap();
            println!(
                "io: {:?}
    decision: {:?}",
                s.report().io,
                s.report().decision
            );
        }
        for r in 0..reps {
            let heap = super::HEAP.load(std::sync::atomic::Ordering::Relaxed);
            let t = Instant::now();
            let s = Store::open(p.clone(), dir, opts.clone()).unwrap();
            let el = t.elapsed().as_secs_f64();
            // Another thread may free during the open: never below zero.
            let held = super::HEAP
                .load(std::sync::atomic::Ordering::Relaxed)
                .saturating_sub(heap);
            let rep = s.report();
            println!(
                "open {r}: {el:.3} s  ({:.1} us a region)  heap {:.1} MiB ({} B a region)  unavailable {}  retried reads {}",
                el * 1e6 / f64::from(n.max(1)),
                held as f64 / f64::from(1 << 20),
                held / n.max(1) as usize,
                rep.unavailable.len(),
                rep.metadata.retried_reads
            );
            #[cfg(target_os = "linux")]
            println!(
                "  io_uring fallbacks so far: {}; path now: {:?}",
                store_io_posix::io_uring_fallbacks(),
                store_io_platform::Platform::io_path(&p)
            );
            assert!(rep.unavailable.is_empty(), "{:?}", rep.unavailable.first());
            drop(s);
        }
    }
}

#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
fn main() {
    scale::main();
}

/// No backend here.
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn main() {}
