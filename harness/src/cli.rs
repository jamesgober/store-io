//! Command-line parsing (`std::env::args` only).

use std::path::PathBuf;

/// Usage text.
pub const USAGE: &str = "\
store-io-harness: time-boxed store-io vs raw-primitive vs fsys workloads

USAGE:
    store-io-harness <COMMAND> [OPTIONS]

COMMANDS:
    all            every workload below, in order
    lone           W1  lone writer, QD1 durable 4 KiB
    concurrent     W2  1..64 concurrent durable writers
    caller-batch   W3  N records + one barrier (N = 1..1000, 64 B..4 KiB)
    page-batch     W4  N random 4 KiB pages + one barrier
    sequential     W5  1 MiB sequential writes, 1/8 MiB sequential reads
    submitter      W6  N in flight from one thread, random 4 KiB, vs fio
    gates          the performance gate table (W1-W5, fio controls, extras;
                   G18 too with --devices, else NOT RUN)
    multi-gates    the gates that need several devices (G18), on --devices
    g19            G19 (commits beside a background Submitter), on --g19-dir
    probe          create a store, print the device report and exit
    observe-cost   what the observer adds per durable write (build with
                   --features observe; without it, the compiled-out cost)
    help           this text

OPTIONS:
    --secs <S>       measured seconds per point (default 5; warm-up is 10%, 0.2..1 s)
    --settle <S>     idle seconds before each workload (and each of `gates`'
                     inner workloads), so none starts while the device is
                     still recovering from the previous one's writes
                     (default 10; 0 to skip)
    --dir <PATH>     directory for stores and files (default: %TEMP% on Windows,
                     $HOME on Linux); a unique subdirectory is created and removed
    --out <PATH>     report directory (default: the harness's results/ folder)
    --commit <SHA>   store-io commit to record (default: git rev-parse HEAD)
    --tier <T>       Linux I/O tier: auto (default), sync or uring
    --devices <D,..> a directory on each of two or more devices, comma-separated
                     (the multi-device gates: G18)
    --g19-dir <D>    a directory on G19's device (CI's calibrated device, or
                     the dev box's drive); `gates` runs G19 there too
    --g19-part <P>   what `g19` runs: main (G19, its negative control and
                     G19t, auto-tuning's run, published),
                     variants (G19s, G19b, G19l: about 13 minutes) or all
                     (default)
    --g19-ref-mbps <X>
                     fio's MB/s at the paced shape alone on that device (256
                     KiB, one in flight; the device script's); else G19
                     measures its own writer at that shape
";

/// A workload command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Every workload.
    All,
    /// W1.
    Lone,
    /// W2.
    Concurrent,
    /// W3.
    CallerBatch,
    /// W4.
    PageBatch,
    /// W5.
    Sequential,
    /// W6.
    Submitter,
    /// The performance gates.
    Gates,
    /// The gates that need several devices.
    MultiGates,
    /// G19 alone.
    G19,
    /// Device report only.
    Probe,
    /// The observer's cost per durable write.
    ObserveCost,
    /// Usage.
    Help,
}

impl Command {
    /// The command's name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Lone => "lone",
            Self::Concurrent => "concurrent",
            Self::CallerBatch => "caller-batch",
            Self::PageBatch => "page-batch",
            Self::Sequential => "sequential",
            Self::Submitter => "submitter",
            Self::Gates => "gates",
            Self::MultiGates => "multi-gates",
            Self::G19 => "g19",
            Self::Probe => "probe",
            Self::ObserveCost => "observe-cost",
            Self::Help => "help",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        [
            Self::All,
            Self::Lone,
            Self::Concurrent,
            Self::CallerBatch,
            Self::PageBatch,
            Self::Sequential,
            Self::Submitter,
            Self::Gates,
            Self::MultiGates,
            Self::G19,
            Self::Probe,
            Self::ObserveCost,
            Self::Help,
        ]
        .into_iter()
        .find(|c| c.name() == s)
    }
}

/// What `g19` runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum G19Part {
    /// G19 and its negative control.
    Main,
    /// G19s, G19b and G19l.
    Variants,
    /// Both.
    All,
}

/// Parsed arguments.
#[derive(Debug, Clone)]
pub struct Args {
    /// What to run.
    pub command: Command,
    /// Measured seconds per point.
    pub secs: f64,
    /// Idle seconds before each workload.
    pub settle: f64,
    /// Parent of the run directory.
    pub dir: PathBuf,
    /// Report directory.
    pub out: PathBuf,
    /// Commit override.
    pub commit: Option<String>,
    /// Linux I/O tier.
    pub tier: store_io_platform::LinuxTier,
    /// A directory on each device of the multi-device gates.
    pub devices: Vec<PathBuf>,
    /// G19's device directory.
    pub g19_dir: Option<PathBuf>,
    /// fio's rate at G19's paced shape, MB/s.
    pub g19_ref_mbps: Option<f64>,
    /// What `g19` runs.
    pub g19_part: G19Part,
}

fn default_dir() -> PathBuf {
    if cfg!(windows) {
        std::env::temp_dir()
    } else {
        std::env::var_os("HOME").map_or_else(std::env::temp_dir, PathBuf::from)
    }
}

/// Parses arguments (without the program name).
///
/// # Errors
///
/// A message naming the bad argument.
pub fn parse(mut it: impl Iterator<Item = String>) -> Result<Args, String> {
    let cmd = it.next().ok_or("missing command")?;
    let command = Command::parse(&cmd).ok_or_else(|| format!("unknown command `{cmd}`"))?;
    let mut a = Args {
        command,
        secs: 5.0,
        settle: 10.0,
        dir: default_dir(),
        out: PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/results")),
        commit: None,
        tier: store_io_platform::LinuxTier::Auto,
        devices: Vec::new(),
        g19_dir: None,
        g19_ref_mbps: None,
        g19_part: G19Part::All,
    };
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("`{flag}` needs a value"));
        match flag.as_str() {
            "--secs" => {
                let v = value()?;
                a.secs = v
                    .parse::<f64>()
                    .ok()
                    .filter(|s| s.is_finite() && *s > 0.0 && *s <= 3600.0)
                    .ok_or_else(|| format!("bad --secs `{v}` (0 < S <= 3600)"))?;
            }
            "--settle" => {
                let v = value()?;
                a.settle = v
                    .parse::<f64>()
                    .ok()
                    .filter(|s| s.is_finite() && (0.0..=600.0).contains(s))
                    .ok_or_else(|| format!("bad --settle `{v}` (0 <= S <= 600)"))?;
            }
            "--dir" => a.dir = PathBuf::from(value()?),
            "--out" => a.out = PathBuf::from(value()?),
            "--commit" => a.commit = Some(value()?),
            "--tier" => {
                use store_io_platform::LinuxTier;
                let v = value()?;
                a.tier = match v.as_str() {
                    "auto" => LinuxTier::Auto,
                    "sync" => LinuxTier::Sync,
                    "uring" => LinuxTier::Uring,
                    _ => return Err(format!("bad --tier `{v}` (auto, sync or uring)")),
                };
            }
            "--devices" => {
                let v = value()?;
                a.devices = v
                    .split(',')
                    .filter(|d| !d.is_empty())
                    .map(PathBuf::from)
                    .collect();
                if a.devices.len() < 2 {
                    return Err(format!("bad --devices `{v}` (two or more directories)"));
                }
            }
            "--g19-dir" => a.g19_dir = Some(PathBuf::from(value()?)),
            "--g19-part" => {
                let v = value()?;
                a.g19_part = match v.as_str() {
                    "main" => G19Part::Main,
                    "variants" => G19Part::Variants,
                    "all" => G19Part::All,
                    _ => return Err(format!("bad --g19-part `{v}` (main, variants or all)")),
                };
            }
            "--g19-ref-mbps" => {
                let v = value()?;
                a.g19_ref_mbps = Some(
                    v.parse::<f64>()
                        .ok()
                        .filter(|x| x.is_finite() && *x > 0.0)
                        .ok_or_else(|| format!("bad --g19-ref-mbps `{v}` (MB/s above 0)"))?,
                );
            }
            other => return Err(format!("unknown option `{other}`")),
        }
    }
    if a.command == Command::MultiGates && a.devices.is_empty() {
        return Err("`multi-gates` needs --devices (two or more directories)".to_owned());
    }
    if a.command == Command::G19 && a.g19_dir.is_none() {
        return Err("`g19` needs --g19-dir (a directory on G19's device)".to_owned());
    }
    Ok(a)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Result<Args, String> {
        parse(v.iter().map(|s| (*s).to_owned()))
    }

    #[test]
    fn test_parses_the_multi_device_gates_and_their_devices() {
        let a = args(&["multi-gates", "--devices", "/d0,/d1,/d2,/d3"])
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(a.command, Command::MultiGates);
        assert_eq!(a.devices.len(), 4);
        assert!(args(&["multi-gates", "--devices", "/d0"]).is_err());
        assert!(args(&["multi-gates"]).is_err());
        assert!(args(&["gates"]).is_ok_and(|a| a.devices.is_empty()));
    }

    #[test]
    fn test_parses_g19_and_its_device() {
        let a = args(&["g19", "--g19-dir", "/mnt/g19", "--g19-ref-mbps", "61.5"])
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(a.command, Command::G19);
        assert_eq!(a.g19_dir, Some(PathBuf::from("/mnt/g19")));
        assert_eq!(a.g19_ref_mbps, Some(61.5));
        assert!(args(&["g19"]).is_err());
        assert!(args(&["g19", "--g19-dir", "/d", "--g19-ref-mbps", "0"]).is_err());
        assert_eq!(a.g19_part, G19Part::All);
        let v = args(&["g19", "--g19-dir", "/d", "--g19-part", "variants"])
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(v.g19_part, G19Part::Variants);
        assert!(args(&["g19", "--g19-dir", "/d", "--g19-part", "some"]).is_err());
        assert!(args(&["gates"]).is_ok_and(|a| a.g19_dir.is_none()));
    }

    #[test]
    fn test_parses_commands_and_options() {
        let a = args(&["lone", "--secs", "2.5", "--dir", "x", "--commit", "abc"])
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(a.command, Command::Lone);
        assert!((a.secs - 2.5).abs() < 1e-12);
        assert_eq!(a.dir, PathBuf::from("x"));
        assert_eq!(a.commit.as_deref(), Some("abc"));
        assert!((a.settle - 10.0).abs() < 1e-12);
        let a = args(&["all", "--settle", "0"]).unwrap_or_else(|e| panic!("{e}"));
        assert!(a.settle.abs() < 1e-12);
        assert_eq!(
            args(&["caller-batch"]).map(|a| a.command),
            Ok(Command::CallerBatch)
        );
    }

    #[test]
    fn test_rejects_bad_input() {
        assert!(args(&[]).is_err());
        assert!(args(&["nope"]).is_err());
        assert!(args(&["all", "--secs", "0"]).is_err());
        assert!(args(&["all", "--secs"]).is_err());
        assert!(args(&["all", "--settle", "-1"]).is_err());
        assert!(args(&["all", "--settle", "601"]).is_err());
        assert!(args(&["all", "--bogus", "1"]).is_err());
    }
}
