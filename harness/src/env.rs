//! The environment a run measured: OS, kernel / build, CPU, filesystem and
//! mount, and the date and file-name pieces of the report.

use std::path::Path;
use std::process::Command;

/// Facts about the machine and the directory under test.
#[derive(Debug, Clone)]
pub struct Env {
    /// `windows` or `linux`.
    pub os: &'static str,
    /// OS name and version.
    pub os_version: String,
    /// Kernel release (Linux) or OS build (Windows).
    pub kernel: String,
    /// CPU model string.
    pub cpu: String,
    /// Logical CPUs available to the process.
    pub cpus: usize,
    /// Filesystem of the directory under test, with mount details where the
    /// OS exposes them.
    pub filesystem: String,
    /// The directory the stores and raw files lived in.
    pub dir: String,
    /// UTC date of the run, `yyyy-mm-dd`.
    pub date: String,
    /// UTC timestamp of the run start, `yyyy-mm-ddThh:mm:ssZ`.
    pub started: String,
    /// The store-io commit measured.
    pub commit: String,
}

/// Collects the environment for `dir`, whose filesystem kind store-io's
/// probe reported as `fs_kind`.
#[must_use]
pub fn collect(dir: &Path, fs_kind: &str, commit: Option<&str>) -> Env {
    let (date, started) = utc_now();
    Env {
        os: std::env::consts::OS,
        os_version: os_version(),
        kernel: kernel(),
        cpu: cpu_model(),
        cpus: std::thread::available_parallelism().map_or(0, std::num::NonZeroUsize::get),
        filesystem: filesystem(dir, fs_kind),
        dir: dir.display().to_string(),
        date,
        started,
        commit: commit.map_or_else(git_commit, str::to_owned),
    }
}

fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn git_commit() -> String {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/..");
    let sha = run("git", &["-C", root, "rev-parse", "HEAD"]);
    let dirty = run(
        "git",
        &["-C", root, "status", "--porcelain", "--", "crates"],
    )
    .map(|s| !s.is_empty());
    match (sha, dirty) {
        (Some(s), Some(true)) => format!("{s} (crates/ modified)"),
        (Some(s), _) => s,
        (None, _) => "unknown (git unavailable; pass --commit)".to_owned(),
    }
}

#[cfg(windows)]
fn os_version() -> String {
    run("cmd", &["/C", "ver"]).unwrap_or_else(|| "Windows (version unreadable)".to_owned())
}

#[cfg(windows)]
fn kernel() -> String {
    // `ver` prints "Microsoft Windows [Version 10.0.26200.9457]".
    run("cmd", &["/C", "ver"])
        .and_then(|s| {
            let start = s.find("Version ")? + "Version ".len();
            let end = s[start..].find(']')? + start;
            Some(format!("NT build {}", &s[start..end]))
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

#[cfg(windows)]
fn cpu_model() -> String {
    std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_else(|_| "unknown".to_owned())
}

#[cfg(windows)]
fn filesystem(dir: &Path, fs_kind: &str) -> String {
    let drive = dir
        .components()
        .next()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("{fs_kind} (volume {drive})")
}

#[cfg(target_os = "linux")]
fn os_version() -> String {
    std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|t| {
            t.lines()
                .find_map(|l| l.strip_prefix("PRETTY_NAME="))
                .map(|v| v.trim_matches('"').to_owned())
        })
        .unwrap_or_else(|| "Linux (os-release unreadable)".to_owned())
}

#[cfg(target_os = "linux")]
fn kernel() -> String {
    std::fs::read_to_string("/proc/version")
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|_| "unknown".to_owned())
}

#[cfg(target_os = "linux")]
fn cpu_model() -> String {
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|t| {
            t.lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split_once(':'))
                .map(|(_, v)| v.trim().to_owned())
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

/// The mount holding `dir`, from `/proc/self/mountinfo` (longest matching
/// mount point wins): type, source, mount options and super options.
#[cfg(target_os = "linux")]
fn filesystem(dir: &Path, fs_kind: &str) -> String {
    let Ok(text) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return format!("{fs_kind} (mountinfo unreadable)");
    };
    let dir = dir.to_string_lossy();
    let mut best: Option<(usize, String)> = None;
    for line in text.lines() {
        // id parent maj:min root mountpoint options [optional...] - type source superopts
        let Some((left, right)) = line.split_once(" - ") else {
            continue;
        };
        let l: Vec<&str> = left.split(' ').collect();
        let r: Vec<&str> = right.split(' ').collect();
        let (Some(mp), Some(opts)) = (l.get(4), l.get(5)) else {
            continue;
        };
        let covers = dir == *mp
            || (dir.starts_with(*mp) && (mp.ends_with('/') || dir[mp.len()..].starts_with('/')));
        if covers && best.as_ref().is_none_or(|(n, _)| mp.len() >= *n) {
            let desc = format!(
                "{} on {} (device {} {}, mount options {}, super options {})",
                r.first().unwrap_or(&"?"),
                mp,
                r.get(1).unwrap_or(&"?"),
                l.get(2).unwrap_or(&"?"),
                opts,
                r.get(2).unwrap_or(&"?"),
            );
            best = Some((mp.len(), desc));
        }
    }
    best.map_or_else(
        || format!("{fs_kind} (mount not found)"),
        |(_, d)| format!("{d}; store-io classified it {fs_kind}"),
    )
}

/// Today's UTC date and the current UTC timestamp.
fn utc_now() -> (String, String) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    (
        format!("{y:04}-{m:02}-{d:02}"),
        format!(
            "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
            rem / 3600,
            rem % 3600 / 60,
            rem % 60
        ),
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// A short, file-name-safe device name: lowercase ASCII letters, digits and
/// single dashes.
#[must_use]
pub fn short_device_name(model: Option<&str>) -> String {
    let Some(model) = model else {
        return "unknown-device".to_owned();
    };
    let mut out = String::new();
    for c in model.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-').to_owned();
    if out.is_empty() {
        "unknown-device".to_owned()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(20_735), (2026, 10, 9));
    }

    #[test]
    fn test_short_device_names() {
        assert_eq!(
            short_device_name(Some("T-FORCE TM8FPZ004T")),
            "t-force-tm8fpz004t"
        );
        assert_eq!(
            short_device_name(Some("  Msft  Virtual Disk ")),
            "msft-virtual-disk"
        );
        assert_eq!(short_device_name(None), "unknown-device");
        assert_eq!(short_device_name(Some("--")), "unknown-device");
    }
}
