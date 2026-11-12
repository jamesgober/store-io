//! Read streams (R5.5) on the deterministic simulator: order and bytes,
//! ranges, the durable prefix, media errors, lending, options, retirement,
//! cancel and drop. The races against a recycle are `tests/stream_race.rs`.

// Test setup unwraps (a failure is the test failing) and ignores values a
// given test does not inspect.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_results,
    unused_must_use
)]

use std::ops::Range;
use std::path::{Path, PathBuf};

use store_io_core::IoClass;
use store_io_core::error::{CorruptionKind, Error, NotWrittenCause};
use store_io_engine::{
    AppendRegion, CONTAINER, Chunk, PageRegion, ReadStream, Store, StoreOptions, StreamOptions,
    SubmitterConfig, WriteMode,
};
use store_io_format::slot::FORMAT_2;
use store_io_sim::{FaultPlan, SimConfig, SimPlatform};

const DIR: &str = "/db";
const CHUNK: usize = 1 << 17;

fn opts() -> StoreOptions {
    StoreOptions {
        queues: 2,
        buffers_per_class: 64,
        max_io: CHUNK,
        ..StoreOptions::default()
    }
}

fn store(seed: u64, o: StoreOptions) -> (SimPlatform, Store<SimPlatform>) {
    let p = SimPlatform::new(SimConfig::volatile(seed));
    let s = Store::create(p.clone(), Path::new(DIR), o).unwrap();
    (p, s)
}

/// Stamped bytes: each 8-byte word holds its own offset, so a byte read
/// from the wrong place shows.
fn stamped(from: u64, len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len + 8);
    let mut at = from / 8 * 8;
    while v.len() < len + (from - from / 8 * 8) as usize {
        v.extend_from_slice(&at.to_le_bytes());
        at += 8;
    }
    let skip = (from % 8) as usize;
    v[skip..skip + len].to_vec()
}

/// A page region of `len` bytes, written stamped and made durable.
fn page_region(s: &Store<SimPlatform>, name: &str, len: u64) -> PageRegion<SimPlatform> {
    let r = s.provision_page_region(name, len).unwrap();
    let data = stamped(0, len as usize);
    for (i, c) in data.chunks(CHUNK).enumerate() {
        let pos = r.pos((i * CHUNK) as u64).unwrap();
        r.write_durable(pos, c).unwrap();
    }
    r
}

/// An append region with `len` stamped bytes appended and synced.
fn append_region(s: &Store<SimPlatform>, name: &str, len: u64) -> AppendRegion<SimPlatform> {
    let r = s.provision_append_region(name, 4 * len).unwrap();
    let t = r.append(&stamped(0, len as usize)).unwrap();
    r.sync_through(&t).unwrap();
    r
}

/// Every item of a stream, chunks checked contiguous: the bytes (bad
/// ranges filled with 0xEE) and the bad ranges.
fn drain(stream: ReadStream<SimPlatform>, from: u64) -> (Vec<u8>, Vec<Range<u64>>) {
    let mut data = Vec::new();
    let mut bad = Vec::new();
    let mut at = from;
    for item in stream {
        match item {
            Ok(c) => {
                assert_eq!(c.offset(), at, "a chunk out of order");
                data.extend_from_slice(c.bytes());
                at += c.bytes().len() as u64;
            }
            Err(Error::Corruption {
                kind: CorruptionKind::MediaError,
                ctx,
            }) => {
                let r = ctx.range.unwrap();
                assert_eq!(r.start, at, "a bad range out of order");
                bad.push(r.start..r.end);
                data.resize(data.len() + (r.end - r.start) as usize, 0xEE);
                at = r.end;
            }
            Err(e) => panic!("the stream ended with {e:?}"),
        }
    }
    (data, bad)
}

fn cause(e: &Error) -> Option<NotWrittenCause> {
    match e {
        Error::NotWritten { cause, .. } => Some(*cause),
        _ => None,
    }
}

#[test]
fn test_a_stream_reads_a_page_region_in_order() {
    let (_p, s) = store(1, opts());
    let len = 5 * CHUNK as u64 + 3 * 4096;
    let r = page_region(&s, "pages", len);
    for class in [IoClass::Background, IoClass::Read] {
        let stream = r
            .read_stream_with(0..len, StreamOptions::default().class(class))
            .unwrap();
        let (data, bad) = drain(stream, 0);
        assert!(bad.is_empty());
        assert!(data == stamped(0, len as usize), "{class:?}: the bytes");
    }
}

#[test]
fn test_a_stream_reads_any_range_and_only_its_bytes() {
    let (_p, s) = store(2, opts());
    let len = 3 * CHUNK as u64;
    let r = page_region(&s, "pages", len);
    for range in [100..200_000u64, 4096..8192, 1..2, 0..len, len - 7..len] {
        let (data, _) = drain(r.read_stream(range.clone()).unwrap(), range.start);
        assert!(
            data == stamped(range.start, (range.end - range.start) as usize),
            "{range:?}"
        );
    }
    // Empty: no item.
    assert!(r.read_stream(10..10).unwrap().next().is_none());
}

#[test]
fn test_a_stream_refuses_a_range_out_of_bounds() {
    let (_p, s) = store(3, opts());
    let r = page_region(&s, "pages", 64 << 10);
    // A reversed range, built at run time (clippy refuses a literal one).
    let ten = std::hint::black_box(10u64);
    #[allow(clippy::reversed_empty_ranges)]
    for range in [ten..5u64, 0..(64 << 10) + 1] {
        let e = r.read_stream(range.clone()).unwrap_err();
        assert_eq!(cause(&e), Some(NotWrittenCause::OutOfBounds), "{range:?}");
    }
}

#[test]
fn test_an_append_region_streams_its_durable_prefix_as_of_the_call() {
    let (_p, s) = store(4, opts());
    let r = append_region(&s, "log", 200_000);
    let durable = r.durable_through();
    // An append not synced: past the prefix, never streamed.
    let _t = r.append(&[0xAB; 50_000]).unwrap();
    assert_eq!(r.durable_through(), durable);
    let (data, bad) = drain(r.read_stream(0..r.len()).unwrap(), 0);
    assert!(bad.is_empty());
    assert_eq!(data.len() as u64, durable);
    assert!(data[..200_000] == stamped(0, 200_000)[..]);
    // A range wholly past the prefix: empty.
    assert!(
        r.read_stream(durable..durable + 4096)
            .unwrap()
            .next()
            .is_none()
    );
}

#[test]
fn test_an_unpositioned_append_region_is_refused() {
    let (p, s) = store(5, opts());
    let _r = append_region(&s, "log", 64 << 10);
    drop(_r);
    drop(s);
    let s = Store::open(p, Path::new(DIR), opts()).unwrap();
    let r = s.append_region("log").unwrap();
    let e = r.read_stream(0..4096).unwrap_err();
    assert_eq!(cause(&e), Some(NotWrittenCause::NotPositioned));
    let tail = r.len();
    let _ = r.resume_at(tail.min(64 << 10));
    let (data, _) = drain(r.read_stream(0..4096).unwrap(), 0);
    assert!(data == stamped(0, 4096));
}

/// Two bad ranges inside one piece, and a third in another: every good
/// byte comes back at its place, each bad range exactly, the stream
/// going on after them.
#[test]
fn test_media_errors_are_exact_items_and_the_stream_goes_on() {
    for lend in [None, Some(1usize)] {
        let (p, s) = store(6, opts());
        let len = 4 * CHUNK as u64;
        let r = page_region(&s, "pages", len);
        let base = {
            let first = stamped(CHUNK as u64, 64);
            p.with_world(|w| {
                let media = w.media_of(&PathBuf::from(DIR), CONTAINER).unwrap();
                media.windows(64).position(|win| win == &first[..]).unwrap() as u64
            }) - CHUNK as u64
        };
        let bads = [
            CHUNK as u64 + 8192..CHUNK as u64 + 12288,
            CHUNK as u64 + 40960..CHUNK as u64 + 45056,
            3 * CHUNK as u64 + 4096..3 * CHUNK as u64 + 8192,
        ];
        p.with_world(|w| {
            let file = w.file_id(&PathBuf::from(DIR), CONTAINER).unwrap();
            *w.faults_mut() = FaultPlan {
                bad_ranges: bads
                    .iter()
                    .map(|b| (file, base + b.start, base + b.end))
                    .collect(),
                ..FaultPlan::default()
            };
        });
        let o = match lend {
            None => StreamOptions::default(),
            Some(l) => StreamOptions::default().depth(2).lend(l),
        };
        let mut stream = r.read_stream_with(0..len, o).unwrap();
        // With `lend` 1, each chunk is dropped before the next call: a
        // buffer a run is still to be copied from is never reused.
        let mut data = Vec::new();
        let mut bad = Vec::new();
        for item in stream.by_ref() {
            match item {
                Ok(c) => {
                    assert_eq!(c.offset(), data.len() as u64);
                    data.extend_from_slice(c.bytes());
                }
                Err(Error::Corruption { ctx, .. }) => {
                    let rg = ctx.range.unwrap();
                    assert_eq!(rg.start, data.len() as u64);
                    bad.push(rg.start..rg.end);
                    data.resize(rg.end as usize, 0xEE);
                }
                Err(e) => panic!("{e:?}"),
            }
        }
        assert_eq!(bad, bads.to_vec(), "lend {lend:?}");
        let mut want = stamped(0, len as usize);
        for b in &bads {
            want[b.start as usize..b.end as usize].fill(0xEE);
        }
        assert!(
            data == want,
            "lend {lend:?}: the good bytes at their places"
        );
    }
}

#[test]
fn test_a_next_with_every_lent_chunk_held_is_refused_and_the_stream_goes_on() {
    let (_p, s) = store(7, opts());
    let len = 6 * CHUNK as u64;
    let r = page_region(&s, "pages", len);
    let mut stream = r
        .read_stream_with(0..len, StreamOptions::default().depth(2).lend(2))
        .unwrap();
    let a = stream.next().unwrap().unwrap();
    let b = stream.next().unwrap().unwrap();
    let e = stream.next().unwrap().unwrap_err();
    assert_eq!(cause(&e), Some(NotWrittenCause::TooManyInFlight));
    drop(a);
    let c = stream.next().unwrap().unwrap();
    assert_eq!(c.offset(), b.offset() + b.bytes().len() as u64);
    drop((b, c));
    let (rest, bad) = drain(stream, 3 * CHUNK as u64);
    assert!(bad.is_empty());
    assert!(rest == stamped(3 * CHUNK as u64, 3 * CHUNK));
}

#[test]
fn test_stream_options_are_validated_and_the_defaults_fit_the_pool() {
    let small = StoreOptions {
        buffers_per_class: 16,
        ..opts()
    };
    let (_p, s) = store(8, small);
    let len = 8 * CHUNK as u64;
    let r = page_region(&s, "pages", len);
    for o in [
        StreamOptions::default().depth(0),
        StreamOptions::default().lend(0),
        StreamOptions::default().class(IoClass::Commit),
        // Over the cap: half of 16 buffers.
        StreamOptions::default().depth(4).lend(4),
        // A sum that would overflow.
        StreamOptions::default().depth(4).lend(usize::MAX),
    ] {
        let e = r.read_stream_with(0..len, o).unwrap_err();
        assert_eq!(cause(&e), Some(NotWrittenCause::InvalidOptions), "{o:?}");
    }
    // Over the cap on an empty range too.
    let o = StreamOptions::default().depth(4).lend(4);
    let e = r.read_stream_with(10..10, o).unwrap_err();
    assert_eq!(cause(&e), Some(NotWrittenCause::InvalidOptions));
    // The defaults on a 16-buffer class: cap 8, so depth 3 and lend 3.
    let mut stream = r.read_stream(0..len).unwrap();
    // 7 of the cap's 8 held: another default stream finds fewer than 3.
    let e = r.read_stream(0..len).unwrap_err();
    assert_eq!(cause(&e), Some(NotWrittenCause::PoolExhausted));
    let held: Vec<Chunk> = (0..3).map(|_| stream.next().unwrap().unwrap()).collect();
    let e = stream.next().unwrap().unwrap_err();
    assert_eq!(cause(&e), Some(NotWrittenCause::TooManyInFlight));
    drop(held);
    assert!(stream.next().unwrap().is_ok());
}

#[test]
fn test_a_recycle_or_release_on_the_same_thread_ends_the_stream() {
    let (_p, s) = store(9, opts());
    let len = 6 * CHUNK as u64;
    let r = page_region(&s, "pages", len);
    let mut stream = r.read_stream(0..len).unwrap();
    let first = stream.next().unwrap().unwrap();
    assert!(first.bytes() == &stamped(0, first.bytes().len())[..]);
    drop(first);
    // The stream holds no pass: the recycle does not wait on it.
    let r2 = r.recycle().unwrap();
    let e = stream.next().unwrap().unwrap_err();
    assert_eq!(cause(&e), Some(NotWrittenCause::StaleGeneration));
    assert!(stream.next().is_none());
    let mut stream = r2.read_stream(0..r2.len()).unwrap();
    assert!(stream.next().unwrap().is_ok());
    r2.release().unwrap();
    let e = stream.next().unwrap().unwrap_err();
    assert_eq!(cause(&e), Some(NotWrittenCause::NotReady));
}

#[test]
fn test_a_chunk_dropped_on_a_worker_after_its_stream_goes_back_to_the_pool() {
    let small = StoreOptions {
        buffers_per_class: 16,
        ..opts()
    };
    let (_p, s) = store(10, small);
    let len = 8 * CHUNK as u64;
    let r = page_region(&s, "pages", len);
    // Exactly the cap (half of 16): depth 3 + lend 4 + the scratch.
    let o = StreamOptions::default().depth(3).lend(4);
    let mut stream = r.read_stream_with(0..len, o).unwrap();
    let c = stream.next().unwrap().unwrap();
    let held = stream.next().unwrap().unwrap();
    drop(stream);
    let worker = std::thread::spawn(move || drop(c));
    worker.join().unwrap();
    // `c`'s buffer is back in the pool though `held` keeps the stream's
    // list alive: a stream taking all of the cap but `held`'s opens.
    let all_but_one = StreamOptions::default().depth(3).lend(3);
    let again = r.read_stream_with(0..len, all_but_one).unwrap();
    drop((again, held));
    let again = r.read_stream_with(0..len, o).unwrap();
    drop(again);
}

#[test]
fn test_streams_leave_the_other_half_of_a_class_for_writes() {
    let small = StoreOptions {
        buffers_per_class: 16,
        ..opts()
    };
    let (_p, s) = store(11, small);
    let len = 8 * CHUNK as u64;
    let r = page_region(&s, "pages", len);
    let mut streams = Vec::new();
    loop {
        match r.read_stream_with(0..len, StreamOptions::default().depth(1).lend(1)) {
            Ok(st) => streams.push(st),
            Err(e) => {
                assert_eq!(cause(&e), Some(NotWrittenCause::PoolExhausted));
                break;
            }
        }
        assert!(streams.len() <= 8, "streams past the cap");
    }
    // Each takes 3 buffers (depth 1, lend 1, the scratch) under a cap of
    // 8 (half of 16): two of them, never more.
    assert_eq!(streams.len(), 2, "streams past the cap");
    // Half the class held by streams: a blocking write of the class still
    // gets its buffer.
    let pos = r.pos(0).unwrap();
    r.write_durable(pos, &stamped(0, CHUNK)).unwrap();
    drop(streams);
}

#[test]
fn test_cancel_waits_and_a_failed_wait_is_its_error() {
    let (p, s) = store(12, opts());
    let len = 32 * CHUNK as u64;
    let r = page_region(&s, "pages", len);
    let mut stream = r.read_stream(0..len).unwrap();
    let _first = stream.next().unwrap().unwrap();
    stream.cancel().unwrap();
    // Unpaced (the writes above made commits active, and a paced stream
    // keeps one read in flight, which `next()` itself waits for).
    let read = StreamOptions::default().class(IoClass::Read);
    let mut stream = r.read_stream_with(0..len, read).unwrap();
    let first = stream.next().unwrap().unwrap();
    drop(first);
    // The next call's new read stays in flight (its reap held back).
    p.with_world(|w| w.hold_reaps(1));
    let second = stream.next().unwrap().unwrap();
    drop(second);
    p.with_world(|w| {
        let next = w.waits() + 1;
        w.faults_mut().fail_wait = Some((next, 5));
    });
    let before = p.with_world(|w| w.waits());
    assert!(matches!(stream.cancel(), Err(Error::Io { .. })));
    // The failed wait is the only one: the drop never waits again.
    assert_eq!(p.with_world(|w| w.waits()), before + 1);
}

#[test]
fn test_a_submitter_and_a_stream_share_a_thread_and_the_store() {
    let (_p, s) = store(13, opts());
    let len = 4 * CHUNK as u64;
    let r = page_region(&s, "pages", len);
    let log = s.provision_append_region("log", 1 << 20).unwrap();
    let mut sub = s.submitter(SubmitterConfig::default()).unwrap();
    let stream = r.read_stream(0..len).unwrap();
    let mut got = Vec::new();
    for item in stream {
        let c = item.unwrap();
        got.extend_from_slice(c.bytes());
        // The caller uses the store between chunks.
        let buf = sub.take_buf(4096).unwrap();
        let _pos = sub.append(&log, buf, WriteMode::Durable, 1).unwrap();
        while sub.in_flight() > 0 {
            let _done: Vec<_> = sub.wait(1).unwrap().collect();
        }
        let pos = r.pos(len - 4096).unwrap();
        r.write(pos, &stamped(len - 4096, 4096)).unwrap();
    }
    assert!(got == stamped(0, len as usize));
}

/// A region grown past its capacity onto an extent that is not adjacent
/// (another region after it): no read crosses from one extent to the next,
/// and every byte comes back from its own run.
#[test]
fn test_a_stream_cuts_its_reads_where_an_extent_ends() {
    const B: u64 = 4096;
    let o = StoreOptions {
        container_format: FORMAT_2,
        region_capacity: 400,
        ..opts()
    };
    let (_p, s) = store(14, o);
    let cap = 16 * B;
    let r = s.provision_page_region("p", cap).unwrap();
    let _q = s.provision_page_region("q", cap).unwrap();
    let len = cap + 8 * B;
    assert_eq!(r.grow(len).unwrap(), len);
    let data = stamped(0, len as usize);
    for at in (0..len).step_by(B as usize) {
        r.write_durable(r.pos(at).unwrap(), &data[at as usize..(at + B) as usize])
            .unwrap();
    }
    for class in [IoClass::Background, IoClass::Read] {
        let o = StreamOptions::default().class(class);
        let (got, bad) = drain(r.read_stream_with(0..len, o).unwrap(), 0);
        assert!(bad.is_empty());
        assert!(got == data, "{class:?}: the bytes of both runs");
    }
}

/// A stream's read held in flight shows in `oldest_outstanding` (its lane
/// mark registered), and is gone once `next()` collects it.
#[test]
fn test_a_stream_read_in_flight_is_outstanding() {
    let (p, s) = store(15, opts());
    let len = 32 * CHUNK as u64;
    let r = page_region(&s, "pages", len);
    let read = StreamOptions::default().class(IoClass::Read);
    let mut stream = r.read_stream_with(0..len, read).unwrap();
    let first = stream.next().unwrap().unwrap();
    drop(first);
    p.with_world(|w| w.hold_reaps(1));
    let second = stream.next().unwrap().unwrap();
    drop(second);
    assert!(s.oldest_outstanding().is_some(), "a stream read in flight");
    stream.cancel().unwrap();
    assert!(s.oldest_outstanding().is_none());
}
