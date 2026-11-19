//! `FileSource` (PLANNING R5.6, D-169): an [`ImportSource`] reading back the
//! directory a [`FileSink`](crate::FileSink) wrote, for [`import`](crate::import).
//!
//! **Nothing is trusted.** `regions.info` is checked whole against its
//! CRC-32C first, read in bounded pieces; then its header and lists are
//! decoded and each region's record at its turn, every count bounded by
//! the bytes left, and after the last region nothing may follow but the
//! CRC, and every region the facts say was verified must be one exported
//! in full. Each data file must be exactly its region's stream length, and
//! each 1 MiB span of it is checked against its CRC-32C before any of its
//! bytes is handed out. A short, missing or undecodable `regions.info`, or
//! any mismatch, is `Corruption(Metadata)`: the directory is not
//! restorable, and the import renames nothing.
//!
//! **Reads.** Direct, through one staging buffer, each one admitted by the
//! import's record on the source's device group and completed before the
//! call that made it returns: the source holds no piece between calls, so
//! it registers no collector (R5.4).

use std::collections::HashMap;
use std::marker::PhantomData;
use std::path::Path;
use std::rc::Rc;

use store_io_buf::{BufPool, PoolConfig};
use store_io_core::errno::is_not_found;
use store_io_core::error::{ByteRange, CorruptionKind, Error, ErrorContext, Named, Op};
use store_io_format::crc32c::{crc32c, crc32c_append};
use store_io_format::exportinfo::{
    self, HEADER_LEN, InfoHeader, MAX_LEN, RECORD_LEN, RegionRecord, SPAN,
};
use store_io_platform::{FileMode, FileName, IoOp, Platform, QueueConfig};

use crate::call::{self, CallState};
use crate::exec::{QueueSet, Ran, run};
use crate::export::{ExportHeader, ImportSource, RegionInfo, SinkError};
use crate::file_sink::REGIONS_INFO;
use crate::io::not_written;
use crate::pace::Pace;
use crate::store::StoreOptions;

/// How a [`FileSource`] reads.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct FileSourceOptions {
    /// Its pacing on the source's device group: the background fields
    /// ([`StoreOptions::background_io`], pacing and the rest), as a store
    /// on that device would take them.
    pub store: StoreOptions,
}

/// The largest read when the source is not paced.
const UNPACED_PIECE: usize = 256 << 10;
/// The window `regions.info`'s records are read through.
const WINDOW: usize = 64 << 10;
/// The most items any of `regions.info`'s lists may hold (the format's
/// table bound): a hostile count is refused before it is read.
const MAX_ITEMS: u32 = crate::v2::MAX_REGION_CAPACITY;

/// Where the source is in its calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Fresh,
    Begun,
    InRegion,
    Done,
    Failed,
}

/// A region's data file name (as `FileSink` names it).
fn data_name(index: u32) -> String {
    format!("region-{index:05}.data")
}

/// What `regions.info` or a data file says that a restorable export would
/// not.
fn bad(range: Option<(u64, u64)>) -> Error {
    Error::Corruption {
        kind: CorruptionKind::Metadata,
        ctx: ErrorContext {
            region: None,
            range: range.map(|(start, end)| ByteRange { start, end }),
        },
    }
}

fn io(op: Op, raw: store_io_core::error::OsError) -> Error {
    Error::Io {
        op,
        raw,
        ctx: ErrorContext::default(),
    }
}

/// The source error a failure shows its caller.
fn shown(e: &Error) -> SinkError {
    match e {
        Error::Io { raw, .. } => SinkError::Io(*raw),
        _ => SinkError::Refused,
    }
}

/// An export read back from a directory: see the module docs. Not `Send`:
/// it is bound to the import's call on the thread that made it.
pub struct FileSource<P: Platform> {
    platform: P,
    dir: P::Dir,
    opts: StoreOptions,
    state: State,
    call: Option<Rc<CallState>>,
    pace: Option<Pace>,
    queue: Option<QueueSet<P::Queue>>,
    pool: Option<BufPool>,
    /// The read block (the larger of the file system's and the export's)
    /// and the staging buffer's size.
    block: u64,
    piece: usize,
    info: P::File,
    info_len: u64,
    header: Option<ExportHeader>,
    /// A window of `regions.info`, and where it starts.
    window: Vec<u8>,
    win_at: u64,
    /// The next region record's offset in `regions.info`.
    pos: u64,
    index: u32,
    /// The regions exported in full so far (the verified list's check).
    full: HashMap<String, u64>,
    /// A CRC-32C of `regions.info`'s bytes `[0, crc_at)` as they were
    /// handed out after the CRC pass, which must end equal to the record's
    /// CRC (`want_crc`): the bytes decoded are the bytes checked.
    run_crc: u32,
    crc_at: u64,
    want_crc: u32,
    /// The current region: its file, stream length, span CRCs' offset.
    file: Option<P::File>,
    stream_len: u64,
    spans_at: u64,
    /// The current span's bytes (verified), and how far handed out.
    span: Vec<u8>,
    span_index: u64,
    span_used: usize,
    span_filled: usize,
    read_at: u64,
    _thread: PhantomData<*const ()>,
}

impl<P: Platform> core::fmt::Debug for FileSource<P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FileSource")
            .field("state", &self.state)
            .field("index", &self.index)
            .finish_non_exhaustive()
    }
}

impl<P: Platform> FileSource<P> {
    /// A source reading the export in the directory at `dir`.
    ///
    /// # Errors
    ///
    /// `NotFound(Store)` if the directory does not exist,
    /// `Corruption(Metadata)` if it holds no `regions.info` (not a finished
    /// export), `Io` if it cannot be opened or probed.
    pub fn open(platform: P, dir: &Path, opts: FileSourceOptions) -> Result<Self, Error> {
        let d = platform.open_dir(dir, false).map_err(|raw| {
            if is_not_found(raw) {
                Error::NotFound { what: Named::Store }
            } else {
                io(Op::Open, raw)
            }
        })?;
        let name = FileName::new(REGIONS_INFO).map_err(|_| bad(None))?;
        let info = platform
            .open_file(&d, &name, FileMode::ReadOnly)
            .map_err(|raw| {
                if is_not_found(raw) {
                    bad(None)
                } else {
                    io(Op::Open, raw)
                }
            })?;
        let info_len = platform.size(&info).map_err(|raw| io(Op::Open, raw))?;
        Ok(Self {
            platform,
            dir: d,
            opts: opts.store,
            state: State::Fresh,
            call: None,
            pace: None,
            queue: None,
            pool: None,
            block: 0,
            piece: 0,
            info,
            info_len,
            header: None,
            window: Vec::new(),
            win_at: 0,
            pos: 0,
            index: 0,
            full: HashMap::new(),
            run_crc: 0,
            crc_at: 0,
            want_crc: 0,
            file: None,
            stream_len: 0,
            spans_at: 0,
            span: Vec::new(),
            span_index: 0,
            span_used: 0,
            span_filled: 0,
            read_at: 0,
            _thread: PhantomData,
        })
    }

    /// Fails the source (sticky) with `e`, reported to the import's call
    /// at the bytes of the current region handed out.
    fn fail(&mut self, e: Error) -> SinkError {
        self.state = State::Failed;
        let s = shown(&e);
        if let Some(c) = &self.call {
            c.part_failed(e, self.read_at);
        }
        s
    }

    /// Reads `dst.len()` bytes of `file` at `offset` (a multiple of the
    /// read block) through the staging buffer, each piece admitted and
    /// completed in turn. Returns the bytes read: fewer only at the file's
    /// end.
    fn read_into(&self, file: &P::File, offset: u64, dst: &mut [u8]) -> Result<usize, Error> {
        let (Some(queue), Some(pool)) = (self.queue.as_ref(), self.pool.as_ref()) else {
            return Err(bad(None));
        };
        let block = self.block as usize;
        let mut done = 0usize;
        while done < dst.len() {
            let want = (dst.len() - done).min(self.piece);
            let req = want.div_ceil(block) * block;
            let mut buf = pool.take(self.piece).map_err(|_| {
                not_written(
                    store_io_core::error::NotWrittenCause::PoolExhausted,
                    ErrorContext::default(),
                )
            })?;
            if !buf.set_len(req) {
                return Err(bad(None));
            }
            let _held = self.pace.as_ref().map(Pace::admit).transpose()?;
            let mut lane = queue.get(0);
            let ran = run(
                &mut lane,
                IoOp::Read {
                    file,
                    offset: offset + done as u64,
                    buf,
                },
            );
            drop(lane);
            let (buf, n) = match ran {
                Ok(Ran::Done { buf, result }) => (buf, result.map_err(|raw| io(Op::Read, raw))?),
                Ok(Ran::Abandoned(raw)) => return Err(io(Op::Read, raw)),
                Err((_, raw)) => {
                    return Err(raw.map_or_else(
                        || {
                            not_written(
                                store_io_core::error::NotWrittenCause::QueueFull,
                                ErrorContext::default(),
                            )
                        },
                        |r| io(Op::Read, r),
                    ));
                }
            };
            let Some(buf) = buf else {
                return Err(bad(None));
            };
            let got = n.min(want);
            dst[done..done + got].copy_from_slice(&buf.as_slice()[..got]);
            done += got;
            if n < want {
                break;
            }
        }
        Ok(done)
    }

    /// `n` bytes of `regions.info` at `at`, through the window (whole, or
    /// `Corruption`).
    fn info_bytes(&mut self, at: u64, n: usize) -> Result<&[u8], Error> {
        let end = at.checked_add(n as u64).ok_or_else(|| bad(None))?;
        if end > self.info_len {
            return Err(bad(Some((at, end))));
        }
        let have = at >= self.win_at && end <= self.win_at + self.window.len() as u64;
        // Bytes the running CRC already covers are never fetched again and
        // handed out unchecked: only the head is read from its start twice,
        // and its header is compared with the first read's.
        if !have && at < self.crc_at && at != 0 {
            return Err(bad(Some((at, self.crc_at))));
        }
        if !have {
            let start = at / self.block * self.block;
            let len = (end - start).max(WINDOW as u64).min(self.info_len - start) as usize;
            let mut w = std::mem::take(&mut self.window);
            w.resize(len, 0);
            let got = self.read_into(&self.info, start, &mut w)?;
            if got != len {
                return Err(bad(Some((start, start + len as u64))));
            }
            self.window = w;
            self.win_at = start;
        }
        let i = (at - self.win_at) as usize;
        // The bytes past what the running CRC covers, in order (a gap
        // leaves it short, and the last region's check refuses it).
        if at <= self.crc_at && end > self.crc_at {
            let from = (self.crc_at - self.win_at) as usize;
            self.run_crc = crc32c_append(self.run_crc, &self.window[from..i + n]);
            self.crc_at = end;
        }
        Ok(&self.window[i..i + n])
    }

    /// The staging piece for reads in blocks of `block`: the admitted
    /// piece rounded down to the block, at least one block, at most a span.
    fn piece_for(&self, block: u64) -> usize {
        let piece = self.pace.as_ref().map_or(UNPACED_PIECE, Pace::piece) as u64;
        (piece / block * block).max(block).min(SPAN) as usize
    }

    /// A staging pool of two buffers that hold the current piece (its
    /// classes are powers of two; the piece need not be).
    fn make_pool(&mut self, align: usize) -> Result<(), Error> {
        let size = self.piece.next_power_of_two();
        let pool =
            BufPool::new(&PoolConfig::uniform(align.min(size), size, size, 2)).map_err(|_| {
                not_written(
                    store_io_core::error::NotWrittenCause::PoolExhausted,
                    ErrorContext::default(),
                )
            })?;
        self.pool = Some(pool);
        Ok(())
    }

    fn header_run(&mut self) -> Result<ExportHeader, Error> {
        self.call = call::current();
        let ev = self
            .platform
            .probe(&self.dir, &self.info)
            .map_err(|raw| io(Op::Probe, raw))?;
        let fs_block = 1u64 << crate::layout::choose_log2_block(&ev, None);
        // The CRC pass reads in the file system's blocks; the regions in
        // the larger of those and the export's (as `FileSink` wrote them).
        self.block = fs_block;
        let qos = crate::store::join_group(&self.platform, &ev, &self.opts, fs_block);
        let clock = self.platform.clock();
        self.pace = match &self.call {
            Some(c) => c.pace_for(&qos, &clock),
            None => Some(Pace::new(&qos, clock)),
        };
        self.piece = self.piece_for(fs_block);
        let align = [
            4096,
            ev.fs.dio_mem_align as usize,
            ev.device.logical_block as usize,
        ]
        .into_iter()
        .max()
        .unwrap_or(4096)
        .next_power_of_two();
        self.make_pool(align)?;
        let queue = self
            .platform
            .queue(QueueConfig {
                depth: 1,
                ..QueueConfig::default()
            })
            .map_err(|raw| io(Op::Queue, raw))?;
        self.queue = Some(QueueSet::new(vec![queue], 1));
        // The whole record against its CRC-32C, in bounded pieces.
        let len = self.info_len;
        if len < (HEADER_LEN + 4) as u64 || len > MAX_LEN {
            return Err(bad(Some((0, len))));
        }
        let body = len - 4;
        let mut buf = vec![0u8; SPAN as usize];
        let mut crc = 0u32;
        let mut tail = [0u8; 4];
        let mut at = 0u64;
        while at < len {
            let n = (len - at).min(SPAN) as usize;
            let got = self.read_into(&self.info, at, &mut buf[..n])?;
            if got != n {
                return Err(bad(Some((at, at + n as u64))));
            }
            let b = body.saturating_sub(at).min(n as u64) as usize;
            crc = crc32c_append(crc, &buf[..b]);
            for (i, &x) in buf[b..n].iter().enumerate() {
                tail[(at + b as u64 - body) as usize + i] = x;
            }
            at += n as u64;
        }
        if crc != u32::from_le_bytes(tail) {
            return Err(bad(Some((0, len))));
        }
        self.want_crc = crc;
        // The header and lists, every count bounded before it is read.
        let h = InfoHeader::decode(self.info_bytes(0, HEADER_LEN)?).map_err(|_| bad(None))?;
        let counts = [
            h.region_count,
            h.omitted_count,
            h.lost_count,
            h.stale_count,
            h.verified_count,
        ];
        if counts.iter().any(|&c| c > MAX_ITEMS) {
            return Err(bad(None));
        }
        let head = exportinfo::head_len(&h).ok_or_else(|| bad(None))?;
        if head as u64 > body {
            return Err(bad(Some((0, head as u64))));
        }
        let header = {
            let bytes = self.info_bytes(0, head)?;
            let (view, used) = exportinfo::decode_prefix(bytes).map_err(|_| bad(None))?;
            if used != head {
                return Err(bad(None));
            }
            // A head past the window was read again from its start, its
            // first bytes outside the running CRC: the header must be the
            // one the first read decoded.
            if view.header != h {
                return Err(bad(Some((0, HEADER_LEN as u64))));
            }
            ExportHeader::from_view(&view)
        };
        // The regions' reads in the larger block, their pool remade if
        // the piece changed with it.
        self.block = fs_block.max(header.block_size);
        let piece = self.piece_for(self.block);
        if piece != self.piece {
            self.piece = piece;
            self.make_pool(align)?;
        }
        self.window = Vec::new();
        self.win_at = 0;
        self.pos = head as u64;
        self.span = vec![0u8; SPAN as usize];
        self.header = Some(header.clone());
        Ok(header)
    }

    fn next_run(&mut self) -> Result<Option<RegionInfo>, Error> {
        let Some(header) = self.header.clone() else {
            return Err(bad(None));
        };
        self.file = None;
        let body = self.info_len - 4;
        if self.index == header.region_count {
            // What was decoded is what the CRC pass checked, and all of it:
            // the file did not change between the passes, and nothing
            // follows the last region but the CRC (the decoded bytes end at
            // the body's end). Every region the facts say was verified is
            // one exported in full.
            if self.crc_at != body || self.run_crc != self.want_crc {
                return Err(bad(Some((0, body))));
            }
            let all = header
                .facts
                .verified_up_to
                .iter()
                .all(|(n, len)| self.full.get(n).is_some_and(|&stream| *len <= stream));
            if !all {
                return Err(bad(None));
            }
            return Ok(None);
        }
        let rec = RegionRecord::decode(self.info_bytes(self.pos, RECORD_LEN)?)
            .map_err(|_| bad(Some((self.pos, self.pos + RECORD_LEN as u64))))?;
        let spans_at = self.pos + RECORD_LEN as u64;
        let next = rec
            .spans()
            .checked_mul(4)
            .and_then(|n| spans_at.checked_add(n))
            .filter(|&n| n <= body)
            .ok_or_else(|| bad(Some((self.pos, body))))?;
        // Decoded, then validated for the export's block, before any of
        // its files is opened.
        let info = RegionInfo::from_record(&rec, header.block_size);
        info.validate(header.block_size)
            .map_err(|_| bad(Some((self.pos, spans_at))))?;
        let name = FileName::new(&data_name(self.index)).map_err(|_| bad(None))?;
        let file = self
            .platform
            .open_file(&self.dir, &name, FileMode::ReadOnly)
            .map_err(|raw| {
                if is_not_found(raw) {
                    bad(None)
                } else {
                    io(Op::Open, raw)
                }
            })?;
        let size = self.platform.size(&file).map_err(|raw| io(Op::Open, raw))?;
        if size != rec.stream_len {
            return Err(bad(Some((
                rec.stream_len.min(size),
                rec.stream_len.max(size),
            ))));
        }
        if rec.full_copy {
            let _was = self.full.insert(info.name.clone(), rec.stream_len);
        }
        self.pos = next;
        self.index += 1;
        self.file = Some(file);
        self.stream_len = rec.stream_len;
        self.spans_at = spans_at;
        self.span_index = 0;
        self.span_used = 0;
        self.span_filled = 0;
        self.read_at = 0;
        Ok(Some(info))
    }

    fn read_run(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        if self.read_at == self.stream_len || buf.is_empty() {
            return Ok(0);
        }
        if self.span_used == self.span_filled {
            // The next span, whole, checked before any of it is handed out.
            let start = self.span_index * SPAN;
            let len = (self.stream_len - start).min(SPAN) as usize;
            let mut span = std::mem::take(&mut self.span);
            let got = match self.file.as_ref() {
                Some(f) => self.read_into(f, start, &mut span[..len]),
                None => Err(bad(None)),
            };
            let got = match got {
                Ok(n) => n,
                Err(e) => {
                    self.span = span;
                    return Err(e);
                }
            };
            let sum = crc32c(&span[..len]);
            self.span = span;
            if got != len {
                return Err(bad(Some((start + got as u64, start + len as u64))));
            }
            let at = self.spans_at + 4 * self.span_index;
            let b = self.info_bytes(at, 4)?;
            let want = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            if sum != want {
                return Err(bad(Some((start, start + len as u64))));
            }
            self.span_index += 1;
            self.span_filled = len;
            self.span_used = 0;
        }
        let n = buf.len().min(self.span_filled - self.span_used);
        buf[..n].copy_from_slice(&self.span[self.span_used..self.span_used + n]);
        self.span_used += n;
        self.read_at += n as u64;
        Ok(n)
    }
}

impl<P: Platform> ImportSource for FileSource<P> {
    fn header(&mut self) -> Result<ExportHeader, SinkError> {
        if self.state != State::Fresh {
            return Err(self.fail(bad(None)));
        }
        match self.header_run() {
            Ok(h) => {
                self.state = State::Begun;
                Ok(h)
            }
            Err(e) => Err(self.fail(e)),
        }
    }

    fn next_region(&mut self) -> Result<Option<RegionInfo>, SinkError> {
        // The current region read to its end first.
        let ready = match self.state {
            State::Begun => true,
            State::InRegion => self.read_at == self.stream_len,
            _ => false,
        };
        if !ready {
            return Err(self.fail(bad(None)));
        }
        match self.next_run() {
            Ok(Some(i)) => {
                self.state = State::InRegion;
                Ok(Some(i))
            }
            Ok(None) => {
                self.state = State::Done;
                Ok(None)
            }
            Err(e) => Err(self.fail(e)),
        }
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<usize, SinkError> {
        if self.state != State::InRegion {
            return Err(self.fail(bad(None)));
        }
        match self.read_run(buf) {
            Ok(n) => Ok(n),
            Err(e) => Err(self.fail(e)),
        }
    }
}
