//! The verdict on a completed operation and its integrity bookkeeping (A1,
//! PLANNING R3.3): one place where a write's ticket is minted, an append's
//! reservation is completed, and a write that did not transfer every byte
//! poisons the store. The `Submitter`'s op table settles each of its rows
//! here.

use store_io_core::errno::is_media_error;
use store_io_core::error::{
    ByteRange, CorruptionKind, Error, ErrorContext, NotWrittenCause, Op, OsError,
};
use store_io_core::observe::Counter;
use store_io_platform::{IoBuf, Platform, RawResult};

use crate::frontier::Reservation;
use crate::io::{ctx_range, not_written};
use crate::receipt::{DurableReceipt, WriteTicket};
use crate::store::{LiveRegion, Store};

/// What a write wrote, in region bytes.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Wrote {
    /// An append and its reserved range.
    Append(Reservation),
    /// A page write of `[start, end)`.
    Page { start: u64, end: u64 },
}

/// The verdict on a write of `state`: `full` when every byte of it was
/// transferred (for a write issued as several device writes, every one of
/// them in full), else `raw` is the failure, if the OS gave one.
///
/// An append that transferred every byte completes its reservation, and
/// `prefix` is set to how far the region is then complete when that reaches
/// the append's end (read before the ticket's need is taken, so a flush that
/// far covers every byte of it). Any write that did not transfer every byte
/// poisons the store (`DurabilityUnknown`): for an append its reserved range
/// is a hole no later append may be acknowledged past.
///
/// `Wrote::Append` needs an append region's state: without a frontier the
/// write is not acknowledged (it poisons, as a failed write). `prefix` is
/// only ever set, never cleared: pass `None` for each write.
pub(crate) fn settle_write<P: Platform>(
    store: &Store<P>,
    state: &LiveRegion,
    wrote: Wrote,
    full: bool,
    raw: Option<OsError>,
    prefix: &mut Option<u64>,
) -> Result<WriteTicket, Error>
where
    P::Queue: Send,
{
    match wrote {
        Wrote::Append(r) => match state.frontier.as_ref() {
            Some(f) if full => {
                f.complete(r);
                let through = f.completed();
                if through >= r.end {
                    *prefix = Some(through);
                }
                Ok(store.ticket(state, r.start, r.end))
            }
            _ => Err(store.unknown(Op::Write, raw, ctx_range(state, r.start, r.data_end))),
        },
        Wrote::Page { start, end } => {
            if full {
                Ok(store.ticket(state, start, end))
            } else {
                Err(store.unknown(Op::Write, raw, ctx_range(state, start, end)))
            }
        }
    }
}

/// The verdict on a read of `[start, end)` of `state`: the length read, a
/// media error (`Corruption(MediaError)`, counted), or the raw failure as
/// `Io`. A read that succeeded but whose buffer the backend kept gave no
/// bytes back: `Interrupted`.
pub(crate) fn settle_read<P: Platform>(
    store: &Store<P>,
    state: &LiveRegion,
    start: u64,
    end: u64,
    buf: Option<&IoBuf>,
    result: RawResult<usize>,
) -> Result<usize, Error>
where
    P::Queue: Send,
{
    let c = ctx_range(state, start, end);
    let obs = &store.inner.observer;
    match result {
        Ok(_) if buf.is_none() => Err(not_written(NotWrittenCause::Interrupted, c)),
        Ok(n) => {
            obs.add(Counter::Reads, 1);
            obs.add(Counter::ReadBytes, n as u64);
            Ok(n)
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

/// A durable write's receipt, backed by flush `need` (`u64::MAX` on a
/// power-safe store): a prefix receipt through `prefix` for an append whose
/// region was complete that far (published as the region's durable
/// frontier), else one for the write's own range.
pub(crate) fn durable_receipt<P: Platform>(
    store: &Store<P>,
    state: Option<&LiveRegion>,
    t: &WriteTicket,
    prefix: Option<u64>,
    need: u64,
) -> DurableReceipt
where
    P::Queue: Send,
{
    match (prefix, state) {
        (Some(through), Some(state)) => {
            let synced = need != u64::MAX;
            if let Some(f) = state.frontier.as_ref() {
                if synced {
                    f.publish_synced(through);
                } else {
                    f.publish_durable(through);
                }
            }
            store.receipt(state, 0, through, need, true, synced)
        }
        _ => store.receipt_for(t, need),
    }
}

/// A failed flush's error, located at the write it was for.
pub(crate) fn in_range(e: Error, t: &WriteTicket) -> Error {
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
