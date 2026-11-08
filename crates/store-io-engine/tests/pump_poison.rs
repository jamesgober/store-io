//! No write piece goes to a poisoned store (fail-stop, FAIL-01; FILEMAP
//! draft 23 M3): a long blocking write split into pieces, an append and a
//! provisioning fill each stop at a poison set by another operation, their
//! pieces in flight waited for, and return `Poisoned`.
//!
//! One test in its own binary: the test hook it installs is process-wide,
//! so its cases run one after another.

#![allow(clippy::unwrap_used, unused_results)]

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};

use store_io_core::error::Error;
use store_io_engine::test_hook::{self, Step};
use store_io_engine::{Store, StoreOptions};
use store_io_sim::{SimConfig, SimPlatform};

/// The store the hook poisons, its platform, and the step it counts.
static STORE: Mutex<Option<(Store<SimPlatform>, SimPlatform)>> = Mutex::new(None);
static STEP: AtomicU8 = AtomicU8::new(0);
static SEEN: AtomicUsize = AtomicUsize::new(0);
/// The device's write submits when the counted step first ran.
static FIRST: AtomicU64 = AtomicU64::new(0);

const PUMP: u8 = 1;
const FILL: u8 = 2;

/// Poisons the store at the counted step's second run (after one piece).
fn poison_at_second_piece(s: Step) {
    let counted = match STEP.load(Ordering::SeqCst) {
        PUMP => s == Step::PumpPiece,
        FILL => s == Step::FillPiece,
        _ => false,
    };
    if !counted {
        return;
    }
    let n = SEEN.fetch_add(1, Ordering::SeqCst);
    if let Some((store, p)) = STORE.lock().unwrap().as_ref() {
        if n == 0 {
            FIRST.store(p.with_world(|w| w.write_submits()), Ordering::SeqCst);
        }
        if n == 1 {
            store.poison();
        }
    }
}

/// Runs `f` with the hook counting `step`; the device's write submits since
/// the step's first run.
fn under_hook<R>(
    p: &SimPlatform,
    s: &Store<SimPlatform>,
    step: u8,
    f: impl FnOnce() -> R,
) -> (R, u64) {
    *STORE.lock().unwrap() = Some((s.clone(), p.clone()));
    SEEN.store(0, Ordering::SeqCst);
    STEP.store(step, Ordering::SeqCst);
    test_hook::install(Some(poison_at_second_piece as fn(_)));
    let r = f();
    test_hook::install(None);
    STEP.store(0, Ordering::SeqCst);
    *STORE.lock().unwrap() = None;
    let pieces = p.with_world(|w| w.write_submits()) - FIRST.load(Ordering::SeqCst);
    (r, pieces)
}

/// A store whose writes go out in 1 MiB pieces.
fn store(seed: u64) -> (SimPlatform, Store<SimPlatform>) {
    let p = SimPlatform::new(SimConfig::volatile(seed));
    let opts = StoreOptions {
        max_io: 1 << 20,
        ..StoreOptions::default()
    };
    let s = Store::create(p.clone(), Path::new("/db"), opts).unwrap();
    (p, s)
}

#[test]
fn test_a_poison_stops_every_multi_piece_write() {
    // A 4 MiB page write: four pieces unhooked, one after a poison.
    let (p, s) = store(1);
    let pages = s.provision_page_region("pages", 8 << 20).unwrap();
    let data = vec![9u8; 4 << 20];
    let before = p.with_world(|w| w.write_submits());
    let _t = pages.write(pages.pos(0).unwrap(), &data).unwrap();
    assert_eq!(
        p.with_world(|w| w.write_submits()) - before,
        4,
        "the control"
    );
    let (got, pieces) = under_hook(&p, &s, PUMP, || pages.write(pages.pos(0).unwrap(), &data));
    assert!(matches!(got, Err(Error::Poisoned { .. })), "{got:?}");
    assert_eq!(pieces, 1, "a page piece was written after the poison");

    // A 4 MiB append: stopped likewise, its range a hole; what follows on
    // the store is refused at once, never waits on the hole.
    let (p, s) = store(2);
    let log = s.provision_append_region("log", 16 << 20).unwrap();
    let (got, pieces) = under_hook(&p, &s, PUMP, || log.append(&data));
    assert!(matches!(got, Err(Error::Poisoned { .. })), "{got:?}");
    assert_eq!(pieces, 1, "an append piece was written after the poison");
    let (tx, rx) = std::sync::mpsc::channel();
    let log2 = log.clone();
    let _waiter = std::thread::spawn(move || tx.send(log2.append_durable(b"after").map(|_| ())));
    let later = rx.recv_timeout(std::time::Duration::from_secs(10));
    assert!(
        matches!(later, Ok(Err(Error::Poisoned { .. }))),
        "a later durable append: {later:?}"
    );

    // An 8 MiB region's provisioning fill: stopped likewise.
    let (p, s) = store(3);
    let (got, pieces) = under_hook(&p, &s, FILL, || {
        s.provision_page_region("big", 8 << 20).map(|_| ())
    });
    assert!(matches!(got, Err(Error::Poisoned { .. })), "{got:?}");
    assert_eq!(pieces, 1, "a fill piece was written after the poison");
}
