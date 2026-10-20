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
use store_io_core::evidence::PlatformKind;
use store_io_core::id::Generation;
use store_io_format::meta::{REGION_META_LEN, RegionKind, RegionMeta, RegionState};
use store_io_platform::{Platform, ReleaseHow};

use crate::frontier::AppendFrontier;
use crate::gate::{Gate, Retire, Retired};
use crate::io::{ctx, not_written};
use crate::layout::region_object;
use crate::slots::{PairAt, plan_next};
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
        let next = plan_next(&header, &payload, inner.owner_generation)?;
        // Converted before the write, so nothing can fail after it.
        let generation = Generation::from_raw(next.generation).ok_or_else(|| metadata(c))?;
        let at = self.header_at(r);
        let mut lane = self.lane(r, Op::Write)?;
        let written = durable_slot::<P>(
            &mut lane,
            &inner.file,
            &inner.pool,
            &at,
            &header,
            &next,
            &inner.policy,
            &inner.domain,
        )
        .inspect_err(|_| self.wake_frontiers())?;
        *header = written;
        Ok(generation)
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
                // which case the domain is poisoned and stays that way; any
                // other failure (refused, unreadable, or the lane could not
                // be cleared) leaves the region as it was, open again.
                if self.inner.domain.poisoned().is_none() {
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
                    crate::frontier::ring_for(
                        self.inner.opts.append_window,
                        r.data_size,
                        u32::from(self.inner.layout.log2_block),
                    ),
                )
            }),
            positioned: std::sync::atomic::AtomicBool::new(true),
            gate: Gate::new(),
            // A runtime setting of the region, shared with its next
            // generation.
            force_sync: Arc::clone(&r.force_sync),
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
        // Retired (waiting out every operation in flight) before the
        // metadata lock is taken: a `Submitter` operation holds its pass
        // until its thread processes the completion, and that thread may
        // first call something that takes the metadata lock (provisioning,
        // `space`). Holding the lock while waiting would deadlock.
        self.retire(r, Retire::Release)?;
        let mut meta = lock_meta(&inner.meta);
        // 1. The table: the only authority on open.
        let mut entries = meta.entries.clone();
        let Some(entry) = entries.iter_mut().find(|e| e.region_id == r.id.get()) else {
            r.gate.restore();
            return Err(metadata(c));
        };
        entry.state = RegionState::Released;
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
        // The metadata lock stays held through the header write and the
        // deallocation (it is released on return): the table already names
        // the extent released, so a provisioning that ran in between could
        // reuse it, and this release's late writes would then land in the
        // new region (its header slot, and a hole punched through its data).
        // 2. The header, for tools that list regions from headers.
        let _generation = self.rewrite_header(r, RegionState::Released)?;
        // 3. The data area (never the header, which must keep saying
        //    "released"). Contents are undefined from here; provisioning
        //    refills the range before any reuse.
        //
        //    Linux punches a hole. Windows trims in place: deallocating would
        //    make the whole container sparse for good, and a sparse file may
        //    allocate on overwrite, which costs the store its power-safe
        //    class. A trim the device or file system does not support only
        //    means the space stays as it was, so it is not an error there.
        let windows = inner.report.evidence.platform == PlatformKind::Windows;
        let how = if windows {
            ReleaseHow::TrimInPlace
        } else {
            ReleaseHow::Deallocate
        };
        let released_range =
            inner
                .platform
                .release_range(&inner.file, r.data_offset, r.data_size, how);
        // The metadata lock is released on return.
        match released_range {
            Ok(()) => Ok(()),
            Err(_) if windows => Ok(()),
            Err(raw) => Err(Error::Io {
                op: Op::Release,
                raw,
                ctx: c,
            }),
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use store_io_sim::{SimConfig, SimPlatform};

    use crate::store::{Store, StoreOptions};

    /// A release keeps the metadata lock until its region's space is
    /// deallocated. Without it a provisioning could reuse the extent the
    /// table already names released, and the release's late header write and
    /// deallocation would land in the new region. Checked deterministically:
    /// the deallocation runs on the releasing thread, where `try_lock`
    /// reports the lock as held.
    #[test]
    fn test_a_release_holds_the_metadata_lock_until_its_space_is_deallocated() {
        let p = SimPlatform::new(SimConfig::volatile(7));
        let s = Store::create(p.clone(), Path::new("/db"), StoreOptions::default()).unwrap();
        let pages = s.provision_page_region("old", 1 << 20).unwrap();
        let (held, calls) = (Arc::new(AtomicBool::new(true)), Arc::new(AtomicU64::new(0)));
        {
            let (inner, held, calls) =
                (Arc::clone(&s.inner), Arc::clone(&held), Arc::clone(&calls));
            p.with_world(|w| {
                w.set_release_probe(Some(Arc::new(move || {
                    let _was = calls.fetch_add(1, Ordering::Relaxed);
                    if inner.meta.try_lock().is_ok() {
                        held.store(false, Ordering::Relaxed);
                    }
                })));
            });
        }
        pages.release().unwrap();
        p.with_world(|w| w.set_release_probe(None));
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "the space was not deallocated once"
        );
        assert!(
            held.load(Ordering::Relaxed),
            "the space was deallocated without the metadata lock: a provisioning could have reused it"
        );
    }

    /// A region provisioned into the space of one just released keeps every
    /// byte it was told is durable, across a reopen.
    #[test]
    fn test_a_region_reusing_released_space_keeps_its_data() {
        let p = SimPlatform::new(SimConfig::volatile(8));
        let s = Store::create(p.clone(), Path::new("/db"), StoreOptions::default()).unwrap();
        let old = s.provision_page_region("old", 1 << 20).unwrap();
        let _r = old
            .write_durable(old.pos(0).unwrap(), &[1u8; 4096])
            .unwrap();
        old.release().unwrap();
        let new = s.provision_page_region("new", 1 << 20).unwrap();
        // The new region did reuse the released extent (else this test would
        // prove nothing about reuse).
        assert_eq!(s.space().released, 0, "the released extent was not reused");
        let _r = new
            .write_durable(new.pos(0).unwrap(), &[2u8; 4096])
            .unwrap();
        drop((new, s));
        let s = Store::open(p, Path::new("/db"), StoreOptions::default()).unwrap();
        let new = s.page_region("new").unwrap();
        let mut out = [0u8; 4096];
        assert_eq!(new.read(new.pos(0).unwrap(), &mut out).unwrap(), 4096);
        assert!(
            out.iter().all(|&b| b == 2),
            "the new region lost its durable bytes"
        );
    }
}
