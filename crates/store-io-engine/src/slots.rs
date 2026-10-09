//! Reading and writing A/B slot pairs through a platform queue.
//!
//! Every piece of store-io metadata (volume record, region table, region
//! headers, small objects) is an A/B slot pair. Reading a pair reads both
//! slots, validates each, and lets [`store_io_format::pair::decide`] pick the
//! winner; it never rewrites anything. Writing a slot encodes the whole slot
//! into one aligned buffer and writes it as one device I/O.

use store_io_buf::BufPool;
use store_io_core::error::{Error, ErrorContext, NotWrittenCause, Op};
use store_io_format::pair::{PairReport, decide};
use store_io_format::slot::{Expect, SlotFields, SlotIndex, SlotStatus, encode, validate};
use store_io_platform::{IoOp, Queue};

use crate::exec::{Lane, run};

/// Where a pair lives and what it must contain.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PairAt {
    pub(crate) volume: [u8; 16],
    pub(crate) object: u64,
    pub(crate) a: u64,
    pub(crate) b: u64,
    pub(crate) log2: u8,
}

impl PairAt {
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
}

fn pool_err(ctx: ErrorContext) -> Error {
    Error::NotWritten {
        cause: NotWrittenCause::PoolExhausted,
        ctx,
    }
}

/// Reads one slot; any read failure becomes [`SlotStatus::ReadError`].
fn read_one<Q: Queue>(
    q: &mut Lane<Q>,
    file: &Q::File,
    pool: &BufPool,
    at: &PairAt,
    i: SlotIndex,
) -> Result<(SlotStatus, Option<Vec<u8>>), Error> {
    let buf = pool
        .take(at.size())
        .map_err(|_| pool_err(ErrorContext::default()))?;
    let offset = at.offset(i);
    let done = run(q, IoOp::Read { file, offset, buf }).map_err(|(_b, raw)| match raw {
        Some(raw) => Error::Io {
            op: Op::Read,
            raw,
            ctx: ErrorContext::default(),
        },
        None => pool_err(ErrorContext::default()),
    })?;
    let Some(buf) = done.buf else {
        return Ok((SlotStatus::ReadError, None));
    };
    match done.result {
        Ok(n) if n == at.size() => {}
        _ => return Ok((SlotStatus::ReadError, None)),
    }
    let expect = Expect {
        volume_uuid: at.volume,
        object_id: at.object,
        slot_offset: offset,
        slot_index: i,
        log2_slot_size: at.log2,
    };
    let status = validate(buf.as_slice(), &expect);
    let payload = match status {
        SlotStatus::Valid(info) => Some(info.payload(buf.as_slice()).to_vec()),
        _ => None,
    };
    Ok((status, payload))
}

/// Reads and decides a pair. Never writes.
pub(crate) fn read_pair<Q: Queue>(
    q: &mut Lane<Q>,
    file: &Q::File,
    pool: &BufPool,
    at: &PairAt,
) -> Result<PairRead, Error> {
    let (a, pa) = read_one(q, file, pool, at, SlotIndex::A)?;
    let (b, pb) = read_one(q, file, pool, at, SlotIndex::B)?;
    let report = decide(a, b);
    let payload = match report.winner_info() {
        Some((SlotIndex::A, _)) => pa,
        Some((SlotIndex::B, _)) => pb,
        None => None,
    };
    Ok(PairRead { report, payload })
}

/// What to write into the next slot of a pair.
pub(crate) struct NextSlot<'a> {
    pub(crate) index: SlotIndex,
    pub(crate) generation: u64,
    pub(crate) owner_generation: u64,
    pub(crate) prev_header_crc: u32,
    pub(crate) payload: &'a [u8],
}

/// Writes one complete slot as one device I/O. Returns the new header CRC.
///
/// # Errors
///
/// `NotWritten` if nothing reached the device; `DurabilityUnknown` (the
/// caller poisons) for a failed or short write.
pub(crate) fn write_slot<Q: Queue>(
    q: &mut Lane<Q>,
    file: &Q::File,
    pool: &BufPool,
    at: &PairAt,
    next: &NextSlot<'_>,
    dsync: bool,
) -> Result<u32, Error> {
    let ctx = ErrorContext::default();
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
    };
    let crc = encode(buf.as_mut_slice(), &fields, next.payload).map_err(|_| Error::NotWritten {
        cause: NotWrittenCause::TooLarge,
        ctx,
    })?;
    let offset = at.offset(next.index);
    let done = run(
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
    match done.result {
        Ok(n) if n == at.size() => Ok(crc),
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
    })
}
