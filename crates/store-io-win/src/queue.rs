//! The IOCP queue: overlapped `WriteFile` / `ReadFile` reaped from a
//! per-queue completion port.
//!
//! ## Port association
//!
//! A file object can be associated with one completion port only. Rather
//! than sharing one port between queues (whose threads would then reap each
//! other's packets) or restricting a file to one queue, every queue opens
//! its own handle to each file it touches with `ReOpenFile` (same file, no
//! path re-resolution, `NO_BUFFERING | OVERLAPPED`) and associates that
//! handle with its own port. Any number of queues may therefore submit to
//! the same file, and each reaps exactly its own completions. The per-file
//! handle is created on a file's first use by a queue (a setup event, not a
//! per-operation cost) and kept until the queue drops or the table is full
//! and the entry is idle.
//!
//! ## Synchronous completion
//!
//! Every per-queue handle has `FILE_SKIP_COMPLETION_PORT_ON_SUCCESS` set, so
//! a `WriteFile` that returns `TRUE` has completed and will queue no packet:
//! the queue completes it inline. `ERROR_IO_PENDING` means a packet will
//! arrive. Any other synchronous failure is reported as a completion with
//! that error (not as a `submit` rejection), because the backend cannot
//! prove such a request never reached the device.
//!
//! ## `dsync` and `FlushData`
//!
//! Windows has no certified per-write durability flag (`WRITE_THROUGH`'s
//! FUA may be dropped below the API). A `dsync` write therefore performs
//! the write and then the file's data flush (`FlushMode`) before it is
//! reported complete; the flush runs inline where the completion is
//! observed (`submit` for a synchronous completion, `reap` / `wait`
//! otherwise). `FlushData` is synchronous and runs inside `submit`; its
//! completion is queued on an internal ready list.
//!
//! ## Lifetime
//!
//! Buffers are never touched or released while the kernel owns them.
//! Dropping a queue with operations in flight cancels them and then waits
//! for every completion packet before any buffer or handle is released.

use std::collections::VecDeque;
use std::sync::Arc;

use store_io_platform::{
    Completion, CompletionBuf, IoBuf, IoOp, Queue, QueueConfig, RawResult, Rejected,
};
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, ERROR_TOO_MANY_OPEN_FILES, GENERIC_READ,
    GENERIC_WRITE,
};
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_NO_BUFFERING, FILE_FLAG_OVERLAPPED};

use crate::file::{self, FlushMode, WinFile};
use crate::sys::{self, Entries, Handle, Issued, Overlapped, Packet};

/// Per-queue file handles kept at once.
const FILES_PER_QUEUE: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Write { dsync: bool },
    Read,
    Flush,
}

#[derive(Debug)]
enum State {
    Free,
    InKernel,
    Ready(RawResult<usize>),
}

#[derive(Debug)]
struct Slot {
    ov: Overlapped,
    state: State,
    tag: u64,
    kind: Kind,
    buf: Option<IoBuf>,
    entry: usize,
}

#[derive(Debug)]
struct FileEntry {
    id: u64,
    handle: Handle,
    /// The file's synchronous flush handle (never this queue's overlapped
    /// one: see `crate::file`).
    flush: Option<Arc<Handle>>,
    flush_mode: FlushMode,
    in_flight: u32,
}

/// A per-thread IOCP submission queue (see the module docs).
#[derive(Debug)]
pub struct WinQueue {
    port: Handle,
    slots: Box<[Slot]>,
    free: Vec<u32>,
    ready: VecDeque<u32>,
    entries: Entries,
    files: Vec<Option<FileEntry>>,
    kernel_pending: usize,
    draining: bool,
}

impl WinQueue {
    pub(crate) fn new(cfg: QueueConfig) -> RawResult<Self> {
        let depth = cfg.depth.max(1) as usize;
        let port = sys::create_port()?;
        let slots: Vec<Slot> = (0..depth)
            .map(|_| Slot {
                ov: Overlapped::new(),
                state: State::Free,
                tag: 0,
                kind: Kind::Flush,
                buf: None,
                entry: 0,
            })
            .collect();
        Ok(Self {
            port,
            slots: slots.into_boxed_slice(),
            free: (0..depth as u32).rev().collect(),
            ready: VecDeque::with_capacity(depth),
            entries: Entries::new(depth),
            files: Vec::with_capacity(FILES_PER_QUEUE),
            kernel_pending: 0,
            draining: false,
        })
    }

    /// Operations whose packets the kernel still owes.
    #[must_use]
    pub fn kernel_pending(&self) -> usize {
        self.kernel_pending
    }

    /// Files this queue currently holds its own handle to.
    #[must_use]
    pub fn open_files(&self) -> usize {
        self.files.iter().flatten().count()
    }

    /// Index of this queue's handle for `file`, opening it on first use.
    fn entry_for(&mut self, file: &WinFile) -> RawResult<usize> {
        if let Some(i) = self
            .files
            .iter()
            .position(|e| e.as_ref().is_some_and(|e| e.id == file.id()))
        {
            return Ok(i);
        }
        let slot = match self.files.iter().position(Option::is_none) {
            Some(i) => i,
            None if self.files.len() < FILES_PER_QUEUE => {
                self.files.push(None);
                self.files.len() - 1
            }
            None => {
                let idle = self
                    .files
                    .iter()
                    .position(|e| e.as_ref().is_some_and(|e| e.in_flight == 0))
                    .ok_or(sys::win32(ERROR_TOO_MANY_OPEN_FILES))?;
                self.files[idle] = None;
                idle
            }
        };
        let access = if file.is_writable() {
            GENERIC_READ | GENERIC_WRITE
        } else {
            GENERIC_READ
        };
        let handle = sys::reopen(
            file.handle(),
            access,
            sys::SHARE_ALL,
            FILE_FLAG_NO_BUFFERING | FILE_FLAG_OVERLAPPED,
        )?;
        sys::associate(&handle, &self.port, 0)?;
        self.files[slot] = Some(FileEntry {
            id: file.id(),
            handle,
            flush: file.flush_handle().cloned(),
            flush_mode: file.flush_mode(),
            in_flight: 0,
        });
        Ok(slot)
    }

    fn entry(&self, i: usize) -> Option<&FileEntry> {
        self.files.get(i).and_then(Option::as_ref)
    }

    /// Fills a free slot, or hands the buffer back when the queue is full.
    fn take_slot(
        &mut self,
        tag: u64,
        kind: Kind,
        buf: Option<IoBuf>,
        entry: usize,
        offset: u64,
    ) -> Result<usize, Rejected> {
        let Some(idx) = self.free.pop() else {
            return Err(Rejected { buf, raw: None });
        };
        let s = &mut self.slots[idx as usize];
        s.ov.reset(offset);
        s.state = State::InKernel;
        s.tag = tag;
        s.kind = kind;
        s.buf = buf;
        s.entry = entry;
        Ok(idx as usize)
    }

    /// Records the outcome of an issue call.
    fn settle(&mut self, idx: usize, outcome: Issued) {
        match outcome {
            Issued::Pending => {
                self.kernel_pending += 1;
                if let Some(Some(e)) = self.files.get_mut(self.slots[idx].entry) {
                    e.in_flight += 1;
                }
            }
            Issued::Done(n) => self.finish(idx, Ok(n)),
            Issued::Failed(e) => self.finish(idx, Err(e)),
        }
    }

    /// Completes a slot: the trailing flush of a `dsync` write, then the
    /// ready list.
    fn finish(&mut self, idx: usize, result: RawResult<usize>) {
        let result = match (self.slots[idx].kind, result) {
            (Kind::Write { dsync: true }, Ok(n)) if !self.draining => {
                match self.entry(self.slots[idx].entry) {
                    Some(e) => file::flush_data(e.flush.as_deref(), e.flush_mode).map(|()| n),
                    None => Err(sys::win32(ERROR_INVALID_PARAMETER)),
                }
            }
            (_, r) => r,
        };
        self.slots[idx].state = State::Ready(result);
        self.ready.push_back(idx as u32);
    }

    /// Resolves reaped packets to their slots.
    fn process(&mut self, n: usize) {
        let base = self.slots.as_ptr() as usize;
        let stride = size_of::<Slot>();
        for i in 0..n {
            let Some(Packet {
                overlapped,
                status,
                bytes,
            }) = self.entries.get(i)
            else {
                break;
            };
            let idx = overlapped.wrapping_sub(base) / stride;
            let Some(slot) = self.slots.get(idx) else {
                continue;
            };
            if slot.ov.as_ptr() as usize != overlapped || !matches!(slot.state, State::InKernel) {
                continue;
            }
            let entry = slot.entry;
            let result = if status >= 0 {
                Ok(bytes as usize)
            } else {
                match self.entry(entry) {
                    Some(e) => sys::completed_result(&e.handle, &slot.ov),
                    None => Err(sys::nt(status)),
                }
            };
            self.kernel_pending = self.kernel_pending.saturating_sub(1);
            if let Some(Some(e)) = self.files.get_mut(entry) {
                e.in_flight = e.in_flight.saturating_sub(1);
            }
            self.finish(idx, result);
        }
    }

    /// Moves ready completions into `out` while it has room.
    fn drain_ready(&mut self, out: &mut CompletionBuf) -> usize {
        let mut added = 0;
        while out.room() > 0 {
            let Some(idx) = self.ready.pop_front() else {
                break;
            };
            let s = &mut self.slots[idx as usize];
            let result = match core::mem::replace(&mut s.state, State::Free) {
                State::Ready(r) => r,
                State::Free | State::InKernel => Err(sys::win32(ERROR_INVALID_PARAMETER)),
            };
            let c = Completion {
                tag: s.tag,
                buf: s.buf.take(),
                result,
            };
            self.free.push(idx);
            // `room() > 0` was checked, so the push is accepted.
            if let Err(c) = out.push(c) {
                drop(c);
                break;
            }
            added += 1;
        }
        added
    }

    fn reject(buf: Option<IoBuf>, code: u32) -> Rejected {
        Rejected {
            buf,
            raw: Some(sys::win32(code)),
        }
    }
}

impl Queue for WinQueue {
    type File = WinFile;

    fn submit(&mut self, op: IoOp<'_, WinFile>, tag: u64) -> Result<(), Rejected> {
        match op {
            IoOp::Write {
                file,
                offset,
                buf,
                dsync,
            } => {
                if !file.is_writable() {
                    return Err(Self::reject(Some(buf), ERROR_ACCESS_DENIED));
                }
                let Ok(len) = u32::try_from(buf.len()) else {
                    return Err(Self::reject(Some(buf), ERROR_INVALID_PARAMETER));
                };
                let entry = match self.entry_for(file) {
                    Ok(e) => e,
                    Err(raw) => {
                        return Err(Rejected {
                            buf: Some(buf),
                            raw: Some(raw),
                        });
                    }
                };
                let idx = self.take_slot(tag, Kind::Write { dsync }, Some(buf), entry, offset)?;
                let slot = &self.slots[idx];
                let (Some(Some(e)), Some(b)) = (self.files.get(entry), slot.buf.as_ref()) else {
                    self.free.push(idx as u32);
                    return Err(Self::reject(
                        self.slots[idx].buf.take(),
                        ERROR_INVALID_PARAMETER,
                    ));
                };
                // SAFETY: the buffer lives in the slot and the record in the
                // boxed slot table; neither moves or is released until the
                // operation completes (`settle` / `process`); `len == buf.len()`.
                let outcome = unsafe { sys::write_file(&e.handle, b.as_ptr(), len, &slot.ov) };
                self.settle(idx, outcome);
                Ok(())
            }
            IoOp::Read { file, offset, buf } => {
                let Ok(len) = u32::try_from(buf.len()) else {
                    return Err(Self::reject(Some(buf), ERROR_INVALID_PARAMETER));
                };
                let entry = match self.entry_for(file) {
                    Ok(e) => e,
                    Err(raw) => {
                        return Err(Rejected {
                            buf: Some(buf),
                            raw: Some(raw),
                        });
                    }
                };
                let idx = self.take_slot(tag, Kind::Read, Some(buf), entry, offset)?;
                let slot = &mut self.slots[idx];
                let (Some(Some(e)), Some(b)) = (self.files.get(entry), slot.buf.as_mut()) else {
                    let buf = slot.buf.take();
                    self.free.push(idx as u32);
                    return Err(Self::reject(buf, ERROR_INVALID_PARAMETER));
                };
                // SAFETY: as for the write; only the kernel touches the buffer
                // until the completion is observed.
                let outcome = unsafe { sys::read_file(&e.handle, b.as_mut_ptr(), len, &slot.ov) };
                self.settle(idx, outcome);
                Ok(())
            }
            IoOp::FlushData { file } => {
                if !file.is_writable() {
                    return Err(Self::reject(None, ERROR_ACCESS_DENIED));
                }
                let entry = match self.entry_for(file) {
                    Ok(e) => e,
                    Err(raw) => {
                        return Err(Rejected {
                            buf: None,
                            raw: Some(raw),
                        });
                    }
                };
                let idx = self.take_slot(tag, Kind::Flush, None, entry, 0)?;
                let result = match self.entry(entry) {
                    Some(e) => file::flush_data(e.flush.as_deref(), e.flush_mode).map(|()| 0),
                    None => Err(sys::win32(ERROR_INVALID_PARAMETER)),
                };
                self.finish(idx, result);
                Ok(())
            }
        }
    }

    fn reap(&mut self, out: &mut CompletionBuf) -> usize {
        let mut added = self.drain_ready(out);
        if self.kernel_pending > 0 && out.room() > 0 {
            // A failed poll is not lost: `wait` reports the port error.
            if let Ok(n) = sys::get_queued(&self.port, &mut self.entries, false) {
                self.process(n);
            }
            added += self.drain_ready(out);
        }
        added
    }

    fn wait(&mut self, min: usize, out: &mut CompletionBuf) -> RawResult<usize> {
        let mut added = self.drain_ready(out);
        while added < min && out.room() > 0 && self.kernel_pending > 0 {
            let n = sys::get_queued(&self.port, &mut self.entries, true)?;
            self.process(n);
            added += self.drain_ready(out);
        }
        Ok(added)
    }

    fn in_flight(&self) -> usize {
        self.slots.len() - self.free.len()
    }
}

impl Drop for WinQueue {
    fn drop(&mut self) {
        if self.kernel_pending == 0 {
            return;
        }
        self.draining = true;
        for e in self.files.iter().flatten() {
            if e.in_flight > 0 {
                sys::cancel_io(&e.handle);
            }
        }
        while self.kernel_pending > 0 {
            match sys::get_queued(&self.port, &mut self.entries, true) {
                Ok(n) => self.process(n),
                Err(_) => {
                    // The port cannot be waited on: buffers and handles the
                    // kernel may still write to are leaked rather than freed.
                    let slots = core::mem::replace(&mut self.slots, Vec::new().into_boxed_slice());
                    core::mem::forget(slots);
                    let files = core::mem::take(&mut self.files);
                    core::mem::forget(files);
                    return;
                }
            }
        }
    }
}
