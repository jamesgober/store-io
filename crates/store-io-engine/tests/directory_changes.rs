//! Directory changes across handles and against poison: one change at a
//! time runs in a directory (another handle's change fails `Locked`, so the
//! temporary file is never shared), a handle poisoned while a replace runs
//! never commits it, and names the directory uses for itself are refused.

#![allow(clippy::unwrap_used, unused_results, unused_must_use)]

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use store_io_core::decide::Trust;
use store_io_core::error::{Error, NotWrittenCause};
use store_io_engine::Directory;
use store_io_sim::{CrashMode, FaultPlan, SimConfig, SimPlatform};

const DIR: &str = "/conf";
const LONG: Duration = Duration::from_secs(10);

fn open(p: &SimPlatform) -> Directory<SimPlatform> {
    Directory::open(p.clone(), Path::new(DIR), true, Trust::default()).unwrap()
}

/// While one handle is inside a replace, another handle's replace fails
/// `Locked` and writes nothing; the first one's bytes are what the
/// directory holds, before and after a power failure.
#[test]
fn test_a_second_handle_cannot_change_the_directory_meanwhile() {
    let p = SimPlatform::new(SimConfig::volatile(9004));
    let (a, b) = (Arc::new(open(&p)), open(&p));
    a.replace("m", b"old").unwrap();
    // Hold A inside its replace, at its first write.
    let (inside, on_inside) = mpsc::channel::<()>();
    let (go, wait_go) = mpsc::channel::<()>();
    let (inside, wait_go) = (Mutex::new(inside), Mutex::new(wait_go));
    let held = AtomicBool::new(false);
    p.with_world(|w| {
        w.set_submit_probe(Some(Arc::new(move || {
            if std::thread::current().name() == Some("A") && !held.swap(true, Ordering::SeqCst) {
                let _sent = inside.lock().unwrap().send(());
                let _released = wait_go.lock().unwrap().recv_timeout(LONG);
            }
        })));
    });
    let new_a = vec![0xAA; 5000];
    let ta = {
        let (a, v) = (Arc::clone(&a), new_a.clone());
        std::thread::Builder::new()
            .name("A".into())
            .spawn(move || a.replace("m", &v))
            .unwrap()
    };
    assert_eq!(on_inside.recv_timeout(LONG), Ok(()));
    let rb = b.replace("m", &[0xBB; 5000]);
    let _sent = go.send(());
    let ra = ta.join().unwrap();
    p.with_world(|w| w.set_submit_probe(None));
    assert!(matches!(rb, Err(Error::Locked)), "{rb:?}");
    assert!(ra.is_ok(), "{ra:?}");
    assert!(a.read("m").unwrap() == new_a);
    // Once A is done, B changes freely.
    b.replace("n", b"b").unwrap();
    p.with_world(|w| w.crash(&CrashMode::LoseAll));
    let d = Directory::open(p.clone(), Path::new(DIR), false, Trust::default()).unwrap();
    assert!(d.read("m").unwrap() == new_a);
    assert_eq!(d.read("n").unwrap(), b"b");
}

/// A handle poisoned while a replace runs (here a failed `sync` from
/// another caller) never commits it: the replace fails and the target keeps
/// its old bytes.
#[test]
fn test_a_replace_never_commits_on_a_handle_poisoned_meanwhile() {
    let mut cfg = SimConfig::volatile(9003);
    // Directory flushes: the open's own probe is the first, the v1
    // replace's the second; the third, a `sync` from inside the v2
    // replace, fails.
    cfg.faults = FaultPlan {
        fail_dir_sync: Some(3),
        ..FaultPlan::default()
    };
    let p = SimPlatform::new(cfg);
    let d = Arc::new(open(&p));
    d.replace("manifest", b"v1").unwrap();
    let weak = Arc::downgrade(&d);
    let fired = AtomicBool::new(false);
    let synced = Arc::new(Mutex::new(None));
    let s2 = Arc::clone(&synced);
    p.with_world(|w| {
        w.set_submit_probe(Some(Arc::new(move || {
            if !fired.swap(true, Ordering::SeqCst) {
                if let Some(d) = weak.upgrade() {
                    *s2.lock().unwrap() = Some(d.sync().is_err());
                }
            }
        })));
    });
    let r = d.replace("manifest", b"v2");
    p.with_world(|w| w.set_submit_probe(None));
    assert_eq!(*synced.lock().unwrap(), Some(true), "the sync did not fail");
    assert!(matches!(r, Err(Error::Poisoned { .. })), "{r:?}");
    assert_eq!(d.read("manifest").unwrap(), b"v1");
}

/// Names the directory uses for itself, and names too long to take the
/// temporary suffix, are refused before anything is written.
#[test]
fn test_names_the_directory_reserves_are_refused() {
    let p = SimPlatform::new(SimConfig::volatile(9005));
    let d = open(&p);
    let invalid = |r: Result<(), Error>| {
        matches!(
            r,
            Err(Error::NotWritten {
                cause: NotWrittenCause::InvalidName,
                ..
            })
        )
    };
    d.replace("x", b"x").unwrap();
    // Every spelling a case-insensitive file system, or Windows, would
    // resolve to the directory's own files.
    for reserved in [
        "x.sio-tmp",
        "X.SIO-TMP",
        "x.sio-tmp.",
        "x.sio-tmp ",
        ".sio-lock",
        ".SIO-LOCK",
        ".sio-lock.",
        // Unicode case folding: long s, Kelvin sign.
        ".\u{17F}io-lock",
        "x.\u{17F}io-tmp",
        ".sio-loc\u{212A}",
        // Windows short names, which can alias any long name.
        "SIO-LO~1",
        "sio-lo~1",
        "X~1.SIO",
        "93A5~1.SIO",
        "XF7FE~1.SIO",
        "AB~CDE~1",
        "A~BCDE~1.TXT",
        "~$DOCU~1.DOC",
        "abcdef~1",
        "X~123456",
        // Ignorable code points, which case-insensitive file systems drop
        // when comparing names (every range's ends are checked below).
        ".sio\u{200C}-lock",
        // Folded to `.` or `..`, or to nothing.
        ".\u{200B}",
        "..\u{200D}",
        "\u{FEFF}..",
        "\u{200C}",
        "x \u{200B}",
        "x.sio-tmp\u{FEFF}",
        ".sio\u{200B}-lock",
        ".sio\u{200F}-lock",
        ".sio\u{AD}-lock",
        ".sio\u{2060}-lock",
        ".sio\u{206F}-lock",
        ".sio\u{202A}-lock",
        ".sio\u{202E}-lock",
        "x.sio\u{FE0F}-tmp",
        "x.sio\u{FE00}-tmp",
        "x.sio-\u{E0001}tmp",
        // Trailing dots and spaces, and names of only those.
        "x.",
        "x ",
        "...",
        " ",
        // Windows devices.
        "NUL",
        "nul.txt",
        "nul .txt",
        "COM1",
        "COM0",
        "COM\u{B9}",
        "lpt\u{B3}",
        "lpt9.log",
        "CON",
        "AUX",
        "prn.log",
        "CONIN$",
        "CONOUT$",
    ] {
        assert!(invalid(d.replace(reserved, b"y")), "{reserved}");
        assert!(invalid(d.remove(reserved)), "{reserved}");
        assert!(
            matches!(
                d.read(reserved),
                Err(Error::NotWritten {
                    cause: NotWrittenCause::InvalidName,
                    ..
                })
            ),
            "{reserved}"
        );
    }
    // Names merely like them are objects like any other.
    // Just past each limit of the short-name shape, and near device names.
    for ok in [
        "a.sio-lock",
        "file~",
        "notes~backup.txt",
        "CONFIG",
        "COM",
        "com10",
        "x.y",
        "abcdefg~1",
        "abcdef~12",
        "abcdef~123",
        "a~b.txt",
        "ab~1x",
        // Ignorable code points elsewhere are kept: ordinary names.
        "mi\u{200C}x",
        "\u{1F469}\u{200D}\u{1F4BB}",
        ".sio\u{2010}-lock",
        "x~1.json",
        "x~1.a.b",
    ] {
        d.replace(ok, ok.as_bytes()).unwrap();
        assert_eq!(d.read(ok).unwrap(), ok.as_bytes(), "{ok}");
    }
    assert_eq!(
        d.read("x").unwrap(),
        b"x",
        "a reserved name touched an object"
    );
    let longest = "n".repeat(247);
    d.replace(&longest, b"ok").unwrap();
    assert_eq!(d.read(&longest).unwrap(), b"ok");
    let too_long = "n".repeat(248);
    assert!(invalid(d.replace(&too_long, b"no")));
    assert!(invalid(d.remove(&too_long)));
    assert!(matches!(
        d.read(&too_long),
        Err(Error::NotWritten {
            cause: NotWrittenCause::InvalidName,
            ..
        })
    ));
}

/// Unicode's Default_Ignorable_Code_Point ranges, as case-insensitive file
/// systems drop them when comparing names: both ends of every range, inside
/// a reserved name, are refused; the code points just outside are not
/// ignorable, so the same names with them are objects like any other.
#[test]
fn test_every_ignorable_range_is_dropped_before_comparing() {
    const RANGES: [(u32, u32); 17] = [
        (0xAD, 0xAD),
        (0x34F, 0x34F),
        (0x61C, 0x61C),
        (0x115F, 0x1160),
        (0x17B4, 0x17B5),
        (0x180B, 0x180F),
        (0x200B, 0x200F),
        (0x202A, 0x202E),
        (0x2060, 0x206F),
        (0x3164, 0x3164),
        (0xFE00, 0xFE0F),
        (0xFEFF, 0xFEFF),
        (0xFFA0, 0xFFA0),
        (0xFFF0, 0xFFF8),
        (0x1BCA0, 0x1BCA3),
        (0x1D173, 0x1D17A),
        (0xE0000, 0xE0FFF),
    ];
    let p = SimPlatform::new(SimConfig::volatile(9006));
    let d = open(&p);
    let ignorable = |c: u32| RANGES.iter().any(|&(lo, hi)| (lo..=hi).contains(&c));
    for (lo, hi) in RANGES {
        for c in [lo, hi] {
            let ch = char::from_u32(c).unwrap();
            for name in [format!(".sio{ch}-lock"), format!("x.sio-tmp{ch}")] {
                assert!(
                    matches!(
                        d.replace(&name, b"y"),
                        Err(Error::NotWritten {
                            cause: NotWrittenCause::InvalidName,
                            ..
                        })
                    ),
                    "U+{c:04X} not dropped"
                );
            }
        }
        for c in [lo - 1, hi + 1] {
            let Some(ch) = char::from_u32(c).filter(|_| !ignorable(c)) else {
                continue;
            };
            let name = format!(".sio{ch}-lock");
            d.replace(&name, b"ok").unwrap();
            assert_eq!(d.read(&name).unwrap(), b"ok", "U+{c:04X}");
        }
    }
}
