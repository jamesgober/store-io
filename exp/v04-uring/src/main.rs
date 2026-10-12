//! v0.4 research experiments. Not library code: assertions abort on anything
//! unexpected, and every result is printed as one `key=value` line per point
//! so the runner scripts can collect it.
//!
//! Subcommands (see `usage()`):
//!   probe    kernel, setup flags, opcode support, block queue limits (E1, E4, E7)
//!   nop      QD1 NOP round trip per ring configuration (E1)
//!   wake     wake latency of an owner parked in GETEVENTS (E1)
//!   qd1      QD1 4 KiB writes, plain and durable, ring vs synchronous (E2)
//!   fsynchop OP_FSYNC vs inline fdatasync, warm and after idle (E3)
//!   batch    N random pages + one flush: ring, serial, pool (E5)
//!   qd       QD-N writes or reads, fixed/unfixed buffers and files (E6)
//!   memlock  registered-buffer accounting across rings (E6)
//!   seq      sequential bandwidth, ring vs synchronous (E7)
//!   idle     burst, then park in GETEVENTS until SIGUSR1 (E8)
//!   overflow CQ overflow with NODROP (E9)
//!   killlock dead owner's in-flight writes vs the OFD lock (E13)

use std::ffi::CString;
use std::io::{BufRead, Write as _};
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use io_uring::{IoUring, cqueue, opcode, squeue, types};

const BS: usize = 4096;

// ---------------------------------------------------------------------------
// Common: aligned memory, Ready files, statistics, ring configurations.
// ---------------------------------------------------------------------------

/// Page-aligned anonymous memory, MADV_DONTFORK, never reallocated.
struct Mem {
    ptr: *mut u8,
    len: usize,
}

impl Mem {
    fn new(len: usize) -> Self {
        // SAFETY: anonymous private mapping; checked below.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_POPULATE,
                -1,
                0,
            )
        };
        assert!(p != libc::MAP_FAILED, "mmap {}", errno());
        // SAFETY: p..p+len is our mapping.
        let r = unsafe { libc::madvise(p, len, libc::MADV_DONTFORK) };
        assert_eq!(r, 0, "madvise");
        Self { ptr: p.cast(), len }
    }
    fn at(&self, off: usize) -> *mut u8 {
        assert!(off < self.len);
        // SAFETY: in bounds.
        unsafe { self.ptr.add(off) }
    }
    fn fill(&self, off: usize, len: usize, byte: u8) {
        assert!(off + len <= self.len);
        // SAFETY: in bounds, no outstanding I/O on it when called.
        unsafe { std::ptr::write_bytes(self.ptr.add(off), byte, len) };
    }
    fn slice(&self, off: usize, len: usize) -> &[u8] {
        assert!(off + len <= self.len);
        // SAFETY: in bounds; callers read only after the I/O completed.
        unsafe { std::slice::from_raw_parts(self.ptr.add(off), len) }
    }
}

impl Drop for Mem {
    fn drop(&mut self) {
        // SAFETY: our mapping; no I/O in flight when dropped (experiments
        // drain before returning).
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn open_direct(path: &str, create: bool) -> std::fs::File {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .custom_flags(libc::O_DIRECT)
        .open(path)
        .unwrap_or_else(|e| panic!("open {path}: {e}"))
}

/// A Ready file: allocated with fallocate mode 0, every block written with
/// direct I/O (no page cache), size and data durable.
fn prepare_ready(path: &str, size: u64) -> std::fs::File {
    // A block device is opened as it is: never removed, never allocated.
    let bdev = path.starts_with("/dev/");
    if !bdev {
        let _ = std::fs::remove_file(path);
    }
    let f = open_direct(path, !bdev);
    let fd = f.as_raw_fd();
    if !bdev {
        // SAFETY: valid fd.
        let r = unsafe { libc::fallocate(fd, 0, 0, size as i64) };
        assert_eq!(r, 0, "fallocate {}", errno());
    }
    let chunk = 1 << 20;
    let m = Mem::new(chunk);
    m.fill(0, chunk, 0x5A);
    let mut off = 0u64;
    while off < size {
        let n = chunk.min((size - off) as usize);
        // SAFETY: m is valid for n bytes, aligned.
        let w = unsafe { libc::pwrite(fd, m.at(0).cast(), n, off as i64) };
        assert_eq!(w, n as isize, "fill pwrite {}", errno());
        off += n as u64;
    }
    // SAFETY: valid fd.
    assert_eq!(unsafe { libc::fsync(fd) }, 0);
    println!("prepared path={path} size={size} cached_pages={}", cached_pages(fd, size));
    f
}

/// Pages of the file range resident in the page cache (cachestat, 6.5+), or -1.
fn cached_pages(fd: RawFd, len: u64) -> i64 {
    #[repr(C)]
    struct Range {
        off: u64,
        len: u64,
    }
    #[repr(C)]
    #[derive(Default)]
    struct Cs {
        nr_cache: u64,
        nr_dirty: u64,
        nr_writeback: u64,
        nr_evicted: u64,
        nr_recently_evicted: u64,
    }
    let range = Range { off: 0, len };
    let mut cs = Cs::default();
    // SAFETY: cachestat(2) with valid pointers; ENOSYS on older kernels.
    let r = unsafe { libc::syscall(451, fd, &range as *const Range, &mut cs as *mut Cs, 0u32) };
    if r == 0 { cs.nr_cache as i64 } else { -1 }
}

struct Stats(Vec<u64>);

impl Stats {
    fn new(n: usize) -> Self {
        Self(Vec::with_capacity(n))
    }
    fn push(&mut self, d: Duration) {
        self.0.push(d.as_nanos() as u64);
    }
    fn line(&mut self) -> String {
        let v = &mut self.0;
        if v.is_empty() {
            return "n=0".into();
        }
        v.sort_unstable();
        let n = v.len();
        let p = |q: f64| v[((n as f64 * q) as usize).min(n - 1)] as f64 / 1000.0;
        let mean = v.iter().sum::<u64>() as f64 / n as f64 / 1000.0;
        format!(
            "n={n} mean_us={mean:.2} p50_us={:.2} p99_us={:.2} p999_us={:.2} max_us={:.2}",
            p(0.5),
            p(0.99),
            p(0.999),
            v[n - 1] as f64 / 1000.0
        )
    }
}

/// Ring configurations by name: parts joined with '+'.
/// plain, coop, tf (TASKRUN_FLAG), single, defer (implies single), nosq, all (SUBMIT_ALL).
fn ring(cfg: &str, entries: u32) -> IoUring {
    let mut b: io_uring::Builder = IoUring::builder();
    for part in cfg.split('+') {
        match part {
            "plain" => {}
            "coop" => {
                b.setup_coop_taskrun();
            }
            "tf" => {
                b.setup_taskrun_flag();
            }
            "single" => {
                b.setup_single_issuer();
            }
            "defer" => {
                b.setup_single_issuer();
                b.setup_defer_taskrun();
            }
            "nosq" => {
                b.setup_no_sqarray();
            }
            "all" => {
                b.setup_submit_all();
            }
            other => panic!("unknown ring part {other}"),
        }
    }
    b.setup_cqsize(entries * 2);
    b.build(entries).unwrap_or_else(|e| panic!("ring {cfg}: {e}"))
}

/// Pushes one entry, submitting first if the SQ is full.
fn push(r: &mut IoUring, e: &squeue::Entry) {
    loop {
        // SAFETY: callers keep every buffer and fd alive until the CQE.
        if unsafe { r.submission().push(e) }.is_ok() {
            return;
        }
        r.submit().expect("submit");
    }
}

/// Waits for exactly one CQE and returns its result.
fn wait_one(r: &mut IoUring) -> (u64, i32) {
    loop {
        if let Some(c) = r.completion().next() {
            return (c.user_data(), c.result());
        }
        r.submit_and_wait(1).expect("submit_and_wait");
    }
}

fn now_ns(base: Instant) -> u64 {
    base.elapsed().as_nanos() as u64
}

fn cpu_us() -> f64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: valid pointer.
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let t = |tv: libc::timeval| tv.tv_sec as f64 * 1e6 + tv.tv_usec as f64;
    t(ru.ru_utime) + t(ru.ru_stime)
}

fn iowq_threads() -> usize {
    std::fs::read_dir("/proc/self/task")
        .map(|d| {
            d.filter_map(Result::ok)
                .filter(|e| {
                    std::fs::read_to_string(e.path().join("comm"))
                        .map(|c| c.starts_with("iou-wrk"))
                        .unwrap_or(false)
                })
                .count()
        })
        .unwrap_or(0)
}

fn arg<T: std::str::FromStr>(a: &[String], i: usize, name: &str) -> T {
    a.get(i)
        .unwrap_or_else(|| panic!("missing {name}\n{}", usage()))
        .parse()
        .unwrap_or_else(|_| panic!("bad {name}"))
}

fn usage() -> &'static str {
    "usage:
  probe <path>
  nop <cfg> <n>
  wake <msg|eventfd|futex> <n>
  qd1 <cfg> <file> <n> <ring-plain|ring-dsync|ring-fsyncop|ring-fdatasync|t3-plain|t3-dsync|t3-fdatasync>
  fsynchop <file> <samples> <idle_ms>
  batch <file> <pages> <reps> <ring-fdatasync|ring-fsyncop|t3-serial|t3-pool:P>
  qd <cfg> <file> <n> <depth> <bs> <write|read> <fixedbuf 0|1> <fixedfile 0|1>
  memlock <mib> <rings> <chunk_mib>
  seq <file> <total_mib> <bs_kib> <depth> <write|read> <ring|t3>
  idle <file> <burst> <fsyncop 0|1>
  overflow <cfg>
  killlock <file> <blocks> <ring|t3>
  (cfg = plain | coop | coop+tf | defer | defer+tf | defer+nosq ...)"
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    match a.get(1).map(String::as_str) {
        Some("probe") => probe(&arg::<String>(&a, 2, "path")),
        Some("nop") => nop(&arg::<String>(&a, 2, "cfg"), arg(&a, 3, "n")),
        Some("wake") => wake(&arg::<String>(&a, 2, "method"), arg(&a, 3, "n")),
        Some("qd1") => qd1(
            &arg::<String>(&a, 2, "cfg"),
            &arg::<String>(&a, 3, "file"),
            arg(&a, 4, "n"),
            &arg::<String>(&a, 5, "mode"),
        ),
        Some("fsynchop") => fsynchop(&arg::<String>(&a, 2, "file"), arg(&a, 3, "samples"), arg(&a, 4, "idle_ms")),
        Some("batch") => batch(
            &arg::<String>(&a, 2, "file"),
            arg(&a, 3, "pages"),
            arg(&a, 4, "reps"),
            &arg::<String>(&a, 5, "mode"),
        ),
        Some("qd") => qd(
            &arg::<String>(&a, 2, "cfg"),
            &arg::<String>(&a, 3, "file"),
            arg(&a, 4, "n"),
            arg(&a, 5, "depth"),
            arg(&a, 6, "bs"),
            &arg::<String>(&a, 7, "op"),
            arg::<u8>(&a, 8, "fixedbuf") == 1,
            arg::<u8>(&a, 9, "fixedfile") == 1,
        ),
        Some("memlock") => memlock(arg(&a, 2, "mib"), arg(&a, 3, "rings"), arg(&a, 4, "chunk_mib")),
        Some("seq") => seq(
            &arg::<String>(&a, 2, "file"),
            arg(&a, 3, "total_mib"),
            arg(&a, 4, "bs_kib"),
            arg(&a, 5, "depth"),
            &arg::<String>(&a, 6, "op"),
            &arg::<String>(&a, 7, "engine"),
        ),
        Some("idle") => idle(&arg::<String>(&a, 2, "file"), arg(&a, 3, "burst"), arg::<u8>(&a, 4, "fsyncop") == 1),
        Some("overflow") => overflow(&arg::<String>(&a, 2, "cfg")),
        Some("killlock") => killlock(&arg::<String>(&a, 2, "file"), arg(&a, 3, "blocks"), &arg::<String>(&a, 4, "engine")),
        Some("killlock-child") => killlock_child(&arg::<String>(&a, 2, "file"), arg(&a, 3, "blocks"), &arg::<String>(&a, 4, "engine")),
        _ => {
            eprintln!("{}", usage());
            std::process::exit(2);
        }
    }
}

// ---------------------------------------------------------------------------
// probe
// ---------------------------------------------------------------------------

fn probe(path: &str) {
    let mut u: libc::utsname = unsafe { std::mem::zeroed() };
    // SAFETY: valid pointer.
    unsafe { libc::uname(&mut u) };
    // SAFETY: NUL-terminated by the kernel.
    let rel = unsafe { std::ffi::CStr::from_ptr(u.release.as_ptr()) }.to_string_lossy().into_owned();
    println!("kernel={rel}");
    for f in ["io_uring_disabled", "io_uring_group"] {
        println!("{f}={}", std::fs::read_to_string(format!("/proc/sys/kernel/{f}")).unwrap_or_default().trim());
    }
    let mut rl: libc::rlimit = unsafe { std::mem::zeroed() };
    // SAFETY: valid pointer.
    unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut rl) };
    println!("memlock_cur={} memlock_max={}", rl.rlim_cur, rl.rlim_max);
    for cfg in ["plain", "coop", "coop+tf", "single", "defer", "defer+tf", "defer+nosq", "defer+tf+nosq+all"] {
        let mut b: io_uring::Builder = IoUring::builder();
        for part in cfg.split('+') {
            match part {
                "coop" => { b.setup_coop_taskrun(); }
                "tf" => { b.setup_taskrun_flag(); }
                "single" => { b.setup_single_issuer(); }
                "defer" => { b.setup_single_issuer(); b.setup_defer_taskrun(); }
                "nosq" => { b.setup_no_sqarray(); }
                "all" => { b.setup_submit_all(); }
                _ => {}
            }
        }
        match b.build(8) {
            Ok(r) => println!("setup cfg={cfg} ok=1 nodrop={} native_workers={}", r.params().is_feature_nodrop(), r.params().is_feature_native_workers()),
            Err(e) => println!("setup cfg={cfg} ok=0 err={}", e.raw_os_error().unwrap_or(0)),
        }
    }
    let r = ring("plain", 8);
    let mut p = io_uring::Probe::new();
    r.submitter().register_probe(&mut p).expect("probe");
    for (name, code) in [
        ("NOP", opcode::Nop::CODE),
        ("READ", opcode::Read::CODE),
        ("WRITE", opcode::Write::CODE),
        ("READ_FIXED", opcode::ReadFixed::CODE),
        ("WRITE_FIXED", opcode::WriteFixed::CODE),
        ("FSYNC", opcode::Fsync::CODE),
        ("MSG_RING", opcode::MsgRingData::CODE),
        ("ASYNC_CANCEL", opcode::AsyncCancel::CODE),
        ("WRITEV_FIXED", 61u8),
    ] {
        println!("opcode {name}={}", p.is_supported(code));
    }
    // Block queue limits for the device under `path`.
    let c = CString::new(path).expect("path");
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: valid pointers.
    assert_eq!(unsafe { libc::stat(c.as_ptr(), &mut st) }, 0, "stat {path}");
    let dev = if st.st_mode & libc::S_IFMT == libc::S_IFBLK { st.st_rdev } else { st.st_dev };
    let (maj, min) = (libc::major(dev), libc::minor(dev));
    let base = format!("/sys/dev/block/{maj}:{min}");
    let mut q = format!("{base}/queue");
    if !std::path::Path::new(&q).exists() {
        q = format!("{base}/../queue");
    }
    println!("dev={maj}:{min}");
    for k in ["max_sectors_kb", "max_hw_sectors_kb", "max_segments", "nr_requests", "write_cache", "fua", "logical_block_size", "io_poll"] {
        println!("queue {k}={}", std::fs::read_to_string(format!("{q}/{k}")).unwrap_or_default().trim());
    }
}

// ---------------------------------------------------------------------------
// E1: NOP round trip and wake latency
// ---------------------------------------------------------------------------

fn nop(cfg: &str, n: usize) {
    let mut r = ring(cfg, 64);
    let mut s = Stats::new(n);
    for _ in 0..n {
        let t = Instant::now();
        push(&mut r, &opcode::Nop::new().build().user_data(1));
        let (_, res) = wait_one(&mut r);
        assert_eq!(res, 0);
        s.push(t.elapsed());
    }
    println!("nop cfg={cfg} {}", s.line());
}

/// The owner parks in `enter(GETEVENTS, min_complete = 1)` on a DEFER ring;
/// a producer thread wakes it once per millisecond. Latency = owner's wake
/// time minus the producer's send time (one shared monotonic base).
fn wake(method: &str, n: usize) {
    let base = Instant::now();
    let sent = Arc::new(AtomicU64::new(0));
    let fut = Arc::new(AtomicU32::new(0));
    let (tx, rx) = std::sync::mpsc::channel::<RawFd>();
    let (sent_o, fut_o, method_o) = (sent.clone(), fut.clone(), method.to_owned());
    let owner = std::thread::spawn(move || {
        let mut s = Stats::new(n);
        if method_o == "futex" {
            tx.send(-1).expect("send");
            for i in 0..n as u32 {
                while fut_o.load(Ordering::Acquire) == i {
                    // SAFETY: futex wait on a live u32.
                    unsafe { libc::syscall(libc::SYS_futex, fut_o.as_ptr(), libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG, i, std::ptr::null::<libc::timespec>()) };
                }
                let woke = now_ns(base);
                s.push(Duration::from_nanos(woke.saturating_sub(sent_o.load(Ordering::Acquire))));
            }
        } else {
            let mut r = ring("defer+tf", 64);
            let efd = if method_o == "eventfd" {
                // SAFETY: plain syscall.
                unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) }
            } else {
                -1
            };
            tx.send(if method_o == "eventfd" { efd } else { r.as_raw_fd() }).expect("send");
            let m = Mem::new(4096);
            for _ in 0..n {
                if efd >= 0 {
                    push(&mut r, &opcode::Read::new(types::Fd(efd), m.at(0), 8).build().user_data(7));
                }
                let (_, res) = wait_one(&mut r);
                let woke = now_ns(base);
                assert!(res >= 0, "wake cqe {res}");
                s.push(Duration::from_nanos(woke.saturating_sub(sent_o.load(Ordering::Acquire))));
            }
        }
        println!("wake method={method_o} {}", s.line());
    });
    let target = rx.recv().expect("recv");
    let mut pr = ring("plain", 8);
    for i in 0..n {
        std::thread::sleep(Duration::from_millis(1));
        sent.store(now_ns(base), Ordering::Release);
        match method {
            "msg" => {
                let e = opcode::MsgRingData::new(types::Fd(target), 0, 9, None)
                    .build()
                    .flags(squeue::Flags::SKIP_SUCCESS)
                    .user_data(i as u64);
                push(&mut pr, &e);
                pr.submit().expect("submit");
                // Reap a failure CQE if there is one (success is skipped).
                for c in pr.completion() {
                    panic!("msg_ring failed {}", c.result());
                }
            }
            "eventfd" => {
                let one: u64 = 1;
                // SAFETY: valid eventfd and 8-byte buffer.
                let w = unsafe { libc::write(target, (&one as *const u64).cast(), 8) };
                assert_eq!(w, 8);
            }
            "futex" => {
                fut.fetch_add(1, Ordering::AcqRel);
                // SAFETY: futex wake on a live u32.
                unsafe { libc::syscall(libc::SYS_futex, fut.as_ptr(), libc::FUTEX_WAKE | libc::FUTEX_PRIVATE_FLAG, 1) };
            }
            _ => panic!("method"),
        }
    }
    owner.join().expect("owner");
}

// ---------------------------------------------------------------------------
// E2: QD1 4 KiB writes, plain and durable
// ---------------------------------------------------------------------------

const QD1_FILE: u64 = 64 << 20;

fn qd1(cfg: &str, path: &str, n: usize, mode: &str) {
    let f = prepare_ready(path, QD1_FILE);
    let fd = f.as_raw_fd();
    let m = Mem::new(BS);
    let mut r = ring(cfg, 64);
    let mut s = Stats::new(n);
    let blocks = QD1_FILE / BS as u64;
    let (mut nowait_eagain, mut nowait_short) = (0u64, 0u64);
    let cpu0 = cpu_us();
    let t0 = Instant::now();
    for i in 0..n as u64 {
        // SAFETY: no I/O in flight on m here.
        unsafe { *(m.at(0) as *mut u64) = i };
        let off = (i % blocks) * BS as u64;
        let t = Instant::now();
        match mode {
            "t3-plain" | "t3-dsync" | "t3-fdatasync" => {
                let iov = libc::iovec { iov_base: m.at(0).cast(), iov_len: BS };
                let fl = if mode == "t3-dsync" { libc::RWF_DSYNC } else { 0 };
                // SAFETY: valid fd and iovec.
                let w = unsafe { libc::pwritev2(fd, &iov, 1, off as i64, fl) };
                assert_eq!(w, BS as isize, "pwritev2 {}", errno());
                if mode == "t3-fdatasync" {
                    // SAFETY: valid fd.
                    assert_eq!(unsafe { libc::fdatasync(fd) }, 0);
                }
            }
            "ring-nowait" | "ring-nowait-dsync" => {
                // E14: never let io-wq run a write. RWF_NOWAIT makes a write
                // that would block fail with EAGAIN before any I/O; it is then
                // issued synchronously on this thread (the store's own path).
                let dsync = mode == "ring-nowait-dsync";
                let fl = libc::RWF_NOWAIT | if dsync { libc::RWF_DSYNC } else { 0 };
                let w = opcode::Write::new(types::Fd(fd), m.at(0), BS as u32).offset(off).rw_flags(fl);
                push(&mut r, &w.build().user_data(1));
                let (_, res) = wait_one(&mut r);
                if res == -libc::EAGAIN {
                    nowait_eagain += 1;
                    let iov = libc::iovec { iov_base: m.at(0).cast(), iov_len: BS };
                    let sfl = if dsync { libc::RWF_DSYNC } else { 0 };
                    // SAFETY: valid fd and iovec.
                    let w = unsafe { libc::pwritev2(fd, &iov, 1, off as i64, sfl) };
                    assert_eq!(w, BS as isize, "fallback pwritev2 {}", errno());
                } else if res >= 0 && res != BS as i32 {
                    nowait_short += 1;
                } else {
                    assert_eq!(res, BS as i32, "nowait write cqe");
                }
            }
            _ => {
                let mut w = opcode::Write::new(types::Fd(fd), m.at(0), BS as u32).offset(off);
                if mode == "ring-dsync" {
                    w = w.rw_flags(libc::RWF_DSYNC);
                }
                push(&mut r, &w.build().user_data(1));
                let (_, res) = wait_one(&mut r);
                assert_eq!(res, BS as i32, "write cqe");
                match mode {
                    "ring-fsyncop" => {
                        push(&mut r, &opcode::Fsync::new(types::Fd(fd)).flags(types::FsyncFlags::DATASYNC).build().user_data(2));
                        let (_, res) = wait_one(&mut r);
                        assert_eq!(res, 0, "fsync cqe");
                    }
                    "ring-fdatasync" => {
                        // SAFETY: valid fd.
                        assert_eq!(unsafe { libc::fdatasync(fd) }, 0);
                    }
                    "ring-plain" | "ring-dsync" => {}
                    other => panic!("mode {other}"),
                }
            }
        }
        s.push(t.elapsed());
    }
    let secs = t0.elapsed().as_secs_f64();
    let cpu = (cpu_us() - cpu0) / n as f64;
    println!(
        "qd1 cfg={cfg} mode={mode} ops_s={:.1} cpu_us_op={cpu:.2} nowait_eagain={nowait_eagain} nowait_short={nowait_short} iowq_threads={} cached_pages={} {}",
        n as f64 / secs,
        iowq_threads(),
        cached_pages(fd, QD1_FILE),
        s.line()
    );
}

// ---------------------------------------------------------------------------
// E3: the flush hop, warm and after idle
// ---------------------------------------------------------------------------

fn fsynchop(path: &str, samples: usize, idle_ms: u64) {
    let f = prepare_ready(path, QD1_FILE);
    let fd = f.as_raw_fd();
    let m = Mem::new(BS);
    let mut r = ring("defer+tf", 64);
    let mut op = Stats::new(samples);
    let mut inl = Stats::new(samples);
    let mut created = 0usize;
    for i in 0..samples as u64 {
        for (k, stats) in [(0, &mut op), (1, &mut inl)] {
            // A fresh dirty block so the flush has data to cover.
            unsafe { *(m.at(0) as *mut u64) = i * 2 + k };
            push(&mut r, &opcode::Write::new(types::Fd(fd), m.at(0), BS as u32).offset(((i * 2 + k) % 1024) * BS as u64).build().user_data(1));
            assert_eq!(wait_one(&mut r).1, BS as i32);
            if idle_ms > 0 {
                std::thread::sleep(Duration::from_millis(idle_ms));
            }
            let before = iowq_threads();
            let t = Instant::now();
            if k == 0 {
                push(&mut r, &opcode::Fsync::new(types::Fd(fd)).flags(types::FsyncFlags::DATASYNC).build().user_data(2));
                assert_eq!(wait_one(&mut r).1, 0);
            } else {
                // SAFETY: valid fd.
                assert_eq!(unsafe { libc::fdatasync(fd) }, 0);
            }
            stats.push(t.elapsed());
            if k == 0 && before == 0 && iowq_threads() > 0 {
                created += 1;
            }
        }
    }
    println!("fsynchop kind=op idle_ms={idle_ms} worker_created={created} {}", op.line());
    println!("fsynchop kind=inline idle_ms={idle_ms} {}", inl.line());
}

// ---------------------------------------------------------------------------
// E5: N random pages + one flush
// ---------------------------------------------------------------------------

struct Pool {
    shared: Arc<PoolShared>,
    workers: Vec<std::thread::JoinHandle<()>>,
}

struct PoolShared {
    state: Mutex<(u64, Arc<Vec<(u64, usize)>>, bool)>,
    start: Condvar,
    done: Condvar,
    next: AtomicUsize,
    left: AtomicUsize,
    fd: RawFd,
}

impl Pool {
    fn new(p: usize, fd: RawFd) -> Self {
        let shared = Arc::new(PoolShared {
            state: Mutex::new((0, Arc::new(Vec::new()), false)),
            start: Condvar::new(),
            done: Condvar::new(),
            next: AtomicUsize::new(0),
            left: AtomicUsize::new(0),
            fd,
        });
        let workers = (0..p)
            .map(|_| {
                let sh = shared.clone();
                std::thread::spawn(move || {
                    let mut seen = 0u64;
                    loop {
                        let jobs = {
                            let mut g = sh.state.lock().expect("lock");
                            while g.0 == seen && !g.2 {
                                g = sh.start.wait(g).expect("wait");
                            }
                            if g.2 {
                                return;
                            }
                            seen = g.0;
                            g.1.clone()
                        };
                        loop {
                            let i = sh.next.fetch_add(1, Ordering::AcqRel);
                            if i >= jobs.len() {
                                break;
                            }
                            let (off, ptr) = jobs[i];
                            let iov = libc::iovec { iov_base: ptr as *mut libc::c_void, iov_len: BS };
                            // SAFETY: the caller keeps the buffer alive until `left` hits 0.
                            let w = unsafe { libc::pwritev2(sh.fd, &iov, 1, off as i64, 0) };
                            assert_eq!(w, BS as isize, "pool pwritev2 {}", errno());
                            if sh.left.fetch_sub(1, Ordering::AcqRel) == 1 {
                                let _g = sh.state.lock().expect("lock");
                                sh.done.notify_all();
                            }
                        }
                    }
                })
            })
            .collect();
        Self { shared, workers }
    }

    fn run(&self, jobs: Vec<(u64, usize)>) {
        let n = jobs.len();
        self.shared.next.store(0, Ordering::Release);
        self.shared.left.store(n, Ordering::Release);
        let mut g = self.shared.state.lock().expect("lock");
        g.0 += 1;
        g.1 = Arc::new(jobs);
        self.shared.start.notify_all();
        while self.shared.left.load(Ordering::Acquire) != 0 {
            g = self.shared.done.wait(g).expect("wait");
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        {
            let mut g = self.shared.state.lock().expect("lock");
            g.2 = true;
            self.shared.start.notify_all();
        }
        for w in self.workers.drain(..) {
            w.join().expect("join");
        }
    }
}

fn batch(path: &str, pages: usize, reps: usize, mode: &str) {
    let f = prepare_ready(path, QD1_FILE);
    let fd = f.as_raw_fd();
    let m = Mem::new(pages * BS);
    let total = (QD1_FILE / BS as u64) as usize;
    let pool = mode.strip_prefix("t3-pool:").map(|p| Pool::new(p.parse().expect("pool size"), fd));
    let mut r = ring("defer+tf+all", 256);
    let mut s = Stats::new(reps);
    let t0 = Instant::now();
    let mut seed = 0x9E37_79B9usize;
    for rep in 0..reps {
        for i in 0..pages {
            unsafe { *(m.at(i * BS) as *mut u64) = (rep * pages + i) as u64 };
        }
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let offs: Vec<u64> = (0..pages).map(|i| (((seed >> 7) + i * 7919) % total) as u64 * BS as u64).collect();
        let t = Instant::now();
        match mode {
            "t3-serial" => {
                for (i, off) in offs.iter().enumerate() {
                    let iov = libc::iovec { iov_base: m.at(i * BS).cast(), iov_len: BS };
                    // SAFETY: valid fd and buffer.
                    assert_eq!(unsafe { libc::pwritev2(fd, &iov, 1, *off as i64, 0) }, BS as isize);
                }
                assert_eq!(unsafe { libc::fdatasync(fd) }, 0);
            }
            _ if pool.is_some() => {
                let jobs = offs.iter().enumerate().map(|(i, off)| (*off, m.at(i * BS) as usize)).collect();
                pool.as_ref().expect("pool").run(jobs);
                assert_eq!(unsafe { libc::fdatasync(fd) }, 0);
            }
            "ring-fdatasync" | "ring-fsyncop" => {
                for (i, off) in offs.iter().enumerate() {
                    push(&mut r, &opcode::Write::new(types::Fd(fd), m.at(i * BS), BS as u32).offset(*off).build().user_data(i as u64));
                }
                let mut got = 0;
                while got < pages {
                    r.submit_and_wait(pages - got).expect("submit_and_wait");
                    for c in r.completion() {
                        assert_eq!(c.result(), BS as i32, "batch cqe");
                        got += 1;
                    }
                }
                if mode == "ring-fsyncop" {
                    push(&mut r, &opcode::Fsync::new(types::Fd(fd)).flags(types::FsyncFlags::DATASYNC).build().user_data(u64::MAX));
                    assert_eq!(wait_one(&mut r).1, 0);
                } else {
                    assert_eq!(unsafe { libc::fdatasync(fd) }, 0);
                }
            }
            other => panic!("mode {other}"),
        }
        s.push(t.elapsed());
    }
    let secs = t0.elapsed().as_secs_f64();
    println!("batch mode={mode} pages={pages} pages_s={:.1} {}", (pages * reps) as f64 / secs, s.line());
}

// ---------------------------------------------------------------------------
// E6: QD-N, fixed buffers and files; memlock accounting
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn qd(cfg: &str, path: &str, n: usize, depth: usize, bs: usize, op: &str, fixedbuf: bool, fixedfile: bool) {
    let size: u64 = 256 << 20;
    let f = prepare_ready(path, size);
    let fd = f.as_raw_fd();
    let m = Mem::new(depth * bs);
    m.fill(0, depth * bs, 0xC3);
    let mut r = ring(cfg, (depth as u32).next_power_of_two().max(8));
    if fixedbuf {
        let iov = [libc::iovec { iov_base: m.at(0).cast(), iov_len: depth * bs }];
        // SAFETY: m outlives the ring's use of it (unregistered on drop of r, before m).
        unsafe { r.submitter().register_buffers(&iov) }.expect("register_buffers");
    }
    if fixedfile {
        r.submitter().register_files(&[fd]).expect("register_files");
    }
    let slots = size / bs as u64;
    let mut free: Vec<usize> = (0..depth).collect();
    let (mut issued, mut done, mut inflight) = (0usize, 0usize, 0usize);
    let cpu0 = cpu_us();
    let t0 = Instant::now();
    let mut seed = 12345u64;
    while done < n {
        while inflight < depth && issued < n {
            let slot = free.pop().expect("slot");
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let off = ((seed >> 11) % slots) * bs as u64;
            let buf = m.at(slot * bs);
            let e = match (op, fixedbuf, fixedfile) {
                ("write", false, false) => opcode::Write::new(types::Fd(fd), buf, bs as u32).offset(off).build(),
                ("write", false, true) => opcode::Write::new(types::Fixed(0), buf, bs as u32).offset(off).build(),
                ("write", true, false) => opcode::WriteFixed::new(types::Fd(fd), buf, bs as u32, 0).offset(off).build(),
                ("write", true, true) => opcode::WriteFixed::new(types::Fixed(0), buf, bs as u32, 0).offset(off).build(),
                ("read", false, false) => opcode::Read::new(types::Fd(fd), buf, bs as u32).offset(off).build(),
                ("read", false, true) => opcode::Read::new(types::Fixed(0), buf, bs as u32).offset(off).build(),
                ("read", true, false) => opcode::ReadFixed::new(types::Fd(fd), buf, bs as u32, 0).offset(off).build(),
                ("read", true, true) => opcode::ReadFixed::new(types::Fixed(0), buf, bs as u32, 0).offset(off).build(),
                _ => panic!("op"),
            };
            push(&mut r, &e.user_data(slot as u64));
            issued += 1;
            inflight += 1;
        }
        r.submit_and_wait(1).expect("submit_and_wait");
        for c in r.completion() {
            assert_eq!(c.result(), bs as i32, "qd cqe");
            free.push(c.user_data() as usize);
            inflight -= 1;
            done += 1;
        }
    }
    let secs = t0.elapsed().as_secs_f64();
    let cpu = (cpu_us() - cpu0) / n as f64;
    println!(
        "qd cfg={cfg} op={op} bs={bs} depth={depth} fixedbuf={} fixedfile={} ops_s={:.1} mb_s={:.1} cpu_us_op={cpu:.3} iowq_threads={}",
        fixedbuf as u8,
        fixedfile as u8,
        n as f64 / secs,
        (n * bs) as f64 / secs / 1e6,
        iowq_threads()
    );
    drop(r);
    drop(m);
}

fn status_kb(key: &str) -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap_or_default()
        .lines()
        .find(|l| l.starts_with(key))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn memlock(mib: usize, rings: usize, chunk_mib: usize) {
    let len = mib << 20;
    let m = Mem::new(len);
    let chunk = chunk_mib << 20;
    let iov: Vec<libc::iovec> = (0..len / chunk)
        .map(|i| libc::iovec { iov_base: m.at(i * chunk).cast(), iov_len: chunk })
        .collect();
    println!("memlock arena_mib={mib} chunks={} VmPin_kb={} VmLck_kb={}", iov.len(), status_kb("VmPin:"), status_kb("VmLck:"));
    let mut rs = Vec::new();
    for i in 0..rings {
        let r = ring("plain", 8);
        // SAFETY: m outlives every ring (rings dropped first below).
        let res = unsafe { r.submitter().register_buffers(&iov) };
        println!(
            "memlock ring={i} register={} VmPin_kb={} VmLck_kb={}",
            match &res { Ok(()) => "ok".to_owned(), Err(e) => format!("err{}", e.raw_os_error().unwrap_or(0)) },
            status_kb("VmPin:"),
            status_kb("VmLck:")
        );
        rs.push(r);
    }
    // Clone (6.12+): an R_DISABLED destination ring cloned from ring 0.
    if let Some(src) = rs.first() {
        let mut b: io_uring::Builder = IoUring::builder();
        b.setup_r_disabled();
        let dst: IoUring = b.build(8).expect("dst ring");
        let res = dst.submitter().register_buffers_clone(src.as_raw_fd());
        println!(
            "memlock clone={} VmPin_kb={}",
            match &res { Ok(()) => "ok".to_owned(), Err(e) => format!("err{}", e.raw_os_error().unwrap_or(0)) },
            status_kb("VmPin:")
        );
        drop(dst);
    }
    drop(rs);
    drop(m);
}

// ---------------------------------------------------------------------------
// E7: sequential bandwidth
// ---------------------------------------------------------------------------

fn seq(path: &str, total_mib: u64, bs_kib: usize, depth: usize, op: &str, engine: &str) {
    let size = total_mib << 20;
    let bs = bs_kib << 10;
    let f = if op == "read" && std::fs::metadata(path).map(|md| md.len() >= size).unwrap_or(false) {
        open_direct(path, false)
    } else {
        prepare_ready(path, size)
    };
    let fd = f.as_raw_fd();
    let d = if engine == "t3" { 1 } else { depth };
    let m = Mem::new(d * bs);
    m.fill(0, d * bs, 0x3C);
    let n = (size / bs as u64) as usize;
    let t0 = Instant::now();
    if engine == "t3" {
        for i in 0..n {
            let off = (i * bs) as i64;
            let iov = libc::iovec { iov_base: m.at(0).cast(), iov_len: bs };
            // SAFETY: valid fd and buffer.
            let w = unsafe { if op == "write" { libc::pwritev2(fd, &iov, 1, off, 0) } else { libc::preadv2(fd, &iov, 1, off, 0) } };
            assert_eq!(w, bs as isize, "seq t3 {}", errno());
        }
    } else {
        let mut r = ring("defer+tf", (d as u32).next_power_of_two().max(8));
        let mut free: Vec<usize> = (0..d).collect();
        let (mut issued, mut done, mut inflight) = (0usize, 0usize, 0usize);
        while done < n {
            while inflight < d && issued < n {
                let slot = free.pop().expect("slot");
                let off = (issued * bs) as u64;
                let e = if op == "write" {
                    opcode::Write::new(types::Fd(fd), m.at(slot * bs), bs as u32).offset(off).build()
                } else {
                    opcode::Read::new(types::Fd(fd), m.at(slot * bs), bs as u32).offset(off).build()
                };
                push(&mut r, &e.user_data(slot as u64));
                issued += 1;
                inflight += 1;
            }
            r.submit_and_wait(1).expect("submit_and_wait");
            for c in r.completion() {
                assert_eq!(c.result(), bs as i32, "seq cqe");
                free.push(c.user_data() as usize);
                inflight -= 1;
                done += 1;
            }
        }
    }
    if op == "write" {
        // SAFETY: valid fd.
        assert_eq!(unsafe { libc::fdatasync(fd) }, 0);
    }
    let secs = t0.elapsed().as_secs_f64();
    println!("seq op={op} engine={engine} bs_kib={bs_kib} depth={d} mb_s={:.1} secs={secs:.3} durable={}", size as f64 / secs / 1e6, (op == "write") as u8);
}

// ---------------------------------------------------------------------------
// E8: idle
// ---------------------------------------------------------------------------

extern "C" fn on_usr1(_: libc::c_int) {}

fn idle(path: &str, burst: usize, fsyncop: bool) {
    let f = prepare_ready(path, QD1_FILE);
    let fd = f.as_raw_fd();
    let m = Mem::new(BS);
    let mut r = ring("defer+tf", 64);
    for i in 0..burst as u64 {
        push(&mut r, &opcode::Write::new(types::Fd(fd), m.at(0), BS as u32).offset((i % 1024) * BS as u64).build().user_data(1));
        assert_eq!(wait_one(&mut r).1, BS as i32);
        if fsyncop {
            push(&mut r, &opcode::Fsync::new(types::Fd(fd)).flags(types::FsyncFlags::DATASYNC).build().user_data(2));
            assert_eq!(wait_one(&mut r).1, 0);
        }
    }
    // SAFETY: installing a no-op handler without SA_RESTART so enter returns EINTR.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_usr1 as usize;
        libc::sigaction(libc::SIGUSR1, &sa, std::ptr::null_mut());
    }
    // SAFETY: plain syscall.
    let tid = unsafe { libc::syscall(libc::SYS_gettid) };
    println!("idle_start pid={} tid={tid} iowq_threads={}", std::process::id(), iowq_threads());
    std::io::stdout().flush().expect("flush");
    // Park like an owner with nothing to do: GETEVENTS, min_complete = 1, no timeout.
    let res = r.submit_and_wait(1);
    println!("idle_end res={:?} iowq_threads={}", res.map_err(|e| e.raw_os_error()), iowq_threads());
}

// ---------------------------------------------------------------------------
// E9: CQ overflow
// ---------------------------------------------------------------------------

fn overflow(cfg: &str) {
    let mut b: io_uring::Builder = IoUring::builder();
    for part in cfg.split('+') {
        match part {
            "defer" => { b.setup_single_issuer(); b.setup_defer_taskrun(); }
            "tf" => { b.setup_taskrun_flag(); }
            "coop" => { b.setup_coop_taskrun(); }
            _ => {}
        }
    }
    let mut r: IoUring = b.build(4).expect("ring");
    let (sq, cq) = (r.params().sq_entries(), r.params().cq_entries());
    let total = 64u64;
    let mut pushed = 0u64;
    while pushed < total {
        for _ in 0..sq {
            if pushed == total { break; }
            push(&mut r, &opcode::Nop::new().build().user_data(pushed));
            pushed += 1;
        }
        // Submit without reaping; on a DEFER ring GETEVENTS runs the task work.
        let res = r.submitter().submit_and_wait(0);
        if let Err(e) = res { println!("overflow submit_err={}", e.raw_os_error().unwrap_or(0)); }
    }
    let flag = r.submission().cq_overflow();
    let dropped = r.completion().overflow();
    let mut seen = std::collections::BTreeSet::new();
    let mut rounds = 0;
    while (seen.len() as u64) < total && rounds < 1000 {
        for c in r.completion() {
            let _new = seen.insert(c.user_data());
        }
        // Flush the kernel's overflow list into the CQ.
        let res = r.submit_and_wait(0);
        if let Err(e) = res { println!("overflow flush_err={}", e.raw_os_error().unwrap_or(0)); }
        rounds += 1;
    }
    println!(
        "overflow cfg={cfg} sq={sq} cq={cq} submitted={total} cq_overflow_flag={} kernel_dropped={dropped} reaped_unique={} rounds={rounds} nodrop={}",
        flag as u8,
        seen.len(),
        r.params().is_feature_nodrop()
    );
}

// ---------------------------------------------------------------------------
// E13: dead owner's in-flight writes vs the OFD lock
// ---------------------------------------------------------------------------

fn ofd_lock(fd: RawFd) -> bool {
    let mut fl: libc::flock = unsafe { std::mem::zeroed() };
    fl.l_type = libc::F_WRLCK as i16;
    fl.l_whence = libc::SEEK_SET as i16;
    // SAFETY: valid fd and struct.
    unsafe { libc::fcntl(fd, libc::F_OFD_SETLK, &fl) == 0 }
}

fn killlock_child(path: &str, blocks: usize, engine: &str) {
    let f = open_direct(path, false);
    let fd = f.as_raw_fd();
    assert!(ofd_lock(fd), "child lock {}", errno());
    let m = Mem::new(blocks * BS);
    m.fill(0, blocks * BS, 0xAA);
    let base = Instant::now();
    if engine == "ring" {
        let mut r = ring("defer+tf", (blocks as u32).next_power_of_two().max(8));
        for i in 0..blocks {
            push(&mut r, &opcode::Write::new(types::Fd(fd), m.at(i * BS), BS as u32).offset((i * BS) as u64).build().user_data(i as u64));
        }
        r.submit().expect("submit");
        println!("SUBMITTED {}", now_ns(base));
        std::io::stdout().flush().expect("flush");
        std::mem::forget(r);
    } else {
        let p = m.at(0) as usize;
        let _t = std::thread::spawn(move || {
            let iov = libc::iovec { iov_base: p as *mut libc::c_void, iov_len: blocks * BS };
            // SAFETY: the mapping is leaked below; valid fd.
            unsafe { libc::pwritev2(fd, &iov, 1, 0, 0) };
        });
        std::thread::sleep(Duration::from_millis(50));
        println!("SUBMITTED {}", now_ns(base));
        std::io::stdout().flush().expect("flush");
    }
    std::mem::forget(m);
    std::mem::forget(f);
    loop {
        // SAFETY: plain syscall; killed by the parent.
        unsafe { libc::pause() };
    }
}

fn killlock(path: &str, blocks: usize, engine: &str) {
    {
        // Baseline content 0x11, durable.
        let f = open_direct(path, false);
        let m = Mem::new(blocks * BS);
        m.fill(0, blocks * BS, 0x11);
        let iov = libc::iovec { iov_base: m.at(0).cast(), iov_len: blocks * BS };
        // SAFETY: valid fd and buffer.
        assert_eq!(unsafe { libc::pwritev2(f.as_raw_fd(), &iov, 1, 0, libc::RWF_DSYNC) }, (blocks * BS) as isize);
    }
    let exe = std::env::current_exe().expect("exe");
    let mut child = std::process::Command::new(exe)
        .args(["killlock-child", path, &blocks.to_string(), engine])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn");
    let mut line = String::new();
    std::io::BufReader::new(child.stdout.take().expect("stdout")).read_line(&mut line).expect("read");
    assert!(line.starts_with("SUBMITTED"), "child said {line}");
    let t_kill = Instant::now();
    // SAFETY: our child.
    unsafe { libc::kill(child.id() as i32, libc::SIGKILL) };
    let f = open_direct(path, false);
    let fd = f.as_raw_fd();
    let mut tries = 0u64;
    while !ofd_lock(fd) {
        tries += 1;
        std::thread::sleep(Duration::from_micros(200));
    }
    let t_lock = t_kill.elapsed();
    // Write our own content right away, durably.
    let m = Mem::new(blocks * BS);
    m.fill(0, blocks * BS, 0xBB);
    let iov = libc::iovec { iov_base: m.at(0).cast(), iov_len: blocks * BS };
    // SAFETY: valid fd and buffer.
    assert_eq!(unsafe { libc::pwritev2(fd, &iov, 1, 0, libc::RWF_DSYNC) }, (blocks * BS) as isize);
    let t_ours = t_kill.elapsed();
    let status = child.wait().expect("wait");
    let t_reaped = t_kill.elapsed();
    // Let anything still in the delay target land, then read back.
    std::thread::sleep(Duration::from_secs(6));
    let rm = Mem::new(blocks * BS);
    let iov = libc::iovec { iov_base: rm.at(0).cast(), iov_len: blocks * BS };
    // SAFETY: valid fd and buffer.
    assert_eq!(unsafe { libc::preadv2(fd, &iov, 1, 0, 0) }, (blocks * BS) as isize);
    let (mut aa, mut bb, mut base) = (0, 0, 0);
    for i in 0..blocks {
        match rm.slice(i * BS, BS)[0] {
            0xAA => aa += 1,
            0xBB => bb += 1,
            0x11 => base += 1,
            _ => {}
        }
    }
    println!(
        "killlock engine={engine} blocks={blocks} lock_after_kill_ms={:.1} our_write_done_ms={:.1} child_reaped_ms={:.1} lock_tries={tries} final_ours={bb} final_dead_owner={aa} final_baseline={base} child_status={status}",
        t_lock.as_secs_f64() * 1e3,
        t_ours.as_secs_f64() * 1e3,
        t_reaped.as_secs_f64() * 1e3
    );
}

#[allow(dead_code)]
fn _unused(_: cqueue::Entry) {}
