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
    probe          create a store, print the device report and exit
    help           this text

OPTIONS:
    --secs <S>       measured seconds per point (default 5; warm-up is 10%, 0.2..1 s)
    --dir <PATH>     directory for stores and files (default: %TEMP% on Windows,
                     $HOME on Linux); a unique subdirectory is created and removed
    --out <PATH>     report directory (default: the harness's results/ folder)
    --commit <SHA>   store-io commit to record (default: git rev-parse HEAD)
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
    /// Device report only.
    Probe,
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
            Self::Probe => "probe",
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
            Self::Probe,
            Self::Help,
        ]
        .into_iter()
        .find(|c| c.name() == s)
    }
}

/// Parsed arguments.
#[derive(Debug, Clone)]
pub struct Args {
    /// What to run.
    pub command: Command,
    /// Measured seconds per point.
    pub secs: f64,
    /// Parent of the run directory.
    pub dir: PathBuf,
    /// Report directory.
    pub out: PathBuf,
    /// Commit override.
    pub commit: Option<String>,
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
        dir: default_dir(),
        out: PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/results")),
        commit: None,
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
            "--dir" => a.dir = PathBuf::from(value()?),
            "--out" => a.out = PathBuf::from(value()?),
            "--commit" => a.commit = Some(value()?),
            other => return Err(format!("unknown option `{other}`")),
        }
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
    fn test_parses_commands_and_options() {
        let a = args(&["lone", "--secs", "2.5", "--dir", "x", "--commit", "abc"])
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(a.command, Command::Lone);
        assert!((a.secs - 2.5).abs() < 1e-12);
        assert_eq!(a.dir, PathBuf::from("x"));
        assert_eq!(a.commit.as_deref(), Some("abc"));
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
        assert!(args(&["all", "--bogus", "1"]).is_err());
    }
}
