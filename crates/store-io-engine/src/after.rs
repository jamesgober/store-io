//! Dependent writes (R5.3, API-09's `write_after`): a write that names the
//! receipts of the bytes it points at, submitted only with them in hand.
//!
//! A receipt exists only for durable bytes, so holding one is the whole
//! durability guarantee. Two things are checked besides, here and nowhere
//! else:
//!
//! - **a dependency on an append region must be a prefix receipt**
//!   (`NotPrefix`): a receipt for one append's range says nothing of the
//!   bytes before it, which a pointer past them may still need. Receipts
//!   carry the region's kind, so this holds whichever store they are from;
//! - **a dependency's region generation must still be live**
//!   (`StaleGeneration`; a released region `NotReady`): a recycle restarts
//!   the region at 0, and a pointer to bytes a recycle overwrites is the
//!   crash state API-09 forbids. A receipt from an earlier open of the store
//!   is refused once any open since resumed the region below its end (that
//!   open's appends overwrite from there): every open of this process is
//!   recorded (`frontier::record_resume`), and an open this process did not
//!   see (another process's: the file's metadata sequence is not where this
//!   process left it, `frontier::note_open`) refuses every append-region
//!   receipt of an earlier open. A receipt is matched to an earlier open of
//!   a store only if that open was of the same file (a container copied by
//!   another tool has the same volume id). A slot's receipt carries its
//!   commit count, not a region generation: slots are never recycled, so
//!   only their release is checked.
//!
//! The generation is held, not just read: a non-waiting gate pass on each
//! dependency's region, taken once the dependent write holds its lane (a
//! `Submitter`'s takes none: once its own checks pass) and before its
//! reservation, and dropped once the space is reserved (a page write, which
//! reserves nothing: just before its issue). Taken before the lane, a
//! recycle on one device could wait on a lane held by a write hung on
//! another; taken after the reservation, a refusal would leave a reserved
//! hole, which poisons the store; held through the write, a recycle on one
//! device would wait on an I/O on another (MDEV-02: no cross-device lock).
//! So the guarantee is "the dependency's generation was live when the
//! dependent write was reserved": a recycle after that may overwrite the
//! dependency before the dependent write lands, which is the caller's
//! reclaim decision (store-io never reclaims caller data on its own).
//! Nothing is checked after the write: a check there could only report
//! `NotWritten` for bytes already on the device.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use store_io_core::error::{ByteRange, Error, ErrorContext, NotWrittenCause};
use store_io_core::observe::Counter;
use store_io_format::meta::RegionKind;
use store_io_platform::{IoBuf, Platform};

use crate::completion::{Rejected, WriteMode};
use crate::frontier::{self, Earlier};
use crate::gate::OwnedPass;
use crate::io::not_written;
use crate::lifecycle::retired_error;
use crate::receipt::{DurableReceipt, RegionPos, WriteTicket};
use crate::region::{AppendRegion, PageRegion};
use crate::store::{LiveRegion, Store};
use crate::submitter::Submitter;
use crate::test_hook::{self, Step};

/// The dependencies of one write, checked: the regions to hold, in the
/// order of the stores that can be reached.
pub(crate) struct Checked {
    regions: Vec<Arc<LiveRegion>>,
}

/// Gate passes on a dependent write's dependency regions, held from the
/// check through the reservation.
pub(crate) struct Passes {
    _held: Vec<OwnedPass<LiveRegion>>,
}

fn dep_ctx(d: &DurableReceipt) -> ErrorContext {
    ErrorContext {
        region: Some(d.region),
        range: Some(ByteRange {
            start: d.start,
            end: d.through,
        }),
    }
}

/// Checks `deps` for a write that can reach `reach` (the written region's
/// store; a set's stores). Each receipt is matched to a reachable store by
/// its volume and open, else by its volume alone (a receipt is `Copy` and
/// outlives a close and reopen; its generation is on disk, so a recycle
/// since is still seen); a receipt of no reachable store is checked only
/// for being a prefix where it must be.
///
/// # Errors
///
/// `NotWritten`, with the dependency's region and range (a refused pass,
/// [`Checked::take_passes`], names the region only): `NotPrefix`,
/// `StaleGeneration` (recycled, or an earlier open's receipt past where an
/// open since resumed its region), `NotReady` (released), `NotPositioned`
/// (an earlier open's receipt on an append region not yet resumed).
pub(crate) fn check_deps<P: Platform>(
    deps: &[DurableReceipt],
    reach: &[&Store<P>],
) -> Result<Checked, Error> {
    let mut matched = Vec::with_capacity(deps.len());
    for d in deps {
        let c = dep_ctx(d);
        if d.append && !d.prefix {
            return Err(not_written(NotWrittenCause::NotPrefix, c));
        }
        let mut vouched = true;
        let at = match reach
            .iter()
            .position(|s| s.inner.volume == d.volume && s.inner.epoch == d.epoch)
        {
            Some(i) => Some(i),
            // An earlier open of a reachable store, if it was this file's.
            None => match reach.iter().position(|s| s.inner.volume == d.volume) {
                Some(i) => match frontier::earlier_open(reach[i].inner.epoch, d.epoch) {
                    Earlier::Same { vouched: v } => {
                        vouched = v;
                        Some(i)
                    }
                    Earlier::Other => None,
                    Earlier::Unknown => {
                        return Err(not_written(NotWrittenCause::StaleGeneration, c));
                    }
                },
                None => None,
            },
        };
        let Some(i) = at else {
            continue;
        };
        let store = &reach[i].inner;
        let region = store
            .regions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|r| r.id == d.region)
            .map(Arc::clone);
        let Some(region) = region else {
            return Err(not_written(NotWrittenCause::NotReady, c));
        };
        // A slot's receipt carries its commit count, not the region's
        // generation (slots are never recycled; each commit replaces the
        // object, as a page write does).
        if region.kind != RegionKind::Slot && region.generation != d.generation {
            return Err(not_written(NotWrittenCause::StaleGeneration, c));
        }
        // An earlier open's receipt of an append region: refused until this
        // open resumes the region, and if any open since resumed it below
        // the receipt's end (its appends overwrite from there). Region ids
        // are never reused, so a region positioned in this open without a
        // resume (provisioned or recycled) never matches one.
        if d.epoch != store.epoch && region.frontier.is_some() {
            if !region.positioned.load(Ordering::Acquire) {
                return Err(not_written(NotWrittenCause::NotPositioned, c));
            }
            if !vouched {
                return Err(not_written(NotWrittenCause::StaleGeneration, c));
            }
            if frontier::resumed_below((d.volume, d.region, d.generation), d.epoch, d.through) {
                return Err(not_written(NotWrittenCause::StaleGeneration, c));
            }
        }
        matched.push((i, region));
    }
    // Passes are taken in set order (stable within a store).
    matched.sort_by_key(|(i, _)| *i);
    Ok(Checked {
        regions: matched.into_iter().map(|(_, r)| r).collect(),
    })
}

impl Checked {
    /// A pass on every matched region, never waiting. The regions are the
    /// ones whose generation matched, never looked up again (a retry would
    /// otherwise find a new generation's open gate).
    ///
    /// # Errors
    ///
    /// `NotWritten(StaleGeneration)` if one is being recycled,
    /// `NotWritten(NotReady)` if released; the passes taken are dropped.
    pub(crate) fn take_passes(&self) -> Result<Passes, Error> {
        test_hook::step(Step::BeforePasses);
        let held = self
            .regions
            .iter()
            .map(|r| OwnedPass::enter(r).map_err(|why| retired_error(why, r)))
            .collect::<Result<Vec<_>, Error>>()?;
        test_hook::step(Step::BeforeReserve);
        Ok(Passes { _held: held })
    }
}

/// Drops a dependent append's passes once its space is reserved.
fn reserved(passes: Passes) {
    test_hook::step(Step::Reserved);
    drop(passes);
}

/// A page write's passes, about to be dropped before its issue.
fn to_issue(passes: Passes) -> Passes {
    test_hook::step(Step::Reserved);
    passes
}

fn count<P: Platform>(s: &Store<P>, deps: &[DurableReceipt]) {
    s.inner
        .observer
        .add(Counter::Dependencies, deps.len() as u64);
}

impl<P: Platform> AppendRegion<P>
where
    P::Queue: Send,
{
    /// Appends `data` as [`Self::append`] does, for a record that points at
    /// bytes other writes made durable: `deps` are their receipts. Holding
    /// them is the durability guarantee; besides, a receipt of an append
    /// region must be a prefix receipt ([`DurableReceipt::is_prefix`]: from
    /// `sync_through` or `append_durable`, not a receipt for one write's
    /// range), and a receipt of this store must name a live generation of
    /// its region (one recycled since is refused, and so is a receipt of an
    /// earlier open once an open since, this one included, resumed its
    /// region with [`Self::resume_at`] below the receipt's end: that open's
    /// appends may overwrite the bytes). An open of the store by another
    /// process in between is not seen: a receipt kept across one is the
    /// caller's to retire.
    ///
    /// Each dependency's region on this store is held against recycle and
    /// release from when this append holds its lane until its space is
    /// reserved, never longer (the check before is made again then, by the
    /// hold itself): a recycle after that may overwrite a dependency's
    /// bytes before this append lands, which is the caller's reclaim
    /// decision. A receipt of another store is checked only for being a
    /// prefix where it must be.
    ///
    /// # Errors
    ///
    /// As [`Self::append`]; before anything is reserved, `NotWritten` with
    /// the dependency's region (and, when the check refuses it, its range):
    /// `NotPrefix`, `StaleGeneration` (also for a receipt of an open this
    /// process has no record of),
    /// `NotReady` (its region released) or `NotPositioned` (an earlier
    /// open's receipt on a region not yet resumed).
    pub fn append_after(&self, deps: &[DurableReceipt], data: &[u8]) -> Result<WriteTicket, Error> {
        let checked = check_deps(deps, &[self.store()])?;
        test_hook::step(Step::BeforeLane);
        let ticket = self.append_with(data, || checked.take_passes(), reserved)?;
        count(self.store(), deps);
        Ok(ticket)
    }
}

impl<P: Platform> PageRegion<P>
where
    P::Queue: Send,
{
    /// Writes `data` at `pos` as [`Self::write`] does, for pages that point
    /// at bytes other writes made durable: `deps` are their receipts,
    /// checked as [`AppendRegion::append_after`] checks them. Each
    /// dependency's region on this store is held from when the write holds
    /// its lane until just
    /// before the write is issued, never during it.
    ///
    /// # Errors
    ///
    /// As [`Self::write`], and before anything is written the dependency
    /// refusals of [`AppendRegion::append_after`].
    pub fn write_after(
        &self,
        deps: &[DurableReceipt],
        pos: RegionPos,
        data: &[u8],
    ) -> Result<WriteTicket, Error> {
        let checked = check_deps(deps, &[self.store()])?;
        test_hook::step(Step::BeforeLane);
        let ticket = self.write_with(pos, data, || checked.take_passes().map(to_issue))?;
        count(self.store(), deps);
        Ok(ticket)
    }
}

impl<P: Platform> Submitter<P>
where
    P::Queue: Send,
{
    /// [`Self::append`] for a record that points at durable bytes: `deps`
    /// are their receipts, checked as [`AppendRegion::append_after`] checks
    /// them, the regions held from after the append's own checks through
    /// the reservation. Never
    /// waits: a full append window is `Rejected` (`TooManyInFlight`) with
    /// the buffer, as for [`Self::append`].
    ///
    /// # Errors
    ///
    /// As [`Self::append`], and the dependency refusals of
    /// [`AppendRegion::append_after`], with the buffer and nothing reserved.
    pub fn append_after(
        &mut self,
        region: &AppendRegion<P>,
        deps: &[DurableReceipt],
        buf: IoBuf,
        mode: WriteMode,
        tag: u64,
    ) -> Result<RegionPos, Rejected> {
        let checked = match check_deps(deps, &[region.store()]) {
            Ok(checked) => checked,
            Err(error) => {
                return Err(Rejected {
                    error,
                    buf: Some(buf),
                });
            }
        };
        test_hook::step(Step::BeforeLane);
        let pos = self.append_with(region, buf, mode, tag, || checked.take_passes(), reserved)?;
        count(region.store(), deps);
        Ok(pos)
    }

    /// [`Self::write`] for pages that point at durable bytes: `deps` are
    /// their receipts, checked as [`AppendRegion::append_after`] checks
    /// them, the regions held from after the write's own checks until just
    /// before the issue.
    ///
    /// # Errors
    ///
    /// As [`Self::write`], and the dependency refusals of
    /// [`AppendRegion::append_after`], with the whole buffer.
    pub fn write_after(
        &mut self,
        region: &PageRegion<P>,
        deps: &[DurableReceipt],
        pos: RegionPos,
        buf: IoBuf,
        mode: WriteMode,
        tag: u64,
    ) -> Result<(), Rejected> {
        let checked = match check_deps(deps, &[region.store()]) {
            Ok(checked) => checked,
            Err(error) => {
                return Err(Rejected {
                    error,
                    buf: Some(buf),
                });
            }
        };
        test_hook::step(Step::BeforeLane);
        self.write_with(region, pos, buf, mode, tag, || {
            checked.take_passes().map(to_issue)
        })?;
        count(region.store(), deps);
        Ok(())
    }
}
