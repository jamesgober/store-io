//! dm-log-writes replay feasibility (v0.4 brief E11). Not library code.
//!
//! - `write <dir> <n> <dm-name>`: create a store in `<dir>`, provision an
//!   append region, mark `ready` in the log, then append `n` records of one
//!   block each with `append_durable`. After each receipt, insert the log mark
//!   `r<i>` (`dmsetup message <dm-name> 0 mark r<i>`), so every mark sits
//!   after the flush that made record `i` durable.
//! - `check <dir> <need>`: open the store read-only (on a replayed crash
//!   state) and count the leading intact records. Exit 0 when the count is at
//!   least `need` and every record after the intact prefix is either absent
//!   (never written) or the fill pattern, never a torn or foreign block.
//!   `need = 0` also accepts a store that does not exist yet.

use std::ops::ControlFlow;
use std::path::Path;
use std::process::{Command, ExitCode};

use store_io::{ScanItem, Store, StoreOptions, Trust};

const BLOCK: usize = 4096;

fn record(i: u64) -> Vec<u8> {
    let mut r = vec![0u8; BLOCK];
    r[..8].copy_from_slice(b"SIOREC01");
    r[8..16].copy_from_slice(&i.to_le_bytes());
    for (j, b) in r[16..].iter_mut().enumerate() {
        *b = (i as usize).wrapping_mul(131).wrapping_add(j) as u8;
    }
    r
}

fn mark(dm: &str, name: &str) {
    let ok = Command::new("dmsetup")
        .args(["message", dm, "0", "mark", name])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(ok, "dmsetup mark {name} failed");
}

fn opts(dir: &Path) -> StoreOptions {
    let probe = store_io::probe(dir).expect("probe");
    StoreOptions {
        trust: Trust {
            override_refusal: !matches!(probe.decision.durable_open, store_io::DurableOpen::Allowed),
            ..Trust::default()
        },
        ..StoreOptions::default()
    }
}

fn write(dir: &Path, n: u64, dm: &str) {
    let store = Store::create(dir.join("store"), opts(dir)).expect("create");
    println!("class {:?} label {:?}", store.report().decision.class, store.report().decision.label);
    let wal = store
        .provision_append_region("wal", (n + 16) * BLOCK as u64)
        .expect("provision");
    mark(dm, "ready");
    for i in 0..n {
        let (pos, receipt) = wal.append_durable(&record(i)).expect("append");
        assert_eq!(pos.offset(), i * BLOCK as u64);
        println!("RECEIPT {i} through {}", receipt.durable_through());
        mark(dm, &format!("r{i}"));
    }
}

fn check(dir: &Path, need: u64) -> ExitCode {
    let store = match Store::open_readonly(dir.join("store")) {
        Ok(s) => s,
        Err(e) if need == 0 => {
            println!("CHECK need=0 store absent or unopenable before ready: {e}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            println!("CHECK FAIL need={need} open: {e}");
            return ExitCode::FAILURE;
        }
    };
    let wal = match store.append_region("wal") {
        Ok(w) => w,
        Err(e) if need == 0 => {
            println!("CHECK need=0 region absent: {e}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            println!("CHECK FAIL need={need} region: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut data = Vec::new();
    let mut unreadable = 0u64;
    let r = wal.scan(0, |item| {
        match item {
            ScanItem::Data { bytes, .. } => data.extend_from_slice(bytes),
            ScanItem::Unreadable { start, end } => unreadable += end - start,
        }
        ControlFlow::Continue(())
    });
    if let Err(e) = r {
        println!("CHECK FAIL need={need} scan: {e}");
        return ExitCode::FAILURE;
    }
    let blocks = data.len() / BLOCK;
    let mut intact = 0u64;
    while (intact as usize) < blocks && data[intact as usize * BLOCK..(intact as usize + 1) * BLOCK] == record(intact)[..] {
        intact += 1;
    }
    // After the intact prefix: no block may be a record with the wrong index
    // or a torn record (magic present, content wrong).
    let mut bad_after = 0u64;
    for b in intact as usize..blocks {
        let blk = &data[b * BLOCK..(b + 1) * BLOCK];
        if blk[..8] == *b"SIOREC01" && blk != &record(b as u64)[..] {
            bad_after += 1;
        }
    }
    let ok = intact >= need && bad_after == 0 && unreadable == 0;
    println!(
        "CHECK {} need={need} intact={intact} torn_after_prefix={bad_after} unreadable={unreadable}",
        if ok { "OK" } else { "FAIL" }
    );
    if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

fn main() -> ExitCode {
    let a: Vec<String> = std::env::args().collect();
    match a.get(1).map(String::as_str) {
        Some("write") => {
            write(Path::new(&a[2]), a[3].parse().expect("n"), &a[4]);
            ExitCode::SUCCESS
        }
        Some("check") => check(Path::new(&a[2]), a[3].parse().expect("need")),
        _ => {
            eprintln!("usage: write <dir> <n> <dm-name> | check <dir> <need>");
            ExitCode::from(2)
        }
    }
}
