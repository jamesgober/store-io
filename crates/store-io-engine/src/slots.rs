//! Reading and writing A/B slot pairs through a platform queue.
//!
//! Every piece of store-io metadata (volume record, region table, region
//! headers, small objects) is an A/B slot pair. Reading a pair reads both
//! slots, validates each, and lets [`store_io_format::pair::decide`] pick the
//! winner; it never rewrites anything. Writing a slot encodes the whole slot
//! into one aligned buffer and writes it as one device I/O.

use store_io_buf::BufPool;
use store_io_core::errno::{is_end_of_file, is_media_error};
use store_io_core::error::{Error, ErrorContext, NotWrittenCause, Op, OsError};
use store_io_format::pair::{PairReport, decide};
use store_io_format::slot::{
    Expect, HeaderFacts, SlotFields, SlotIndex, SlotStatus, encode, header_facts, validate,
};
use store_io_platform::{IoOp, Queue};

use crate::exec::{Lane, Ran, run};

/// Where a pair lives and what it must contain.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PairAt {
    pub(crate) volume: [u8; 16],
    pub(crate) object: u64,
    pub(crate) a: u64,
    pub(crate) b: u64,
    pub(crate) log2: u8,
    /// The container's format (`FORMAT_1` or `FORMAT_2`).
    pub(crate) format: u16,
    /// Format 2: the object carries `meta_seq` (the volume record and the
    /// table pages).
    pub(crate) numbered: bool,
}

impl PairAt {
    fn expect(&self, i: SlotIndex) -> Expect {
        Expect {
            volume_uuid: self.volume,
            object_id: self.object,
            slot_offset: self.offset(i),
            slot_index: i,
            log2_slot_size: self.log2,
            format_major: self.format,
            numbered: self.numbered,
        }
    }

    pub(crate) fn offset(&self, i: SlotIndex) -> u64 {
        match i {
            SlotIndex::A => self.a,
            SlotIndex::B => self.b,
        }
    }

    fn size(&self) -> usize {
        1usize << self.log2
    }
}

/// A read pair: the decision and a copy of the winner's payload.
#[derive(Debug, Clone)]
pub(crate) struct PairRead {
    pub(crate) report: PairReport,
    pub(crate) payload: Option<Vec<u8>>,
    /// A slot could not be read because of a media error (not a short read
    /// or the end of the file): the pair may be intact on the media.
    pub(crate) media_error: bool,
    /// What each slot's header shows (A, B): the format-2 tear judgement
    /// reads them; `None` for a slot that could not be read.
    pub(crate) facts: [Option<HeaderFacts>; 2],
}

impl PairRead {
    /// The pair after `w` was written durably over one of its slots, with no
    /// device read: the written slot has the status of the exact bytes
    /// written, the other slot keeps its previous status, and the decision is
    /// made again (D-069). A read-back would prove nothing about media (it
    /// can be served from the device cache) and could only fail or mislead.
    pub(crate) fn after(&self, w: Written) -> PairRead {
        let (a, b) = match w.index {
            SlotIndex::A => (w.status, self.report.b),
            SlotIndex::B => (self.report.a, w.status),
        };
        let mut facts = self.facts;
        facts[usize::from(w.index.as_u8())] = w.facts;
        let report = decide(a, b);
        let previous_winner = self.report.winner_info().map(|(i, _)| i);
        let payload = match report.winner_info() {
            Some((i, _)) if i == w.index => w.payload,
            Some((i, _)) if Some(i) == previous_winner => self.payload.clone(),
            _ => None,
        };
        PairRead {
            report,
            payload,
            media_error: false,
            facts,
        }
    }

    /// A pair never written (both slots blank).
    pub(crate) fn blank() -> PairRead {
        PairRead {
            report: decide(SlotStatus::Blank, SlotStatus::Blank),
            payload: None,
            media_error: false,
            facts: [None, None],
        }
    }
}

/// One slot as written: its index, the status the validator gives the exact
/// bytes written, the payload those bytes carry and what their header shows.
#[derive(Debug, Clone)]
pub(crate) struct Written {
    pub(crate) index: SlotIndex,
    pub(crate) status: SlotStatus,
    pub(crate) payload: Option<Vec<u8>>,
    pub(crate) facts: Option<HeaderFacts>,
}

fn pool_err(ctx: ErrorContext) -> Error {
    Error::NotWritten {
        cause: NotWrittenCause::PoolExhausted,
        ctx,
    }
}

/// Whether a failed metadata read means the bytes cannot be read (a media
/// error, or a read past the end of the file), as opposed to a failure that
/// says nothing about them (a bad handle, an exhausted resource), which is
/// returned to the caller with its code and never mistaken for damage.
pub(crate) fn unreadable(raw: OsError) -> bool {
    is_media_error(raw) || is_end_of_file(raw)
}

/// One slot as read.
struct OneSlot {
    status: SlotStatus,
    payload: Option<Vec<u8>>,
    /// The read failed with a media error.
    media: bool,
    facts: Option<HeaderFacts>,
}

impl OneSlot {
    fn unreadable(media: bool) -> Self {
        Self {
            status: SlotStatus::ReadError,
            payload: None,
            media,
            facts: None,
        }
    }
}

/// Reads one slot. A media error, a short read or end of file makes it
/// [`SlotStatus::ReadError`]; any other read failure is returned as `Io`.
fn read_one<Q: Queue>(
    q: &mut Lane<Q>,
    file: &Q::File,
    pool: &BufPool,
    at: &PairAt,
    i: SlotIndex,
) -> Result<OneSlot, Error> {
    // Uncapped on purpose: a metadata read may use a large buffer rather
    // than fail when every ordinary one is in use.
    let buf = pool
        .take(at.size())
        .map_err(|_| pool_err(ErrorContext::default()))?;
    let offset = at.offset(i);
    let ran = run(q, IoOp::Read { file, offset, buf }).map_err(|(_b, raw)| match raw {
        Some(raw) => Error::Io {
            op: Op::Read,
            raw,
            ctx: ErrorContext::default(),
        },
        None => pool_err(ErrorContext::default()),
    })?;
    let fail = |raw| Error::Io {
        op: Op::Read,
        raw,
        ctx: ErrorContext::default(),
    };
    let (got, result) = match ran {
        Ran::Done { buf, result } => (buf, result),
        // A failed wait is never an unreadable slot: weighing it as one
        // would let the pair decision pick the older copy (and a later write
        // overwrite the newer one).
        Ran::Abandoned(raw) => return Err(fail(raw)),
    };
    finish_one(at, i, got, result)
}

/// Reads and decides a pair. Never writes.
pub(crate) fn read_pair<Q: Queue>(
    q: &mut Lane<Q>,
    file: &Q::File,
    pool: &BufPool,
    at: &PairAt,
) -> Result<PairRead, Error> {
    let a = read_one(q, file, pool, at, SlotIndex::A)?;
    let b = read_one(q, file, pool, at, SlotIndex::B)?;
    Ok(assemble(a, b))
}

/// The bytes of a completed read of `len` bytes, or `Ok(Err(media))` when
/// they cannot be read: a media error, a short read or a read past the end
/// of the file leaves the slot unreadable, which the pair decision weighs
/// (`media`: whether by a media error). Any other failure is returned with
/// its code as `Io` (see [`unreadable`]).
fn landed(
    got: Option<store_io_buf::IoBuf>,
    result: Result<usize, OsError>,
    len: usize,
) -> Result<Result<store_io_buf::IoBuf, bool>, Error> {
    let media = matches!(result, Err(raw) if is_media_error(raw));
    match result {
        Err(raw) if !unreadable(raw) => Err(Error::Io {
            op: Op::Read,
            raw,
            ctx: ErrorContext::default(),
        }),
        Ok(n) if n == len => Ok(got.ok_or(media)),
        _ => Ok(Err(media)),
    }
}

/// One slot's bytes, validated.
fn judge_slot(at: &PairAt, i: SlotIndex, bytes: &[u8]) -> OneSlot {
    let expect = at.expect(i);
    let status = validate(bytes, &expect);
    let payload = match status {
        SlotStatus::Valid(info) => Some(info.payload(bytes).to_vec()),
        _ => None,
    };
    OneSlot {
        status,
        payload,
        media: false,
        facts: header_facts(bytes, &expect),
    }
}

/// What one slot read came to: a media error, a short read or end of file
/// makes it [`SlotStatus::ReadError`]; any other failure is returned as
/// `Io` (see [`unreadable`]).
fn finish_one(
    at: &PairAt,
    i: SlotIndex,
    got: Option<store_io_buf::IoBuf>,
    result: Result<usize, OsError>,
) -> Result<OneSlot, Error> {
    Ok(match landed(got, result, at.size())? {
        Ok(buf) => judge_slot(at, i, buf.as_slice()),
        Err(media) => OneSlot::unreadable(media),
    })
}

/// The pair decision over two slots as read.
fn assemble(a: OneSlot, b: OneSlot) -> PairRead {
    let report = decide(a.status, b.status);
    let payload = match report.winner_info() {
        Some((SlotIndex::A, _)) => a.payload,
        Some((SlotIndex::B, _)) => b.payload,
        None => None,
    };
    PairRead {
        report,
        payload,
        media_error: a.media || b.media,
        facts: [a.facts, b.facts],
    }
}

/// What one read of [`read_pairs`] covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    One(SlotIndex),
    /// Both slots, B right after A: one device read for the pair.
    Both,
}

/// Reads every pair in `ats` with up to `depth` reads in flight on `q` (fewer
/// when the lane's rows or the pool's buffers run out), a pair whose slot B
/// follows its slot A (every header pair provisioning writes) in one read of
/// both. Read one at a time, every slot costs a device round trip:
/// an open of 65,536 regions read 131,072 header slots in 9 s (E2b/E3 C8).
/// Each slot is judged by the same rules as [`read_pair`]; the result is in
/// `ats`'s order. A slot whose read failed is returned as such (both, when
/// they were read as one) for the caller's one re-read, slot by slot
/// ([`reread_failed`]).
///
/// # Errors
///
/// As [`read_pair`]: `Io` for a read failure that is not an unreadable slot
/// or a failed wait (the queue keeps what is in flight: the lane's next
/// holder clears it, or, for a lane of the caller's own, the queue's drop
/// waits for it), a pool error if no buffer can be had with nothing in
/// flight.
pub(crate) fn read_pairs<Q: Queue>(
    q: &mut Lane<Q>,
    file: &Q::File,
    pool: &BufPool,
    ats: &[PairAt],
    depth: usize,
) -> Result<Vec<PairRead>, Error> {
    let io = |raw| Error::Io {
        op: Op::Read,
        raw,
        ctx: ErrorContext::default(),
    };
    crate::exec::clear_stale(q).map_err(io)?;
    let mut reads: Vec<(usize, Part)> = Vec::with_capacity(ats.len());
    for (i, at) in ats.iter().enumerate() {
        if at.a.checked_add(at.size() as u64) == Some(at.b) {
            reads.push((i, Part::Both));
        } else {
            reads.push((i, Part::One(SlotIndex::A)));
            reads.push((i, Part::One(SlotIndex::B)));
        }
    }
    let mut slots: Vec<[Option<OneSlot>; 2]> = (0..ats.len()).map(|_| [None, None]).collect();
    let (mut next, mut arrived, mut in_flight) = (0usize, 0usize, 0usize);
    // The class each read's buffer comes from while reads are in flight.
    let own_class = |len: usize| pool.class_len(len).unwrap_or(len);
    let depth = depth.max(1);
    while arrived < reads.len() {
        while next < reads.len() && in_flight < depth {
            let (i, part) = reads[next];
            let (offset, len) = match part {
                Part::One(s) => (ats[i].offset(s), ats[i].size()),
                Part::Both => (ats[i].a, 2 * ats[i].size()),
            };
            // A buffer of the read's own class while reads are in flight (a
            // completion gives one back); with none in flight, any class, as
            // `read_one` (a metadata read uses a large buffer rather than fail).
            let taken = if in_flight > 0 {
                pool.take_within(len, own_class(len))
            } else {
                pool.take(len)
            };
            let Ok(buf) = taken else {
                if in_flight > 0 {
                    break;
                }
                return Err(pool_err(ErrorContext::default()));
            };
            match q.submit(IoOp::Read { file, offset, buf }, next as u64) {
                Ok(()) => {
                    next += 1;
                    in_flight += 1;
                }
                // The lane's rows are full: wait for one.
                Err(r) if r.raw.is_none() && in_flight > 0 => break,
                Err(r) => {
                    return Err(match r.raw {
                        Some(raw) => io(raw),
                        None => pool_err(ErrorContext::default()),
                    });
                }
            }
        }
        let _arrived = q.wait(1).map_err(io)?;
        for c in q.done.drain() {
            debug_assert!(
                c.buf.is_some() || c.result.is_err(),
                "a read succeeded without its buffer (platform contract 1)"
            );
            in_flight -= 1;
            arrived += 1;
            let (i, part) = reads[c.tag as usize];
            let at = &ats[i];
            match part {
                Part::One(s) => {
                    slots[i][usize::from(s.as_u8())] = Some(finish_one(at, s, c.buf, c.result)?);
                }
                Part::Both => match landed(c.buf, c.result, 2 * at.size())? {
                    Ok(buf) => {
                        let (a, b) = buf.as_slice().split_at(at.size());
                        slots[i] = [
                            Some(judge_slot(at, SlotIndex::A, a)),
                            Some(judge_slot(at, SlotIndex::B, b)),
                        ];
                    }
                    Err(media) => {
                        slots[i] = [
                            Some(OneSlot::unreadable(media)),
                            Some(OneSlot::unreadable(media)),
                        ];
                    }
                },
            }
        }
    }
    debug_assert!(
        slots.iter().all(|s| s.iter().all(Option::is_some)),
        "a slot read never arrived"
    );
    Ok(slots
        .into_iter()
        .map(|[a, b]| {
            assemble(
                a.unwrap_or_else(|| OneSlot::unreadable(false)),
                b.unwrap_or_else(|| OneSlot::unreadable(false)),
            )
        })
        .collect())
}

/// Reads again, once, each slot of `first` whose read failed, and decides
/// the pair from the slots' best results: a slot that read stays as read
/// (a second read of it could fail and lose a good copy). Nothing writes
/// between the two reads (the caller holds the store). Returns whether any
/// slot was read again.
pub(crate) fn reread_failed<Q: Queue>(
    q: &mut Lane<Q>,
    file: &Q::File,
    pool: &BufPool,
    at: &PairAt,
    first: PairRead,
) -> Result<(PairRead, bool), Error> {
    let failed = |s: SlotStatus| s == SlotStatus::ReadError;
    if !failed(first.report.a) && !failed(first.report.b) {
        return Ok((first, false));
    }
    let mut status = [first.report.a, first.report.b];
    let mut facts = first.facts;
    let mut payloads: [Option<Vec<u8>>; 2] = [None, None];
    let mut media = false;
    for (i, slot) in [SlotIndex::A, SlotIndex::B].into_iter().enumerate() {
        if failed(status[i]) {
            let one = read_one(q, file, pool, at, slot)?;
            status[i] = one.status;
            facts[i] = one.facts;
            payloads[i] = one.payload;
            media |= one.media;
        }
    }
    let report = decide(status[0], status[1]);
    // The first read's payload is its winner's, a slot that read.
    let payload = match report.winner_info() {
        Some((slot, _)) => {
            let i = usize::from(slot.as_u8());
            if failed([first.report.a, first.report.b][i]) {
                payloads[i].take()
            } else {
                first.payload
            }
        }
        None => None,
    };
    Ok((
        PairRead {
            report,
            payload,
            media_error: media,
            facts,
        },
        true,
    ))
}

/// What to write into the next slot of a pair.
pub(crate) struct NextSlot<'a> {
    pub(crate) index: SlotIndex,
    pub(crate) generation: u64,
    pub(crate) owner_generation: u64,
    pub(crate) prev_header_crc: u32,
    pub(crate) payload: &'a [u8],
    /// Format 2, numbered objects: this write's `meta_seq` (0 otherwise).
    pub(crate) meta_seq: u64,
}

/// Writes one complete slot as one device I/O. Returns what was written:
/// the bytes are validated in memory before submission, with the same
/// validator a read uses, so the caller knows the new pair state without
/// reading the device back (D-069).
///
/// # Errors
///
/// `NotWritten` if nothing reached the device; `Corruption(Metadata)` before
/// any I/O if the encoded bytes do not validate (an encoder defect);
/// `DurabilityUnknown` (the caller poisons) for a failed or short write.
pub(crate) fn write_slot<Q: Queue>(
    q: &mut Lane<Q>,
    file: &Q::File,
    pool: &BufPool,
    at: &PairAt,
    next: &NextSlot<'_>,
    dsync: bool,
) -> Result<Written, Error> {
    let ctx = ErrorContext::default();
    // A numbered format-2 object carries a meta_seq, and nothing else does.
    debug_assert_eq!(
        next.meta_seq != 0,
        at.format == store_io_format::slot::FORMAT_2 && at.numbered,
        "meta_seq for a numbered format-2 object only"
    );
    let mut buf = pool.take(at.size()).map_err(|_| pool_err(ctx))?;
    let fields = SlotFields {
        volume_uuid: at.volume,
        object_id: at.object,
        generation: next.generation,
        owner_generation: next.owner_generation,
        slot_offset: at.offset(next.index),
        slot_index: next.index,
        log2_slot_size: at.log2,
        prev_header_crc32c: next.prev_header_crc,
        format_major: at.format,
        meta_seq: next.meta_seq,
    };
    let crc = encode(buf.as_mut_slice(), &fields, next.payload).map_err(|e| match e {
        store_io_format::slot::EncodeError::Format => Error::Unsupported {
            what: store_io_core::error::Capability::FormatVersion,
        },
        _ => Error::NotWritten {
            cause: NotWrittenCause::TooLarge,
            ctx,
        },
    })?;
    let offset = at.offset(next.index);
    let expect = at.expect(next.index);
    let status = validate(buf.as_slice(), &expect);
    let SlotStatus::Valid(info) = status else {
        return Err(Error::Corruption {
            kind: store_io_core::error::CorruptionKind::Metadata,
            ctx,
        });
    };
    if info.header_crc32c != crc {
        return Err(Error::Corruption {
            kind: store_io_core::error::CorruptionKind::Metadata,
            ctx,
        });
    }
    let written = Written {
        index: next.index,
        status,
        payload: Some(info.payload(buf.as_slice()).to_vec()),
        facts: header_facts(buf.as_slice(), &expect),
    };
    let ran = run(
        q,
        IoOp::Write {
            file,
            offset,
            buf,
            dsync,
        },
    )
    .map_err(|(_b, raw)| Error::NotWritten {
        cause: if raw.is_some() {
            NotWrittenCause::Interrupted
        } else {
            NotWrittenCause::QueueFull
        },
        ctx,
    })?;
    let result = match ran {
        Ran::Done { result, .. } => result,
        Ran::Abandoned(raw) => Err(raw),
    };
    match result {
        Ok(n) if n == at.size() => Ok(written),
        Ok(_) => Err(Error::DurabilityUnknown {
            op: Op::Write,
            raw: None,
            ctx,
        }),
        Err(raw) => Err(Error::DurabilityUnknown {
            op: Op::Write,
            raw: Some(raw),
            ctx,
        }),
    }
}

/// The slot to write next and the generation/CRC chain for it.
///
/// # Errors
///
/// `Corruption(Fork)` when the pair has no safe next write (a fork, a
/// refused or lost pair, or an exhausted generation); `Unsupported` when the
/// current version carries a read-only-compatible feature this version of
/// store-io does not know, so it must not be overwritten.
pub(crate) fn plan_next<'a>(
    read: &PairRead,
    payload: &'a [u8],
    owner_generation: u64,
) -> Result<NextSlot<'a>, Error> {
    let fork = || Error::Corruption {
        kind: store_io_core::error::CorruptionKind::Fork,
        ctx: ErrorContext::default(),
    };
    let index = read.report.next_write().ok_or_else(fork)?;
    let (generation, prev) = match read.report.winner_info() {
        Some((_, info)) if !info.writable() => {
            return Err(Error::Unsupported {
                what: store_io_core::error::Capability::FormatVersion,
            });
        }
        Some((_, info)) => (
            info.generation.checked_add(1).ok_or_else(fork)?,
            info.header_crc32c,
        ),
        None => (1, 0),
    };
    Ok(NextSlot {
        index,
        generation,
        owner_generation,
        prev_header_crc: prev,
        payload,
        meta_seq: 0,
    })
}

#[cfg(all(test, not(loom)))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::path::Path;

    use store_io_sim::{SimConfig, SimPlatform};

    use super::*;
    use crate::store::{Store, StoreOptions, pair_volume};

    /// The pair state a write computes in memory (`after`) is what a read
    /// of the device finds: the same statuses and header facts in each
    /// slot, whichever slot was written (format 1's volume pair, written
    /// directly).
    #[test]
    fn test_after_a_write_matches_a_fresh_read() {
        let p = SimPlatform::new(SimConfig::volatile(41));
        let opts = StoreOptions {
            container_format: store_io_format::slot::FORMAT_1,
            ..StoreOptions::default()
        };
        let s = Store::create(p, Path::new("/db"), opts).unwrap();
        let inner = &s.inner;
        let at = pair_volume(inner.volume, &inner.layout);
        let mut prev = {
            let mut q = inner.queues.get(0);
            read_pair(&mut q, &inner.file, &inner.pool, &at).unwrap()
        };
        let payload = prev.payload.clone().unwrap();
        for _ in 0..2 {
            let mut q = inner.queues.get(0);
            let next = plan_next(&prev, &payload, 1).unwrap();
            let w = write_slot(&mut q, &inner.file, &inner.pool, &at, &next, false).unwrap();
            let after = prev.after(w);
            let read = read_pair(&mut q, &inner.file, &inner.pool, &at).unwrap();
            assert_eq!(after.report.a, read.report.a);
            assert_eq!(after.report.b, read.report.b);
            assert_eq!(after.facts, read.facts, "header facts, A then B");
            assert!(after.facts.iter().all(Option::is_some));
            prev = read;
        }
    }
}
