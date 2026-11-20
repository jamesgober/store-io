//! Import (PLANNING R5.6, D-169): a new store built from what an export
//! pushed, pulled through an [`ImportSource`] (a [`FileSource`](crate::FileSource)
//! or a caller's own).
//!
//! **As `copy_store` builds its target.** In the target directory under
//! the copy lock, a crashed copy's or import's temporary removed first,
//! the free space checked against the header's `total` before anything is
//! written; the store created under the temporary name, each region
//! provisioned with its name, kind, data size, cap, tag, generation, fill
//! pattern and key, its bytes written once as its provisioning's fill
//! (durable and verified before its header, D-137): the source's bytes,
//! then for an append region exported with a durable length its pattern
//! from there to its data size; a slot by one commit of its object. Then
//! flushed, closed, and renamed into place by a rename that never
//! replaces: no store under the target name unless the import completed.
//! A [`StoreSink`](crate::StoreSink) restores each region the same way
//! (`restore.rs`).
//!
//! **The source is untrusted.** Its header's block size must be the
//! target's and its region count within the format's table; each region
//! is checked by [`RegionInfo::validate`], its name once, its size within
//! the header's total; its bytes must arrive in order, exactly its stream
//! length (a source short, long, or answering more than it was asked for
//! is `NotWritten(InvalidOptions)`); and it must yield exactly the regions
//! its header counts. Any refusal or failure renames nothing.
//!
//! **Restored append regions open unpositioned**, as any reopened store's:
//! the report gives each one's durable length for the caller's
//! `resume_at`.

use std::cell::Cell;
use std::path::Path;

use store_io_core::error::{Error, ErrorContext, NotWrittenCause, Op};
use store_io_format::meta::RegionKind;
use store_io_platform::Platform;

use crate::call::{self, CallState};
use crate::export::{ExportHeader, ImportSource, RegionInfo, SinkError};
use crate::io::not_written;
use crate::restore::{Restore, check_header, invalid};
use crate::store::StoreOptions;
use crate::target::TargetDir;

/// How [`import`] builds its store.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct ImportOptions {
    /// The new store's options (its trust among them); its block size and
    /// region capacity are the export's.
    pub store: StoreOptions,
}

/// How far an import got.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct ImportReport {
    /// The header the source gave (`None` if it gave none).
    pub header: Option<ExportHeader>,
    /// Each region imported, in order, as the source described it (an
    /// append region's `durable_len` is where its restored writer resumes).
    pub regions: Vec<RegionInfo>,
    /// The source's bytes written.
    pub bytes: u64,
    /// The region being imported when the import stopped: its index in the
    /// stream, and its name once the source gave it.
    pub index: Option<u32>,
    /// See [`Self::index`].
    pub region: Option<String>,
    /// The store was renamed into place.
    pub complete: bool,
}

/// An import that stopped: why, the source's own error if it was the
/// source's, and how far it got. Nothing was renamed into place, except
/// after a `DurabilityUnknown` from the directory sync that follows the
/// rename: the store is then visible under the target name (whole, its
/// directory entry perhaps not yet durable) and `complete` is still false.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ImportError {
    /// Why it stopped.
    pub error: Error,
    /// The source's error, if the source stopped it.
    pub source: Option<SinkError>,
    /// How far it got.
    pub report: ImportReport,
}

impl ImportError {
    /// An import that stopped before its source was asked anything (the
    /// facade's: the native backend unavailable).
    #[must_use]
    pub fn from_error(error: Error) -> Self {
        Self {
            error,
            source: None,
            report: ImportReport::default(),
        }
    }
}

impl core::fmt::Display for ImportError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "import stopped: {}", self.error)
    }
}

impl std::error::Error for ImportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// A source's error as the import's: the failure the source recorded on
/// the call when it is a reference source (its own error, a `Corruption`
/// among them), else `Io(Read)` for [`SinkError::Io`] and
/// `NotWritten(Interrupted)` otherwise.
fn sourced(call: &CallState, e: SinkError) -> Error {
    if let Some(f) = call.failure() {
        return f.error;
    }
    match e {
        SinkError::Io(raw) => Error::Io {
            op: Op::Read,
            raw,
            ctx: ErrorContext::default(),
        },
        SinkError::Refused | SinkError::Other(_) => {
            not_written(NotWrittenCause::Interrupted, ErrorContext::default())
        }
    }
}

/// Imports the export `source` yields into a new store in `dir`, whose
/// directory must hold no store: see the module docs. The store is renamed
/// into place and closed; the caller opens it.
///
/// # Errors
///
/// An [`ImportError`] (boxed: it holds the report) with how far it got,
/// nothing renamed (but see `DurabilityUnknown` below): the source's errors (in `source`; a
/// [`FileSource`](crate::FileSource)'s `Corruption(Metadata)` for a
/// directory that is not a whole export or a span that does not match its
/// CRC); `NotWritten(InvalidOptions)` for a header or region the target
/// cannot take ([`RegionInfo::validate`], a name twice, sizes past the
/// header's total), bytes out of order, short or long, or more or fewer
/// regions than the header counts; `NotWritten(Misaligned)` if the target's
/// device needs larger blocks than the export's; `NoSpace` if the target's
/// free space is short of the header's total (nothing written); as
/// `copy_store` for the target directory (`AlreadyExists`, `Locked`, `Io`)
/// and for building the store and its rename: `DurabilityUnknown` if the
/// rename failed (the store may or may not be in place) or the directory
/// sync after it did (the store is in place, visible under the target
/// name, its entry perhaps not yet durable; `complete` is false: open it,
/// or import again, where `AlreadyExists` means it is there).
pub fn import<P: Platform + Clone>(
    platform: P,
    dir: &Path,
    source: &mut dyn ImportSource,
    opts: ImportOptions,
) -> Result<ImportReport, Box<ImportError>>
where
    P::Queue: Send,
{
    // One call: the source's reads and the target's writes its parts, each
    // charged to the call's record on its own device group (R5.4).
    let call = CallState::detached();
    let _bound = call::bind(&call);
    let mut report = ImportReport::default();
    let from = Cell::new(None);
    match import_run(&platform, dir, source, &opts, &call, &from, &mut report) {
        Ok(()) => {
            report.index = None;
            report.region = None;
            report.complete = true;
            Ok(report)
        }
        Err(error) => Err(Box::new(ImportError {
            error,
            source: from.get(),
            report,
        })),
    }
}

fn import_run<P: Platform + Clone>(
    platform: &P,
    dir: &Path,
    source: &mut dyn ImportSource,
    opts: &ImportOptions,
    call: &CallState,
    from: &Cell<Option<SinkError>>,
    report: &mut ImportReport,
) -> Result<(), Error>
where
    P::Queue: Send,
{
    let src = |e: SinkError| {
        from.set(Some(e));
        sourced(call, e)
    };
    let header = source.header().map_err(src)?;
    report.header = Some(header.clone());
    check_header(&header)?;
    let target = TargetDir::open(platform, dir)?;
    {
        let mut dst = Restore::create(platform, dir, &target, &header, &opts.store, call)?;
        for i in 0..header.region_count {
            report.index = Some(i);
            report.region = None;
            call.enter(i, report.bytes);
            let Some(info) = source.next_region().map_err(src)? else {
                // Fewer regions than the header counts.
                return Err(invalid());
            };
            report.region = Some(info.name.clone());
            dst.admit(&info)?;
            let n = import_region(&dst, &info, source, &src)?;
            report.bytes += n;
            report.regions.push(info);
            call.leave(i.saturating_add(1));
        }
        report.index = None;
        report.region = None;
        // More regions than the header counts.
        if source.next_region().map_err(src)?.is_some() {
            return Err(invalid());
        }
        // Closed here (the drop writes the clean-close record).
        dst.close()?;
    }
    // Into place: the rename is the import's commit.
    target.commit(platform)
}

/// Fills `dst` from `source` in reads of at most `piece`: exactly its
/// length, else `NotWritten(InvalidOptions)` (the source short, or
/// answering more than it was asked for).
fn pull(
    source: &mut dyn ImportSource,
    dst: &mut [u8],
    piece: usize,
    src: &dyn Fn(SinkError) -> Error,
) -> Result<(), Error> {
    let mut done = 0usize;
    while done < dst.len() {
        let want = (dst.len() - done).min(piece);
        let n = source.read(&mut dst[done..done + want]).map_err(src)?;
        if n == 0 || n > want {
            return Err(invalid());
        }
        done += n;
    }
    Ok(())
}

/// The region's bytes end exactly where its info says: one more read must
/// find nothing.
fn ended(source: &mut dyn ImportSource, src: &dyn Fn(SinkError) -> Error) -> Result<(), Error> {
    let mut one = [0u8; 1];
    match source.read(&mut one).map_err(src)? {
        0 => Ok(()),
        _ => Err(invalid()),
    }
}

/// Restores admitted `info`'s region from `source`: an append or page
/// region's bytes read into its fill's pieces in place, its pattern past
/// them; a slot's object read whole, then committed. Returns the source's
/// bytes written.
fn import_region<P: Platform + Clone>(
    dst: &Restore<P>,
    info: &RegionInfo,
    source: &mut dyn ImportSource,
    src: &dyn Fn(SinkError) -> Error,
) -> Result<u64, Error>
where
    P::Queue: Send,
{
    let len = info.stream_len();
    let piece = dst.read_piece();
    if info.kind == RegionKind::Slot {
        let mut object = vec![0u8; len as usize];
        pull(source, &mut object, piece, src)?;
        ended(source, src)?;
        dst.slot(info, &object)?;
        return Ok(len);
    }
    let mut made = dst.region(info)?;
    let mut left = len;
    while left > 0 {
        let room = made.room()?;
        // Past the data area (a backstop: `validate` holds a stream's
        // length within its data size).
        if room.is_empty() {
            return Err(crate::store::corrupt());
        }
        let n = room.len().min(usize::try_from(left).unwrap_or(usize::MAX));
        pull(source, &mut room[..n], piece, src)?;
        made.filled(n)?;
        left -= n as u64;
    }
    ended(source, src)?;
    let _live = made.finish()?;
    Ok(len)
}
