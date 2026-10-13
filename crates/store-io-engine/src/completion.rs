//! What the engine-layer [`crate::Submitter`] hands back (D-098): its
//! configuration, refusals and completions.

use std::collections::VecDeque;

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
}

impl Default for SubmitterConfig {
    fn default() -> Self {
        Self { depth: 64 }
    }
}

impl SubmitterConfig {
    /// The same, with `depth`.
    #[must_use]
    pub fn with_depth(mut self, depth: u32) -> Self {
        self.depth = depth;
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
    /// A receipt, when the write itself was durable on completion and the
    /// submitter was asked for one. Always `None` in this release: make
    /// the ticket durable with `sync_through`.
    pub receipt: Option<DurableReceipt>,
}

/// What a successful operation did.
#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Done {
    /// An append or page write.
    Written(Written),
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
