# store-io &mdash; Engineering Directives

> Engineering standards and the definition of done for this project. Read alongside `REPS.md` (root, authoritative) and `dev/ROADMAP.md` (current phase). If anything here conflicts with `REPS.md`, `REPS.md` wins. A conflict between two requirements is never resolved silently: it is raised with the maintainer and recorded as a numbered decision.

---

## 0. Philosophy

store-io is the only code in its stack allowed to touch files and storage devices. Its first consumer is a database commit layer whose single promise is that an acknowledged write survives. That promise is exactly as strong as this library. A wrong barrier, a silent fallback, or a "helpful" repair in an I/O library loses acknowledged data, and has done so before.

So the bar is not "works", "reaches the goal" or "meets expectations". Those are failures here. The bar is: every durability claim is proven on real media, every performance claim is measured against the best tool on the same box and beaten or matched within a stated budget, and every line can be defended in review. Plan the whole path first, then build one verified step at a time. Explore, experiment and measure before choosing a mechanism; never choose one on assumption.

---

## 1. What this is

store-io moves caller bytes between memory and media, and proves when they are durable. It owns:

- **Device evidence.** Probing each path and device, and classing it as power-safe, flush-required, unverified or unsafe from evidence the device and OS actually report.
- **Buffers.** Aligned, pooled, optionally registered and NUMA-local I/O buffers that move into an I/O and come back in its completion.
- **Submission engines.** Positioned direct writes and reads on every supported platform (Linux io_uring and synchronous fallback, Windows IOCP, macOS threads), with one completion model.
- **Barriers and receipts.** The cheapest correct durability primitive per platform and class, and unforgeable receipts that prove which bytes are durable.
- **Space.** Provisioning, reservations, recycle, release and clone, each run only when the caller asks.
- **Small-object commits.** A/B slots, atomic replace and directory sync.
- **The recovery read path.** Ownership locks, region listing and raw ordered scans with exact error ranges.
- **Proof.** A deterministic simulated backend, a public conformance crate and a standalone harness with per-stage instrumentation.

It deliberately does NOT own framing, payload checksums, log formats, commit rules, recovery decisions, encryption, compression, quota policy, replication or retry policy. Those belong to callers. store-io never parses, validates, truncates, repairs or reclaims caller data on its own.

---

## 2. Engineering law (non-negotiable)

- **Durability first.** A receipt exists only if every covered byte is durable at its class. A barrier is never a silent no-op. A failed flush is never retried and never followed by a reported success. Fallbacks either fail the operation or relabel the receipt and emit an event; nothing downgrades silently.
- **Performance is a gate, not a goal.** Every hot-path claim is measured against the matching reference tool (fio, `t/io_uring`, a raw-syscall microbenchmark) on the same box, kernel, filesystem and flags, and gated by the targets in `dev/ROADMAP.md`. Zero allocations, zero locks and zero timed waits on the submit and reap paths.
- **Beat the predecessor.** For every operation fsys also performs, store-io must be at least 20% faster or cheaper per operation on the same box, or the gap must be shown to be the device floor (for example a consumer drive's flush), with the measurement. Regressions against fsys never merge.
- **Labelled numbers only.** Every number in code comments, docs, benchmarks and release notes is labelled `[published]` (with its source), `[measured]` (with device, durability class, OS and kernel, filesystem, flags and fsync status), `[derived]` (with the arithmetic), `[estimate]` (with the assumptions) or `[unverified]`. An in-memory or no-fsync number is never quoted without the durable number beside it.
- **Evidence over claims.** OS cache toggles and vendor tables are never evidence of power safety. Only the device's own report or an exact-SKU certificate backed by a power-cut rig log promotes a device.
- **No hidden behaviour.** No environment variable, global flag or implicit configuration changes durability or method selection. No timers, no polling threads, no group-commit windows, no idle wakeups.
- **Architecture.** SOLID, KISS, YAGNI. A facade crate over small crates with dependencies pointing strictly downward. Each backend sits behind a feature flag. The core depends on no async runtime; async adapters live in their own crates.
- **Error handling.** Structured errors carry the raw OS error, the operation, the volume, the region and the byte range. "Not written" and "durability unknown" are different kinds and are never conflated. No `unwrap`, `expect`, `panic!`, `todo!`, `unimplemented!` or `unreachable!` in shipping code. Every blocking point has a bound; every queue is bounded with back-pressure.
- **Unsafe.** Confined to the smallest possible scope, each block with a `// SAFETY:` comment citing the platform contract, and every unsafe module checked with Miri and, for lock-free protocols, loom.

---

## 3. Process

1. **Research, then architecture, then code.** No library code is written for an area until its architecture and file map are approved. The file map records every file's purpose, functionality, dependencies, path class, performance, efficiency, security and durability goals, failure modes, concurrency model, instrumentation, extension points and tests.
2. **Scenario review before approval.** Every write, read, crash, recovery, upgrade, error, overload, misconfiguration and hostile-input scenario is walked through the planned files. A scenario with no owning file is a design defect.
3. **Change control.** Any change to an approved design, file map, public API, on-disk header or dependency list gets a numbered decision entry (decision, context, alternatives, reasoning, consequences) before the code changes.
4. **Phase gates.** Each phase starts from a written brief and ends with its decision entries, a CHANGELOG entry, updated file-map status and the maintainer's approval of the exit gate.
5. **Testing accompanies every change.** Unit, property, loom, Miri, fuzz, simulation, kill and power-cut tests as the area requires. Every bug fix adds a regression test proven by mutation to fail on the old code, plus a prevention artefact for the whole bug class.
6. **Measurements accompany every change.** Each completed section records its performance and efficiency numbers in its decision entry and in the committed baseline. A regression beyond the noise budget does not merge.

---

## 4. Definition of done

1. Matches the approved file map and design.
2. Compiles clean on Linux, macOS and Windows, on stable and the MSRV.
3. `cargo fmt`, `cargo clippy --all-targets --all-features -D warnings`, `cargo test --all-features` and `cargo doc -D warnings` are clean.
4. `cargo audit`, `cargo deny check` and `cargo vet` pass.
5. Every goal in the file map is measured and recorded; no baseline regression.
6. Instrumented per stage, with zero cost when the observer is detached.
7. Tests green, including mutation checks on every flush, barrier, poison, directory-sync and generation-bump call.
8. Every public item has rustdoc with a runnable example where non-trivial; `docs/API.md` mirrors it.
9. `CHANGELOG.md` updated for every change, and `docs/release/vX.Y.Z.md` written before the tag.

---

## 5. Release ceremony

1. Run the full verification matrix locally (Windows and Linux) and confirm the CI matrix is green on the release commit.
2. Bump the version, cut the CHANGELOG (`## [X.Y.Z] - YYYY-MM-DD`), sweep every document that names a version or describes current behaviour (README, `docs/`, `dev/ROADMAP.md`, rustdoc).
3. Write `docs/release/vX.Y.Z.md`.
4. Commit (imperative, lowercase, no trailing period), push, and watch CI to green.
5. Tag `vX.Y.Z` (annotated), push the tag, and publish the GitHub release with the release note as its body.
6. `cargo publish --dry-run`, then `cargo publish`; confirm docs.rs builds.

---

## 6. Documentation rules

- Shipping files (`src/`, `tests/`, `examples/`, `benches/`, `docs/`, `README.md`, `CHANGELOG.md`) never use the words comprehensive, robust, seamless or leverage, and never contain the em-dash character (U+2014); HTML entities in Markdown headers are fine.
- No emoji, no "Phase X" headers, no placeholder or verification-report blocks in shipping documentation.
- Consumers are described generically everywhere. No consumer's internal formats or protocols appear in this repository.
