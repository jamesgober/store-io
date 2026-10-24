//! Helpers shared by the format-2 engine tests: a store on the simulator,
//! and raw access to its metadata slots for crafting crashes and damage.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use store_io_buf::{BufPool, PoolConfig};
use store_io_engine::{CONTAINER, Store, StoreOptions};
use store_io_format::crc32c::crc32c;
use store_io_format::meta2::{PageHeader, encode_page};
use store_io_format::slot::{FORMAT_2, SlotFields, SlotIndex, encode};
use store_io_platform::{CompletionBuf, FileMode, FileName, IoOp, Platform, Queue, QueueConfig};
use store_io_sim::{CrashMode, SimPlatform};

pub(crate) mod audit;
pub(crate) mod craft;

pub(crate) const DIR: &str = "/db";
pub(crate) const CHUNK: usize = 4096;
/// Table pages are 16 KiB (the default `log2_table_slot`).
pub(crate) const LOG2_TABLE: u8 = 14;
pub(crate) const TABLE: u64 = 1 << LOG2_TABLE;
/// 400 regions at 169 entries a page: three pages.
pub(crate) const PAGES: u32 = 3;

pub(crate) fn opts() -> StoreOptions {
    StoreOptions {
        queues: 2,
        buffers_per_class: 8,
        append_window: 256 << 12,
        region_capacity: 400,
        container_format: FORMAT_2,
        ..StoreOptions::default()
    }
}

/// A store sized for the default 65,536 regions (a 12 MiB table, past
/// format 1's size bound).
pub(crate) fn opts_large() -> StoreOptions {
    StoreOptions {
        region_capacity: 65_536,
        ..opts()
    }
}

pub(crate) fn create(p: &SimPlatform) -> Store<SimPlatform> {
    Store::create(p.clone(), Path::new(DIR), opts()).unwrap()
}

pub(crate) fn open(p: &SimPlatform) -> Store<SimPlatform> {
    Store::open(p.clone(), Path::new(DIR), opts()).unwrap()
}

/// Power fails with the store still open: no clean close.
pub(crate) fn crash_open(p: &SimPlatform, s: Store<SimPlatform>) {
    std::mem::forget(s);
    p.with_world(|w| w.crash(&CrashMode::LoseAll));
}

pub(crate) fn media(p: &SimPlatform) -> Vec<u8> {
    media_in(p, DIR)
}

/// The durable bytes of the store in `dir`.
pub(crate) fn media_in(p: &SimPlatform, dir: &str) -> Vec<u8> {
    p.with_world(|w| w.media_of(&PathBuf::from(dir), CONTAINER).unwrap().to_vec())
}

/// Writes `data` (whole 4 KiB blocks) at `at`, durably.
pub(crate) fn put(p: &SimPlatform, at: u64, data: &[u8]) {
    let dir = p.open_dir(Path::new(DIR), false).unwrap();
    let file = p
        .open_file(
            &dir,
            &FileName::new(CONTAINER).unwrap(),
            FileMode::ReadWrite,
        )
        .unwrap();
    let pool = BufPool::new(&PoolConfig::uniform(CHUNK, CHUNK, CHUNK, 2)).unwrap();
    let mut q = p.queue(QueueConfig::default()).unwrap();
    for (i, chunk) in data.chunks(CHUNK).enumerate() {
        let mut buf = pool.take(CHUNK).unwrap();
        buf.as_mut_slice().copy_from_slice(chunk);
        q.submit(
            IoOp::Write {
                file: &file,
                offset: at + (i * CHUNK) as u64,
                buf,
                dsync: false,
            },
            1,
        )
        .unwrap();
        let mut out = CompletionBuf::with_capacity(2);
        q.wait(1, &mut out).unwrap();
        assert_eq!(out.drain().next().unwrap().result, Ok(CHUNK));
    }
    p.flush_all(&file).unwrap();
}

pub(crate) fn block(_: &SimPlatform) -> u64 {
    // The simulator's blocks are 4 KiB (checked by the first test).
    4096
}

/// Page `p`'s slot (`0` A, `1` B).
pub(crate) fn page_at(pf: &SimPlatform, p: u32, slot: u64) -> u64 {
    2 * block(pf) + 2 * u64::from(p) * TABLE + slot * TABLE
}

pub(crate) fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
}

pub(crate) fn u64_at(b: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(b[i..i + 8].try_into().unwrap())
}

/// The highest `meta_seq` of any written metadata slot.
pub(crate) fn max_meta_seq(pf: &SimPlatform) -> u64 {
    let m = media(pf);
    let b = block(pf) as usize;
    let mut at = vec![0, b];
    for p in 0..PAGES {
        at.push(page_at(pf, p, 0) as usize);
        at.push(page_at(pf, p, 1) as usize);
    }
    at.iter()
        .filter(|&&i| m[i..i + 8] == *b"SIOSLOT\0")
        .map(|&i| u64_at(&m, i + 96))
        .max()
        .unwrap()
}

/// An empty page payload naming page `p`.
pub(crate) fn empty_page(p: u32) -> Vec<u8> {
    let mut payload = vec![0u8; store_io_format::meta2::PAGE_HEADER_LEN];
    let h = PageHeader {
        page_index: p,
        next_seq: 0,
        count: 0,
    };
    let n = encode_page(&mut payload, &h, &[]).unwrap();
    payload.truncate(n);
    payload
}

/// Page `p`'s next copy in slot B, chained to slot A: generation 2, the next
/// `meta_seq`, an empty page. `torn` corrupts its payload after encoding.
pub(crate) fn page_copy_b(pf: &SimPlatform, p: u32, torn: bool) -> Vec<u8> {
    page_copy_b_of(pf, p, &empty_page(p), torn)
}

/// As [`page_copy_b`], holding `payload`.
pub(crate) fn page_copy_b_of(pf: &SimPlatform, p: u32, payload: &[u8], torn: bool) -> Vec<u8> {
    let m = media(pf);
    let a = page_at(pf, p, 0) as usize;
    let mut slot = vec![0u8; TABLE as usize];
    encode(
        &mut slot,
        &SlotFields {
            volume_uuid: m[32..48].try_into().unwrap(),
            object_id: (1 << 61) | u64::from(p),
            generation: u64_at(&m, a + 56) + 1,
            owner_generation: 1,
            slot_offset: page_at(pf, p, 1),
            slot_index: SlotIndex::B,
            log2_slot_size: LOG2_TABLE,
            prev_header_crc32c: u32_at(&m, a + 8),
            format_major: FORMAT_2,
            meta_seq: max_meta_seq(pf) + 1,
        },
        payload,
    )
    .unwrap();
    if torn {
        slot[128 + 4] ^= 0xff;
    }
    slot
}

/// Flips the first payload byte of the slot at `at`: a torn write (the
/// header, and with it the payload CRC, intact). In a page that byte is the
/// page index, so a torn page no longer decodes either.
pub(crate) fn tear(pf: &SimPlatform, at: u64) {
    let m = media(pf);
    let mut b = m[at as usize..at as usize + CHUNK].to_vec();
    b[128] ^= 0xff;
    put(pf, at, &b);
}

/// The volume record's slot holding the newest generation.
pub(crate) fn newest_volume_slot(pf: &SimPlatform) -> u64 {
    let m = media(pf);
    let b = block(pf);
    if m[b as usize..b as usize + 8] == *b"SIOSLOT\0"
        && u64_at(&m, b as usize + 56) > u64_at(&m, 56)
    {
        b
    } else {
        0
    }
}

/// Redoes the header CRC of an edited slot.
pub(crate) fn rehash(slot: &mut [u8]) {
    slot[8..12].fill(0);
    let crc = crc32c(&slot[..128]);
    slot[8..12].copy_from_slice(&crc.to_le_bytes());
}

/// A volume-record copy for slot B: `generation`, chained to slot A only
/// when it is generation 2, holding slot A's payload; `torn` corrupts it.
pub(crate) fn volume_copy_b(pf: &SimPlatform, generation: u64, torn: bool) -> Vec<u8> {
    volume_copy_b_longer(pf, generation, 0, torn)
}

/// As [`volume_copy_b`], its payload `extra` zero bytes longer.
pub(crate) fn volume_copy_b_longer(
    pf: &SimPlatform,
    generation: u64,
    extra: usize,
    torn: bool,
) -> Vec<u8> {
    let m = media(pf);
    let b = block(pf);
    let mut payload = m[128..128 + store_io_format::meta2::VOLUME_META2_LEN].to_vec();
    payload.resize(payload.len() + extra, 0);
    let mut slot = vec![0u8; b as usize];
    encode(
        &mut slot,
        &SlotFields {
            volume_uuid: m[32..48].try_into().unwrap(),
            object_id: 0,
            generation,
            owner_generation: 1,
            slot_offset: b,
            slot_index: SlotIndex::B,
            log2_slot_size: m[78],
            prev_header_crc32c: u32_at(&m, 8),
            format_major: FORMAT_2,
            meta_seq: max_meta_seq(pf) + 1,
        },
        &payload,
    )
    .unwrap();
    if torn {
        slot[128] ^= 0xff;
    }
    slot
}
