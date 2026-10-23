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
        PairAt {
            volume: *self.inner.volume.as_bytes(),
            object: l.region_object(r.id.get()),
            a: r.header_at[0],
            b: r.header_at[1],
            log2: l.log2_block,
            format: l.format,
            numbered: false,
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
    pub(crate) fn retire(&self, r: &LiveRegion, how: Retire) -> Result<(), Error> {
        self.ready_for(r)?;
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
        let rewritten = if self.inner.layout.format == store_io_format::slot::FORMAT_2 {
            self.recycle_header2(r)
        } else {
            self.rewrite_header(r, RegionState::Ready)
        };
        let generation = match rewritten {
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
        // Format 2: a region that may grow has its ring sized by the window.
        let reach = if self.inner.layout.format == store_io_format::slot::FORMAT_2 {
            let flags = header
                .payload
                .as_deref()
                .and_then(|b| {
                    store_io_format::meta2::decode_region_header(
                        b,
                        self.inner.layout.log2_block,
                        u64::MAX,
                    )
                    .ok()
                })
                .map_or(0, |v| v.header.flags);
            crate::frontier::ring_reach(r.map.capacity(), flags)
        } else {
            r.map.capacity()
        };
        let fresh = Arc::new(LiveRegion {
            id: r.id,
            name: r.name,
            kind: r.kind,
            generation,
            map: r.map.clone(),
            header_at: r.header_at,
            header: std::sync::Mutex::new(header),
            frontier: (r.kind == RegionKind::Append).then(|| {
                AppendFrontier::new(
                    0,
                    r.map.len(),
                    u32::from(self.inner.layout.log2_block),
                    crate::frontier::ring_for(
                        self.inner.opts.append_window,
                        reach,
                        u32::from(self.inner.layout.log2_block),
                    ),
                )
            }),
            positioned: std::sync::atomic::AtomicBool::new(true),
            gate: Gate::new(),
            // A runtime setting of the region, shared with its next
            // generation.
            force_sync: Arc::clone(&r.force_sync),
            read_only: r.read_only,
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
        if self.inner.layout.format == store_io_format::slot::FORMAT_2 {
            return self.release_region2(r);
        }
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
        let Some(entry) = entries
            .iter_mut()
            .find(|e| u64::from(e.region_id) == r.id.get())
        else {
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
        // 3. The data area. The metadata lock is released on return.
        self.release_extents(r.map.extents(), c)
    }

    /// Deallocates a released region's data extents (never its header
    /// blocks, which must keep saying "released"). Contents are undefined
    /// from here; provisioning refills a range before any reuse.
    ///
    /// Linux punches a hole. Windows trims in place: deallocating would make
    /// the whole container sparse for good, and a sparse file may allocate
    /// on overwrite, which costs the store its power-safe class. A trim the
    /// device or file system does not support only means the space stays as
    /// it was, so it is not an error there.
    ///
    /// The release is committed before this, so a failure only means space
    /// stays allocated: every extent is tried, also after one fails, and
    /// elsewhere than Windows the first failure is returned after the last.
    pub(crate) fn release_extents(
        &self,
        extents: impl IntoIterator<Item = (u64, u64)>,
        c: ErrorContext,
    ) -> Result<(), Error> {
        let inner = &*self.inner;
        let windows = inner.report.evidence.platform == PlatformKind::Windows;
        let how = if windows {
            ReleaseHow::TrimInPlace
        } else {
            ReleaseHow::Deallocate
        };
        let mut first = None;
        for (at, len) in extents.into_iter().filter(|x| x.1 > 0) {
            if let Err(raw) = inner.platform.release_range(&inner.file, at, len, how) {
                first = first.or(Some(raw));
            }
        }
        match first {
            None => Ok(()),
            Some(_) if windows => Ok(()),
            Some(raw) => Err(Error::Io {
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

    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use store_io_sim::{SimConfig, SimPlatform};

    use super::*;
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

    /// A page region of two blocks on `p` and its two blocks as extents (the
    /// release of a region of several extents arrives with growth).
    fn two_extents(
        p: &SimPlatform,
        opts: StoreOptions,
    ) -> (Store<SimPlatform>, [(u64, u64); 2], u32) {
        let s = Store::create(p.clone(), Path::new("/db"), opts).unwrap();
        let _pages = s.provision_page_region("two", 8192).unwrap();
        let r = s
            .regions_snapshot()
            .into_iter()
            .find(|r| r.name.as_str() == "two")
            .unwrap();
        let (at, len) = r.map.extents().next().unwrap();
        assert_eq!(len, 8192);
        let file = p.with_world(|w| w.file_id(&PathBuf::from("/db"), crate::CONTAINER).unwrap());
        (s, [(at, 4096), (at + 4096, 4096)], file)
    }

    /// Whether the simulator's release pattern covers `[at, at + 4096)`.
    fn released(p: &SimPlatform, file: u32, at: u64) -> bool {
        p.with_world(|w| {
            let v = w.visible(file).unwrap();
            v[at as usize..at as usize + 4096]
                .iter()
                .enumerate()
                .all(|(i, &b)| b == 0xDE ^ (i as u8))
        })
    }

    /// A release tries every extent: one that fails leaves the others
    /// deallocated, and (off Windows) the first failure is returned after
    /// the last extent was tried.
    #[test]
    fn test_every_extent_is_released_when_one_fails() {
        let p = SimPlatform::new(SimConfig::volatile(9));
        let (s, extents, file) = two_extents(&p, StoreOptions::default());
        let ctx = store_io_core::error::ErrorContext::default();
        let before = p.with_world(|w| w.releases());
        p.with_world(|w| w.faults_mut().fail_release = Some((before + 1, 5)));
        let r = s.release_extents(extents, ctx);
        assert!(
            matches!(r, Err(Error::Io { op: Op::Release, raw, .. }) if raw.code == 5),
            "{r:?}"
        );
        assert_eq!(p.with_world(|w| w.releases()) - before, 2, "both tried");
        assert!(!released(&p, file, extents[0].0), "the failed one stays");
        assert!(released(&p, file, extents[1].0), "the second is released");
        // Without a failure: both, and Ok.
        p.with_world(|w| w.faults_mut().fail_release = None);
        s.release_extents(extents, ctx).unwrap();
        assert!(released(&p, file, extents[0].0));
    }

    /// On Windows a trim is advisory: a failed one is no error, and the
    /// other extents are still trimmed.
    #[test]
    fn test_a_failed_trim_on_windows_is_no_error_and_the_rest_are_trimmed() {
        let probe = SimPlatform::new(SimConfig::volatile(10));
        let mut evidence = Store::create(probe, Path::new("/db"), StoreOptions::default())
            .unwrap()
            .report()
            .evidence
            .clone();
        evidence.platform = PlatformKind::Windows;
        let p = SimPlatform::new(SimConfig {
            evidence: Some(evidence),
            ..SimConfig::volatile(10)
        });
        let mut opts = StoreOptions::default();
        // Borrowed evidence: not verifiable.
        opts.trust.override_refusal = true;
        let (s, extents, file) = two_extents(&p, opts);
        let before = p.with_world(|w| w.releases());
        p.with_world(|w| w.faults_mut().fail_release = Some((before + 1, 5)));
        s.release_extents(extents, store_io_core::error::ErrorContext::default())
            .unwrap();
        assert_eq!(p.with_world(|w| w.releases()) - before, 2, "both tried");
        assert!(released(&p, file, extents[1].0));
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
