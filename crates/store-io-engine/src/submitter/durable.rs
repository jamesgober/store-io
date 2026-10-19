//! Durable submitter writes and barriers (D-099): a flush request per
//! operation, answered at once when a flush already covers it, else by the
//! store's flush thread (or on this thread if it could not be started).

use std::sync::Arc;

use store_io_core::error::{Error, ErrorContext, NotWrittenCause};
use store_io_platform::{Platform, Queue};

use crate::completion::Rejected;
use crate::flusher::{Flushed, Inbox};
use crate::io::not_written;
use crate::optable::{Kind, Table};
use crate::receipt::WriteTicket;
use crate::store::Store;

use super::Submitter;

/// Asks for a flush `>= need` for the row `qtag` names (`flush`: a real
/// flush is wanted; else, on a power-safe store, the writes are durable
/// already). Answered here when no flush is wanted, the store is poisoned,
/// or a flush already covers it (no thread hop), or when the flush thread is
/// unavailable (then the flush runs on this thread); else by the flush
/// thread, through `inbox`.
pub(super) fn ask<P: Platform>(
    table: &mut Table,
    store: &Store<P>,
    inbox: &Arc<Inbox<<P::Queue as Queue>::Waker>>,
    qtag: u64,
    need: u64,
    flush: bool,
) where
    P::Queue: Send,
{
    let domain = &store.inner.domain;
    let now: Option<Flushed> = if !flush {
        Some(match domain.poisoned() {
            Some(first) => Err(Error::Poisoned { first }),
            None => Ok(u64::MAX),
        })
    } else if domain.join_if_done(need) {
        Some(Ok(need))
    } else if let Some(first) = domain.poisoned() {
        Some(Err(Error::Poisoned { first }))
    } else if store
        .flusher()
        .is_some_and(|f| f.request(need, qtag, inbox))
    {
        table.asked += 1;
        None
    } else {
        // No flush thread (the OS refused it): the flush runs here, the one
        // case where a submitter call blocks.
        Some(store.barrier_to(need).map(|()| need))
    };
    if let Some(outcome) = now {
        table.flushed(store, qtag, outcome);
    }
}

impl<P: Platform> Submitter<P>
where
    P::Queue: Send,
{
    /// A barrier for one write: completes (`Done::Synced`) with a receipt
    /// for the ticket's range (not a durable prefix: see
    /// [`DurableReceipt::is_prefix`](crate::DurableReceipt::is_prefix)) once
    /// a flush issued after that write completed has succeeded (at once on a
    /// power-safe store unless the region forces data sync, or when a
    /// finished flush already covers it). It does not block (unless the
    /// store's flush thread could not be started: then the flush runs on
    /// this thread).
    ///
    /// # Errors
    ///
    /// [`Rejected`] (no buffer): `NotWritten(ForeignPosition)` for a ticket
    /// of another store or open, `NotWritten(StaleGeneration | NotReady)` for
    /// a region since recycled or released, `NotWritten(QueueFull |
    /// ReadOnly)`.
    pub fn barrier(&mut self, ticket: &WriteTicket, tag: u64) -> Result<(), Rejected> {
        let c = ErrorContext {
            region: Some(ticket.region),
            range: None,
        };
        let refuse = |cause| Rejected {
            error: not_written(cause, c),
            buf: None,
        };
        let inner = &self.store.inner;
        if inner.read_only {
            return Err(refuse(NotWrittenCause::ReadOnly));
        }
        if ticket.epoch != inner.epoch || ticket.volume != inner.volume {
            return Err(refuse(NotWrittenCause::ForeignPosition));
        }
        let current = inner
            .regions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|r| r.id == ticket.region)
            .map(|r| (r.generation, r.gate.check().is_ok(), r.forced()));
        let forced = match current {
            None => return Err(refuse(NotWrittenCause::NotReady)),
            Some((g, live, _)) if g != ticket.generation || !live => {
                return Err(refuse(NotWrittenCause::StaleGeneration));
            }
            Some((_, _, forced)) => forced,
        };
        let receipt = self.store.receipt_for(ticket, 0);
        let flush = inner.policy.flush || forced;
        self.start_barrier(Kind::Sync(receipt), ticket.need, flush, tag)
            .map_err(refuse)
    }

    /// A barrier for the whole store: completes (`Done::Flushed`) once every
    /// write that completed in the store before this call is durable. It
    /// does not block (as [`Self::barrier`]).
    ///
    /// # Errors
    ///
    /// [`Rejected`] (no buffer): `NotWritten(QueueFull | ReadOnly)`.
    pub fn barrier_all(&mut self, tag: u64) -> Result<(), Rejected> {
        if self.store.inner.read_only {
            // Nothing to flush, and a read-only handle cannot flush.
            return Err(Rejected {
                error: not_written(NotWrittenCause::ReadOnly, ErrorContext::default()),
                buf: None,
            });
        }
        let inner = &self.store.inner;
        let need = inner.domain.ticket();
        // On a power-safe store, a flush only if a region forces data sync.
        let flush = inner.policy.flush
            || inner
                .regions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .any(|r| r.forced());
        self.start_barrier(Kind::All, need, flush, tag)
            .map_err(|cause| Rejected {
                error: not_written(cause, ErrorContext::default()),
                buf: None,
            })
    }

    fn start_barrier(
        &mut self,
        kind: Kind,
        need: u64,
        flush: bool,
        tag: u64,
    ) -> Result<(), NotWrittenCause> {
        if self.core.table.full() {
            return Err(NotWrittenCause::QueueFull);
        }
        let Some((index, qtag)) = self.core.table.claim() else {
            return Err(NotWrittenCause::QueueFull);
        };
        self.core.table.start(index, tag, kind, None, false);
        ask(
            &mut self.core.table,
            &self.store,
            &self.inbox,
            qtag,
            need,
            flush,
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, unused_results, unused_must_use)]

    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use store_io_platform::Wake;
    use store_io_sim::{SimConfig, SimPlatform};

    use crate::completion::Done;
    use crate::{Store, StoreOptions, SubmitterConfig, WriteMode};

    fn store(seed: u64) -> Store<SimPlatform> {
        let opts = StoreOptions {
            queues: 1,
            ..StoreOptions::default()
        };
        Store::create(
            SimPlatform::new(SimConfig::volatile(seed)),
            Path::new("/db"),
            opts,
        )
        .unwrap()
    }

    /// Dropping a submitter whose durable write and barrier wait for a flush
    /// returns only after that flush: here, after the test lets it run.
    #[test]
    fn test_drop_waits_for_a_held_flush() {
        let s = store(1100);
        let wal = s.provision_append_region("wal", 1 << 20).unwrap();
        let mut sub = s.submitter(SubmitterConfig::default()).unwrap();
        assert!(s.inner.domain.hold_leadership());
        let buf = sub.take_buf(100).unwrap();
        sub.append(&wal, buf, WriteMode::Durable, 1).unwrap();
        sub.barrier_all(2).unwrap();
        let released = Arc::new(AtomicBool::new(false));
        let releaser = {
            let (s, released) = (s.clone(), Arc::clone(&released));
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                released.store(true, Ordering::SeqCst);
                s.inner.domain.drop_leadership();
            })
        };
        drop(sub);
        assert!(
            released.load(Ordering::SeqCst),
            "drop returned before the flush it waited for"
        );
        releaser.join().unwrap();
        assert!(s.poisoned().is_none());
    }

    /// The submitter's waker ends a wait that waits only for a flush; the
    /// flush then completes the write with its receipt.
    #[test]
    fn test_the_waker_ends_a_wait_for_a_flush() {
        let s = store(1101);
        let wal = s.provision_append_region("wal", 1 << 20).unwrap();
        let mut sub = s.submitter(SubmitterConfig::default()).unwrap();
        assert!(s.inner.domain.hold_leadership());
        let buf = sub.take_buf(100).unwrap();
        sub.append(&wal, buf, WriteMode::Durable, 1).unwrap();
        let w = sub.waker();
        let waker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            w.wake();
        });
        let start = Instant::now();
        assert_eq!(sub.wait(1).unwrap().count(), 0);
        assert!(
            start.elapsed() >= Duration::from_millis(90),
            "returned before the wake"
        );
        waker.join().unwrap();
        s.inner.domain.drop_leadership();
        let c = sub.wait(1).unwrap().next().unwrap();
        let Ok(Done::Written(w)) = c.outcome else {
            panic!("{:?}", c.outcome)
        };
        assert!(w.receipt.unwrap().covers(&w.ticket));
    }
}
