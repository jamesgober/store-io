//! An export's call, bound to its thread (R5.4, R5.6): the reference sinks
//! made by the caller before the call find it when they begin, join its
//! record for their device group, and report to it, so the export knows
//! whether every part of it finished, whatever a caller's wrapper around a
//! sink returned.

use std::cell::{Cell, RefCell};
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::Arc;

use store_io_core::error::Error;
use store_io_platform::Clock;

use crate::pace::Pace;
use crate::qos::Qos;

/// A part's failure, where the export was when it happened.
#[derive(Clone)]
pub(crate) struct Failure {
    pub(crate) error: Error,
    /// The export's region index then: the region being exported
    /// (`inside`), or the next one to start (between regions, at `begin`
    /// or, past the last, at `finish`).
    pub(crate) region: u32,
    pub(crate) inside: bool,
    /// Inside a region: the offset in its stream the failure is at (a
    /// failed write's start, a refused call's bytes accepted so far).
    pub(crate) offset: u64,
}

/// One call's state.
pub(crate) struct CallState {
    /// The source store's device group and the call's record on it.
    source: Arc<Qos>,
    source_pace: Option<Pace>,
    /// The call's records on other groups, made when a part first joins.
    others: RefCell<Vec<(Arc<Qos>, Pace)>>,
    /// A part joined the source's record (the stream then leaves it a
    /// piece: `Pace::second`).
    shares_source: Cell<bool>,
    /// Reference sinks bound to the call, and those that finished.
    sinks: Cell<u32>,
    finished: Cell<u32>,
    /// The first failure a part recorded (sticky).
    failure: RefCell<Option<Failure>>,
    /// Where the export is: its region index, whether inside it, and the
    /// bytes it had exported when each region began.
    region: Cell<u32>,
    inside: Cell<bool>,
    bases: RefCell<Vec<u64>>,
}

impl CallState {
    pub(crate) fn new(source: Arc<Qos>, source_pace: Option<Pace>) -> Rc<Self> {
        Rc::new(Self {
            source,
            source_pace,
            others: RefCell::new(Vec::new()),
            shares_source: Cell::new(false),
            sinks: Cell::new(0),
            finished: Cell::new(0),
            failure: RefCell::new(None),
            region: Cell::new(0),
            inside: Cell::new(false),
            bases: RefCell::new(Vec::new()),
        })
    }

    /// The call's record on `qos`'s group: the source's when it is the
    /// source's group (the part then shares it), else one made for that
    /// group the first time a part joins it.
    pub(crate) fn pace_for(&self, qos: &Arc<Qos>, clock: &Clock) -> Option<Pace> {
        if Qos::same_group(&self.source, qos) {
            self.shares_source.set(true);
            return self.source_pace.clone();
        }
        let mut others = self.others.borrow_mut();
        if let Some((_, p)) = others.iter().find(|(q, _)| Qos::same_group(q, qos)) {
            return Some(p.clone());
        }
        let p = Pace::new(qos, Arc::clone(clock));
        others.push((Arc::clone(qos), p.clone()));
        Some(p)
    }

    /// Whether a part joined the source's record.
    pub(crate) fn shares_source(&self) -> bool {
        self.shares_source.get()
    }

    /// A reference sink bound itself to the call.
    pub(crate) fn sink_bound(&self) {
        self.sinks.set(self.sinks.get().saturating_add(1));
    }

    /// A bound reference sink finished.
    pub(crate) fn sink_finished(&self) {
        self.finished.set(self.finished.get().saturating_add(1));
    }

    /// Whether every bound reference sink finished.
    pub(crate) fn all_finished(&self) -> bool {
        self.finished.get() >= self.sinks.get()
    }

    /// The export begins region `index`, having exported `bytes`.
    pub(crate) fn enter(&self, index: u32, bytes: u64) {
        self.region.set(index);
        self.inside.set(true);
        let mut b = self.bases.borrow_mut();
        b.truncate(index as usize);
        b.push(bytes);
    }

    /// The export ended a region; `next` is the next one's index.
    pub(crate) fn leave(&self, next: u32) {
        self.region.set(next);
        self.inside.set(false);
    }

    /// The bytes the export had exported when region `index` began.
    pub(crate) fn base(&self, index: u32) -> Option<u64> {
        self.bases.borrow().get(index as usize).copied()
    }

    /// A part failed at `offset` of the export's current region (the first
    /// failure is kept: later refusals are its consequences).
    pub(crate) fn part_failed(&self, error: Error, offset: u64) {
        let mut f = self.failure.borrow_mut();
        if f.is_none() {
            let inside = self.inside.get();
            *f = Some(Failure {
                error,
                region: self.region.get(),
                inside,
                offset: if inside { offset } else { 0 },
            });
        }
    }

    /// The first failure a part recorded.
    pub(crate) fn failure(&self) -> Option<Failure> {
        self.failure.borrow().clone()
    }
}

thread_local! {
    /// The calls in progress on this thread, innermost last.
    static CALLS: RefCell<Vec<Rc<CallState>>> = const { RefCell::new(Vec::new()) };
}

/// A call's binding to this thread, until it drops.
pub(crate) struct Bound {
    _thread: PhantomData<*const ()>,
}

/// Binds `call` to this thread (innermost) until the guard drops.
pub(crate) fn bind(call: &Rc<CallState>) -> Bound {
    let _b = CALLS.try_with(|c| c.borrow_mut().push(Rc::clone(call)));
    Bound {
        _thread: PhantomData,
    }
}

impl Drop for Bound {
    fn drop(&mut self) {
        let _b = CALLS.try_with(|c| c.borrow_mut().pop());
    }
}

/// The innermost call bound to this thread.
pub(crate) fn current() -> Option<Rc<CallState>> {
    CALLS
        .try_with(|c| c.borrow().last().cloned())
        .ok()
        .flatten()
}
