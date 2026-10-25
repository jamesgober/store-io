//! The `store-io` command-line tool.
//!
//! ```text
//! store-io probe <dir>    what store-io learns about the device under <dir>
//! store-io info <store>   a store's device report, regions and space (read-only)
//! store-io copy [--override-refusal] [--accept-stale-pages] <old> <new>
//!                         copy a store into a new format-2 store
//! ```

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::process::ExitCode;

const USAGE: &str = "usage:
  store-io probe <dir>    show the evidence and durability class of <dir>'s device
  store-io info <store>   show a store's report, regions and space (read-only)
  store-io copy [--override-refusal] [--accept-stale-pages] <old> <new>
                          copy the store in <old> into a new format-2 store in <new>
                          (<old> is never written; <new> must hold no store);
                          --override-refusal creates the copy even on a device
                          store-io refuses (its durability is not established;
                          later writable opens need the override too);
                          --accept-stale-pages copies a store whose table is
                          damaged (frozen, or with unavailable regions) as far
                          as it can, and lists what it left out or could not
                          verify (exit status 3 when it lists anything)";

/// `println!` that ends the program when stdout is gone instead of
/// panicking: quietly for a closed pipe (output piped into `head`), with a
/// message and exit status 1 for any other failure.
macro_rules! out {
    ($($t:tt)*) => {{
        use std::io::Write as _;
        if let Err(e) = writeln!(std::io::stdout().lock(), $($t)*) {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                std::process::exit(0);
            }
            eprintln!("store-io: writing the output failed: {e}");
            std::process::exit(1);
        }
    }};
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["probe", dir] => probe(dir),
        ["info", store] => info(store),
        ["copy", rest @ ..] => match copy_args(rest) {
            // A copy that left something out or unverified: exit status 3.
            Some((from, to, flags)) => match copy(from, to, flags) {
                Ok(true) => return ExitCode::from(3),
                other => other.map(drop),
            },
            None => {
                eprintln!("{USAGE}");
                return ExitCode::from(2);
            }
        },
        ["--version" | "-V"] => {
            out!("store-io {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("store-io: {e}");
            if args.first().is_some_and(|a| a == "copy")
                && matches!(
                    e,
                    store_io::Error::UnsafeDevice { .. } | store_io::Error::Unverified { .. }
                )
            {
                eprintln!(
                    "store-io: the target's device is refused; --override-refusal copies anyway"
                );
            }
            ExitCode::FAILURE
        }
    }
}

/// An argument that looks like an option, never taken as a path.
fn is_flag(a: &str) -> bool {
    a.starts_with("--")
}

#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
fn probe(dir: &str) -> Result<(), store_io::Error> {
    let p = store_io::probe(dir)?;
    let d = &p.decision;
    out!("class:    {}", d.class);
    out!("label:    {}", d.label);
    out!("open:     {:?}", d.durable_open);
    for r in d.reasons.iter() {
        out!("reason:   {r}");
    }
    for m in d.missing.iter() {
        out!("missing:  {m}");
    }
    out!();
    out!("{:#?}", p.evidence);
    Ok(())
}

#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
fn info(path: &str) -> Result<(), store_io::Error> {
    let store = store_io::Store::open_readonly(path)?;
    out!("{}", store.report());
    out!();
    out!("regions:");
    for (name, kind) in store.regions() {
        out!("  {name:<24} {kind:?}");
    }
    let s = store.space();
    out!();
    out!(
        "space: container {} B, regions {} B, released {} B, reserved {} B",
        s.container,
        s.regions,
        s.released,
        s.reserved
    );
    for t in &s.tags {
        out!(
            "  tag {:>10}: logical {} B, physical {} B, reserved {} B, cap {:?}",
            t.tag,
            t.logical,
            t.physical,
            t.reserved,
            t.cap
        );
    }
    Ok(())
}

/// `copy`'s flags.
#[derive(Debug, Clone, Copy, Default)]
struct CopyFlags {
    override_refusal: bool,
    accept_stale_pages: bool,
}

/// `copy`'s arguments: each flag at most once, in any order, before the
/// two directories (neither one that looks like a flag).
fn copy_args<'a>(args: &[&'a str]) -> Option<(&'a str, &'a str, CopyFlags)> {
    let mut flags = CopyFlags::default();
    let mut i = 0;
    while let Some(&a) = args.get(i).filter(|a| is_flag(a)) {
        let set = match a {
            "--override-refusal" => &mut flags.override_refusal,
            "--accept-stale-pages" => &mut flags.accept_stale_pages,
            _ => return None,
        };
        if *set {
            return None;
        }
        *set = true;
        i += 1;
    }
    match args[i..] {
        [from, to] if !is_flag(from) && !is_flag(to) => Some((from, to, flags)),
        _ => None,
    }
}

/// Copies; `Ok(true)` when the report lists anything left out or not
/// verified.
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
fn copy(from: &str, to: &str, flags: CopyFlags) -> Result<bool, store_io::Error> {
    let mut opts = store_io::CopyOptions::default();
    opts.store.trust.override_refusal = flags.override_refusal;
    opts.accept_stale_pages = flags.accept_stale_pages;
    let r = store_io::copy_store(from, to, opts)?;
    out!(
        "copied {} regions, {} bytes, from format {} to format 2{}",
        r.regions,
        r.bytes,
        r.source_format,
        if flags.override_refusal {
            " (device refusal overridden)"
        } else {
            ""
        }
    );
    for id in &r.omitted {
        out!("  left out: table entry {id} (see `store-io info` on the source)");
    }
    for page in &r.lost_pages {
        out!(
            "  lost: table page {page} (its regions are unknown, none copied; any region copied may be superseded by one)"
        );
    }
    for page in &r.stale_pages {
        out!(
            "  stale: table page {page} (its newest commit given up: a region it created is missing, one it released is back)"
        );
    }
    for (name, size) in &r.verified_up_to {
        out!(
            "  copied in full: {name} (its stale page recorded {size} bytes: only those are known to be its data)"
        );
    }
    for name in &r.superseded {
        out!("  possibly superseded: {name} (another region's newer data may cover its bytes)");
    }
    Ok(!(r.omitted.is_empty()
        && r.lost_pages.is_empty()
        && r.stale_pages.is_empty()
        && r.verified_up_to.is_empty()
        && r.superseded.is_empty()))
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn copy(_from: &str, _to: &str, _flags: CopyFlags) -> Result<bool, store_io::Error> {
    probe("").map(|()| false)
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn probe(_dir: &str) -> Result<(), store_io::Error> {
    Err(store_io::Error::Unsupported {
        what: store_io::Capability::Platform,
    })
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn info(_path: &str) -> Result<(), store_io::Error> {
    probe("")
}
