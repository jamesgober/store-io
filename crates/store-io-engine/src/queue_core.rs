//! The core both submission paths are built on (A1, PLANNING R3.3): a queue
//! and its op table ([`Table`], one row per operation in flight, matched by
//! row tag). The public `Submitter` is one core owned by one thread; every
//! lane of the blocking layers is a core any thread may borrow, one call at
//! a time (`exec.rs`).

use std::sync::Arc;

use store_io_platform::CompletionBuf;

use crate::exec::LaneMark;
use crate::optable::Table;

/// A queue, the completions it hands back, and the op table their tags
/// name.
pub(crate) struct Core<Q> {
    // Declared first, so it drops (after its owner drained it) before the
    // table releases what the rows hold.
    pub(crate) queue: Q,
    /// What the queue hands back, under row tags.
    pub(crate) raw: CompletionBuf,
    pub(crate) table: Table,
}

impl<Q> Core<Q> {
    /// A core of `depth` rows over `queue`; the table publishes its busy
    /// mark to `mark`.
    pub(crate) fn new(queue: Q, depth: usize, mark: Arc<LaneMark>) -> Self {
        let depth = depth.max(1);
        Self {
            queue,
            raw: CompletionBuf::with_capacity(depth),
            table: Table::new(depth, mark),
        }
    }
}
