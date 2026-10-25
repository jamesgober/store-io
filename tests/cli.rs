//! The `store-io` command-line tool end to end on the real file system:
//! `store-io copy` (v0.5 E5 C2) copies a store, refuses a target holding
//! one, and `store-io info` reads the copy.

#![cfg(all(
    feature = "cli",
    any(windows, target_os = "linux", target_os = "macos")
))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_results,
    unused_must_use
)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use store_io::{Store, StoreOptions, Trust};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        // As tests/native.rs: $STORE_IO_TEST_DIR, else $HOME on Linux, else
        // %TEMP%.
        let base = std::env::var_os("STORE_IO_TEST_DIR")
            .or_else(|| std::env::var_os("HOME").filter(|_| cfg!(target_os = "linux")))
            .map_or_else(std::env::temp_dir, PathBuf::from);
        let d = base.join(format!(
            ".store-io-cli-{}-{}-{tag}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        Self(d)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _cleanup = std::fs::remove_dir_all(&self.0);
    }
}

fn cli(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_store-io"))
        .args(args)
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn test_copy_copies_a_store_and_refuses_a_second_time() {
    let t = TempDir::new("copy");
    let probe = store_io::probe(t.path()).unwrap();
    let refused = !matches!(probe.decision.durable_open, store_io::DurableOpen::Allowed);
    let opts = StoreOptions {
        trust: Trust {
            override_refusal: refused,
            ..Trust::default()
        },
        ..StoreOptions::default()
    };
    let (old, new) = (t.path().join("old"), t.path().join("new"));
    {
        let s = Store::create(&old, opts.clone()).unwrap();
        let wal = s.provision_append_region("wal", 1 << 20).unwrap();
        wal.append_durable(b"order 1001").unwrap();
        let pages = s.provision_page_region("pages", 1 << 20).unwrap();
        pages
            .write_durable(pages.pos(4096).unwrap(), &[7u8; 4096])
            .unwrap();
        s.provision_slot("manifest")
            .unwrap()
            .commit(b"the manifest")
            .unwrap();
    }
    let (old_s, new_s) = (old.to_str().unwrap(), new.to_str().unwrap());
    if refused {
        // Without the override: refused, no store made, the flag named.
        let out = Command::new(env!("CARGO_BIN_EXE_store-io"))
            .args(["copy", old_s, new_s])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&out.stderr).contains("--override-refusal"));
        assert!(!new.join("store.sio").exists());
    }
    let mut args = vec!["copy"];
    if refused {
        args.push("--override-refusal");
    }
    args.extend([old_s, new_s]);
    let (ok, stdout, stderr) = cli(&args);
    assert!(ok, "{stdout}{stderr}");
    assert!(stdout.starts_with("copied 3 regions"), "{stdout}");

    let s = Store::open_readonly(&new).unwrap();
    assert_eq!(s.report().format, 2);
    let mut regions = s.regions();
    regions.sort_by(|a, b| a.0.cmp(&b.0));
    let names: Vec<_> = regions.iter().map(|r| r.0.as_str()).collect();
    assert_eq!(names, ["manifest", "pages", "wal"]);
    let mut b = vec![0u8; 4096];
    let pages = s.page_region("pages").unwrap();
    pages.read(pages.pos(4096).unwrap(), &mut b).unwrap();
    assert!(b == [7u8; 4096]);
    let mut w = vec![0u8; 4096];
    s.append_region("wal").unwrap().read(0, &mut w).unwrap();
    assert_eq!(&w[..10], b"order 1001");
    assert_eq!(s.slot("manifest").unwrap().read().unwrap(), b"the manifest");
    drop((pages, s));

    // `info` reads the copy; a second copy into it is refused.
    let (ok, stdout, _) = cli(&["info", new_s]);
    assert!(ok && stdout.contains("manifest"), "{stdout}");
    let out = Command::new(env!("CARGO_BIN_EXE_store-io"))
        .args(&args)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("already exists"), "{stderr}");
    // `--accept-stale-pages` (before or after the override) on a healthy
    // store: an ordinary copy, nothing listed.
    let third = t.path().join("third");
    let third_s = third.to_str().unwrap();
    let mut args = vec!["copy", "--accept-stale-pages"];
    if refused {
        args.insert(1, "--override-refusal");
    }
    args.extend([old_s, third_s]);
    let (ok, stdout, stderr) = cli(&args);
    assert!(ok, "{stdout}{stderr}");
    assert!(stdout.starts_with("copied 3 regions"), "{stdout}");
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    // Wrong arguments, an option taken for a path among them, an unknown
    // or repeated option: the usage, exit status 2.
    for bad in [
        vec!["copy", old_s],
        vec!["copy", "--override-refusal", new_s],
        vec!["copy", old_s, new_s, "--override-refusal"],
        vec!["copy", "--bogus", old_s, new_s],
        vec![
            "copy",
            "--accept-stale-pages",
            "--accept-stale-pages",
            old_s,
            new_s,
        ],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_store-io"))
            .args(&bad)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{bad:?}");
    }
}
