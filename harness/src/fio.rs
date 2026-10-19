//! fio as the baseline: a job runner and a small, bounded reader of its JSON
//! output (no JSON library: each search stays inside its own object).

use std::path::Path;
use std::process::Command;

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
    /// CPU time the job used (user and system), seconds.
    pub cpu_s: f64,
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
    let text = run_raw(&all)?;
    parse(&text, side).ok_or_else(|| "fio output not understood".to_owned())
}

fn run_raw(args: &[String]) -> Result<String, String> {
    let o = Command::new("fio")
        .args(args)
        .output()
        .map_err(|e| format!("fio did not run: {e}"))?;
    if !o.status.success() {
        return Err(format!(
            "fio failed: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
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
    let jobs = &json[json.find("\"jobs\"")?..];
    let job = object_after(jobs, "[")?;
    let sect = object_after(job, &format!("\"{side}\" :"))?;
    let lat = object_after(sect, "\"lat_ns\" :")?;
    let pct = object_after(lat, "\"percentile\" :")?;
    let at = |k: &str| number_after(pct, &format!("\"{k}\" :")).map(|ns| ns / 1000.0);
    let usr = number_after(job, "\"usr_cpu\" :")?;
    let sys = number_after(job, "\"sys_cpu\" :")?;
    let runtime_s = number_after(job, "\"job_runtime\" :")? / 1000.0;
    Some(Stats {
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
            "write" : { "iops" : 12345.5, "bw_bytes" : 50567168,
                "clat_ns" : { "percentile" : { "50.000000" : 1 } },
                "lat_ns" : { "percentile" : { "50.000000" : 41984, "99.000000" : 70144, "99.900000" : 90112 } } },
            "job_runtime" : 2000, "usr_cpu" : 10.0, "sys_cpu" : 15.0 } ] }"#;
        let w = parse(j, "write").unwrap_or_else(|| panic!("parsed"));
        assert!((w.iops - 12345.5).abs() < 1e-9);
        assert!((w.bytes_per_s - 50_567_168.0).abs() < 1e-6);
        assert!((w.p50_us - 41.984).abs() < 1e-9 && (w.p999_us - 90.112).abs() < 1e-9);
        assert!((w.runtime_s - 2.0).abs() < 1e-9 && (w.cpu_s - 0.5).abs() < 1e-9);
        // The read side has no percentiles: nothing is taken from "write".
        assert!(parse(j, "read").is_none());
    }
}
