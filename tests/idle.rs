//! Exit criterion (v0.4 on Linux, v0.5 on every system): an idle store makes
//! zero system calls, wakeups, timers or I/O, from the moment it is idle (no
//! exemption window).
//!
//! The store is exercised first (durable appends, a page batch, a
//! `Submitter` with writes and a barrier), so its flush thread and any
//! submit-pool workers exist. Once every thread store-io started is asleep
//! and their counters have stopped moving (the idle moment; nothing is
//! exempted after it), they are watched for `STORE_IO_IDLE_SECS` (default
//! 60). None may exit, none may start, and per system:
//!
//! - Linux, on both tiers: none may be switched in (any wakeup or timer is a
//!   context switch), run on a CPU (scheduler time, so a spin without a
//!   switch is caught), make a read or write system call, or move a byte;
//!   and the process moves no byte to or from storage (ring I/O included),
//!   per `/proc/self/task/<tid>/{status,io,schedstat,stat}` and
//!   `/proc/self/io`.
//! - macOS: none may run on a CPU (user plus system time in Mach ticks,
//!   `PROC_PIDTHREADCOUNTS`, which the kernel charges at every switch, so
//!   any wakeup moves it; before macOS 13, `PROC_PIDTHREADID64INFO`'s whole
//!   microseconds); and the process moves no byte to or from storage, per
//!   `proc_pid_rusage`. macOS keeps no per-thread count of switches or
//!   system calls.
//! - Windows: none may be switched in (the kernel's per-thread count) or run
//!   a CPU cycle (`QueryThreadCycleTime`, kernel mode included), per
//!   `NtQuerySystemInformation(SystemProcessInformation)`. Nothing calls
//!   into an idle store, so only its own threads could start I/O, and a
//!   thread that never runs starts none. Bytes are not counted: the
//!   process's transfer counters also count the test harness's console
//!   output, and the kernel's write-back of cached data would not show.
//!   Threads the system starts in this process (unnamed, beginning in
//!   `ntdll.dll`: its thread pool's workers, which CI runners start now and
//!   then in a process with no store at all) are not watched; store-io
//!   names every thread it starts and never uses the system's thread pool
//!   (`test_store_io_uses_no_system_thread_pool` holds it to that).
//!
//! The counters are checked too: a control thread sleeps through the window
//! but for one wakeup in its middle, the least a store could do, and the
//! counters must see that wakeup, or the test fails as blind.
//!
//! Ignored by default (it sleeps for a minute per store); CI runs it with
//! `cargo test --test idle -- --ignored`. `STORE_IO_EXPECT_URING` makes an
//! io_uring refusal fail the io_uring test instead of skipping it.

#![cfg(any(target_os = "linux", target_os = "macos", windows))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stderr,
    unused_results,
    unused_must_use
)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use store_io::engine::completion::Done;
use store_io::engine::{SubmitterConfig, WriteMode};
use store_io::{DurableOpen, IoInterface, LinuxTier, Store, StoreOptions, Trust};

/// A thread: its id, and when it started (where the system keeps it), since
/// Windows soon hands a dead thread's id to a new one.
type Key = (u64, u64);

/// What one thread has done so far. A counter the system does not keep per
/// thread is zero.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Activity {
    name: String,
    asleep: bool,
    /// Times it was switched in (Linux, Windows).
    switches: u64,
    /// Its time on a CPU: nanoseconds (Linux), Mach ticks (macOS) or cycles
    /// (Windows).
    cpu: u64,
    /// Read and write system calls (Linux).
    syscalls: u64,
    /// Bytes read and written (Linux).
    bytes: u64,
    /// The system's own thread, not watched (Windows: the module's
    /// documentation).
    system: bool,
}

#[cfg(target_os = "linux")]
mod sys {
    use std::collections::BTreeMap;

    use super::{Activity, Key};

    fn field(text: &str, key: &str) -> u64 {
        text.lines()
            .find_map(|l| l.strip_prefix(key))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0)
    }

    /// Every thread of this process and its activity counters.
    pub fn threads() -> BTreeMap<Key, Activity> {
        let mut out = BTreeMap::new();
        for e in std::fs::read_dir("/proc/self/task").unwrap() {
            let e = e.unwrap();
            let Ok(tid) = e.file_name().to_string_lossy().parse::<u64>() else {
                continue;
            };
            let base = e.path();
            let read = |f: &str| std::fs::read_to_string(base.join(f));
            let (Ok(status), Ok(io), Ok(sched), Ok(stat)) =
                (read("status"), read("io"), read("schedstat"), read("stat"))
            else {
                continue; // exited meanwhile
            };
            // stat: "tid (comm) S ...": the state follows the last ')', and
            // the start time is the 19th field after it.
            let state = stat.rsplit(')').next().unwrap_or("").trim_start();
            let started = state
                .split_whitespace()
                .nth(19)
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            out.insert(
                (tid, started),
                Activity {
                    name: status
                        .lines()
                        .find_map(|l| l.strip_prefix("Name:"))
                        .unwrap_or("")
                        .trim()
                        .to_owned(),
                    asleep: state.starts_with('S'),
                    switches: field(&status, "voluntary_ctxt_switches:")
                        + field(&status, "nonvoluntary_ctxt_switches:"),
                    cpu: sched
                        .split_whitespace()
                        .next()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0),
                    syscalls: field(&io, "syscr:") + field(&io, "syscw:"),
                    bytes: field(&io, "rchar:") + field(&io, "wchar:"),
                    system: false,
                },
            );
        }
        out
    }

    /// Bytes this process has moved to and from storage.
    pub fn storage_bytes() -> Option<u64> {
        let io = std::fs::read_to_string("/proc/self/io").unwrap();
        Some(field(&io, "read_bytes:") + field(&io, "write_bytes:"))
    }

    /// Where a thread began: its name says it here.
    pub fn origin(_tid: u64) -> String {
        String::new()
    }
}

#[cfg(target_os = "macos")]
mod sys {
    use std::collections::BTreeMap;
    use std::ffi::c_int;

    use super::{Activity, Key};

    /// `<sys/proc_info.h>`: this process's thread ids, one thread's
    /// information by its id, and its counters by its id (macOS 13); libc
    /// names none of them.
    const PROC_PIDLISTTHREADIDS: c_int = 28;
    const PROC_PIDTHREADID64INFO: c_int = 15;
    const PROC_PIDTHREADCOUNTS: c_int = 34;

    /// `struct proc_threadcounts`, with room for four performance levels.
    #[repr(C)]
    struct Counts {
        levels_in: u16,
        reserved0: u16,
        reserved1: u32,
        /// Per level: instructions, cycles, user and system time in Mach
        /// ticks, energy.
        levels: [[u64; 5]; 4],
    }

    /// A thread's user plus system time in Mach ticks, exact to the tick,
    /// where `proc_threadinfo` keeps whole microseconds (a short wakeup
    /// could leave those unchanged); `None` before macOS 13.
    fn ticks(pid: c_int, id: u64) -> Option<u64> {
        // SAFETY: plain integers; zero is valid.
        let mut counts: Counts = unsafe { std::mem::zeroed() };
        let size = c_int::try_from(size_of::<Counts>()).unwrap();
        // SAFETY: the buffer is one `Counts`, `size` bytes.
        let got = unsafe {
            libc::proc_pidinfo(
                pid,
                PROC_PIDTHREADCOUNTS,
                id,
                (&raw mut counts).cast(),
                size,
            )
        };
        let filled = usize::try_from(got).ok()?.checked_sub(8)? / 40;
        let levels = filled.min(usize::from(counts.levels_in)).min(4);
        (levels > 0).then(|| counts.levels[..levels].iter().map(|l| l[2] + l[3]).sum())
    }

    /// Every thread of this process and its activity counters (64-bit
    /// thread ids are never reused).
    pub fn threads() -> BTreeMap<Key, Activity> {
        // SAFETY: getpid has no preconditions.
        let pid = unsafe { libc::getpid() };
        let mut ids = vec![0u64; 256];
        let count = loop {
            let size = c_int::try_from(ids.len() * 8).unwrap();
            // SAFETY: the buffer holds `size` bytes.
            let got = unsafe {
                libc::proc_pidinfo(pid, PROC_PIDLISTTHREADIDS, 0, ids.as_mut_ptr().cast(), size)
            };
            assert!(got > 0, "thread ids: {}", std::io::Error::last_os_error());
            let count = usize::try_from(got).unwrap() / 8;
            if count < ids.len() {
                break count;
            }
            ids.resize(ids.len() * 2, 0);
        };
        let mut out = BTreeMap::new();
        for &id in &ids[..count] {
            // SAFETY: plain integers and an array of them; zero is valid.
            let mut info: libc::proc_threadinfo = unsafe { std::mem::zeroed() };
            let size = c_int::try_from(size_of::<libc::proc_threadinfo>()).unwrap();
            // SAFETY: the buffer is one `proc_threadinfo`, `size` bytes.
            let got = unsafe {
                libc::proc_pidinfo(
                    pid,
                    PROC_PIDTHREADID64INFO,
                    id,
                    (&raw mut info).cast(),
                    size,
                )
            };
            if got != size {
                continue; // exited meanwhile
            }
            let name: Vec<u8> = info
                .pth_name
                .iter()
                .take_while(|&&c| c != 0)
                .map(|&c| c.to_ne_bytes()[0])
                .collect();
            out.insert(
                (id, 0),
                Activity {
                    name: String::from_utf8_lossy(&name).into_owned(),
                    asleep: info.pth_run_state == libc::TH_STATE_WAITING,
                    switches: 0,
                    cpu: ticks(pid, id).unwrap_or(info.pth_user_time + info.pth_system_time),
                    syscalls: 0,
                    bytes: 0,
                    system: false,
                },
            );
        }
        out
    }

    /// Bytes this process has moved to and from storage.
    pub fn storage_bytes() -> Option<u64> {
        // SAFETY: plain integers; zero is valid.
        let mut usage: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
        // SAFETY: the buffer is one `rusage_info_v2`, the flavour asked for.
        let got = unsafe {
            libc::proc_pid_rusage(
                libc::getpid(),
                libc::RUSAGE_INFO_V2,
                (&raw mut usage).cast(),
            )
        };
        assert_eq!(got, 0, "rusage: {}", std::io::Error::last_os_error());
        Some(usage.ri_diskio_bytesread + usage.ri_diskio_byteswritten)
    }

    /// Where a thread began: its name says it here.
    pub fn origin(_tid: u64) -> String {
        String::new()
    }
}

#[cfg(windows)]
mod sys {
    use std::collections::BTreeMap;

    use windows_sys::Wdk::System::SystemInformation::{
        NtQuerySystemInformation, SystemProcessInformation,
    };
    use windows_sys::Wdk::System::Threading::{
        NtQueryInformationThread, ThreadQuerySetWin32StartAddress,
    };
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, STATUS_INFO_LENGTH_MISMATCH};
    use windows_sys::Win32::System::LibraryLoader::{
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        GetModuleFileNameW, GetModuleHandleExW,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcessId, GetThreadDescription, OpenThread, THREAD_QUERY_INFORMATION,
        THREAD_QUERY_LIMITED_INFORMATION,
    };
    use windows_sys::Win32::System::WindowsProgramming::{
        QueryThreadCycleTime, SYSTEM_PROCESS_INFORMATION, SYSTEM_THREAD_INFORMATION,
    };

    use super::{Activity, Key};

    /// `KTHREAD_STATE`: waiting.
    const WAITING: u32 = 5;

    /// Its description (`std::thread::Builder::name` sets it), and its CPU
    /// cycles; `None` once it has exited.
    fn name_and_cycles(tid: u32) -> Option<(String, u64)> {
        // SAFETY: no pointer arguments.
        let handle = unsafe { OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, tid) };
        if handle.is_null() {
            return None;
        }
        let mut cycles = 0;
        // SAFETY: a thread handle with query access; one u64 out.
        let counted = unsafe { QueryThreadCycleTime(handle, &mut cycles) } != 0;
        let mut text = std::ptr::null_mut();
        // SAFETY: as above; the description is ours to free.
        let described = unsafe { GetThreadDescription(handle, &mut text) } >= 0;
        let mut name = String::new();
        if described && !text.is_null() {
            let mut len = 0;
            // SAFETY: a NUL-terminated wide string the system allocated.
            while unsafe { *text.add(len) } != 0 {
                len += 1;
            }
            // SAFETY: `len` units, read above.
            name = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
            // SAFETY: GetThreadDescription's allocation, freed once.
            unsafe { LocalFree(text.cast()) };
        }
        // SAFETY: opened above, closed once.
        unsafe { CloseHandle(handle) };
        counted.then_some((name, cycles))
    }

    /// Every thread of this process and its activity counters.
    pub fn threads() -> BTreeMap<Key, Activity> {
        // u64s: the entries are 8-aligned.
        let mut buf = vec![0u64; 1 << 16];
        loop {
            let len = u32::try_from(buf.len() * 8).unwrap();
            let mut need = 0;
            // SAFETY: the buffer holds `len` bytes.
            let status = unsafe {
                NtQuerySystemInformation(
                    SystemProcessInformation,
                    buf.as_mut_ptr().cast(),
                    len,
                    &mut need,
                )
            };
            if status == STATUS_INFO_LENGTH_MISMATCH {
                let need = usize::try_from(need).unwrap().max(buf.len() * 8);
                buf.resize(need / 8 * 2, 0);
                continue;
            }
            assert!(status >= 0, "NtQuerySystemInformation: {status:#x}");
            break;
        }
        // SAFETY: no arguments.
        let me = usize::try_from(unsafe { GetCurrentProcessId() }).unwrap();
        let base = buf.as_ptr().cast::<u8>();
        let mut at = 0;
        let process = loop {
            // SAFETY: `at` is an entry the system wrote: the first, then each
            // one's `NextEntryOffset` on.
            let p = unsafe { base.add(at).cast::<SYSTEM_PROCESS_INFORMATION>().read() };
            if p.UniqueProcessId.addr() == me {
                break p;
            }
            assert_ne!(p.NextEntryOffset, 0, "this process is not listed");
            at += usize::try_from(p.NextEntryOffset).unwrap();
        };
        let mut out = BTreeMap::new();
        for i in 0..usize::try_from(process.NumberOfThreads).unwrap() {
            // SAFETY: the process's threads follow its entry, as many as it
            // says.
            let t = unsafe {
                base.add(at + size_of::<SYSTEM_PROCESS_INFORMATION>())
                    .cast::<SYSTEM_THREAD_INFORMATION>()
                    .add(i)
                    .read()
            };
            let tid = u32::try_from(t.ClientId.UniqueThread.addr()).unwrap();
            let Some((name, cycles)) = name_and_cycles(tid) else {
                continue; // exited meanwhile
            };
            // Reserved1 is KernelTime, UserTime, CreateTime.
            let created = u64::from_ne_bytes(t.Reserved1[2].to_ne_bytes());
            out.insert(
                (u64::from(tid), created),
                Activity {
                    name: name.clone(),
                    asleep: t.ThreadState == WAITING,
                    // Reserved3 is ContextSwitches.
                    switches: u64::from(t.Reserved3),
                    cpu: cycles,
                    syscalls: 0,
                    bytes: 0,
                    system: name.is_empty()
                        && start(tid).is_ok_and(|(file, _)| file.eq_ignore_ascii_case("ntdll.dll")),
                },
            );
        }
        out
    }

    /// Not counted (the module's documentation says why).
    pub fn storage_bytes() -> Option<u64> {
        None
    }

    /// Where a thread began, for a thread the report names: its start
    /// address, as module and offset (a thread the system starts has no
    /// name, and this tells whose it is).
    pub fn origin(tid: u64) -> String {
        let Ok(tid) = u32::try_from(tid) else {
            return String::new();
        };
        match start(tid) {
            Ok((file, offset)) => format!(", started in {file}+{offset:#x}"),
            Err(why) => format!(", {why}"),
        }
    }

    /// A thread's start address: its module's file name and the offset in
    /// it, or why not.
    fn start(tid: u32) -> Result<(String, usize), String> {
        // SAFETY: no pointer arguments.
        let handle = unsafe { OpenThread(THREAD_QUERY_INFORMATION, 0, tid) };
        if handle.is_null() {
            return Err("exited".to_owned());
        }
        let mut start = 0usize;
        // SAFETY: a thread handle with query access; one pointer-sized out.
        let status = unsafe {
            NtQueryInformationThread(
                handle,
                ThreadQuerySetWin32StartAddress,
                (&raw mut start).cast(),
                u32::try_from(size_of::<usize>()).unwrap(),
                std::ptr::null_mut(),
            )
        };
        // SAFETY: opened above, closed once.
        unsafe { CloseHandle(handle) };
        if status < 0 {
            return Err(format!("start unknown ({status:#x})"));
        }
        let mut module = std::ptr::null_mut();
        // SAFETY: FROM_ADDRESS reads the second argument as an address;
        // UNCHANGED_REFCOUNT takes no reference to release.
        let found = unsafe {
            GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
                    | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                std::ptr::without_provenance(start),
                &mut module,
            )
        } != 0;
        if !found {
            return Err(format!("started at {start:#x}, in no module"));
        }
        let mut path = [0u16; 260];
        // SAFETY: the buffer holds `path.len()` units.
        let len = unsafe { GetModuleFileNameW(module, path.as_mut_ptr(), 260) };
        let path = String::from_utf16_lossy(&path[..usize::try_from(len).unwrap()]);
        let file = path.rsplit('\\').next().unwrap_or(&path).to_owned();
        Ok((file, start - module.addr()))
    }
}

use sys::{origin, storage_bytes, threads};

/// The control thread's name.
const CONTROL: &str = "idle-control";

/// The control thread's counters.
fn control_activity() -> Activity {
    threads()
        .into_values()
        .find(|a| a.name == CONTROL)
        .expect("the control thread is listed")
}

fn is_store_io(name: &str) -> bool {
    name.starts_with("store-io") || name.starts_with("sio-") || name.starts_with("iou-")
}

fn base_dir() -> PathBuf {
    std::env::var_os("STORE_IO_TEST_DIR")
        .or_else(|| std::env::var_os("HOME"))
        .map_or_else(std::env::temp_dir, PathBuf::from)
}

/// The test on a Linux tier, or on the system's only interface (`None`).
fn idle_on(tier: Option<LinuxTier>) {
    let secs: u64 = std::env::var("STORE_IO_IDLE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);
    let label = tier.map_or_else(|| "store".to_owned(), |t| format!("{t:?}"));
    let dir = base_dir().join(format!(".store-io-idle-{}-{label}", std::process::id()));
    let _gone = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // The control (the module's documentation): started before the snapshot,
    // so it is not watched with the store.
    let (poke, poked) = mpsc::channel::<()>();
    let control = std::thread::Builder::new()
        .name(CONTROL.into())
        .spawn(move || while poked.recv().is_ok() {})
        .unwrap();
    let before = threads();
    let leftover: Vec<&str> = before
        .values()
        .map(|a| a.name.as_str())
        .filter(|n| is_store_io(n))
        .collect();
    // A failed earlier test leaves its store behind: its threads predate
    // this test's snapshot and are not watched, so this result stands.
    if !leftover.is_empty() {
        eprintln!("store-io threads from an earlier test, not watched: {leftover:?}");
    }
    let before: BTreeSet<Key> = before.into_keys().collect();
    let probe = store_io::probe(&dir).unwrap();
    let mut opts = StoreOptions {
        t3_pool_threads: 2,
        trust: Trust {
            override_refusal: !matches!(probe.decision.durable_open, DurableOpen::Allowed),
            ..Trust::default()
        },
        ..StoreOptions::default()
    };
    if let Some(t) = tier {
        opts.linux_tier = t;
    }
    let store = match Store::create(dir.join("s"), opts) {
        Ok(s) => s,
        Err(e) if tier == Some(LinuxTier::Uring) => {
            assert!(
                std::env::var_os("STORE_IO_EXPECT_URING").is_none(),
                "io_uring refused ({e}) where STORE_IO_EXPECT_URING says it works"
            );
            eprintln!("io_uring refused here ({e}): its idle test is skipped");
            return;
        }
        Err(e) => panic!("{e}"),
    };
    let interface = store.report().io.interface;
    let want = match tier {
        Some(LinuxTier::Uring) => IoInterface::IoUring,
        None if cfg!(windows) => IoInterface::Iocp,
        _ => IoInterface::Synchronous,
    };
    assert_eq!(interface, want, "{:?}", store.report().io);
    // Exercise it: durable appends, a multi-page batch (the synchronous
    // tier's pool), and a submitter with writes and a barrier (the flush
    // thread).
    let wal = store.provision_append_region("wal", 1 << 20).unwrap();
    for i in 0..8u8 {
        wal.append_durable(&[i; 100]).unwrap();
    }
    let pages = store.provision_page_region("pages", 1 << 20).unwrap();
    let blk = pages.block_size();
    let mut batch = pages.batch();
    for i in 0..16u64 {
        batch
            .write(pages.pos(i * blk).unwrap(), &vec![i as u8; blk as usize])
            .unwrap();
    }
    batch.commit().unwrap();
    let mut sub = store.submitter(SubmitterConfig::default()).unwrap();
    for i in 0..4u64 {
        let mut b = sub.take_buf(blk as usize).unwrap();
        b.as_mut_slice().fill(i as u8);
        sub.write(&pages, pages.pos(i * blk).unwrap(), b, WriteMode::Plain, i)
            .map_err(|r| r.error)
            .unwrap();
    }
    let mut n = 0;
    while n < 4 {
        let got = sub.wait(1).unwrap().count();
        assert!(got > 0 || sub.in_flight() > 0, "writes lost");
        n += got;
    }
    sub.barrier_all(9).map_err(|r| r.error).unwrap();
    let mut flushed = false;
    while !flushed {
        let got: Vec<_> = sub.wait(1).unwrap().collect();
        assert!(
            !got.is_empty() || sub.in_flight() > 0,
            "the barrier was lost"
        );
        for c in got {
            assert_eq!(c.tag, 9);
            assert!(matches!(c.outcome, Ok(Done::Flushed)), "{:?}", c.outcome);
            flushed = true;
        }
    }
    // The submitter stays alive through the window: a live submitter must
    // be idle too.
    let watched = || -> BTreeMap<Key, Activity> {
        threads()
            .into_iter()
            .filter(|(key, a)| !before.contains(key) && !a.system)
            .collect()
    };
    // The idle moment: every store-io thread asleep and nothing moving.
    let deadline = Instant::now() + Duration::from_secs(10);
    let start = loop {
        let a = watched();
        std::thread::sleep(Duration::from_millis(100));
        let b = watched();
        if a == b && b.values().all(|t| t.asleep) {
            break b;
        }
        assert!(
            Instant::now() < deadline,
            "store-io never went idle: {b:#?}"
        );
    };
    // The counters see activity (the flush thread has run), so a quiet
    // window below means a quiet store, not blind counters.
    assert!(
        start.values().any(|a| a.name.contains("flush")
            && a.cpu > 0
            && (a.switches > 0 || cfg!(target_os = "macos"))),
        "{label}: the flush thread shows no activity: {start:?}"
    );
    eprintln!(
        "{label} ({interface}): watching {} store-io thread(s) for {secs} s: {:?}",
        start.len(),
        start.values().map(|a| a.name.as_str()).collect::<Vec<_>>()
    );
    let control_start = control_activity();
    // After the line above, so the test's own output is not counted.
    let disk = storage_bytes();
    std::thread::sleep(Duration::from_millis(secs * 500));
    poke.send(()).unwrap();
    std::thread::sleep(Duration::from_millis(secs * 500));
    let end = watched();
    let control_end = control_activity();
    assert!(
        control_end.cpu != control_start.cpu
            && (control_end.switches != control_start.switches || cfg!(target_os = "macos")),
        "{label}: the counters missed the control thread's one wakeup: {control_start:?} then {control_end:?}"
    );
    let mut busy = Vec::new();
    for (key, a) in &start {
        let tid = key.0;
        match end.get(key) {
            Some(b) if b == a => {}
            Some(b) => busy.push(format!(
                "{} ({tid}{}): {} switches, {} on a CPU, {} syscalls, {} bytes",
                a.name,
                origin(tid),
                b.switches - a.switches,
                b.cpu - a.cpu,
                b.syscalls - a.syscalls,
                b.bytes - a.bytes
            )),
            None => busy.push(format!("{} ({tid}) exited while idle", a.name)),
        }
    }
    for (key, b) in &end {
        if !start.contains_key(key) {
            busy.push(format!(
                "{} ({}{}) started while idle",
                b.name,
                key.0,
                origin(key.0)
            ));
        }
    }
    if let (Some(before), Some(after)) = (disk, storage_bytes()) {
        let moved = after - before;
        if moved != 0 {
            busy.push(format!("{moved} bytes moved to or from storage"));
        }
    }
    assert!(
        busy.is_empty(),
        "{label}: an idle store was active: {busy:#?}"
    );
    drop((sub, wal, pages, batch, store));
    drop(poke);
    control.join().unwrap();
    let _cleanup = std::fs::remove_dir_all(&dir);
}

/// The premise of not watching the system's thread-pool workers (the
/// module's documentation): no store-io source calls the system's thread
/// pool, so none of them is store-io's work.
#[test]
fn test_store_io_uses_no_system_thread_pool() {
    const POOL: [&str; 6] = [
        "Threadpool",
        "QueueUserWorkItem",
        "RegisterWaitForSingleObject",
        "BindIoCompletionCallback",
        "CreateTimerQueue",
        "RtlCreateUserThread",
    ];
    fn scan(dir: &std::path::Path, found: &mut Vec<String>, files: &mut usize) {
        for e in std::fs::read_dir(dir).unwrap() {
            let path = e.unwrap().path();
            if path.is_dir() {
                scan(&path, found, files);
            } else if path.extension().is_some_and(|x| x == "rs") {
                *files += 1;
                let text = std::fs::read_to_string(&path).unwrap();
                for api in POOL.iter().filter(|api| text.contains(*api)) {
                    found.push(format!("{}: {api}", path.display()));
                }
            }
        }
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut found = Vec::new();
    let mut files = 0;
    scan(&root.join("src"), &mut found, &mut files);
    for e in std::fs::read_dir(root.join("crates")).unwrap() {
        let src = e.unwrap().path().join("src");
        if src.is_dir() {
            scan(&src, &mut found, &mut files);
        }
    }
    assert!(files > 50, "only {files} sources found");
    assert!(
        found.is_empty(),
        "the system's thread pool is used: {found:#?}"
    );
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "sleeps for STORE_IO_IDLE_SECS (60); run by CI with --ignored"]
fn test_an_idle_store_is_silent_on_io_uring() {
    idle_on(Some(LinuxTier::Uring));
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "sleeps for STORE_IO_IDLE_SECS (60); run by CI with --ignored"]
fn test_an_idle_store_is_silent_on_the_synchronous_tier() {
    idle_on(Some(LinuxTier::Sync));
}

#[cfg(not(target_os = "linux"))]
#[test]
#[ignore = "sleeps for STORE_IO_IDLE_SECS (60); run by CI with --ignored"]
fn test_an_idle_store_is_silent() {
    idle_on(None);
}
