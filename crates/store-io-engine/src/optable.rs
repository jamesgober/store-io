//! The [`crate::Submitter`]'s op table (D-098, D-099): one row per operation
//! in flight, holding everything its completion needs, and the integrity
//! bookkeeping run when the completion is collected. A durable write keeps
//! its row (and its region's pass) after its I/O completes, until a flush
//! covering it succeeds; a barrier holds a row and no I/O.

use std::collections::VecDeque;
use std::sync::Arc;

use store_io_core::errno::is_media_error;
use store_io_core::error::{ByteRange, CorruptionKind, Error, ErrorContext, NotWrittenCause, Op};
use store_io_core::observe::Counter;
use store_io_platform::{IoBuf, Platform, RawResult};

use crate::completion::{Completion, Done, Written};
use crate::exec::LaneMark;
use crate::flusher::Flushed;
use crate::frontier::Reservation;
use crate::gate::OwnedPass;
use crate::io::{ctx_range, not_written};
use crate::receipt::{DurableReceipt, WriteTicket};
use crate::store::{LiveRegion, Store};

/// What an operation in flight is, in region bytes.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Kind {
    /// An append and its reserved range.
    Append(Reservation),
    /// A page write of `[start, end)`.
    Page { start: u64, end: u64 },
    /// A read of `[start, end)`.
    Read { start: u64, end: u64 },
    /// A barrier for one ticket: the receipt to complete (its `need` set
    /// by the flush).
    Sync(DurableReceipt),
    /// A barrier for the whole store.
    All,
}

/// An operation in flight: everything its completion needs.
struct Live {
    tag: u64,
    seq: u64,
    kind: Kind,
    /// Keeps the region's generation open until the completion is handed
    /// back (`None` for barriers, which do no I/O on a region).
    pass: Option<OwnedPass<LiveRegion>>,
    /// A write to hand back only once durable ([`crate::WriteMode::Durable`]).
    durable: bool,
    /// A durable write that completed in full, waiting for its flush: its
    /// buffer, its ticket, and for an append whose region was complete up
    /// to its end, how far (the receipt is then a prefix receipt).
    held: Option<(Option<IoBuf>, WriteTicket, Option<u64>)>,
}

/// One row. `generation` changes each time the row is freed, so a queue tag
/// names one use of one row.
struct Row {
    generation: u32,
    live: Option<Live>,
}

/// The op table and the completions handed back but not yet taken.
pub(crate) struct Table {
    rows: Box<[Row]>,
    free: Vec<u32>,
    pub(crate) ready: VecDeque<Completion>,
    /// `(row, seq)` in submission order; entries whose row has finished (or
    /// was reused) are stale and skipped. Compacted in place when it reaches
    /// twice the depth, so it never grows: the oldest operation in flight is
    /// found in amortised O(1).
    order: VecDeque<(u32, u64)>,
    /// Operations submitted and not yet processed.
    pub(crate) live: usize,
    /// Rows waiting for a flush outcome (durable writes and barriers).
    pub(crate) flushing: usize,
    /// Of those, requests the flush thread accepted and has not answered
    /// (or whose answer is not taken yet): the ones worth waiting for.
    pub(crate) asked: usize,
    next_seq: u64,
    /// Busy mark for `Store::oldest_outstanding`: `2 * oldest + 1` while
    /// anything is in flight, even otherwise (D-095's lane-mark rules).
    mark: Arc<LaneMark>,
}

impl Table {
    pub(crate) fn new(depth: usize, mark: Arc<LaneMark>) -> Self {
        Self {
            rows: (0..depth)
                .map(|_| Row {
                    generation: 0,
                    live: None,
                })
                .collect(),
            free: (0..depth as u32).rev().collect(),
            ready: VecDeque::with_capacity(depth),
            order: VecDeque::with_capacity(2 * depth),
            live: 0,
            flushing: 0,
            asked: 0,
            next_seq: 0,
            mark,
        }
    }

    pub(crate) fn depth(&self) -> usize {
        self.rows.len()
    }

    /// No room: operations in flight plus completions not yet taken fill
    /// the table (so the ready queue never grows past the depth).
    pub(crate) fn full(&self) -> bool {
        self.live + self.ready.len() >= self.rows.len()
    }

    /// A free row and the queue tag naming this use of it.
    pub(crate) fn claim(&mut self) -> Option<(u32, u64)> {
        let index = self.free.pop()?;
        let generation = self.rows[index as usize].generation;
        Some((index, u64::from(index) | (u64::from(generation) << 32)))
    }

    /// Gives back a claimed row whose operation was refused.
    pub(crate) fn unclaim(&mut self, index: u32) {
        self.free.push(index);
    }

    /// Records a submitted operation in its claimed row (a barrier waits for
    /// a flush from the start).
    pub(crate) fn start(
        &mut self,
        index: u32,
        tag: u64,
        kind: Kind,
        pass: Option<OwnedPass<LiveRegion>>,
        durable: bool,
    ) {
        let seq = self.next_seq;
        self.next_seq += 1;
        if self.order.len() >= 2 * self.rows.len() {
            let rows = &self.rows;
            self.order.retain(|&(i, s)| is_live(rows, i, s));
        }
        self.order.push_back((index, seq));
        if matches!(kind, Kind::Sync(_) | Kind::All) {
            self.flushing += 1;
        }
        self.rows[index as usize].live = Some(Live {
            tag,
            seq,
            kind,
            pass,
            durable,
            held: None,
        });
        self.live += 1;
        self.publish();
    }

    /// Publishes the busy mark (only the owner writes it).
    fn publish(&self) {
        self.mark.set(match self.order.front() {
            Some(&(_, seq)) if self.live > 0 => 2 * seq + 1,
            _ => 2 * self.next_seq,
        });
    }

    /// Whether a write's I/O is in flight (not a durable write whose I/O
    /// completed, nor a barrier).
    pub(crate) fn writes_live(&self) -> bool {
        self.rows.iter().any(|r| {
            r.live.as_ref().is_some_and(|l| {
                l.held.is_none() && matches!(l.kind, Kind::Append(_) | Kind::Page { .. })
            })
        })
    }

    /// Whether `live` counts exactly the rows in use (false only if a panic
    /// interrupted the processing of a completion).
    pub(crate) fn consistent(&self) -> bool {
        self.rows.iter().filter(|r| r.live.is_some()).count() == self.live
    }

    /// The row a tag names, if it is in use by that tag's operation.
    fn row(&mut self, qtag: u64) -> Option<&mut Row> {
        let index = (qtag & 0xFFFF_FFFF) as usize;
        let generation = (qtag >> 32) as u32;
        self.rows
            .get_mut(index)
            .filter(|r| r.generation == generation && r.live.is_some())
    }

    /// Processes one I/O completion: settles the operation (ticket,
    /// reservation or poison). A durable write that completed in full on a
    /// flush-required store keeps its row and returns the flush it needs
    /// (`(qtag, need)`); anything else is handed back now, after which its
    /// row and pass are released.
    pub(crate) fn finish<P: Platform>(
        &mut self,
        store: &Store<P>,
        qtag: u64,
        mut buf: Option<IoBuf>,
        result: RawResult<usize>,
    ) -> Option<(u64, u64)>
    where
        P::Queue: Send,
    {
        let row = self.row(qtag);
        // The queue is this submitter's alone and every tag names one use of
        // one row: an unmatched completion cannot happen. Its buffer, if
        // any, is the device's no longer, so dropping it is safe.
        debug_assert!(row.is_some(), "a completion matched no operation");
        let live = row?.live.as_mut()?;
        let mut prefix = None;
        let mut outcome = settle(store, live, buf.as_ref(), result, &mut prefix);
        if live.durable && store.inner.policy.flush {
            if let Ok(Done::Written(w)) = outcome {
                // The ticket's need was taken when the write's completion
                // had been observed: a flush that far covers it (D-047).
                let need = w.ticket.need;
                live.held = Some((buf, w.ticket, prefix));
                self.flushing += 1;
                return Some((qtag, need));
            }
        } else if live.durable {
            if let Ok(Done::Written(w)) = &mut outcome {
                match store.inner.domain.poisoned() {
                    // As a blocking durable write: no receipt once poisoned.
                    Some(first) => outcome = Err(Error::Poisoned { first }),
                    // Power-safe: the write itself was durable at completion.
                    None => {
                        w.receipt = Some(durable_receipt(store, live, &w.ticket, prefix, u64::MAX));
                    }
                }
            }
        }
        // A short read: the buffer holds only the bytes read, never what
        // its previous user left after them.
        if let (Ok(Done::Read { len }), Some(b)) = (&outcome, buf.as_mut()) {
            let _shorter = b.set_len(*len);
        }
        self.release(qtag, buf, outcome);
        None
    }

    /// Processes a flush outcome for a row waiting for one: the durable
    /// write's receipt (or the flush's failure), or the barrier's.
    pub(crate) fn flushed<P: Platform>(&mut self, store: &Store<P>, qtag: u64, outcome: Flushed)
    where
        P::Queue: Send,
    {
        let live = self.row(qtag).and_then(|r| r.live.as_mut());
        debug_assert!(live.is_some(), "a flush outcome matched no operation");
        let Some(live) = live else { return };
        let (buf, done) = match (live.held.take(), live.kind) {
            (Some((buf, ticket, prefix)), _) => {
                let done = match outcome {
                    Ok(need) => {
                        let receipt = durable_receipt(store, live, &ticket, prefix, need);
                        Ok(Done::Written(Written {
                            ticket,
                            receipt: Some(receipt),
                        }))
                    }
                    Err(e) => Err(in_range(e, &ticket)),
                };
                (buf, done)
            }
            (None, Kind::Sync(mut receipt)) => {
                let done = outcome.map(|need| {
                    receipt.need = need;
                    Done::Synced(receipt)
                });
                (None, done)
            }
            (None, _) => (None, outcome.map(|_| Done::Flushed)),
        };
        self.flushing -= 1;
        self.release(qtag, buf, done);
    }

    /// Hands an operation back and frees its row: its pass is released only
    /// now, after its reservation was completed (or the store poisoned) and
    /// any flush it waited for.
    fn release(&mut self, qtag: u64, buf: Option<IoBuf>, outcome: Result<Done, Error>) {
        let index = (qtag & 0xFFFF_FFFF) as usize;
        let Some(row) = self.row(qtag) else { return };
        let Some(live) = row.live.take() else { return };
        row.generation = row.generation.wrapping_add(1);
        let Live { tag, pass, .. } = live;
        drop(pass);
        self.free.push(index as u32);
        self.live -= 1;
        while let Some(&(i, s)) = self.order.front() {
            if is_live(&self.rows, i, s) {
                break;
            }
            let _stale = self.order.pop_front();
        }
        self.publish();
        self.ready.push_back(Completion { tag, buf, outcome });
    }
}

fn is_live(rows: &[Row], index: u32, seq: u64) -> bool {
    rows[index as usize]
        .live
        .as_ref()
        .is_some_and(|l| l.seq == seq)
}

/// The verdict on one operation, with its integrity bookkeeping: an append
/// that transferred every byte completes its reservation; any write that did
/// not poisons the store (`DurabilityUnknown`).
/// For an append, `prefix` is set to how far the region is complete when
/// that reaches the append's end, read before the ticket's need is taken (so
/// a flush that far covers every byte of it).
fn settle<P: Platform>(
    store: &Store<P>,
    live: &Live,
    buf: Option<&IoBuf>,
    result: RawResult<usize>,
    prefix: &mut Option<u64>,
) -> Result<Done, Error>
where
    P::Queue: Send,
{
    let Some(state) = live.pass.as_ref().map(OwnedPass::get) else {
        // Barriers have no I/O completion.
        return Err(not_written(
            NotWrittenCause::Interrupted,
            ErrorContext::default(),
        ));
    };
    let full = matches!((result, buf), (Ok(n), Some(b)) if n == b.len());
    match live.kind {
        Kind::Append(r) => match state.frontier.as_ref() {
            Some(f) if full => {
                f.complete(r);
                let through = f.completed();
                if through >= r.end {
                    *prefix = Some(through);
                }
                Ok(written(store.ticket(state, r.start, r.end)))
            }
            _ => Err(store.unknown(
                Op::Write,
                result.err(),
                ctx_range(state, r.start, r.data_end),
            )),
        },
        Kind::Page { start, end } => {
            if full {
                Ok(written(store.ticket(state, start, end)))
            } else {
                Err(store.unknown(Op::Write, result.err(), ctx_range(state, start, end)))
            }
        }
        Kind::Read { start, end } => {
            let c = ctx_range(state, start, end);
            let obs = &store.inner.observer;
            match result {
                // A backend that kept the buffer: the bytes never came back.
                Ok(_) if buf.is_none() => Err(not_written(NotWrittenCause::Interrupted, c)),
                Ok(n) => {
                    obs.add(Counter::Reads, 1);
                    obs.add(Counter::ReadBytes, n as u64);
                    Ok(Done::Read { len: n })
                }
                Err(raw) if is_media_error(raw) => {
                    obs.add(Counter::MediaErrors, 1);
                    Err(Error::Corruption {
                        kind: CorruptionKind::MediaError,
                        ctx: c,
                    })
                }
                Err(raw) => Err(Error::Io {
                    op: Op::Read,
                    raw,
                    ctx: c,
                }),
            }
        }
        Kind::Sync(_) | Kind::All => Err(not_written(
            NotWrittenCause::Interrupted,
            ErrorContext::default(),
        )),
    }
}

/// A durable write's receipt, backed by flush `need` (`u64::MAX` on a
/// power-safe store): a prefix receipt through `prefix` for an append whose
/// region was complete that far (published as the region's durable
/// frontier), else one for the write's own range.
fn durable_receipt<P: Platform>(
    store: &Store<P>,
    live: &Live,
    t: &WriteTicket,
    prefix: Option<u64>,
    need: u64,
) -> DurableReceipt
where
    P::Queue: Send,
{
    match (prefix, live.pass.as_ref().map(OwnedPass::get)) {
        (Some(through), Some(state)) => {
            if let Some(f) = state.frontier.as_ref() {
                f.publish_durable(through);
            }
            store.receipt(state, 0, through, need, true)
        }
        _ => store.receipt_for(t, need),
    }
}

/// A failed flush's error, located at the write it was for.
fn in_range(e: Error, t: &WriteTicket) -> Error {
    match e {
        Error::DurabilityUnknown { op, raw, .. } => Error::DurabilityUnknown {
            op,
            raw,
            ctx: ErrorContext {
                region: Some(t.region),
                range: Some(ByteRange {
                    start: t.start,
                    end: t.end,
                }),
            },
        },
        other => other,
    }
}

fn written(ticket: WriteTicket) -> Done {
    Done::Written(Written {
        ticket,
        receipt: None,
    })
}
