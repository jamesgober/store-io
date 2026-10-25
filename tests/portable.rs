//! One portable format (v0.5 exit criterion): the same operations make the
//! same container on Linux, Windows and macOS, field by field and byte for
//! byte, except what differs between any two creates (the volume id, the
//! creation time, a keyed region's fill key and fill, the CRCs that cover
//! them) and the bytes the format leaves undefined (a released region's
//! data: Linux and macOS punch a hole, Windows trims in place), and a
//! container made on one system opens on every other.
//!
//! A fixed scenario (pages, appends, a slot, growth within a cap and beyond
//! capacity, a release and a later region in its space, a writable reopen)
//! is run twice here, with zero fill and with keyed fill. Each container,
//! checked and then masked as above, must be the same both times and hash
//! to [`GOLDEN`] (or [`GOLDEN_KEYED`]) on every system: CI runs this on all
//! three. The container in `tests/data/portable/` was made by the zero-fill
//! scenario on Linux: every system opens it, reads back what the scenario
//! wrote and writes to it, and its masked bytes equal this system's. (That a
//! container made on Windows or macOS opens everywhere follows: masked, it
//! is the same bytes, and what the mask hides is checked consistent first.)

#![cfg(any(windows, target_os = "linux", target_os = "macos"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_results,
    unused_must_use
)]

use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use store_io::{CONTAINER, RegionOptions, ScanItem, Store, StoreOptions, Trust};
use store_io_format::crc32c::crc32c;
use store_io_format::fill::fill_keyed_v1;
use store_io_format::slot::{HEADER_LEN, MAGIC};

/// The masked zero-fill container's FNV-1a 64 hash and length, the same on
/// every system.
const GOLDEN: (u64, usize) = (12_773_067_260_103_021_632, 3_219_456);

/// The same for the keyed-fill scenario.
const GOLDEN_KEYED: (u64, usize) = (3_652_586_157_313_089_982, 3_219_456);

/// The scenario's block and slot size.
const B: usize = 4096;

/// A region header's object id bit (`2^62 | region id`).
const REGION_OBJECT: u64 = 1 << 62;

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let base = std::env::var_os("STORE_IO_TEST_DIR")
            .or_else(|| std::env::var_os("HOME").filter(|_| cfg!(target_os = "linux")))
            .map_or_else(std::env::temp_dir, PathBuf::from);
        let d = base.join(format!(
            ".store-io-portable-{}-{}-{tag}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _gone = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Self(d)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _cleanup = std::fs::remove_dir_all(&self.0);
    }
}

/// The scenario's options: the smallest block (4096: a larger one the
/// device needs is a failure named as such, [`masked`]), a small table, the
/// fill asked for, and the labelled override where this machine's device is
/// refused (it changes no byte).
fn options(dir: &Path, keyed: bool) -> StoreOptions {
    let probe = store_io::probe(dir).unwrap();
    let refused = !matches!(probe.decision.durable_open, store_io::DurableOpen::Allowed);
    StoreOptions {
        trust: Trust {
            override_refusal: refused,
            ..Trust::default()
        },
        block_size: Some(B as u32),
        region_capacity: 256,
        keyed_fill: keyed,
        ..StoreOptions::default()
    }
}

/// Block `i` of the page region: a pattern of its own.
fn block(i: u64) -> Vec<u8> {
    (0..B as u64)
        .map(|j| (i.wrapping_mul(31) ^ j.wrapping_mul(7)) as u8)
        .collect()
}

/// Record `i` of the append region.
fn record(i: u32) -> Vec<u8> {
    format!("record {i:04} of the portable scenario").into_bytes()
}

/// Runs the scenario, making a store at `path`.
fn scenario(path: &Path, opts: &StoreOptions) {
    let s = Store::create(path, opts.clone()).unwrap();
    let pages = s.provision_page_region("pages", 64 * B as u64).unwrap();
    for i in 0..16 {
        pages
            .write_durable(pages.pos(i * B as u64).unwrap(), &block(i))
            .unwrap();
    }
    // A region released at the end, its space then the first that fits
    // the region provisioned after it.
    let gone = s.provision_page_region("gone", 32 * B as u64).unwrap();
    gone.write_durable(gone.pos(0).unwrap(), &block(7)).unwrap();
    let wal = s.provision_append_region("wal", 256 * B as u64).unwrap();
    for i in 0..49 {
        wal.append(&record(i)).unwrap();
    }
    wal.append_durable(&record(49)).unwrap();
    let manifest = s.provision_slot("manifest").unwrap();
    manifest.commit(b"manifest v1").unwrap();
    manifest.commit(b"manifest v2").unwrap();
    // Growth within a cap, then beyond capacity.
    let capped = s
        .provision_page_region_with(
            "capped",
            RegionOptions::default()
                .size(16 * B as u64)
                .max_size(128 * B as u64),
        )
        .unwrap();
    capped.grow(64 * B as u64).unwrap();
    capped
        .write_durable(capped.pos(40 * B as u64).unwrap(), &block(40))
        .unwrap();
    let grown = s.provision_page_region("grown", 16 * B as u64).unwrap();
    grown.grow(96 * B as u64).unwrap();
    grown
        .write_durable(grown.pos(90 * B as u64).unwrap(), &block(90))
        .unwrap();
    // The release, and a later region in its space (26 blocks with its
    // headers, within the 34 released; no other free range holds it, and
    // the file ends at the tail, so placing it anywhere else would grow the
    // file).
    gone.release().unwrap();
    let len = std::fs::metadata(path.join(CONTAINER)).unwrap().len();
    let reuse = s.provision_page_region("reuse", 24 * B as u64).unwrap();
    assert_eq!(
        std::fs::metadata(path.join(CONTAINER)).unwrap().len(),
        len,
        "the later region is not in the released space"
    );
    reuse
        .write_durable(reuse.pos(0).unwrap(), &block(8))
        .unwrap();
    drop((pages, wal, manifest, capped, grown, reuse, s));
    // A writable reopen and a clean close.
    let s = Store::open_with(path, opts.clone()).unwrap();
    let pages = s.page_region("pages").unwrap();
    pages
        .write_durable(pages.pos(16 * B as u64).unwrap(), &block(16))
        .unwrap();
    drop((pages, s));
}

/// Checks a store the scenario made: every region and its bytes, read-only.
fn check(path: &Path) {
    let s = Store::open_readonly(path).unwrap();
    let pages = s.page_region("pages").unwrap();
    let mut back = vec![0u8; B];
    for i in 0..17 {
        pages
            .read(pages.pos(i * B as u64).unwrap(), &mut back)
            .unwrap();
        assert!(back == block(i), "page block {i}");
    }
    let wal = s.append_region("wal").unwrap();
    let mut seen = Vec::new();
    wal.scan(0, |item| {
        if let ScanItem::Data { bytes, .. } = item {
            seen.extend_from_slice(bytes);
        }
        ControlFlow::Continue(())
    })
    .unwrap();
    // Each record, in order (appends may be padded apart).
    let mut from = 0;
    for i in 0..50 {
        let r = record(i);
        let at = seen[from..]
            .windows(r.len())
            .position(|w| w == r.as_slice())
            .unwrap_or_else(|| panic!("append record {i}"));
        from += at + r.len();
    }
    assert_eq!(s.slot("manifest").unwrap().read().unwrap(), b"manifest v2");
    let capped = s.page_region("capped").unwrap();
    capped
        .read(capped.pos(40 * B as u64).unwrap(), &mut back)
        .unwrap();
    assert!(back == block(40), "the capped region's block");
    let grown = s.page_region("grown").unwrap();
    grown
        .read(grown.pos(90 * B as u64).unwrap(), &mut back)
        .unwrap();
    assert!(back == block(90), "the grown region's block");
    assert!(s.page_region("gone").is_err());
    let reuse = s.page_region("reuse").unwrap();
    reuse.read(reuse.pos(0).unwrap(), &mut back).unwrap();
    assert!(back == block(8), "the reused region's block");
}

/// A slot found by its magic and a header CRC that verifies.
struct Slot {
    at: usize,
    object: u64,
    generation: u64,
    header_crc: u32,
    prev_crc: u32,
}

/// A region header's newest slot.
struct Header {
    generation: u64,
    keyed: bool,
    key: [u8; 16],
    extents: Vec<(u64, u64)>,
}

/// A table entry: a live region (the table is the authority on which
/// regions exist).
struct Entry {
    /// Its header pair's object id.
    object: u64,
    header_a: usize,
    header_b: usize,
    extent_count: usize,
    data_size: u64,
}

/// A table page's object id bit (`2^61 | page`).
const PAGE_OBJECT: u64 = 1 << 61;

/// What the pattern-equal blocks of a keyed region's data are replaced
/// with: not zero, so a system that left zeros or holes there instead of
/// the pattern differs.
const FILLED: u8 = 0xA5;

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

/// The container with what differs between any two creates, or is
/// undefined, masked, after checking that what the mask hides is
/// consistent:
///
/// - the block is 4096 (else a larger one the device needs: named);
/// - every slot (at each block boundary, by its magic and a header CRC that
///   verifies) carries one volume id and a payload CRC that verifies; the
///   volume record's slots carry one creation time; every object has at
///   most two slots, a first generation names no predecessor, and a later
///   one's pair holds the generation before it, whose header CRC it names;
/// - masked then: in every slot, the header CRC, the volume id and the
///   previous header's CRC; in the volume record's, the creation time and
///   the payload CRC; in a keyed region header's, the fill key and the
///   payload CRC;
/// - a keyed region's fill: in its data, `[0, data_size)`, every block that
///   holds exactly its pattern (regenerated from its key) is replaced with
///   [`FILLED`]; its slack, `[data_size, capacity)`, is undefined between
///   systems (Linux leaves it unwritten, Windows and macOS write the
///   pattern) and zeroed;
/// - free space: every byte past the table that no table entry's header
///   blocks or committed extents hold, whose contents the format leaves
///   undefined (a released region's data: Linux and macOS punch a hole,
///   Windows trims in place and keeps the bytes; and its headers).
fn masked(path: &Path) -> Vec<u8> {
    let mut bytes = std::fs::read(path.join(CONTAINER)).unwrap();
    assert_eq!(
        bytes[HEADER_LEN], 12,
        "this device needs a block larger than 4096 (log2 {}): the scenario's container cannot be the portable one here",
        bytes[HEADER_LEN]
    );
    let mut slots = Vec::new();
    let mut at = 0;
    while at + B <= bytes.len() {
        let h = &bytes[at..at + HEADER_LEN];
        if h[..8] == MAGIC && usize::from(u16::from_le_bytes([h[12], h[13]])) == HEADER_LEN {
            let mut zeroed = h.to_vec();
            zeroed[8..12].fill(0);
            if crc32c(&zeroed) == u32_at(h, 8) {
                let len = usize::try_from(u32_at(h, 72)).unwrap();
                let payload = &bytes[at + HEADER_LEN..at + HEADER_LEN + len];
                assert_eq!(crc32c(payload), u32_at(h, 28), "payload CRC at {at}");
                assert_eq!(h[32..48], bytes[32..48], "volume id at {at}");
                slots.push(Slot {
                    at,
                    object: u64_at(h, 48),
                    generation: u64_at(h, 56),
                    header_crc: u32_at(h, 8),
                    prev_crc: u32_at(h, 80),
                });
            }
        }
        at += B;
    }
    // One creation time; every pair's chain.
    let created: HashSet<u64> = slots
        .iter()
        .filter(|s| s.object == 0)
        .map(|s| u64_at(&bytes, s.at + HEADER_LEN + 24))
        .collect();
    assert_eq!(created.len(), 1, "the volume record's creation times");
    let mut pairs: HashMap<u64, Vec<&Slot>> = HashMap::new();
    for s in &slots {
        pairs.entry(s.object).or_default().push(s);
    }
    for (object, pair) in &pairs {
        assert!(
            pair.len() <= 2,
            "object {object:#x} has {} slots",
            pair.len()
        );
        // A first generation names no predecessor; of two slots, the newer
        // is the older's next generation and names its header CRC (the
        // older one's predecessor is overwritten); a lone slot is a first
        // generation (nothing here tears a pair).
        for s in pair.iter().filter(|s| s.generation == 1) {
            assert_eq!(
                s.prev_crc, 0,
                "a first generation's predecessor at {}",
                s.at
            );
        }
        if let [x, y] = pair[..] {
            let (older, newer) = if x.generation < y.generation {
                (x, y)
            } else {
                (y, x)
            };
            assert_eq!(
                newer.generation,
                older.generation + 1,
                "the pair at {}",
                newer.at
            );
            assert_eq!(
                newer.prev_crc, older.header_crc,
                "the chain at {}",
                newer.at
            );
        } else {
            assert_eq!(pair[0].generation, 1, "a lone slot at {}", pair[0].at);
        }
    }
    // Each region header's newest slot.
    let mut headers: HashMap<u64, Header> = HashMap::new();
    for s in slots.iter().filter(|s| s.object & REGION_OBJECT != 0) {
        let p = &bytes[s.at + HEADER_LEN..s.at + B];
        let count = usize::from(u16::from_le_bytes([p[14], p[15]]));
        let newer = headers
            .get(&s.object)
            .is_none_or(|h| s.generation > h.generation);
        if newer {
            headers.insert(
                s.object,
                Header {
                    generation: s.generation,
                    keyed: p[10] == 1,
                    key: p[24..40].try_into().unwrap(),
                    extents: (0..count)
                        .map(|k| (u64_at(p, 64 + 16 * k), u64_at(p, 72 + 16 * k)))
                        .collect(),
                },
            );
        }
    }
    // The table's entries, from each page's newest slot.
    let t = 1usize << bytes[HEADER_LEN + 1];
    let pages = u64::from(u32_at(&bytes, HEADER_LEN + 48));
    let mut entries = Vec::new();
    for page in 0..pages {
        let newest = pairs
            .get(&(PAGE_OBJECT | page))
            .and_then(|pair| pair.iter().max_by_key(|s| s.generation))
            .unwrap_or_else(|| panic!("table page {page}"));
        let payload = newest.at + HEADER_LEN;
        let count = usize::from(u16::from_le_bytes([bytes[payload + 8], bytes[payload + 9]]));
        for i in 0..count {
            let e = payload + 24 + 96 * i;
            let seq = u64::from(u32_at(&bytes, e));
            entries.push(Entry {
                object: REGION_OBJECT | (page << 32) | seq,
                header_a: usize::try_from(u64_at(&bytes, e + 40)).unwrap(),
                header_b: usize::try_from(u64_at(&bytes, e + 48)).unwrap(),
                extent_count: usize::from(u16::from_le_bytes([bytes[e + 36], bytes[e + 37]])),
                data_size: u64_at(&bytes, e + 56),
            });
        }
    }
    // The slots' fields.
    for s in &slots {
        let a = s.at;
        bytes[a + 8..a + 12].fill(0);
        bytes[a + 32..a + 48].fill(0);
        bytes[a + 80..a + 84].fill(0);
        let keyed = headers.get(&s.object).is_some_and(|h| h.keyed);
        if s.object == 0 || keyed {
            bytes[a + 28..a + 32].fill(0);
        }
        if s.object == 0 {
            bytes[a + HEADER_LEN + 24..a + HEADER_LEN + 32].fill(0);
        }
        if keyed {
            bytes[a + HEADER_LEN + 24..a + HEADER_LEN + 40].fill(0);
        }
    }
    // What every live region holds: its header blocks and committed
    // extents; and a keyed region's fill.
    let mut live: Vec<(usize, usize)> = Vec::new();
    let mut pattern = vec![0u8; B];
    for e in &entries {
        live.push((e.header_a, B));
        live.push((e.header_b, B));
        let h = headers
            .get(&e.object)
            .unwrap_or_else(|| panic!("no header for region {:#x}", e.object));
        let mut region_offset = 0;
        for &(offset, length) in h.extents.iter().take(e.extent_count) {
            live.push((
                usize::try_from(offset).unwrap(),
                usize::try_from(length).unwrap(),
            ));
            if h.keyed {
                for k in (0..length).step_by(B) {
                    let i = usize::try_from(offset + k).unwrap();
                    if region_offset + k >= e.data_size {
                        bytes[i..i + B].fill(0);
                        continue;
                    }
                    fill_keyed_v1(&h.key, region_offset + k, &mut pattern);
                    if bytes[i..i + B] == pattern[..] {
                        bytes[i..i + B].fill(FILLED);
                    }
                }
            }
            region_offset += length;
        }
    }
    // Free space: past the table (`2B + 2PT`), every block no live region
    // holds.
    let table_end = 2 * B + 2 * usize::try_from(pages).unwrap() * t;
    for i in (table_end..bytes.len()).step_by(B) {
        if !live.iter().any(|&(o, l)| (o..o + l).contains(&i)) {
            let end = (i + B).min(bytes.len());
            bytes[i..end].fill(0);
        }
    }
    bytes
}

/// FNV-1a 64 of `bytes`, with their length.
fn fingerprint(bytes: &[u8]) -> (u64, usize) {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (h, bytes.len())
}

/// Where two masked containers first differ, if they do.
fn first_difference(a: &[u8], b: &[u8]) -> Option<usize> {
    a.iter()
        .zip(b)
        .position(|(x, y)| x != y)
        .or_else(|| (a.len() != b.len()).then(|| a.len().min(b.len())))
}

/// The fixture: the zero-fill scenario's container, made on Linux.
fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/portable")
}

/// Runs the scenario twice with the fill given, checks it, and returns its
/// masked container.
fn twice(keyed: bool) -> (Vec<u8>, TempDir) {
    let (a, b) = (TempDir::new("a"), TempDir::new("b"));
    let opts = options(a.path(), keyed);
    scenario(&a.path().join("s"), &opts);
    scenario(&b.path().join("s"), &opts);
    check(&a.path().join("s"));
    let (ma, mb) = (masked(&a.path().join("s")), masked(&b.path().join("s")));
    assert_eq!(first_difference(&ma, &mb), None, "two runs differ");
    (ma, a)
}

/// The zero-fill scenario makes the same container twice here, and the one
/// every system makes.
#[test]
fn test_the_same_operations_make_the_same_container_on_every_system() {
    let (m, a) = twice(false);
    if let Some(out) = std::env::var_os("STORE_IO_PORTABLE_FIXTURE") {
        // Writes the fixture (run on Linux; see the module doc).
        let out = PathBuf::from(out);
        std::fs::create_dir_all(&out).unwrap();
        std::fs::copy(a.path().join("s").join(CONTAINER), out.join(CONTAINER)).unwrap();
    }
    let f = masked(&fixture());
    assert_eq!(
        first_difference(&m, &f),
        None,
        "this system's container differs from the one made on Linux"
    );
    assert_eq!(
        fingerprint(&m),
        GOLDEN,
        "this system's container differs from the portable one"
    );
}

/// The keyed-fill scenario: the same container but for each region's key
/// and the fill it makes.
#[test]
fn test_keyed_fill_makes_the_same_container_on_every_system() {
    let (m, _a) = twice(true);
    assert_eq!(
        fingerprint(&m),
        GOLDEN_KEYED,
        "this system's keyed container differs from the portable one"
    );
}

/// The container the scenario made on Linux opens here, read-only and
/// writable, with every byte it wrote, and takes writes.
#[test]
fn test_a_container_made_on_linux_opens_here() {
    assert_eq!(
        fingerprint(&masked(&fixture())),
        GOLDEN,
        "the fixture is not what the scenario makes"
    );
    let t = TempDir::new("fixture");
    let path = t.path().join("s");
    std::fs::create_dir_all(&path).unwrap();
    std::fs::copy(fixture().join(CONTAINER), path.join(CONTAINER)).unwrap();
    check(&path);
    let s = Store::open_with(&path, options(t.path(), false)).unwrap();
    let extra = s.provision_page_region("extra", 4 * B as u64).unwrap();
    extra
        .write_durable(extra.pos(0).unwrap(), &block(3))
        .unwrap();
    drop((extra, s));
    check(&path);
    let s = Store::open_readonly(&path).unwrap();
    let extra = s.page_region("extra").unwrap();
    let mut back = vec![0u8; B];
    extra.read(extra.pos(0).unwrap(), &mut back).unwrap();
    assert!(back == block(3));
}
