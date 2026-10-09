//! # store-io-harness
//!
//! Time-boxed performance workloads for store-io on a real device, each
//! measured against the raw platform primitive on the same filesystem and
//! against fsys 1.1.3 where fsys has a comparable operation. Writes a
//! Markdown and a JSON report to `results/<date>-<os>-<device>.{md,json}`.
//!
//! Modules: [`cli`] (arguments), [`env`] (machine facts), [`sio`] (store-io
//! setup and the labelled override), [`raw`] (the raw primitives),
//! [`fsys_cmp`] (fsys configurations), [`data`] (self-identifying payloads),
//! [`stats`] (percentiles), [`report`] / [`json`] (output), [`diag`]
//! (one-off diagnostics), [`watchdog`] (hang guard), and one module per
//! workload under [`workloads`].

mod aligned;
mod cli;
mod data;
mod diag;
mod env;
mod fsys_cmp;
mod json;
mod raw;
mod report;
mod sio;
mod stats;
mod watchdog;
mod workloads;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use cli::{Args, Command};
use report::{Header, Section};
use workloads::Ctx;

/// A workload: its command, its entry point and its point count (for the
/// hang limit and the time estimate).
struct Workload {
    command: Command,
    run: fn(&Ctx) -> Section,
    points: usize,
}

const WORKLOADS: [Workload; 5] = [
    Workload {
        command: Command::Lone,
        run: workloads::lone::run,
        points: 7,
    },
    Workload {
        command: Command::Concurrent,
        run: workloads::concurrent::run,
        points: 28,
    },
    Workload {
        command: Command::CallerBatch,
        run: workloads::caller_batch::run,
        points: 72,
    },
    Workload {
        command: Command::PageBatch,
        run: workloads::page_batch::run,
        points: 16,
    },
    Workload {
        command: Command::Sequential,
        run: workloads::sequential::run,
        points: 11,
    },
];

/// What the hang handler needs to write a partial report.
struct Partial {
    header: Option<Header>,
    sections: Vec<Section>,
    paths: Option<(PathBuf, PathBuf)>,
    started: Instant,
}

fn main() -> ExitCode {
    let args = match cli::parse(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}\n\n{}", cli::USAGE);
            return ExitCode::from(2);
        }
    };
    if args.command == Command::Help {
        print!("{}", cli::USAGE);
        return ExitCode::SUCCESS;
    }
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn write_reports(p: &Partial, incomplete: Option<&str>) -> Result<(), String> {
    let (Some(h), Some((md, js))) = (&p.header, &p.paths) else {
        return Err("no header yet".to_owned());
    };
    let total = p.started.elapsed().as_secs_f64();
    std::fs::write(md, report::markdown(h, &p.sections, total, incomplete))
        .map_err(|e| format!("writing {}: {e}", md.display()))?;
    std::fs::write(js, report::json(h, &p.sections, total, incomplete))
        .map_err(|e| format!("writing {}: {e}", js.display()))
}

fn run(args: &Args) -> Result<(), String> {
    let started = Instant::now();
    let base = args
        .dir
        .join(format!("store-io-harness-{}", std::process::id()));
    std::fs::create_dir_all(&base).map_err(|e| format!("creating {}: {e}", base.display()))?;
    let result = run_in(args, &base, started);
    if let Some(e) = workloads::remove(&base) {
        eprintln!("warning: {e}");
    }
    result
}

fn run_in(args: &Args, base: &Path, started: Instant) -> Result<(), String> {
    eprintln!("probing the device under {} ...", base.display());
    let probe_dir = base.join("probe");
    let setup = sio::probe(&probe_dir)?;
    if let Some(e) = workloads::remove(&probe_dir) {
        eprintln!("warning: {e}");
    }
    eprintln!("{}", setup.report_text);
    if let Some(r) = &setup.refusal {
        eprintln!(
            "\nOVERRIDE: default-trust create was refused ({r}); continuing with Trust::override_refusal (labelled)."
        );
    }
    if args.command == Command::Probe {
        eprintln!("\nevidence:\n{}", setup.evidence_dump);
        return Ok(());
    }
    let fsys = fsys::builder()
        .build()
        .map_err(|e| format!("fsys::builder().build(): {e}"))?;
    let env = env::collect(base, &setup.fs_kind, args.commit.as_deref());
    let device = env::short_device_name(setup.device_model.as_deref());
    let ctx = Ctx::new(base.to_path_buf(), args.secs, setup.opts.clone(), fsys);
    let header = Header {
        device: device.clone(),
        backend: sio::BACKEND.to_owned(),
        store_report: setup.report_text.clone(),
        evidence: setup.evidence_dump.clone(),
        options: format!("{:#?}", setup.opts),
        class: format!(
            "{} (receipt label {}, durable open {}, block size {} B)",
            setup.class, setup.label, setup.durable_open, setup.block_size
        ),
        refusal: setup.refusal.clone(),
        fsys: fsys_cmp::describe(&ctx.fsys),
        secs: ctx.secs,
        warmup: ctx.warmup,
        command: args.command.name().to_owned(),
        diagnostics: Vec::new(),
        raw: vec![raw::PRIMITIVE.to_owned(), raw::READ_PRIMITIVE.to_owned()],
        env: env.clone(),
    };
    std::fs::create_dir_all(&args.out)
        .map_err(|e| format!("creating {}: {e}", args.out.display()))?;
    let stem = if args.command == Command::All {
        format!("{}-{}-{}", env.date, env.os, device)
    } else {
        format!("{}-{}-{}-{}", env.date, env.os, device, args.command.name())
    };
    let paths = (
        args.out.join(format!("{stem}.md")),
        args.out.join(format!("{stem}.json")),
    );
    let partial = Arc::new(Mutex::new(Partial {
        header: Some(header),
        sections: Vec::new(),
        paths: Some(paths.clone()),
        started,
    }));
    let hang_state = Arc::clone(&partial);
    let dog = watchdog::Watchdog::start(move |label, elapsed| {
        let why = format!(
            "the `{label}` workload made no progress past its limit ({:.0} s elapsed); the process was stopped by the harness watchdog. This is a hang in the code under test or the device, recorded as an anomaly.",
            elapsed.as_secs_f64()
        );
        eprintln!("HANG: {why}");
        // A workload thread may hold no lock here; the partial state is only
        // locked between workloads, so this cannot deadlock with them.
        let p = hang_state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Err(e) = write_reports(&p, Some(&why)) {
            eprintln!("could not write the partial report: {e}");
        }
    });

    eprintln!("diagnostics ...");
    let diags = diag::run(&ctx);
    if let Some(h) = partial
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .header
        .as_mut()
    {
        h.diagnostics = diags;
    }

    let selected: Vec<&Workload> = WORKLOADS
        .iter()
        .filter(|w| args.command == Command::All || w.command == args.command)
        .collect();
    let est: f64 = selected
        .iter()
        .map(|w| w.points as f64 * ctx.point_secs())
        .sum();
    eprintln!(
        "running {} workload(s), {} points, at most ~{:.0} s of measurement plus setup",
        selected.len(),
        selected.iter().map(|w| w.points).sum::<usize>(),
        est
    );
    for w in selected {
        let name = w.command.name();
        let limit = Duration::from_secs_f64(w.points as f64 * ctx.point_secs() * 4.0 + 300.0);
        eprintln!("[{name}] running ...");
        dog.enter(name, limit);
        let t = Instant::now();
        let mut sec = (w.run)(&ctx);
        sec.elapsed_s = t.elapsed().as_secs_f64();
        dog.leave();
        eprintln!(
            "[{name}] done in {:.1} s, {} anomalies",
            sec.elapsed_s,
            sec.anomalies.len()
        );
        let mut p = partial.lock().unwrap_or_else(PoisonError::into_inner);
        p.sections.push(sec);
        // Keep a partial report on disk after every workload.
        write_reports(&p, Some("run in progress"))?;
    }
    let p = partial.lock().unwrap_or_else(PoisonError::into_inner);
    write_reports(&p, None)?;
    eprintln!(
        "wrote {} and {} ({:.1} s total)",
        paths.0.display(),
        paths.1.display(),
        started.elapsed().as_secs_f64()
    );
    Ok(())
}
