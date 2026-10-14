//! Provisioning regions: placement, fill, flush, verification, header and
//! the region-table flip that commits it.

use std::sync::{Arc, Mutex, PoisonError};

use store_io_core::error::{
    Error, ErrorContext, FirstCause, Named, NotWrittenCause, Op, ReserveTag,
};
use store_io_core::id::{Generation, RegionId};
use store_io_format::fill::fill_keyed_v1;
use store_io_format::meta::{
    FillPattern, REGION_META_LEN, RegionKind, RegionMeta, RegionName, RegionState, TableEntry,
    encode_table,
};
use store_io_platform::{IoOp, Platform};

use crate::exec::{Ran, run};
use crate::frontier::AppendFrontier;
use crate::gate::Gate;
use crate::slots::{PairRead, plan_next};
use crate::space::{Reservation, lock_acct, no_space};
use crate::store::*;

impl<P: Platform> Store<P>
where
    P::Queue: Send,
{
    /// Provisions a region of `data_size` bytes (rounded up to the block size)
    /// and returns its state, taking the space from `from` when given (else
    /// under tag 0, growing the container past every outstanding
    /// reservation).
    pub(crate) fn provision(
        &self,
        name: &str,
        kind: RegionKind,
        data_size: u64,
        from: Option<&mut Reservation<P>>,
    ) -> Result<Arc<LiveRegion>, Error> {
        let _timed = self
            .inner
            .observer
            .span(store_io_core::observe::Stage::Provision);
        let inner = &self.inner;
        let tag = from.as_ref().map_or(0, |r| r.tag());
        if inner.read_only {
            return Err(Error::NotWritten {
                cause: NotWrittenCause::ReadOnly,
                ctx: ErrorContext::default(),
            });
        }
        if let Some(first) = inner.domain.poisoned() {
            return Err(Error::Poisoned { first });
        }
        let rname = RegionName::new(name).map_err(|_| Error::NotWritten {
            cause: NotWrittenCause::InvalidName,
            ctx: ErrorContext::default(),
        })?;
        let block = inner.layout.block();
        let data_size =
            store_io_core::align::align_up(data_size, block).ok_or(Error::NotWritten {
                cause: NotWrittenCause::TooLarge,
                ctx: ErrorContext::default(),
            })?;
        if data_size / block >= u64::from(u32::MAX) {
            return Err(Error::NotWritten {
                cause: NotWrittenCause::TooLarge,
                ctx: ErrorContext::default(),
            });
        }
        let mut meta = lock_meta(&inner.meta);
        // Names are unique among the table's ready entries, including any
        // region this open could not load (its header failed to verify):
        // reusing such a name would make the table undecodable.
        if meta
            .entries
            .iter()
            .any(|e| e.state == RegionState::Ready && e.name == rname)
        {
            return Err(Error::AlreadyExists {
                what: Named::Region,
            });
        }
        let extent = inner.layout.extent(data_size).ok_or(Error::NotWritten {
            cause: NotWrittenCause::TooLarge,
            ctx: ErrorContext::default(),
        })?;
        // Space: a reservation must hold the extent; otherwise the tag's cap
        // must allow it.
        {
            let mut acct = lock_acct(&inner.acct);
            match &from {
                Some(r) if extent > r.remaining() => return Err(no_space(tag)),
                None if !acct.fits(tag, extent) => return Err(no_space(tag)),
                _ => {}
            }
        }
        // Placement. A large region's data area is aligned (see
        // `Layout::place`). A reservation pays only for the extent: the
        // alignment gap comes from growing the container past every
        // reservation, and when that growth finds no space the region is
        // placed unaligned instead, which needs no growth at all (the
        // container already holds the reservation), so a reservation's
        // guarantee always holds.
        let too_large = || Error::NotWritten {
            cause: NotWrittenCause::TooLarge,
            ctx: ErrorContext::default(),
        };
        let reserved_after = {
            let acct = lock_acct(&inner.acct);
            match &from {
                Some(_) => acct.reserved().saturating_sub(extent),
                None => acct.reserved(),
            }
        };
        let mut align = true;
        let (offset, reuse) = loop {
            let (offset, reuse, _grow) = inner
                .layout
                .place(&meta.entries, meta.container_len, extent, align)
                .ok_or_else(too_large)?;
            let end = offset.checked_add(extent).ok_or_else(too_large)?;
            // The new table must fit its slot. Released entries stay in the
            // table until their space is reused, so they count; a reused
            // entry is replaced in place. Checked before any I/O.
            if meta.entries.len() + usize::from(reuse.is_none()) > inner.layout.table_capacity() {
                return Err(Error::NoSpace {
                    tag: ReserveTag::Table,
                    ctx: ErrorContext::default(),
                });
            }
            let target = self
                .tail(&meta)
                .max(end)
                .checked_add(reserved_after)
                .ok_or_else(too_large)?;
            match self.grow_to(&mut meta, target, tag) {
                Ok(()) => break (offset, reuse),
                Err(Error::NoSpace { .. }) if align && from.is_some() => align = false,
                Err(e) => return Err(e),
            }
        };
        let id = meta.next_region_id;
        let next_id = id.checked_add(1).ok_or(Error::NoSpace {
            tag: ReserveTag::Table,
            ctx: ErrorContext::default(),
        })?;
        let fill = if inner.opts.keyed_fill {
            FillPattern::KeyedV1
        } else {
            FillPattern::Zeros
        };
        let fill_key = if fill == FillPattern::KeyedV1 {
            random_uuid()
        } else {
            [0; 16]
        };
        // 1. Fill the header blocks and the data area with direct writes,
        //    blanking any old header pair at this location.
        self.fill(offset, extent, block, fill, &fill_key)?;
        // 2. Make the fill and the size durable.
        self.flush_all_accounted()?;
        // 3. Verify the extent is really written and out of the page cache.
        self.verify_ready(offset, extent)?;
        // 4. Region header slot A, generation 1, Ready.
        let rmeta = RegionMeta {
            region_id: id,
            kind,
            state: RegionState::Ready,
            fill,
            data_offset: offset + 2 * block,
            data_size,
            fill_key,
        };
        let mut rbuf = [0u8; REGION_META_LEN];
        let _len = rmeta.encode(&mut rbuf).map_err(|_| meta_err())?;
        let entry = TableEntry {
            region_id: id,
            kind,
            state: RegionState::Ready,
            name: rname,
            offset,
            data_size,
            fill,
            tag,
        };
        let at = pair_region(inner.volume, &inner.layout, &entry);
        let blank = PairRead {
            report: store_io_format::pair::decide(
                store_io_format::slot::SlotStatus::Blank,
                store_io_format::slot::SlotStatus::Blank,
            ),
            payload: None,
            media_error: false,
        };
        let next = plan_next(&blank, &rbuf, inner.owner_generation)?;
        let header = {
            // `durable_slot` poisons exactly when durability became unknown;
            // an error before submission leaves the store usable.
            let mut q = inner.queues.get(id as usize);
            durable_slot::<P>(
                &mut q,
                &inner.file,
                &inner.pool,
                &at,
                &blank,
                &next,
                &inner.policy,
                &inner.domain,
            )
            .inspect_err(|_| self.wake_frontiers())?
        };
        // 5. Table flip adding the entry (replacing a reused released entry).
        let mut entries = meta.entries.clone();
        match reuse {
            Some(i) => entries[i] = entry,
            None => entries.push(entry),
        }
        self.flip_table(&mut meta, next_id, &entries)?;
        meta.entries = entries;
        meta.next_region_id = next_id;
        if let Some(r) = from {
            // Checked above against `remaining`, under the same `meta` lock.
            r.consume(extent)?;
        }
        lock_acct(&inner.acct).add(&entry, block);
        let state = Arc::new(LiveRegion {
            id: RegionId::from_raw(id),
            name: rname,
            kind,
            generation: Generation::FIRST,
            data_offset: offset + 2 * block,
            data_size,
            header: Mutex::new(header),
            frontier: (kind == RegionKind::Append).then(|| {
                AppendFrontier::new(
                    0,
                    data_size,
                    u32::from(inner.layout.log2_block),
                    inner.opts.append_ring,
                )
            }),
            positioned: std::sync::atomic::AtomicBool::new(true),
            gate: Gate::new(),
        });
        inner
            .regions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Arc::clone(&state));
        Ok(state)
    }

    fn fill(
        &self,
        offset: u64,
        extent: u64,
        block: u64,
        fill: FillPattern,
        key: &[u8; 16],
    ) -> Result<(), Error> {
        let inner = &self.inner;
        let chunk = (self.chunk() as u64).min(1 << 20).max(block);
        let mut q = inner.queues.get(0);
        let mut at = 0u64;
        while at < extent {
            let len = chunk.min(extent - at);
            let mut buf = self.take(len as usize, ErrorContext::default())?;
            // The two header blocks are always zero so no old header
            // survives; the data area gets the pattern from its first byte,
            // even inside a chunk that also covers the headers.
            let dst = buf.as_mut_slice();
            let zeros = (2 * block).saturating_sub(at).min(len) as usize;
            dst[..zeros].fill(0);
            match fill {
                FillPattern::KeyedV1 => {
                    fill_keyed_v1(key, at + zeros as u64 - 2 * block, &mut dst[zeros..]);
                }
                FillPattern::Zeros => dst[zeros..].fill(0),
            }
            let ran = run(
                &mut q,
                IoOp::Write {
                    file: &inner.file,
                    offset: offset + at,
                    buf,
                    dsync: false,
                },
            )
            .map_err(|(_, raw)| {
                raw.map_or(
                    Error::NotWritten {
                        cause: NotWrittenCause::QueueFull,
                        ctx: ErrorContext::default(),
                    },
                    |r| space_err(Op::Write, r, 0),
                )
            })?;
            // A fill write that failed after submission stops the store
            // (fail-stop, D-071); only running out of space is reported as
            // `NoSpace` without poisoning: the region never became Ready.
            // An abandoned write (its wait failed) is never "out of space":
            // its fate is unknown, so it poisons like any other failure.
            let (result, abandoned) = match ran {
                Ran::Done { result, .. } => (result, false),
                Ran::Abandoned(raw) => (Err(raw), true),
            };
            match result {
                Ok(n) if n as u64 == len => {}
                Err(raw) if !abandoned && store_io_core::errno::is_no_space(raw) => {
                    return Err(space_err(Op::Write, raw, 0));
                }
                result => {
                    let raw = result.err();
                    inner.domain.poison(FirstCause {
                        op: Some(Op::Write),
                        raw,
                    });
                    self.wake_frontiers();
                    return Err(Error::DurabilityUnknown {
                        op: Op::Write,
                        raw,
                        ctx: ErrorContext::default(),
                    });
                }
            }
            at += len;
        }
        Ok(())
    }

    fn flush_all_accounted(&self) -> Result<(), Error> {
        let inner = &self.inner;
        inner.platform.flush_all(&inner.file).map_err(|raw| {
            inner.domain.poison(FirstCause {
                op: Some(Op::FlushAll),
                raw: Some(raw),
            });
            Error::DurabilityUnknown {
                op: Op::FlushAll,
                raw: Some(raw),
                ctx: ErrorContext::default(),
            }
        })
    }

    fn verify_ready(&self, offset: u64, extent: u64) -> Result<(), Error> {
        let inner = &self.inner;
        let st = inner
            .platform
            .range_state(&inner.file, offset, extent)
            .map_err(|e| io(Op::Probe, e))?;
        use store_io_core::evidence::Tri;
        let bad = st.unwritten == Tri::Yes
            || st.shared == Tri::Yes
            || st.cached_pages.is_some_and(|n| n > 0)
            || st.valid_data == Tri::No;
        if bad {
            return Err(Error::NotWritten {
                cause: NotWrittenCause::NotReady,
                ctx: ErrorContext::default(),
            });
        }
        Ok(())
    }

    pub(crate) fn flip_table(
        &self,
        meta: &mut Meta,
        next_id: u32,
        entries: &[TableEntry],
    ) -> Result<(), Error> {
        let inner = &self.inner;
        // The payload room of the table slot: the slot less its header.
        let mut tbuf = vec![0u8; inner.layout.table() as usize - store_io_format::slot::HEADER_LEN];
        let len = encode_table(&mut tbuf, next_id, entries).map_err(|_| Error::NoSpace {
            tag: ReserveTag::Table,
            ctx: ErrorContext::default(),
        })?;
        let tat = pair_table(inner.volume, &inner.layout);
        let next = plan_next(&meta.table_pair, &tbuf[..len], inner.owner_generation)?;
        let mut q = inner.queues.get(1);
        // `durable_slot` poisons exactly when durability became unknown.
        meta.table_pair = durable_slot::<P>(
            &mut q,
            &inner.file,
            &inner.pool,
            &tat,
            &meta.table_pair,
            &next,
            &inner.policy,
            &inner.domain,
        )
        .inspect_err(|_| self.wake_frontiers())?;
        Ok(())
    }
}
