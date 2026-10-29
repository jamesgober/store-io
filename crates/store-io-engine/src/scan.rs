//! Raw ordered scans: the recovery read path.
//!
//! A scan reads a region from a block-aligned offset to the end of its data
//! area and hands every byte to the caller in order, as [`ScanItem::Data`].
//! It keeps several large reads in flight (direct I/O, never the page cache)
//! and delivers them strictly in order.
//!
//! A caller's visitor never runs while the scan holds a shared lane, which
//! every other call on the store may wait for: a visitor that calls the
//! store would wait on itself (one lane), or on a thread waiting for that
//! lane under the metadata lock. On a parallel lane (D-096), which no one
//! waits for, reads are delivered as they arrive; on a shared lane, the
//! scan collects every read in flight and puts the lane back before it
//! delivers them, and a bisection's items are held back until it has. The
//! store's own visitors (a read's pipeline and its bad-sector search),
//! which never call the store, are delivered as reads arrive on any lane.
//!
//! A scan never stops at bad data. A read the device cannot complete is
//! bisected down to single sectors (the device's logical block where the
//! probe knows it and direct I/O allows reads that small; else the store
//! block); every sector that still fails is reported as
//! [`ScanItem::Unreadable`] (adjacent ones merged), and the scan carries on
//! after it. Only the caller decides where valid data ends, by returning
//! [`ControlFlow::Break`] from its visitor.

use std::ops::ControlFlow;

use store_io_core::errno::is_media_error;
use store_io_core::error::{Error, ErrorContext, NotWrittenCause, Op, OsError};
use store_io_core::observe::Counter;
use store_io_platform::{IoBuf, IoOp, Platform, Queue, RawResult};

use crate::exec::{Lane, LaneGuard, Ran, run};
use crate::io::{ctx_range, not_written};
use crate::store::{LiveRegion, Store};

/// Reads kept in flight by a scan.
const DEPTH: usize = 8;

/// One step of a scan, in region order.
#[derive(Debug, PartialEq, Eq)]
pub enum ScanItem<'a> {
    /// Bytes read from the region, starting at region offset `offset`.
    Data {
        /// Region offset of `bytes[0]`.
        offset: u64,
        /// The bytes, exactly as the device returned them.
        bytes: &'a [u8],
    },
    /// The device could not read `[start, end)` (whole sectors: the
    /// device's logical blocks where known, else store blocks). The scan
    /// continues after it; the range is not "end of data".
    Unreadable {
        /// First unreadable byte.
        start: u64,
        /// One past the last unreadable byte.
        end: u64,
    },
}

/// How a scan ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScanSummary {
    /// One past the last byte delivered, as data or as unreadable.
    pub end: u64,
    /// Bytes reported unreadable.
    pub unreadable: u64,
    /// The visitor stopped the scan.
    pub stopped: bool,
}

/// Delivers items to the visitor in order, merging adjacent unreadable
/// ranges, and keeps the summary.
struct Emitter<F> {
    visit: F,
    bad: Option<(u64, u64)>,
    sum: ScanSummary,
    /// Items held back (copied) while a shared lane is held, replayed in
    /// order by [`Self::release`].
    held: Option<Vec<Held>>,
}

/// An item held back by [`Emitter::hold`].
enum Held {
    Data(u64, Vec<u8>),
    Unreadable(u64, u64),
}

impl<F: FnMut(ScanItem<'_>) -> ControlFlow<()>> Emitter<F> {
    /// Holds back every item until [`Self::release`].
    fn hold(&mut self) {
        let _held = self.held.get_or_insert_with(Vec::new);
    }

    /// Delivers the items held back, in order.
    fn release(&mut self) {
        for item in self.held.take().unwrap_or_default() {
            match item {
                Held::Data(offset, bytes) => self.data(offset, &bytes),
                Held::Unreadable(start, end) => self.unreadable(start, end),
            }
        }
    }

    fn flush_bad(&mut self) {
        if let Some((start, end)) = self.bad.take() {
            if (self.visit)(ScanItem::Unreadable { start, end }).is_break() {
                self.sum.stopped = true;
            }
        }
    }

    fn data(&mut self, offset: u64, bytes: &[u8]) {
        if let Some(held) = &mut self.held {
            held.push(Held::Data(offset, bytes.to_vec()));
            return;
        }
        if self.sum.stopped {
            return;
        }
        self.flush_bad();
        if self.sum.stopped || bytes.is_empty() {
            return;
        }
        self.sum.end = offset + bytes.len() as u64;
        if (self.visit)(ScanItem::Data { offset, bytes }).is_break() {
            self.sum.stopped = true;
        }
    }

    fn unreadable(&mut self, start: u64, end: u64) {
        if let Some(held) = &mut self.held {
            held.push(Held::Unreadable(start, end));
            return;
        }
        if self.sum.stopped || start >= end {
            return;
        }
        self.sum.unreadable += end - start;
        self.sum.end = end;
        match &mut self.bad {
            Some((_, e)) if *e == start => *e = end,
            _ => {
                self.flush_bad();
                self.bad = Some((start, end));
            }
        }
    }

    fn finish(mut self) -> ScanSummary {
        if !self.sum.stopped {
            self.flush_bad();
        }
        self.sum
    }
}

/// A read in flight, and its completion once it arrives.
struct Pending {
    start: u64,
    len: usize,
    done: Option<(RawResult<usize>, Option<IoBuf>)>,
}

fn read_error(raw: OsError, c: ErrorContext) -> Error {
    Error::Io {
        op: Op::Read,
        raw,
        ctx: c,
    }
}

impl<P: Platform> Store<P>
where
    P::Queue: Send,
{
    /// Scans `r` from `from` to `to` (clamped to the data area).
    /// A scan whose unreadable ranges are reported to the caller: each is
    /// counted once as a media error (internal rescans are not counted).
    pub(crate) fn scan_reported<F>(
        &self,
        r: &LiveRegion,
        from: u64,
        to: u64,
        mut visit: F,
    ) -> Result<ScanSummary, Error>
    where
        F: FnMut(ScanItem<'_>) -> ControlFlow<()>,
    {
        let obs = &self.inner.observer;
        self.scan_region(r, from, to, true, |item| {
            if matches!(item, ScanItem::Unreadable { .. }) {
                obs.add(Counter::MediaErrors, 1);
            }
            visit(item)
        })
    }

    /// Scans `r` from `from` to `to` (clamped to the data area). `calls_store`:
    /// the visitor may call the store (a caller's), so it never runs while a
    /// shared lane is held (the module's docs).
    pub(crate) fn scan_region<F>(
        &self,
        r: &LiveRegion,
        from: u64,
        to: u64,
        calls_store: bool,
        visit: F,
    ) -> Result<ScanSummary, Error>
    where
        F: FnMut(ScanItem<'_>) -> ControlFlow<()>,
    {
        let block = self.block();
        let to = to.min(r.map.len());
        let c = ctx_range(r, from, to);
        if from % block != 0 {
            return Err(not_written(NotWrittenCause::Misaligned, c));
        }
        if from > to {
            return Err(not_written(NotWrittenCause::OutOfBounds, c));
        }
        let mut out = Emitter {
            visit,
            bad: None,
            held: None,
            sum: ScanSummary {
                end: from,
                ..ScanSummary::default()
            },
        };
        if from == to {
            return Ok(out.finish());
        }
        let chunk = self.chunk();
        let mut free: Vec<IoBuf> = (0..DEPTH)
            .map_while(|_| self.inner.pool.take_within(chunk, chunk).ok())
            .collect();
        if free.is_empty() {
            return Err(not_written(NotWrittenCause::PoolExhausted, c));
        }
        let depth = free.len();
        let mut ring: Vec<Option<Pending>> = (0..depth).map(|_| None).collect();
        // Several reads in flight: a parallel lane (D-096). A range of one
        // buffer in one run, or a pipeline of one, is a single read: the
        // caller's lane.
        let several = to - from > chunk as u64 || r.map.run_end(from) < to;
        let take = || {
            if several && depth > 1 {
                self.par_lane(r, Op::Read)
            } else {
                self.lane(r, Op::Read)
            }
        };
        let first = take()?;
        // For a visitor that may call the store, on a shared lane every read
        // in flight is collected and the lane put back before anything is
        // delivered (the module's docs); each batch takes a lane again, a
        // parallel one if it can.
        let mut deferred = calls_store && first.is_shared();
        let mut lane: Option<LaneGuard<'_, P::Queue>> = Some(first);
        let file = &self.inner.file;
        let (mut submitted, mut delivered) = (0u64, 0u64);
        let mut next = from;
        let mut in_flight = 0usize;
        let mut failure: Option<Error> = None;
        loop {
            let progress = (submitted, delivered);
            // Keep the pipeline full.
            while failure.is_none() && !out.sum.stopped && next < to && !free.is_empty() {
                if lane.is_none() {
                    match take() {
                        Ok(l) => {
                            deferred = calls_store && l.is_shared();
                            lane = Some(l);
                        }
                        Err(e) => {
                            failure = Some(e);
                            break;
                        }
                    }
                }
                let Some(lane) = &mut lane else { break };
                let Some(mut buf) = free.pop() else { break };
                // Never across a run's end (one device operation each).
                let len = (to - next).min(chunk as u64).min(r.map.run_left(next)) as usize;
                // `len ≤ chunk ≤ capacity`.
                let _fits = buf.set_len(len);
                let op = IoOp::Read {
                    file,
                    offset: r.map.locate(next, len as u64),
                    buf,
                };
                match lane.submit(op, submitted) {
                    Ok(()) => {
                        ring[(submitted % depth as u64) as usize] = Some(Pending {
                            start: next,
                            len,
                            done: None,
                        });
                        submitted += 1;
                        next += len as u64;
                        in_flight += 1;
                    }
                    Err(rejected) => {
                        if let Some(b) = rejected.buf {
                            free.push(b);
                        }
                        match rejected.raw {
                            None if in_flight > 0 => break,
                            None => failure = Some(not_written(NotWrittenCause::QueueFull, c)),
                            Some(raw) => failure = Some(read_error(raw, c)),
                        }
                    }
                }
            }
            if deferred {
                if let Some(l) = &mut lane {
                    if let Err(e) = collect(l, &mut ring, depth, &mut in_flight) {
                        // As below: the reads in flight keep their buffers.
                        return Err(read_error(e, c));
                    }
                }
                lane = None;
            }
            // Deliver everything that has arrived, in order.
            while delivered < submitted {
                let idx = (delivered % depth as u64) as usize;
                if ring[idx].as_ref().is_none_or(|p| p.done.is_none()) {
                    break;
                }
                let Some(Pending {
                    start,
                    len,
                    done: Some((result, buf)),
                }) = ring[idx].take()
                else {
                    break;
                };
                delivered += 1;
                let mut buf = buf;
                debug_assert!(
                    buf.is_some() || result.is_err(),
                    "a read succeeded without its buffer (platform contract 1)"
                );
                // Counted as every read is: completed, with the bytes it
                // returned.
                if let (Ok(n), Some(_)) = (&result, &buf) {
                    self.inner.observer.add(Counter::Reads, 1);
                    self.inner
                        .observer
                        .add(Counter::ReadBytes, (*n).min(len) as u64);
                }
                if failure.is_none() && !out.sum.stopped {
                    match result {
                        Ok(n) => match &buf {
                            Some(b) => {
                                let n = n.min(len);
                                out.data(start, &b.as_slice()[..n]);
                                // A short read inside the container: the
                                // rest could not be read.
                                out.unreadable(start + n as u64, start + len as u64);
                            }
                            // No bytes came back (a backend breaking
                            // platform contract 1): the scan stops, never
                            // reporting the range as unreadable media.
                            None => {
                                failure = Some(not_written(NotWrittenCause::Interrupted, c));
                            }
                        },
                        Err(raw) if is_media_error(raw) => {
                            // Bisect on an idle queue: first collect every
                            // read still in flight (they are delivered later,
                            // in order), then split the failed range.
                            if let Some(l) = &mut lane {
                                if let Err(e) = collect(l, &mut ring, depth, &mut in_flight) {
                                    return Err(read_error(e, c));
                                }
                            }
                            match buf.take() {
                                // On a lane kept (a parallel one, or any for the
                                // store's own visitors), delivering as it goes.
                                Some(b) if !deferred => {
                                    let bisected = match &mut lane {
                                        Some(l) => {
                                            self.bisect(l, r, start, len as u64, b, &mut out, c)
                                        }
                                        None => Err(not_written(NotWrittenCause::Interrupted, c)),
                                    };
                                    match bisected {
                                        Ok(b) => buf = Some(b),
                                        Err(e) => failure = Some(e),
                                    }
                                }
                                // On a shared lane taken again, holding its
                                // items back until the lane is put back.
                                Some(b) => match self.lane(r, Op::Read) {
                                    Ok(mut l) => {
                                        out.hold();
                                        let bisected = self
                                            .bisect(&mut l, r, start, len as u64, b, &mut out, c);
                                        drop(l);
                                        out.release();
                                        match bisected {
                                            Ok(b) => buf = Some(b),
                                            // The visitor stopped inside the
                                            // held items: what failed after
                                            // was never asked for.
                                            Err(_) if out.sum.stopped => {}
                                            Err(e) => failure = Some(e),
                                        }
                                    }
                                    Err(e) => failure = Some(e),
                                },
                                // No buffer to bisect with (withheld by a
                                // backend that lost track of its kernel,
                                // platform contract 1): stop, never drop the range.
                                None => {
                                    failure = Some(not_written(NotWrittenCause::Interrupted, c));
                                }
                            }
                        }
                        Err(raw) => failure = Some(read_error(raw, c)),
                    }
                }
                if let Some(b) = buf {
                    free.push(b);
                }
            }
            if in_flight == 0 && (next >= to || out.sum.stopped || failure.is_some()) {
                break;
            }
            // Nothing in flight and nothing done this round: nothing ever
            // will be (a backstop: the scan fails, never spins).
            if in_flight == 0 && (submitted, delivered) == progress {
                return Err(not_written(NotWrittenCause::Interrupted, c));
            }
            if in_flight > 0 {
                // Only a lane kept (not deferred) has reads in flight here.
                let Some(l) = &mut lane else {
                    return Err(not_written(NotWrittenCause::Interrupted, c));
                };
                if let Err(e) = collect_one(l, &mut ring, depth, &mut in_flight) {
                    // The reads in flight still own their buffers; the queue
                    // keeps them. Nothing else is safe.
                    return Err(read_error(e, c));
                }
            }
        }
        match failure {
            Some(e) => Err(e),
            None => Ok(out.finish()),
        }
    }

    /// Reads `[start, start + len)` again in halves on an idle lane until each
    /// failing piece is one sector ([`Store::sector`]), delivering in order.
    /// Returns the buffer.
    #[allow(clippy::too_many_arguments)]
    fn bisect<F>(
        &self,
        lane: &mut Lane<P::Queue>,
        r: &LiveRegion,
        start: u64,
        len: u64,
        mut buf: IoBuf,
        out: &mut Emitter<F>,
        c: ErrorContext,
    ) -> Result<IoBuf, Error>
    where
        F: FnMut(ScanItem<'_>) -> ControlFlow<()>,
    {
        if out.sum.stopped {
            return Ok(buf);
        }
        let sector = self.sector();
        // `len ≤ chunk ≤ capacity`.
        let _fits = buf.set_len(len as usize);
        let done = run(
            lane,
            IoOp::Read {
                file: &self.inner.file,
                offset: r.map.locate(start, len),
                buf,
            },
        );
        let (result, back) = match done {
            Ok(Ran::Done { buf, result }) => (result, buf),
            Ok(Ran::Abandoned(raw)) => return Err(read_error(raw, c)),
            Err((_refused, Some(raw))) => return Err(read_error(raw, c)),
            Err((_refused, None)) => return Err(not_written(NotWrittenCause::QueueFull, c)),
        };
        let Some(buf) = back else {
            // No buffer came back (a backend breaking platform contract 1;
            // an abandoned read is `Ran::Abandoned` above).
            return Err(match result {
                Err(raw) => read_error(raw, c),
                Ok(_) => not_written(NotWrittenCause::Interrupted, c),
            });
        };
        match result {
            Ok(n) => {
                let n = (n as u64).min(len);
                self.inner.observer.add(Counter::Reads, 1);
                self.inner.observer.add(Counter::ReadBytes, n);
                out.data(start, &buf.as_slice()[..n as usize]);
                out.unreadable(start + n, start + len);
                Ok(buf)
            }
            Err(raw) if is_media_error(raw) => {
                if len <= sector {
                    out.unreadable(start, start + len);
                    return Ok(buf);
                }
                // Whole blocks while the piece spans several, so every read
                // of a block or more stays block-aligned; sectors only
                // inside one block.
                let block = self.block();
                let unit = if len > block { block } else { sector };
                let half = (len / unit).div_ceil(2) * unit;
                let buf = self.bisect(lane, r, start, half, buf, out, c)?;
                self.bisect(lane, r, start + half, len - half, buf, out, c)
            }
            // Below the store block the device refused the smaller read
            // (alignment) rather than failing on the media: this piece lies
            // in a block that failed with a media error, so it is reported
            // unreadable and the scan goes on.
            Err(_) if len < self.block() => {
                out.unreadable(start, start + len);
                Ok(buf)
            }
            Err(raw) => Err(read_error(raw, c)),
        }
    }
}

/// Waits for at least one completion and files it under its read.
fn collect_one<Q: Queue>(
    lane: &mut Lane<Q>,
    ring: &mut [Option<Pending>],
    depth: usize,
    in_flight: &mut usize,
) -> Result<(), OsError> {
    let _added = lane.wait(1)?;
    for comp in lane.done.drain() {
        *in_flight = in_flight.saturating_sub(1);
        if let Some(p) = ring[(comp.tag % depth as u64) as usize].as_mut() {
            p.done = Some((comp.result, comp.buf));
        }
    }
    Ok(())
}

/// Waits for every read in flight.
fn collect<Q: Queue>(
    lane: &mut Lane<Q>,
    ring: &mut [Option<Pending>],
    depth: usize,
    in_flight: &mut usize,
) -> Result<(), OsError> {
    while *in_flight > 0 {
        collect_one(lane, ring, depth, in_flight)?;
    }
    Ok(())
}
