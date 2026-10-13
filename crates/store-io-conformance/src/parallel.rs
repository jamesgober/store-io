//! Parallel queues (`QueueConfig::parallel`, D-096): the platform contract
//! holds whether a backend runs the operations on worker threads (the Linux
//! synchronous tier) or ignores the hint (IOCP, io_uring, the simulator).

use store_io_platform::{IoOp, Platform, Queue, QueueConfig};

use super::{BLOCK, Check, Ctx, durable_and_flushes_on, many_in_flight_on, pattern, queue_full_on};

impl<P: Platform> Ctx<'_, P> {
    /// A queue asked to run its operations concurrently
    /// ([`QueueConfig::parallel`]); backends that are already asynchronous
    /// ignore the hint.
    fn parallel_queue(&self, depth: u32) -> Result<P::Queue, String> {
        let cfg = QueueConfig {
            depth,
            parallel: true,
            pool_threads: 4,
        };
        super::ok(self.p.queue(cfg), "parallel queue")
    }
}

pub(super) fn many_in_flight<P: Platform>(c: &Ctx<'_, P>) -> Check {
    many_in_flight_on(c, c.parallel_queue(16)?)
}

pub(super) fn durable_and_flushes<P: Platform>(c: &Ctx<'_, P>) -> Check {
    durable_and_flushes_on(c, c.parallel_queue(4)?)
}

pub(super) fn queue_full<P: Platform>(c: &Ctx<'_, P>) -> Check {
    queue_full_on(c, c.parallel_queue(2)?)
}

/// Buffers free in the scenario's pool, over every class.
fn free<P>(c: &Ctx<'_, P>) -> usize {
    c.pool.occupancy().iter().map(|&(_, free, _)| free).sum()
}

/// Sixteen writes submitted and the queue dropped at once: the drop waits
/// for what is in flight (or cancels it) and every buffer comes back.
pub(super) fn drop_in_flight<P: Platform>(c: &Ctx<'_, P>) -> Check {
    let dir = c.dir()?;
    let f = c.file(&dir, "x", 1 << 20)?;
    let before = free(c);
    let mut q = c.parallel_queue(16)?;
    for i in 0..16u8 {
        let buf = c.buf(&pattern(i, 4 * BLOCK))?;
        q.submit(
            IoOp::Write {
                file: &f,
                offset: u64::from(i) * 4 * BLOCK as u64,
                buf,
                dsync: false,
            },
            u64::from(i),
        )
        .map_err(|r| format!("write {i} refused: {:?}", r.raw))?;
    }
    check!(
        free(c) + 16 == before,
        "16 buffers should be in flight, {} are",
        before.abs_diff(free(c))
    );
    drop(q);
    let after = free(c);
    check!(
        after == before,
        "{} buffers were not returned after the drop",
        before.abs_diff(after)
    );
    Ok(())
}
