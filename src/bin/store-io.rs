//! The `store-io` command-line tool.
//!
//! ```text
//! store-io probe <dir>    what store-io learns about the device under <dir>
//! store-io info <store>   a store's device report, regions and space (read-only)
//! ```

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::process::ExitCode;

const USAGE: &str = "usage:
  store-io probe <dir>    show the evidence and durability class of <dir>'s device
  store-io info <store>   show a store's report, regions and space (read-only)";

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
        ["--version" | "-V"] => {
            println!("store-io {}", env!("CARGO_PKG_VERSION"));
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
            ExitCode::FAILURE
        }
    }
}

#[cfg(any(windows, target_os = "linux"))]
fn probe(dir: &str) -> Result<(), store_io::Error> {
    let p = store_io::probe(dir)?;
    let d = &p.decision;
    println!("class:    {}", d.class);
    println!("label:    {}", d.label);
    println!("open:     {:?}", d.durable_open);
    for r in d.reasons.iter() {
        println!("reason:   {r}");
    }
    for m in d.missing.iter() {
        println!("missing:  {m}");
    }
    println!();
    println!("{:#?}", p.evidence);
    Ok(())
}

#[cfg(any(windows, target_os = "linux"))]
fn info(path: &str) -> Result<(), store_io::Error> {
    let store = store_io::Store::open_readonly(path)?;
    println!("{}", store.report());
    println!();
    println!("regions:");
    for (name, kind) in store.regions() {
        println!("  {name:<24} {kind:?}");
    }
    let s = store.space();
    println!();
    println!(
        "space: container {} B, regions {} B, released {} B, reserved {} B",
        s.container, s.regions, s.released, s.reserved
    );
    for t in &s.tags {
        println!(
            "  tag {:>10}: logical {} B, physical {} B, reserved {} B, cap {:?}",
            t.tag, t.logical, t.physical, t.reserved, t.cap
        );
    }
    Ok(())
}

#[cfg(not(any(windows, target_os = "linux")))]
fn probe(_dir: &str) -> Result<(), store_io::Error> {
    Err(store_io::Error::Unsupported {
        what: store_io::Capability::Platform,
    })
}

#[cfg(not(any(windows, target_os = "linux")))]
fn info(_path: &str) -> Result<(), store_io::Error> {
    probe("")
}
