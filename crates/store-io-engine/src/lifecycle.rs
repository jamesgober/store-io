//! Recycle and release: the caller-invoked region mutators.
//!
//! - **Recycle** keeps a region's blocks and starts a new generation: the
//!   region is retired (no new operation is admitted and every operation in
//!   flight is waited for), its header is rewritten with the next generation
//!   and made durable, and only then is the new generation live. It does no
//!   other I/O: the old bytes stay on disk, so callers embed the generation
//!   in their records to tell old from new after a crash.
//! - **Release** gives a region's space back: the region is retired, the
//!   region table marks it released (durable), its header says so too, and
//!   the range is deallocated. The table is flipped first because it is the
//!   only authority on open: a crash between the two steps leaves a released
//!   entry whose stale header is ignored, never a ready entry whose region can
//!   no longer be opened (which would leak its space).
//!
//! Neither runs implicitly: nothing in open, scan or drop recycles or
//! releases anything.

use std::sync::{Arc, PoisonError};

use store_io_core::error::{CorruptionKind, Error, ErrorContext, NotWrittenCause, Op};
use store_io_core::id::Generation;
use store_io_format::meta::{REGION_META_LEN, RegionKind, RegionMeta, RegionState};
use store_io_platform::{Platform, ReleaseHow};

use crate::frontier::AppendFrontier;
use crate::gate::{Gate, Retire, Retired};
use crate::io::{ctx, not_written};
use crate::layout::region_object;
use crate::slots::{PairAt, plan_next, read_pair};
use crate::store::{LiveRegion, Store, durable_slot, lock_meta};

/// The error for an operation on a retired region.
pub(crate) fn retired_error(why: Retired, r: &LiveRegion) -> Error {
    not_written(
        match why {
            Retired::Recycled => NotWrittenCause::StaleGeneration,
            Retired::Released => NotWrittenCause::NotReady,
        },
        ctx(r),
    )
}

fn metadata(c: ErrorContext) -> Error {
    Error::Corruption {
        kind: CorruptionKind::Metadata,
        ctx: c,
    }
}

impl<P: Platform> Store<P>
where
    P::Queue: Send,
{
    /// Where `r`'s header pair lives.
    pub(crate) fn header_at(&self, r: &LiveRegion) -> PairAt {
        let l = &self.inner.layout;
        let a = r.data_offset - 2 * l.block();
        PairAt {
            volume: *self.inner.volume.as_bytes(),
            object: region_object(r.id.get()),
            a,
            b: a + l.block(),
            log2: l.log2_block,
        }
    }

    /// Writes the next generation of `r`'s header with `state` and makes it
    /// durable. Returns the new generation.
    fn rewrite_header(&self, r: &LiveRegion, state: RegionState) -> Result<Generation, Error> {
        let inner = &self.inner;
        let c = ctx(r);
        let mut header = r.header.lock().unwrap_or_else(PoisonError::into_inner);
        let meta_bytes = header
            .payload
            .as_ref()
            .and_then(|p| p.get(..REGION_META_LEN))
            .ok_or_else(|| metadata(c))?;
        let mut rm = RegionMeta::decode(meta_bytes).map_err(|_| metadata(c))?;
        rm.state = state;
        let mut payload = [0u8; REGION_META_LEN];
        let _len = rm.encode(&mut payload).map_err(|_| metadata(c))?;
        let next =
            plan_next(&header, &payload, inner.owner_generation).ok_or(Error::Corruption {
                kind: CorruptionKind::Fork,
                ctx: c,
            })?;
        let at = self.header_at(r);
        let mut lane = self.lane(r);
        durable_slot::<P>(
            &mut lane,
            &inner.file,
            &inner.pool,
            &at,
            &next,
            &inner.policy,
            &inner.domain,
        )
        .inspect_err(|_| self.wake_frontiers())?;
        *header = read_pair(&mut lane, &inner.file, &inner.pool, &at)?;
        Generation::from_raw(next.generation).ok_or_else(|| metadata(c))
    }

    /// Retires `r` for `how`, waiting out every operation in flight.
    fn retire(&self, r: &LiveRegion, how: Retire) -> Result<(), Error> {
        self.ready()?;
        if r.kind == RegionKind::Slot {
            return Err(Error::Unsupported {
                what: store_io_core::error::Capability::Platform,
            });
        }
        r.gate.retire(how).map_err(|why| retired_error(why, r))
    }

    /// Starts a new generation of `r` and returns its state.
    pub(crate) fn recycle_region(&self, r: &Arc<LiveRegion>) -> Result<Arc<LiveRegion>, Error> {
        self.retire(r, Retire::Recycle)?;
        let generation = match self.rewrite_header(r, RegionState::Ready) {
            Ok(g) => g,
            Err(e) => {
                // Nothing changed on disk unless the write was submitted, in
                // which case the domain is poisoned and stays that way.
                if matches!(e, Error::NotWritten { .. } | Error::Corruption { .. }) {
                    r.gate.restore();
                }
                return Err(e);
            }
        };
        let header = r
            .header
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let fresh = Arc::new(LiveRegion {
            id: r.id,
            name: r.name,
            kind: r.kind,
            generation,
            data_offset: r.data_offset,
            data_size: r.data_size,
            header: std::sync::Mutex::new(header),
            frontier: (r.kind == RegionKind::Append).then(|| {
                AppendFrontier::new(
                    0,
                    r.data_size,
                    u32::from(self.inner.layout.log2_block),
                    self.inner.opts.append_ring,
                )
            }),
            positioned: std::sync::atomic::AtomicBool::new(true),
            gate: Gate::new(),
        });
        let mut regions = self
            .inner
            .regions
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(slot) = regions.iter_mut().find(|x| x.id == r.id) {
            *slot = Arc::clone(&fresh);
        }
        Ok(fresh)
    }

    /// Releases `r`: table entry, header, then the space.
    pub(crate) fn release_region(&self, r: &Arc<LiveRegion>) -> Result<(), Error> {
        let inner = &self.inner;
        let c = ctx(r);
        let mut meta = lock_meta(&inner.meta);
        self.retire(r, Retire::Release)?;
        // 1. The table: the only authority on open.
        let mut entries = meta.entries.clone();
        let Some(entry) = entries.iter_mut().find(|e| e.region_id == r.id.get()) else {
            r.gate.restore();
            return Err(metadata(c));
        };
        entry.state = RegionState::Released;
        let offset = entry.offset;
        let released = *entry;
        let next_id = meta.next_region_id;
        if let Err(e) = self.flip_table(&mut meta, next_id, &entries) {
            if matches!(e, Error::NotWritten { .. } | Error::NoSpace { .. }) {
                r.gate.restore();
            }
            return Err(e);
        }
        meta.entries = entries;
        crate::space::lock_acct(&inner.acct).remove(&released, inner.layout.block());
        inner
            .regions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|x| x.id != r.id);
        drop(meta);
        // 2. The header, for tools that list regions from headers.
        let _generation = self.rewrite_header(r, RegionState::Released)?;
        // 3. The space. Contents are undefined from here; provisioning
        //    refills the range before any reuse.
        let extent = inner
            .layout
            .extent(r.data_size)
            .ok_or_else(|| metadata(c))?;
        inner
            .platform
            .release_range(&inner.file, offset, extent, ReleaseHow::Deallocate)
            .map_err(|raw| Error::Io {
                op: Op::Release,
                raw,
                ctx: c,
            })
    }
}
