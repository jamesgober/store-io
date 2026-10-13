//! The [`crate::Submitter`]'s op table (D-098): one row per operation in
//! flight, holding everything its completion needs, and the integrity
//! bookkeeping run when the completion is collected.

use std::collections::VecDeque;
use std::sync::Arc;

use store_io_core::errno::is_media_error;
use store_io_core::error::{CorruptionKind, Error, NotWrittenCause, Op};
use store_io_core::observe::Counter;
use store_io_platform::{IoBuf, Platform, RawResult};

use crate::completion::{Completion, Done, Written};
use crate::exec::LaneMark;
use crate::frontier::Reservation;
use crate::gate::OwnedPass;
use crate::io::{ctx_range, not_written};
use crate::receipt::WriteTicket;
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
}

/// An operation in flight: everything its completion needs.
struct Live {
    tag: u64,
    seq: u64,
    kind: Kind,
    /// Keeps the region's generation open until the completion is processed.
    pass: OwnedPass<LiveRegion>,
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

    /// Records a submitted operation in its claimed row.
    pub(crate) fn start(&mut self, index: u32, tag: u64, kind: Kind, pass: OwnedPass<LiveRegion>) {
        let seq = self.next_seq;
        self.next_seq += 1;
        if self.order.len() >= 2 * self.rows.len() {
            let rows = &self.rows;
            self.order.retain(|&(i, s)| is_live(rows, i, s));
        }
        self.order.push_back((index, seq));
        self.rows[index as usize].live = Some(Live {
            tag,
            seq,
            kind,
            pass,
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

    /// Whether a write is in flight.
    pub(crate) fn writes_live(&self) -> bool {
        self.rows.iter().any(|r| {
            r.live
                .as_ref()
                .is_some_and(|l| !matches!(l.kind, Kind::Read { .. }))
        })
    }

    /// Whether `live` counts exactly the rows in use (false only if a panic
    /// interrupted the processing of a completion).
    pub(crate) fn consistent(&self) -> bool {
        self.rows.iter().filter(|r| r.live.is_some()).count() == self.live
    }

    /// Processes one completion: settles the operation (ticket, reservation
    /// or poison) and only then releases its row and pass.
    pub(crate) fn finish<P: Platform>(
        &mut self,
        store: &Store<P>,
        qtag: u64,
        mut buf: Option<IoBuf>,
        result: RawResult<usize>,
    ) where
        P::Queue: Send,
    {
        let index = (qtag & 0xFFFF_FFFF) as usize;
        let generation = (qtag >> 32) as u32;
        let row = self
            .rows
            .get_mut(index)
            .filter(|r| r.generation == generation && r.live.is_some());
        // The queue is this submitter's alone and every tag names one use of
        // one row: an unmatched completion cannot happen. Its buffer, if
        // any, is the device's no longer, so dropping it is safe.
        debug_assert!(row.is_some(), "a completion matched no operation");
        let Some(row) = row else { return };
        let Some(live) = row.live.take() else { return };
        row.generation = row.generation.wrapping_add(1);
        let outcome = settle(store, &live, buf.as_ref(), result);
        let Live { tag, pass, .. } = live;
        // The pass is released after the reservation is completed (or the
        // store poisoned): recycle may proceed from here.
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
        // A short read: the buffer holds only the bytes read, never what
        // its previous user left after them.
        if let (Ok(Done::Read { len }), Some(b)) = (&outcome, buf.as_mut()) {
            let _shorter = b.set_len(*len);
        }
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
fn settle<P: Platform>(
    store: &Store<P>,
    live: &Live,
    buf: Option<&IoBuf>,
    result: RawResult<usize>,
) -> Result<Done, Error>
where
    P::Queue: Send,
{
    let state = live.pass.get();
    let full = matches!((result, buf), (Ok(n), Some(b)) if n == b.len());
    match live.kind {
        Kind::Append(r) => match state.frontier.as_ref() {
            Some(f) if full => {
                f.complete(r);
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
    }
}

fn written(ticket: WriteTicket) -> Done {
    Done::Written(Written {
        ticket,
        receipt: None,
    })
}
