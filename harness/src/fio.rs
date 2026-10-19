//! fio as the baseline: a job runner and a small, bounded reader of its JSON
//! output (no JSON library: each search stays inside its own object).

use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};

/// fio's asynchronous engine on this system.
#[must_use]
pub fn engine() -> &'static str {
    if cfg!(windows) {
        "windowsaio"
    } else if cfg!(target_os = "linux") {
        "io_uring"
    } else {
        "posixaio"
    }
}

/// fio's synchronous engine: `psync`; on Windows, which has no synchronous
/// engine for direct I/O, `windowsaio` at depth 1 (fio's own advice).
#[must_use]
pub fn sync_engine() -> &'static str {
    if cfg!(windows) { "windowsaio" } else { "psync" }
}

/// fio's `--filename`: on Windows a `:` must be escaped.
#[must_use]
pub fn path_arg(path: &Path) -> String {
    let s = path.display().to_string();
    if cfg!(windows) {
        s.replace(':', "\\:")
    } else {
        s
    }
}

/// One side ("read" or "write") of a finished fio job.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stats {
    /// Operations per second.
    pub iops: f64,
    /// Bytes per second.
    pub bytes_per_s: f64,
    /// Total latency (submission and completion) p50 / p99 / p99.9, µs.
    pub p50_us: f64,
    pub p99_us: f64,
    pub p999_us: f64,
    /// CPU time the job used (user and system), seconds, as fio reports
    /// it (the job's own thread or process: on Windows that leaves out the
    /// completion thread of `windowsaio`).
    pub cpu_s: f64,
    /// Operations issued in the measured window (fio's `total_ios`; at
    /// most the queue depth more than those completed).
    pub ios: f64,
    /// The job's run time, seconds.
    pub runtime_s: f64,
}

/// Writes `file` in full (1 MiB direct writes, then fsync), so later jobs
/// overwrite allocated, written blocks, as store-io's regions are.
///
/// # Errors
///
/// fio's failure, as text.
pub fn lay_out(file: &Path, bytes: u64) -> Result<(), String> {
    run_raw(&[
        "--name=layout".to_owned(),
        format!("--filename={}", path_arg(file)),
        format!("--size={bytes}"),
        "--rw=write".to_owned(),
        "--bs=1m".to_owned(),
        "--direct=1".to_owned(),
        "--end_fsync=1".to_owned(),
    ])
    .map(|_| ())
}

/// Runs one time-based job on `file` (`args` are the job's own options),
/// with `warmup` seconds of ramp and `secs` measured, and returns the
/// `side` ("read" or "write") of its result.
///
/// # Errors
///
/// fio's failure or an output this reader does not understand, as text.
pub fn job(
    file: &Path,
    bytes: u64,
    args: &[&str],
    side: &str,
    secs: f64,
    warmup: f64,
) -> Result<Stats, String> {
    let mut all = vec![
        "--name=job".to_owned(),
        format!("--filename={}", path_arg(file)),
        format!("--size={bytes}"),
        "--direct=1".to_owned(),
        "--numjobs=1".to_owned(),
        "--randrepeat=0".to_owned(),
        "--lat_percentiles=1".to_owned(),
        "--time_based".to_owned(),
        format!("--runtime={}ms", (secs * 1000.0) as u64),
        format!("--ramp_time={}ms", (warmup * 1000.0) as u64),
        "--output-format=json".to_owned(),
    ];
    all.extend(args.iter().map(|a| (*a).to_owned()));
    let (text, _) = run_raw(&all)?;
    parse(&text, side).ok_or_else(|| "fio output not understood".to_owned())
}

/// Runs one time-based job of `secs` (no ramp) on `file` and returns its
/// `side` with `cpu_s` replaced by the CPU time of fio's whole process
/// (every thread: on Windows, `windowsaio` reaps on a thread of its own)
/// less that of the same job stopped after a few operations (start-up and
/// exit). `ios` then counts every operation the process issued. This is
/// what the measurement of store-io counts: its whole process (`fio` must
/// be fio itself, not a launcher: only that process is measured on
/// Windows; a launcher fails the gate, never passes it). fio runs
/// lean (`--gtod_reduce=1`: no clock reads or latency statistics per
/// operation; `--disk_util=0` on Linux), so it does no more bookkeeping than
/// store-io's loop; latencies are not measured (NaN).
///
/// # Errors
///
/// fio's failure, an output this reader does not understand, or a system
/// that does not report a child's CPU time, as text.
pub fn cpu_job(
    file: &Path,
    bytes: u64,
    args: &[&str],
    side: &str,
    secs: f64,
) -> Result<Stats, String> {
    let base = |more: &[String]| {
        let mut all = vec![
            "--name=job".to_owned(),
            format!("--filename={}", path_arg(file)),
            format!("--size={bytes}"),
            "--direct=1".to_owned(),
            "--numjobs=1".to_owned(),
            "--randrepeat=0".to_owned(),
            "--gtod_reduce=1".to_owned(),
            "--output-format=json".to_owned(),
        ];
        if cfg!(target_os = "linux") {
            all.push("--disk_util=0".to_owned());
        }
        all.extend(args.iter().map(|a| (*a).to_owned()));
        all.extend_from_slice(more);
        all
    };
    let (text, cpu) = run_raw(&base(&[
        "--time_based".to_owned(),
        format!("--runtime={}ms", (secs * 1000.0) as u64),
    ]))?;
    // The same job, ended after 256 KiB: what start-up and exit cost.
    let (_, idle) = run_raw(&base(&["--io_size=256k".to_owned()]))?;
    let mut st =
        parse_with(&text, side, false).ok_or_else(|| "fio output not understood".to_owned())?;
    match (cpu, idle) {
        (Some(c), Some(i)) if c > i => st.cpu_s = c - i,
        (Some(c), Some(i)) => {
            return Err(format!(
                "fio's start-up run used as much CPU as the measured one ({i:.3} s, {c:.3} s)"
            ));
        }
        _ => return Err("fio's process CPU time is not measured on this system".to_owned()),
    }
    Ok(st)
}

/// Runs fio; returns its standard output and, where the system reports it,
/// the CPU time its whole process used (user and system, every thread and
/// every process it waited for), seconds.
fn run_raw(args: &[String]) -> Result<(String, Option<f64>), String> {
    let before = children_cpu_s();
    let mut child = Command::new("fio")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("fio did not run: {e}"))?;
    // stderr on its own thread, so neither pipe can fill and stall fio.
    let mut err = child.stderr.take();
    let errs = std::thread::spawn(move || {
        let mut s = Vec::new();
        if let Some(e) = err.as_mut() {
            let _n = e.read_to_end(&mut s);
        }
        s
    });
    let mut out = Vec::new();
    if let Some(o) = child.stdout.as_mut() {
        let _n = o.read_to_end(&mut out);
    }
    let status = child
        .wait()
        .map_err(|e| format!("fio did not finish: {e}"))?;
    let stderr = errs.join().unwrap_or_default();
    let cpu = process_cpu_s(&child).or_else(|| Some(children_cpu_s()? - before?));
    if !status.success() {
        return Err(format!(
            "fio failed: {}",
            String::from_utf8_lossy(&stderr).trim()
        ));
    }
    Ok((String::from_utf8_lossy(&out).into_owned(), cpu))
}

/// The CPU time of a child that has exited and is still held: Windows.
#[cfg(windows)]
fn process_cpu_s(child: &Child) -> Option<f64> {
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetProcessTimes;
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut c, mut e, mut k, mut u) = (zero, zero, zero, zero);
    // SAFETY: the child's process handle, open while `child` is held (an
    // exited process's times stay readable through it), and four FILETIMEs
    // the call fills.
    let ok = unsafe {
        GetProcessTimes(
            child.as_raw_handle(),
            &raw mut c,
            &raw mut e,
            &raw mut k,
            &raw mut u,
        )
    };
    // 100 ns units.
    let t = |f: FILETIME| {
        ((u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime)) as f64 / 1e7
    };
    (ok != 0).then(|| t(k) + t(u))
}

#[cfg(not(windows))]
fn process_cpu_s(_child: &Child) -> Option<f64> {
    None
}

/// The CPU time of every child this process has waited for, so far: Linux
/// (fio's job processes are waited for by fio, so they count).
#[cfg(target_os = "linux")]
fn children_cpu_s() -> Option<f64> {
    // SAFETY: getrusage(2) fills the struct it is given.
    let mut u: libc::rusage = unsafe { core::mem::zeroed() };
    // SAFETY: as above.
    let rc = unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &raw mut u) };
    let tv = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    (rc == 0).then(|| tv(u.ru_utime) + tv(u.ru_stime))
}

#[cfg(not(target_os = "linux"))]
fn children_cpu_s() -> Option<f64> {
    None
}

/// The object that follows `key` in `s` (from its `{` to the matching `}`).
fn object_after<'a>(s: &'a str, key: &str) -> Option<&'a str> {
    let start = s.find(key)? + key.len();
    let open = start + s[start..].find('{')?;
    let mut depth = 0usize;
    for (i, ch) in s[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[open..=open + i]);
                }
            }
            _ => {}
        }
    }
    None
}

fn number_after(s: &str, key: &str) -> Option<f64> {
    let i = s.find(key)? + key.len();
    let rest = s[i..].trim_start();
    let end = rest
        .find(|ch: char| !(ch.is_ascii_digit() || ch == '.' || ch == '-' || ch == 'e' || ch == '+'))
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// The first job's `side` in fio's JSON output.
fn parse(json: &str, side: &str) -> Option<Stats> {
    parse_with(json, side, true)
}

/// As [`parse`]; without `latencies` the percentiles are not read (NaN).
fn parse_with(json: &str, side: &str, latencies: bool) -> Option<Stats> {
    let jobs = &json[json.find("\"jobs\"")?..];
    let job = object_after(jobs, "[")?;
    let sect = object_after(job, &format!("\"{side}\" :"))?;
    let pct = if latencies {
        let lat = object_after(sect, "\"lat_ns\" :")?;
        Some(object_after(lat, "\"percentile\" :")?)
    } else {
        None
    };
    let at = |k: &str| match pct {
        Some(p) => number_after(p, &format!("\"{k}\" :")).map(|ns| ns / 1000.0),
        None => Some(f64::NAN),
    };
    let usr = number_after(job, "\"usr_cpu\" :")?;
    let sys = number_after(job, "\"sys_cpu\" :")?;
    let runtime_s = number_after(job, "\"job_runtime\" :")? / 1000.0;
    Some(Stats {
        ios: number_after(sect, "\"total_ios\" :")?,
        iops: number_after(sect, "\"iops\" :")?,
        bytes_per_s: number_after(sect, "\"bw_bytes\" :")?,
        p50_us: at("50.000000")?,
        p99_us: at("99.000000")?,
        p999_us: at("99.900000")?,
        cpu_s: (usr + sys) / 100.0 * runtime_s,
        runtime_s,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_reads_each_side_inside_its_own_object() {
        let j = r#"{ "jobs" : [ { "jobname" : "job", "job options" : { "rw" : "randwrite" },
            "read" : { "iops" : 0.0, "bw_bytes" : 0, "lat_ns" : { "min" : 0 } },
            "write" : { "iops" : 12345.5, "bw_bytes" : 50567168, "total_ios" : 24691,
                "clat_ns" : { "percentile" : { "50.000000" : 1 } },
                "lat_ns" : { "percentile" : { "50.000000" : 41984, "99.000000" : 70144, "99.900000" : 90112 } } },
            "job_runtime" : 2000, "usr_cpu" : 10.0, "sys_cpu" : 15.0 } ] }"#;
        let w = parse(j, "write").unwrap_or_else(|| panic!("parsed"));
        assert!((w.iops - 12345.5).abs() < 1e-9 && (w.ios - 24691.0).abs() < 1e-9);
        assert!((w.bytes_per_s - 50_567_168.0).abs() < 1e-6);
        assert!((w.p50_us - 41.984).abs() < 1e-9 && (w.p999_us - 90.112).abs() < 1e-9);
        assert!((w.runtime_s - 2.0).abs() < 1e-9 && (w.cpu_s - 0.5).abs() < 1e-9);
        // The read side has no percentiles: nothing is taken from "write".
        assert!(parse(j, "read").is_none());
        // Without latencies (a lean CPU job) the percentiles are not needed.
        let r = parse_with(j, "write", false).unwrap_or_else(|| panic!("parsed"));
        assert!(r.p50_us.is_nan() && (r.ios - 24691.0).abs() < 1e-9);
    }
}
