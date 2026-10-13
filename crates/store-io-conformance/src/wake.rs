//! Queue wakers (D-083): a wake from another thread never creates, loses
//! or repeats a completion. (That a wake ends a blocked wait is proven per
//! backend: loom for the Linux pool's inbox, a unit test of the IOCP
//! queue.)

use store_io_platform::{CompletionBuf, IoOp, Platform, Queue, Wake};

use super::{BLOCK, Check, Ctx, pattern};

pub(super) fn wake_keeps_every_completion<P: Platform>(c: &Ctx<'_, P>) -> Check {
    let dir = c.dir()?;
    let f = c.file(&dir, "w", 1 << 20)?;
    for parallel in [false, true] {
        let mut q = if parallel {
            c.parallel_queue(8)?
        } else {
            c.queue(8)?
        };
        // A wake from another thread, with nothing in flight: the next wait
        // returns, with nothing.
        let w = q.waker();
        std::thread::spawn(move || w.wake())
            .join()
            .map_err(|_| "the waking thread panicked".to_owned())?;
        let mut out = CompletionBuf::with_capacity(8);
        let n = super::ok(q.wait(1, &mut out), "wait after a wake")?;
        check!(n == 0 && out.is_empty(), "a wake produced {n} completions");
        // Writes in flight and a wake: every completion arrives exactly
        // once, whatever the wake cut short.
        for i in 0..4u8 {
            let buf = c.buf(&pattern(i, BLOCK))?;
            q.submit(
                IoOp::Write {
                    file: &f,
                    offset: u64::from(i) * BLOCK as u64,
                    buf,
                    dsync: false,
                },
                u64::from(i),
            )
            .map_err(|r| format!("write {i} refused: {:?}", r.raw))?;
        }
        q.waker().wake();
        let mut seen = [false; 4];
        let mut waits = 0;
        while seen.iter().any(|s| !s) {
            waits += 1;
            check!(waits <= 100, "completions never arrived after a wake");
            let _added = super::ok(q.wait(1, &mut out), "wait")?;
            for comp in out.drain() {
                let i = usize::try_from(comp.tag).unwrap_or(usize::MAX);
                check!(
                    i < 4 && !seen[i],
                    "completion tag {} invented or repeated (parallel: {parallel})",
                    comp.tag
                );
                seen[i] = true;
                check!(comp.result == Ok(BLOCK), "write {i}: {:?}", comp.result);
            }
        }
        check!(q.in_flight() == 0, "operations left in flight");
        let n = q.reap(&mut out);
        check!(n == 0, "{n} completions after every write was reaped");
    }
    Ok(())
}
