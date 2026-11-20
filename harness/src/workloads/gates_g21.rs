//! G21's measurement (R5.6, FILEMAP's gates row): [`Store::export_with`]
//! of a store of several regions (one large page region, one append region
//! written and made durable, and a run of small page regions, so the
//! per-region work of a backup is in the figure: a data file made, synced
//! and its directory synced for each) to a [`FileSink`] in a directory on
//! the same device, repeated for the measured time, alternating with fio
//! reading one file while it writes another on that device at the same
//! time, [`REPS`] times each (G7's rule: medians of alternated runs). The
//! gate is the export's rate against the lower of fio's two rates: a copy
//! reads and writes every byte once, so it can go no faster than the
//! slower side while both share the device.
//!
//! The same export into a [`StoreSink`] (a new store restored directly) is
//! measured after and published beside it, never gated.
//!
//! **Reported in 0.6.0, gated at 80% from V6f (D-172)**: the export ran at
//! 71 to 81% of the row's baseline on CI's kind of device, its own rate
//! steady while fio's moved between runs, so as a gate it would pass about
//! one run in three. The row's primary figure is the export against fio's
//! writer with its end fsync counted beside its reader (an export reads,
//! writes and must flush), with min(fio read, fio write), as the row
//! states, and fio writing a fresh file alone beside it.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use store_io_engine::{
    ExportOptions, ExportSink, FileSink, FileSinkOptions, FileSource, FileSourceOptions,
    ImportSource, StoreSink, StoreSinkOptions,
};

use super::gates::{Row, Verdict, f};
use super::stamp::{self, BLOCK};
use super::{Ctx, remove, sioerr, with_store};
use crate::report::{Point, System, Verify};
use crate::sio::{self, SioStore};

/// Alternations of fio and the export.
pub const REPS: usize = 3;
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
/// fsync counted, and a fresh file's write alone with its fsync counted):
/// `mb_per_s` the export's median,
/// `fio_read_mb_per_s` and `fio_write_mb_per_s` fio's medians and
/// `fio_min_mb_per_s` the median of each run's lower side, `spread` the
/// larger relative range of the export's and fio's lower side;
/// `store_sink_mb_per_s` a `StoreSink` export's median, measured after.
/// Every block holds its region's pattern ([`stamp`]); the last export is
/// read back through a `FileSource` (every span's CRC checked) and every
/// word checked.
pub fn export(ctx: &Ctx, fio: &mut dyn FnMut() -> Result<(f64, f64, f64, f64), String>) -> Point {
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
        let (mut synced, mut fresh) = (Vec::new(), Vec::new());
        let mut last = None;
        for _ in 0..REPS {
            let (r, w, ws, wf) = fio()?;
            fresh.push(wf);
            reads.push(r);
            writes.push(w);
            synced.push(ws);
            lows.push(r.min(w));
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
        }
        let (m, low) = (median(&ours), median(&lows));
        p.metric("mb_per_s", Some(m));
        p.metric("fio_read_mb_per_s", Some(median(&reads)));
        p.metric("fio_write_mb_per_s", Some(median(&writes)));
        p.metric("fio_min_mb_per_s", Some(low));
        p.metric("fio_write_synced_mb_per_s", Some(median(&synced)));
        p.metric("fio_fresh_mb_per_s", Some(median(&fresh)));
        p.metric("spread", Some(range(&ours, m).max(range(&lows, low))));
        // Published, not gated: a refusal is noted and leaves the gate to
        // `FileSink`; a `StoreSink` export that ends incomplete fails the
        // point, as wrong bytes would.
        let mut direct = Vec::new();
        for _ in 0..REPS {
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
        p.notes.push(format!(
            "alternated with fio reading and writing together: export {ours:.0?} MB/s, fio read {reads:.0?}, write {writes:.0?} MB/s; StoreSink export {direct:.0?} MB/s"
        ));
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
        Ok(())
    })
}

/// G21's row (D-172): reported, never a failure, the 80% it is gated at
/// from V6f in its text; the export against fio's writer with its end
/// fsync counted, then against min(fio read, fio write) and fio writing a
/// fresh file alone. A point that did not complete, or whose bytes did not
/// check out, is reported as not measured, with why.
pub(super) fn row(g21: &Point) -> Row {
    let v = |k: &str| if g21.usable() { g21.get(k) } else { None };
    let export = v("mb_per_s");
    let share = |base: Option<f64>| match export.zip(base) {
        Some((e, b)) if b > 0.0 => format!("{:.0}%", 100.0 * e / b),
        _ => "n/a".to_owned(),
    };
    let synced = v("fio_write_synced_mb_per_s");
    let low = v("fio_min_mb_per_s");
    let fresh = v("fio_fresh_mb_per_s");
    let measured = if g21.usable() {
        format!(
            "store-io export {} MB/s: {} of fio's writer with its end fsync counted ({} MB/s, a lower bound), beside its reader; {} of min(fio read {}, write {}) = {} MB/s; {} of fio writing a fresh 1 GiB file alone, its fsync counted ({} MB/s); medians of {} alternated runs, spread {:.0}%; {} MiB in {} regions; StoreSink export {} MB/s (published)",
            f(export, 0),
            share(synced),
            f(synced, 0),
            share(low),
            f(v("fio_read_mb_per_s"), 0),
            f(v("fio_write_mb_per_s"), 0),
            f(low, 0),
            share(fresh),
            f(fresh, 0),
            REPS,
            v("spread").unwrap_or(0.0) * 100.0,
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
        gate: "Export to FileSink against fio's writer with its end fsync counted, fio reading one file and writing another on the same device at once (QD8, 1 MiB): reported in 0.6.0, gated at 80% from V6f; also against min(fio read, fio write) and fio writing a fresh file alone",
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

/// The median of a non-empty set.
fn median(v: &[f64]) -> f64 {
    let mut v = v.to_vec();
    v.sort_by(f64::total_cmp);
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
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
        p.metric("fio_write_synced_mb_per_s", Some(2875.0));
        p.metric("fio_read_mb_per_s", Some(4800.0));
        p.metric("fio_write_mb_per_s", Some(3290.0));
        p.metric("fio_min_mb_per_s", Some(3290.0));
        p.metric("fio_fresh_mb_per_s", Some(6600.0));
        p.metric("spread", Some(0.04));
        p.metric("store_sink_mb_per_s", Some(1540.0));
        p.verify = verify;
        p
    }

    /// The row is computed from the point: reported, the export against
    /// each baseline, and the 80% it is gated at from V6f in its text.
    #[test]
    fn test_g21_s_row_is_reported_with_each_baseline() {
        let r = row(&point(Verify::Passed("checked".to_owned())));
        assert_eq!(r.id, "G21");
        assert!(r.verdict == Verdict::Reported);
        assert!(r.gate.contains("gated at 80% from V6f"), "{}", r.gate);
        assert!(
            r.measured.contains("2300 MB/s: 80% of fio's writer"),
            "{}",
            r.measured
        );
        assert!(
            r.measured
                .contains("70% of min(fio read 4800, write 3290) = 3290 MB/s"),
            "{}",
            r.measured
        );
        assert!(
            r.measured.contains("35% of fio writing a fresh"),
            "{}",
            r.measured
        );
    }

    /// A point whose bytes did not check out is reported, never a failure,
    /// as not measured, with why.
    #[test]
    fn test_g21_s_row_without_a_measurement_is_reported_as_not_measured() {
        let r = row(&point(Verify::Failed("a region's words".to_owned())));
        assert!(r.verdict == Verdict::Reported);
        assert_eq!(r.measured, "not measured: a region's words");
    }
}
