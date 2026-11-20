//! `StoreSink` (PLANNING R5.6, D-170): an [`ExportSink`] restoring an
//! export straight into a new store, with no files between.
//!
//! **As [`import`](crate::import) builds its store**, and through the same
//! code (`restore.rs`): at [`StoreSink::create`] the target directory
//! made, its copy lock taken, a store already there refused
//! (`AlreadyExists`) and a crashed copy's or import's temporary removed;
//! at `begin` the header checked, the free space against its total
//! (`NoSpace`, nothing written), and the store created under the temporary
//! name, its writes the export's call's; each region admitted as an
//! import admits it ([`RegionInfo::validate`], its name once, the sizes
//! within the header's total, no more than the header counts) and
//! recreated with its attributes, its bytes written once as its
//! provisioning's fill (durable and verified before its header), an append
//! region's pattern past its durable length, a slot by one commit of its
//! object (an empty one included, none for a slot never committed); at
//! `finish`, every region there, the store flushed, closed and renamed into
//! place by a rename that never replaces. **No store is under the target
//! name unless `finish` returned `Ok`** (but see `DurabilityUnknown` from
//! the directory sync after the rename, as an import's).
//!
//! **What it is pushed is checked**, as `FileSink` checks it: the calls in
//! order, each chunk at its region's next byte and within its stream
//! length, each region's length whole at `end_region`, exactly the regions
//! the header counts. Any refusal or failure is sticky: every later call
//! is refused, the new store closed at once (its temporary removed by the
//! next copy, import or `StoreSink` into the directory).
//!
//! **Writes.** Each piece of a region's fill is filled in place from the
//! chunks (one copy), in whole blocks of the new store and never past its
//! chunk, admitted by the call's record on the target's device group and
//! completed before the call that filled it returns, so the sink holds no
//! piece in flight between calls and no chunk at all (it never needs
//! `settle`). A failure is reported to the export's call with the offset in
//! the region's stream where the failed write began, so the export's report
//! never counts bytes the sink did not write; an admission that ended
//! without one (`NotWritten(Interrupted)`: another part of the call
//! failed, R5.4) is that part's to report, never the sink's.

use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use store_io_core::error::Error;
use store_io_format::meta::RegionKind;
use store_io_platform::Platform;

use crate::call::{self, CallState};
use crate::export::{ExportHeader, ExportSink, RegionInfo, SinkError};
use crate::provision2::Pushed;
use crate::restore::{Restore, check_header, invalid};
use crate::store::StoreOptions;
use crate::target::TargetDir;

/// How a [`StoreSink`] builds its store.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct StoreSinkOptions {
    /// The new store's options (its trust and pacing among them); its
    /// block size and region capacity are the export's.
    pub store: StoreOptions,
}

/// Where the sink is in its calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Fresh,
    Begun,
    InRegion { declared: u64, written: u64 },
    Ended,
    Finished,
    Failed,
}

/// The region being restored.
enum Cur<P: Platform>
where
    P::Queue: Send,
{
    /// An append or page region: its fill, given the chunks.
    Area(Box<Pushed<P>>),
    /// A slot: its object, committed at `end_region`.
    Slot(Vec<u8>),
}

/// The sink error a failure shows its caller: the operating system's
/// error when it carries one.
fn shown(e: &Error) -> SinkError {
    match e {
        Error::Io { raw, .. } | Error::DurabilityUnknown { raw: Some(raw), .. } => {
            SinkError::Io(*raw)
        }
        _ => SinkError::Refused,
    }
}

/// An export restored into a new store: see the module docs. Not `Send`:
/// it is bound to the export's call on the thread that made it.
pub struct StoreSink<P: Platform + Clone>
where
    P::Queue: Send,
{
    platform: P,
    dir: PathBuf,
    opts: StoreSinkOptions,
    state: State,
    /// The export's call, found at `begin` (one of the sink's own when
    /// driven by hand), and the sink's id on it.
    call: Option<Rc<CallState>>,
    id: usize,
    /// This thread's ended admissions when the current call into the sink
    /// began (`pace::admissions_ended`).
    mark: u64,
    /// The export's header, as `begin` was given it.
    header: Option<ExportHeader>,
    /// Dropped in this order: the region's fill (its holds), the store
    /// (its close), then the directory's lock.
    cur: Option<(RegionInfo, Cur<P>)>,
    restore: Option<Restore<P>>,
    target: Option<TargetDir<P>>,
    /// Every region restored, as the export described it (an append
    /// region's `durable_len` is where its restored writer resumes).
    done: Vec<RegionInfo>,
    _thread: PhantomData<*const ()>,
}

impl<P: Platform + Clone> core::fmt::Debug for StoreSink<P>
where
    P::Queue: Send,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StoreSink")
            .field("state", &self.state)
            .field("regions", &self.done.len())
            .finish_non_exhaustive()
    }
}

impl<P: Platform + Clone> StoreSink<P>
where
    P::Queue: Send,
{
    /// A sink restoring into a new store in `dir` (made if missing), which
    /// must hold no store: its copy lock is held from here until the sink
    /// drops.
    ///
    /// # Errors
    ///
    /// As `copy_store` for its target directory: `AlreadyExists` if a
    /// store is there, `Locked` while another copy, import or sink into it
    /// runs, `Io` if it cannot be made or opened.
    pub fn create(platform: P, dir: &Path, opts: StoreSinkOptions) -> Result<Self, Error> {
        let target = TargetDir::open(&platform, dir)?;
        Ok(Self {
            platform,
            dir: dir.to_path_buf(),
            opts,
            state: State::Fresh,
            call: None,
            id: 0,
            mark: 0,
            header: None,
            cur: None,
            restore: None,
            target: Some(target),
            done: Vec::new(),
            _thread: PhantomData,
        })
    }

    /// Every region restored so far, as the export described it: an
    /// append region restored opens unpositioned, as any reopened store's,
    /// and its `durable_len` here is the caller's `resume_at`.
    #[must_use]
    pub fn restored(&self) -> &[RegionInfo] {
        &self.done
    }

    /// The export's header as `begin` was given it, once checked (`None`
    /// before, or for a header the sink refused): its
    /// volume facts say what the export left out (omitted entries, lost or
    /// stale table pages, possibly superseded regions), so a restore never
    /// hides a loss.
    #[must_use]
    pub fn header(&self) -> Option<&ExportHeader> {
        self.header.as_ref()
    }

    /// Whether the store was renamed into place (`finish` returned `Ok`).
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.state == State::Finished
    }

    /// Fails the sink (sticky) with `e`, reporting it to the call at
    /// `offset` of the region being exported (a failed write's start, or
    /// the bytes accepted when a call is refused), closes the new store
    /// unless it is already in place, and returns what its caller is
    /// shown. A failure that came with an ended admission (of any of the
    /// target's admitted writes, since this call into the sink began) is
    /// another part's, reported by it, never the sink's.
    fn fail(&mut self, e: Error, offset: u64) -> SinkError {
        // A store already renamed into place stays complete: a later call
        // is refused, and the call told.
        if self.state != State::Finished {
            self.state = State::Failed;
        }
        let shown = shown(&e);
        let ended = crate::pace::admissions_ended() != self.mark;
        if let Some(c) = &self.call {
            if !ended {
                c.part_failed(e, offset);
            }
        }
        self.cur = None;
        self.restore = None;
        shown
    }

    /// Refuses the call (sticky), at `offset` of the region being exported.
    fn refuse(&mut self, offset: u64) -> SinkError {
        self.fail(invalid(), offset)
    }

    fn begin_run(&mut self, header: &ExportHeader) -> Result<(), Error> {
        // The export's call when bound to one (its failures are then the
        // export's from here on), else one of the sink's own.
        let call = match call::current() {
            Some(c) => {
                self.id = c.sink_bound();
                c
            }
            None => CallState::detached(),
        };
        self.call = Some(Rc::clone(&call));
        check_header(header)?;
        self.header = Some(header.clone());
        let Some(target) = self.target.as_ref() else {
            return Err(invalid());
        };
        self.restore = Some(Restore::create(
            &self.platform,
            &self.dir,
            target,
            header,
            &self.opts.store,
            &call,
        )?);
        Ok(())
    }

    fn region_run(&mut self, info: &RegionInfo) -> Result<(), Error> {
        let Some(restore) = self.restore.as_mut() else {
            return Err(invalid());
        };
        restore.admit(info)?;
        let cur = match info.kind {
            RegionKind::Slot => Cur::Slot(Vec::new()),
            _ => Cur::Area(Box::new(restore.region(info)?)),
        };
        self.cur = Some((info.clone(), cur));
        Ok(())
    }

    /// The offset in the region's stream its bytes are written up to.
    fn written(&self) -> u64 {
        match &self.cur {
            Some((_, Cur::Area(p))) => p.written(),
            _ => 0,
        }
    }

    fn end_run(&mut self) -> Result<(), Error> {
        let Some((info, cur)) = self.cur.as_mut() else {
            return Err(invalid());
        };
        match cur {
            Cur::Area(p) => {
                let _live = p.finish()?;
            }
            Cur::Slot(object) => match self.restore.as_ref() {
                Some(r) => r.slot(info, object)?,
                None => return Err(invalid()),
            },
        }
        if let Some((info, _)) = self.cur.take() {
            self.done.push(info);
        }
        Ok(())
    }

    fn finish_run(&mut self) -> Result<(), Error> {
        let (Some(restore), Some(target)) = (self.restore.take(), self.target.as_ref()) else {
            return Err(invalid());
        };
        // Closed here (the drop writes the clean-close record).
        restore.close()?;
        // Into place: the rename is the restore's commit.
        target.commit(&self.platform)
    }
}

impl<P: Platform + Clone> ExportSink for StoreSink<P>
where
    P::Queue: Send,
{
    fn begin(&mut self, header: &ExportHeader) -> Result<(), SinkError> {
        self.mark = crate::pace::admissions_ended();
        if self.state != State::Fresh {
            return Err(self.refuse(0));
        }
        match self.begin_run(header) {
            Ok(()) => {
                self.state = State::Begun;
                Ok(())
            }
            Err(e) => Err(self.fail(e, 0)),
        }
    }

    fn region(&mut self, info: &RegionInfo) -> Result<(), SinkError> {
        self.mark = crate::pace::admissions_ended();
        if !matches!(self.state, State::Begun | State::Ended) {
            return Err(self.refuse(0));
        }
        match self.region_run(info) {
            Ok(()) => {
                self.state = State::InRegion {
                    declared: info.stream_len(),
                    written: 0,
                };
                Ok(())
            }
            Err(e) => Err(self.fail(e, 0)),
        }
    }

    fn data(&mut self, chunk: crate::stream::Chunk) -> Result<(), SinkError> {
        self.mark = crate::pace::admissions_ended();
        let State::InRegion { declared, written } = self.state else {
            return Err(self.refuse(0));
        };
        let bytes = chunk.bytes();
        let n = bytes.len() as u64;
        // At the region's next byte, within its declared length.
        if chunk.offset() != written || n > declared - written {
            return Err(self.refuse(written));
        }
        let given = match self.cur.as_mut() {
            Some((_, Cur::Area(p))) => p.give(bytes),
            Some((_, Cur::Slot(object))) => {
                object.extend_from_slice(bytes);
                Ok(())
            }
            None => Err(invalid()),
        };
        drop(chunk);
        // Its writes done (bytes held in a piece not yet written count
        // none).
        if let Some(c) = &self.call {
            c.wrote(self.id, self.written().min(declared));
        }
        match given {
            Ok(()) => {
                self.state = State::InRegion {
                    declared,
                    written: written + n,
                };
                Ok(())
            }
            // The failed write's start (its fill moves past a write done).
            Err(e) => {
                let at = self.written();
                Err(self.fail(e, at))
            }
        }
    }

    fn end_region(&mut self) -> Result<(), SinkError> {
        self.mark = crate::pace::admissions_ended();
        let State::InRegion { declared, written } = self.state else {
            return Err(self.refuse(0));
        };
        if written != declared {
            return Err(self.refuse(written));
        }
        // Where its last piece begins: any failure from here is at or
        // past it (a conservative offset, never past a write that failed).
        let at = self.written().min(declared);
        match self.end_run() {
            Ok(()) => {
                self.state = State::Ended;
                if let Some(c) = &self.call {
                    c.region_ended(self.id);
                }
                Ok(())
            }
            Err(e) => Err(self.fail(e, at)),
        }
    }

    fn finish(&mut self) -> Result<(), SinkError> {
        self.mark = crate::pace::admissions_ended();
        if !matches!(self.state, State::Begun | State::Ended) {
            return Err(self.refuse(0));
        }
        match self.finish_run() {
            Ok(()) => {
                self.state = State::Finished;
                if let Some(c) = &self.call {
                    c.sink_finished();
                }
                Ok(())
            }
            Err(e) => Err(self.fail(e, 0)),
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Arc;

    use store_io_format::meta::FillPattern;
    use store_io_sim::{SimConfig, SimPlatform};

    use super::*;
    use crate::Store;
    use crate::export::VolumeFacts;

    /// An admission that ends without one (here: every piece of the call
    /// held on this thread, which no collector can collect) fails the sink
    /// but is not recorded as the sink's failure: the part whose pieces
    /// they are reports its own. A failure of the sink's own, with no
    /// admission ended since the call into the sink began, is recorded.
    #[test]
    fn test_an_admission_ended_by_another_part_is_not_the_sink_s_failure() {
        let p = SimPlatform::new(SimConfig::volatile(5500));
        let s = Store::create(p.clone(), Path::new("/db"), StoreOptions::default()).unwrap();
        let pace = s.inner.domain.new_pace();
        let call = CallState::new(Arc::clone(&s.inner.qos), pace.clone());
        let _bound = call::bind(&call);
        let mut sink =
            StoreSink::create(p.clone(), Path::new("/to"), StoreSinkOptions::default()).unwrap();
        let header = ExportHeader {
            region_count: 1,
            block_size: 4096,
            facts: VolumeFacts::default(),
            writable: true,
            lend: 4,
            total: 1 << 30,
        };
        sink.begin(&header).unwrap();
        let info = RegionInfo {
            name: "r".to_owned(),
            kind: RegionKind::Page,
            generation: 1,
            tag: 0,
            cap: None,
            fill: FillPattern::Zeros,
            fill_key: [0; 16],
            block_size: 4096,
            data_size: 4 << 20,
            durable_len: None,
            object_len: None,
            full_copy: false,
            superseded: false,
        };
        sink.region(&info).unwrap();
        // Every piece the call may hold, held here (another part's).
        let pace = pace.unwrap();
        let mut held = Vec::new();
        while let Some(h) = pace.try_charge() {
            held.push(h);
            assert!(held.len() < 1024, "an admission that never refuses");
        }
        sink.mark = crate::pace::admissions_ended();
        let e = match sink.cur.as_mut() {
            Some((_, Cur::Area(p))) => p.give(&vec![7u8; 2 << 20]).unwrap_err(),
            _ => panic!("no fill"),
        };
        assert_ne!(crate::pace::admissions_ended(), sink.mark);
        let _shown = sink.fail(e, 0);
        assert_eq!(sink.state, State::Failed);
        assert!(call.failure().is_none(), "recorded as the sink's");
        sink.mark = crate::pace::admissions_ended();
        let _shown = sink.fail(invalid(), 0);
        assert!(call.failure().is_some());
        drop(held);
    }

    /// A caller's wrapper that, at the first region's `end_region`, holds
    /// every piece the call may hold on this thread (so its sink's
    /// admission ends without one: another part's, never recorded), then
    /// swallows what the sink returns.
    struct HoldAtEnd {
        inner: Box<dyn ExportSink>,
        qos: Arc<crate::qos::Qos>,
        clock: store_io_platform::Clock,
        ends: usize,
        swallowed: Vec<SinkError>,
        /// `finish` forwarded (else never: no later refusal recorded).
        finishes: bool,
    }

    impl ExportSink for HoldAtEnd {
        fn begin(&mut self, header: &ExportHeader) -> Result<(), SinkError> {
            self.inner.begin(header)
        }
        fn region(&mut self, info: &RegionInfo) -> Result<(), SinkError> {
            if let Err(e) = self.inner.region(info) {
                self.swallowed.push(e);
            }
            Ok(())
        }
        fn data(&mut self, chunk: crate::stream::Chunk) -> Result<(), SinkError> {
            if let Err(e) = self.inner.data(chunk) {
                self.swallowed.push(e);
            }
            Ok(())
        }
        fn end_region(&mut self) -> Result<(), SinkError> {
            self.ends += 1;
            let mut held = Vec::new();
            if self.ends == 1 {
                let c = call::current().expect("the export's call");
                let pace = c.pace_for(&self.qos, &self.clock).expect("paced");
                while let Some(h) = pace.try_charge() {
                    held.push(h);
                    assert!(held.len() < 1024, "an admission that never refuses");
                }
            }
            let r = self.inner.end_region();
            drop(held);
            if let Err(e) = r {
                self.swallowed.push(e);
            }
            Ok(())
        }
        fn finish(&mut self) -> Result<(), SinkError> {
            if !self.finishes {
                return Ok(());
            }
            if let Err(e) = self.inner.finish() {
                self.swallowed.push(e);
            }
            Ok(())
        }
    }

    /// A store of two page regions, the first a piece and a half of the
    /// sinks' pieces (so its `end_region` writes).
    fn two_regions(p: &SimPlatform) -> Store<SimPlatform> {
        let s = Store::create(p.clone(), Path::new("/db"), StoreOptions::default()).unwrap();
        for (name, len) in [("a", 384u64 << 10), ("b", 8 << 10)] {
            let r = s.provision_page_region(name, len).unwrap();
            let _d = r
                .write_durable(r.pos(0).unwrap(), &vec![7u8; len as usize])
                .unwrap();
        }
        s
    }

    /// An admission ended at a region's `end_region` (no part recorded a
    /// failure) through a wrapper that swallows it: the region is never
    /// counted (its sink never ended it), for either reference sink, and
    /// the export is not complete.
    #[test]
    fn test_a_region_a_sink_never_ended_is_not_counted() {
        for (seed, file) in [(5510u64, false), (5511, true)] {
            let p = SimPlatform::new(SimConfig::volatile(seed));
            let s = two_regions(&p);
            let inner: Box<dyn ExportSink> = if file {
                drop(p.open_dir(Path::new("/out"), true).unwrap());
                Box::new(
                    crate::FileSink::create(
                        p.clone(),
                        Path::new("/out"),
                        crate::FileSinkOptions::default(),
                    )
                    .unwrap(),
                )
            } else {
                Box::new(
                    StoreSink::create(p.clone(), Path::new("/to"), StoreSinkOptions::default())
                        .unwrap(),
                )
            };
            let mut w = HoldAtEnd {
                inner,
                qos: Arc::clone(&s.inner.qos),
                clock: p.clock(),
                ends: 0,
                swallowed: Vec::new(),
                finishes: true,
            };
            let e = s.export(&mut w).unwrap_err();
            assert!(!w.swallowed.is_empty(), "file {file}: nothing refused");
            assert!(!e.report.complete);
            assert_eq!(
                e.report.regions, 0,
                "file {file}: a region never ended counted"
            );
        }
    }

    /// The same at a store's only region, its `finish` never forwarded: no
    /// part recorded any failure, the sink never finished, and the region
    /// it never ended is not counted.
    #[test]
    fn test_an_unfinished_export_counts_no_region_its_sink_never_ended() {
        let p = SimPlatform::new(SimConfig::volatile(5512));
        let s = Store::create(p.clone(), Path::new("/db"), StoreOptions::default()).unwrap();
        let r = s.provision_page_region("a", 384 << 10).unwrap();
        let _d = r
            .write_durable(r.pos(0).unwrap(), &vec![7u8; 384 << 10])
            .unwrap();
        let mut w = HoldAtEnd {
            inner: Box::new(
                StoreSink::create(p.clone(), Path::new("/to"), StoreSinkOptions::default())
                    .unwrap(),
            ),
            qos: Arc::clone(&s.inner.qos),
            clock: p.clock(),
            ends: 0,
            swallowed: Vec::new(),
            finishes: false,
        };
        let e = s.export(&mut w).unwrap_err();
        assert!(!w.swallowed.is_empty());
        assert!(e.sink.is_none(), "{:?}", e.sink);
        assert!(!e.report.complete);
        assert_eq!(e.report.regions, 0);
    }
}
