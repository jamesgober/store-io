//! What the engine-layer [`crate::Submitter`] hands back (D-098): its
//! configuration, refusals and completions.

use std::collections::VecDeque;
use store_io_core::IoClass;

use store_io_core::error::Error;
use store_io_platform::IoBuf;

use crate::receipt::{DurableReceipt, WriteTicket};

/// How a [`crate::Submitter`] is set up:
/// `SubmitterConfig::default().with_depth(128)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct SubmitterConfig {
    /// Operations submitted and not yet taken back from `reap` or `wait`, at
    /// most (1 to [`crate::MAX_DEPTH`]; default 64). Also the depth of the
    /// submitter's queue.
    pub depth: u32,
    /// The class of this submitter's I/O (PLANNING R5.4).
    ///
    /// - `Commit` (the default): never paced; its barriers, and on a
    ///   power-safe store its writes, mark the device's commits active.
    /// - `Background`: its reads and writes go out in pieces of at most
    ///   `StoreOptions::background_io`, admitted by its device's pacing: while
    ///   the device's commits are active, one piece of the submitter in
    ///   flight (unless its group has room), else up to
    ///   `background_unpaced`. An operation refused before anything is
    ///   reserved is `NotWritten(Paced)` with its buffer (reap and retry);
    ///   its later pieces wait in the submitter, oldest first, and go out at
    ///   its next `reap` or `wait`, or at its drop. Its buffers
    ///   ([`crate::Submitter::take_buf`]) never hold more than half of a pool
    ///   class; its barriers and writes never mark commits active. An append
    ///   whose pieces wait holds back its region's completed prefix, so a
    ///   commit syncing that region waits for the submitter's thread to reap
    ///   or wait: keep commits and background appends in separate regions.
    /// - `Read`: its reads are of the read class, its writes `Commit`.
    pub class: IoClass,
}

impl Default for SubmitterConfig {
    fn default() -> Self {
        Self {
            depth: 64,
            class: IoClass::Commit,
        }
    }
}

impl SubmitterConfig {
    /// The same, with `depth`.
    #[must_use]
    pub fn with_depth(mut self, depth: u32) -> Self {
        self.depth = depth;
        self
    }

    /// The same, of `class`.
    #[must_use]
    pub fn with_class(mut self, class: IoClass) -> Self {
        self.class = class;
        self
    }
}

/// An operation the submitter refused: nothing was submitted, unless
/// `error` is `DurabilityUnknown` (an append refused by the OS after its
/// position was reserved, which poisons the store as a failed blocking
/// append does). The buffer comes back; `None` only from a backend that
/// broke its contract and kept it.
#[derive(Debug)]
pub struct Rejected {
    /// Why.
    pub error: Error,
    /// The operation's buffer.
    pub buf: Option<IoBuf>,
}

/// A write that completed in full.
#[derive(Debug, PartialEq, Eq)]
pub struct Written {
    /// The write's ticket, for `sync_through` on its region. Minted only
    /// now, when every byte has been transferred (D-080).
    pub ticket: WriteTicket,
    /// For a [`WriteMode::Durable`] write, the receipt for its own range;
    /// `None` for a plain write (make the ticket durable with a barrier or
    /// `sync_through`).
    pub receipt: Option<DurableReceipt>,
}

/// How a submitter write is made durable (D-099).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum WriteMode {
    /// The completion carries the ticket; make it durable with a barrier.
    #[default]
    Plain,
    /// The completion is handed back only once the write is durable, with a
    /// receipt (a prefix receipt for an append whose region was complete up
    /// to it, else one for its own range): at once on a power-safe store
    /// (the write itself was durable) unless its region forces data sync,
    /// else after a covering flush.
    Durable,
}

/// What a successful operation did.
#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Done {
    /// An append or page write.
    Written(Written),
    /// A barrier for one ticket: a receipt for that ticket's range.
    Synced(DurableReceipt),
    /// A barrier for the whole store: every write that completed in the
    /// store before it was submitted is durable.
    Flushed,
    /// A read: the bytes read into the buffer, whose length is set to it
    /// (fewer than asked only if the device returned fewer).
    Read {
        /// Bytes read.
        len: usize,
    },
}

/// One finished operation.
#[derive(Debug)]
pub struct Completion {
    /// The tag given at submission.
    pub tag: u64,
    /// The operation's buffer, given back (for a read, holding the bytes);
    /// `None` only from a backend that broke its contract and kept it.
    pub buf: Option<IoBuf>,
    /// The result. A write that failed or transferred short after it was
    /// issued is `DurabilityUnknown`, and the store is poisoned (as for the
    /// blocking calls). A read the device could not do is
    /// `Corruption(MediaError)` for the read's whole range (narrow it with a
    /// scan), or `Io` with the raw code for any other failure.
    pub outcome: Result<Done, Error>,
}

/// Completions handed back by `reap` or `wait`, in the order they were
/// collected. Any not taken from the iterator stay for the next call (and
/// count against the submitter's depth).
#[derive(Debug)]
#[must_use = "completions not taken stay with the submitter and count against its depth"]
pub struct Completions<'a> {
    pub(crate) ready: &'a mut VecDeque<Completion>,
}

impl Iterator for Completions<'_> {
    type Item = Completion;

    fn next(&mut self) -> Option<Completion> {
        self.ready.pop_front()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.ready.len(), Some(self.ready.len()))
    }
}

impl ExactSizeIterator for Completions<'_> {}
