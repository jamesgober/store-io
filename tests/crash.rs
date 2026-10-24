//! Crash states on a real kernel (D-079): every receipt holds at every point
//! a crash could leave the device in.
//!
//! Driven by the CI job "Crash replay (dm-log-writes)". The file system sits
//! on a `log-writes` device-mapper target, which records on a log device,
//! in order: a FUA write when it completes, any other write at the next
//! flush (until then a power cut could lose it), every flush, and the
//! test's marks at once. A state at a mark therefore holds only what a
//! power cut at that moment would keep.
//!
//! 1. `crash_phase_write` runs a store workload (durable appends, a batch,
//!    durable page writes, slot commits, the format-2 changes: a capped
//!    region provisioned, grown within its capacity and released, another
//!    provisioned, grown beyond its capacity and recycled; then a submitter
//!    with durable and plain writes and a store-wide barrier; last, the
//!    container cut short into the recycled region's grown extent, the
//!    store opened writable, and that region, now unavailable, repaired
//!    away). After each
//!    acknowledgement it logs what was acknowledged to
//!    `$STORE_IO_CRASH_ACKS` and inserts a mark naming it into the log
//!    (`dmsetup message ... mark`); before each slot commit and each
//!    format-2 change it marks the attempt.
//! 2. `crash_phase_replay` reads the log (store-io's own reader of the
//!    `dm-log-writes` format) and, from the mark that follows the store's
//!    creation, rebuilds the device as it stood after every entry: every
//!    write, flush, FUA and mark. At each of those states it mounts a copy
//!    (the file system recovers its journal, as after a power cut), opens
//!    the store read-only, and checks every acknowledgement whose mark
//!    precedes the state: appended records and pages byte for byte, and
//!    each slot holding its last acknowledged value or the value of a
//!    commit already attempted after it, and each region of a format-2
//!    change as it was last acknowledged or as the change attempted after
//!    it makes it (absent, unavailable, or its length and generation),
//!    never anything else; no other region unavailable, nothing frozen.
//!
//! 3. `crash_phase_subsets` (SIM-05): a crash before a flush can keep any
//!    subset of the writes the device still cached, in any order (a cache
//!    writes back the newest data of a block, so overlapping writes keep log
//!    order). For every run of such writes it checks the states keeping
//!    some of them: exhaustively for runs of up to six, else each alone,
//!    all but each one, and seeded random subsets, capped per run.
//!
//! The phases are `#[ignore]`d: they make sense only inside that job. The
//! log reader and the subset generator are unit-tested here.

#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stderr,
    unused_results,
    unused_must_use
)]

use std::collections::BTreeMap;
use std::io::Write as _;
use std::os::unix::fs::FileExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use store_io::engine::completion::Done;
use store_io::engine::{SubmitterConfig, WriteMode};
use store_io::{Error, RegionOptions, Store, StoreOptions, Trust};

// ----- the dm-log-writes log ---------------------------------------------------

const MAGIC: u64 = 0x006a_7366_7773_6872;
const FLUSH: u64 = 1 << 0;
const FUA: u64 = 1 << 1;
const DISCARD: u64 = 1 << 2;
const MARK: u64 = 1 << 3;

/// One logged event, in the order the device completed it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    flags: u64,
    /// First byte on the device, and the length in bytes (writes and
    /// discards).
    at: u64,
    len: u64,
    /// Where a write's data sits in the log.
    data: u64,
    mark: Option<String>,
}

/// The log: its entries in order (`Documentation/admin-guide/device-mapper/
/// log-writes.rst`): a super block in sector 0, then each entry as one
/// header sector followed, for a write, by its data.
fn read_log(log: &std::fs::File) -> Vec<Entry> {
    let mut sup = [0u8; 28];
    log.read_exact_at(&mut sup, 0).unwrap();
    let u64_at = |b: &[u8], i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
    assert_eq!(u64_at(&sup, 0), MAGIC, "not a dm-log-writes log");
    assert_eq!(u64_at(&sup, 8), 1, "log version");
    let n = u64_at(&sup, 16);
    let ss = u64::from(u32::from_le_bytes(sup[24..28].try_into().unwrap()));
    assert!(ss >= 512 && ss.is_power_of_two(), "sector size {ss}");
    let mut out = Vec::with_capacity(n as usize);
    let mut pos = ss;
    let mut head = vec![0u8; ss as usize];
    for _ in 0..n {
        log.read_exact_at(&mut head, pos).unwrap();
        let sector = u64_at(&head, 0);
        let sectors = u64_at(&head, 8);
        let flags = u64_at(&head, 16);
        let data_len = u64_at(&head, 24) as usize;
        pos += ss;
        let mark = (flags & MARK != 0).then(|| {
            String::from_utf8_lossy(&head[32..32 + data_len.min(ss as usize - 32)])
                .trim_end_matches('\0')
                .to_owned()
        });
        let len = sectors * ss;
        let data = pos;
        if flags & DISCARD == 0 {
            pos += len;
        }
        out.push(Entry {
            flags,
            at: sector * ss,
            len,
            data,
            mark,
        });
    }
    out
}

/// Applies one entry to the device image: a write's data, a discard as
/// zeroes (what a discarding device may return), nothing for the rest.
fn apply(log: &std::fs::File, image: &std::fs::File, e: &Entry) {
    if e.len == 0 {
        return;
    }
    let mut buf = vec![0u8; e.len as usize];
    if e.flags & DISCARD == 0 {
        log.read_exact_at(&mut buf, e.data).unwrap();
    }
    image.write_all_at(&buf, e.at).unwrap();
}

// ----- the workload ------------------------------------------------------------

const RECORD: usize = 4096;

fn record(i: u64) -> Vec<u8> {
    let mut r = vec![0u8; RECORD];
    for (j, b) in r.iter_mut().enumerate() {
        *b = (i as usize * 131 + j * 7) as u8;
    }
    r[..8].copy_from_slice(&i.to_le_bytes());
    r
}

fn env_path(var: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(var).unwrap_or_else(|| panic!("{var} is not set")))
}

/// The CI device is a loop device on a virtual machine: refused, so the
/// labelled override is used (the test is about crash states, not class).
fn opts() -> StoreOptions {
    StoreOptions {
        trust: Trust {
            override_refusal: true,
            ..Trust::default()
        },
        queues: 2,
        ..StoreOptions::default()
    }
}

/// Logs acknowledgements, each followed by a mark naming it.
struct Acks {
    file: std::fs::File,
    /// The log-writes target to mark; `None` for a dry run off the CI job.
    dm: Option<String>,
    n: u64,
}

impl Acks {
    /// Records `what` (an acknowledgement or an attempt) and marks the log
    /// after it. The mark is logged at once; writes not yet flushed are
    /// logged only at their flush, so a state at the mark holds exactly
    /// what was durable when it was acknowledged.
    fn note(&mut self, what: &str) {
        let mark = format!("a{}", self.n);
        self.n += 1;
        writeln!(self.file, "{mark} {what}").unwrap();
        self.file.sync_all().unwrap();
        if let Some(dm) = &self.dm {
            let ok = Command::new("sudo")
                .args(["dmsetup", "message", dm, "0", "mark", &mark])
                .status()
                .unwrap()
                .success();
            assert!(ok, "could not mark the log");
        }
    }
}

#[test]
#[ignore = "run by the crash-replay CI job"]
fn crash_phase_write() {
    workload(
        &env_path("STORE_IO_CRASH_DIR").join("store"),
        &env_path("STORE_IO_CRASH_ACKS"),
        std::env::var("STORE_IO_CRASH_DM").ok(),
    );
}

/// The workload of [`crash_phase_write`], marking the log `dm` (if any).
fn workload(dir: &Path, acks_path: &Path, dm: Option<String>) {
    let mut acks = Acks {
        file: std::fs::File::create(acks_path).unwrap(),
        dm,
        n: 0,
    };
    let store = Store::create(dir, opts()).unwrap();
    let wal = store.provision_append_region("wal", 8 << 20).unwrap();
    let pages = store.provision_page_region("pages", 4 << 20).unwrap();
    let slot = store.provision_slot("manifest").unwrap();
    acks.note("created");
    let mut i = 0u64;
    for round in 0..4u64 {
        // Durable appends, one at a time.
        for _ in 0..6 {
            let (pos, _r) = wal.append_durable(&record(i)).unwrap();
            acks.note(&format!("append {i} {}", pos.offset()));
            i += 1;
        }
        // A batch: many appends, one barrier.
        let mut batch = wal.batch();
        let first = i;
        let mut at = Vec::new();
        for _ in 0..8 {
            at.push(batch.append(&record(i)).unwrap());
            i += 1;
        }
        // Offsets within the batch, from where the batch landed.
        let (start, _r) = batch.commit().unwrap();
        for (k, within) in at.iter().enumerate() {
            let off = start.offset() + within;
            acks.note(&format!("append {} {off}", first + k as u64));
        }
        // Durable page writes, each page written once.
        for p in 0..4u64 {
            let page = round * 4 + p;
            let seed = 10_000 + page;
            let pos = pages.pos(page * RECORD as u64).unwrap();
            pages.write_durable(pos, &record(seed)).unwrap();
            acks.note(&format!("page {} {seed}", page * RECORD as u64));
        }
        // A slot commit, marked when attempted and when acknowledged.
        acks.note(&format!("slot-try {round}"));
        slot.commit(&round.to_le_bytes()).unwrap();
        acks.note(&format!("slot {round}"));
    }
    // The format-2 changes (v0.5 E7 C6), each marked when attempted and
    // when acknowledged with the region's state after it: its length and
    // generation, or gone.
    let state = |name: &str, len: u64, generation: u64| format!("{name} {len} {generation}");
    let small = 256u64 << 10;
    acks.note(&format!("region-try {}", state("capped", small, 1)));
    let capped = store
        .provision_page_region_with(
            "capped",
            RegionOptions::default().size(small).max_size(1 << 20),
        )
        .unwrap();
    acks.note(&format!("region {}", state("capped", small, 1)));
    assert_eq!(capped.capacity(), 1 << 20, "capped: room to grow within");
    // Growth within its capacity: one table write.
    acks.note(&format!("region-try {}", state("capped", 1 << 20, 1)));
    capped.grow(1 << 20).unwrap();
    acks.note(&format!("region {}", state("capped", 1 << 20, 1)));
    acks.note(&format!("region-try {}", state("loose", small, 1)));
    let loose = store.provision_page_region("loose", small).unwrap();
    acks.note(&format!("region {}", state("loose", small, 1)));
    assert_eq!(loose.capacity(), small, "loose: no room to grow within");
    // Growth beyond its capacity: a new extent filled, the header, the
    // table.
    let grown = small + (2 << 20);
    acks.note(&format!("region-try {}", state("loose", grown, 1)));
    loose.grow(grown).unwrap();
    assert!(loose.capacity() > small, "loose grew by a new extent");
    acks.note(&format!("region {}", state("loose", grown, 1)));
    acks.note(&format!("region-try {}", state("loose", grown, 2)));
    loose.recycle().unwrap();
    acks.note(&format!("region {}", state("loose", grown, 2)));
    acks.note("region-try capped gone");
    capped.release().unwrap();
    acks.note("region capped gone");
    // A submitter: durable appends acknowledged one by one, then plain
    // appends made durable by a store-wide barrier.
    let mut sub = store.submitter(SubmitterConfig::default()).unwrap();
    let mut live = BTreeMap::new();
    for k in 0..12u64 {
        let mut b = sub.take_buf(RECORD).unwrap();
        b.as_mut_slice().copy_from_slice(&record(i));
        let mode = if k < 6 {
            WriteMode::Durable
        } else {
            WriteMode::Plain
        };
        let pos = sub.append(&wal, b, mode, i).map_err(|r| r.error).unwrap();
        live.insert(i, (pos.offset(), mode));
        i += 1;
    }
    let mut plain = Vec::new();
    while !live.is_empty() {
        for c in sub.wait(1).unwrap() {
            let (off, mode) = live.remove(&c.tag).unwrap();
            assert!(matches!(c.outcome, Ok(Done::Written(_))), "{:?}", c.outcome);
            if mode == WriteMode::Durable {
                acks.note(&format!("append {} {off}", c.tag));
            } else {
                plain.push((c.tag, off));
            }
        }
    }
    sub.barrier_all(u64::MAX).map_err(|r| r.error).unwrap();
    let mut flushed = false;
    while !flushed {
        for c in sub.wait(1).unwrap() {
            assert!(matches!(c.outcome, Ok(Done::Flushed)), "{:?}", c.outcome);
            flushed = true;
        }
    }
    for (tag, off) in plain {
        acks.note(&format!("append {tag} {off}"));
    }
    drop(sub);
    // A repair (v0.5 E7 C6): the container cut short into `loose`'s grown
    // extent (the last space allocated: its tail), the store reopened
    // writable, `loose` unavailable (its extent past the file's end), and
    // repaired away: the entry released.
    drop((wal, pages, slot, loose, store));
    let container = dir.join(store_io::CONTAINER);
    acks.note("region-try loose unavailable");
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&container)
        .unwrap();
    let len = file.metadata().unwrap().len();
    file.set_len(len - (1 << 20)).unwrap();
    file.sync_all().unwrap();
    drop(file);
    acks.note("region loose unavailable");
    let store = Store::open_with(dir, opts()).unwrap();
    let unavailable = store.unavailable_regions();
    assert!(
        unavailable.len() == 1 && unavailable[0].name == "loose",
        "the cut made loose unavailable: {unavailable:?}"
    );
    acks.note("region-try loose gone");
    store.repair_unavailable(unavailable[0].id).unwrap();
    acks.note("region loose gone");
    drop(store);
    eprintln!("{} acknowledgements and attempts marked", acks.n);
}

// ----- replay and check ---------------------------------------------------------

/// What must hold once a mark has been passed.
#[derive(Debug, Clone)]
enum Ack {
    Created,
    Append {
        i: u64,
        offset: u64,
    },
    Page {
        offset: u64,
        seed: u64,
    },
    SlotTry(u64),
    Slot(u64),
    /// A format-2 change attempted: the region's state it makes.
    RegionTry(String, Held),
    /// Acknowledged: the region's state.
    Region(String, Held),
}

/// What a store holds of a changed region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Held {
    Gone,
    Unavailable,
    /// Served: its length and generation.
    Served(u64, u64),
}

fn parse_acks(text: &str) -> BTreeMap<String, Ack> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let w: Vec<&str> = line.split(' ').collect();
        let num = |k: usize| w[k].parse::<u64>().unwrap();
        let ack = match w[1] {
            "created" => Ack::Created,
            "append" => Ack::Append {
                i: num(2),
                offset: num(3),
            },
            "page" => Ack::Page {
                offset: num(2),
                seed: num(3),
            },
            "slot-try" => Ack::SlotTry(num(2)),
            "slot" => Ack::Slot(num(2)),
            "region-try" | "region" => {
                let state = match w[3] {
                    "gone" => Held::Gone,
                    "unavailable" => Held::Unavailable,
                    _ => Held::Served(num(3), num(4)),
                };
                if w[1] == "region" {
                    Ack::Region(w[2].to_owned(), state)
                } else {
                    Ack::RegionTry(w[2].to_owned(), state)
                }
            }
            other => panic!("unknown acknowledgement {other}"),
        };
        out.insert(w[0].to_owned(), ack);
    }
    out
}

/// Unmounts, retrying while the kernel is still letting go of the file
/// system (io_uring releases registered files from a work queue).
fn umount(mnt: &Path) {
    for _ in 0..100 {
        if Command::new("sudo")
            .arg("umount")
            .arg(mnt)
            .status()
            .unwrap()
            .success()
        {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("could not unmount {}", mnt.display());
}

fn sh(cmd: &mut Command) {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{cmd:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Checks one crash state: the store opens read-only and every
/// acknowledgement passed so far holds.
fn check_state(mnt: &Path, passed: &[&Ack]) -> Result<(), String> {
    let store = Store::open_readonly(mnt.join("store")).map_err(|e| format!("open: {e}"))?;
    let wal = store
        .append_region("wal")
        .map_err(|e| format!("wal: {e}"))?;
    let pages = store
        .page_region("pages")
        .map_err(|e| format!("pages: {e}"))?;
    let mut last_slot = None;
    let mut tried = None;
    // Each changed region's acknowledged state, and the state a change
    // attempted after it makes (before any acknowledgement: absent).
    let mut acked: BTreeMap<&str, Held> = BTreeMap::new();
    let mut pending: BTreeMap<&str, Held> = BTreeMap::new();
    for a in passed {
        match a {
            Ack::Created => {}
            Ack::Append { i, offset } => {
                let mut back = vec![0u8; RECORD];
                wal.read(*offset, &mut back)
                    .map_err(|e| format!("record {i}: {e}"))?;
                if back != record(*i) {
                    return Err(format!("record {i} at {offset} lost or altered"));
                }
            }
            Ack::Page { offset, seed } => {
                let mut back = vec![0u8; RECORD];
                pages
                    .read(pages.pos(*offset).map_err(|e| e.to_string())?, &mut back)
                    .map_err(|e| format!("page {offset}: {e}"))?;
                if back != record(*seed) {
                    return Err(format!("page at {offset} lost or altered"));
                }
            }
            Ack::SlotTry(v) => tried = Some(*v),
            Ack::Slot(v) => last_slot = Some(*v),
            Ack::RegionTry(name, state) => {
                acked.entry(name.as_str()).or_insert(Held::Gone);
                pending.insert(name.as_str(), *state);
            }
            Ack::Region(name, state) => {
                acked.insert(name.as_str(), *state);
                pending.remove(name.as_str());
            }
        }
    }
    if store.frozen() {
        return Err(format!("frozen: {:?}", store.report().metadata));
    }
    let unavailable = store.unavailable_regions();
    for (name, was) in &acked {
        let found = if unavailable.iter().any(|u| u.name == *name) {
            Held::Unavailable
        } else {
            match store.page_region(name) {
                Ok(r) => Held::Served(r.len(), r.generation().get()),
                Err(Error::NotFound { .. }) => Held::Gone,
                Err(e) => return Err(format!("region {name}: {e}")),
            }
        };
        if found != *was && pending.get(name) != Some(&found) {
            return Err(format!(
                "region {name} is {found:?}: acknowledged {was:?}, attempted {:?}",
                pending.get(name)
            ));
        }
    }
    if let Some(u) = unavailable
        .iter()
        .find(|u| !acked.contains_key(u.name.as_str()))
    {
        return Err(format!("region unavailable: {u:?}"));
    }
    let slot = store.slot("manifest").map_err(|e| format!("slot: {e}"))?;
    let value = slot
        .read()
        .map(|b| u64::from_le_bytes(b.as_slice().try_into().unwrap_or([0xFF; 8])));
    // The last acknowledged value, or the one of a commit attempted after
    // it; before any acknowledgement, empty is allowed too.
    let allowed =
        |v: Option<u64>| v == last_slot || (v.is_some() && v == tried && tried > last_slot);
    if !allowed(value) {
        return Err(format!(
            "slot holds {value:?}: last acknowledged {last_slot:?}, last attempted {tried:?}"
        ));
    }
    Ok(())
}

#[test]
#[ignore = "run by the crash-replay CI job"]
fn crash_phase_replay() {
    let log = std::fs::File::open(env_path("STORE_IO_CRASH_LOG")).unwrap();
    let size: u64 = std::env::var("STORE_IO_CRASH_SIZE")
        .unwrap()
        .parse()
        .unwrap();
    let acks = parse_acks(&std::fs::read_to_string(env_path("STORE_IO_CRASH_ACKS")).unwrap());
    let mnt = env_path("STORE_IO_CRASH_MNT");
    let work = env_path("STORE_IO_CRASH_WORK");
    let fs = std::env::var("STORE_IO_CRASH_FS").unwrap_or_default();
    let entries = read_log(&log);
    let base_path = work.join("base.img");
    let base = std::fs::File::create(&base_path).unwrap();
    base.set_len(size).unwrap();
    let created = entries
        .iter()
        .position(|e| {
            e.mark
                .as_deref()
                .is_some_and(|m| acks.get(m).is_some_and(|a| matches!(a, Ack::Created)))
        })
        .expect("the log has no mark for the store's creation");
    let mut passed: Vec<&Ack> = Vec::new();
    let (mut states, mut flushes, mut marks) = (0u64, 0u64, 0u64);
    for (n, e) in entries.iter().enumerate() {
        apply(&log, &base, e);
        if let Some(m) = &e.mark {
            if let Some(a) = acks.get(m) {
                passed.push(a);
            }
            marks += 1;
        }
        if e.flags & (FLUSH | FUA) != 0 {
            flushes += 1;
        }
        if n < created {
            continue;
        }
        // The device as it stood after entry `n`, mounted as after a crash
        // (a fresh file each time, so no loop device can hold a stale one).
        base.sync_data().unwrap();
        let state_path = work.join(format!("state-{n}.img"));
        sh(Command::new("cp")
            .args(["--sparse=always"])
            .arg(&base_path)
            .arg(&state_path));
        let mut mount = Command::new("sudo");
        mount.args(["mount", "-o"]);
        mount.arg(if fs == "xfs" { "loop,nouuid" } else { "loop" });
        mount.arg(&state_path).arg(&mnt);
        sh(&mut mount);
        let checked = check_state(&mnt, &passed);
        umount(&mnt);
        if checked.is_ok() {
            std::fs::remove_file(&state_path).unwrap();
        }
        if let Err(why) = checked {
            panic!(
                "crash state after entry {n} of {} (flags {:#x}, mark {:?}): {why}",
                entries.len(),
                e.flags,
                e.mark
            );
        }
        states += 1;
        if states % 100 == 0 {
            eprintln!(
                "{states} crash states checked (entry {n} of {})",
                entries.len()
            );
        }
    }
    // Every acknowledgement was found in the log, in the order noted: a
    // missing mark cannot shrink what was checked.
    assert_eq!(
        passed.len(),
        acks.len(),
        "acknowledgements missing from the log"
    );
    let order: Vec<u64> = entries
        .iter()
        .filter_map(|e| e.mark.as_deref()?.strip_prefix('a')?.parse().ok())
        .collect();
    assert!(
        order.iter().enumerate().all(|(k, n)| *n == k as u64),
        "marks out of order: {order:?}"
    );
    eprintln!(
        "{states} crash states checked ({} entries, {flushes} flushes or FUA, {marks} marks, {} acknowledgements)",
        entries.len(),
        acks.len()
    );
    assert!(states > 0);
}

// ----- subsets between flushes (SIM-05) -------------------------------------------

/// A write the device may still hold in its volatile cache when it is logged:
/// not FUA, not a flush, discard or mark. dm-log-writes logs such writes at
/// the flush that makes them durable, as one run just before it.
fn is_cached_write(e: &Entry) -> bool {
    e.len > 0 && e.flags & (FLUSH | FUA | DISCARD | MARK) == 0
}

/// Which members of a run of `k` cached writes a crash before their flush
/// could have kept, besides none and all (the prefix states), at most `cap`
/// distinct subsets, each sorted: every proper subset when they fit;
/// otherwise each write alone and all but each one (as many as fit), then
/// seeded random subsets.
fn subsets(k: usize, cap: usize, seed: u64) -> Vec<Vec<usize>> {
    let mut out: Vec<Vec<usize>> = Vec::new();
    if k < 2 || cap == 0 {
        return out;
    }
    if k < 63 && (1u64 << k) - 2 <= cap as u64 {
        for mask in 1..(1u64 << k) - 1 {
            out.push((0..k).filter(|i| mask & (1 << i) != 0).collect());
        }
        return out;
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut add = |s: Vec<usize>, out: &mut Vec<Vec<usize>>| {
        if out.len() < cap && !s.is_empty() && s.len() < k && seen.insert(s.clone()) {
            out.push(s);
        }
    };
    for i in 0..k {
        add(vec![i], &mut out);
        add((0..k).filter(|j| *j != i).collect(), &mut out);
    }
    // splitmix64: well mixed from any seed (an entry index is small).
    let mut state = seed;
    let mut next = || {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };
    // Bounded: duplicates are skipped, never retried for ever.
    for _ in 0..cap.saturating_mul(8) {
        if out.len() == cap {
            break;
        }
        let s: Vec<usize> = (0..k).filter(|_| next() >> 63 == 1).collect();
        add(s, &mut out);
    }
    out
}

#[allow(clippy::too_many_arguments)]
/// Copies `base`, applies `extra` writes, mounts the copy as after a crash
/// and checks `passed`; the copy is removed.
fn check_copy(
    work: &Path,
    base_path: &Path,
    log: &std::fs::File,
    extra: &[&Entry],
    fs: &str,
    mnt: &Path,
    passed: &[&Ack],
    name: &str,
) -> Result<(), String> {
    let state_path = work.join(format!("{name}.img"));
    sh(Command::new("cp")
        .args(["--sparse=always"])
        .arg(base_path)
        .arg(&state_path));
    let state = std::fs::OpenOptions::new()
        .write(true)
        .open(&state_path)
        .unwrap();
    for e in extra {
        apply(log, &state, e);
    }
    drop(state);
    let mut mount = Command::new("sudo");
    mount.args(["mount", "-o"]);
    mount.arg(if fs == "xfs" { "loop,nouuid" } else { "loop" });
    mount.arg(&state_path).arg(mnt);
    let mounted = mount.output().unwrap();
    let checked = if mounted.status.success() {
        let c = check_state(mnt, passed);
        umount(mnt);
        c
    } else {
        Err(format!(
            "the file system does not mount: {}",
            String::from_utf8_lossy(&mounted.stderr).trim()
        ))
    };
    // A failing state is kept for the CI job to upload.
    if checked.is_ok() {
        std::fs::remove_file(&state_path).unwrap();
    } else {
        eprintln!("kept the failing state as {}", state_path.display());
    }
    checked
}

/// Subset states (SIM-05): for every run of cached writes from the store's
/// creation on, the device as a crash before their flush could leave it,
/// keeping only some of them: the store opens and every acknowledgement
/// passed before the run holds. `STORE_IO_CRASH_SUBSETS` caps the subsets
/// per run (default 16).
#[test]
#[ignore = "run by the crash-replay CI job"]
fn crash_phase_subsets() {
    let log = std::fs::File::open(env_path("STORE_IO_CRASH_LOG")).unwrap();
    let size: u64 = std::env::var("STORE_IO_CRASH_SIZE")
        .unwrap()
        .parse()
        .unwrap();
    let cap: usize = std::env::var("STORE_IO_CRASH_SUBSETS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(16);
    let acks = parse_acks(&std::fs::read_to_string(env_path("STORE_IO_CRASH_ACKS")).unwrap());
    let mnt = env_path("STORE_IO_CRASH_MNT");
    let work = env_path("STORE_IO_CRASH_WORK");
    let fs = std::env::var("STORE_IO_CRASH_FS").unwrap_or_default();
    let entries = read_log(&log);
    let base_path = work.join("subset-base.img");
    let base = std::fs::File::create(&base_path).unwrap();
    base.set_len(size).unwrap();
    let mut passed: Vec<&Ack> = Vec::new();
    let mut created = false;
    let (mut runs, mut states, mut largest) = (0u64, 0u64, 0usize);
    let mut n = 0;
    while n < entries.len() {
        let e = &entries[n];
        if created && is_cached_write(e) {
            let end = n + entries[n..]
                .iter()
                .take_while(|e| is_cached_write(e))
                .count();
            let run: Vec<&Entry> = entries[n..end].iter().collect();
            base.sync_data().unwrap();
            for s in subsets(run.len(), cap, n as u64) {
                let kept: Vec<&Entry> = s.iter().map(|i| run[*i]).collect();
                if let Err(why) =
                    check_copy(&work, &base_path, &log, &kept, &fs, &mnt, &passed, "subset")
                {
                    panic!(
                        "crash keeping writes {s:?} of the {} cached before entry {end}: {why}",
                        run.len()
                    );
                }
                states += 1;
            }
            runs += 1;
            largest = largest.max(run.len());
            for w in &run {
                apply(&log, &base, w);
            }
            n = end;
            continue;
        }
        apply(&log, &base, e);
        if let Some(a) = e.mark.as_deref().and_then(|m| acks.get(m)) {
            created |= matches!(a, Ack::Created);
            passed.push(a);
        }
        n += 1;
    }
    std::fs::remove_file(&base_path).unwrap();
    eprintln!(
        "{states} subset states checked over {runs} runs of cached writes (largest {largest}, at most {cap} subsets each)"
    );
    assert!(created, "the log has no mark for the store's creation");
    assert_eq!(
        passed.len(),
        acks.len(),
        "acknowledgements missing from the log"
    );
    assert!(states > 0, "no run of cached writes was enumerated");
}

#[test]
fn test_subsets_are_proper_distinct_and_bounded() {
    assert!(subsets(1, 16, 1).is_empty());
    // Exhaustive when they fit: 2^3 - 2, neither none nor all.
    assert_eq!(subsets(3, 16, 1).len(), 6);
    assert_eq!(subsets(7, 126, 1).len(), 126);
    for (k, cap) in [(5usize, 16usize), (7, 16), (40, 64), (200, 64)] {
        for seed in [0u64, 1, 99] {
            let s = subsets(k, cap, seed);
            assert_eq!(s.len(), cap, "k {k} cap {cap}");
            let distinct: std::collections::BTreeSet<_> = s.iter().collect();
            assert_eq!(distinct.len(), s.len(), "k {k}: duplicates");
            assert!(s.iter().all(|x| !x.is_empty() && x.len() < k));
            assert!(s.iter().all(|x| x.windows(2).all(|w| w[0] < w[1])));
        }
    }
    // Each write alone and all but each one come first.
    let s = subsets(10, 64, 3);
    assert_eq!((&s[0], s[1].len()), (&vec![0], 9));
}

// ----- without a crash: the workload passes its own checks ------------------------

/// The final state, with no crash, satisfies every acknowledgement: the
/// workload and the checker agree (the CI job then applies the checker to
/// every crash state).
#[test]
fn test_the_workload_passes_its_own_checks_without_a_crash() {
    let base = std::env::var_os("HOME")
        .map_or_else(std::env::temp_dir, PathBuf::from)
        .join(format!(".store-io-crash-dry-{}", std::process::id()));
    std::fs::create_dir_all(&base).unwrap();
    let acks_path = base.join("acks");
    workload(&base.join("store"), &acks_path, None);
    let acks = parse_acks(&std::fs::read_to_string(&acks_path).unwrap());
    // In the order they were noted (mark names are `a0`, `a1`, ...).
    let mut ordered: Vec<(u64, &Ack)> = acks
        .iter()
        .map(|(m, a)| (m[1..].parse().unwrap(), a))
        .collect();
    ordered.sort_by_key(|(n, _)| *n);
    let passed: Vec<&Ack> = ordered.into_iter().map(|(_, a)| a).collect();
    let checked = check_state(&base, &passed);
    let _cleanup = std::fs::remove_dir_all(&base);
    checked.unwrap();
}

// ----- the reader, on a log built in memory ---------------------------------------

#[test]
fn test_the_log_reader_reads_writes_discards_and_marks() {
    let ss = 512u64;
    let dir = std::env::temp_dir().join(format!("store-io-crash-unit-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("log");
    let mut log = Vec::new();
    let mut sup = vec![0u8; ss as usize];
    sup[0..8].copy_from_slice(&MAGIC.to_le_bytes());
    sup[8..16].copy_from_slice(&1u64.to_le_bytes());
    sup[16..24].copy_from_slice(&4u64.to_le_bytes());
    sup[24..28].copy_from_slice(&(ss as u32).to_le_bytes());
    log.extend_from_slice(&sup);
    let header = |sector: u64, sectors: u64, flags: u64, label: &str| {
        let mut h = vec![0u8; ss as usize];
        h[0..8].copy_from_slice(&sector.to_le_bytes());
        h[8..16].copy_from_slice(&sectors.to_le_bytes());
        h[16..24].copy_from_slice(&flags.to_le_bytes());
        h[24..32].copy_from_slice(&(label.len() as u64).to_le_bytes());
        h[32..32 + label.len()].copy_from_slice(label.as_bytes());
        h
    };
    // A two-sector write at sector 3, a flush, a discard of sector 3, a mark.
    log.extend(header(3, 2, 0, ""));
    log.extend(vec![0xAB; 2 * ss as usize]);
    log.extend(header(0, 0, FLUSH, ""));
    log.extend(header(3, 1, DISCARD, ""));
    log.extend(header(0, 0, MARK, "a7"));
    std::fs::write(&path, &log).unwrap();
    let f = std::fs::File::open(&path).unwrap();
    let entries = read_log(&f);
    assert_eq!(entries.len(), 4);
    assert_eq!((entries[0].at, entries[0].len), (3 * ss, 2 * ss));
    assert_eq!(entries[1].flags, FLUSH);
    assert_eq!(
        (entries[2].at, entries[2].len, entries[2].flags),
        (3 * ss, ss, DISCARD)
    );
    assert_eq!(entries[3].mark.as_deref(), Some("a7"));
    let image_path = dir.join("image");
    let image = std::fs::File::create(&image_path).unwrap();
    image.set_len(8 * ss).unwrap();
    apply(&f, &image, &entries[0]);
    let after_write = std::fs::read(&image_path).unwrap();
    assert!(
        after_write[3 * ss as usize..5 * ss as usize]
            .iter()
            .all(|&b| b == 0xAB)
    );
    assert!(after_write[..3 * ss as usize].iter().all(|&b| b == 0));
    apply(&f, &image, &entries[2]);
    let after_discard = std::fs::read(&image_path).unwrap();
    assert!(
        after_discard[3 * ss as usize..4 * ss as usize]
            .iter()
            .all(|&b| b == 0)
    );
    assert!(
        after_discard[4 * ss as usize..5 * ss as usize]
            .iter()
            .all(|&b| b == 0xAB)
    );
    let _cleanup = std::fs::remove_dir_all(&dir);
}
