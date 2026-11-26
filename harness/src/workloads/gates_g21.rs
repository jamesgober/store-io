//! G21's measurement (R5.6, FILEMAP's gates row): [`Store::export_with`]
//! of a store of several regions (one large page region, one append region
//! written and made durable, and a run of small page regions, so the
//! per-region work of a backup is in the figure: a data file made, synced
//! and its directory synced for each) to a [`FileSink`] in a directory on
//! the same device, repeated for the measured time, alternating with fio
//! reading one file while it writes another on that device at the same
//! time, in [`paired::ROUNDS`] rounds, the side first alternating, judged
//! on the rounds' ratios ([`paired`]). The baseline is each round's lower
//! of fio's two rates: a copy reads and writes every byte once, so it can
//! go no faster than the slower side while both share the device. fio's
//! write rate counts its end fsync by fio's own clock (an export reads,
//! writes and must flush; direct, the fsync costs fio little), so the
//! lower side is also the writer with its fsync counted
//! ([`fio::read_write_together`]).
//!
//! Published beside it, never gated, measured after the rounds: the same
//! export into a [`StoreSink`] (a new store restored directly), and fio
//! writing a fresh file alone (its file's removal never just before a
//! timed export).
//!
//! **Reported, gated at 80% from V6f (D-172)**: a store-io that has not
//! met it is no failure yet; an integrity failure is (the point's check
//! fails, and with it the run's integrity line, which CI requires).

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use store_io_engine::{
    ExportOptions, ExportSink, FileSink, FileSinkOptions, FileSource, FileSourceOptions,
    ImportSource, StoreSink, StoreSinkOptions,
};

use super::gates::{Row, Verdict, f};
use super::paired::{self, median};
use super::stamp::{self, BLOCK};
use super::{Ctx, remove, sioerr, with_store};
#[cfg(doc)]
use crate::fio;
use crate::report::{Point, System, Verify};
use crate::sio::{self, SioStore};

/// Runs of each published figure: the `StoreSink` export, fio's fresh file.
const QUIET: usize = 3;
const MIB: u64 = 1 << 20;
/// The large page region.
const BIG: u64 = 160 << 20;
/// The append region, written in full and made durable.
const LOG: u64 = 64 << 20;
/// The small page regions, and each one's size.
const SMALL: u64 = 32;
const SMALL_SIZE: u64 = MIB;
/// The regions the store holds.
pub const REGIONS: u64 = 2 + SMALL;
/// Every byte the export carries.
pub const TOTAL: u64 = BIG + LOG + SMALL * SMALL_SIZE;

/// Each region's name and size, in provisioning order, with the stamp's
/// sequence its blocks were written with (its index).
fn layout() -> Vec<(String, u64)> {
    let mut v = vec![("g21-big".to_owned(), BIG), ("g21-log".to_owned(), LOG)];
    for i in 0..SMALL {
        v.push((format!("g21-small-{i:02}"), SMALL_SIZE));
    }
    v
}

/// Exports a store of [`layout`]'s regions to a fresh `FileSink` directory
/// repeatedly; MB/s (bytes exported over the time inside the exports, each
/// from the sink's creation to the export's return, `finish` and its syncs
/// included), alternating with `fio` (one baseline run: the read side's and
/// the write side's MB/s, run together, the write side's with its end
/// fsync counted) in [`paired::ROUNDS`] rounds against each round's lower
/// side: the rounds' ratios as [`paired::record`] names them, `mb_per_s`
/// the export's median, `fio_read_mb_per_s` and `fio_write_mb_per_s`
/// fio's medians and `fio_min_mb_per_s` the median of each round's lower
/// side, `spread` the larger relative range of the export's and fio's
/// lower side; after the rounds, `store_sink_mb_per_s` a `StoreSink`
/// export's median and `fio_fresh_mb_per_s` the median of `fresh` (fio
/// writing a fresh file alone, its fsync counted), [`QUIET`] runs each.
/// Every block holds its region's pattern ([`stamp`]); the last `FileSink`
/// export is read back through a `FileSource` (every span's CRC checked)
/// and every word checked.
pub fn export(
    ctx: &Ctx,
    fio: &mut dyn FnMut() -> Result<(f64, f64), String>,
    fresh: &mut dyn FnMut() -> Result<f64, String>,
) -> Point {
    let p = Point::new(System::StoreIo, "Store::export to FileSink");
    with_store(ctx, "g21", p, |store, p| {
        let names = fill(store)?;
        let warm_end = Instant::now() + Duration::from_secs_f64(ctx.warmup);
        loop {
            let (_, dir) = file_once(ctx, store)?;
            if let Some(e) = remove(&dir) {
                p.notes.push(e);
            }
            if Instant::now() >= warm_end {
                break;
            }
        }
        let (mut ours, mut reads, mut writes, mut lows, mut exports) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new(), 0u64);
        let mut last = None;
        for i in 0..paired::ROUNDS {
            let first = paired::ours_first(i);
            for ours_turn in [first, !first] {
                if ours_turn {
                    let (mb, dir) = timed(ctx, &mut exports, |keep| {
                        let (secs, dir) = file_once(ctx, store)?;
                        Ok((secs, keep.then_some(dir)))
                    })?;
                    ours.push(mb);
                    if let Some(old) = dir.and_then(|d| last.replace(d)) {
                        if let Some(e) = remove(&old) {
                            p.notes.push(e);
                        }
                    }
                } else {
                    let (r, w) = fio()?;
                    reads.push(r);
                    writes.push(w);
                    lows.push(r.min(w));
                }
            }
        }
        let (m, low) = (median(&ours), median(&lows));
        p.metric("mb_per_s", Some(m));
        p.metric("fio_read_mb_per_s", Some(median(&reads)));
        p.metric("fio_write_mb_per_s", Some(median(&writes)));
        p.metric("fio_min_mb_per_s", Some(low));
        p.metric("spread", Some(range(&ours, m).max(range(&lows, low))));
        paired::record(p, &ours, &lows);
        p.notes.push(format!(
            "alternated with fio reading and writing together (store-io first in the 2nd, 4th, ... rounds): export {ours:.0?} MB/s, fio read {reads:.0?}, write {writes:.0?} MB/s"
        ));
        // The last FileSink export checked now, before anything published:
        // nothing after can leave the gated figure unchecked.
        let Some(dir) = last else {
            return Err("no export was kept to check".to_owned());
        };
        let checked = check(ctx, &dir, &names);
        if let Some(e) = remove(&dir) {
            p.notes.push(e);
        }
        p.verify = match checked {
            Ok(()) => Verify::Passed(format!(
                "every export complete with all {} regions and {} MiB; the last one read back through a FileSource (every span's CRC checked) and every word of every region checked",
                names.len(),
                TOTAL / MIB
            )),
            Err(why) => Verify::Failed(why),
        };
        // Published, not gated, after the rounds and the check: a
        // `StoreSink` export (any failure, an incomplete store included, is
        // noted and leaves the gate to `FileSink`), then fio writing a fresh
        // file alone (its failure is noted).
        let mut direct = Vec::new();
        for _ in 0..QUIET {
            match timed(ctx, &mut exports, |_| {
                let secs = store_once(ctx, store)?;
                Ok((secs, None))
            }) {
                Ok((mb, _)) => direct.push(mb),
                Err(why) => {
                    p.notes
                        .push(format!("the StoreSink export not measured: {why}"));
                    break;
                }
            }
        }
        if !direct.is_empty() {
            p.metric("store_sink_mb_per_s", Some(median(&direct)));
        }
        p.metric("exports", Some(exports as f64));
        let mut alone = Vec::new();
        for _ in 0..QUIET {
            match fresh() {
                Ok(mb) => alone.push(mb),
                Err(why) => {
                    p.notes
                        .push(format!("fio's fresh-file write not measured: {why}"));
                    break;
                }
            }
        }
        if !alone.is_empty() {
            p.metric("fio_fresh_mb_per_s", Some(median(&alone)));
        }
        p.notes.push(format!(
            "after the rounds: StoreSink export {direct:.0?} MB/s; fio writing a fresh file alone {alone:.0?} MB/s"
        ));
        Ok(())
    })
}

/// G21's row (D-172): reported, the 80% it is gated at from V6f in its
/// text (an integrity failure still fails the run: the point's check, and
/// with it the report's integrity line, which CI requires); the export
/// against each round's min(fio read, fio write), fio's write rate counting
/// its end fsync, with the rounds' ratio, its interval and the pooled one,
/// and fio writing a fresh file alone beside it. A point that did not
/// complete, or whose bytes did not check out, is reported as not
/// measured, with why.
pub(super) fn row(g21: &Point) -> Row {
    let v = |k: &str| if g21.usable() { g21.get(k) } else { None };
    let export = v("mb_per_s");
    let share = |base: Option<f64>| match export.zip(base) {
        Some((e, b)) if b > 0.0 => format!("{:.0}%", 100.0 * e / b),
        _ => "n/a".to_owned(),
    };
    let pct = |r: Option<f64>| r.map_or("n/a".to_owned(), |r| format!("{:.0}%", 100.0 * r));
    let fresh = v("fio_fresh_mb_per_s");
    let measured = if g21.usable() {
        format!(
            "store-io export {} MB/s, against each round's min(fio read, fio write) (fio's write rate counts its end fsync): median {} (interval {} to {}), pooled {}; fio read {}, write {} MB/s, lower side {} MB/s (medians of {} rounds, spread {:.0}%); {} of fio writing a fresh 1 GiB file alone, its fsync counted ({} MB/s, published); {} MiB in {} regions; StoreSink export {} MB/s (published)",
            f(export, 0),
            pct(v("ratio")),
            pct(v("ratio_lo")),
            pct(v("ratio_hi")),
            pct(v("pooled_ratio")),
            f(v("fio_read_mb_per_s"), 0),
            f(v("fio_write_mb_per_s"), 0),
            f(v("fio_min_mb_per_s"), 0),
            paired::ROUNDS,
            v("spread").unwrap_or(0.0) * 100.0,
            share(fresh),
            f(fresh, 0),
            TOTAL >> 20,
            REGIONS,
            f(v("store_sink_mb_per_s"), 0)
        )
    } else {
        let why = match (&g21.error, &g21.verify) {
            (Some(e), _) => e.clone(),
            (None, Verify::Failed(w) | Verify::NotChecked(w)) => w.clone(),
            (None, Verify::Passed(_)) => "unknown".to_owned(),
        };
        format!("not measured: {why}")
    };
    Row {
        id: "G21",
        gate: "Export to FileSink against min(fio read, fio write), fio reading one file and writing another on the same device at once (QD8, 1 MiB; its write rate counts its end fsync), 10 alternated rounds (G1's rule on their ratios): reported, gated at 80% from V6f; fio writing a fresh file alone published beside it",
        measured,
        verdict: Verdict::Reported,
    }
}

/// Provisions and writes [`layout`]'s regions, each block stamped with its
/// region's index; the append region is made durable. Returns each name's
/// index.
fn fill(store: &SioStore) -> Result<BTreeMap<String, u64>, String> {
    let mut chunk = vec![0u8; MIB as usize];
    let mut names = BTreeMap::new();
    for (seq, (name, size)) in layout().into_iter().enumerate() {
        let seq = seq as u64;
        if name == "g21-log" {
            let log = store
                .provision_append_region(&name, size)
                .map_err(|e| sioerr("provision_append_region", &e))?;
            let mut ticket = None;
            for i in 0..size / MIB {
                stamp::stamp(&mut chunk, i * MIB / BLOCK as u64, seq);
                ticket = Some(log.append(&chunk).map_err(|e| sioerr("append", &e))?);
            }
            if let Some(t) = ticket {
                let _r = log
                    .sync_through(&t)
                    .map_err(|e| sioerr("sync_through", &e))?;
            }
        } else {
            let pages = store
                .provision_page_region(&name, size)
                .map_err(|e| sioerr("provision", &e))?;
            for i in 0..size / MIB {
                stamp::stamp(&mut chunk, i * MIB / BLOCK as u64, seq);
                let pos = pages.pos(i * MIB).map_err(|e| sioerr("pos", &e))?;
                let _t = pages.write(pos, &chunk).map_err(|e| sioerr("write", &e))?;
            }
        }
        names.insert(name, seq);
    }
    Ok(names)
}

/// Exports for the measured time with `once` (seconds inside one export,
/// and its directory if asked to keep it: only the last one is kept);
/// MB/s over the time inside the exports, and the kept directory.
fn timed(
    ctx: &Ctx,
    exports: &mut u64,
    mut once: impl FnMut(bool) -> Result<(f64, Option<std::path::PathBuf>), String>,
) -> Result<(f64, Option<std::path::PathBuf>), String> {
    let (mut bytes, mut inside, mut kept) = (0u64, 0.0f64, None);
    let t0 = Instant::now();
    loop {
        let (secs, dir) = once(true)?;
        inside += secs;
        bytes += TOTAL;
        *exports += 1;
        if let Some(old) = std::mem::replace(&mut kept, dir) {
            // Only the last export is kept: the earlier ones go now, outside
            // the timed part.
            let _e = remove(&old);
        }
        if t0.elapsed().as_secs_f64() >= ctx.secs {
            break;
        }
    }
    Ok((bytes as f64 / inside.max(1e-9) / 1e6, kept))
}

/// One export into a new `FileSink` directory: the seconds from the sink's
/// creation to the export's return, and the directory. An export that is
/// not complete, or that carried other than every region's bytes, is an
/// error.
fn file_once(ctx: &Ctx, store: &SioStore) -> Result<(f64, std::path::PathBuf), String> {
    let dir = ctx.path("g21-out");
    std::fs::create_dir_all(&dir).map_err(|e| format!("make {}: {e}", dir.display()))?;
    let mut o = FileSinkOptions::default();
    o.store = ctx.opts.clone();
    let t = Instant::now();
    let platform = sio::platform(&ctx.opts)?;
    let mut sink = FileSink::create(platform, &dir, o).map_err(|e| sioerr("FileSink", &e))?;
    run(store, &mut sink)?;
    let secs = t.elapsed().as_secs_f64();
    drop(sink);
    Ok((secs, dir))
}

/// One export into a new store through a `StoreSink`: the seconds from the
/// sink's creation to its drop (the store renamed into place), the new
/// store removed after.
fn store_once(ctx: &Ctx, store: &SioStore) -> Result<f64, String> {
    let dir = ctx.path("g21-store");
    let mut o = StoreSinkOptions::default();
    o.store = ctx.opts.clone();
    let t = Instant::now();
    let platform = sio::platform(&ctx.opts)?;
    let mut sink = StoreSink::create(platform, &dir, o).map_err(|e| sioerr("StoreSink", &e))?;
    run(store, &mut sink)?;
    let whole = sink.is_complete();
    drop(sink);
    let secs = t.elapsed().as_secs_f64();
    let _e = remove(&dir);
    if whole {
        Ok(secs)
    } else {
        Err("the StoreSink did not complete".to_owned())
    }
}

/// Exports every region of `store` into `sink`: an error unless it is
/// complete with every region and byte.
fn run(store: &SioStore, sink: &mut dyn ExportSink) -> Result<(), String> {
    let r = store
        .export_with(sink, ExportOptions::default())
        .map_err(|e| sioerr("export", &e.error))?;
    let regions = layout().len();
    if !r.complete || r.regions != regions || r.bytes != TOTAL {
        return Err(format!(
            "an export ended at {} of {regions} regions, {} of {TOTAL} bytes, complete {}",
            r.regions, r.bytes, r.complete
        ));
    }
    Ok(())
}

/// Reads the export in `dir` back through a `FileSource` and checks every
/// word of every region against its pattern.
fn check(ctx: &Ctx, dir: &Path, names: &BTreeMap<String, u64>) -> Result<(), String> {
    let mut o = FileSourceOptions::default();
    o.store = ctx.opts.clone();
    let platform = sio::platform(&ctx.opts)?;
    let mut src = FileSource::open(platform, dir, o).map_err(|e| sioerr("FileSource", &e))?;
    let h = src.header().map_err(|e| format!("the header: {e:?}"))?;
    if h.region_count as usize != names.len() {
        return Err(format!(
            "the header counts {} regions, the store holds {}",
            h.region_count,
            names.len()
        ));
    }
    let mut buf = vec![0u8; MIB as usize];
    let mut seen = 0usize;
    while let Some(info) = src.next_region().map_err(|e| format!("a region: {e:?}"))? {
        let Some(&seq) = names.get(&info.name) else {
            return Err(format!("a region the store does not hold: {}", info.name));
        };
        let size = layout()
            .into_iter()
            .find(|(n, _)| *n == info.name)
            .map_or(0, |(_, s)| s);
        let mut at = 0u64;
        loop {
            let n = read_full(&mut src, &mut buf)?;
            if n == 0 {
                break;
            }
            if n % BLOCK != 0 || !stamp::holds(&buf[..n], at / BLOCK as u64, seq) {
                return Err(format!("{} at {at}: {n} bytes not its blocks", info.name));
            }
            at += n as u64;
        }
        if at != size {
            return Err(format!("{} read back {at} of {size} bytes", info.name));
        }
        seen += 1;
    }
    if seen != names.len() {
        return Err(format!("{seen} of {} regions read back", names.len()));
    }
    Ok(())
}

/// Fills `buf` from the source's current region as far as it goes: the
/// bytes read, short only at the region's end.
fn read_full(src: &mut FileSource<sio::Plat>, buf: &mut [u8]) -> Result<usize, String> {
    let mut n = 0;
    while n < buf.len() {
        let k = src
            .read(&mut buf[n..])
            .map_err(|e| format!("a read: {e:?}"))?;
        if k == 0 {
            break;
        }
        n += k;
    }
    Ok(n)
}

/// A set's range relative to `m`.
fn range(v: &[f64], m: f64) -> f64 {
    let (lo, hi) = v
        .iter()
        .fold((f64::MAX, f64::MIN), |(l, h), x| (l.min(*x), h.max(*x)));
    if m > 0.0 { (hi - lo) / m } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(verify: Verify) -> Point {
        let mut p = Point::new(System::StoreIo, "Store::export to FileSink");
        p.metric("mb_per_s", Some(2300.0));
        p.metric("fio_read_mb_per_s", Some(4800.0));
        p.metric("fio_write_mb_per_s", Some(3290.0));
        p.metric("fio_min_mb_per_s", Some(3290.0));
        p.metric("fio_fresh_mb_per_s", Some(6600.0));
        p.metric("spread", Some(0.04));
        p.metric("ratio", Some(0.70));
        p.metric("ratio_lo", Some(0.66));
        p.metric("ratio_hi", Some(0.74));
        p.metric("pooled_ratio", Some(0.699));
        p.metric("store_sink_mb_per_s", Some(1540.0));
        p.verify = verify;
        p
    }

    /// The row is computed from the point: reported, the export against
    /// each round's lower fio side (the ratio, its interval, the pooled
    /// one), and the 80% it is gated at from V6f in its text.
    #[test]
    fn test_g21_s_row_is_reported_with_each_baseline() {
        let r = row(&point(Verify::Passed("checked".to_owned())));
        assert_eq!(r.id, "G21");
        assert!(r.verdict == Verdict::Reported);
        assert!(r.gate.contains("gated at 80% from V6f"), "{}", r.gate);
        assert!(
            r.measured
                .contains("2300 MB/s, against each round's min(fio read, fio write)"),
            "{}",
            r.measured
        );
        assert!(
            r.measured
                .contains("median 70% (interval 66% to 74%), pooled 70%"),
            "{}",
            r.measured
        );
        assert!(
            r.measured
                .contains("fio read 4800, write 3290 MB/s, lower side 3290 MB/s"),
            "{}",
            r.measured
        );
        assert!(
            r.measured.contains("35% of fio writing a fresh"),
            "{}",
            r.measured
        );
    }

    /// A point whose bytes did not check out is reported as not measured,
    /// with why (its failed check fails the run's integrity line, which CI
    /// requires, not this row).
    #[test]
    fn test_g21_s_row_without_a_measurement_is_reported_as_not_measured() {
        let r = row(&point(Verify::Failed("a region's words".to_owned())));
        assert!(r.verdict == Verdict::Reported);
        assert_eq!(r.measured, "not measured: a region's words");
    }
}
