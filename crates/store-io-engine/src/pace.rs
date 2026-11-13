//! A blocking background call's pacing (PLANNING R5.4).
//!
//! A store created at [`IoClass::Background`](store_io_core::IoClass)
//! (`copy_store`'s target) belongs to the call that created it: every write
//! of its metadata and of its regions' data is a piece of that call, at
//! most `background_io` bytes, admitted through the call's issuer record
//! before it is submitted and charged until it completes. A blocking path
//! submits one piece at a time and holds none between them, so a refused
//! piece waits on the record (R5.4's progress rule: only a thread holding
//! none of the call's pieces waits); it never parks.
//!
//! A background scan (`region.scan`, a copy's reader) keeps several reads
//! in flight: each is admitted before it is submitted ([`Pace::try_charge`])
//! and its charge released the moment its completion is collected. A read
//! refused with reads of its own in flight reaps them first; with none, it
//! waits ([`Pace::wait_charge`]) for a piece of the call on another thread
//! (a copy's fill, which shares the reader's record when both stores are in
//! one device group).
//!
//! **Several parts on one thread.** A part that keeps pieces in flight
//! between its calls (a read stream) registers as a collector of its record
//! on its thread ([`Pace::register`]). Each admitted piece is counted for
//! its record on the thread holding it ([`Held`]). A refused piece is never
//! waited for while this thread holds pieces of the call: the record's
//! collectors run first, outside the record's lock (while unpaced one
//! completion at a time, until a piece would be admitted; refused again,
//! every piece), and the wait is entered only with this thread's count for
//! the record at zero (otherwise `Interrupted`, never a wait on itself).

use std::cell::{Cell, RefCell};
use std::marker::PhantomData;
use std::rc::{Rc, Weak};
use std::sync::Arc;

use store_io_core::error::{Error, ErrorContext, NotWrittenCause};
use store_io_platform::Clock;

use crate::qos::{Charge, Collect, Issuer, Qos};

/// A created store's device group, joined before its metadata is written
/// (so a background call's metadata goes out as paced pieces from its
/// first), and the creating call's pacing if it was created at
/// `Background`.
pub(crate) struct Joined {
    pub(crate) qos: Arc<Qos>,
    pub(crate) pace: Option<Pace>,
}

/// One background call's pacing: its issuer record, the clock admission
/// reads, and the piece its I/O is split at. A clone is the same call (one
/// record: a copy's reader and fill on one device group).
#[derive(Clone)]
pub(crate) struct Pace {
    issuer: Arc<Issuer>,
    clock: Clock,
    piece: usize,
    /// The call's unpaced pieces this part leaves to its others
    /// ([`Pace::second`]).
    leave: u64,
}

impl Pace {
    /// A new call on `qos`'s group, its pieces at the group member's
    /// `background_io`.
    pub(crate) fn new(qos: &Arc<Qos>, clock: Clock) -> Self {
        Self {
            issuer: Issuer::new(qos),
            clock,
            piece: qos.background_io() as usize,
            leave: 0,
        }
    }

    /// The same call, for a second thread of it that reads ahead (a copy's
    /// reader beside its fill): while commits are quiet it leaves one of
    /// the call's unpaced pieces to the first, so its read-ahead never
    /// refuses the other's piece.
    pub(crate) fn second(&self) -> Self {
        Self {
            leave: 1,
            ..self.clone()
        }
    }

    /// The piece this call's I/O is split at (a multiple of the store's
    /// block, at least one).
    pub(crate) fn piece(&self) -> usize {
        self.piece
    }

    /// The length of the next piece of a call that keeps one piece in
    /// flight at a time (a fill, D-156): its piece while the group paces;
    /// while commits are quiet as many of its pieces as the call may then
    /// keep in flight (`background_unpaced`), so the device holds no more
    /// of it than it would hold of a deeper call, at most `max` (`max` is
    /// a multiple of the piece; the result is never under one piece).
    pub(crate) fn lone_piece(&self, max: usize) -> usize {
        let (paced, cap) = self.issuer.pacing((self.clock)());
        if paced {
            return self.piece;
        }
        let quiet = self
            .piece
            .saturating_mul(usize::try_from(cap).unwrap_or(usize::MAX));
        (quiet.min(max) / self.piece).max(1) * self.piece
    }

    /// The record's key among this thread's counts and collectors (the
    /// record outlives every entry under it: a count's pieces each hold it
    /// through their charge, a collector's registration holds it).
    fn key(&self) -> usize {
        Arc::as_ptr(&self.issuer).cast::<()>() as usize
    }

    /// A charge, counted for the record on this thread.
    fn held(&self, charge: Charge) -> Held {
        let key = self.key();
        let _c = HELD.try_with(|h| {
            let mut h = h.borrow_mut();
            match h.iter_mut().find(|(k, _)| *k == key) {
                Some((_, n)) => *n += 1,
                None => h.push((key, 1)),
            }
        });
        Held {
            _charge: charge,
            key,
            _thread: PhantomData,
        }
    }

    /// Admits one piece now, or refuses it: a scan's read, whose charge
    /// rides with it until its completion is collected. Refused, the scan
    /// reaps its own reads first ([`Self::wait_charge`] only with none in
    /// flight).
    pub(crate) fn try_charge(&self) -> Option<Held> {
        self.issuer
            .admit_leaving((self.clock)(), self.leave)
            .map(|c| self.held(c))
    }

    /// Waits for one piece's admission, on a thread whose own pieces of the
    /// call are in flight only in its registered collectors (R5.4's
    /// progress rule, [`Self::obtain`]). `None` if the wait ended without
    /// an admission (the record's end or poison wake, a failed collection,
    /// or pieces of the call this thread holds that no collector can
    /// collect).
    pub(crate) fn wait_charge(&self) -> Option<Held> {
        self.obtain().ok()
    }

    /// Admits one piece for a blocking path: at once if pacing admits it,
    /// else once the record's collectors on this thread (other parts of
    /// the call) or a piece of the call on another thread released one
    /// ([`Self::obtain`]). The piece is charged until the returned [`Held`]
    /// drops (after its I/O completed, or with it abandoned).
    ///
    /// # Errors
    ///
    /// `NotWritten(Interrupted)` if no admission came: the record's end or
    /// poison wake, a failed collection (the part's own error is its to
    /// report), or pieces of the call this thread holds that no collector
    /// can collect (a wait would be on itself).
    pub(crate) fn admit(&self) -> Result<Held, Error> {
        self.obtain().map_err(|()| Error::NotWritten {
            cause: NotWrittenCause::Interrupted,
            ctx: ErrorContext::default(),
        })
    }

    /// One piece's admission, never waiting while this thread holds pieces
    /// of the call: refused, the record's collectors on this thread run
    /// first, outside the record's lock (`Issuer::wait_admit` holds it with
    /// its waiter counted: a charge released there would lock it again).
    /// While the group is unpaced they collect one completion at a time
    /// until a piece would be admitted (or none has any), and admission is
    /// tried again; paced, or refused again (the group turned paced
    /// meanwhile), every piece is collected. The wait is entered only with
    /// this thread's count for the record at zero. A failed collection ends
    /// it at once, before any retry.
    fn obtain(&self) -> Result<Held, ()> {
        if let Some(h) = self.try_charge() {
            return Ok(h);
        }
        let parts = parts_of(self.key());
        if !parts.is_empty() {
            if !self.issuer.pacing((self.clock)()).0 {
                'one: loop {
                    let mut any = false;
                    for p in &parts {
                        any |= p.collect_one().map_err(|_| ())?;
                        if self.issuer.would_admit((self.clock)(), self.leave) {
                            break 'one;
                        }
                    }
                    if !any {
                        break;
                    }
                }
                crate::test_hook::step(crate::test_hook::Step::CollectRetry);
                if let Some(h) = self.try_charge() {
                    return Ok(h);
                }
            }
            for p in &parts {
                p.collect_all().map_err(|_| ())?;
            }
        }
        if held_of(self.key()) != 0 {
            return Err(());
        }
        self.issuer
            .wait_admit(&*self.clock, self.leave)
            .map(|c| self.held(c))
            .map_err(|_| ())
    }

    /// Registers `part` as a collector of this call's record on this
    /// thread, until the returned guard drops (held weakly: a part dropped
    /// first is skipped).
    pub(crate) fn register(&self, part: Weak<dyn Collect>) -> Registered {
        let id = NEXT_PART
            .try_with(|n| {
                let id = n.get();
                n.set(id + 1);
                id
            })
            .unwrap_or(u64::MAX);
        let key = self.key();
        let _c = PARTS.try_with(|p| p.borrow_mut().push((key, id, part)));
        Registered {
            id,
            _issuer: Arc::clone(&self.issuer),
            _thread: PhantomData,
        }
    }

    /// Whether this part leaves a piece to the call's other part.
    #[cfg(all(test, not(loom)))]
    pub(crate) fn is_second(&self) -> bool {
        self.leave > 0
    }

    /// Whether `other` is this same call (one issuer record).
    #[cfg(all(test, not(loom)))]
    pub(crate) fn same_call(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.issuer, &other.issuer)
    }

    /// This thread's count of pieces held for the record.
    #[cfg(all(test, not(loom)))]
    pub(crate) fn held_here(&self) -> u32 {
        held_of(self.key())
    }
}

/// A record's collectors on this thread, in registration order (cloned
/// out, so none runs under the list's borrow).
fn parts_of(key: usize) -> Vec<Rc<dyn Collect>> {
    PARTS
        .try_with(|p| {
            p.borrow()
                .iter()
                .filter(|(k, _, _)| *k == key)
                .filter_map(|(_, _, w)| w.upgrade())
                .collect()
        })
        .unwrap_or_default()
}

/// This thread's count of pieces held for a record (unknown while the
/// thread's locals are being torn down: then never zero, so no wait).
fn held_of(key: usize) -> u32 {
    HELD.try_with(|h| {
        h.borrow()
            .iter()
            .find(|(k, _)| *k == key)
            .map_or(0, |&(_, n)| n)
    })
    .unwrap_or(u32::MAX)
}

/// A collector's registration: its record, its id and the part.
type PartEntry = (usize, u64, Weak<dyn Collect>);

thread_local! {
    /// Pieces this thread holds, per record ([`Pace::obtain`]'s check).
    static HELD: RefCell<Vec<(usize, u32)>> = const { RefCell::new(Vec::new()) };
    /// Collectors registered on this thread: their record, id and part.
    static PARTS: RefCell<Vec<PartEntry>> = const { RefCell::new(Vec::new()) };
    static NEXT_PART: Cell<u64> = const { Cell::new(0) };
}

/// An admitted piece's charge, counted for its record on the thread holding
/// it (not `Send`: the count is that thread's): released on drop.
pub(crate) struct Held {
    _charge: Charge,
    key: usize,
    _thread: PhantomData<*const ()>,
}

impl Drop for Held {
    fn drop(&mut self) {
        let key = self.key;
        let _c = HELD.try_with(|h| {
            let mut h = h.borrow_mut();
            if let Some(i) = h.iter().position(|(k, _)| *k == key) {
                h[i].1 = h[i].1.saturating_sub(1);
                if h[i].1 == 0 {
                    let _gone = h.swap_remove(i);
                }
            }
        });
    }
}

/// A part's registration as a collector: removed on drop. It holds the
/// record, so the record's key is never another's while it is listed.
pub(crate) struct Registered {
    id: u64,
    _issuer: Arc<Issuer>,
    _thread: PhantomData<*const ()>,
}

impl Drop for Registered {
    fn drop(&mut self) {
        let id = self.id;
        let _c = PARTS.try_with(|p| p.borrow_mut().retain(|(_, i, _)| *i != id));
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, unused_results)]

    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use store_io_core::IoClass;
    use store_io_core::error::{Error, NotWrittenCause};
    use store_io_format::slot::{FORMAT_1, FORMAT_2};
    use store_io_sim::{SimConfig, SimPlatform};

    use crate::store::container_name;
    use crate::{Store, StoreOptions};

    /// Runs `f` with `sample` taken at every submission; returns the
    /// samples and the writes submitted meanwhile.
    fn sampled(
        p: &SimPlatform,
        sample: impl Fn() -> u64 + Send + Sync + 'static,
        f: impl FnOnce(),
    ) -> (Vec<u64>, u64) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let out = Arc::clone(&seen);
        p.with_world(|w| {
            w.set_submit_probe(Some(Arc::new(move || {
                out.lock().unwrap().push(sample());
            })))
        });
        let before = p.with_world(|w| w.write_submits());
        f();
        p.with_world(|w| w.set_submit_probe(None));
        let writes = p.with_world(|w| w.write_submits()) - before;
        let seen = seen.lock().unwrap().clone();
        (seen, writes)
    }

    fn background(p: &SimPlatform, dir: &str, format: u16) -> Store<SimPlatform> {
        Store::create_named(
            p.clone(),
            Path::new(dir),
            StoreOptions {
                container_format: format,
                ..StoreOptions::default()
            },
            &container_name().unwrap(),
            IoClass::Background,
        )
        .unwrap()
    }

    /// Provisioning (a fill, its region header, the table) and a slot
    /// commit.
    fn work(s: &Store<SimPlatform>) {
        let _log = s.provision_append_region("log", 1 << 20).unwrap();
        let slot = s.provision_slot("obj").unwrap();
        let _receipt = slot.commit(b"object").unwrap();
    }

    /// Every write of a background call's store, from its creation's first
    /// metadata write on, is a piece charged to its device group when it
    /// is submitted (a flush is none), in either format.
    #[test]
    fn test_every_write_of_a_background_store_is_charged() {
        for (seed, format) in [(45, FORMAT_1), (46, FORMAT_2)] {
            let p = SimPlatform::new(SimConfig::volatile(seed));
            // A store of the same device, whose group the samples read.
            let w = Store::create(p.clone(), Path::new("/w"), StoreOptions::default()).unwrap();
            // The probe holds a clone: it is dropped under the world's lock,
            // where the store's last handle could not close.
            let w2 = w.clone();
            let (seen, writes) = sampled(
                &p,
                move || w2.qos().in_flight,
                || {
                    work(&background(&p, "/bg", format));
                },
            );
            let charged = seen.iter().filter(|&&n| n >= 1).count() as u64;
            assert_eq!(charged, writes, "format {format}: {seen:?}");
            assert!(writes > 0);
            drop(w);
        }
    }

    /// A caller's scan takes its buffers under the pool's background cap.
    #[test]
    fn test_a_scan_takes_capped_buffers() {
        let p = SimPlatform::new(SimConfig::volatile(52));
        let s = Store::create(p.clone(), Path::new("/s"), StoreOptions::default()).unwrap();
        let log = s.provision_append_region("log", 1 << 20).unwrap();
        let t = log.append(&[9u8; 1 << 20]).unwrap();
        let _receipt = log.sync_through(&t).unwrap();
        let s2 = s.clone();
        let holdings = move || {
            s2.inner
                .pool
                .background_holdings()
                .iter()
                .map(|c| u64::from(c.1))
                .sum()
        };
        let (held, writes) = sampled(&p, holdings, || {
            let _sum = log
                .scan(0, |_| std::ops::ControlFlow::Continue(()))
                .unwrap();
        });
        assert_eq!(writes, 0);
        assert!(!held.is_empty());
        assert!(held.iter().all(|&n| n >= 1), "{held:?}");
    }

    /// A clone of a call's pacing is the same call (one record), a new one
    /// another.
    #[test]
    fn test_a_clone_is_the_same_call() {
        let p = SimPlatform::new(SimConfig::volatile(53));
        let s = Store::create(p.clone(), Path::new("/s"), StoreOptions::default()).unwrap();
        let a = s.inner.domain.new_pace().unwrap();
        assert!(a.same_call(&a.clone()));
        assert!(!a.same_call(&s.inner.domain.new_pace().unwrap()));
    }

    /// A call's second part (a copy's reader) leaves one of the call's
    /// unpaced pieces to the first (its fill): it stops one short of the
    /// cap, and the first still gets its piece.
    #[test]
    fn test_a_second_part_leaves_the_first_a_piece() {
        let p = SimPlatform::new(SimConfig::volatile(57));
        let s = Store::create(p.clone(), Path::new("/s"), StoreOptions::default()).unwrap();
        p.with_world(|w| w.advance_clock_ns(10_000_000_000));
        assert!(!s.qos().active);
        let first = s.inner.domain.new_pace().unwrap();
        let second = first.second();
        let mut held: Vec<_> = (0..7).map(|_| second.try_charge().unwrap()).collect();
        assert!(second.try_charge().is_none(), "one left");
        held.push(first.try_charge().unwrap());
        assert!(first.try_charge().is_none(), "the cap");
        drop(held);
    }

    /// A fake part of a call: the charges it holds, how it collects them.
    #[derive(Default)]
    struct FakeState {
        held: Vec<crate::pace::Held>,
        fail: bool,
        ones: u32,
        alls: u32,
    }

    struct Fake(std::cell::RefCell<FakeState>);

    impl Fake {
        /// A part holding `n` of `pace`'s pieces, registered as its
        /// collector.
        fn holding(
            pace: &crate::pace::Pace,
            n: usize,
        ) -> (std::rc::Rc<Self>, crate::pace::Registered) {
            let held = (0..n).map(|_| pace.try_charge().unwrap()).collect();
            let f = std::rc::Rc::new(Fake(std::cell::RefCell::new(FakeState {
                held,
                ..FakeState::default()
            })));
            let weak: std::rc::Weak<Fake> = std::rc::Rc::downgrade(&f);
            let reg = pace.register(weak);
            (f, reg)
        }

        fn state(&self) -> std::cell::Ref<'_, FakeState> {
            self.0.borrow()
        }
    }

    impl crate::qos::Collect for Fake {
        fn collect_one(&self) -> Result<bool, store_io_core::error::OsError> {
            let Ok(mut s) = self.0.try_borrow_mut() else {
                return Ok(false);
            };
            s.ones += 1;
            if s.fail {
                return Err(EIO);
            }
            Ok(s.held.pop().is_some())
        }

        fn collect_all(&self) -> Result<(), store_io_core::error::OsError> {
            let Ok(mut s) = self.0.try_borrow_mut() else {
                return Ok(());
            };
            s.alls += 1;
            if s.fail {
                return Err(EIO);
            }
            s.held.clear();
            Ok(())
        }

        fn failed(&self) -> Option<(store_io_core::error::OsError, u64)> {
            None
        }
    }

    const EIO: store_io_core::error::OsError = store_io_core::error::OsError {
        code: 5,
        source: store_io_core::error::OsErrorSource::Errno,
    };

    /// Runs `f` on a thread of its own, failing if it has not returned in
    /// 20 s (a wait on itself would never return).
    fn within(f: impl FnOnce() + Send + 'static) {
        let (tx, rx) = std::sync::mpsc::channel();
        let _t = std::thread::spawn(move || {
            f();
            let _sent = tx.send(());
        });
        rx.recv_timeout(std::time::Duration::from_secs(20))
            .expect("the admission never returned (or panicked)");
    }

    /// A store whose device's commits are quiet, and a new call on it.
    fn quiet(seed: u64) -> (SimPlatform, Store<SimPlatform>, crate::pace::Pace) {
        let p = SimPlatform::new(SimConfig::volatile(seed));
        let s = Store::create(p.clone(), Path::new("/s"), StoreOptions::default()).unwrap();
        p.with_world(|w| w.advance_clock_ns(10_000_000_000));
        assert!(!s.qos().active);
        let pace = s.inner.domain.new_pace().unwrap();
        (p, s, pace)
    }

    /// Refused because another part on this thread holds the call's cap,
    /// a piece is admitted once that part's collector released one: one
    /// completion at a time while unpaced, stopping as soon as a piece
    /// would be admitted (the part keeps the rest).
    #[test]
    fn test_a_refused_piece_collects_another_part_one_at_a_time() {
        within(|| {
            let (_p, _s, pace) = quiet(60);
            let (fake, _reg) = Fake::holding(&pace, 8);
            assert_eq!(pace.held_here(), 8);
            assert!(pace.try_charge().is_none(), "the call's cap");
            let piece = pace.admit().unwrap();
            let st = fake.state();
            assert_eq!((st.ones, st.alls, st.held.len()), (1, 0, 7));
            assert_eq!(pace.held_here(), 8, "seven of the part's and the piece");
            drop(st);
            drop(piece);
        });
    }

    /// Paced, a refused piece collects every piece of the call's other
    /// parts on this thread, then is admitted (the call has none in
    /// flight).
    #[test]
    fn test_a_paced_refusal_collects_every_piece_of_the_call() {
        within(|| {
            let (_p, s, pace) = quiet(61);
            let (fake, _reg) = Fake::holding(&pace, 2);
            let _hold = s.commit_hold();
            assert!(s.qos().active);
            assert!(pace.try_charge().is_none(), "paced: the call holds two");
            let _piece = pace.admit().unwrap();
            let st = fake.state();
            assert_eq!((st.ones, st.alls, st.held.len()), (0, 1, 0));
        });
    }

    /// Commit holds a test's injected commit keeps.
    static HOLDS: Mutex<Vec<crate::CommitHold>> = Mutex::new(Vec::new());

    /// The group turns paced between the collection that would admit a
    /// piece and the retry: the retry is refused, every piece of the call
    /// on this thread is collected, and only then is the wait entered (it
    /// admits at once: the call has nothing in flight). Never a wait with
    /// the thread's own pieces in flight.
    #[test]
    fn test_a_commit_between_the_collection_and_the_retry_collects_the_rest() {
        within(|| {
            let (_p, s, pace) = quiet(62);
            let (fake, _reg) = Fake::holding(&pace, 8);
            let s2 = s.clone();
            crate::test_hook::lib_tests::at(
                crate::test_hook::Step::CollectRetry,
                Box::new(move || HOLDS.lock().unwrap().push(s2.commit_hold())),
            );
            let piece = pace.admit();
            crate::test_hook::lib_tests::clear();
            assert!(s.qos().active, "the commit came");
            let _piece = piece.unwrap();
            let st = fake.state();
            assert_eq!((st.ones, st.alls, st.held.len()), (1, 1, 0));
            HOLDS.lock().unwrap().clear();
        });
    }

    /// Pieces of the call this thread holds that no collector can collect
    /// (a part never registered, unregistered, or registered but borrowed:
    /// skipped): the admission is refused `Interrupted`, never a wait on
    /// itself.
    #[test]
    fn test_pieces_no_collector_holds_refuse_the_wait() {
        within(|| {
            let (_p, _s, pace) = quiet(67);
            let held: Vec<_> = (0..8).map(|_| pace.try_charge().unwrap()).collect();
            assert!(pace.admit().is_err(), "never registered");
            drop(held);
            let (fake, _reg) = Fake::holding(&pace, 8);
            {
                let _asking = fake.0.borrow_mut();
                assert!(pace.admit().is_err(), "registered, borrowed");
            }
            assert_eq!(fake.state().ones, 0, "borrowed: skipped");
        });
        within(|| {
            let (_p, _s, pace) = quiet(63);
            let (fake, reg) = Fake::holding(&pace, 8);
            drop(reg);
            let e = pace.admit().err().unwrap();
            assert!(matches!(
                e,
                Error::NotWritten {
                    cause: NotWrittenCause::Interrupted,
                    ..
                }
            ));
            assert!(pace.wait_charge().is_none());
            assert_eq!(fake.state().held.len(), 8);
            assert_eq!(fake.state().ones, 0, "unregistered: never asked");
        });
    }

    /// A failed collection ends the admission at once: no retry, no
    /// collection of the rest, no wait.
    #[test]
    fn test_a_failed_collection_ends_the_admission() {
        within(|| {
            let (_p, _s, pace) = quiet(64);
            let (fake, _reg) = Fake::holding(&pace, 8);
            fake.0.borrow_mut().fail = true;
            assert!(pace.admit().is_err());
            let st = fake.state();
            assert_eq!((st.ones, st.alls), (1, 0));
        });
    }

    /// A part that leaves a piece to the call's other part stops collecting
    /// only once a piece would be admitted leaving it (two collected of
    /// eight held: the cap less one).
    #[test]
    fn test_a_second_part_collects_until_it_would_be_admitted_leaving_one() {
        within(|| {
            let (_p, _s, pace) = quiet(68);
            let second = pace.second();
            let (fake, _reg) = Fake::holding(&pace, 8);
            let _piece = second.admit().unwrap();
            let st = fake.state();
            assert_eq!((st.ones, st.alls, st.held.len()), (2, 0, 6));
        });
    }

    /// Counts and collectors are per record: a call refused on this thread
    /// collects only its own parts, never another call's sharing the thread.
    #[test]
    fn test_counts_and_collectors_are_per_record() {
        within(|| {
            let (_p, s, a) = quiet(66);
            let b = s.inner.domain.new_pace().unwrap();
            let (fa, _ra) = Fake::holding(&a, 8);
            let (fb, _rb) = Fake::holding(&b, 8);
            assert_eq!((a.held_here(), b.held_here()), (8, 8));
            let _piece = b.admit().unwrap();
            assert_eq!(fa.state().ones, 0, "another call's part untouched");
            assert_eq!(fa.state().held.len(), 8);
            assert_eq!(fb.state().ones, 1);
            assert_eq!((a.held_here(), b.held_here()), (8, 8));
        });
    }

    /// `would_admit` answers as an admission would, taking no charge:
    /// unpaced up to the cap (less what the part leaves); paced only with
    /// nothing of the call in flight (the budget at its floor).
    #[test]
    fn test_would_admit_answers_as_an_admission() {
        let (_p, s, pace) = quiet(65);
        let second = pace.second();
        let now = (pace.clock)();
        let mut held = Vec::new();
        for n in 0..8 {
            assert!(pace.issuer.would_admit(now, 0), "{n} held");
            assert_eq!(second.issuer.would_admit(now, 1), n < 7, "{n} held");
            held.push(pace.try_charge().unwrap());
        }
        assert!(!pace.issuer.would_admit(now, 0), "the cap");
        assert!(pace.try_charge().is_none());
        assert_eq!(pace.issuer.in_flight(), 8, "no charge taken");
        held.truncate(1);
        let _hold = s.commit_hold();
        let now = (pace.clock)();
        assert!(!pace.issuer.would_admit(now, 0), "paced, one in flight");
        held.clear();
        assert!(pace.issuer.would_admit(now, 0), "paced, none in flight");
    }

    /// A background call's store takes every write's buffer under the
    /// pool's background cap: its fills and its metadata alike.
    #[test]
    fn test_a_background_store_s_writes_take_capped_buffers() {
        let p = SimPlatform::new(SimConfig::volatile(47));
        let s = background(&p, "/bg", FORMAT_2);
        let s2 = s.clone();
        let holdings = move || {
            s2.inner
                .pool
                .background_holdings()
                .iter()
                .map(|c| u64::from(c.1))
                .sum()
        };
        let (held, writes) = sampled(&p, holdings, || work(&s));
        // A write holds its buffer when it is submitted; a flush none.
        let capped = held.iter().filter(|&&n| n >= 1).count() as u64;
        assert_eq!(capped, writes, "{held:?}");
        assert!(writes > 0);
    }
}
