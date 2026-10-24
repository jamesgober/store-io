//! Writes, flushes and reads whose queue wait fails are abandoned (D-097):
//! a write's fate is then unknown, so the call fails `DurabilityUnknown` and
//! the store is poisoned; a read fails `Io`; nothing is ever reported as
//! done, and a failed wait is never taken for a media verdict.

#![allow(clippy::unwrap_used, unused_results, unused_must_use)]

use std::path::{Path, PathBuf};

use store_io_core::decide::Trust;
use store_io_core::error::Error;
use store_io_engine::{CONTAINER, Directory, Store, StoreOptions};
use store_io_platform::{CompletionBuf, FileMode, FileName, IoOp, Platform, Queue, QueueConfig};
use store_io_sim::{SimConfig, SimPlatform};

const MIB: usize = 1 << 20;
/// The simulator's EIO.
const EIO: i32 = 5;

fn opts() -> StoreOptions {
    StoreOptions {
        queues: 1,
        ..StoreOptions::default()
    }
}

/// Fails the next queue wait.
fn fail_next_wait(p: &SimPlatform) {
    p.with_world(|w| {
        let next = w.waits() + 1;
        w.faults_mut().fail_wait = Some((next, EIO));
    });
}

/// Every operation submitted and waited for (also with
/// `STORE_IO_SIM_INLINE`): these are the waits' failures.
fn waited(seed: u64) -> SimConfig {
    SimConfig {
        inline: false,
        ..SimConfig::volatile(seed)
    }
}

fn store(seed: u64) -> (SimPlatform, Store<SimPlatform>) {
    let p = SimPlatform::new(waited(seed));
    let s = Store::create(p.clone(), Path::new("/db"), opts()).unwrap();
    (p, s)
}

#[test]
fn test_an_abandoned_page_write_is_unknown_and_poisons() {
    let (p, s) = store(600);
    let pages = s.provision_page_region("pages", MIB as u64).unwrap();
    let bs = pages.block_size() as usize;
    fail_next_wait(&p);
    let r = pages.write(pages.pos(0).unwrap(), &vec![1; bs]);
    assert!(matches!(r, Err(Error::DurabilityUnknown { .. })), "{r:?}");
    assert!(s.poisoned().is_some());
    let again = pages.write(pages.pos(0).unwrap(), &vec![2; bs]);
    assert!(matches!(again, Err(Error::Poisoned { .. })), "{again:?}");
}

#[test]
fn test_an_abandoned_slot_commit_is_unknown_and_poisons() {
    let (p, s) = store(601);
    let slot = s.provision_slot("manifest").unwrap();
    slot.commit(b"v1").unwrap();
    fail_next_wait(&p);
    let r = slot.commit(b"v2");
    assert!(matches!(r, Err(Error::DurabilityUnknown { .. })), "{r:?}");
    assert!(s.poisoned().is_some());
    assert!(matches!(slot.commit(b"v3"), Err(Error::Poisoned { .. })));
}

/// The flush a barrier leads is abandoned: no receipt, and the store stops.
#[test]
fn test_an_abandoned_flush_gives_no_receipt_and_poisons() {
    let (p, s) = store(602);
    let wal = s.provision_append_region("wal", MIB as u64).unwrap();
    let ticket = wal.append(b"needs a flush").unwrap();
    fail_next_wait(&p);
    let r = wal.sync_through(&ticket);
    assert!(matches!(r, Err(Error::DurabilityUnknown { .. })), "{r:?}");
    assert!(s.poisoned().is_some());
    assert!(matches!(wal.append(b"x"), Err(Error::Poisoned { .. })));
}

/// Clearing an earlier caller's abandoned reads fails: the batch fails with
/// `Io` before anything of it is submitted, the store is not poisoned, and
/// a retry (which clears the queue) lands every byte.
#[test]
fn test_a_failed_clear_fails_the_batch_before_any_write() {
    let (p, s) = store(603);
    let pages = s.provision_page_region("pages", 8 * MIB as u64).unwrap();
    let bs = pages.block_size() as usize;
    // Abandon a large read (reads in flight on the parallel queue).
    fail_next_wait(&p);
    let mut out = vec![0u8; 3 * MIB];
    assert!(pages.read(pages.pos(0).unwrap(), &mut out).is_err());
    let writes = p.with_world(|w| w.writes());
    let fill = |i: usize| vec![0x40 | i as u8; bs];
    let stage = || {
        let mut batch = pages.batch();
        for i in 0..8usize {
            batch
                .write(pages.pos((i * 2 * bs) as u64).unwrap(), &fill(i))
                .unwrap();
        }
        batch
    };
    // The clear's own wait fails.
    fail_next_wait(&p);
    let r = stage().commit();
    assert!(matches!(r, Err(Error::Io { .. })), "{r:?}");
    assert_eq!(p.with_world(|w| w.writes()), writes, "a write was issued");
    assert!(s.poisoned().is_none());
    stage().commit().unwrap();
    let mut one = vec![0u8; bs];
    for i in 0..8usize {
        pages
            .read(pages.pos((i * 2 * bs) as u64).unwrap(), &mut one)
            .unwrap();
        assert!(one == fill(i), "write {i} did not land");
    }
}

const DIR: &str = "/conf";

fn directory(seed: u64) -> (SimPlatform, Directory<SimPlatform>) {
    let p = SimPlatform::new(waited(seed));
    drop(Directory::open(p.clone(), Path::new(DIR), true, Trust::default()).unwrap());
    let d = Directory::open(p.clone(), Path::new(DIR), false, Trust::default()).unwrap();
    (p, d)
}

#[test]
fn test_an_abandoned_directory_write_fails_and_stops_the_handle() {
    let (p, d) = directory(604);
    d.replace("CURRENT", b"one").unwrap();
    fail_next_wait(&p);
    assert!(d.replace("CURRENT", b"two").is_err());
    assert!(
        d.replace("CURRENT", b"three").is_err(),
        "the handle went on after an abandoned write"
    );
}

#[test]
fn test_an_abandoned_directory_read_is_io_not_media() {
    let (p, d) = directory(605);
    d.replace("CURRENT", b"one").unwrap();
    d.replace("OTHER", b"two").unwrap();
    fail_next_wait(&p);
    let r = d.read("CURRENT");
    assert!(matches!(r, Err(Error::Io { .. })), "{r:?}");
    // The next read, of another file, gets its own bytes, never the
    // abandoned read's.
    assert_eq!(d.read("OTHER").unwrap(), b"two");
    assert_eq!(d.read("CURRENT").unwrap(), b"one");
}

/// A leftover container that holds a store's first slot is never reclaimed,
/// whichever of create's waits fails.
#[test]
fn test_create_never_reclaims_a_container_after_a_failed_wait() {
    let p = SimPlatform::new(waited(606));
    drop(Store::create(p.clone(), Path::new("/a"), opts()).unwrap());
    let name = FileName::new(CONTAINER).unwrap();
    // The first 4 KiB of a real container, in a small file at /b: what a
    // create interrupted after its first slot leaves behind.
    let head = {
        let dir = p.open_dir(Path::new("/a"), false).unwrap();
        let f = p.open_file(&dir, &name, FileMode::ReadOnly).unwrap();
        let pool =
            store_io_buf::BufPool::new(&store_io_buf::PoolConfig::uniform(4096, 4096, 4096, 2))
                .unwrap();
        let mut q = p.queue(QueueConfig::default()).unwrap();
        q.submit(
            IoOp::Read {
                file: &f,
                offset: 0,
                buf: pool.take(4096).unwrap(),
            },
            0,
        )
        .unwrap();
        let mut out = CompletionBuf::with_capacity(1);
        q.wait(1, &mut out).unwrap();
        out.drain().next().unwrap().buf.unwrap().as_slice().to_vec()
    };
    {
        let dir = p.open_dir(Path::new("/b"), true).unwrap();
        let f = p.create_file(&dir, &name).unwrap();
        p.allocate(&f, 8192).unwrap();
        let pool =
            store_io_buf::BufPool::new(&store_io_buf::PoolConfig::uniform(4096, 4096, 4096, 2))
                .unwrap();
        let mut buf = pool.take(4096).unwrap();
        buf.as_mut_slice().copy_from_slice(&head);
        let mut q = p.queue(QueueConfig::default()).unwrap();
        q.submit(
            IoOp::Write {
                file: &f,
                offset: 0,
                buf,
                dsync: true,
            },
            0,
        )
        .unwrap();
        let mut out = CompletionBuf::with_capacity(1);
        q.wait(1, &mut out).unwrap();
        p.sync_dir(&dir).unwrap();
    }
    let size = || {
        let dir = p.open_dir(Path::new("/b"), false).unwrap();
        let f = p.open_file(&dir, &name, FileMode::ReadOnly).unwrap();
        p.size(&f).unwrap()
    };
    for k in 1..=12u64 {
        p.with_world(|w| {
            let at = w.waits() + k;
            w.faults_mut().fail_wait = Some((at, EIO));
        });
        let r = Store::create(p.clone(), &PathBuf::from("/b"), opts());
        p.with_world(|w| w.faults_mut().fail_wait = None);
        assert!(r.is_err(), "the container was reclaimed (wait {k} failed)");
        assert_eq!(
            size(),
            8192,
            "the container was truncated (wait {k} failed)"
        );
    }
}

/// A recycle whose lane could not be cleared fails with nothing changed:
/// the region stays usable (it was locked until the store was reopened).
#[test]
fn test_a_failed_clear_during_recycle_leaves_the_region_usable() {
    let (p, s) = store(607);
    let pages = s.provision_page_region("pages", MIB as u64).unwrap();
    let bs = pages.block_size() as usize;
    pages
        .write_durable(pages.pos(0).unwrap(), &vec![7; bs])
        .unwrap();
    // Abandon a single read on the shared queue, then fail the wait that
    // would clear it for the recycle.
    fail_next_wait(&p);
    let mut one = vec![0u8; bs];
    assert!(pages.read(pages.pos(0).unwrap(), &mut one).is_err());
    let g0 = pages.generation();
    fail_next_wait(&p);
    assert!(matches!(pages.recycle(), Err(Error::Io { .. })));
    assert!(s.poisoned().is_none());
    assert_eq!(
        pages.generation(),
        g0,
        "a failed recycle changed the generation"
    );
    pages.read(pages.pos(0).unwrap(), &mut one).unwrap();
    assert!(one.iter().all(|&b| b == 7));
    let fresh = pages.recycle().unwrap();
    fresh
        .write_durable(fresh.pos(0).unwrap(), &vec![8; bs])
        .unwrap();
}

/// Provisioning whose queue wait fails at any step never makes the region
/// ready with a fill write still in flight, and never reads an abandoned
/// write as "out of space" (even when the failed wait's code is ENOSPC).
#[test]
fn test_provisioning_with_a_failed_wait_never_succeeds_or_runs_out_of_space() {
    for code in [EIO, 28] {
        let mut saw_ok = false;
        for k in 1..=24u64 {
            let (p, s) = store(700 + k);
            let at = p.with_world(|w| {
                let at = w.waits() + k;
                w.faults_mut().fail_wait = Some((at, code));
                at
            });
            let r = s.provision_page_region("pages", 4 * MIB as u64);
            let fired = p.with_world(|w| w.waits() >= at);
            match r {
                Ok(_) => {
                    assert!(!fired, "ready after wait {k} failed (code {code})");
                    saw_ok = true;
                }
                Err(Error::NoSpace { .. }) => {
                    panic!("an abandoned write was read as no space (wait {k}, code {code})")
                }
                Err(
                    Error::DurabilityUnknown { .. } | Error::Io { .. } | Error::Poisoned { .. },
                ) => {
                    assert!(fired);
                }
                Err(e) => panic!("wait {k}, code {code}: {e:?}"),
            }
        }
        // Some step lay past every wait: every step of provisioning was
        // covered.
        assert!(saw_ok, "provisioning has more than 24 waits now");
    }
}

/// A read-only open whose queue wait fails at any step fails `Io`: it
/// writes nothing, so every wait is a read's, and a failed one is never
/// taken for an unreadable location (the volume probe moving on to the next
/// candidate) or an unreadable slot (a pair decided from its other copy, an
/// older state). Both formats: format 1 opens read-only from 0.5. Without
/// a failure it opens every region.
#[test]
fn test_a_read_only_open_with_a_failed_wait_fails_io() {
    for (seed, format) in [
        (610, store_io_format::slot::FORMAT_1),
        (611, store_io_format::slot::FORMAT_2),
    ] {
        let p = SimPlatform::new(waited(seed));
        // A small table (format 2's default reads 388 pages: a wait failing
        // at each of them is the same case 388 times).
        let opts = || StoreOptions {
            queues: 1,
            region_capacity: 400,
            container_format: format,
            ..StoreOptions::default()
        };
        let s = Store::create(p.clone(), Path::new("/db"), opts()).unwrap();
        // Several generations of every pair: the table's (format 1) and the
        // volume record's (format 2, by a writable reopen).
        for name in ["r0", "r1", "r2"] {
            s.provision_page_region(name, MIB as u64).unwrap();
        }
        drop(s);
        if format == store_io_format::slot::FORMAT_2 {
            drop(Store::open(p.clone(), Path::new("/db"), opts()).unwrap());
        }
        let mut clean = false;
        for k in 1..=80u64 {
            let at = p.with_world(|w| {
                let at = w.waits() + k;
                w.faults_mut().fail_wait = Some((at, EIO));
                at
            });
            let r = Store::open_readonly(p.clone(), Path::new("/db"), opts());
            let fired = p.with_world(|w| {
                w.faults_mut().fail_wait = None;
                w.waits() >= at
            });
            match r {
                Err(Error::Io { raw, .. }) if fired => assert_eq!(raw.code, EIO),
                Ok(s) if !fired => {
                    let mut names: Vec<String> = s.regions().into_iter().map(|(n, _)| n).collect();
                    names.sort();
                    assert_eq!(names, ["r0", "r1", "r2"], "format {format}");
                    clean = true;
                    break;
                }
                Ok(_) => panic!("format {format}: opened after wait {k} failed"),
                Err(e) => panic!("format {format}, wait {k} (fired: {fired}): {e:?}"),
            }
        }
        assert!(
            clean,
            "format {format}: the open has more than 80 waits now"
        );
    }
}
