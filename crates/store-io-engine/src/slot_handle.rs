//! The [`Slot`] handle: a small object committed atomically (one block
//! write plus one barrier per commit; the slot pair protocol is in
//! `slots.rs`).

use std::sync::{Arc, PoisonError};

use store_io_core::error::{CorruptionKind, Error, NotWrittenCause, Op};
use store_io_format::meta::{REGION_META_LEN, RegionMeta, RegionState as Lifecycle};
use store_io_format::slot::HEADER_LEN;
use store_io_platform::Platform;

use crate::io::ctx;
use crate::receipt::DurableReceipt;
use crate::slots::{PairAt, plan_next};
use crate::store::{LiveRegion, Store, durable_slot};

/// A small object (a manifest, a superblock) committed atomically: after any
/// crash it reads back as exactly the old or exactly the new version.
///
/// One commit is one block write plus one barrier. Commits to one slot are
/// serialised (at most one slot write in flight).
pub struct Slot<P: Platform> {
    store: Store<P>,
    state: Arc<LiveRegion>,
}

impl<P: Platform> Slot<P> {
    pub(crate) fn from_parts(store: Store<P>, state: Arc<LiveRegion>) -> Self {
        Self { store, state }
    }
}

impl<P: Platform> Clone for Slot<P> {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            state: Arc::clone(&self.state),
        }
    }
}

impl<P: Platform> Slot<P>
where
    P::Queue: Send,
{
    pub(crate) fn state(&self) -> &Arc<LiveRegion> {
        &self.state
    }

    /// Largest payload a commit accepts.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.store.inner.layout.block() as usize - HEADER_LEN - REGION_META_LEN
    }

    fn at(&self) -> PairAt {
        let l = &self.store.inner.layout;
        let a = self.state.data_offset - 2 * l.block();
        PairAt {
            volume: *self.store.inner.volume.as_bytes(),
            object: crate::layout::region_object(self.state.id.get()),
            a,
            b: a + l.block(),
            log2: l.log2_block,
        }
    }

    /// The current committed object, or `None` if never committed.
    ///
    /// Generation 1 of the slot pair is the provisioning header; commits start
    /// at generation 2.
    #[must_use]
    pub fn read(&self) -> Option<Vec<u8>> {
        let h = self
            .state
            .header
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let (_, info) = h.report.winner_info()?;
        if info.generation <= 1 {
            return None;
        }
        h.payload
            .as_ref()
            .and_then(|p| p.get(REGION_META_LEN..))
            .map(<[u8]>::to_vec)
    }

    /// Commits a new version of the object.
    ///
    /// # Errors
    ///
    /// `NotWritten(TooLarge)` beyond [`Self::capacity`], `Io` if the store's
    /// queue could not be prepared (nothing was written), `Poisoned`, or
    /// `DurabilityUnknown` (the domain is now poisoned).
    pub fn commit(&self, data: &[u8]) -> Result<DurableReceipt, Error> {
        let s = &self.store;
        s.ready()?;
        let _timed = s
            .inner
            .observer
            .span(store_io_core::observe::Stage::SlotCommit);
        let c = ctx(&self.state);
        if data.len() > self.capacity() {
            return Err(Error::NotWritten {
                cause: NotWrittenCause::TooLarge,
                ctx: c,
            });
        }
        let mut header = self
            .state
            .header
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let meta_bytes = header
            .payload
            .as_ref()
            .and_then(|p| p.get(..REGION_META_LEN))
            .map(<[u8]>::to_vec)
            .ok_or(Error::Corruption {
                kind: CorruptionKind::Metadata,
                ctx: c,
            })?;
        let rm = RegionMeta::decode(&meta_bytes).map_err(|_| Error::Corruption {
            kind: CorruptionKind::Metadata,
            ctx: c,
        })?;
        if rm.state != Lifecycle::Ready {
            return Err(Error::NotWritten {
                cause: NotWrittenCause::NotReady,
                ctx: c,
            });
        }
        let mut payload = Vec::with_capacity(REGION_META_LEN + data.len());
        payload.extend_from_slice(&meta_bytes);
        payload.extend_from_slice(data);
        let next = plan_next(&header, &payload, s.inner.owner_generation)?;
        let generation = next.generation;
        let at = self.at();
        let mut lane = s.lane(&self.state, Op::Write)?;
        // A forced slot flushes on a power-safe store too (D-102).
        let flushed = s.inner.policy.flush || self.state.forced();
        let policy = crate::store::Policy {
            flush: flushed,
            ..s.inner.policy
        };
        let written = durable_slot::<P>(
            &mut lane,
            &s.inner.file,
            &s.inner.pool,
            &at,
            &header,
            &next,
            &policy,
            &s.inner.domain,
        )
        .inspect_err(|_| s.wake_frontiers())?;
        *header = written;
        // A slot has no tickets: its receipt covers the commit alone.
        let mut receipt = s.receipt(&self.state, 0, data.len() as u64, 0, false, flushed);
        if let Some(g) = store_io_core::id::Generation::from_raw(generation) {
            receipt.generation = g;
        }
        Ok(receipt)
    }
}

impl<P: Platform> core::fmt::Debug for Slot<P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("Slot").field(&*self.state).finish()
    }
}
