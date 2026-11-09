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

use std::cell::Cell;
use std::sync::Arc;

use store_io_core::error::{Error, ErrorContext, NotWrittenCause};
use store_io_platform::Clock;

use crate::qos::{Charge, Issuer, Qos};

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

    /// Admits one piece now, or refuses it: a scan's read, whose charge
    /// rides with it until its completion is collected. Refused, the scan
    /// reaps its own reads first ([`Self::wait_charge`] only with none in
    /// flight).
    pub(crate) fn try_charge(&self) -> Option<Charge> {
        self.issuer.admit_leaving((self.clock)(), self.leave)
    }

    /// Waits for one piece's admission, on a thread with none of the
    /// call's pieces in flight (R5.4's progress rule): it is admitted once
    /// a piece of the call on another thread completes. `None` if the wait
    /// ended without an admission (the record's end or poison wake).
    pub(crate) fn wait_charge(&self) -> Option<Charge> {
        self.issuer.wait_admit(&*self.clock, self.leave).ok()
    }

    /// Admits one piece for a thread holding none of the call's pieces: at
    /// once if pacing admits it (always, with nothing of the call in
    /// flight), else once a piece of the call completes on another thread.
    /// The piece is charged until the returned [`Piece`] drops (after its
    /// I/O completed, or with it abandoned). A debug build checks that this
    /// thread holds no other piece: one would wait on itself.
    ///
    /// # Errors
    ///
    /// `NotWritten(Interrupted)` if the wait ended without an admission
    /// (the record's end or poison wake). No caller can meet it yet: a wait
    /// ends once a piece of the call on another thread completes, and every
    /// piece in flight does (a copy's reader drains its reads before it
    /// blocks; a caller's scan, whose visitor may block holding reads, is a
    /// call of its own, which no other thread waits on).
    pub(crate) fn admit(&self) -> Result<Piece, Error> {
        debug_assert_eq!(
            HELD.with(Cell::get),
            0,
            "a background piece admitted while this thread holds one: it could wait on itself"
        );
        let charge = match self.try_charge() {
            Some(c) => c,
            None => self.wait_charge().ok_or(Error::NotWritten {
                cause: NotWrittenCause::Interrupted,
                ctx: ErrorContext::default(),
            })?,
        };
        HELD.with(|h| h.set(h.get() + 1));
        Ok(Piece { _charge: charge })
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
}

thread_local! {
    /// Blocking pieces this thread holds ([`Pace::admit`]'s check).
    static HELD: Cell<u32> = const { Cell::new(0) };
}

/// A blocking path's admitted piece: its charge, released on drop.
pub(crate) struct Piece {
    _charge: Charge,
}

impl Drop for Piece {
    fn drop(&mut self) {
        HELD.with(|h| h.set(h.get().saturating_sub(1)));
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    #![allow(clippy::unwrap_used, unused_results)]

    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use store_io_core::IoClass;
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
