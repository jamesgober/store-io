//! Raw ordered scans: the recovery read path.
//!
//! A scan reads a region from a block-aligned offset to the end of its data
//! area and hands every byte to the caller in order, as [`ScanItem::Data`].
//! It keeps several large reads in flight (direct I/O, never the page cache)
//! and delivers them strictly in order.
//!
//! A scan never stops at bad data. A read the device cannot complete is
//! bisected down to single blocks; every block that still fails is reported
//! as [`ScanItem::Unreadable`] (adjacent ones merged), and the scan carries on
//! after it. Only the caller decides where valid data ends, by returning
//! [`ControlFlow::Break`] from its visitor.

use std::ops::ControlFlow;

use store_io_core::errno::is_media_error;
use store_io_core::error::{Error, ErrorContext, NotWrittenCause, Op, OsError};
use store_io_platform::{IoBuf, IoOp, Platform, Queue, RawResult};

use crate::exec::{Lane, run};
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
    /// The device could not read `[start, end)` (whole blocks). The scan
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
}

impl<F: FnMut(ScanItem<'_>) -> ControlFlow<()>> Emitter<F> {
    fn flush_bad(&mut self) {
        if let Some((start, end)) = self.bad.take() {
            if (self.visit)(ScanItem::Unreadable { start, end }).is_break() {
                self.sum.stopped = true;
            }
        }
    }

    fn data(&mut self, offset: u64, bytes: &[u8]) {
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
    pub(crate) fn scan_region<F>(
        &self,
        r: &LiveRegion,
        from: u64,
        to: u64,
        visit: F,
    ) -> Result<ScanSummary, Error>
    where
        F: FnMut(ScanItem<'_>) -> ControlFlow<()>,
    {
        let block = self.block();
        let to = to.min(r.data_size);
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
            .map_while(|_| self.inner.pool.take(chunk).ok())
            .collect();
        if free.is_empty() {
            return Err(not_written(NotWrittenCause::PoolExhausted, c));
        }
        let depth = free.len();
        let mut ring: Vec<Option<Pending>> = (0..depth).map(|_| None).collect();
        let mut lane = self.lane(r);
        let file = &self.inner.file;
        let (mut submitted, mut delivered) = (0u64, 0u64);
        let mut next = from;
        let mut in_flight = 0usize;
        let mut failure: Option<Error> = None;
        loop {
            // Keep the pipeline full.
            while failure.is_none() && !out.sum.stopped && next < to {
                let Some(mut buf) = free.pop() else { break };
                let len = (to - next).min(chunk as u64) as usize;
                // `len ≤ chunk ≤ capacity`.
                let _fits = buf.set_len(len);
                let op = IoOp::Read {
                    file,
                    offset: r.data_offset + next,
                    buf,
                };
                match lane.queue.submit(op, submitted) {
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
                if failure.is_none() && !out.sum.stopped {
                    match result {
                        Ok(n) => {
                            let n = n.min(len);
                            if let Some(b) = &buf {
                                out.data(start, &b.as_slice()[..n]);
                            }
                            // A short read inside the container: the rest
                            // could not be read.
                            out.unreadable(start + n as u64, start + len as u64);
                        }
                        Err(raw) if is_media_error(raw) => {
                            // Bisect on an idle queue: first collect every
                            // read still in flight (they are delivered later,
                            // in order), then split the failed range.
                            if let Err(e) = collect(&mut lane, &mut ring, depth, &mut in_flight) {
                                return Err(read_error(e, c));
                            }
                            if let Some(b) = buf.take() {
                                match self.bisect(&mut lane, r, start, len as u64, b, &mut out, c) {
                                    Ok(b) => buf = Some(b),
                                    Err(e) => failure = Some(e),
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
            if in_flight > 0 {
                if let Err(e) = collect_one(&mut lane, &mut ring, depth, &mut in_flight) {
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
    /// failing piece is one block, delivering in order. Returns the buffer.
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
        let block = self.block();
        // `len ≤ chunk ≤ capacity`.
        let _fits = buf.set_len(len as usize);
        let done = run(
            lane,
            IoOp::Read {
                file: &self.inner.file,
                offset: r.data_offset + start,
                buf,
            },
        );
        let (result, back) = match done {
            Ok(d) => (d.result, d.buf),
            Err((_refused, Some(raw))) => return Err(read_error(raw, c)),
            Err((_refused, None)) => return Err(not_written(NotWrittenCause::QueueFull, c)),
        };
        let Some(buf) = back else {
            // The wait failed with the read in flight; its buffer stays with
            // the queue.
            return Err(match result {
                Err(raw) => read_error(raw, c),
                Ok(_) => not_written(NotWrittenCause::QueueFull, c),
            });
        };
        match result {
            Ok(n) => {
                let n = (n as u64).min(len);
                out.data(start, &buf.as_slice()[..n as usize]);
                out.unreadable(start + n, start + len);
                Ok(buf)
            }
            Err(raw) if is_media_error(raw) => {
                if len <= block {
                    out.unreadable(start, start + len);
                    return Ok(buf);
                }
                let half = (len / block).div_ceil(2) * block;
                let buf = self.bisect(lane, r, start, half, buf, out, c)?;
                self.bisect(lane, r, start + half, len - half, buf, out, c)
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
    let _added = lane.queue.wait(1, &mut lane.done)?;
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
