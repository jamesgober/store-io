//! The [`crate::Submitter`]'s op table (D-098, D-099): one row per operation
//! in flight, holding everything its completion needs, and the integrity
//! bookkeeping run when the completion is collected. A durable write keeps
//! its row (and its region's pass) after its I/O completes, until a flush
//! covering it succeeds; a barrier holds a row and no I/O.

use std::collections::VecDeque;
use std::sync::Arc;

use store_io_core::error::{Error, ErrorContext, NotWrittenCause};
use store_io_platform::{IoBuf, Platform, RawResult};

use crate::completion::{Completion, Done, Written};
use crate::exec::LaneMark;
use crate::flusher::Flushed;
use crate::frontier::Reservation;
use crate::gate::OwnedPass;
use crate::io::not_written;
use crate::receipt::{DurableReceipt, WriteTicket};
use crate::settle::{Wrote, durable_receipt, in_range, settle_read, settle_write};
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
    /// A blocking layer's operation on a lane (A1): no region bookkeeping in
    /// the row; its caller settles it ([`crate::settle`]) or discards it.
    Raw,
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

    /// Frees the row a lane operation's completion names (`Kind::Raw`,
    /// never settled here) and returns the caller's tag; `None` if the
    /// completion names no row in use (a backend breaking its contract).
    pub(crate) fn take(&mut self, qtag: u64) -> Option<u64> {
        let index = (qtag & 0xFFFF_FFFF) as usize;
        let row = self.row(qtag)?;
        let live = row.live.take()?;
        row.generation = row.generation.wrapping_add(1);
        debug_assert!(
            matches!(live.kind, Kind::Raw),
            "a settled row freed unsettled"
        );
        self.free.push(index as u32);
        self.live -= 1;
        while let Some(&(i, s)) = self.order.front() {
            if is_live(&self.rows, i, s) {
                break;
            }
            let _stale = self.order.pop_front();
        }
        self.publish();
        Some(live.tag)
    }

    /// Frees every row in use without settling it: a lane whose queue
    /// holds nothing in flight can never see their completions.
    pub(crate) fn forget(&mut self) {
        for (i, row) in self.rows.iter_mut().enumerate() {
            if row.live.take().is_some() {
                row.generation = row.generation.wrapping_add(1);
                self.free.push(i as u32);
                self.live -= 1;
            }
        }
        self.order.clear();
        self.publish();
    }

    /// Undoes [`Self::start`] for an operation the queue refused: the row is
    /// free again (same generation: no completion can name it) and the busy
    /// mark is republished. Its pass, if any, is released.
    pub(crate) fn abort(&mut self, index: u32) {
        let Some(row) = self.rows.get_mut(index as usize) else {
            return;
        };
        let Some(live) = row.live.take() else { return };
        if matches!(live.kind, Kind::Sync(_) | Kind::All) {
            self.flushing -= 1;
        }
        drop(live);
        self.free.push(index);
        self.live -= 1;
        while let Some(&(i, s)) = self.order.front() {
            if is_live(&self.rows, i, s) {
                break;
            }
            let _stale = self.order.pop_front();
        }
        self.publish();
    }

    /// Records an operation in its claimed row and publishes the busy mark,
    /// **before** the operation is submitted: a queue that performs the
    /// operation inside `submit` (the synchronous tier) must already show
    /// it as outstanding, so a hung device is visible to
    /// `Store::oldest_outstanding`. A refused submission is undone with
    /// [`Self::abort`]. A barrier waits for a flush from the start.
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
        // A flush-required store, or a region forcing data sync (D-102):
        // read only for a durable write.
        let flushes =
            || store.inner.policy.flush || live.pass.as_ref().is_some_and(|p| p.get().forced());
        if live.durable && flushes() {
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
                        w.receipt = Some(durable_receipt(
                            store,
                            region(live),
                            &w.ticket,
                            prefix,
                            u64::MAX,
                        ));
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
                        let receipt = durable_receipt(store, region(live), &ticket, prefix, need);
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
                    receipt.synced = need != u64::MAX;
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

/// The verdict on one row's I/O completion, through the shared verdicts
/// ([`crate::settle`]): a write is full when it transferred its whole
/// buffer; `prefix` as [`settle_write`].
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
    let Some(state) = region(live) else {
        // Barriers have no I/O completion.
        return Err(not_written(
            NotWrittenCause::Interrupted,
            ErrorContext::default(),
        ));
    };
    let full = matches!((result, buf), (Ok(n), Some(b)) if n == b.len());
    let wrote = match live.kind {
        Kind::Append(r) => Wrote::Append(r),
        Kind::Page { start, end } => Wrote::Page { start, end },
        Kind::Read { start, end } => {
            return settle_read(store, state, start, end, buf, result)
                .map(|len| Done::Read { len });
        }
        Kind::Sync(_) | Kind::All | Kind::Raw => {
            return Err(not_written(
                NotWrittenCause::Interrupted,
                ErrorContext::default(),
            ));
        }
    };
    settle_write(store, state, wrote, full, result.err(), prefix).map(|ticket| {
        Done::Written(Written {
            ticket,
            receipt: None,
        })
    })
}

/// The region a row's pass was admitted to (`None` for barriers).
fn region(live: &Live) -> Option<&LiveRegion> {
    live.pass.as_ref().map(|p| &**p.get())
}
