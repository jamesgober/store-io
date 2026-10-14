//! The engine-layer `Submitter` (D-065) on the platform under test:
//! appends (plain and durable), page writes, a store-wide barrier and reads,
//! many in flight from one thread; every completion arrives once, with its
//! tag and the right outcome, and everything reads back through the
//! blocking API after a reopen.

use std::collections::HashSet;

use store_io_engine::completion::{Completion, Done};
use store_io_engine::{Store, Submitter, SubmitterConfig, WriteMode};
use store_io_platform::Platform;

use super::{Check, Ctx, done, opts, pattern};

/// Tag of the store-wide barrier.
const BARRIER: u64 = 999;
/// Appends, and page writes, of each kind: twelve in flight together, fewer
/// than the pool's buffers per class, so taking one never waits on a
/// completion.
const EACH: u8 = 6;

/// The next completions, waiting for at least one; an error if none can
/// come (nothing in flight), never a spin.
fn next<P: Platform>(sub: &mut Submitter<P>, what: &str) -> Result<Vec<Completion>, String>
where
    P::Queue: Send,
{
    let got: Vec<Completion> = done(sub.wait(1), what)?.collect();
    if got.is_empty() && sub.in_flight() == 0 {
        return Err(format!("{what}: nothing in flight and nothing completed"));
    }
    Ok(got)
}

pub(super) fn submitter_round_trip<P: Platform + Clone>(c: &Ctx<'_, P>) -> Check
where
    P::Queue: Send,
{
    let s = done(Store::create(c.p.clone(), &c.path, opts(c.trust)), "create")?;
    let wal = done(s.provision_append_region("wal", 4 << 20), "provision wal")?;
    let pages = done(s.provision_page_region("pages", 1 << 20), "provision pages")?;
    // Whole blocks of the store (the device may require more than 4 KiB).
    let blk = pages.block_size() as usize;
    let mut sub = done(
        s.submitter(SubmitterConfig::default().with_depth(16)),
        "submitter",
    )?;
    let filled = |sub: &Submitter<P>, seed: u8| -> Result<_, String> {
        let mut b = done(sub.take_buf(blk), "take_buf")?;
        b.as_mut_slice().copy_from_slice(&pattern(seed, blk));
        Ok(b)
    };
    // Appends, durable and plain in turn (tags 0..EACH), and page writes
    // (tags 100..100+EACH), all in flight together.
    let mut positions = Vec::new();
    for i in 0..EACH {
        let mode = if i % 2 == 0 {
            WriteMode::Durable
        } else {
            WriteMode::Plain
        };
        let b = filled(&sub, i)?;
        let pos = sub
            .append(&wal, b, mode, u64::from(i))
            .map_err(|r| format!("append {i}: {}", r.error))?;
        positions.push(pos);
    }
    for i in 0..EACH {
        let b = filled(&sub, 100 + i)?;
        let pos = done(pages.pos(u64::from(i) * blk as u64), "pos")?;
        sub.write(&pages, pos, b, WriteMode::Plain, 100 + u64::from(i))
            .map_err(|r| format!("page write {i}: {}", r.error))?;
    }
    let mut appended = Vec::new();
    let mut seen = HashSet::new();
    while seen.len() < 2 * usize::from(EACH) {
        for comp in next(&mut sub, "wait for writes")? {
            check!(seen.insert(comp.tag), "tag {} completed twice", comp.tag);
            match done(comp.outcome, "write completion")? {
                Done::Written(w) => {
                    if comp.tag < u64::from(EACH) {
                        let i = comp.tag as usize;
                        check!(
                            w.ticket.pos() == positions[i],
                            "append {i}: the ticket's position is not the one submit returned"
                        );
                        let offset = w.ticket.pos().offset();
                        match &w.receipt {
                            Some(r) => check!(
                                i % 2 == 0 && r.durable_through() >= offset + blk as u64,
                                "append {i}: a receipt short of its range, or on a plain write"
                            ),
                            None => check!(i % 2 == 1, "append {i}: durable without a receipt"),
                        }
                        appended.push((comp.tag, offset));
                    }
                }
                other => return Err(format!("tag {}: unexpected {other:?}", comp.tag)),
            }
        }
    }
    check!(
        seen == (0..u64::from(EACH))
            .chain(100..100 + u64::from(EACH))
            .collect::<HashSet<u64>>(),
        "wrong tags completed: {seen:?}"
    );
    // A store-wide barrier covers every write above.
    sub.barrier_all(BARRIER)
        .map_err(|r| format!("barrier_all: {}", r.error))?;
    let mut flushed = false;
    while !flushed {
        for comp in next(&mut sub, "wait for the barrier")? {
            check!(comp.tag == BARRIER, "unexpected tag {}", comp.tag);
            match done(comp.outcome, "barrier")? {
                Done::Flushed => flushed = true,
                other => return Err(format!("barrier: unexpected {other:?}")),
            }
        }
    }
    // Reads through the submitter (tags 200..).
    for i in 0..EACH {
        let b = done(sub.take_buf(blk), "take_buf")?;
        let pos = done(pages.pos(u64::from(i) * blk as u64), "pos")?;
        sub.read_page(&pages, pos, b, 200 + u64::from(i))
            .map_err(|r| format!("read {i}: {}", r.error))?;
    }
    let mut read = HashSet::new();
    while read.len() < usize::from(EACH) {
        for comp in next(&mut sub, "wait for reads")? {
            check!(
                read.insert(comp.tag),
                "read tag {} completed twice",
                comp.tag
            );
            let i = comp
                .tag
                .checked_sub(200)
                .filter(|i| *i < u64::from(EACH))
                .ok_or_else(|| format!("unexpected tag {}", comp.tag))?;
            match done(comp.outcome, "read")? {
                Done::Read { len } => check!(len == blk, "read {i}: {len} bytes"),
                other => return Err(format!("read {i}: unexpected {other:?}")),
            }
            let buf = comp.buf.ok_or("a read completed without its buffer")?;
            check!(
                buf.as_slice() == pattern(100 + i as u8, blk).as_slice(),
                "page {i} read back wrong through the submitter"
            );
        }
    }
    drop(sub);
    drop((wal, pages, s));
    // Everything reads back through the blocking API after a reopen.
    let s = done(Store::open(c.p.clone(), &c.path, opts(c.trust)), "reopen")?;
    let wal = done(s.append_region("wal"), "wal lookup")?;
    for (tag, offset) in appended {
        let mut back = vec![0; blk];
        let _n = done(wal.read(offset, &mut back), "append read")?;
        check!(
            back == pattern(tag as u8, blk),
            "append {tag} did not round-trip"
        );
    }
    let pages = done(s.page_region("pages"), "pages lookup")?;
    for i in 0..EACH {
        let mut back = vec![0; blk];
        let _n = done(
            pages.read(
                done(pages.pos(u64::from(i) * blk as u64), "pos")?,
                &mut back,
            ),
            "page read",
        )?;
        check!(back == pattern(100 + i, blk), "page {i} did not round-trip");
    }
    Ok(())
}
