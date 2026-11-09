#!/usr/bin/env python3
"""Mutation check for store-io's durability call sites (Directives section 4.7, D-086).

Each entry is (file, pattern, replacement[, packages[, env]]): a plausible bug at a
flush, barrier, poison, directory-sync, generation-bump, accounting or
buffer-ownership site. For each one the pattern is replaced (it must occur
exactly once), the tests of `packages` run (default: the engine and the
simulator) with `env` added to the environment, and the source is restored.
A mutation the tests do not kill is a
survivor, and one that does not compile is invalid; either fails the run.
Entries that can only run on one system are skipped elsewhere and reported
as skipped: the macOS backend's (`crates/store-io-posix/src/mac/`), the
Windows backend's (`crates/store-io-win/`), and the Linux backend's (its
packages Linux-only).

Usage: python3 dev/tools/mutate.py [index ...]
       python3 dev/tools/mutate.py --shard K/N   (entries whose index is K mod N)
       python3 dev/tools/mutate.py --check   (every pattern occurs exactly once; nothing built)
       python3 dev/tools/mutate.py --only-platform   (only the entries that run only on this system)
"""
import os
import subprocess
import time
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
TIMEOUT = int(os.environ.get("MUTATE_TIMEOUT", "600"))

ENGINE = ["-p", "store-io-engine", "-p", "store-io-sim"]
# The Linux backend's own tests (its submit pool included); Linux only.
POSIX = ["-p", "store-io-posix"]
# (It holds the macOS backend too: `platform_of` tags that by its files
# first.)
LINUX_ONLY = {"store-io-posix"}
# The observer's own tests (feature `observe`).
OBSERVE = ["-p", "store-io-core", "--features", "store-io-core/observe"]
SUB = "crates/store-io-engine/src/submitter/mod.rs"
# The buffer crate's loom models, bounded as the CI loom job runs them
# (loom's bounded and unbounded searches each find bugs the other misses),
# in a target directory of their own so the normal build stays cached.
LOOM_BUF = ["-p", "store-io-buf", "--lib", "--release", "loom"]
# The engine's loom models (domain, frontier, gate), bounded the same way.
LOOM_ENGINE = ["-p", "store-io-engine", "--lib", "--release", "loom"]
# The domain's wake tests: a mutant that strands a barrier fails there within
# their 10 s waits, where suites that join stuck threads would hang to the
# timeout.
DOMAIN = ["-p", "store-io-engine", "--lib", "domain::wake_tests"]
# Every domain test, for a mutant only the full-flush tests see.
DOMAIN_ALL = ["-p", "store-io-engine", "--lib", "domain::"]
# The format-2 settling and free-space tests.
FOOTPRINT = ["-p", "store-io-engine", "--lib", "footprint::"]
LOOM = {"RUSTFLAGS": "--cfg loom", "LOOM_MAX_PREEMPTIONS": "3",
        "CARGO_TARGET_DIR": os.path.join(ROOT, "target", "loom")}
STACK = "crates/store-io-buf/src/stack.rs"
# The format crate's own tests (slot, pair and payload codecs).
FORMAT = ["-p", "store-io-format"]
SLOT = "crates/store-io-format/src/slot.rs"
META2 = "crates/store-io-format/src/meta2.rs"
JUDGE = "crates/store-io-format/src/judge.rs"
# Format 2 in the engine: its own tests (the format-1 suites never reach it).
F2 = ["-p", "store-io-engine", "--test", "format2", "--test", "format2_create"]
# The format-2 open's region checks, on crafted media.
F2R = ["-p", "store-io-engine", "--test", "format2_regions", "--test", "format2"]
# Format-2 provisioning, audited on the raw media.
F2P = ["-p", "store-io-engine", "--test", "format2_provision", "--test", "format2"]
# Format-2 release, audited on the raw media.
F2X = ["-p", "store-io-engine", "--test", "format2_release"]
# Format-2 recycle and slot commit.
F2C = ["-p", "store-io-engine", "--test", "format2_recycle"]
# The format-2 open's batched region header reads.
F2O = ["-p", "store-io-engine", "--test", "format2_open"]
# The engine's lifecycle unit tests (release, its extents).
ENGINE_LIB_LIFECYCLE = ["-p", "store-io-engine", "--lib", "lifecycle::"]
# The Submitter's tests.
SUBMITTER_TEST = ["-p", "store-io-engine", "--test", "submitter"]
SCAN_TEST = ["-p", "store-io-engine", "--test", "scan"]
SCAN_SRC = "crates/store-io-engine/src/scan.rs"
# Format-2 regions of several extents.
F2E = ["-p", "store-io-engine", "--test", "format2_extents"]
# Format-2 growth within capacity.
F2G = ["-p", "store-io-engine", "--test", "format2_grow"]
# Format-2 growth beyond capacity.
F2B = ["-p", "store-io-engine", "--test", "format2_grow_beyond"]
# Format-2 region options (capped regions).
F2OPT = ["-p", "store-io-engine", "--test", "format2_options"]
# Format-2 provisioning and growth beside each other.
F2CC = ["-p", "store-io-engine", "--test", "format2_concurrent"]
# The append frontier's unit tests.
FRONTIER = ["-p", "store-io-engine", "--lib", "frontier::"]
# The region map's unit tests; the format crate's footprint tests.
REGIONMAP = ["-p", "store-io-engine", "--lib", "regionmap::"]
FOOTPRINT_FMT = ["-p", "store-io-format", "--test", "footprint"]
V2 = "crates/store-io-engine/src/v2.rs"
# copy_store.
COPY_SRC = "crates/store-io-engine/src/copy.rs"
COPYT = ["-p", "store-io-engine", "--test", "copy"]
# Format 1 read-only.
F1T = ["-p", "store-io-engine", "--test", "format1"]
# Repairs (v0.5 E6).
REPAIR_SRC = "crates/store-io-engine/src/repair.rs"
INLINE = ["-p", "store-io-engine", "--test", "inline"]
COPYS = ["-p", "store-io-engine", "--test", "copy_stale"]
EXEC_SRC = "crates/store-io-engine/src/exec.rs"
F2REP = ["-p", "store-io-engine", "--test", "format2_repair"]
CREATE = "crates/store-io-engine/src/create.rs"
# Crash enumeration (v0.5 E7).
SIMW = "crates/store-io-sim/src/world.rs"
SIMP = "crates/store-io-sim/src/platform.rs"
MODEL = ["-p", "store-io-sim", "--test", "model"]
DAMAGE = ["-p", "store-io-engine", "--test", "format2_damage"]
V2_SRC = "crates/store-io-engine/src/v2.rs"
CRASH_OPEN = ["-p", "store-io-engine", "--test", "format2_crash_open"]

# The macOS backend's files and its own tests (run on macOS only).
MAC_LOCK = "crates/store-io-posix/src/mac/lock.rs"
MAC_ALLOC = "crates/store-io-posix/src/mac/alloc.rs"
MAC_DIR = "crates/store-io-posix/src/mac/dir.rs"
MAC_PROBE = "crates/store-io-posix/src/mac/probe.rs"
MAC_LIB = ["-p", "store-io-posix", "--lib"]
# The Windows backend's own tests (run on Windows only).
WIN_LIB = ["-p", "store-io-win", "--lib"]
# The class rules' own tests, and the Linux probe's (Linux only).
DECIDE = ["-p", "store-io-core", "--lib", "decide::"]
LINUX_PROBE = ["-p", "store-io-posix", "--lib", "probe::"]
BLOCK = "crates/store-io-posix/src/probe/block.rs"
WIN_STORAGE = "crates/store-io-win/src/probe/storage.rs"
WIN_NVME = "crates/store-io-win/src/probe/nvme.rs"

class Joint(tuple):
    """Several mutations made together: one property held by guards in
    layers, each the others' backstop, so removing one alone changes
    nothing; every layer is removed at once. `Joint(parts, tests)`, parts a
    list of (file, pattern, replacement), each pattern found once in its
    file and applied in order."""

    def __new__(cls, parts, tests):
        f, a, b = parts[0]
        return super().__new__(cls, (f, a, b, tests))

    def __init__(self, parts, tests):
        super().__init__()
        self.parts = list(parts)


def parts_of(entry):
    """An entry's mutations: (file, pattern, replacement) each."""
    return getattr(entry, "parts", None) or [tuple(entry[:3])]


# v0.5 audit entries' test sets.
M2_SRC = "crates/store-io-format/src/meta2.rs"
SLOT_SRC = "crates/store-io-format/src/slot.rs"
AUD_CODEC = ["-p", "store-io-format"]
AUD_RO = ["-p", "store-io-engine", "--test", "format2_regions", "--test", "format2_repair"]
AUD_OPEN = ["-p", "store-io-engine", "--test", "format2_regions", "--test", "format2_open",
            "--test", "format2_repair", "--test", "format2_damage", "--test", "format2_release",
            "--test", "format2_grow_beyond"]
AUD_STALE = ["-p", "store-io-engine", "--test", "format2_repair", "--test", "format2_grow",
             "--test", "format2_grow_beyond", "--test", "format2_release", "--test", "format2_recycle",
             "--test", "format2_provision", "--test", "format2_crash_open"]
AUD_SEQ = ["-p", "store-io-engine", "--test", "format2", "--test", "format2_create",
           "--test", "format2_damage", "--test", "format2_crash_open"]
# The flush before provisioning's header is accounting (a power-safe device
# needs none): the header's own durable write flushes the fill before the
# page commit, so no crash state tells it apart.
AUD_FLUSHES = ["-p", "store-io-engine", "--test", "batch", "--test", "store"]
AUD_PROV = ["-p", "store-io-engine", "--test", "format2_provision", "--test", "format2_concurrent",
            "--test", "format2_grow", "--test", "format2_grow_beyond", "--test", "format2_release",
            "--test", "format2_recycle"]

P4B = ["-p", "store-io-engine", "--test", "inline", "--test", "format2_extents",
       "--test", "store", "--test", "durable"]
INLINE_ENV = {"STORE_IO_SIM_INLINE": "1"}
# PLANNING R5.2 and R5.7 (v0.6 V1): device distinctness, open's identity check.
IDENT = ["-p", "store-io-core", "--lib", "identity::"]
STORE_T = ["-p", "store-io-engine", "--test", "store"]
LINUX_ID = ["-p", "store-io-posix", "--lib", "probe::identity::"]
WIN_ID = ["-p", "store-io-win", "--lib", "probe::identity_tests"]
BUF_LIB = ["-p", "store-io-buf", "--lib"]
POSIX_LINUX_T = ["-p", "store-io-posix", "--test", "linux"]
WIN_NTFS_T = ["-p", "store-io-win", "--test", "ntfs"]
ROOT_CONFORMANCE = ["-p", "store-io", "--test", "conformance"]
QOS_SRC = "crates/store-io-engine/src/qos.rs"
QOS_T = ["-p", "store-io-engine", "--lib", "qos::"]
QOS_STORE_T = ["-p", "store-io-engine", "--test", "qos"]
PACE_T = ["-p", "store-io-engine", "--lib", "pace::"]
COPY_FEED_T = ["-p", "store-io-engine", "--test", "copy_feed"]
COPY_UNIT_T = ["-p", "store-io-engine", "--lib", "copy::tests"]
PUMP_POISON_T = ["-p", "store-io-engine", "--test", "pump_poison"]
WIN_SMBIOS = ["-p", "store-io-win", "--lib", "probe::storage::tests::test_smbios_says_physical"]
WIN_SYS = ["-p", "store-io-win", "--lib", "sys::tests::test_a_file_id_of_zero_or_minus_one_identifies_nothing"]
MAC_ID = ["-p", "store-io-posix", "--lib", "probe::tests::test_only_one_physical_store"]
# V2's dependent writes (group AFTER): the checks, the receipt's append bit,
# the resume base; the race's order of lane, passes, reservation and issue.
AFTER_SRC = "crates/store-io-engine/src/after.rs"
IO_SRC = "crates/store-io-engine/src/io.rs"
SUB_SRC = "crates/store-io-engine/src/submitter/mod.rs"
MULTI_T = ["-p", "store-io-engine", "--test", "multi"]
MULTI_CRASH = ["-p", "store-io-engine", "--test", "multi_crash"]
MULTI_SRC = "crates/store-io-engine/src/multi.rs"
DOMAIN_SRC = "crates/store-io-engine/src/domain/mod.rs"
POISON_T = ["-p", "store-io-engine", "--lib", "domain::tests::test_every_poisoning"]
AFTER_RACE = ["-p", "store-io-engine", "--test", "after_race"]
EXTENTS_T = ["-p", "store-io-engine", "--test", "format2_extents"]
DEPS_OBS = ["-p", "store-io-engine", "--features", "observe", "--test", "observe"]

M = [
 ("crates/store-io-engine/src/batch.rs", "                carried = l % block;", "                carried = l % block * 0;"),
 ("crates/store-io-engine/src/io.rs", "                self.offset += buf.len() as u64;", "                self.offset += buf.len() as u64 + 4096;"),
 ("crates/store-io-engine/src/io.rs", "        dst[n..padded].fill(0);", "        let _ = &dst[n..padded];"),
 ("crates/store-io-engine/src/batch.rs", "            last.as_mut_capacity()[l..padded].fill(0);", "            let _ = l;"),
 ("crates/store-io-engine/src/batch.rs", "            slot.fill(0);\n            fill(slot);", "            fill(slot);"),
 ("crates/store-io-engine/src/receipt.rs", "&& (t.need <= self.need ||", "&& (t.need <= self.need.saturating_add(1) ||"),
 ("crates/store-io-engine/src/receipt.rs", "        self.epoch == t.epoch\n            &&", "        "),
 ("crates/store-io-engine/src/io.rs", "            need: self.inner.domain.ticket(),", "            need: 0,"),
 ("crates/store-io-engine/src/batch.rs", "            .any(|w| w[0].0 + w[0].1.len() as u64 > w[1].0);", "            .any(|w| w[0].0 + w[0].1.len() as u64 > w[1].0 + 4096);"),
 ("crates/store-io-engine/src/exec.rs", "                (Ok(n), Some(buf)) if n == buf.len() => src.recycle(buf),\n                (result, _) => {", "                (Ok(_), Some(buf)) => src.recycle(buf),\n                (result, _) => {"),
 ("crates/store-io-engine/src/io.rs", "        if self.r.take().is_some() {\n            self.store.inner.domain.poison(", "        if self.r.take().is_some() && false {\n            self.store.inner.domain.poison("),
 ("crates/store-io-engine/src/batch.rs", "                    self.truncate(at);\n", "                    if false { self.truncate(at); }\n"),
 ("crates/store-io-engine/src/scan.rs", "                if len <= sector {", "                if len <= sector || true {"),
 ("crates/store-io-engine/src/scan.rs", "            Some((_, e)) if *e == start => *e = end,", "            Some((_, e)) if *e == start && false => *e = end,"),
 ("crates/store-io-engine/src/scan.rs", "                if ring[idx].as_ref().is_none_or(|p| p.done.is_none()) {", "                if ring[idx].is_none() {"),
 ("crates/store-io-engine/src/scan.rs", "                                out.unreadable(start + n as u64, start + len as u64, &mut drain);", ""),
 ("crates/store-io-engine/src/scan.rs", "        if (self.visit)(ScanItem::Data { offset, bytes }, drain).is_break() {", "        if (self.visit)(ScanItem::Data { offset, bytes }, drain).is_break() && false {"),
 ("crates/store-io-engine/src/scan.rs", "                let half = (len / unit).div_ceil(2) * unit;", "                let half = len - unit;"),
 ("crates/store-io-engine/src/gate.rs", "            LIVE => Ok(()),\n            s => {\n                self.leave();", "            s if s == LIVE || s != LIVE => Ok(()),\n            s => {\n                self.leave();"),
 ("crates/store-io-engine/src/gate.rs", "        while self.active.fetch_add(0, Ordering::AcqRel) != 0 {", "        while self.active.fetch_add(0, Ordering::AcqRel) != 0 && false {"),
 ("crates/store-io-engine/src/lifecycle.rs", "            .retain(|x| x.id != r.id);", "            .retain(|_| true);"),
 ("crates/store-io-engine/src/lifecycle.rs", "        entry.state = RegionState::Released;", ""),
 ("crates/store-io-engine/src/provision.rs", "            .checked_add(reserved_after)", "            .checked_add(0 * reserved_after)"),
 ("crates/store-io-engine/src/space.rs", "        t.cap.is_none_or(|cap| {", "        true || t.cap.is_none_or(|cap| {"),
 ("crates/store-io-engine/src/provision.rs", "            r.consume(extent)?;", "            if false { r.consume(extent)?; }"),
 ("crates/store-io-engine/src/space.rs", "            a.add(e, block);", "            let _ = (e, block, &mut a);"),
 ("crates/store-io-engine/src/space.rs", "        lock_acct(&self.store.inner.acct).unreserve(self.tag, self.bytes);", ""),
 ("crates/store-io-engine/src/space.rs", "        let mut meta = lock_meta(&inner.meta);\n        let mut acct = lock_acct(&inner.acct);\n        if !acct.fits(tag, bytes) {", "        let mut acct = lock_acct(&inner.acct);\n        let mut meta = lock_meta(&inner.meta);\n        if !acct.fits(tag, bytes) {"),
 ("crates/store-io-engine/src/directory.rs", "        self.platform\n            .sync_dir(&self.dir)\n            .map_err(|raw| self.poison(Op::SyncDir, Some(raw)))\n    }\n\n    /// Takes the directory", "        let _ = self.platform.sync_dir(&self.dir);\n        Ok(())\n    }\n\n    /// Takes the directory"),
 ("crates/store-io-engine/src/directory.rs", "            .set_len(file, len)", "            .set_len(file, len.div_ceil(4096) * 4096)"),
 ("crates/store-io-engine/src/directory.rs", "            .map_err(|raw| self.poison(Op::FlushAll, Some(raw)))", "            .or(Ok(()))"),
 ("crates/store-io-engine/src/directory.rs", "        let _absent_or_removed = self.platform.unlink(&self.dir, &temp);", ""),
 ("crates/store-io-engine/src/provision.rs", "            .any(|e| e.state == RegionState::Ready && e.name == rname)", "            .any(|e| e.state == RegionState::Ready && e.name == rname && false)"),
 ("crates/store-io-engine/src/provision.rs", "        if meta.entries.len() + usize::from(reuse.is_none()) > inner.layout.table_capacity() {", "        if false {"),
 ("crates/store-io-engine/src/provision.rs", "            let z = zeros.saturating_sub(at).min(len) as usize;", "            let z = if at < zeros { len as usize } else { 0 };"),
 ('crates/store-io-engine/src/lifecycle.rs', 'release_range(&inner.file, at, len, how)', 'release_range(&inner.file, at - 2 * inner.layout.block(), len + 2 * inner.layout.block(), how)'),
 ("crates/store-io-engine/src/create.rs", "        if holds != Some(false) ||", "        if holds != Some(false) && false ||"),
 ("crates/store-io-engine/src/provision.rs", "                Err(Error::NoSpace { .. }) if align && from.is_some() => align = false,\n", "                Err(Error::NoSpace { .. }) if align && from.is_some() && false => align = false,\n"),
 # v0.3.1 (D-068..D-072)
 ("crates/store-io-engine/src/create.rs", "        if holds != Some(false) ||", "        if holds == Some(true) ||"),
 ("crates/store-io-engine/src/slots.rs", "            Some((i, _)) if i == w.index => w.payload,", "            Some((i, _)) if i == w.index && false => w.payload,"),
 ("crates/store-io-engine/src/store.rs", "    Ok(prev.after(written))", "    let _ = written;\n    Ok(prev.clone())"),
 ("crates/store-io-engine/src/io.rs", "                Some(mut lane) => flush_data(&mut lane, file),", "                Some(_lane) => flush_data(&mut self.inner.queues.get(0), file),"),
 ("crates/store-io-engine/src/provision.rs", "                    inner.domain.poison(FirstCause {\n                        op: Some(Op::Write),\n                        raw,\n                    });\n                    self.wake_frontiers();", "                    self.wake_frontiers();"),
 ("crates/store-io-engine/src/region.rs", "        if self.store.inner.read_only || self.state.is_read_only() {\n            return Err(Error::NotWritten {\n                cause: NotWrittenCause::ReadOnly,", "        if self.state.is_read_only() {\n            return Err(Error::NotWritten {\n                cause: NotWrittenCause::ReadOnly,"),
 # D-090: unavailable regions are reported and never NotFound
 ("crates/store-io-engine/src/store.rs", ".find(|u| u.name == name.as_str() && u.kind == kind)", ".find(|u| u.name == name.as_str() && u.kind == kind && false)"),
 ("crates/store-io-engine/src/open.rs", "            io: platform.io_path(),\n            unavailable,\n            format:", "            io: platform.io_path(),\n            unavailable: {\n                drop(unavailable);\n                Vec::new()\n            },\n            format:"),
 # Read errors are classified and media errors name the exact range
 ('crates/store-io-engine/src/settle.rs', '        Err(raw) if is_media_error(raw) => {', '        Err(raw) if true || is_media_error(raw) => {'),
 ("crates/store-io-engine/src/io.rs", "self.first_unreadable(r, start, end).unwrap_or((start, end));", "self.first_unreadable(r, start, end).map(|_| (start, end)).unwrap_or((start, end));"),
 # Directory: refuse before any write without directory flushes; fail-stop on set_len
 ("crates/store-io-engine/src/directory.rs", "        let dir_flush = platform.sync_dir(&dir).err();", "        let dir_flush = None;"),
 ("crates/store-io-engine/src/directory.rs", "            .map_err(|raw| self.poison(Op::Allocate, Some(raw)))?;", "            .map_err(|raw| io(Op::Allocate, raw))?;"),
 # TYP-02: an already-durable receipt is counted
 ("crates/store-io-engine/src/region.rs", "            s.inner.domain.count_already_durable();", "            {}"),
 # D-092: a read-only open takes the shared lock on its read-only handle
 ("crates/store-io-engine/src/open.rs", "            platform.lock_shared(&file)", "            platform.lock_exclusive(&file)"),
 # Sector-exact bisection, and the pipelined read names the exact range
 ("crates/store-io-engine/src/io.rs", "        need.next_power_of_two().min(self.block())", "        need.min(0).max(self.block())"),
 ("crates/store-io-engine/src/io.rs", "                Some((start, end)) => ctx_range(r, start, end),", "                Some(_) => c,"),
 # Review fixes: growth never resurrects a truncated region; sector gate; slot read classification; bisect backstop
 # (grow_to's format-1 guard, a listed ContainerShort region, went in v0.5 E5:
 # format 1 opens read-only, and format 2's cut_short guard fires first.)
 ("crates/store-io-engine/src/io.rs", "        if ev.missing.contains(Missing::DioAlignment) || ev.fs.dio_offset_align == 0 {", "        if (ev.missing.contains(Missing::DioAlignment) || ev.fs.dio_offset_align == 0) && false {"),
 ("crates/store-io-engine/src/slots.rs", "    is_media_error(raw) || is_end_of_file(raw)\n", "    is_media_error(raw) || is_end_of_file(raw) || true\n"),
 ("crates/store-io-engine/src/scan.rs", "            Err(_) if len < self.block() => {", "            Err(_) if false => {"),
 # Second review: block-aligned halving; volume search keeps real errors; buffer released before the rescan
 ("crates/store-io-engine/src/scan.rs", "let unit = if len > block { block } else { sector };", "let unit = if len > block && false { block } else { sector };"),
 ("crates/store-io-engine/src/open.rs", "                Err(raw) if !crate::slots::unreadable(raw) => return Err(io(raw)),", "                Err(raw) if !crate::slots::unreadable(raw) && false => return Err(io(raw)),"),
 ("crates/store-io-engine/src/io.rs", ".unwrap_or((start, end));", "?;"),
 # Final review: the slot read classification (`landed`, shared by read_one and read_pairs); volume search reports media errors
 ("crates/store-io-engine/src/slots.rs", "        Err(raw) if !unreadable(raw) => Err(Error::Io {", "        Err(raw) if !unreadable(raw) && false => Err(Error::Io {"),
 ("crates/store-io-engine/src/open.rs", "                    media_error |= store_io_core::errno::is_media_error(raw);", "                    media_error |= store_io_core::errno::is_media_error(raw) && false;"),
 ("crates/store-io-engine/src/open.rs", "            if bytes.len() > store_io_format::meta::VOLUME_META_LEN {", "            if bytes.len() > store_io_format::meta::VOLUME_META_LEN && false {"),
 ("crates/store-io-engine/src/open.rs", "store_io_format::meta::MetaError::Field(\"features\") => newer,", "store_io_format::meta::MetaError::Field(\"features\") if false => newer,"),
 ("crates/store-io-engine/src/open.rs", "            media_error |= pair.media_error;", "            media_error |= pair.media_error && false;"),
 ("crates/store-io-engine/src/open.rs", "        Winner::Lost if p.media_error => Err(Error::Corruption {", "        Winner::Lost if p.media_error && false => Err(Error::Corruption {"),
 # D-095: a new operation is first seen anew (lane marks: A1 C6, below)
 ("crates/store-io-engine/src/stats/mod.rs", "            if mark % 2 == 0 || entry.0 != mark {", "            if mark % 2 == 0 {"),
 # D-096: a pooled queue's drop waits for its jobs; its depth bounds the pool
 ("crates/store-io-posix/src/queue.rs", "                p.inbox.settle(p.in_flight);", "                if false { p.inbox.settle(p.in_flight); }", POSIX),
 ("crates/store-io-posix/src/queue.rs", "        if self.in_flight() >= self.depth {", "        if self.ready.len() >= self.depth {", POSIX),
 # Starting the pool leaves the caller's signal mask as it was, and the
 # workers open the signals the kernel forces on a faulting thread
 ("crates/store-io-posix/src/posix.rs", "        if let Some(old) = &self.old {", "        if let Some(old) = &self.old.filter(|_| false) {", POSIX),
 ("crates/store-io-posix/src/posix.rs", "    for sig in FAULT_SIGNALS {\n        // SAFETY: sigaddset", "    for sig in FAULT_SIGNALS.iter().copied().filter(|_| false) {\n        // SAFETY: sigaddset", POSIX),
 ("crates/store-io-posix/src/posix.rs", "        if unsafe { libc::sigfillset(set.as_mut_ptr()) } != 0 {", "        if unsafe { libc::sigfillset(set.as_mut_ptr()) } != 0 || true {", POSIX),
 # D-096: each parallel lane owns its own busy mark
 ("crates/store-io-engine/src/exec.rs", ".zip(&self.marks[n..2 * n])", ".zip(&self.marks[0..n])"),
 # D-097: every lane holder starts on an empty queue; an abandoned operation is never success nor a media verdict
 ("crates/store-io-engine/src/exec.rs", "    if let Err(raw) = clear_stale(slot) {", "    if let Err(raw) = Ok::<(), OsError>(()) {"),
 ("crates/store-io-engine/src/io.rs", "        clear_stale(&mut lane).map_err(|raw| Error::Io {", "        { let _never = clear_stale::<P::Queue>; let _keep = &mut lane; Ok::<(), OsError>(()) }.map_err(|raw| Error::Io {"),
 ("crates/store-io-engine/src/slots.rs", "        Ran::Abandoned(raw) => return Err(fail(raw)),", "        Ran::Abandoned(raw) => {\n            let _ = &fail;\n            (None, Err(raw))\n        }"),
 ("crates/store-io-engine/src/io.rs", "                Ran::Abandoned(raw) => {\n                    return Err(Error::Io {\n                        op: Op::Read,\n                        raw,\n                        ctx: c,\n                    });\n                }", "                Ran::Abandoned(raw) => (None, Err(raw)),"),
 ("crates/store-io-engine/src/open.rs", "                Ok(Ran::Abandoned(raw)) | Err((_, Some(raw))) => return Err(io(raw)),", "                Ok(Ran::Abandoned(raw)) => (None, Err(raw)),\n                Err((_, Some(raw))) => return Err(io(raw)),"),
 ("crates/store-io-engine/src/store.rs", "        Ok(Ran::Abandoned(raw)) => Err(Some(raw)),", "        Ok(Ran::Abandoned(_raw)) => Ok(()),"),
 ("crates/store-io-engine/src/slots.rs", "        Ran::Abandoned(raw) => Err(raw),", "        Ran::Abandoned(_raw) => Ok(at.size()),"),
 ("crates/store-io-engine/src/directory.rs", "                Ran::Abandoned(raw) => Err(raw),", "                Ran::Abandoned(_raw) => Ok(piece_len),"),
 ("crates/store-io-engine/src/directory.rs", "                Ran::Abandoned(raw) => return Err(io(Op::Read, raw)),", "                Ran::Abandoned(raw) => (None, Err(raw)),"),
 ("crates/store-io-engine/src/create.rs", "        else {\n            return None;\n        };", "        else {\n            return Some(false);\n        };"),
 ("crates/store-io-engine/src/provision.rs", "                Ran::Abandoned(raw) => (Err(raw), true),", "                Ran::Abandoned(_raw) => (Ok(len as usize), false),"),
 ("crates/store-io-engine/src/provision.rs", "                Err(raw) if !abandoned && store_io_core::errno::is_no_space(raw) => {", "                Err(raw) if (abandoned || !abandoned) && store_io_core::errno::is_no_space(raw) => {"),
 # D-098: the Submitter mints tickets and completes reservations only from full
 # completions, holds passes until processed, drains on drop, admits like the
 # blocking calls, and release never waits for it holding the metadata lock
 ('crates/store-io-engine/src/optable.rs', '    let full = matches!((result, buf), (Ok(n), Some(b)) if n == b.len());', '    let full = matches!((result, buf), (Ok(_), Some(_)));'),
 ('crates/store-io-engine/src/settle.rs', '            Some(f) if full => {', '            Some(f) if full || true => {'),
 ('crates/store-io-engine/src/settle.rs', '                f.complete(r);\n', '                let _ = (f, r);\n'),
 ('crates/store-io-engine/src/gate.rs', '        held.gate().admit()?;\n', '        held.gate().admit()?;\n        held.gate().leave();\n'),
 (SUB, '        while self.core.table.live > 0\n            && (self.core.queue.in_flight() > 0 || !self.core.table.parked.is_empty())\n        {', '        while self.core.table.live > 0\n            && (self.core.queue.in_flight() > 0 || !self.core.table.parked.is_empty())\n            && false\n        {'),
 (SUB, '            && (self.core.table.writes_live() || !self.core.table.consistent())\n        {', '            && (self.core.table.writes_live() || !self.core.table.consistent())\n            && false\n        {'),
 (SUB, '        if !Arc::ptr_eq(&store.inner, &self.store.inner) {', '        if !Arc::ptr_eq(&store.inner, &self.store.inner) && false {'),
 (SUB, '        if !self.store.inner.pool.owns(buf) {', '        if !self.store.inner.pool.owns(buf) && false {'),
 ('crates/store-io-engine/src/optable.rs', '        self.live + self.ready.len() >= self.rows.len()', '        self.live >= self.rows.len()'),
 (SUB, '        let pass = admit!(self.admit(region.store(), state, &buf), buf);\n        admit!(self.store.ready_for(state), buf);\n        let Some(f)', '        let pass = admit!(self.admit(region.store(), state, &buf), buf);\n        let Some(f)'),
 (SUB, '        if !state.positioned.load(std::sync::atomic::Ordering::Acquire) {', '        if false {'),
 (SUB, '        buf.as_mut_capacity()[len..padded].fill(0);', '        let _ = (len, padded);'),
 (SUB, '        let end = admit!(region.check(&pos, buf.len()), buf);', '        let end = pos.offset + buf.len() as u64;'),
 (SUB, '        if pos.generation != state.generation {', '        if false {'),
 ('crates/store-io-engine/src/settle.rs', '        Err(raw) if is_media_error(raw) => {', '        Err(raw) if false && is_media_error(raw) => {'),
 ('crates/store-io-engine/src/settle.rs', '                Ok(store.ticket(state, r.start, r.end))', '                Ok(store.ticket(state, r.start, r.data_end))'),
 (SUB, '    _thread: PhantomData<*const ()>,', '    _thread: PhantomData<()>,'),
 ('crates/store-io-engine/src/optable.rs', '            let _shorter = b.set_len(*len);', '            let _shorter = (b, len);'),
 ('crates/store-io-engine/src/lifecycle.rs', '        self.retire(r, Retire::Release)?;\n        let mut meta = lock_meta(&inner.meta);', '        let mut meta = lock_meta(&inner.meta);\n        self.retire(r, Retire::Release)?;'),
 ('crates/store-io-engine/src/optable.rs', '            Some(&(_, seq)) if self.live > 0 => 2 * seq + 1,', '            Some(&(_, _seq)) if self.live > 0 => 2 * self.next_seq + 1,'),
 # D-099: a durable write's receipt only after a flush issued after its
 # completion succeeded; barriers check their tickets; drop waits for flushes
 ('crates/store-io-engine/src/optable.rs', '        if live.durable && flushes() {', '        if live.durable && flushes() && false {'),
 ('crates/store-io-engine/src/optable.rs', '                let need = w.ticket.need;', '                let need = 0 * w.ticket.need;'),
 ('crates/store-io-engine/src/flusher.rs', '        let outcome = held.barrier_to_classed(need, class).map(|()| need);', '        let outcome = held.barrier_to_classed(need, class).map(|()| need).or(Ok(need));'),
 ('crates/store-io-engine/src/flusher.rs', '        let need = guard.batch.iter().map(|r| r.need).max().unwrap_or(0);', '        let need = guard.batch.iter().map(|r| r.need).min().unwrap_or(0);'),
 ('crates/store-io-engine/src/submitter/durable.rs', '            Some((g, live, _)) if g != ticket.generation || !live => {', '            Some((g, live, _)) if (g != ticket.generation || !live) && false => {'),
 ('crates/store-io-engine/src/submitter/durable.rs', '        if ticket.epoch != inner.epoch || ticket.volume != inner.volume {', '        if (ticket.epoch != inner.epoch || ticket.volume != inner.volume) && false {'),
 (SUB, '        while self.core.table.asked > 0 {\n            self.inbox.wait(false);', '        while self.core.table.asked > 0 && false {\n            self.inbox.wait(false);'),
 ('crates/store-io-engine/src/domain/mod.rs', '        let done = self.done.load(Ordering::SeqCst) >= need;', '        let done = self.done.load(Ordering::SeqCst) >= need || true;'),
 ('crates/store-io-engine/src/optable.rs', '                    receipt.need = need;', '                    receipt.need = 0 * need;'),
 ('crates/store-io-engine/src/settle.rs', '                if through >= r.end {', '                if through >= r.end || true {'),
 ('crates/store-io-engine/src/submitter/durable.rs', '        if self.store.inner.read_only {\n            // Nothing to flush', '        if self.store.inner.read_only && false {\n            // Nothing to flush'),
 ('crates/store-io-engine/src/flusher.rs', '            IN_QUEUE => self.queue.wake(),', '            IN_QUEUE => {}'),
 ('crates/store-io-engine/src/flusher.rs', '        if self.count.load(Ordering::SeqCst) > 0 || self.woken.load(Ordering::SeqCst) {', '        if (self.count.load(Ordering::SeqCst) > 0 || self.woken.load(Ordering::SeqCst)) && false {'),
 (SUB, '            if self.inbox.take_woken() {\n                break;', '            if self.inbox.take_woken() && false {\n                break;'),
 # D-101: the blocking layers keep max_io pieces; a submitter's largest operation is max_write
 ('crates/store-io-engine/src/open.rs', '        let chunk = (ordinary / block * block).max(block);', '        let chunk = (pool.max_len() / block * block).max(block);'),
 (SUB, '        if len > self.max_buf() {', '        if len > self.max_buf() && false {'),
 ('crates/store-io-engine/src/io.rs', '            .take_within(len, self.ordinary())', '            .take_within(len, self.ordinary().max(usize::MAX))'),
 # D-102: a forced region's barriers flush, and only flush-backed receipts say data_synced
 ('crates/store-io-engine/src/io.rs', '        if !self.inner.policy.flush && !r.forced() {', '        if !self.inner.policy.flush && (!r.forced() || true) {'),
 ('crates/store-io-engine/src/optable.rs', '            || store.inner.policy.flush || live.pass.as_ref().is_some_and(|p| p.get().forced());', '            || store.inner.policy.flush;'),
 ('crates/store-io-engine/src/submitter/durable.rs', '        let flush = inner.policy.flush || forced;', '        let flush = inner.policy.flush || (forced && false);'),
 ('crates/store-io-engine/src/region.rs', '        let synced = flushes || self.state.forced();', '        let synced = flushes;'),
 ('crates/store-io-engine/src/io.rs', '            synced: need != u64::MAX && need != 0,', '            synced: (need != u64::MAX && need != 0) || true,'),
 ('crates/store-io-engine/src/lifecycle.rs', '            force_sync: Arc::clone(&r.force_sync),', '            force_sync: Arc::new(std::sync::atomic::AtomicBool::new(false)),'),
 ('crates/store-io-engine/src/optable.rs', '                    receipt.synced = need != u64::MAX;', '                    receipt.synced = true;'),
 ('crates/store-io-engine/src/slot_handle.rs', '        let flushed = s.inner.policy.flush || self.state.forced();', '        let flushed = s.inner.policy.flush;'),
 ('crates/store-io-engine/src/region.rs', '        if synced {\n            f.publish_synced(snapshot);', '        if synced && false {\n            f.publish_synced(snapshot);'),
 ('crates/store-io-engine/src/submitter/durable.rs', '                .any(|r| r.forced());', '                .any(|r| r.forced() && false);'),
 ('crates/store-io-engine/src/settle.rs', '            crate::region::sync_tail(store, state, through, need, need != u64::MAX)', '            crate::region::sync_tail(store, state, through, need, need != u64::MAX || true)'),
 # D-103 3a: the io_uring ring: allowlist, full ring, deferred completions, ring sizes,
 # the owner thread, the registered descriptor only on owned rings
 ('crates/store-io-posix/src/uring/ring.rs', '        let can_release = restrict(&fd, &supported)?;', '        let can_release = true || restrict(&fd, &supported)?;', POSIX),
 ('crates/store-io-posix/src/uring/ring.rs', '        if self.space() == 0 {', '        if false {', POSIX),
 ('crates/store-io-posix/src/uring/ring.rs', '            if self.rings.sq_flags() & (SQ_TASKRUN | SQ_CQ_OVERFLOW) != 0 {', '            if self.rings.sq_flags() & (SQ_TASKRUN | SQ_CQ_OVERFLOW) != 0 && false {', POSIX),
 ('crates/store-io-posix/src/uring/sys.rs', '            return Err(einval());\n        }\n        if p.flags & setup::NO_SQARRAY == 0 {', '            let _ = pow2;\n        }\n        if p.flags & setup::NO_SQARRAY == 0 {', POSIX),
 ('crates/store-io-posix/src/uring/ring.rs', '            Some(id) if !is_this_thread(id) => Err(errno(libc::EXDEV)),', '            Some(id) if false && !is_this_thread(id) => Err(errno(libc::EXDEV)),', POSIX),
 ('crates/store-io-posix/src/uring/ring.rs', '        let registered = if flavour == Flavour::Owned && can_release {', '        let registered = if (flavour == Flavour::Owned || true) && can_release {', POSIX),
 # D-103 3b: the io_uring queue
 ('crates/store-io-posix/src/uring/queue.rs', '        if self.lost {\n            return Err(refuse(Some(buf), Some(libc::EBADR)));', '        if false {\n            return Err(refuse(Some(buf), Some(libc::EBADR)));', POSIX),
 ('crates/store-io-posix/src/uring/queue.rs', '        if let Err(e) = self.ring.check_thread() {\n            return Err(Rejected {\n                buf: Some(buf),', '        if let Err(e) = self.ring.check_thread().or_else(|_| Ok::<(), OsError>(())) {\n            return Err(Rejected {\n                buf: Some(buf),', POSIX),
 ('crates/store-io-posix/src/uring/queue.rs', '        if write.is_some() && !file.is_writable() {', '        if false && write.is_some() && !file.is_writable() {', POSIX),
 ('crates/store-io-posix/src/uring/queue.rs', '        if self.in_flight() >= self.depth || self.ring.space() == 0 {', '        if self.ring.space() == 0 {', POSIX),
 ('crates/store-io-posix/src/uring/queue.rs', '        r if r == -libc::EAGAIN || r == -libc::EOPNOTSUPP => 0,', '        r if r == -libc::EOPNOTSUPP => 0,', POSIX),
 ('crates/store-io-posix/src/uring/queue.rs', '        r if r == -libc::EAGAIN || r == -libc::EOPNOTSUPP => 0,', '        r if r == -libc::EAGAIN => 0,', POSIX),
 ('crates/store-io-posix/src/uring/queue.rs', '            op.buf.as_ptr().wrapping_add(done),', '            op.buf.as_ptr(),', POSIX),
 ('crates/store-io-posix/src/uring/queue.rs', '    let dsync = if write == Some(true) {', '    let dsync = if write == Some(true) && false {', POSIX),
 ('crates/store-io-posix/src/uring/queue.rs', '        self.dropping = true;\n        if self.armed {\n            sys::eventfd_signal(&self.wake);', '        self.dropping = true;\n        if false {\n            sys::eventfd_signal(&self.wake);', POSIX),
 ('crates/store-io-posix/src/uring/queue.rs', '    if done >= len || (op.write.is_none() && res == 0) {', '    if done >= len || (op.write.is_none() && res == 0) || op.write.is_some() {', POSIX),
 ('crates/store-io-posix/src/uring/queue.rs', '    let offset = op.offset + done as libc::off_t;', '    let offset = op.offset + 0 * done as libc::off_t;', POSIX),
 ('crates/store-io-posix/src/uring/queue.rs', '                core::mem::forget(op.buf);\n                self.ready.push_back(', '                drop(op.buf);\n                self.ready.push_back(', POSIX),
 ('crates/store-io-posix/src/uring/queue.rs', '    if slot.generation != (ud >> 32) as u32 {', '    if false && slot.generation != (ud >> 32) as u32 {', POSIX),
 ('crates/store-io-posix/src/uring/queue.rs', '            Ok(_) if !unexpected => {}', '            Ok(_) if !unexpected || true => {}', POSIX),
 ('crates/store-io-posix/src/uring/queue.rs', '        while !self.lost && (self.kernel > 0 || self.armed) {', '        while false && !self.lost && (self.kernel > 0 || self.armed) {', POSIX),
 # D-100: only a block's one writer may update it without an atomic add
 ('crates/store-io-core/src/observe.rs', '                None => (&self.shared[index & self.mask], false),', '                None => (&self.shared[index & self.mask], true),', OBSERVE),
 # A1 C2: the blocking writes' one verdict (settle_pumped): an append the
 # platform refused after its reservation is a hole (poisons, never a plain
 # refusal), and a pump that failed is never acknowledged
 ('crates/store-io-engine/src/settle.rs', '        (Pumped::Refused(raw) | Pumped::Failed(raw), _) => {\n', '        (Pumped::Refused(raw) | Pumped::Failed(raw), _) => {\n            if matches!(outcome, Pumped::Refused(_)) {\n                return Err(Store::<P>::refused(raw, ErrorContext::default()));\n            }\n'),
 ('crates/store-io-engine/src/settle.rs', '            settle_write(store, state, wrote, false, raw, &mut no_prefix)', '            settle_write(store, state, wrote, true, raw, &mut no_prefix)'),
 # A1 C3: a submitter operation is recorded (busy mark published) before it
 # is submitted, and a refused submission is aborted
 ('crates/store-io-engine/src/submitter/mod.rs', '        self.core.table.start(index, tag, kind, Some(pass), durable);\n        if let Some(s) = split {\n            self.core.table.set_split(index, s);\n        }\n        // Held by the row: released when the piece completes, or with the\n        // row (a refused submission below aborts it).\n        self.core.table.set_charge(qtag, charge);\n        if let Err(r) = self.core.queue.submit(op, qtag) {', '        let refused = self.core.queue.submit(op, qtag);\n        self.core.table.start(index, tag, kind, Some(pass), durable);\n        if let Some(s) = split {\n            self.core.table.set_split(index, s);\n        }\n        self.core.table.set_charge(qtag, charge);\n        if let Err(r) = refused {'),
 ('crates/store-io-engine/src/optable.rs', '        drop(live);\n        self.free.push(index);\n        self.live -= 1;', '        drop(live);\n        self.free.push(index);'),
 ('crates/store-io-engine/src/optable.rs', '        drop(live);\n        self.free.push(index);\n', '        drop(live);\n'),
 # A1 C5: a lane's operations live in its op table; a refused submission
 # gives its row back
 ('crates/store-io-engine/src/exec.rs', '            .inspect_err(|_| self.core.table.abort(index))', '            .inspect_err(|_| ())'),
 # A1 C6: a lane's busy mark is its op table's: odd while an operation is in
 # flight (published when a row starts and when one is freed)
 ('crates/store-io-engine/src/optable.rs', '            Some(&(_, seq)) if self.live > 0 => 2 * seq + 1,', '            Some(&(_, seq)) if self.live > 0 => 2 * seq,'),
 ('crates/store-io-engine/src/optable.rs', '        self.free.push(index as u32);\n        self.live -= 1;\n        while let Some(&(i, s)) = self.order.front() {\n            if is_live(&self.rows, i, s) {\n                break;\n            }\n            let _stale = self.order.pop_front();\n        }\n        self.publish();\n        Some(live.tag)', '        self.free.push(index as u32);\n        self.live -= 1;\n        self.publish();\n        Some(live.tag)'),
 ('crates/store-io-engine/src/exec.rs', '        self.core.table.start(index, tag, Kind::Raw, None, false);\n        self.core\n            .queue\n            .submit(op, qtag)', '        let r = self.core.queue.submit(op, qtag);\n        self.core.table.start(index, tag, Kind::Raw, None, false);\n        r'),
 # 3d (D-078, D-107): the direct descriptor only for a read of the
 # registered file itself, never a write (lock fencing); the slot is a
 # read-only twin of the file, never the store's own description (the
 # ban on registered-buffer opcodes is checked at compile time)
 ('crates/store-io-posix/src/file.rs', '    let twin = sys::openat(\n        libc::AT_FDCWD,\n        &path,\n        access | libc::O_DIRECT | libc::O_CLOEXEC,\n        0,\n    )?;', '    let _ = (&path, access);\n    let twin = sys::dup_cloexec(fd.as_raw_fd())?;', POSIX),
 ('crates/store-io-posix/src/file.rs', '    twin(fd, libc::O_RDONLY)', '    twin(fd, libc::O_RDWR)', POSIX),
 ('crates/store-io-posix/src/uring/queue.rs', '            write.is_none() && self.fixed_file.as_ref().is_some_and(|f| file.same_fd(f));', '            self.fixed_file.as_ref().is_some_and(|f| file.same_fd(f));', POSIX),
 ('crates/store-io-posix/src/uring/queue.rs', '            write.is_none() && self.fixed_file.as_ref().is_some_and(|f| file.same_fd(f));', '            write.is_none() && self.fixed_file.as_ref().is_some_and(|f| file.same_fd(f) || true);', POSIX),
 # release keeps the metadata lock until its space is deallocated (no
 # provisioning can reuse the extent under its late writes)
 ('crates/store-io-engine/src/lifecycle.rs', '        // 2. The header, for tools that list regions from headers.', '        drop(meta);\n        // 2. The header, for tools that list regions from headers.'),
 # G1: a flush leader wakes a word only when a sleeper is registered on it
 ('crates/store-io-engine/src/domain/mod.rs', '        if st.shared > 0 {', '        if st.shared > 1 {', DOMAIN),
 ('crates/store-io-engine/src/domain/mod.rs', '    fn poison_unknown_flush(&self, slot: u32) {\n        let _first_cause_kept = self.poison.set(FirstCause {', '    fn poison_unknown_flush(&self, slot: u32) {\n        let _first_cause_kept = Some(FirstCause {'),
 # The pool's LIFO free list: a head without its tag lets a pop install a
 # stale `next` after another thread popped and pushed back (ABA), handing
 # out a buffer that is still held; a push that publishes its value without
 # release lets a popper read its `next` before it was written
 (STACK, '    (tag << TOP_BITS) | u64::from(top_plus_one)', '    let _ = tag;\n    u64::from(top_plus_one)', LOOM_BUF, LOOM),
 (STACK, '                pack(tag.wrapping_add(1), value + 1),\n                Ordering::Release,', '                pack(tag.wrapping_add(1), value + 1),\n                Ordering::Relaxed,', LOOM_BUF, LOOM),
 # a pop that reads the head without acquiring it can read the `next` of
 # an entry from before that entry was pushed
 (STACK, '        let mut head = self.head.load(Ordering::Acquire);\n        loop {\n            let (tag, top) = unpack(head);\n            let index', '        let mut head = self.head.load(Ordering::Relaxed);\n        loop {\n            let (tag, top) = unpack(head);\n            let index', LOOM_BUF, LOOM),
 (STACK, '                pack(tag.wrapping_add(1), below),\n                Ordering::Acquire,\n                Ordering::Acquire,', '                pack(tag.wrapping_add(1), below),\n                Ordering::Acquire,\n                Ordering::Relaxed,', LOOM_BUF, LOOM),
 # The bounded queue (public): a push that publishes its slot without
 # release lets a popper read the value before it was written
 ('crates/store-io-buf/src/queue.rs', '                        cell.seq.store(pos.wrapping_add(1), Ordering::Release);', '                        cell.seq.store(pos.wrapping_add(1), Ordering::Relaxed);', LOOM_BUF, LOOM),
 # Format 2 slots (v0.5): a slot of another format than the container's
 # is refused; a numbered object needs meta_seq >= 1 and any other 0; a
 # format-2 header is exactly 128 bytes
 (SLOT, '    if major != e.format_major || !(major == FORMAT_1 || major == FORMAT_2) {', '    if !(major == FORMAT_1 || major == FORMAT_2) {', FORMAT),
 (SLOT, '        (FORMAT_2, true) => meta_seq >= 1,', '        (FORMAT_2, true) => true,', FORMAT),
 (SLOT, '        _ => meta_seq == 0,', '        _ => true,', FORMAT),
 (SLOT, '        || (major == FORMAT_2 && header_len != HEADER_LEN)', '        || false', FORMAT),
 (SLOT, '    if major != e.format_major || !(major == FORMAT_1 || major == FORMAT_2) {', '    if major != e.format_major {', FORMAT),
 # Format 2 payloads (v0.5): page checks (seqs below next_seq and unique,
 # the page index, exact lengths), entry checks (headers past the table
 # area and inside the span, two distinct header blocks, footprint_seq at
 # most its page copy's, data_size at most capacity), region header extents
 # (inside the file, non-empty, exact list length), the volume geometry
 (META2, '        if seq >= header.next_seq {', '        if seq > header.next_seq {', FORMAT),
 (META2, '        if chunks().skip(i + 1).any(|d| seq_at(d) == seq) {', '        if chunks().skip(i + 1).any(|d| seq_at(d) == seq && false) {', FORMAT),
 (META2, '    if header.page_index != page_index {', '    if header.page_index != page_index && false {', FORMAT),
 (META2, '    if bytes.len() != PAGE_HEADER_LEN + len {', '    if bytes.len() < PAGE_HEADER_LEN + len {', FORMAT),
 (META2, '            if h < lim.table_end || h < self.span_start || end > self.span_end {', '            if h < self.span_start || end > self.span_end {', FORMAT),
 (META2, '        if self.header_a == self.header_b {', '        if false {', FORMAT),
 (META2, '        if self.footprint_seq == 0 || self.footprint_seq > lim.page_meta_seq {', '        if self.footprint_seq == 0 {', FORMAT),
 (META2, '        if self.data_size > self.capacity {', '        if false {', FORMAT),
 (META2, '            Some(end) if end <= file_len => {}', '            Some(end) if end <= file_len || true => {}', FORMAT),
 (META2, '        if len == 0 || off % block != 0 || len % block != 0 {', '        if off % block != 0 || len % block != 0 {', FORMAT),
 (META2, '        if tail.len() != usize::from(extent_count) * EXTENT_LEN {', '        if tail.len() < usize::from(extent_count) * EXTENT_LEN {', FORMAT),
 (META2, '    if entries != entries_per_page(base.log2_table_slot_size) {', '    if false {', FORMAT),
 (META2, '    if table_pages == 0 || table_pages >= MAX_TABLE_PAGES {', '    if table_pages == 0 {', FORMAT),
 # region header bounds (extent count, slot object size, extent offsets
 # aligned) and an entry's agreement with its header (at most one extent
 # ahead; data_size in the header's blocks)
 (META2, '            || usize::from(extent_count) > max_extents(log2_block)', '            || false', FORMAT),
 (META2, '        if tail.len() != object_len as usize || tail.len() > max_object(log2_block) {', '        if tail.len() != object_len as usize {', FORMAT),
 (META2, '        if len == 0 || off % block != 0 || len % block != 0 {', '        if len == 0 || len % block != 0 {', FORMAT),
 (META2, '        if have < want || have > want + more {', '        if have < want || more > 1 {', FORMAT),
 (META2, '        if self.data_size % (1u64 << h.log2_block.clamp(9, 16)) != 0 {', '        if self.data_size % (1u64 << h.log2_block.clamp(9, 16)) != 0 && false {', FORMAT),
 # an entry's capacity in whole blocks; a slot entry has no capacity; a
 # slot region's header is never one extent ahead
 (META2, '        if self.capacity % block != 0 || self.data_size % unit != 0 {', '        if self.capacity % unit != 0 || self.data_size % unit != 0 {', FORMAT),
 (META2, '            if self.extent_count != 0 || self.capacity != 0 || self.data_size != 0 {', '            if self.extent_count != 0 || self.data_size != 0 {', FORMAT),
 (META2, '        let more = if self.kind == RegionKind::Slot { 0 } else { 1 };', '        let more = if self.kind == RegionKind::Slot { 1 } else { 1 };', FORMAT),
 # The format-2 tear judgement: blank opposite a later generation is
 # damage; a candidate is the next generation chained to the winner; an
 # older copy and the predecessor are only reported; a candidate's number
 # is one above every other numbered slot (damaged ones included); refused
 # and forked pairs are lost; a clean close, two candidates or a damaged
 # volume record leave nothing in flight; a damaged volume record with a
 # page candidate makes the open read-only
 (JUDGE, '                && n.prev_header_crc32c == winner.header_crc32c', '                && true', FORMAT),
 (JUDGE, '            Some(n) if n.generation < g => Opposite::OlderDamaged,', '            Some(n) if n.generation <= g + 1 => Opposite::OlderDamaged,', FORMAT),
 (JUDGE, '            Some(f) if g >= 2 && f.stored_header_crc32c == winner.prev_header_crc32c => {', '            Some(f) if f.stored_header_crc32c == winner.prev_header_crc32c => {', FORMAT),
 (JUDGE, '                            Some(m) => others_max(Some(m)).checked_add(1) == Some(m),', '                            Some(m) => others_max(Some(m)).checked_add(1) != Some(0),', FORMAT),
 (JUDGE, '        Some(v) if v == max1 && max1_count == 1 => max2,', '        Some(v) if v == max1 => max2,', FORMAT),
 (JUDGE, '        SlotStatus::BadPayloadCrc => s.facts.and_then(|f| f.numbered).map(|n| n.meta_seq),', '        SlotStatus::BadPayloadCrc => None,', FORMAT),
 (JUDGE, '        if refused(&p.a.status) || refused(&p.b.status) {', '        if refused(&p.a.status) && refused(&p.b.status) {', FORMAT),
 (JUDGE, '                if a.generation == b.generation {', '                if a.generation == b.generation && false {', FORMAT),
 (JUDGE, '                        page_had_candidate |= i > 0;', '                        page_had_candidate |= i > 99;', FORMAT),
 # (judgement, after its review) only slot B may be blank opposite a
 # first write; a header-torn copy with no facts is damage; a refused pair
 # is refused, not lost; damage anywhere blocks healing; clean needs a
 # confirmed volume record with no candidate; refused, lost and damaged
 # volume records forbid writing
 (JUDGE, '        SlotStatus::Blank if g == 1 && o_is_b => Opposite::Nothing,', '        SlotStatus::Blank if g == 1 && (o_is_b || true) => Opposite::Nothing,', FORMAT),
 (JUDGE, '        SlotStatus::Blank if g == 1 && o_is_b => Opposite::Nothing,', '        SlotStatus::Blank if o_is_b => Opposite::Nothing,', FORMAT),
 (JUDGE, '            None => Opposite::Damage,', '            None => Opposite::Candidate(None),', FORMAT),
 (JUDGE, '            *o = PairVerdict::REFUSED;', '            *o = PairVerdict { standing: Standing::Lost, ..PairVerdict::REFUSED };', FORMAT),
 (JUDGE, '    let troubled = out.iter().any(|v| v.standing != Standing::Confirmed);', '    let troubled = out.iter().any(|v| v.standing == Standing::Refused);', FORMAT),
 (JUDGE, '    let clean = out[0].standing == Standing::Confirmed && !volume_has_candidate && clean_close;', '    let clean = out[0].standing == Standing::Confirmed && (!volume_has_candidate || true) && clean_close;', FORMAT),
 (JUDGE, '    let clean = out[0].standing == Standing::Confirmed && !volume_has_candidate && clean_close;', '    let clean = !volume_has_candidate && clean_close;', FORMAT),
 (JUDGE, '    let writable = !refused\n        && !volume_lost\n', '    let writable = !volume_lost\n', FORMAT),
 (JUDGE, '        && !volume_lost\n        && (out[0].standing', '        && (!volume_lost || true)\n        && (out[0].standing', FORMAT),
 (JUDGE, '        && (out[0].standing != Standing::Unconfirmed || !page_had_candidate);', '        && (out[0].standing != Standing::Unconfirmed || !page_had_candidate || true);', FORMAT),
 # the trouble rule applies to page candidates only; a volume candidate
 # heals unless another candidate or a refused pair exists
 (JUDGE, '    let page_demote = clean || candidates != 1 || troubled;', '    let page_demote = candidates != 1 || troubled;', FORMAT),
 (JUDGE, '    let page_demote = clean || candidates != 1 || troubled;', '    let page_demote = clean || candidates > 99 || troubled;', FORMAT),
 (JUDGE, '    let page_demote = clean || candidates != 1 || troubled;', '    let page_demote = clean || candidates != 1 || troubled && false;', FORMAT),
 (JUDGE, '    let volume_demote = candidates != 1 || refused;', '    let volume_demote = candidates > 99 || refused;', FORMAT),
 (JUDGE, '    let volume_demote = candidates != 1 || refused;', '    let volume_demote = candidates != 1 || refused && false;', FORMAT),
 (JUDGE, '                let demote = if i == 0 { volume_demote } else { page_demote };', '                let demote = if i == 0 { page_demote || volume_demote } else { page_demote };', FORMAT),
 # E1: a region range outside its map is refused, in release builds too
 ('crates/store-io-engine/src/regionmap.rs', '            pos.checked_add(len).is_some_and(|end| end <= data),', '            pos.checked_add(len).is_some_and(|end| end <= data) || true,'),
 # E2a: create makes every page durable before the volume record and
 # numbers them in order; the volume record is numbered
 (V2, '    }\n    full_flush(platform, file)?;\n    let vm = VolumeMeta2 {', '    }\n    let vm = VolumeMeta2 {', F2),
 (V2, '        next.meta_seq = u64::from(p) + 1;', '        next.meta_seq = u64::from(p) + 2;', F2),
 (V2, '    next.meta_seq = u64::from(layout.pages) + 1;', '    next.meta_seq = u64::from(layout.pages);', F2),
 ('crates/store-io-engine/src/store.rs', '        numbered: l.format == store_io_format::slot::FORMAT_2,', '        numbered: false,', F2),
 # the open: a short file is corrupt; a refused pair, and a page holding
 # regions (until E3), fail it; a page that does not decode is lost; the
 # judgement sees each copy's facts; stale and lost pages freeze; an
 # unwritable judgement opens read-only; a heal keeps the torn copy's
 # meta_seq; an open records an unclean store; a read-only store never
 # writes at close
 (V2, '    if layout.data_start() > container_len {', '    if layout.data_start() > container_len && false {', F2),
 (V2, '    if j.refused {', '    if j.refused && false {', F2),
 (V2, '                    standing = Standing::Lost;', '                    standing = Standing::Unconfirmed;', F2),
 (V2, '            facts: p.facts[1],', '            facts: None,', F2),
 (V2, '    report.frozen = !report.stale_pages.is_empty() || !report.lost_pages.is_empty();', '    report.frozen = !report.stale_pages.is_empty();', F2),
 (V2, '    report.frozen = !report.stale_pages.is_empty() || !report.lost_pages.is_empty();', '    report.frozen = !report.lost_pages.is_empty();', F2),
 (V2, '    let read_only = read_only || {', '    let read_only = read_only && {', F2),
 (V2, '        Some(m) => m,', '        Some(m) => m + 1,', F2),
 (V2, '        q, file, pool, layout, volume, policy, &domain, &mut meta, og, false,', '        q, file, pool, layout, volume, policy, &domain, &mut meta, og, true,', F2),
 (V2, '        if self.read_only\n            || self.domain', '        if self.domain', F2),
 (V2, '            || std::thread::panicking()\n', '', F2),
 # a newer volume record is refused, not corrupt; format 2 regions wait
 # for E3
 (V2, 'Field("features" | "flags") => Error::Unsupported {', 'Field("features") => Error::Unsupported {', F2),
 ('crates/store-io-engine/src/provision.rs', '        if inner.layout.format == store_io_format::slot::FORMAT_2 {', '        if inner.layout.format == 99 {', F2),
 # create takes over only a crashed format-2 create: the leftover rule is
 # format 2's; generation 1 only; a valid volume record is a store; a torn
 # page is a leftover; a file past the metadata is not
 (CREATE, '            _ if v2.is_some_and(|t| crashed_create_leftover(b.as_slice(), at, t)) => {', '            _ if v2.or(Some(14)).is_some_and(|t| crashed_create_leftover(b.as_slice(), at, t)) => {', F2),
 (CREATE, '        || u64_at(56) != 1\n', '        || u64_at(56) == 0\n', F2),
 (CREATE, '        return torn && at == 0;', '        return true && at == 0;', F2),
 (CREATE, '        && (torn\n            || store_io_format::meta2::decode_page', '        && (false\n            || store_io_format::meta2::decode_page', F2),
 (CREATE, '        let largest_metadata = if v2 {', '        let largest_metadata = if v2 && false {', F2),
 # review: the format is read only from a verifying header; a failed
 # metadata read is retried once; a page failing its checks stops a page
 # heal; an unknown ro_compat bit opens read-only; clean_close is the
 # judged one; a torn copy of unknown meta_seq heals at the next number
 ('crates/store-io-engine/src/open.rs', '            if !header_verifies(b.as_slice()) {', '            if !header_verifies(b.as_slice()) && false {', F2),
 ('crates/store-io-engine/src/slots.rs', '    if !failed(first.report.a) && !failed(first.report.b) {', '    if true {', F2),
 (V2, '    if reread {', '    if reread && false {', F2),
 (V2, '        if pair > 0 && (decode_lost || pages[pair - 1].ahead().next().is_some()) {', '        if pair > 0 && (decode_lost && false || pages[pair - 1].ahead().next().is_some()) {', F2),
 (V2, '        .any(|p| p.report.winner_info().is_some_and(|(_, i)| !i.writable()));', '        .any(|p| p.report.winner_info().is_some_and(|(_, i)| !i.writable() && false));', F2),
 (V2, '        clean_close: j.clean,', '        clean_close: vm.clean_close,', F2),
 (V2, '        None => next_meta_seq(&meta.volume_pair, &v2.pages, Some((pair, slot)))?,', '        None => next_meta_seq(&meta.volume_pair, &v2.pages, Some((pair, slot)))? + 1,', F2),
 # second review: a retry reads only the failed slot and keeps the other's
 # result; a longer volume record is refused; the file grows past page 0
 # only once page 0 is durable; past format 1's bound a takeover needs a
 # format-2 leftover
 ('crates/store-io-engine/src/slots.rs', '        if failed(status[i]) {', '        if true {', F2),
 ('crates/store-io-engine/src/slots.rs', '            if failed([first.report.a, first.report.b][i]) {', '            if true {', F2),
 (V2, '    if bytes.len() > VOLUME_META2_LEN {', '    if bytes.len() > VOLUME_META2_LEN && false {', F2),
 (V2, '        layout.page_slots(0).1\n    })?;', '        layout.data_start()\n    })?;', F2),
 (V2, '        if p == 1 {\n            full_flush(platform, file)?;', '        if p == 1 {', F2),
 (CREATE, '        if holds != Some(false) || (len > v1_metadata && !leftovers) {', '        if holds != Some(false) {', F2),
 (CREATE, '                *leftovers = true;', '                *leftovers = false;', F2),
 # third review: a newer volume copy read on the retry is refused; a
 # one-page table is sized whole; the size bound before the probe covers
 # every block size
 (V2, '        vm = decode_volume(&crate::open::winner_payload(&volume_pair)?)?;', '        vm = decode_volume(volume_pair.payload.as_deref().ok_or_else(corrupt)?)?;', F2),
 (V2, '    allocate(if layout.pages == 1 {', '    allocate(if false {', F2),
 (CREATE, '            (crate::layout::MIN_LOG2_BLOCK..=crate::layout::MAX_LOG2_BLOCK)\n                .map(', '            (crate::layout::MAX_LOG2_BLOCK..=crate::layout::MAX_LOG2_BLOCK)\n                .map(', F2),
 # D-124 flushes in flight: done advances only over an unbroken run of
 # successes; a main-handle barrier leads on slot 0 only; nothing is
 # recorded once poisoned; the issue window bounds the success mask; a
 # read-only store opens no flush handles; a lane without a handle closes
 ('crates/store-io-engine/src/domain/mod.rs', '        while st.ahead & 1 == 1 {', '        while st.ahead != 0 {'),
 ('crates/store-io-engine/src/domain/mod.rs', '            let mut free = (!st.busy) & ((1u32 << usable) - 1);', '            let mut free = (!st.busy) & ((1u32 << self.slots.max(usable)) - 1);'),
 ('crates/store-io-engine/src/domain/mod.rs', '                None if self.poison.get().is_none() => self.record(&mut st, k),', '                None if true => self.record(&mut st, k),'),
 ('crates/store-io-engine/src/domain/mod.rs', '            if issued >= need || free == 0 || issued - done >= WINDOW {', '            if issued >= need || free == 0 || issued - done >= 4 * WINDOW {'),
 ('crates/store-io-engine/src/open.rs', '        let lanes = if read_only || !policy.flush {\n            1\n        } else {\n            queues.flush_lanes()', '        let lanes = if !policy.flush {\n            1\n        } else {\n            queues.flush_lanes()'),
 ('crates/store-io-engine/src/open.rs', '        queues.keep_flush_lanes(1 + flush_files.len());', '        queues.keep_flush_lanes(lanes);'),
 ('crates/store-io-engine/src/open.rs', '        queues.keep_flush_lanes(1 + flush_files.len());', '        queues.keep_flush_lanes(queues.flush_lanes());'),
 # E2b/E3 C1: a format-2 region's header object is 2^62 | id
 ('crates/store-io-engine/src/layout.rs', '            (1u64 << 62) | id', '            (1u64 << 32) | id'),
 # E2b/E3 C5 review: the entry names its own page write; that write's
 # meta_seq is the one chosen first (a torn one heals); free bytes leave
 # reservations out
 ('crates/store-io-engine/src/provision2.rs', '            footprint_seq: meta_seq,', '            footprint_seq: 1,', F2P),
 ('crates/store-io-engine/src/v2.rs', '    let mut next = plan_next(&page.pair, payload, owner_generation)?;\n    next.meta_seq = meta_seq;', '    let mut next = plan_next(&page.pair, payload, owner_generation)?;\n    next.meta_seq = meta_seq + 1;', F2P),
 ('crates/store-io-engine/src/space.rs', '                released: v2.free.free_bytes().saturating_sub(acct.reserved()),', '                released: v2.free.free_bytes(),', F2P),
 # E4 C8: growth within capacity: the fill flushed and verified before the
 # commit; the capacity bound; published after the commit (the map, then
 # the frontier's limit); the fill keyed by region offset; the usage and
 # accounting follow
 ('crates/store-io-engine/src/grow2.rs', '        self.flush_all_accounted()?;\n        for &(file, _, n) in pieces {', '        for &(file, _, n) in pieces {', F2G),
 ('crates/store-io-engine/src/grow2.rs', '        for &(file, _, n) in pieces {\n            self.verify_ready(file, n)?;', '        for &(file, _, n) in pieces {\n            let _ = (file, n);', F2G),
 ('crates/store-io-engine/src/grow2.rs', '        if new > r.map.capacity() {\n            return self.grow_beyond2(', '        if new >= r.map.capacity() {\n            return self.grow_beyond2(', F2G),
 ('crates/store-io-engine/src/grow2.rs', '        // 2. The commit: the page with the entry\'s new data size.', '        r.map.publish_len(new);\n        // 2. The commit: the page with the entry\'s new data size.', F2G),
 ('crates/store-io-engine/src/grow2.rs', '        if let Some(f) = &r.frontier {\n            f.raise_limit(new);\n        }\n        drop(header);', '        if let Some(f) = r.frontier.as_ref().filter(|_| false) {\n            f.raise_limit(new);\n        }\n        drop(header);', F2G),
 ('crates/store-io-engine/src/grow2.rs', 'self.fill_range(file, n, 0, fill, key, at)', 'self.fill_range(file, n, 0, fill, key, file + (at & 0))', F2G),
 ('crates/store-io-engine/src/grow2.rs', '*u = (old.tag, new, physical.saturating_add(extra));', '*u = (old.tag, old.data_size, physical.saturating_add(extra));', F2G),
 ('crates/store-io-engine/src/grow2.rs', '        acct.add_usage(old.tag, new, 0);', '        acct.add_usage(old.tag, old.data_size, 0);', F2G),
 ('crates/store-io-engine/src/grow2.rs', 'the map\'s length is the entry\'s");\n            e.data_size = new;', 'the map\'s length is the entry\'s");\n            e.data_size = len;', F2G),
 # C8 review: refused while frozen (stricter than the spec)
 ('crates/store-io-engine/src/grow2.rs', '        if inner.frozen.load(std::sync::atomic::Ordering::Acquire) {', '        if inner.frozen.load(std::sync::atomic::Ordering::Acquire) && false {', F2G),
 # E4 C13: provisioning and growth beyond capacity fill outside the
 # metadata lock: the space marked (and charged, a reservation consumed)
 # before, given back on a failure before the commit, settled after; a
 # provisioning's name and table slot claimed; the commit takes the page
 # as it is then
 ('crates/store-io-engine/src/grow2.rs', '            v2.mark(&[(at, ext_len)], container);', '            let _ = (v2, container, at, ext_len);', F2CC),
 ('crates/store-io-engine/src/grow2.rs', '        lock_acct(&inner.acct).add_usage(old.tag, 0, ext_len);', '        lock_acct(&inner.acct).add_usage(old.tag, 0, ext_len & 0);', F2B),
 ('crates/store-io-engine/src/provision2.rs', '            v2.unmark(&self.ranges, container);', '            v2.unmark(&self.ranges[..0], container);', F2B),
 ('crates/store-io-engine/src/provision2.rs', '            lock_acct(&inner.acct).remove_usage(self.tag, 0, self.charged);', '            lock_acct(&inner.acct).remove_usage(self.tag, 0, self.charged & 0);', F2B),
 ('crates/store-io-engine/src/grow2.rs', '            v2.settle(ext_len);', '            v2.settle(ext_len & 0);', F2CC),
 ('crates/store-io-engine/src/provision2.rs', '                    let seq = held.map(|c| c.seq + 1).fold(h.next_seq, u32::max);', '                    let seq = held.map(|c| c.seq + 1).fold(h.next_seq, |a, _| a);', F2CC),
 ('crates/store-io-engine/src/provision2.rs', '                        && usize::from(h.count) + claimed < usize::from(per_page)', '                        && usize::from(h.count) + (claimed & 0) < usize::from(per_page)', F2CC),
 ('crates/store-io-engine/src/provision2.rs', '            v2.mark(&footprint, container);', '            let _ = (&footprint, container);', F2CC),
 ('crates/store-io-engine/src/provision2.rs', '                v2.claims.retain(|c| !(c.page == page && c.seq == seq));', '                v2.claims.retain(|c| c.page != usize::MAX || (page, seq) != (c.page, c.seq));', F2CC),
 ('crates/store-io-engine/src/provision2.rs', '            r.unhold(bytes);', '            r.unhold(bytes & 0);', F2CC),
 ('crates/store-io-engine/src/provision2.rs', '                next_seq: ph.next_seq.max(pl.seq + 1),', '                next_seq: ph.next_seq.min(0).max(pl.seq + 1),', F2CC),
 ('crates/store-io-engine/src/provision2.rs', '            v2.settle(pl.extent);', '            v2.settle(pl.extent & 0);', F2CC),
 ('crates/store-io-engine/src/provision2.rs', '                v.reserved_names.contains(name) || v.claims.iter().any(|c| c.name == *name)', '                v.reserved_names.contains(name) || v.claims.iter().any(|c| c.name == *name && false)', F2CC),
 ('crates/store-io-engine/src/provision2.rs', '                acct.unreserve(pl.tag, pl.extent);', '                acct.unreserve(pl.tag, pl.extent & 0);', F2CC),
 ('crates/store-io-engine/src/provision2.rs', '        holds.armed = false;\n        if let Some(v2) = meta.v2.as_deref_mut() {\n            v2.settle(pl.extent);', '        holds.armed = true;\n        if let Some(v2) = meta.v2.as_deref_mut() {\n            v2.settle(pl.extent);', F2CC),
 ('crates/store-io-engine/src/grow2.rs', '        holds.armed = false;', '        holds.armed = true;', F2B),
 # E4 C12: a format-2 region that may grow has a ring of the window's
 # size (a small region's ring held a grown one to a tenth of the rate)
 ('crates/store-io-engine/src/frontier.rs', '    if flags & store_io_format::meta2::FLAG_CAPPED != 0 {\n        capacity', '    if flags & store_io_format::meta2::FLAG_CAPPED != 0 || flags == 0 {\n        capacity', FRONTIER),
 # E4 C11: store-wide region defaults, used by the _default calls,
 # refused when unset
 ('crates/store-io-engine/src/space.rs', '        let opts = self.store.region_defaults()?;\n        self.provision_append_region_with(name, opts)', '        let opts = self.store.region_defaults().map(|_| RegionOptions::default())?;\n        self.provision_append_region_with(name, opts)', F2OPT),
 ('crates/store-io-engine/src/region.rs', '        self.provision_append_region_with(name, self.region_defaults()?)', '        self.provision_append_region_with(name, self.region_defaults().map(|_| RegionOptions::default())?)', F2OPT),
 ('crates/store-io-engine/src/region.rs', '        self.provision_page_region_with(name, self.region_defaults()?)', '        self.provision_page_region_with(name, self.region_defaults().map(|_| RegionOptions::default())?)', F2OPT),
 ('crates/store-io-engine/src/region.rs', '        self.inner.opts.region_defaults.ok_or(Error::NotWritten {', '        Some(self.inner.opts.region_defaults.unwrap_or_default()).ok_or(Error::NotWritten {', F2OPT),
 # E4 C10: a capped region is one extent of its cap, flagged, charged
 # whole; its slack allocated unwritten, or written where writes_slack
 # (Windows, macOS); validated before any I/O
 ('crates/store-io-engine/src/provision2.rs', '        let flags = if capped.is_some() { FLAG_CAPPED } else { 0 };', '        let flags = if capped.is_some() { FLAG_CAPPED & 0 } else { 0 };', F2OPT),
 ('crates/store-io-engine/src/provision2.rs', '            fill_whole: capped.is_none() || self.writes_slack(),', '            fill_whole: capped.is_none(),', F2OPT),
 ('crates/store-io-engine/src/provision.rs', '            store_io_core::evidence::PlatformKind::Windows\n                | store_io_core::evidence::PlatformKind::MacOs', '            store_io_core::evidence::PlatformKind::Windows', F2OPT),
 ('crates/store-io-engine/src/provision2.rs', '        if filled < pl.extent {', '        if filled < pl.extent && false {', F2OPT),
 ('crates/store-io-engine/src/provision2.rs', '        self.verify_ready(pl.offset, filled)?;', '        self.verify_ready(pl.offset, pl.extent.max(filled))?;', F2OPT),
 ('crates/store-io-engine/src/provision2.rs', '                .ok_or_else(too_large)?\n                .max(data_size),', '                .ok_or_else(too_large)?\n                .min(data_size),', F2OPT),
 ('crates/store-io-engine/src/provision2.rs', '            capacity: pl.capacity,\n            span_start: pl.offset,', '            capacity: pl.data_size + (pl.capacity & 0),\n            span_start: pl.offset,', F2OPT),
 ('crates/store-io-engine/src/provision.rs', '        opts.validate()?;', '        let _ = opts.validate();', F2OPT),
 ('crates/store-io-engine/src/provision.rs', '        if opts.max_size_bytes().is_some() {\n            return Err(Error::Unsupported {', '        if opts.max_size_bytes().is_some() && false {\n            return Err(Error::Unsupported {', F2OPT),
 ('crates/store-io-engine/src/options.rs', '            Some(cap) if cap < self.size => Err(Error::NotWritten {', '            Some(cap) if cap < self.size && false => Err(Error::NotWritten {', F2OPT),
 ('crates/store-io-engine/src/provision2.rs', '        lock_acct(&inner.acct).add_usage(tag, 0, extent);', '        lock_acct(&inner.acct).add_usage(tag, 0, 2 * block + data_size + (extent & 0));', F2OPT),
 ('crates/store-io-sim/src/world.rs', '        let gap = asked.0.saturating_sub(f.valid_data);\n        f.valid_data = f.valid_data.max(asked.0.saturating_add(len as u64));', '        let gap = asked.0.saturating_sub(f.valid_data);\n        f.valid_data = f.valid_data.max(asked.0);', ['-p', 'store-io-sim', '--test', 'model']),
 # E4 C9: growth beyond capacity. Refusals before any I/O (capped, a
 # full extent list, the tag's cap); at least 1 MiB; after the last
 # extent when free or the tail, else first fit; the slack allocated
 # (Windows: the whole extent filled); the header lists the extent
 # before the page commits every entry field; memory after the commit
 # (the extent allocated, the usage, the run, the length); running out
 # of space is NoSpace(Growth)
 ('crates/store-io-engine/src/grow2.rs', '        if old.flags & FLAG_CAPPED != 0 {', '        if old.flags & FLAG_CAPPED != 0 && false {', F2B),
 ('crates/store-io-engine/src/grow2.rs', '        if usize::from(old.extent_count) >= max_extents(', '        if usize::from(old.extent_count) > max_extents(', F2B),
 ('crates/store-io-engine/src/grow2.rs', '        if !lock_acct(&inner.acct).fits(old.tag, ext_len) {', '        if !lock_acct(&inner.acct).fits(old.tag, ext_len) && false {', F2B),
 ('crates/store-io-engine/src/grow2.rs', 'align_up(MIN_EXTENT.max(old_cap / 8)', 'align_up((MIN_EXTENT & 0).max(old_cap / 8)', F2B),
 ('crates/store-io-engine/src/grow2.rs', '            let at = if v2.free.fits_at(last_end, ext_len) || last_end == v2.free.end() {', '            let at = if v2.free.fits_at(last_end, ext_len) && false {', F2B),
 ('crates/store-io-engine/src/grow2.rs', '            let at = if v2.free.fits_at(last_end, ext_len) || last_end == v2.free.end() {', '            let at = if (v2.free.fits_at(last_end, ext_len) && false) || last_end == v2.free.end() {', F2B),
 ('crates/store-io-engine/src/grow2.rs', '            e.data_size = g.new;\n            e.capacity = g.capacity;', '            e.data_size = g.len;\n            e.capacity = g.capacity;', F2B),
 ('crates/store-io-engine/src/grow2.rs', '            e.capacity = g.capacity;', '            e.capacity = g.old_cap + (g.capacity & 0);', F2B),
 ('crates/store-io-engine/src/grow2.rs', '            e.extent_count += 1;', '            e.extent_count += 0;', F2B),
 ('crates/store-io-engine/src/grow2.rs', '            e.span_start = e.span_start.min(at);', '            e.span_start = e.span_start.min(at | u64::MAX);', F2B),
 ('crates/store-io-engine/src/grow2.rs', '            e.span_end = e.span_end.max(at + ext_len);', '            e.span_end = e.span_end.max(at & 0);', F2B),
 ('crates/store-io-engine/src/grow2.rs', '            e.footprint_seq = meta_seq;', '            e.footprint_seq = e.footprint_seq.max(meta_seq & 0);', F2B),
 ('crates/store-io-engine/src/grow2.rs', '        let filled = if self.writes_slack() {', '        let filled = if self.writes_slack() && false {', F2B),
 ('crates/store-io-engine/src/grow2.rs', '        if filled < ext_len {', '        if filled < ext_len && false {', F2B),
 ('crates/store-io-engine/src/grow2.rs', '        pieces.push((at, g.old_cap, filled));', '        pieces.push((at, g.old_cap, filled.min(0)));', F2B),
 ('crates/store-io-engine/src/grow2.rs', '            RegionState::Ready,\n            0,\n            committed,\n            Some((at, ext_len)),', '            RegionState::Ready,\n            1,\n            committed,\n            Some((at, ext_len)),', F2B),
 ('crates/store-io-engine/src/grow2.rs', '            Some((at, ext_len)),\n        )?;', '            Some((at, ext_len)).filter(|_| false),\n        )?;', F2B),
 ('crates/store-io-engine/src/grow2.rs', '        self.count_growth(&mut meta, &g.old, g.new, ext_len);', '        self.count_growth(&mut meta, &g.old, g.new, 0);', F2B),
 ('crates/store-io-engine/src/grow2.rs', '            tag: ReserveTag::Growth,', '            tag: ReserveTag::Caller(0),', F2B),
 ('crates/store-io-engine/src/grow2.rs', '        if listed > committed {', '        if listed > committed && false {', F2B),
 ('crates/store-io-engine/src/grow2.rs', '        r.map.extend(at, ext_len);', '        r.map.extend(at, 0);', F2B),
 ('crates/store-io-engine/src/grow2.rs', '        if let Some(f) = &r.frontier {\n            f.raise_limit(new);\n        }\n        Ok(new)', '        if let Some(f) = r.frontier.as_ref().filter(|_| false) {\n            f.raise_limit(new);\n        }\n        Ok(new)', F2B),
 ('crates/store-io-engine/src/open2.rs', '            if let Some((off, len)) = c.extra {', '            if let Some((off, len)) = c.extra.filter(|_| false) {', F2B),
 ('crates/store-io-engine/src/lifecycle2.rs', '        extents.extend(append);', '        extents.extend(append.filter(|_| false));', F2B),
 ('crates/store-io-engine/src/footprint.rs', '            .is_some_and(|end| self.free.iter().any(|&(s, e)| s <= at && end <= e))', '            .is_some_and(|end| self.free.iter().any(|&(s, e)| s <= at && end < e))', FOOTPRINT),
 ('crates/store-io-engine/src/regionmap.rs', '        if at.checked_add(cap - start) != Some(file) {', '        if at.checked_add(cap - start) != Some(file) || true {', REGIONMAP),
 ('crates/store-io-engine/src/regionmap.rs', 'get_or_init(|| slots(1 << c))', 'get_or_init(|| slots((1 << c) / 2 + 1))', REGIONMAP),
 ('crates/store-io-engine/src/regionmap.rs', '            s.start.store(cap, Ordering::Relaxed);\n            s.file.store(file, Ordering::Relaxed);\n            self.count.store(count as u64 + 1, Ordering::Release);', '            self.count.store(count as u64 + 1, Ordering::Release);\n            s.start.store(cap, Ordering::Relaxed);\n            s.file.store(file, Ordering::Relaxed);', LOOM_ENGINE, LOOM),
 # E4 C7: the completion ring allocated at the first completion, never at
 # construction (outside loom); a resume_at that moves the tail has the
 # ring through get_or_init (a plain look races a first completion)
 ('crates/store-io-engine/src/frontier.rs', '        #[cfg(loom)]\n        let _set = cell.set(slots(ring));', '        #[cfg(not(loom))]\n        let _set = cell.set(slots(ring));', FRONTIER),
 ('crates/store-io-engine/src/frontier.rs', '        let _moved = self.advance(self.ring());', '        let _moved = self.ring.get().is_some_and(|r| self.advance(r));', FRONTIER),
 # E4 C6: allocate_range inside the file only, at its own range (Linux);
 # the simulator's holes (punched by a deallocation, filled by a write) and
 # its bounds
 ('crates/store-io-posix/src/alloc.rs', '    if end > size(file)? {\n        return Err(sys::errno(libc::EINVAL));\n    }\n    if len == 0 {', '    if end > size(file)? && false {\n        return Err(sys::errno(libc::EINVAL));\n    }\n    if len == 0 {', POSIX),
 ('crates/store-io-posix/src/alloc.rs', '    sys::fallocate(file.raw(), libc::FALLOC_FL_KEEP_SIZE, off, l)\n}', '    sys::fallocate(file.raw(), libc::FALLOC_FL_KEEP_SIZE, off + 4096, l)\n}', POSIX),
 ('crates/store-io-posix/src/alloc.rs', '    if len == 0 {\n        return Ok(());\n    }\n    let off', '    if len == 0 && false {\n        return Ok(());\n    }\n    let off', POSIX),
 ('crates/store-io-sim/src/world.rs', '        unmark(&mut f.holes, asked.0, asked.0.saturating_add(len as u64));', '        let _ = asked;', ENGINE),
 ('crates/store-io-sim/src/world.rs', '        if hole {\n            mark(&mut f.holes, s as u64, e as u64);', '        if hole && false {\n            mark(&mut f.holes, s as u64, e as u64);', ENGINE),
 ('crates/store-io-sim/src/world.rs', '        if end > f.visible.len() as u64 {\n            return Err(EINVAL);\n        }\n        Ok(())', '        if end > f.visible.len() as u64 && false {\n            return Err(EINVAL);\n        }\n        Ok(())', ENGINE),
 # C6 review: a trim punches no hole; past the end is unwritten; growth,
 # and the gap a write past the end leaves, are holes; a lost write is
 # still written to the file system
 ('crates/store-io-sim/src/platform.rs', 'w.release(file.id, offset, len, how == ReleaseHow::Deallocate)', 'w.release(file.id, offset, len, how == ReleaseHow::Deallocate || true)', ENGINE),
 ('crates/store-io-sim/src/world.rs', 'Ok(end > f.visible.len() as u64 || f.holes', 'Ok(end > f.visible.len() as u64 && false || f.holes', ENGINE),
 ('crates/store-io-sim/src/world.rs', '                mark(&mut f.holes, cur, len);', '                let _ = cur;', ENGINE),
 ('crates/store-io-sim/src/world.rs', '            mark(&mut f.holes, old, end as u64);', '            let _ = old;', ENGINE),
 ('crates/store-io-sim/src/world.rs', '            if let Some(f) = self.files.get_mut(&file) {\n                unmark(&mut f.holes, asked.0, asked.1);', '            if let Some(f) = self.files.get_mut(&file).filter(|_| false) {\n                unmark(&mut f.holes, asked.0, asked.1);', ['-p', 'store-io-engine', '-p', 'store-io-sim']),
 # E4 C5: a Submitter's operation across runs: its first piece; the
 # rotation after each piece and at the end; settled once, over the whole
 # operation; a refused next piece settles over what was done; a refused
 # first piece gives the buffer back whole
 ('crates/store-io-engine/src/submitter/mod.rs', '    let piece = (state.map.run_left(start) as usize)\n        .min(total)\n        .min(piece_max);', '    let piece = total.min(piece_max);', F2E),
 ('crates/store-io-engine/src/optable.rs', 'b.as_mut_slice().rotate_left(piece);', 'b.as_mut_slice().rotate_left(0);', F2E),
 ('crates/store-io-engine/src/optable.rs', '                b.as_mut_slice().rotate_left(split.total - split.done);\n            }\n            result', '                b.as_mut_slice().rotate_left(0);\n            }\n            result', F2E),
 ('crates/store-io-engine/src/optable.rs', '(whole, left > 0, region(live), buf.as_mut())', '(whole, false, region(live), buf.as_mut())', F2E),
 ('crates/store-io-engine/src/optable.rs', 'result = result.map(|n| split.done + n);', 'result = result.map(|n| n.max(split.done));', F2E),
 ('crates/store-io-engine/src/optable.rs', 'raw.map_or(Ok(split.done), Err)', 'raw.map_or(Ok(split.total), |_| Ok(split.total))', F2E),
 ('crates/store-io-engine/src/submitter/mod.rs', '            if let Some(s) = split {\n                let fits = buf.set_len(s.total);', '            if let Some(s) = split.filter(|_| false) {\n                let fits = buf.set_len(s.total);', F2E),
 # C5 review: the split recorded at all; the restore after a refused next
 # piece; a read never taken for a write; three pieces (the bytes done
 # carried, and where the next piece starts)
 ('crates/store-io-engine/src/submitter/mod.rs', '        if let Some(s) = split {\n            self.core.table.set_split(index, s);', '        if let Some(s) = split.filter(|_| false) {\n            self.core.table.set_split(index, s);', F2E),
 ('crates/store-io-engine/src/optable.rs', '            b.as_mut_slice().rotate_left(split.total - split.done);\n        }\n        // A read refused', '            b.as_mut_slice().rotate_left(0);\n        }\n        // A read refused', F2E),
 ('crates/store-io-engine/src/optable.rs', 'let write = !matches!(live.kind, Kind::Read { .. });', 'let write = !matches!(live.kind, Kind::Read { .. }) || true;', F2E),
 ('crates/store-io-engine/src/optable.rs', '                    done: split.done + piece,', '                    done: piece,', F2E),
 ('crates/store-io-engine/src/optable.rs', 'let pos = start_of(live.kind) + (split.done + piece) as u64;', 'let pos = start_of(live.kind) + piece as u64;', F2E),
 # E4 C4: blocking writes split at runs (the piece, the rest moved to the
 # front, the split buffer kept until its last piece, the source waiting
 # for it)
 ('crates/store-io-engine/src/mapped.rs', 'let piece = self.map.run_left(pos).min(left as u64);', 'let piece = left as u64;', F2E),
 ('crates/store-io-engine/src/mapped.rs', '.copy_within(written..written + left, 0);', '.copy_within(written - 4096..written + left - 4096, 0);', F2E),
 ('crates/store-io-engine/src/mapped.rs', 'let Some(k) = self.parts.iter().position(|p| p.addr == addr) else {', 'let Some(k) = self.parts.iter().position(|p| p.addr == addr && false) else {', F2E),
 ('crates/store-io-engine/src/mapped.rs', 'Next::Done if !self.parts.is_empty() => Next::Wait,', 'Next::Done if false => Next::Wait,', F2E),
 # E4 C3: regions of several extents open; reads and scans stop at a
 # run's end
 ('crates/store-io-engine/src/io.rs', 'let piece = chunk.min(len - done as u64).min(r.map.run_left(at));', 'let piece = chunk.min(len - done as u64);', F2E),
 ('crates/store-io-engine/src/scan.rs', 'let len = (to - next).min(chunk as u64).min(r.map.run_left(next)) as usize;', 'let len = (to - next).min(chunk as u64) as usize;', F2E),
 ('crates/store-io-engine/src/regionmap.rs', 'let more: Vec<(u64, u64)> = all.map(|r| (r.start, r.file)).collect();', 'let more: Vec<(u64, u64)> = all.take(0).map(|r| (r.start, r.file)).collect();', F2E),
 # E4 C2: runs coalesce adjacent extents in data order; a range never
 # crosses a run; the run search; the runs cover the capacity
 ('crates/store-io-format/src/footprint.rs', '(Some(end), Some(total)) if end == next => len = total,', '(Some(end), Some(total)) if end <= next => len = total,', FOOTPRINT_FMT),
 ('crates/store-io-engine/src/regionmap.rs', '            pos + len <= end,', '            pos + len <= end || true,', REGIONMAP),
 ('crates/store-io-engine/src/regionmap.rs', '            if self.run(mid).0 <= pos {', '            if self.run(mid).0 < pos {', REGIONMAP),
 ('crates/store-io-engine/src/regionmap.rs', '            .chain(core::iter::once(cap))\n            .collect();', '            .chain(core::iter::once(self.len().min(cap | u64::MAX)))\n            .collect();', REGIONMAP),
 # E4 C1: a release tries every extent (a failed trim on Windows is no
 # error); a submitter's reservation is held under a poisoning guard until
 # its row holds it
 ('crates/store-io-engine/src/lifecycle.rs', '        for (at, len) in extents.into_iter().filter(|x| x.1 > 0) {', '        for (at, len) in extents.into_iter().filter(|x| x.1 > 0).take(1) {', ENGINE_LIB_LIFECYCLE),
 ('crates/store-io-engine/src/lifecycle.rs', '            Some(_) if windows => Ok(()),', '            Some(_) if windows && false => Ok(()),', ENGINE_LIB_LIFECYCLE),
 ('crates/store-io-engine/src/io.rs', '            store: self,\n            r: Some(r),\n        }\n    }\n\n    /// A lane for an append', '            store: self,\n            r: Some(r).filter(|_| false),\n        }\n    }\n\n    /// A lane for an append', SUBMITTER_TEST),
 # E2b/E3 C8: the open reads a pair whose B follows A in one read, every
 # other pair slot by slot, a short read never judged, a failed read of
 # both read again slot by slot, each header matched to its own entry
 ('crates/store-io-engine/src/slots.rs', '        if at.a.checked_add(at.size() as u64) == Some(at.b) {', '        if at.a.checked_add(at.size() as u64) == Some(at.b) || at.log2 > 0 {', F2O),
 ('crates/store-io-engine/src/slots.rs', '        if at.a.checked_add(at.size() as u64) == Some(at.b) {', '        if at.a.checked_add(at.size() as u64) == Some(at.b) && at.log2 == 0 {', F2O),
 ('crates/store-io-engine/src/slots.rs', '        Ok(n) if n == len => Ok(got.ok_or(media)),', '        Ok(n) if n <= len => Ok(got.ok_or(media)),', F2O),
 ('crates/store-io-engine/src/slots.rs', '                            Some(OneSlot::unreadable(media)),\n                            Some(OneSlot::unreadable(media)),', '                            Some(judge_slot(at, SlotIndex::A, &vec![0u8; at.size()])),\n                            Some(OneSlot::unreadable(media)),', F2O),
 ('crates/store-io-engine/src/open2.rs', '        for (&(id, p, entry, lim, short), header) in batch.iter().zip(headers) {', '        for (&(id, p, entry, lim, short), header) in batch.iter().zip(headers.into_iter().rev()) {', F2O),
 # E2b/E3 C8 review: completions matched by tag (out of order too); the
 # open's own queue from 16 header pairs
 ('crates/store-io-engine/src/slots.rs', '            let (i, part) = reads[c.tag as usize];', '            let (i, part) = reads[(c.tag as usize).min(arrived - 1)];', F2O),
 ('crates/store-io-engine/src/open2.rs', '    let mut own = if pending.len() >= OWN_QUEUE_FROM {', '    let mut own = if pending.len() > OWN_QUEUE_FROM {', F2O),
 ('crates/store-io-engine/src/open.rs', 'platform.queue(cfg).ok().map(|q| (q, cfg.depth as usize))', '(cfg.depth == 0).then(|| platform.queue(cfg).ok()).flatten().map(|q| (q, cfg.depth as usize))', F2O),
 # E2b/E3 C7 review: a header version newer than the winner's that does
 # not read is reported as a possibly lost recycle or slot commit
 ('crates/store-io-engine/src/open2.rs', '                if newer {', '                if newer && false {', F2C),
 # E2b/E3 C7: recycle in format 2 moves the region generation on and
 # writes the entry's extents only; a slot commit in format 2 carries
 # the object after the fixed part; neither takes format 1's path
 ('crates/store-io-engine/src/lifecycle2.rs', 'self.rewrite_header2(r, &mut header, RegionState::Ready, 1, committed, None)', 'self.rewrite_header2(r, &mut header, RegionState::Ready, 0, committed, None)', F2C),
 ('crates/store-io-engine/src/lifecycle2.rs', '                .map(|e| usize::from(e.extent_count))', '                .map(|e| usize::from(e.extent_count) + 1)', F2C),
 ('crates/store-io-engine/src/lifecycle.rs', '        let rewritten = if self.inner.layout.format == store_io_format::slot::FORMAT_2 {', '        let rewritten = if self.inner.layout.format == store_io_format::slot::FORMAT_2 && r.id.get() == u64::MAX {', F2C),
 ('crates/store-io-engine/src/slot_handle.rs', '        let payload = if s.inner.layout.format == store_io_format::slot::FORMAT_2 {', '        let payload = if s.inner.layout.format == store_io_format::slot::FORMAT_2 && data.is_empty() {', F2C),
 # E2b/E3 C6 review: the footprint is the header's committed extents (by
 # capacity); a release whose header write is refused after the commit
 # still stands
 ('crates/store-io-engine/src/lifecycle2.rs', '        let extents = committed_extents(&header, inner.layout.log2_block, committed);', '        let extents: Vec<(u64, u64)> = r.map.extents().chain(committed_extents(&header, inner.layout.log2_block, 0)).collect();', F2X + ['--test', 'format2_grow_beyond']),
 ('crates/store-io-engine/src/lifecycle2.rs', '            Err(_) if inner.domain.poisoned().is_none() => return Ok(()),', '            Err(_) if inner.domain.poisoned().is_none() => return Err(corrupt()),', F2X),
 # E2b/E3 C6: release in format 2: refused while frozen; next_seq kept;
 # the footprint and usage leave memory; free space recomputed; the
 # header says Released; format 2 never takes format 1's path
 ('crates/store-io-engine/src/lifecycle2.rs', '        if inner.frozen.load(std::sync::atomic::Ordering::Acquire) {', '        if inner.frozen.load(std::sync::atomic::Ordering::Acquire) && false {', F2X),
 ('crates/store-io-engine/src/lifecycle2.rs', '                next_seq: ph.next_seq,', '                next_seq: ph.next_seq - 1,', F2X),
 ('crates/store-io-engine/src/lifecycle2.rs', '                    let _gone = v2.allocated.swap_remove(i);', '                    let _gone = i;', F2X),
 ('crates/store-io-engine/src/lifecycle2.rs', '                let _gone = v2.usage.swap_remove(i);', '                let _gone = i;', F2X),
 ('crates/store-io-engine/src/lifecycle2.rs', '            v2.rebuild_free(len);', '            let _ = (v2, len);', F2X),
 ('crates/store-io-engine/src/lifecycle2.rs', 'self.rewrite_header2(r, &mut header, RegionState::Released, 0, committed, None)', 'self.rewrite_header2(r, &mut header, RegionState::Ready, 0, committed, None)', F2X),
 ('crates/store-io-engine/src/lifecycle.rs', '        if self.inner.layout.format == store_io_format::slot::FORMAT_2 {\n            return self.release_region2(r);', '        if self.inner.layout.format == store_io_format::slot::FORMAT_2 && r.id.get() == u64::MAX {\n            return self.release_region2(r);', F2X),
 # E2b/E3 C5: provisioning in format 2: frozen refuses provisioning
 # and reservations; a name held by an unavailable or unknown-kind entry
 # is taken; a page is used while it has room; first fit before the
 # tail; large data aligned; free space rebuilt after the commit; growth
 # refused past a truncated entry; a reservation grows past what is
 # allocated
 ('crates/store-io-engine/src/provision2.rs', '        if inner.frozen.load(std::sync::atomic::Ordering::Acquire) {', '        if inner.frozen.load(std::sync::atomic::Ordering::Acquire) && false {', F2P),
 ('crates/store-io-engine/src/space.rs', '        if inner.frozen.load(std::sync::atomic::Ordering::Acquire) {', '        if inner.frozen.load(std::sync::atomic::Ordering::Acquire) && false {', F2P),
 ('crates/store-io-engine/src/provision2.rs', '            || meta.unavailable.iter().any(|u| u.name == name.as_str())', '            || meta.unavailable.iter().any(|u| u.name == name.as_str() && false)', F2P),
 ('crates/store-io-engine/src/provision2.rs', '                v.reserved_names.contains(name) || v.claims.iter().any(|c| c.name == *name)', '                (v.reserved_names.contains(name) && false) || v.claims.iter().any(|c| c.name == *name)', F2P),
 ('crates/store-io-engine/src/provision2.rs', '                        && usize::from(h.count) + claimed < usize::from(per_page)', '                        && usize::from(h.count) + claimed <= usize::from(per_page)', F2P),
 ('crates/store-io-engine/src/provision2.rs', '            let offset = match v2.free.first_fit(extent, align, 2 * block) {', '            let offset = match v2.free.first_fit(extent, align, 2 * block).filter(|_| false) {', ['-p', 'store-io-engine', '--test', 'format2_provision', '--test', 'format2']),
 ('crates/store-io-engine/src/provision2.rs', '        let mut align = if capacity >= DATA_ALIGN {\n            DATA_ALIGN', '        let mut align = if capacity >= DATA_ALIGN && false {\n            DATA_ALIGN', ['-p', 'store-io-engine', '--test', 'format2_provision', '--test', 'format2']),
 ('crates/store-io-engine/src/space.rs', '        if meta.v2.as_deref().is_some_and(|v| v.cut_short) {', '        if meta.v2.as_deref().is_some_and(|v| v.cut_short && false) {', F2P),
 ('crates/store-io-engine/src/space.rs', '            Some(v2) => v2.free.end(),', '            Some(v2) => v2.free.end().min(self.inner.layout.tail(&meta.entries)),', F2P),
 # E2b/E3 C4: the format-2 open's regions: stale, superseded and
 # foreign-header regions are read-only; overlap and name demote; extra
 # extents are reported; several extents and 512 B blocks are refused
 # or read-only; every reserved overlap and stale page's agreement is said
 ('crates/store-io-engine/src/open2.rs', '                unconfirmed\n                    || (!settled', '                false\n                    || (!settled', F2R + ['--test', 'format2_repair']),
 ('crates/store-io-engine/src/open2.rs', '                    || s.superseded\n                    || !writable,', '                    || !writable,', F2R),
 ('crates/store-io-engine/src/open2.rs', '                    || s.superseded\n                    || !writable,', '                    || s.superseded\n                    || (!writable && false),', F2R),
 ('crates/store-io-engine/src/open2.rs', '            (Ok(_), Some(s)) if s.overlapped => Some(Unavailable::Overlaps),', '            (Ok(_), Some(s)) if s.overlapped && false => Some(Unavailable::Overlaps),', F2R),
 ('crates/store-io-engine/src/open2.rs', '            (Ok(_), Some(s)) if s.renamed => Some(Unavailable::NameTaken),', '            (Ok(_), Some(s)) if s.renamed && false => Some(Unavailable::NameTaken),', F2R),
 ('crates/store-io-engine/src/open2.rs', '            if c.outcome.is_err() || (c.extra.is_some() && page.standing == Standing::Unconfirmed) {', '            if c.extra.is_some() && page.standing == Standing::Unconfirmed {', F2R),
 ('crates/store-io-engine/src/open2.rs', '        if shares {', '        if shares && false {', F2R),
 ('crates/store-io-engine/src/open2.rs', '                    if usize::from(view.header.extent_count) == committed + 1 {', '                    if usize::from(view.header.extent_count) == committed + 2 {', F2R),
 ('crates/store-io-engine/src/open2.rs', '                c.outcome = Err(Unavailable::HeaderInvalid);\n', '', F2R),
 # E2b/E3 C4 review: the capacity fits the span; format 1's recycle,
 # release and slot commit refuse format 2; a format-2 slot reads its
 # object after the fixed part; a stale page's extra extent disagrees;
 # only a usable 512 B region makes the store read-only; format 2's
 # space report gives the free bytes
 ('crates/store-io-format/src/meta2.rs', '        if self.capacity > self.span_end - self.span_start {', '        if self.capacity > self.span_end - self.span_start && false {', FORMAT),
 ('crates/store-io-engine/src/slot_handle.rs', '        if l.format == store_io_format::slot::FORMAT_2 {\n            // The object follows', '        if l.format == store_io_format::slot::FORMAT_2 && false {\n            // The object follows', F2R),
 ('crates/store-io-engine/src/slot_handle.rs', '        if l.format == store_io_format::slot::FORMAT_2 {\n            // After the format-2', '        if l.format == store_io_format::slot::FORMAT_2 && false {\n            // After the format-2', F2R),
 ('crates/store-io-engine/src/open2.rs', '            if c.outcome.is_err() || (c.extra.is_some() && page.standing == Standing::Unconfirmed) {', '            if c.outcome.is_err() || (c.extra.is_some() && page.standing == Standing::Unconfirmed && false) {', F2R),
 ('crates/store-io-engine/src/open2.rs', '        if c.small {', '        if c.small && false {', F2R),
 ('crates/store-io-engine/src/open2.rs', '                    c.small = view.header.log2_block == 9;', '                    c.small = view.header.log2_block == 10;', F2R),
 ('crates/store-io-engine/src/space.rs', '                released: v2.free.free_bytes().saturating_sub(acct.reserved()),', '                released: v2.free.free_bytes().min(0),', F2R),
 ('crates/store-io-engine/src/open2.rs', '                let seq = if raw.footprint_seq == 0 || raw.footprint_seq > page_meta_seq {', '                let seq = if raw.footprint_seq == 0 {', F2R),
 # E2b/E3 C3: settling (the lower footprint_seq of two overlapping or
 # same-named usable regions loses, both on a tie; only a newer
 # unavailable region's own extents supersede; the sweep's stab, inside
 # and self-exclusion parts) and free space as a complement
 ('crates/store-io-engine/src/footprint.rs', '        if m >= Some(iv.seq) {', '        if m > Some(iv.seq) {', FOOTPRINT),
 ('crates/store-io-engine/src/footprint.rs', '        s.renamed = u.seq < top || at_top > 1;', '        s.renamed = u.seq < top || (at_top > 1 && u.seq > top);', FOOTPRINT),
 ('crates/store-io-engine/src/footprint.rs', '        if m > Some(iv.seq) {', '        if m >= Some(iv.seq) {', FOOTPRINT),
 ('crates/store-io-engine/src/footprint.rs', '        if m.is_some() {', '        if m.is_some() && false {', FOOTPRINT),
 ('crates/store-io-engine/src/footprint.rs', '        for &(off, n) in allocated.iter().filter(|r| r.1 > 0) {', '        for &(off, n) in allocated.iter() {', FOOTPRINT),
 ('crates/store-io-engine/src/footprint.rs', '                if gap_end > at {', '                if gap_end >= at {', FOOTPRINT),
 ('crates/store-io-engine/src/footprint.rs', '    Some((at.checked_add(skew)?.checked_add(align - 1)? & !(align - 1)) - skew)', '    Some(at + (skew & 0))', FOOTPRINT),
 ('crates/store-io-engine/src/footprint.rs', '        out[qi] = stab.max(inside);', '        out[qi] = inside.max(stab.filter(|_| false));', FOOTPRINT),
 ('crates/store-io-engine/src/footprint.rs', '        out[qi] = stab.max(inside);', '        out[qi] = stab.max(inside.filter(|_| false));', FOOTPRINT),
 ('crates/store-io-engine/src/footprint.rs', '            tree.max(lo, qi).max(tree.max(qi + 1, hi))', '            tree.max(lo, hi)', FOOTPRINT),
 ('crates/store-io-engine/src/footprint.rs', '        while heap.peek().is_some_and(|&(_, end)| end <= q.start) {', '        while heap.peek().is_some_and(|&(_, end)| end < q.start) {', FOOTPRINT),
 ('crates/store-io-engine/src/footprint.rs', '            at = at.max(end);', '            at = end;', FOOTPRINT),
 ('crates/store-io-engine/src/footprint.rs', '            let at = (start.checked_add(skew)?.checked_add(align - 1)? & !(align - 1)) - skew;', '            let at = start + (skew & 0);', FOOTPRINT),
 # E2b/E3 C2: a region's committed extents sum to its capacity, lie in
 # its span past the table area, and overlap nothing of its footprint
 ('crates/store-io-format/src/footprint.rs', '    if sum != entry.capacity {', '    if sum != entry.capacity && false {', FORMAT),
 ('crates/store-io-format/src/footprint.rs', '        if off < entry.span_start || end > entry.span_end || off < lim.table_end {', '        if off < entry.span_start || end > entry.span_end {', FORMAT),
 ('crates/store-io-format/src/footprint.rs', '        if off < entry.span_start || end > entry.span_end || off < lim.table_end {', '        if end > entry.span_end || off < lim.table_end {', FORMAT),
 ('crates/store-io-format/src/footprint.rs', '        if off < entry.span_start || end > entry.span_end || off < lim.table_end {', '        if off < entry.span_start || end > entry.span_end.saturating_add(entry.capacity) || off < lim.table_end {', FORMAT),
 ('crates/store-io-format/src/footprint.rs', '        if pair[1].0 - off < len {', '        if pair[1].0 - off < len && false {', FORMAT),
 ('crates/store-io-format/src/footprint.rs', '    out.sort_unstable();\n', '', FORMAT),
 # D-125/D-126 wakes: a finished flush wakes every bucket it covered (a
 # jump in done wakes each), then the designate and the shared sleepers;
 # a barrier needing the next flush sleeps as the designate when there is
 # none, its fellows in that flush's bucket; a narrow barrier, or one whose
 # bucket holds another need, sleeps on the shared word; a waker
 # deregisters whom it wakes; a sleeper leaves only when its word moved;
 # poison and a freed slot 0 wake whom they must
 ('crates/store-io-engine/src/domain/mod.rs', '            if w.waiting > 0 && w.need > before && w.need <= after {', '            if w.waiting > 0 && w.need > before && w.need <= after && false {', DOMAIN),
 ('crates/store-io-engine/src/domain/mod.rs', '            wakes.send(self);', '            wakes.len = 0;\n            wakes.shared = false;\n            wakes.send(self);', DOMAIN),
 ('crates/store-io-engine/src/domain/mod.rs', '        for d in before + 1..=after.min(before + BUCKETS as u64) {', '        for d in after..=after {', DOMAIN),
 ('crates/store-io-engine/src/domain/mod.rs', '                // shared sleepers include those that wait for a slot.\n                self.take_designate(&mut st, &mut wakes);', '                // shared sleepers include those that wait for a slot.', DOMAIN),
 ('crates/store-io-engine/src/domain/mod.rs', '        if narrow {\n            return Bed::Shared;', '        if narrow && false {\n            return Bed::Shared;', DOMAIN),
 ('crates/store-io-engine/src/domain/mod.rs', '        if need > issued && st.designate.is_none() {', '        if need > issued && st.designate.is_none() && false {', DOMAIN),
 ('crates/store-io-engine/src/domain/mod.rs', '        let covered_later = need <= issued || st.designate == Some(need);', '        let covered_later = need <= issued;', DOMAIN),
 ('crates/store-io-engine/src/domain/mod.rs', '        if covered_later && (bucket.waiting == 0 || bucket.need == need) {', '        if covered_later && (bucket.waiting == 0 || bucket.need == need || true) {', DOMAIN),
 ('crates/store-io-engine/src/domain/mod.rs', '        st.buckets[b].waiting = 0;\n        self.bucket_words[b].bump();', '        st.buckets[b].waiting -= 0;\n        self.bucket_words[b].bump();', DOMAIN),
 ('crates/store-io-engine/src/domain/mod.rs', '            if st.buckets[b].waiting > 0 {\n                self.take_bucket(st, b, wakes);', '            if st.buckets[b].waiting > 0 && false {\n                self.take_bucket(st, b, wakes);', DOMAIN),
 ('crates/store-io-engine/src/domain/mod.rs', '        while word.load() == seen {\n            word.wait(seen);', '        if word.load() == seen {\n            word.wait(seen);', DOMAIN),
 ('crates/store-io-engine/src/domain/mod.rs', '        // Slot 0 is free: whoever waits for a slot.\n        let mut wakes = Wakes::new();\n        d.take_designate(&mut st, &mut wakes);', '        // Slot 0 is free: whoever waits for a slot.\n        let mut wakes = Wakes::new();', DOMAIN_ALL),
 ('crates/store-io-buf/src/wait.rs', '            if self.word.load(Ordering::SeqCst) == seen {', '            if self.word.load(Ordering::SeqCst) == seen || true {', LOOM_BUF, LOOM),
 # D-124 review: done never jumps to the highest finished flush; every
 # flush lane is a flush to the watchdog; a full flush of the main handle
 # waits for slot 0 and gives it back
 ('crates/store-io-engine/src/domain/mod.rs', '        self.done.store(done, Ordering::SeqCst);', '        self.done.store(done.max(k), Ordering::SeqCst);'),
 ('crates/store-io-engine/src/stats/mod.rs', '            let flush = i >= first_flush;', '            let flush = i >= first_flush + 3;'),
 ('crates/store-io-engine/src/domain/mod.rs', '        while st.busy & 1 == 1 {', '        while st.busy & 1 == 2 {'),
 ('crates/store-io-engine/src/domain/mod.rs', '        st.busy &= !1;', '        st.busy &= !0;'),
 # D-124 second review: a metadata flush leads only on slot 0;
 # provisioning's full flush holds slot 0; data leaders take slot 0 last
 ('crates/store-io-engine/src/store.rs', '        domain.barrier_main(need, || flush_data(q, file))?;', '        domain.barrier(need, |_, _| flush_data(q, file))?;'),
 ('crates/store-io-engine/src/provision.rs', '        let flushed = inner.domain.exclusive_main(|| {\n            let r = inner.platform.flush_all(&inner.file);', '        let flushed = (|f: &dyn Fn() -> _| f())(&|| {\n            let r = inner.platform.flush_all(&inner.file);'),
 ('crates/store-io-engine/src/domain/mod.rs', '            let slot = 31 - free.leading_zeros();', '            let slot = free.trailing_zeros();'),
 # a full flush waiting for slot 0 has it before new leaders
 ('crates/store-io-engine/src/domain/mod.rs', '            if st.main_waiting > 0 {', '            if st.main_waiting > 0 && false {'),
 # audit: resume_at raises the durable point only from the old tail and
 # never on a poisoned store; a waiter for room waits on a prefix read
 # before its refusal; the last completion's wake is never lost; nothing
 # is reserved once told to give up
 ('crates/store-io-engine/src/frontier.rs', '        let _moved = self\n            .durable\n            .compare_exchange(tail, new, Ordering::SeqCst, Ordering::SeqCst);', '        let _moved = self.durable.fetch_max(new, Ordering::SeqCst);'),
 ('crates/store-io-engine/src/frontier.rs', '        let _moved = self\n            .synced\n            .compare_exchange(tail, new, Ordering::SeqCst, Ordering::SeqCst);', '        let _moved = self.synced.fetch_max(new, Ordering::SeqCst);'),
 ('crates/store-io-engine/src/region.rs', '        if let Some(first) = self.store.inner.domain.poisoned() {\n            return Err(Error::Poisoned { first });\n        }\n        let _pass = self.pass()?;\n        let at = self', '        let _pass = self.pass()?;\n        let at = self'),
 ('crates/store-io-engine/src/frontier.rs', '            if give_up() {\n                return Err(NotWrittenCause::TooManyInFlight);\n            }\n            // The prefix', '            // The prefix'),
 ('crates/store-io-engine/src/frontier.rs', '                    if !self.wait_completed(p + 1, &give_up) {', '                    if !self.wait_completed(p.max(self.prefix.load(Ordering::SeqCst)) + 1, &give_up) {', LOOM_ENGINE, LOOM),
 ('crates/store-io-engine/src/frontier.rs', '        if self.waiters.fetch_or(0, Ordering::AcqRel) > 0 {', '        if self.waiters.load(Ordering::SeqCst) > 0 {', LOOM_ENGINE, LOOM),
 # resume_at advances over an append that raced it, and wakes waiters
 ('crates/store-io-engine/src/frontier.rs', '        let _moved = self.advance(self.ring());\n        self.wake_if_waiting();\n        // A writable', '        // A writable', LOOM_ENGINE, LOOM),
 # audit: one replace at a time in a directory across handles (remove
 # takes no lock); no commit on a handle poisoned meanwhile; names that
 # could resolve to another file refused (too long, aliases, the
 # directory's own); at most 65,536 pool classes
 ('crates/store-io-engine/src/directory.rs', '        let _owner = self.change_lock()?;\n        // A leftover', '        let _owner = self.change_lock().ok();\n        // A leftover'),
 ('crates/store-io-engine/src/directory.rs', '        if let Some(first) = self.poisoned.get() {\n            drop(file);', '        if let Some(first) = self.poisoned.get().filter(|_| false) {\n            drop(file);'),
 ('crates/store-io-engine/src/directory.rs', 'let too_long = name.len() > NAME_MAX;', 'let too_long = name.len() > NAME_MAX.max(255);'),
 ('crates/store-io-engine/src/directory.rs', 'let alias = short_name_shaped(name) || device_name(name);', 'let alias = (short_name_shaped(name) && false) || device_name(name);'),
 ('crates/store-io-engine/src/directory.rs', 'let alias = short_name_shaped(name) || device_name(name);', 'let alias = short_name_shaped(name) || (device_name(name) && false);'),
 ('crates/store-io-engine/src/directory.rs', "            '\\u{17F}' => 's',", "            '\\u{17F}' => '\\u{17F}',"),
 ('crates/store-io-engine/src/directory.rs', 'let own = folded.ends_with(TEMP_SUFFIX) || folded == LOCK_NAME;', 'let own = folded == LOCK_NAME;'),
 ('crates/store-io-engine/src/directory.rs', 'let own = folded.ends_with(TEMP_SUFFIX) || folded == LOCK_NAME;', 'let own = folded.ends_with(TEMP_SUFFIX);'),
 ('crates/store-io-buf/src/pool.rs', '        if cfg.classes.len() > usize::from(u16::MAX) + 1 {', '        if cfg.classes.len() > usize::from(u16::MAX) + 2 {', ['-p', 'store-io-buf']),
 # names a case-insensitive file system or Windows resolves to another:
 # short names split at the last `~`, each limit of the shape held;
 # ignorable code points dropped before the comparison; device stems
 # trimmed
 ('crates/store-io-engine/src/directory.rs', '.filter(|&c| !ignorable(c))', '.filter(|&c| !ignorable(c) || true)'),
 ('crates/store-io-engine/src/directory.rs', "    let Some((_, digits)) = base.rsplit_once('~') else {", "    let Some((_, digits)) = base.split_once('~') else {"),
 ('crates/store-io-engine/src/directory.rs', '        && base.chars().count() <= 8\n', '        && base.chars().count() <= 9\n'),
 ('crates/store-io-engine/src/directory.rs', '        && ext.chars().count() <= 3\n', '        && ext.chars().count() <= 4\n'),
 ('crates/store-io-engine/src/directory.rs', "        && !ext.contains('.')\n", "        && !ext.contains('\\u{0}')\n"),
 ('crates/store-io-engine/src/directory.rs', '    let stem = name.split(\'.\').next().unwrap_or(name).trim_end();', '    let stem = name.split(\'.\').next().unwrap_or(name);'),
 # folded names that are empty, or end in `.` or a space (the directory,
 # its parent, a name Windows trims); every ignorable
 # range end; the Kelvin sign folds to k
 ('crates/store-io-engine/src/directory.rs', "let trimmed = folded.is_empty() || folded.ends_with(['.', ' ']);", 'let trimmed = folded.is_empty();'),
 ('crates/store-io-engine/src/directory.rs', "let trimmed = folded.is_empty() || folded.ends_with(['.', ' ']);", "let trimmed = folded.ends_with(['.', ' ']);"),
 ('crates/store-io-engine/src/directory.rs', "            | '\\u{180B}'..='\\u{180F}'", "            | '\\u{180B}'..='\\u{180E}'"),
 ('crates/store-io-engine/src/directory.rs', "            | '\\u{E0000}'..='\\u{E0FFF}'", "            | '\\u{E0001}'..='\\u{E0FFF}'"),
 ('crates/store-io-engine/src/directory.rs', "            '\\u{212A}' => 'k',", "            '\\u{212A}' => '\\u{212A}',"),
 (COPY_SRC, "        } else {\n            r.generation.get()\n        };", "        } else {\n            r.generation.get() & 1\n        };", COPYT),
 (COPY_SRC, "let generation = if !format_2 && r.kind == RegionKind::Slot {", "let generation = if !format_2 && r.kind == RegionKind::Slot && false {", COPYT),
 (COPY_SRC, "                fill_key,\n                tag,", "                fill_key: [fill_key[0] ^ 1; 16],\n                tag,", COPYT),
 (COPY_SRC, "(m.fill, m.fill_key, false)", "(m.fill, [0; 16], false)", COPYT),
 (COPY_SRC, "                tag,\n            },", "                tag: tag & 0,\n            },", COPYT),
 (COPY_SRC, "cap: capped.then(|| r.map.capacity().max(data_size)),", "cap: capped.then(|| r.map.capacity().max(data_size)).and(None),", COPYT),
 # The copy's one barrier makes a stale page's full copies durable (not
 # written as a fill, D-137), also when the clean close's flush fails.
 (COPY_SRC, "        dst.flush_all_accounted()?;", "", COPYT + ["--test", "copy_stale"]),
 (COPY_SRC, "    match platform.unlink(&dir, &temp) {", "    match platform.unlink(&dir, &name(\"absent\")?) {", COPYT),
 (COPY_SRC, "    platform\n        .sync_dir(&dir)\n", "    Ok::<(), store_io_platform::OsError>(())\n", COPYT),
 (COPY_SRC, "let src = Store::open_readonly(platform.clone(), from, opts.store.clone())?;", "let src = Store::open(platform.clone(), from, opts.store.clone())?;", COPYT),
 (COPY_SRC, "if src.frozen() {", "if src.frozen() && false {", COPYT),
 (COPY_SRC, "if let Some(u) = src.unavailable_regions().first() {", "if let Some(u) = src.unavailable_regions().first().filter(|_| false) {", COPYT),
 (COPY_SRC, "o.len() > max_object(log2)", "o.len() > max_object(log2) + 100", COPYT),
 (COPY_SRC, "        Ok(_) => Err(Error::AlreadyExists { what: Named::Store }),", "        Ok(_) => Ok(()),", COPYT),
 (COPY_SRC, "if let Some(&(p, s)) = report.metadata.invalid_entries.first() {", "if let Some(&(p, s)) = report.metadata.invalid_entries.first().filter(|_| false) {", COPYT),
 (COPY_SRC, "    let _lock = platform.lock_exclusive(&lock_file).map_err(|raw| {", "    let _lock = platform.lock_exclusive(&lock_file).map_or(Ok(()), |_| Ok(())).map_err(|raw: store_io_platform::OsError| {", COPYT),
 (COPY_SRC, "    match platform.rename_noreplace(&dir, &temp, &final_name, &file) {", "    match platform.rename_replace(&dir, &temp, &final_name, &file) {", COPYT),
 (COPY_SRC, "    holds_store()?;\n    match platform.unlink(&dir, &temp) {", "    drop(_lock);\n    holds_store()?;\n    match platform.unlink(&dir, &temp) {", COPYT),
 (COPY_SRC, "        Err(raw) if is_not_found(raw) => Ok(()),\n        Err(raw) => Err(io(Op::Open, raw)),", "        Err(_) => Ok(()),", COPYT),
 (COPY_SRC, "if dst.report().block_size != block {", "if dst.report().block_size != block && false {", COPYT),
 (COPY_SRC, "if f.kind != RegionKind::Slot && f.data_size % self.block() != 0 {", "if f.kind != RegionKind::Slot && f.data_size % self.block() != 0 && false {", COPYT),
 ('crates/store-io-engine/src/open.rs', '            Found::V1(_) if !read_only => {', '            Found::V1(_) if !read_only && false => {', F1T),
 ('crates/store-io-engine/src/create.rs', '    placed\n        && (torn', '    (placed || true)\n        && (torn', F2),
 ('crates/store-io-engine/src/create.rs', '        return torn && at == 0;', '        return torn;', F2),
 ('crates/store-io-engine/src/create.rs', '        || u64_at(64) != at\n', '', F2),
 ('crates/store-io-engine/src/create.rs', '        || b[76] != 0\n', '', F2),
 ('crates/store-io-engine/src/create.rs', 'let placed = (crate::layout::MIN_LOG2_BLOCK..=crate::layout::MAX_LOG2_BLOCK).any(', 'let placed = (crate::layout::MIN_LOG2_BLOCK..=crate::layout::MIN_LOG2_BLOCK).any(', F2),
 ('crates/store-io-engine/src/store.rs', '            container_format: store_io_format::slot::FORMAT_2,', '            container_format: store_io_format::slot::FORMAT_1,', F1T),
 ('crates/store-io-engine/src/provision2.rs', 'let data_size = if slot || (data_size == 0 && copied.is_some()) {', 'let data_size = if slot || (data_size == 0 && copied.is_some() && false) {', COPYT),
 ('crates/store-io-sim/src/world.rs', '        if d.entries.contains_key(to) {\n            return Err(EEXIST);', '        if d.entries.contains_key(to) && false {\n            return Err(EEXIST);', COPYT),
 (COPY_SRC, "            dst[done..done + n].copy_from_slice(&self.current[self.used..self.used + n]);", "            dst[done..done + n].fill(0);", COPYT),
 (COPY_SRC, "                            if v.try_reserve_exact(part.len()).is_err() {", "                            if v.try_reserve_exact(part.len()).is_err() || true {", COPYT),
 ('crates/store-io-engine/src/provision2.rs', '            region_generation: pl.generation.get(),', '            region_generation: 1,', COPYT),
 ('crates/store-io-engine/src/provision2.rs', '            generation: pl.generation,', '            generation: Generation::FIRST,', COPYT),
 ('crates/store-io-engine/src/provision2.rs', '            Some(c) => (c.fill, c.fill_key),', '            Some(c) => (c.fill, if c.fill_key == [0; 16] { random_uuid() } else { random_uuid() }),', COPYT),
 ('crates/store-io-engine/src/provision2.rs', 'let tag = copied.map_or_else(|| from.as_ref().map_or(0, |r| r.tag()), |c| c.tag);', 'let tag = copied.map_or_else(|| from.as_ref().map_or(0, |r| r.tag()), |c| c.tag & 0);', COPYT),
 (REPAIR_SRC, "id: RegionId) -> Result<(), Error> {\n        self.ready()?;", "id: RegionId) -> Result<(), Error> {", F2REP),
 (REPAIR_SRC, "        };\n        if inner.layout.format != store_io_format::slot::FORMAT_2 {", "        };\n        if inner.layout.format != store_io_format::slot::FORMAT_2 && false {", F2REP),
 (REPAIR_SRC, "        if inner.frozen.load(std::sync::atomic::Ordering::Acquire) {", "        if inner.frozen.load(std::sync::atomic::Ordering::Acquire) && false {", F2REP),
 (REPAIR_SRC, "        if let Some(span) = hold.span {", "        if let Some(span) = hold.span.filter(|_| false) {", F2REP),
 (REPAIR_SRC, "        if let Some(usage) = hold.usage {", "        if let Some(usage) = hold.usage.filter(|_| false) {", F2REP),
 (REPAIR_SRC, "        if let Some(name) = &hold.name {", "        if let Some(name) = hold.name.as_ref().filter(|_| false) {", F2REP),
 (REPAIR_SRC, "        v2.cut_short = v2.holds.iter().any(|h| h.short);", "        v2.cut_short = v2.cut_short || v2.holds.iter().any(|h| h.short);", F2REP),
 (REPAIR_SRC, "                region: meta.unavailable.remove(i),", "                region: meta.unavailable[i].clone(),", F2REP),
 (REPAIR_SRC, "        meta.repairs.push(record);", "        let _ = record;", F2REP),
 (REPAIR_SRC, "\n        v2.rebuild_free(len);", "\n        let _ = len;", F2REP),
 (REPAIR_SRC, "        v2.cut_short = v2.holds.iter().any(|h| h.short);", "        v2.cut_short = v2.holds.iter().any(|h| h.short && false);", F2REP),
 (REPAIR_SRC, "            if let Some(region) = crate::repair::would_lift(self, v2, id.get()) {", "            if let Some(region) = crate::repair::would_lift(self, v2, id.get()).filter(|_| false) {", F2REP),
 ('crates/store-io-engine/src/lifecycle2.rs', "            .and_then(|v2| crate::repair::would_lift(self, v2, id))", "            .and_then(|v2| crate::repair::would_lift(self, v2, id).filter(|_| false))", F2REP),
 (REPAIR_SRC, "    if !demoted && !superseded {", "    if !demoted || !superseded {", F2REP),
 (REPAIR_SRC, "            (*demoted && !(s.overlapped || s.renamed)) || (*superseded && !s.superseded)", "            (*demoted && !(s.overlapped || s.renamed)) || (*superseded && !s.superseded && false)", F2REP),
 (REPAIR_SRC, "            (*demoted && !(s.overlapped || s.renamed)) || (*superseded && !s.superseded)", "            (*demoted && !(s.overlapped || s.renamed) && false) || (*superseded && !s.superseded)", F2REP),
 (REPAIR_SRC, "            (*demoted && !(s.overlapped || s.renamed)) || (*superseded && !s.superseded)", "            (*demoted && !s.overlapped) || (*superseded && !s.superseded)", F2REP),
 (REPAIR_SRC, "        .filter(|h| h.id() != removing && h.demoted.is_none())", "        .filter(|h| h.demoted.is_none())", F2REP),
 (REPAIR_SRC, "    for r in live.iter().filter(|r| r.id.get() != removing) {", "    for r in live.iter() {", F2REP),
 ('crates/store-io-engine/src/open2.rs', "                bad_entries.push((seq, span, entry_name));", "                bad_entries.push((seq, span, entry_name.filter(|_| false)));", F2REP),
 ('crates/store-io-engine/src/open2.rs', "                    demoted: None,\n                    entry_name,\n", "                    demoted: None,\n                    entry_name: entry_name.filter(|_| false),\n", F2REP),
 ('crates/store-io-engine/src/open2.rs', "                entry_name: Some(e.name),", "                entry_name: Some(e.name).filter(|_| false),", F2REP),
 ('crates/store-io-engine/src/open2.rs', "                (Err(_), None) => vec![(e.span_start, e.span_end - e.span_start)],", "                (Err(_), None) => Vec::new(),", F2REP),
 ('crates/store-io-engine/src/footprint.rs', "            .filter_map(|x| x.name.map(|n| (n, x.seq))),", "            .filter_map(|x| x.name.map(|n| (n, x.seq)).filter(|_| false)),", F2REP),
 ('crates/store-io-engine/src/open2.rs', "                    if usize::from(view.header.extent_count) >= committed {", "                    if usize::from(view.header.extent_count) >= committed || true {", F2REP),
 (REPAIR_SRC, "        let mut f = vec![(r.header_at[0], block), (r.header_at[1], block)];", "        let mut f = vec![(r.header_at[0], 0 * block), (r.header_at[1], 0)];", F2REP),
 ('crates/store-io-engine/src/open2.rs', "                if let Ok(view) = decode_region_header(bytes, layout.log2_block, u64::MAX) {", "                if let Ok(view) = decode_region_header(bytes, layout.log2_block, container_len) {", F2REP),
 (V2, "        if pair > 0 && (decode_lost || pages[pair - 1].ahead().next().is_some()) {", "        if pair > 0 && (decode_lost || pages[pair - 1].ahead().next().is_some() && false) {", F2REP),
 (V2, "        .find(|e| ahead.contains(&(e.seq, e.footprint_seq)))", "        .find(|e| ahead.contains(&(e.seq, e.footprint_seq)) && false)", F2REP),
 ('crates/store-io-engine/src/provision2.rs', "                        && page.ahead().next().is_none()", "                        && (page.ahead().next().is_none() || true)", F2REP),
 ('crates/store-io-engine/src/open2.rs', "                    .then(|| (e.tag, e.data_size, e.capacity.saturating_add(2 * block))),", "                    .then(|| (e.tag, e.data_size, 0)),", F2REP),
 # accept_page_rollback (v0.5 E6 C3).
 (REPAIR_SRC, "pages: &[u32]) -> Result<(), Error> {\n        self.ready()?;", "pages: &[u32]) -> Result<(), Error> {", F2REP),
 (REPAIR_SRC, "        let inner = &self.inner;\n        if inner.layout.format != store_io_format::slot::FORMAT_2 {", "        let inner = &self.inner;\n        if inner.layout.format != store_io_format::slot::FORMAT_2 && false {", F2REP),
 (REPAIR_SRC, "                        NotWrittenCause::OutOfBounds,", "                        NotWrittenCause::NotReady,", F2REP),
 (REPAIR_SRC, "                    Standing::Confirmed => continue,", "                    Standing::Confirmed => {}", F2REP),
 (REPAIR_SRC, "                    Standing::Unconfirmed => {}\n                    Standing::Lost | Standing::Refused => {", "                    Standing::Unconfirmed | Standing::Lost => {}\n                    Standing::Refused => {", F2REP),
 (REPAIR_SRC, "                if let Some((seq, _)) = pg.ahead().next() {", "                if let Some((seq, _)) = pg.ahead().next().filter(|_| false) {", F2REP),
 (REPAIR_SRC, "                                (u64::from(page) << 32) | u64::from(seq),", "                                u64::from(seq),", F2REP),
 (REPAIR_SRC, "                if let Some(i) = v2.allocated.iter().position(|a| a == range) {", "                if let Some(i) = v2.allocated.iter().position(|a| a == range).filter(|_| false) {", F2B),
 (REPAIR_SRC, "                .filter(|(at, _, _)| *at == p)", "                .filter(|(at, _, _)| *at == p || true)", F2B),
 (REPAIR_SRC, "            v2.rebuild_free(len);", "            let _ = len;", F2B),
 (REPAIR_SRC, "                .filter(|r| !r.superseded && !r.foreign)", "                .filter(|r| !r.foreign)", F2REP),
 (REPAIR_SRC, "                .filter(|r| !r.superseded && !r.foreign)", "                .filter(|r| !r.superseded)", F2REP),
 (REPAIR_SRC, "                r.read_only\n                    .store(false, std::sync::atomic::Ordering::Release);\n            }\n            inner", "                r.read_only\n                    .store(true, std::sync::atomic::Ordering::Release);\n            }\n            inner", F2REP),
 (REPAIR_SRC, "        if v2.pages.iter().all(|pg| pg.standing == Standing::Confirmed) {\n            for r in inner", "        if v2.pages.iter().all(|pg| pg.standing == Standing::Confirmed) || true {\n            for r in inner", F2REP),
 (REPAIR_SRC, "        if v2.pages.iter().all(|pg| pg.standing == Standing::Confirmed) {\n            for r in inner", "        if v2.pages.iter().all(|pg| pg.standing == Standing::Confirmed) && false {\n            for r in inner", F2REP),
 (REPAIR_SRC, "                if !todo.iter().any(|&(_, at)| at == p) {", "                if true {", F2REP),
 (REPAIR_SRC, "                                range: Some(crate::v2::page_range(&inner.layout, page)),", "                                range: None,", F2REP),
 (V2, "            domain.poison(store_io_core::error::FirstCause {\n                op: Some(store_io_core::error::Op::Read),", "            let _ = core::convert::identity(store_io_core::error::FirstCause {\n                op: Some(store_io_core::error::Op::Read),", F2REP),
 (V2, "        end: a.max(b) + (1u64 << layout.log2_table),", "        end: a.max(b),", F2REP),
 (REPAIR_SRC, "            meta.repairs.push(Repair::PageAccepted { page });", "            let _ = page;", F2REP),
 (V2, "    let mut next = plan_next(&page.pair, &payload, og)?;\n    next.meta_seq = meta_seq;", "    let mut next = plan_next(&page.pair, &payload, og)?;\n    next.meta_seq = meta_seq - 1;", F2REP),
 (V2, "            page.pair = back;\n            page.standing = Standing::Confirmed;", "            page.pair = back;", F2REP),
 ('crates/store-io-engine/src/open2.rs', "            foreign: !writable,", "            foreign: false,", F2REP),
 # The twin trace (D-130).
 (V2, "        if back.twin {", "        if true {", F2REP),
 (V2, "                    if standing == Standing::Unconfirmed || page.pair.possibly_accepted() {", "                    if page.pair.possibly_accepted() {", F2REP),
 (V2, "                    if standing == Standing::Unconfirmed || page.pair.possibly_accepted() {", "                    if standing == Standing::Unconfirmed {", F2REP),
 (V2, "            h.next_seq = h.next_seq.saturating_add(SEQ_MARGIN);", "            h.next_seq = h.next_seq.saturating_add(SEQ_MARGIN * 0);", F2REP),
 ('crates/store-io-engine/src/slots.rs', "    let twin = same(a.payload.as_deref(), b.payload.as_deref());", "    let twin = false;", F2REP),
 ('crates/store-io-engine/src/slots.rs', "    let twin = same(a.payload.as_deref(), b.payload.as_deref());", "    let twin = a.payload.is_some() && b.payload.is_some();", F2REP),
 ('crates/store-io-engine/src/slots.rs', "    let twin = both\n", "    let twin = false && both\n", F2REP),
 ('crates/store-io-engine/src/open2.rs', "                    || (!settled && v2.pages[c.page].pair.possibly_accepted())", "                    || (!settled && v2.pages[c.page].pair.possibly_accepted() && false)", F2REP),
 ('crates/store-io-engine/src/open2.rs', "                    || (!settled && v2.pages[c.page].pair.possibly_accepted())", "                    || ((!settled || true) && v2.pages[c.page].pair.possibly_accepted())", F2REP),
 ('crates/store-io-engine/src/open2.rs', "    let settled = v2.pages.iter().all(|p| p.standing == Standing::Confirmed);", "    let settled = v2.pages.iter().all(|p| p.standing != Standing::Unconfirmed);", F2REP),
 ('crates/store-io-engine/src/slots.rs', "        self.twin || !matches!(other, SlotStatus::Valid(_) | SlotStatus::Blank)", "        self.twin || (!matches!(other, SlotStatus::Valid(_) | SlotStatus::Blank) && false)", F2REP),
 ('crates/store-io-engine/src/slots.rs', "        self.twin || !matches!(other, SlotStatus::Valid(_) | SlotStatus::Blank)", "        !matches!(other, SlotStatus::Valid(_) | SlotStatus::Blank)", F2REP),
 ('crates/store-io-engine/src/open2.rs', "                    stale_extra.push((c.page, c.id, range));", "                    stale_extra.clear();\n                    let _ = range;", F2B),
 # copy_store accept_stale_pages (v0.5 E6 C4).
 (COPY_SRC, "    let accept = opts.accept_stale_pages;", "    let accept = false;", COPYS),
 (COPY_SRC, "    let accept = opts.accept_stale_pages;", "    let accept = true;", COPYT),
 (COPY_SRC, "        if v2.pages.get(page)?.standing != Standing::Unconfirmed {", "        if v2.pages.get(page)?.standing == Standing::Lost {", COPYS),
 (COPY_SRC, "            if !others.any(|o| overlaps(extra, o)) {", "            if true || !others.any(|o| overlaps(extra, o)) {", COPYS),
 (COPY_SRC, "            if !others.any(|o| overlaps(extra, o)) {", "            if false && !others.any(|o| overlaps(extra, o)) {", COPYS),
 (COPY_SRC, ".chain(o.header_at.iter().map(|&h| (h, block)))", ".chain(o.header_at.iter().map(|&h| (h, block)).filter(|_| false))", COPYS),
 (COPY_SRC, ".chain(v2.holds.iter().filter_map(|h| h.span))", ".chain(v2.holds.iter().filter_map(|h| h.span).filter(|_| false))", COPYS),
 (COPY_SRC, ".chain(v2.holds.iter().flat_map(|h| h.own.iter().copied()))", ".chain(v2.holds.iter().flat_map(|h| h.own.iter().copied()).filter(|_| false))", COPYS),
 (COPY_SRC, ".chain(core::iter::once((0, self.inner.layout.data_start())))", ".chain(core::iter::once((0, self.inner.layout.data_start())).filter(|_| false))", COPYS),
 (COPY_SRC, "        if let Some(extra) = extra.filter(|_| !lost) {", "        if let Some(extra) = extra.filter(|_| !lost || true) {", COPYS),
 (COPY_SRC, "            offset += piece.len() as u64;\n            at += piece.len() as u64;", "            offset += 0 * piece.len() as u64;\n            at += piece.len() as u64;", COPY_UNIT_T),
 (COPY_SRC, "                    src.read_raw(&f.source, at, file + done, &mut buf[..n], reading)?;", "                    src.read_raw(&f.source, at, file, &mut buf[..n], reading)?;", COPYS),
 (COPY_SRC, ".filter(|e| e.1 != r.id.get())", ".filter(|_| false)", COPYS),
 (COPY_SRC, "a < b + m && b < a + n;", "a < b + m && n > 0;", COPYS),
 (COPY_SRC, "a < b + m && b < a + n;", "b < a + n && m > 0;", COPYS),
 (COPY_SRC, "        let full = if r.kind == RegionKind::Slot {", "        let full = if false {", COPYS),
 (COPY_SRC, "            verified_up_to.push((f.name.clone(), *recorded));", "            verified_up_to.truncate(0);\n            let _ = recorded;", COPYS),
 (COPY_SRC, "            superseded.push(f.name.clone());", "            superseded.truncate(0);", COPYS),
 (COPY_SRC, "        omitted = src.omitted_for_copy();", "        omitted.truncate(0);\n        let _ = src.omitted_for_copy();", COPYS),
 (COPY_SRC, "        lost_pages.clone_from(&report.metadata.lost_pages);", "        lost_pages.truncate(0);", COPYS),
 (COPY_SRC, "        stale_pages.clone_from(&report.metadata.stale_pages);", "        stale_pages.truncate(0);", COPYS),
 (COPY_SRC, "            superseded: r.superseded || self.at_risk(r),", "            superseded: r.superseded || (self.at_risk(r) && false),", COPYS),
 (COPY_SRC, "            (pg.standing == Standing::Unconfirmed || pg.pair.possibly_accepted())", "            (pg.standing == Standing::Unconfirmed || (pg.pair.possibly_accepted() && false))", COPYS),
 (COPY_SRC, "                    i != page && (o.standing", "                    (i != page || true) && (o.standing", COPYS),
 (COPY_SRC, "(o.standing != Standing::Confirmed || o.pair.possibly_accepted())", "(o.standing != Standing::Confirmed || (o.pair.possibly_accepted() && false))", COPYS),
 (COPY_SRC, "        if let Some((ranges, _)) = &f.full {", "        if let Some((ranges, _)) = f.full.as_ref().filter(|_| false) {", COPYS),
 # The inline fast path (G1 P1).
 (EXEC_SRC, "        self.core.table.busy();\n        let _idle", "        if false {\n            self.core.table.busy();\n        }\n        let _idle", INLINE),
 (EXEC_SRC, "                self.0.publish();", "                let _ = &self.0;", INLINE),
 (EXEC_SRC, "        let _idle = Idle(&self.core.table);", "        let _ = Idle(&self.core.table);", INLINE),
 (EXEC_SRC, "    if lane.done.is_empty() && lane.in_flight() == 0 && lane.core.queue.in_flight() == 0 {", "    if lane.in_flight() == 0 && lane.core.queue.in_flight() == 0 {", ["-p", "store-io-engine", "--lib", "exec::"]),
 (EXEC_SRC, "|| !self.done.is_empty() || !self.core.queue.performs() {", "|| !self.done.is_empty() || true {", INLINE),
 (EXEC_SRC, "            return Ok(Ran::Done { buf, result });", "            return Ok(Ran::Done { buf, result: result.or(Ok(0)) });", INLINE),
 (EXEC_SRC, "                            (Ok(n), Some(buf)) if n == buf.len() => src.recycle(buf),\n                            (result, _) => failure = Some(result.err()),", "                            (Ok(_), Some(buf)) => src.recycle(buf),\n                            (result, _) => failure = Some(result.err()),", INLINE),
 (EXEC_SRC, "                            (result, _) => failure = Some(result.err()),", "                            (_, _) => {}", INLINE),
 (EXEC_SRC, "            let op = if in_flight == 0 {\n                match lane.perform(op) {", "            let op = if false {\n                match lane.perform(op) {", INLINE),
 (EXEC_SRC, "                        if submitted == 0 {\n                            return Pumped::Refused(r.raw);\n                        }\n                        failure = Some(r.raw);\n                        break;", "                        if false {\n                            return Pumped::Refused(r.raw);\n                        }\n                        failure = Some(r.raw);\n                        break;", INLINE),
 (EXEC_SRC, "                        submitted += 1;\n                        obs.add(Counter::Writes, 1);\n                        obs.add(Counter::WriteBytes, bytes);\n                        match (result, buf) {", "                        obs.add(Counter::Writes, 1);\n                        obs.add(Counter::WriteBytes, bytes);\n                        match (result, buf) {", INLINE),
 ('crates/store-io-engine/src/optable.rs', "        self.mark.set(2 * seq + 1);", "        self.mark.set(2 * seq);", INLINE),
 ('crates/store-io-posix/src/queue.rs', "        self.pooled.is_none() && self.ready.is_empty()\n    }", "        self.pooled.is_none()\n    }", POSIX),
 ('crates/store-io-posix/src/queue.rs', "        self.pooled.is_none() && self.ready.is_empty()\n    }", "        false\n    }", POSIX),
 ('crates/store-io-posix/src/queue.rs', "        if self.uring.is_some() {\n            return false;\n        }\n        self.pooled", "        if false {\n            return false;\n        }\n        self.pooled", POSIX),
 ('crates/store-io-posix/src/queue.rs', "        if !self.performs() {\n            return Ok(Err(op));\n        }\n        Ok(Ok(check(op, PosixFile::raw)?.perform()))", "        if false {\n            return Ok(Err(op));\n        }\n        Ok(Ok(check(op, PosixFile::raw)?.perform()))", POSIX),
 # E7 C1: each cached write's exact fate (kept whole, torn at whole
 # blocks and never past its end, lost, lost past the list); its length
 # in blocks; the step probe before each step that can change a crash
 (SIMW, '            Some(Fate::Kept) => w.data.len(),', '            Some(Fate::Kept) => 0,', MODEL),
 (SIMW, '            Some(Fate::Torn(n)) => n.saturating_mul(lb).min(w.data.len()),', '            Some(Fate::Torn(_)) => w.data.len(),', MODEL),
 (SIMW, '            Some(Fate::Torn(n)) => n.saturating_mul(lb).min(w.data.len()),', '            Some(Fate::Torn(n)) => (*n).min(w.data.len()),', MODEL),
 (SIMW, '            Some(Fate::Lost) | None => 0,', '            Some(Fate::Lost) | None => w.data.len(),', MODEL),
 (SIMW, '            .iter()\n            .map(|w| (w.file, w.offset, w.data.len().div_ceil(lb)))', '            .iter()\n            .map(|w| (w.file, w.offset, w.data.len().div_ceil(lb) & 1))', MODEL),
 (SIMW, '            .iter()\n            .map(|w| (w.file, w.offset, w.data.len().div_ceil(lb)))', '            .iter()\n            .map(|w| (w.file, 0, w.data.len().div_ceil(lb)))', MODEL),
 (SIMW, '        for f in self.files.values_mut() {\n            f.durable_len = f.visible.len() as u64;\n        }\n        for d in self.dirs.values_mut() {\n            d.durable = d.entries.clone();\n        }', '        for d in self.dirs.values_mut() {\n            d.durable = d.entries.clone();\n        }', MODEL),
 (SIMW, '            f.durable_len = f.visible.len() as u64;\n        }\n        for d in self.dirs.values_mut() {\n            d.durable = d.entries.clone();\n        }', '            f.durable_len = f.visible.len() as u64;\n        }', MODEL),
 (SIMP, '        if let Err(raw) = step(&self.world) {', '        if let Err(raw) = step(&self.world).and(Ok(())).or(Ok::<(), OsError>(())) {', MODEL),
 (SIMP, '        step(&self.world)?;\n        let mut w = lock(&self.world);\n        dir.check(&w)?;\n        let id = w.create_file(', '        let mut w = lock(&self.world);\n        dir.check(&w)?;\n        let id = w.create_file(', MODEL),
 (SIMP, '        if create {\n            step(&self.world)?;\n        }', '        if create && false {\n            step(&self.world)?;\n        }', MODEL),
 (SIMP, '    if cut {\n        w.cut_power();\n    }', '    if cut && false {\n        w.cut_power();\n    }', MODEL),
 (SIMW, '        self.powered_off = true;', '        self.powered_off = false;', MODEL),
 (SIMP, '            } else if w.powered_off() || w.handle_lost(file, opened) {\n                Err(EIO)\n            } else if !aligned(w, offset, &buf) {\n                w.count_misaligned_read();', '            } else if w.handle_lost(file, opened) {\n                Err(EIO)\n            } else if !aligned(w, offset, &buf) {\n                w.count_misaligned_read();', MODEL),
 (SIMP, '            } else if w.powered_off() || w.handle_lost(file, opened) {\n                Err(EIO)\n            } else {\n                w.complete_flush(file, cover)', '            } else if w.handle_lost(file, opened) {\n                Err(EIO)\n            } else {\n                w.complete_flush(file, cover)', MODEL),
 (SIMW, '    mark(\n        &mut f.holes,\n        before.get(&id).copied().unwrap_or(0),\n        f.durable_len,\n    );', '    let _ = before.get(&id);', MODEL),
 (SIMW, '        unmark(&mut f.holes, s, e.min(f.durable_len));', '        let _ = (s, e);', MODEL),
 (SIMP, '            } else if w.powered_off() || w.handle_lost(file, opened) {\n                Err(EIO)\n            } else if !aligned(w, offset, &buf) {\n                Err(EINVAL)', '            } else if w.handle_lost(file, opened) {\n                Err(EIO)\n            } else if !aligned(w, offset, &buf) {\n                Err(EINVAL)', MODEL),
 (SIMW, '        self.powered_off = false;\n        self.epoch += 1;', '        self.epoch += 1;', MODEL),
 (SIMP, '        step(&self.world)?;\n        let mut w = lock(&self.world);\n        dir.check(&w)?;\n        w.rename(&dir.path', '        let mut w = lock(&self.world);\n        dir.check(&w)?;\n        w.rename(&dir.path', MODEL),
 (SIMP, '        step(&self.world)?;\n        let mut w = lock(&self.world);\n        dir.check(&w)?;\n        w.rename_noreplace(', '        let mut w = lock(&self.world);\n        dir.check(&w)?;\n        w.rename_noreplace(', MODEL),
 (SIMP, '        step(&self.world)?;\n        let mut w = lock(&self.world);\n        dir.check(&w)?;\n        w.unlink(', '        let mut w = lock(&self.world);\n        dir.check(&w)?;\n        w.unlink(', MODEL),
 (SIMP, '        step(&self.world)?;\n        let mut w = lock(&self.world);\n        dir.check(&w)?;\n        w.sync_dir(', '        let mut w = lock(&self.world);\n        dir.check(&w)?;\n        w.sync_dir(', MODEL),
 (SIMP, '        step(&self.world)?;\n        let mut w = lock(&self.world);\n        file.check(&w, true)?;\n        w.allocate(file.id, len)', '        let mut w = lock(&self.world);\n        file.check(&w, true)?;\n        w.allocate(file.id, len)', MODEL),
 (SIMP, '        step(&self.world)?;\n        let mut w = lock(&self.world);\n        file.check(&w, true)?;\n        w.set_len(', '        let mut w = lock(&self.world);\n        file.check(&w, true)?;\n        w.set_len(', MODEL),
 (SIMP, '        step(&self.world)?;\n        let mut w = lock(&self.world);\n        file.check(&w, true)?;\n        // A deallocation', '        let mut w = lock(&self.world);\n        file.check(&w, true)?;\n        // A deallocation', MODEL),
 (SIMP, '        step(&self.world)?;\n        let mut w = lock(&self.world);\n        file.check(&w, true)?;\n        w.allocate_range(', '        let mut w = lock(&self.world);\n        file.check(&w, true)?;\n        w.allocate_range(', MODEL),
 (SIMP, '        step(&self.world)?;\n        let mut w = lock(&self.world);\n        file.check(&w, true)?;\n        w.flush_all(', '        let mut w = lock(&self.world);\n        file.check(&w, true)?;\n        w.flush_all(', MODEL),
 # E7 C3: a killed process's locks released and handles dead, its cache kept
 (SIMW, '    pub fn kill(&mut self) {\n        self.locks.clear();\n        self.epoch += 1;', '    pub fn kill(&mut self) {\n        self.epoch += 1;', MODEL),
 (SIMW, '    pub fn kill(&mut self) {\n        self.locks.clear();\n        self.epoch += 1;', '    pub fn kill(&mut self) {\n        self.locks.clear();', MODEL),
 (SIMW, '    pub fn restore_power(&mut self) {\n        self.powered_off = false;', '    pub fn restore_power(&mut self) {\n        self.powered_off = true;', MODEL),
 # SIM devices (V2b): a flush covers its own file; a lost device fails
 # its handles; each device its kind, volume and placement.
 (SIMW, '        w.seq <= cover\n            && (w.file == file\n                || (self.cfg.share_flushes && self.device_of(w.file) == self.device_of(file)))', '        w.seq <= cover\n            && (w.file == file\n                || true)', MODEL),
 (SIMW, '        w.seq <= cover\n            && (w.file == file\n                || (self.cfg.share_flushes && self.device_of(w.file) == self.device_of(file)))', '        w.seq <= cover\n            && (w.file == file)', MODEL),
 (SIMP, '        } else if w.handle_lost(self.id, self.opened) {\n            Err(EIO)\n        } else {', '        } else if w.handle_lost(self.id, self.opened) && false {\n            Err(EIO)\n        } else {', MODEL),
 (SIMP, '            } else if w.powered_off() || w.handle_lost(file, opened) {\n                Err(EIO)\n            } else if !aligned(w, offset, &buf) {\n                Err(EINVAL)', '            } else if w.powered_off() || w.handle_lost(file, opened) && false {\n                Err(EIO)\n            } else if !aligned(w, offset, &buf) {\n                Err(EINVAL)', MODEL),
 (SIMW, '        let durable_now = dsync || self.device_config(file).kind == DeviceKind::PowerSafe;', '        let durable_now = dsync || self.cfg.device == DeviceKind::PowerSafe;', MODEL),
 (SIMW, '            .partition(|w| device(self, w.file) == d);', '            .partition(|w| device(self, w.file) == d || true);', MODEL),
 (SIMW, '        self.lost_through[d] = self.opens;', '        self.lost_through[d] = 0;', MODEL),
 (SIMW, '        (self.id << 16) | self.device_of(file) as u64', '        (self.id & 0) | self.device_of(file) as u64', MODEL),
 (SIMW, '        dir.ancestors()\n            .find_map(', '        std::iter::once(dir)\n            .find_map(', MODEL),
 (SIMP, '        w.device_config(file.id)\n            .evidence', '        w.device_config(file.id & 0)\n            .evidence', MODEL),
 # E7 C3: the open drains a dead owner's cached writes before reading a slot
 ('crates/store-io-engine/src/open.rs', '            platform\n                .flush_all(&file)\n                .map_err(|e| Error::DurabilityUnknown {', '            Ok::<(), OsError>(())\n                .map_err(|e| Error::DurabilityUnknown {', CRASH_OPEN),
 # E7 C4: every candidate and damage reported, by slot (spec §9)
 (V2_SRC, '                SlotVerdict::OlderDamaged => SlotDamage::Older,', '                SlotVerdict::OlderDamaged => SlotDamage::Damage,', DAMAGE),
 (V2_SRC, '                SlotVerdict::InFlight if to_heal.is_some_and(|(h, _)| h == i) => {', '                SlotVerdict::InFlight if true => {', DAMAGE),
 (V2_SRC, '                SlotVerdict::Winner if lost_late => SlotDamage::Damage,', '                SlotVerdict::Winner if lost_late && false => SlotDamage::Damage,', DAMAGE),
 (V2_SRC, '            report.damaged_slots.push(DamagedSlot {\n                page,', '            let _ = std::convert::identity(DamagedSlot {\n                page,', DAMAGE),
 (V2_SRC, '        let page = i.checked_sub(1).map(|p| p as u32);', '        let page = i.checked_sub(1).map(|_| 0u32);', DAMAGE),
 (V2_SRC, '        for (slot_b, verdict) in [(false, v.a), (true, v.b)] {', '        for (slot_b, verdict) in [(true, v.a), (false, v.b)] {', DAMAGE),
 # E7 C6: a region cut short has its header judged alone (spec §9 step 4):
 # its own extents, not its span, supersede; its span clamped to the file
 # (the free map's end: placement); its usage not counted; growth refused
 # until it is repaired
 ('crates/store-io-engine/src/open2.rs', '        e.span_end.min(container_len)\n    } else {', '        e.span_end.max(container_len & 0)\n    } else {', ['-p', 'store-io-engine', '--test', 'unavailable', '--test', 'format2_repair']),
 ('crates/store-io-engine/src/open2.rs', '                Err(_) if short => raw\n                    .check(&EntryLimits {', '                Err(_) if false => raw\n                    .check(&EntryLimits {', ['-p', 'store-io-engine', '--test', 'format2_repair']),
 ('crates/store-io-engine/src/open2.rs', '            if !entry_short {\n                usage.push((', '            if true {\n                usage.push((', ['-p', 'store-io-engine', '--test', 'format2_repair']),
 ('crates/store-io-engine/src/open2.rs', '            if short {\n                c.outcome = Err(Unavailable::ContainerShort {', '            if false {\n                c.outcome = Err(Unavailable::ContainerShort {', ['-p', 'store-io-engine', '--test', 'format2_repair']),
 ('crates/store-io-engine/src/open2.rs', '                usage: (!c.short)\n                    .then(', '                usage: true\n                    .then(', ['-p', 'store-io-engine', '--test', 'format2_repair']),
 ('crates/store-io-engine/src/open2.rs', '                short: c.short,', '                short: false,', ['-p', 'store-io-engine', '--test', 'format2_repair']),
 # macOS M2: only a built-in device is qualified on macOS (an unknown one
 # is not); a writable open's probe failing (the flush check) is
 # DurabilityUnknown, a reader's plain Io
 ('crates/store-io-core/src/decide.rs', '    if ev.platform == PlatformKind::MacOs && d.internal != Tri::Yes {', '    if ev.platform == PlatformKind::MacOs && d.internal == Tri::No {', ['-p', 'store-io-core', '--lib', 'decide::']),
 ('crates/store-io-engine/src/open.rs', '            if read_only {\n                io(Op::Probe, e)', '            if true {\n                io(Op::Probe, e)', ['-p', 'store-io-engine', '--test', 'refused']),
 ('crates/store-io-engine/src/open.rs', '            if read_only {\n                io(Op::Probe, e)', '            if false {\n                io(Op::Probe, e)', ['-p', 'store-io-engine', '--test', 'refused']),
 # A create under a parent this caller may not read syncs the parent's file
 # system instead (its entry is there too)
 ('crates/store-io-posix/src/dir.rs', '        Err(e) if matches!(e.code, libc::EACCES | libc::EPERM) => {\n            sys::fsync(dirfd)', '        Err(e) if e.code < 0 => {\n            sys::fsync(dirfd)', POSIX + ['--test', 'linux']),
 # macOS (M4): the ownership lock, space, the flush state, the probe's
 # rules. Run by the macOS mutation job (`--only-platform`).
 (MAC_LOCK, '                let t = if shared { libc::F_RDLCK } else { libc::F_WRLCK };', '                let t = if shared { libc::F_RDLCK } else { libc::F_RDLCK };', MAC_LIB),
 (MAC_LOCK, '            Kind::Ofd => sys::ofd_setlk(raw, libc::F_UNLCK),', '            Kind::Ofd => Ok(()),', MAC_LIB),
 (MAC_ALLOC, '    if len > held {', '    if false && len > held {', MAC_LIB),
 (MAC_ALLOC, '            sys::punch_hole(file.raw(), off, l)', '            sys::punch_hole(file.raw(), off, l & 0)', MAC_LIB),
 (MAC_ALLOC, '    if len == 0 {\n        return Ok(());', '    if len == 0 && false {\n        return Ok(());', ['-p', 'store-io', '--test', 'conformance']),
 (MAC_DIR, '            Err(was) => was == REFUSED,', '            Err(_) => true,', MAC_LIB),
 (MAC_DIR, '            ACCEPTED => Tri::Yes,', '            ACCEPTED => Tri::Unknown,', MAC_LIB),
 (MAC_DIR, '        Err(e) if matches!(e.code, libc::EACCES | libc::EPERM) => None,', '        Err(e) if e.code < 0 => None,', MAC_LIB),
 (MAC_PROBE, '    if file.is_writable() && state.full() {', '    if state.full() {', MAC_LIB),
 (MAC_PROBE, '    if removable_mount {', '    if false && removable_mount {', MAC_LIB),
 (MAC_PROBE, '        || d.tunnelled == Some(true)', '        || false', MAC_LIB),
 (MAC_PROBE, '        || d.single == Some(false)', '        || false', MAC_LIB),
 (MAC_PROBE, '        || is_disk_image(d)', '        || (is_disk_image(d) && false)', MAC_LIB),
 (MAC_PROBE, '        || is_vm_disk(d)', '        || (is_vm_disk(d) && false)', MAC_LIB),
 (MAC_PROBE, '    let fixed = d.removable == Some(false) && d.ejectable == Some(false);', '    let fixed = d.removable != Some(true) && d.ejectable != Some(true);', MAC_LIB),
 (MAC_PROBE, '        (false, Some(true)) if fixed && lineage => Tri::Yes,', '        (false, Some(true)) if fixed && (lineage || true) => Tri::Yes,', MAC_LIB),
 (MAC_PROBE, '    ok.then(|| CString::new(name).ok()).flatten()', '    (ok || true).then(|| CString::new(name).ok()).flatten()', MAC_LIB),
 # Windows (M4): a made directory is removed only through a handle checked
 # to be on it, with POSIX semantics (the name goes at once). Run by the
 # Windows mutation job.
 ('crates/store-io-win/src/dir.rs', '    if sys::file_id(&d)? != sys::file_id(h)? {', '    if sys::file_id(&d)? != sys::file_id(h)? && false {', WIN_LIB),
 ('crates/store-io-win/src/dir.rs', '    match sys::set_file_info(&d, FileDispositionInfoEx, &flags.to_le_bytes()) {', '    match sys::set_file_info(&d, FileDispositionInfo, &[u8::from(flags != 0)]) {', WIN_LIB),
 # v0.5 audit (.dev/design/v05-guard-audit.md): guards without an entry.
 # G1: a read-only region refuses writes.
 ('crates/store-io-engine/src/io.rs', '        if r.is_read_only() {\n            return Err(not_written(NotWrittenCause::ReadOnly, ctx(r)));', '        if r.is_read_only() && false {\n            return Err(not_written(NotWrittenCause::ReadOnly, ctx(r)));', AUD_RO),
 # G2: the open's free space leaves out everything allocated.
 ('crates/store-io-engine/src/open2.rs', '                    allocated.push(range);\n                    stale_extra.push', '                    let _ = range;\n                    stale_extra.push', AUD_OPEN),
 ('crates/store-io-engine/src/open2.rs', '                allocated.push((e.span_start, e.span_end - e.span_start));\n                reserved.push', '                reserved.push', AUD_OPEN),
 ('crates/store-io-engine/src/open2.rs', '    let mut allocated: Vec<Extent> = gone.iter().filter_map(|g| g.1).collect();\n    // Every unavailable region\'s span: those above, then the demoted.\n    let mut reserved: Vec<Extent> = allocated.clone();', '    let mut allocated: Vec<Extent> = Vec::new();\n    // Every unavailable region\'s span: those above, then the demoted.\n    let mut reserved: Vec<Extent> = gone.iter().filter_map(|g| g.1).collect();', AUD_OPEN),
 ('crates/store-io-engine/src/open2.rs', '        allocated.extend_from_slice(&ranges);', '        let _ = &ranges;', AUD_OPEN),
 ('crates/store-io-engine/src/open2.rs', 'FreeMap::complement(table_end, container_len, &mut allocated.clone())', 'FreeMap::complement(table_end, container_len, &mut Vec::new())', AUD_OPEN),
 # G3: placement stays inside free space; names tie.
 ('crates/store-io-engine/src/footprint.rs', '            (at.checked_add(len)? <= end).then_some(at)', '            (at.checked_add(len)? <= end.saturating_add(4096)).then_some(at)', FOOTPRINT),
 ('crates/store-io-engine/src/footprint.rs', '        if len > at {\n            free.push((at, len));', '        if len > at && false {\n            free.push((at, len));', FOOTPRINT),
 ('crates/store-io-engine/src/footprint.rs', '            core::cmp::Ordering::Equal => e.1 += 1,', '            core::cmp::Ordering::Equal => {}', FOOTPRINT),
 ('crates/store-io-engine/src/footprint.rs', '        while next < set.len() && set[next].start < q.start {', '        while next < set.len() && set[next].start <= q.start {', FOOTPRINT),
 # G4: a stale page is never rewritten unasked.
 # (No entries for the per-page standing checks: write_page's (v2.rs), growth's
 # (grow2.rs), release's (lifecycle2.rs), provisioning's commit
 # (provision2.rs) and repair's (repair.rs). They cannot be reached: the
 # store-wide `frozen` flag is set while any page is stale or lost, cleared
 # only once every page is Confirmed, and every operation refuses on it
 # first (an entry for each). Kept as backstops.)
 (V2, '            .filter(move |e| e.footprint_seq > ms)', '            .filter(move |e| e.footprint_seq > ms && false)', AUD_STALE),
 ('crates/store-io-engine/src/grow2.rs', '    if page.standing != Standing::Confirmed || page.ahead().next().is_some() {\n        return Err(not_written(NotWrittenCause::Frozen, ctx(r)));', '    if page.standing != Standing::Confirmed {\n        return Err(not_written(NotWrittenCause::Frozen, ctx(r)));', AUD_STALE),
 Joint([('crates/store-io-engine/src/lifecycle2.rs', '            if page.standing != Standing::Confirmed || page.ahead().next().is_some() {\n                return Err(refused(not_written(NotWrittenCause::Frozen, c)));', '            if page.standing != Standing::Confirmed {\n                return Err(refused(not_written(NotWrittenCause::Frozen, c)));'),
        (V2, '        .find(|e| ahead.contains(&(e.seq, e.footprint_seq)))', '        .find(|e| ahead.contains(&(e.seq, e.footprint_seq)) && false)')], F2REP),
 # G6: meta_seq numbering counts a torn copy, and skips only the slot replaced.
 (V2, '        SlotStatus::BadPayloadCrc => pair.facts[i].and_then(|f| f.numbered).map(|n| n.meta_seq),', '        SlotStatus::BadPayloadCrc => pair.facts[i].and_then(|f| f.numbered).map(|n| n.meta_seq).filter(|_| false),', AUD_SEQ),
 # (No entry for next_meta_seq's skip of the slot a write replaces: without
 # it a write is numbered higher, never lower, so no number is reused;
 # an exactness rule, not an integrity guard.)
 # G8: provisioning flushes before its header; a failed or panicked
 # provisioning keeps its holds; no clean close when poisoned.
 ('crates/store-io-engine/src/provision2.rs', '        self.flush_all_accounted()?;\n        self.verify_ready(pl.offset, filled)?;', '        self.verify_ready(pl.offset, filled)?;', AUD_FLUSHES),
 ('crates/store-io-engine/src/provision2.rs', '        if std::thread::panicking() {\n            self.store\n                .inner\n                .domain\n                .poison(', '        if std::thread::panicking() && false {\n            self.store\n                .inner\n                .domain\n                .poison(', AUD_PROV),
 ('crates/store-io-engine/src/provision2.rs', '        if self.store.inner.domain.poisoned().is_some() {\n            return;\n        }\n        let inner = &self.store.inner;', '        if self.store.inner.domain.poisoned().is_some() && false {\n            return;\n        }\n        let inner = &self.store.inner;', AUD_PROV),
 # (No entry for the clean close's own poisoned check, v2.rs Drop: equivalent,
 # since durable_slot refuses every slot write on a poisoned domain;
 # format2.rs test_a_poisoned_store_records_no_clean_close holds the pair.)
 # G10: lower-risk guards.
 (V2, '        if vm.table_pages != layout.pages\n            || vm.base.log2_block_size != layout.log2_block\n            || vm.base.log2_table_slot_size != layout.log2_table\n        {', '        if false\n        {', AUD_SEQ),
 # (No entry for the heal's slot match, v2.rs: plan_next writes the slot not
 # holding the winner, and a torn copy is never the winner, so the heal
 # always lands on the torn slot; an invariant, not a reachable refusal.)
 ('crates/store-io-engine/src/lifecycle2.rs', '                if inner.domain.poisoned().is_none() {\n                    r.gate.restore();', '                if true {\n                    r.gate.restore();', AUD_PROV),
 ('crates/store-io-engine/src/provision2.rs', '        if live\n            || meta.unavailable', '        if (live && false)\n            || meta.unavailable', AUD_PROV),
 ('crates/store-io-engine/src/grow2.rs', '        if new / block >= u64::from(u32::MAX) {\n            return Err(too_large());', '        if new / block >= u64::from(u32::MAX) && false {\n            return Err(too_large());', AUD_PROV),
 # G11: the format crate's field checks (fail safe: unavailable or corrupt).
 # (Each `A || B` check: one entry removes A, another B.)
 (M2_SRC, '        if self.span_start % block != 0 || self.span_end % block != 0 {', '        if self.span_end % block != 0 {', AUD_CODEC),
 (M2_SRC, '        if reserved0 != 0 || reserved1 != 0 {', '        if (reserved0 != 0 && false) || reserved1 != 0 {', AUD_CODEC),
 (M2_SRC, '    if header.count > max_entries || u32::from(header.count) > header.next_seq {', '    if (header.count > max_entries && false) || u32::from(header.count) > header.next_seq {', AUD_CODEC),
 (M2_SRC, '        if h.region_id != region_id {', '        if h.region_id != region_id && false {', AUD_CODEC),
 (M2_SRC, '        if h.kind != self.kind {', '        if h.kind != self.kind && false {', AUD_CODEC),
 (M2_SRC, '        if h.flags != self.flags {', '        if h.flags != self.flags && false {', AUD_CODEC),
 (M2_SRC, '        if h.state != RegionState::Ready {', '        if h.state != RegionState::Ready && false {', AUD_CODEC),
 (M2_SRC, '        if self.flags & !KNOWN_FLAGS != 0 {\n            return Err(MetaError::Field("flags"));\n        }\n        if !LOG2_RANGE', '        if self.flags & !KNOWN_FLAGS != 0 && false {\n            return Err(MetaError::Field("flags"));\n        }\n        if !LOG2_RANGE', AUD_CODEC),
 (M2_SRC, '        if !LOG2_RANGE.contains(&lim.log2_block) {', '        if !LOG2_RANGE.contains(&lim.log2_block) && false {', AUD_CODEC),
 (M2_SRC, '        if RegionState::from_u8(self.state)? != RegionState::Ready {', '        if RegionState::from_u8(self.state)? != RegionState::Ready && false {', AUD_CODEC),
 (M2_SRC, '        if self.span_start >= self.span_end || self.span_end > lim.file_len {', '        if self.span_end > lim.file_len {', AUD_CODEC),
 (M2_SRC, '        if self.span_start >= self.span_end || self.span_end > lim.file_len {', '        if self.span_start >= self.span_end {', AUD_CODEC),
 (M2_SRC, '        if self.span_start % block != 0 || self.span_end % block != 0 {', '        if self.span_start % block != 0 || self.span_end % block != 0 && false {', AUD_CODEC),
 (M2_SRC, '            if h % block != 0 {', '            if h % block != 0 && false {', AUD_CODEC),
 (M2_SRC, '        } else if self.extent_count == 0 {', '        } else if self.extent_count == 0 && false {', AUD_CODEC),
 (M2_SRC, '        } else if self.capacity == 0 {', '        } else if self.capacity == 0 && false {', AUD_CODEC),
 (M2_SRC, '        if reserved0 != 0 || reserved1 != 0 {', '        if reserved0 != 0 || reserved1 != 0 && false {', AUD_CODEC),
 (M2_SRC, '        if base.table_offset != 2u64 << base.log2_block_size {', '        if base.table_offset != 2u64 << base.log2_block_size && false {', AUD_CODEC),
 (M2_SRC, '        if clean_close > 1 {', '        if clean_close > 1 && false {', AUD_CODEC),
 (M2_SRC, '        if self.table_end() > file_len {', '        if self.table_end() > file_len && false {', AUD_CODEC),
 (M2_SRC, '    if r.bytes(14)?.iter().any(|&b| b != 0) {', '    if r.bytes(14)?.iter().any(|&b| b != 0) && false {', AUD_CODEC),
 (M2_SRC, '    if header.count > max_entries || u32::from(header.count) > header.next_seq {', '    if header.count > max_entries || u32::from(header.count) > header.next_seq && false {', AUD_CODEC),
 (M2_SRC, '    if flags & !KNOWN_FLAGS != 0 {\n        return Err(MetaError::Field("flags"));\n    }\n    let kind = RegionKind::from_u8(kind)?;', '    if flags & !KNOWN_FLAGS != 0 && false {\n        return Err(MetaError::Field("flags"));\n    }\n    let kind = RegionKind::from_u8(kind)?;', AUD_CODEC),
 (M2_SRC, '    if r.bytes(20)?.iter().any(|&b| b != 0) {', '    if r.bytes(20)?.iter().any(|&b| b != 0) && false {', AUD_CODEC),
 (M2_SRC, '    if !(log2 == log2_block || (log2 == 9 && kind == RegionKind::Append)) {', '    if !(log2 == log2_block || (log2 == 9 && kind == RegionKind::Append)) && false {', AUD_CODEC),
 (M2_SRC, '    if region_generation == 0 {', '    if region_generation == 0 && false {', AUD_CODEC),
 (M2_SRC, '    if fill == FillPattern::Zeros && fill_key != [0; 16] {', '    if fill == FillPattern::Zeros && fill_key != [0; 16] && false {', AUD_CODEC),
 (M2_SRC, '        if object_len != 0\n            || extent_count == 0', '        if false\n            || extent_count == 0', AUD_CODEC),
 (SLOT_SRC, '        || slot[..HEADER_LEN].iter().all(|&b| b == 0)\n        || slot[..8] != MAGIC[..]', '        || slot[..HEADER_LEN].iter().all(|&b| b == 0)', AUD_CODEC),
 # A slot's compat_flags carried on every rewrite (unknown bits kept).
 ('crates/store-io-engine/src/slots.rs', '            info.compat_flags,\n        ),', '            0,\n        ),', F2),
 (SLOT_SRC, '    w.u32(f.compat_flags)?;', '    w.u32(0)?;', F2),
 # G1 P4b: the direct single-chunk write takes only what it can settle as
 # the pump would (inline queues: the simulator's inline mode).
 ('crates/store-io-engine/src/io.rs', '        if data.len() > chunk || r.map.run_left(offset) < padded as u64 {', '        if r.map.run_left(offset) < padded as u64 {', P4B, INLINE_ENV),
 ('crates/store-io-engine/src/io.rs', '        if data.len() > chunk || r.map.run_left(offset) < padded as u64 {', '        if data.len() > chunk {', P4B, INLINE_ENV),
 ('crates/store-io-engine/src/io.rs', '                    (Ok(n), Some(buf)) if n == buf.len() => Pumped::All,', '                    (Ok(_), Some(_)) => Pumped::All,', P4B, INLINE_ENV),
 ('crates/store-io-engine/src/io.rs', '                obs.add(Counter::Refused, 1);\n                Ok(Pumped::Refused(rejected.raw))', '                obs.add(Counter::Refused, 1);\n                Ok(Pumped::Failed(rejected.raw))', P4B, INLINE_ENV),
 # Remote targets and storage pools are Unverified for what they are; an
 # NVMe namespace is local only over known-local transports, all of them;
 # Windows uses namespace 1's answer only when it is the disk's size.
 ('crates/store-io-core/src/decide.rs', '    if d.bus == Bus::Remote {', '    if false {', DECIDE),
 ('crates/store-io-core/src/decide.rs', '    if ev.stack.contains(&StackLayer::Pool) {', '    if false {', DECIDE),
 (BLOCK, '        return if transports.is_empty() {', '        return if false {', LINUX_PROBE),
 (BLOCK, '            .all(|t| matches!(*t, "pcie" | "apple-nvme"))', '            .all(|_| true)', LINUX_PROBE),
 (BLOCK, '        c.strip_prefix("session")\n            .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))\n            || c.starts_with("rport-")', '        false\n            || c.starts_with("rport-")', LINUX_PROBE),
 (BLOCK, '        c.strip_prefix("session")\n            .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))', '        c.strip_prefix("session")\n            .is_some_and(|_| true)', LINUX_PROBE),
 (BLOCK, '            || c.starts_with("rport-")', '            || false', LINUX_PROBE),
 (BLOCK, '        n.strip_prefix("nvme")\n            .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))', '        n.strip_prefix("nvme")\n            .is_some_and(|_| true)', LINUX_PROBE),
 (BLOCK, '    if !name.starts_with("nvme-subsys") {', '    if false {', LINUX_PROBE),
 (BLOCK, '        .map(|c| read_sysfs(&c.join("transport")).map(|t| t.trim().to_owned()))\n        .collect::<Option<_>>()\n        .unwrap_or_default()', '        .filter_map(|c| read_sysfs(&c.join("transport")).map(|t| t.trim().to_owned()))\n        .collect()', LINUX_PROBE),
 (BLOCK, '    leaf.name.starts_with("nvme") && leaf.bus == Bus::Nvme', '    leaf.name.starts_with("nvme")', LINUX_PROBE),
 (BLOCK, '    .any(|p| name.starts_with(p))', '    .any(|_| false)', LINUX_PROBE),
 (BLOCK, '    matches!(\n        driver,\n        "ib_srp"', '    driver.is_empty() && matches!(\n        driver,\n        "ib_srp"', LINUX_PROBE),
 (WIN_STORAGE, '        BUS_ISCSI | BUS_FIBRE => Bus::Remote,', '        BUS_ISCSI | BUS_FIBRE => Bus::Scsi,', WIN_LIB),
 (WIN_STORAGE, '    (code == BUS_SPACES).then_some(StackLayer::Pool)', '    (code == BUS_SPACES && false).then_some(StackLayer::Pool)', WIN_LIB),
 (WIN_NVME, '    let nsabp = nsfeat & 0b10 != 0;', '    let nsabp = nsfeat & 0b1000 != 0;', WIN_LIB),
 (BLOCK, ' || name.starts_with("nullb")', '', LINUX_PROBE),
 (WIN_NVME, '    disk_bytes != 0 && ns.nsze.checked_mul(u64::from(ns.lba_bytes)) == Some(disk_bytes)', '    ns.nsze.checked_mul(u64::from(ns.lba_bytes)) == Some(disk_bytes)', WIN_LIB),
 (WIN_NVME, '    disk_bytes != 0 && ns.nsze.checked_mul(u64::from(ns.lba_bytes)) == Some(disk_bytes)', '    disk_bytes != 0 && ns.nsze.checked_mul(u64::from(ns.lba_bytes)).is_some()', WIN_LIB),
 # No user code and no wait for room runs with a shared lane held: a
 # scan's visitor (the lane put back first; a bisection's items held back),
 # an append waiting for room in its window.
 (SCAN_SRC, '                lane = None;\n            }\n            // Deliver everything that has arrived, in order.', '            }\n            // Deliver everything that has arrived, in order.', SCAN_TEST),
 (SCAN_SRC, '        let _held = self.held.get_or_insert_with(Vec::new);', '        let _held = self.held.as_ref();', SCAN_TEST),
 (SCAN_SRC, '                                        drop(l);\n                                        out.release();', '                                        out.release();\n                                        drop(l);', SCAN_TEST),
 ('crates/store-io-engine/src/exec.rs', '        self._notify.is_some()', '        false', SCAN_TEST),
 (SCAN_SRC, '        self.scan_region(r, from, to, true, pace.as_ref(), |item, _drain| {', '        self.scan_region(r, from, to, false, pace.as_ref(), |item, _drain| {', SCAN_TEST),
 (SCAN_SRC, '                if lane.is_none() {\n                    match take() {', '                if false {\n                    match take() {', SCAN_TEST),
 ('crates/store-io-engine/src/io.rs', '                    drop(lane);\n                    if !f.wait_completed(seen + 1', '                    if !f.wait_completed(seen + 1', SUBMITTER_TEST),
 # A release whose commit is durable succeeds unless the store is poisoned.
 ("crates/store-io-engine/src/lifecycle2.rs", '            Err(_) if inner.domain.poisoned().is_none() => return Ok(()),', '            Err(Error::NotWritten { .. }) => return Ok(()),', ["-p", "store-io-engine", "--test", "abandoned"]),
 ("crates/store-io-engine/src/lifecycle.rs", '            Err(_) if inner.domain.poisoned().is_none() => return Ok(()),', '            Err(_) if false => return Ok(()),', ["-p", "store-io-engine", "--test", "abandoned"]),
 # A header pair with an unknown ro_compat bit: its region served read-only.
 ('crates/store-io-engine/src/open2.rs', '                    c.decoded = Some((view.header.region_generation, info.writable()));', '                    c.decoded = Some((view.header.region_generation, true));', ["-p", "store-io-engine", "--test", "format2_regions"]),
 # (No entry for plan_next's unknown-ro_compat refusal, slots.rs: every
 # caller is refused first, the region served read-only, a store with
 # such a page or volume record opened read-only; a backstop.)
 # A lock failure other than a holder is `Io`, never `Locked`.
 ('crates/store-io-engine/src/open.rs', '            if store_io_core::errno::is_lock_held(raw) {', '            if store_io_core::errno::is_lock_held(raw) || true {', ["-p", "store-io-engine", "--test", "store"]),
 ('crates/store-io-engine/src/create.rs', '                    if store_io_core::errno::is_lock_held(raw) {', '                    if store_io_core::errno::is_lock_held(raw) || true {', ["-p", "store-io-engine", "--test", "store"]),
 ('crates/store-io-engine/src/create.rs', '            if store_io_core::errno::is_lock_held(raw) {\n                exists()', '            if store_io_core::errno::is_lock_held(raw) || true {\n                exists()', ["-p", "store-io-engine", "--test", "store"]),
 ('crates/store-io-engine/src/copy.rs', '        if is_lock_held(raw) {', '        if is_lock_held(raw) || true {', ["-p", "store-io-engine", "--test", "copy"]),
 ('crates/store-io-engine/src/open.rs', '            platform.sync_dir(&dir).map_err(|e| {', '            Ok::<(), store_io_platform::OsError>(()).map_err(|e| {', COPYT),
 ('crates/store-io-engine/src/open.rs', '            platform.sync_dir(&dir).map_err(|e| {', '            let _ = platform.sync_dir(&dir);\n            Ok::<(), store_io_platform::OsError>(()).map_err(|e| {', COPYT),
 ('crates/store-io-engine/src/open.rs', '                if store_io_core::errno::is_unsupported(e) {\n                    Error::Unsupported {\n                        what: store_io_core::error::Capability::DirectoryFlush,', '                if store_io_core::errno::is_unsupported(e) && false {\n                    Error::Unsupported {\n                        what: store_io_core::error::Capability::DirectoryFlush,', COPYT),
 ('crates/store-io-engine/src/directory.rs', '            if store_io_core::errno::is_lock_held(raw) {', '            if store_io_core::errno::is_lock_held(raw) || true {', ["-p", "store-io-engine", "--test", "directory_changes"]),
 # Format 1's read-only open: a header that names another region, or says
 # released, makes its region unavailable.
 ('crates/store-io-engine/src/open.rs', '            if rm.region_id != e.region_id || rm.kind != e.kind {', '            if rm.kind != e.kind {', ["-p", "store-io-engine", "--test", "format1"]),
 ('crates/store-io-engine/src/open.rs', '            if rm.state != RegionState::Ready {', '            if rm.state != RegionState::Ready && false {', ["-p", "store-io-engine", "--test", "format1"]),
 # A copied region's data is its fill (D-137): a source media error fails
 # the copy with its range; the data lands past the zeroed header blocks;
 # a capped region's slack is still the pattern (Windows writes it). Noted,
 # no entry: the backstops refusing a non-empty append or page region past
 # the fill path (every one with data takes it) and a data area the fill
 # did not cover (the pre-check makes the sizes equal); the empty request
 # starting nothing (step order only); the feed's in-order check
 # and its early-end error
 # (backstops: the scan delivers every byte in order, D-136); a reader that
 # keeps scanning after the fill stopped (wasted reads, then it ends); the
 # reader's error after the last byte was taken (the scan reads nothing
 # past the region's data size); the reader thread's start failure
 # (`Io { op: Queue }`: no test can make a thread fail to start) and a panic
 # in it (resumed on the copying thread).
 (COPY_SRC, "                    (_, Some(e)) | (Err(e), None) => Some(e),\n                    (Ok(_), None) => None,", "                    (Err(e), _) => Some(e),\n                    (Ok(_), _) => None,", COPYT),
 (COPY_SRC, "        if f.full.is_none() && f.kind != RegionKind::Slot && f.data_size > 0 {", "        if f.full.is_none() && f.kind != RegionKind::Slot && f.data_size > 0 && false {", COPYT),
 (COPY_SRC, "                            let at = offset + (i * FEED_PIECE) as u64;", "                            let at = offset + (i * 0) as u64;", COPY_UNIT_T),
 (COPY_SRC, "                        drop(reader);\n                        match handle.join() {", "                        let _kept = &reader;\n                        match handle.join() {", COPYT),
 ("crates/store-io-engine/src/provision2.rs", "                self.write_range(pl.offset, head, 2 * block, 0, data)?;", "                self.write_range(pl.offset, head, 0, 0, data)?;", COPYT),
 ("crates/store-io-engine/src/provision2.rs", "                if filled > head {", "                if filled > head && false {", COPYT),
 ("crates/store-io-engine/src/provision2.rs", "                        &fill_key,\n                        pl.data_size,\n", "                        &fill_key,\n                        0,\n", COPYT),
 # D-139: a waiting barrier yields, once per goal, before it sleeps; the
 # goal of one needing the next flush is the flush before it; yields that
 # keep running out stop, and try again once after the skips
 ('crates/store-io-engine/src/domain/mod.rs', '        if missed >= MISSES {', '        if missed > MISSES {', DOMAIN),
 ('crates/store-io-engine/src/domain/mod.rs', '            self.missed.store(MISSES - 1, Relaxed);', '            self.missed.store(0, Relaxed);', DOMAIN),
 ('crates/store-io-engine/src/domain/mod.rs', '        if skip == 0 {', '        if skip == 0 || true {', DOMAIN),
 ('crates/store-io-engine/src/domain/mod.rs', '        self.skip.store(skip - 1, Relaxed);\n', '        self.skip.store(skip, Relaxed);\n', DOMAIN),
 ('crates/store-io-engine/src/domain/mod.rs', '        if self.missed.load(Relaxed) != 0 {', '        if self.missed.load(Relaxed) != 0 && false {', DOMAIN),
 ('crates/store-io-engine/src/domain/mod.rs', '            self.ran_out();\n', '            if false {\n                self.ran_out();\n            }\n', DOMAIN),
 ('crates/store-io-engine/src/domain/mod.rs', '            self.reached();\n', '            if false {\n                self.reached();\n            }\n', DOMAIN),
 ('crates/store-io-engine/src/domain/mod.rs', '        let reached = || self.done.load(Ordering::SeqCst) >= goal || self.poison.get().is_some();', '        let reached = || self.done.load(Ordering::SeqCst) >= goal;', DOMAIN),
 ('crates/store-io-engine/src/domain/mod.rs', '                let goal = need.min(issued);', '                let goal = need;', DOMAIN),
 ('crates/store-io-engine/src/domain/mod.rs', '                if goal > yielded_to && self.patience.try_yield() {', '                if goal > yielded_to {', DOMAIN),
 ('crates/store-io-engine/src/domain/mod.rs', '                    yielded_to = goal;\n', '                    yielded_to = 0;\n', DOMAIN),
 ('crates/store-io-core/src/identity.rs', '        if !self.complete || !other.complete || self.devices.is_empty() || other.devices.is_empty()', '        if (!self.complete && false) || !other.complete || self.devices.is_empty() || other.devices.is_empty()', IDENT),
 ('crates/store-io-core/src/identity.rs', '        if !self.complete || !other.complete || self.devices.is_empty() || other.devices.is_empty()', '        if !self.complete || (!other.complete && false) || self.devices.is_empty() || other.devices.is_empty()', IDENT),
 ('crates/store-io-core/src/identity.rs', '        if !self.complete || !other.complete || self.devices.is_empty() || other.devices.is_empty()', '        if !self.complete || !other.complete || (self.devices.is_empty() && false) || (other.devices.is_empty() && false)', IDENT),
 ('crates/store-io-core/src/identity.rs', '        self.node == other.node || self.names.iter().any(|n| other.names.contains(n))', '        (self.node == other.node && false) || self.names.iter().any(|n| other.names.contains(n))', IDENT),
 ('crates/store-io-core/src/identity.rs', '        self.node == other.node || self.names.iter().any(|n| other.names.contains(n))', '        self.node == other.node', IDENT),
 ('crates/store-io-core/src/identity.rs', '            if !rest.is_empty()\n                && scheme.len() <= 4', '            if scheme.len() <= 4', IDENT),
 ('crates/store-io-core/src/identity.rs', 'if out.len() + usize::from(space) >= NAME_MAX {', 'if out.len() >= NAME_MAX {', IDENT),
 ('crates/store-io-engine/src/open.rs', '            Ok(now) if now == locked => return Ok((file, lock)),', '            Ok(now) if now == locked || true => return Ok((file, lock)),', STORE_T),
 ('crates/store-io-engine/src/open.rs', '            Err(e) if store_io_core::errno::is_not_found(e) => {}', '            Err(e) if store_io_core::errno::is_not_found(e) => return Ok((file, lock)),', STORE_T),
 ('crates/store-io-engine/src/open.rs', '        if read_only {\n            return Ok((file, lock));\n        }\n        let locked', '        if true {\n            return Ok((file, lock));\n        }\n        let locked', STORE_T),
 ('crates/store-io-win/src/probe/mod.rs', '            Bus::Nvme => controller_named,', '            Bus::Nvme => controller_named || true,', WIN_ID),
 ('crates/store-io-win/src/probe/mod.rs', 'controller_named |= normalize_name(n).is_some();', 'controller_named |= normalize_name(n).is_some() && false;', WIN_ID),
 ('crates/store-io-win/src/probe/mod.rs', '    present && !root\n', '    present && !(root && false)\n', WIN_ID),
 ('crates/store-io-win/src/probe/mod.rs', '        && c.max_leaf >= 0x4000_0004\n', '        && (c.max_leaf >= 0x4000_0004 || true)\n', WIN_ID),
 ('crates/store-io-win/src/probe/mod.rs', '        && c.interface == HV_INTERFACE\n', '        && (c.interface == HV_INTERFACE || true)\n', WIN_ID),
 ('crates/store-io-win/src/probe/mod.rs', '        && c.privileges_hi & (1 << 12) != 0\n', '        && (c.privileges_hi & (1 << 12) != 0 || true)\n', WIN_ID),
 ('crates/store-io-win/src/probe/mod.rs', '        && c.privileges_hi & (1 << 22) == 0\n', '        && (c.privileges_hi & (1 << 22) == 0 || true)\n', WIN_ID),
 ('crates/store-io-win/src/probe/mod.rs', '        && c.max_leaf >= 0x4000_0004\n', '        && c.max_leaf > 0x4000_0004\n', WIN_ID),
 ('crates/store-io-win/src/probe/mod.rs', '    let present = c.leaf1_ecx & (1 << 31) != 0;', '    let present = c.leaf1_ecx & (1 << 31) != 0 || true;', WIN_ID),
 ('crates/store-io-win/src/probe/mod.rs', '    let root = &c.vendor == b"Microsoft Hv"\n', '    let root = (&c.vendor == b"Microsoft Hv" || true)\n', WIN_ID),
 ('crates/store-io-win/src/sys.rs', '    *id != [0; 16] && !low_invalid', '    !low_invalid', WIN_SYS),
 ('crates/store-io-win/src/sys.rs', '    *id != [0; 16] && !low_invalid', '    *id != [0; 16] && !(low_invalid && false)', WIN_SYS),
 ('crates/store-io-win/src/probe/mod.rs', '        && c.recommendations & (1 << 12) == 0;', '        && (c.recommendations & (1 << 12) == 0 || true);', WIN_ID),
 ('crates/store-io-win/src/probe/mod.rs', 'controller_named |= normalize_name(n).is_some();', 'controller_named |= normalize_name(n).is_some() || true;', WIN_ID),
 ('crates/store-io-win/src/probe/storage.rs', 'if !maker.trim().is_empty() && !product.trim().is_empty() {', 'if !maker.trim().is_empty() || !product.trim().is_empty() {', WIN_SMBIOS),
 ('crates/store-io-win/src/probe/mod.rs', '            Bus::Sata => true,\n            _ => false,', '            Bus::Sata => true,\n            _ => true,', WIN_ID),
 ('crates/store-io-win/src/probe/mod.rs', '            Bus::Sata => true,\n            _ => false,', '            Bus::Sata | Bus::Usb => true,\n            _ => false,', WIN_ID),
 ('crates/store-io-win/src/probe/mod.rs', '        if !seen_whole || hosted {', '        if !seen_whole || (hosted && false) {', WIN_ID),
 ('crates/store-io-win/src/probe/mod.rs', '        complete: all,', '        complete: all || true,', WIN_ID),
 ('crates/store-io-posix/src/mac/probe.rs', '        && internal_of(device, removable_mount) == Tri::Yes\n', '        && (internal_of(device, removable_mount) == Tri::Yes || true)\n', MAC_ID),
 ('crates/store-io-win/src/probe/mod.rs', '        let hosted = guest || storage::hypervisor_from(bus, desc.product.as_deref()).is_some();', '        let hosted = (guest && false) || storage::hypervisor_from(bus, desc.product.as_deref()).is_some();', WIN_ID),
 ('crates/store-io-posix/src/probe/identity.rs', '        Bus::Nvme | Bus::Usb => !hosted && named,', '        Bus::Nvme | Bus::Usb => !hosted && (named || true),', LINUX_ID),
 ('crates/store-io-posix/src/probe/identity.rs', '        Bus::Nvme | Bus::Usb => !hosted && named,', '        Bus::Nvme | Bus::Usb => named,', LINUX_ID),
 ('crates/store-io-posix/src/probe/identity.rs', '        Bus::Sata => !hosted,', '        Bus::Sata => true,', LINUX_ID),
 ('crates/store-io-posix/src/probe/identity.rs', '        Bus::Sata => !hosted,\n        _ => false,', '        Bus::Sata => !hosted,\n        _ => true,', LINUX_ID),
 ('crates/store-io-posix/src/probe/identity.rs', '        m.contains("amazon elastic block store") || m.contains("nvme_card-pd")', '        m.contains("nvme_card-pd")', LINUX_ID),
 ('crates/store-io-posix/src/probe/identity.rs', '    let bridge = &device_path[..at];', '    let bridge = &device_path[..at.max(device_path.len())];', LINUX_ID),
 ('crates/store-io-posix/src/probe/identity.rs', '            .is_some_and(|ino| !ino.is_empty() && ino.bytes().all(|b| b.is_ascii_digit()))', '            .is_some_and(|ino| ino.bytes().all(|b| b.is_ascii_digit()))', LINUX_ID),
 ('crates/store-io-posix/src/probe/identity.rs', "            .and_then(|r| r.strip_prefix('-'))", "            .map(|r| r.strip_prefix('-').unwrap_or(r))", LINUX_ID),
 ('crates/store-io-posix/src/probe/identity.rs', '        FsKind::Ext4 => ext4_internal(),', '        FsKind::Ext4 => ext4_internal() || true,', LINUX_ID),
 ('crates/store-io-posix/src/probe/identity.rs', '        FsKind::Xfs => xfs_options().is_some_and(|o| !xfs_uses_other_devices(&o)),\n        _ => false,', '        FsKind::Xfs => xfs_options().is_some_and(|o| !xfs_uses_other_devices(&o)),\n        _ => true,', LINUX_ID),
 ('crates/store-io-posix/src/probe/identity.rs', '        .any(|o| o.starts_with("logdev=") || o.starts_with("rtdev="))', '        .any(|o| o.starts_with("logdev="))', LINUX_ID),
 ('crates/store-io-posix/src/probe/identity.rs', '        .any(|o| o.starts_with("logdev=") || o.starts_with("rtdev="))', '        .any(|o| o.starts_with("rtdev="))', LINUX_ID),
 ('crates/store-io-win/src/probe/storage.rs', '            0 if formatted.get(0x13).is_some_and(|b| b & 0x10 != 0) => guest = true,', '            0 if formatted.get(0x13).is_some_and(|b| b & 0x10 != 0 && false) => guest = true,', WIN_SMBIOS),
 ('crates/store-io-win/src/probe/storage.rs', '                if GUESTS.iter().any(|g| name.contains(g)) {\n                    guest = true;', '                if GUESTS.iter().any(|g| name.contains(g)) && false {\n                    guest = true;', WIN_SMBIOS),
 ('crates/store-io-win/src/probe/storage.rs', '    system && !guest\n', '    (system || true) && !guest\n', WIN_SMBIOS),
 # AFTER: a dependency on an append region must be a prefix receipt.
 (AFTER_SRC, "        if d.append && !d.prefix {", "        if d.append && !d.prefix && false {", MULTI_T),
 # AFTER: an earlier open's receipt is matched by its volume alone.
 (AFTER_SRC, "            None => match reach.iter().position(|s| s.inner.volume == d.volume) {", "            None => match reach.iter().position(|s| s.inner.volume == d.volume && false) {", MULTI_T),
 (AFTER_SRC, "                    Earlier::Other => None,", "                    Earlier::Other => Some(i),", MULTI_T),
 (AFTER_SRC, "                    Earlier::Unknown => {\n                        return Err(not_written(NotWrittenCause::StaleGeneration, c));\n                    }", "                    Earlier::Unknown => None,", MULTI_T),
 (AFTER_SRC, "            if !vouched {", "            if !vouched && false {", MULTI_T),
 # AFTER: the lineage: an open this process did not see, or a close it did
 # not finish cleanly, fences off earlier receipts.
 ("crates/store-io-engine/src/frontier.rs", "    if found.is_none() || lineage.expect != found {", "    if found.is_none() || lineage.expect != found && false {", MULTI_T),
 ("crates/store-io-engine/src/frontier.rs", "        (Some(a), Some(b)) if a != b => Earlier::Other,", "        (Some(a), Some(b)) if a != b && false => Earlier::Other,", FRONTIER),
 ("crates/store-io-engine/src/frontier.rs", "is_some_and(|l| earlier >= l.fence)", "is_some_and(|l| earlier > l.fence)", FRONTIER),
 ("crates/store-io-engine/src/frontier.rs", "    if list.iter().any(|&(e, b)| e >= epoch && b <= at) {", "    if list.iter().any(|&(e, b)| e <= epoch && b <= at) {", FRONTIER),
 ("crates/store-io-engine/src/v2.rs", "            Ok(()) => next_meta_seq_of(&meta).ok(),", "            Ok(()) => next_meta_seq_of(&meta).ok().and(None),", MULTI_T),
 ("crates/store-io-engine/src/open.rs", "                crate::frontier::note_open(volume, id, epoch, found_seq);", "                crate::frontier::note_open(volume, id, epoch, found_seq.map(|s| s + 1));", MULTI_T),
 (AFTER_SRC, "        let Some(region) = region else {\n            return Err(not_written(NotWrittenCause::NotReady, c));\n        };", "        let Some(region) = region else {\n            continue;\n        };", MULTI_T),
 (AFTER_SRC, "        if region.kind != RegionKind::Slot && region.generation != d.generation {", "        if region.kind != RegionKind::Slot && region.generation != d.generation && false {", MULTI_T),
 (AFTER_SRC, "        if region.kind != RegionKind::Slot && region.generation != d.generation {", "        if (region.kind != RegionKind::Slot || true) && region.generation != d.generation {", MULTI_T),
 (AFTER_SRC, "        if d.epoch != store.epoch && region.frontier.is_some() {", "        if d.epoch != store.epoch && region.frontier.is_some() && false {", MULTI_T),
 (AFTER_SRC, "        if d.epoch != store.epoch && region.frontier.is_some() {", "        if d.epoch != store.epoch {", MULTI_T),
 (AFTER_SRC, "            if !region.positioned.load(Ordering::Acquire) {", "            if !region.positioned.load(Ordering::Acquire) && false {", MULTI_T),
 (AFTER_SRC, "            if frontier::resumed_below((d.volume, d.region, d.generation), d.epoch, d.through) {", "            if frontier::resumed_below((d.volume, d.region, d.generation), d.epoch, d.through) && false {", MULTI_T),
 (AFTER_SRC, ".map_err(|why| retired_error(why, r)))", ".map_err(|_| retired_error(crate::gate::Retired::Recycled, r)))", AFTER_RACE),
 # AFTER: no pass taken: a recycle drains under a reserving write.
 (AFTER_SRC, "            .iter()\n            .map(|r| OwnedPass::enter(r)", "            .iter()\n            .filter(|_| false)\n            .map(|r| OwnedPass::enter(r)", AFTER_RACE),
 (AFTER_SRC, "        .add(Counter::Dependencies, deps.len() as u64);", "        .add(Counter::Dependencies, deps.len() as u64 * 0);", DEPS_OBS),
 # AFTER: the append bit set by the ticket and copied by `receipt_for`.
 (IO_SRC, "            need: self.inner.domain.ticket(),\n            epoch: self.inner.epoch,\n            append: r.kind == RegionKind::Append,", "            need: self.inner.domain.ticket(),\n            epoch: self.inner.epoch,\n            append: r.kind != r.kind,", MULTI_T),
 (IO_SRC, "            append: t.append,", "            append: false && t.append,", MULTI_T),
 # AFTER: every resume of the process recorded; a later open below the end counts.
 ("crates/store-io-engine/src/frontier.rs", ".is_some_and(|list| list.iter().any(|&(e, b)| e > epoch && b < through))", ".is_some_and(|list| list.iter().any(|&(e, b)| e >= epoch && b < through))", MULTI_T),
 ("crates/store-io-engine/src/frontier.rs", ".is_some_and(|list| list.iter().any(|&(e, b)| e > epoch && b < through))", ".is_some_and(|list| list.iter().any(|&(e, b)| e > epoch && b <= through))", MULTI_T),
 ("crates/store-io-engine/src/region.rs", "            inner.epoch,\n            at,\n        );", "            0,\n            at,\n        );", MULTI_T),
 # AFTER: the order. Passes before the lane; through the reservation only
 # (taken after it, a refusal leaves a reserved hole); dropped for the wait
 # for room and before the issue, in every path.
 (IO_SRC, "            let lane = self.lane(r, Op::Write)?;\n            let held = before_reserve()?;", "            let held = before_reserve()?;\n            let lane = self.lane(r, Op::Write)?;", AFTER_RACE),
 Joint([
     (IO_SRC, "            let held = before_reserve()?;\n", ""),
     (IO_SRC, "                        held,\n                    ));", "                        before_reserve()?,\n                    ));"),
     (IO_SRC, "                    drop(held);\n                    drop(lane);", "                    drop(lane);"),
 ], AFTER_RACE),
 (IO_SRC, "                    drop(held);\n                    drop(lane);", "                    drop(lane);", AFTER_RACE),
 (IO_SRC, "            drop(before_issue()?);\n            return Ok(self.pump(", "            let _keep = before_issue()?;\n            return Ok(self.pump(", AFTER_RACE),
 (IO_SRC, "        drop(before_issue()?);\n        if self.inner.policy.dsync {\n            self.inner.domain.note_commit();\n        }\n        let obs = &self.inner.observer;", "        let _keep = before_issue()?;\n        if self.inner.policy.dsync {\n            self.inner.domain.note_commit();\n        }\n        let obs = &self.inner.observer;", AFTER_RACE),
 ("crates/store-io-engine/src/region.rs", "        on_reserved(before);", "        let _keep = (before, &on_reserved);", AFTER_RACE),
 (SUB_SRC, "        on_reserved(passes);", "        let _keep = (passes, &on_reserved);", AFTER_RACE),
 (SUB_SRC, "        drop(passes);\n        self.issue(\n            req,", "        let _keep = passes;\n        self.issue(\n            req,", AFTER_RACE),
 # AFTER: a Submitter page write's hook before its buffer is cut to a run.
 (SUB_SRC, "        let passes = admit!(before_issue(), buf);\n        // A background write is admitted by pacing before it is issued.\n        let charge = admit!(self.admit_first(c), buf);\n        let (offset, split) = first_piece(state, pos.offset, &mut buf, self.piece_max);", "        let (offset, split) = first_piece(state, pos.offset, &mut buf, self.piece_max);\n        let passes = admit!(before_issue(), buf);\n        // A background write is admitted by pacing before it is issued.\n        let charge = admit!(self.admit_first(c), buf);", EXTENTS_T),
 # MULTI (V2b): a set barrier matches each ticket to its store and region;
 # no flush where none is needed; receipts by each ticket's kind; a failed
 # store's tickets only what is published; set-wide dependent writes.
 (MULTI_SRC, '                .position(|s| s.inner.volume == t.volume && s.inner.epoch == t.epoch);', '                .position(|s| s.inner.volume == t.volume);', MULTI_T),
 (MULTI_SRC, '            return Ticket::Done(Err(not_written(NotWrittenCause::NotReady, c)));', '            return Ticket::Done(Err(not_written(NotWrittenCause::StaleGeneration, c)));', MULTI_T),
 (MULTI_SRC, '        if region.generation != t.generation || region.gate.check().is_err() {', '        if region.gate.check().is_err() {', MULTI_T),
 (MULTI_SRC, '        if durable >= t.end {\n            store.inner.domain.count_already_durable();', '        if durable >= t.end && false {\n            store.inner.domain.count_already_durable();', MULTI_T),
 (MULTI_SRC, '                    appends = true;\n                    flush |= *synced;', '                    appends = true;\n                    flush |= *synced && false;', MULTI_T),
 (MULTI_SRC, '                    need = need.max(t.need);\n                    flush |= *synced;', '                    need = need.max(t.need);\n                    flush |= *synced && false;', MULTI_T),
 (MULTI_SRC, '        if !flush {', '        if !flush && false {', MULTI_T),
 (MULTI_SRC, '                        let need = if synced { part.need } else { u64::MAX };\n                        Ok(store.receipt_for(t, need))', '                        let need = if synced || true { part.need } else { u64::MAX };\n                        Ok(store.receipt_for(t, need))', MULTI_T),
 (MULTI_SRC, '                        Ok(sync_tail(store, &region, through, need, synced))', '                        Ok(sync_tail(store, &region, through, need, synced || true))', MULTI_T),
 (MULTI_SRC, '    if store.inner.domain.join_if_done(t.need) {', '    if false && store.inner.domain.join_if_done(t.need) {', MULTI_T),
 (MULTI_SRC, '        identity(&self.stores[i]).distinct(&identity(&self.stores[j]))', '        identity(&self.stores[i]).distinct(&identity(&self.stores[i.min(j)]))', MULTI_T),
 (MULTI_SRC, '        self.member(region.store())?;\n        region.append_after_in(', '        region.append_after_in(', MULTI_T),
 (MULTI_SRC, '        self.member(region.store())?;\n        region.write_after_in(', '        region.write_after_in(', MULTI_T),
 (MULTI_SRC, '        region.append_after_in(deps, bytes, &self.stores)', '        region.append_after_in(deps, bytes, core::slice::from_ref(region.store()))', MULTI_T),
 (MULTI_SRC, '        region.write_after_in(deps, pos, bytes, &self.stores)', '        region.write_after_in(deps, pos, bytes, core::slice::from_ref(region.store()))', MULTI_T),
 (MULTI_SRC, '    ) -> Result<RegionPos, Rejected> {\n        if let Err(error) = set.member(&self.store) {', '    ) -> Result<RegionPos, Rejected> {\n        if let Err(error) = set.member(&self.store).or(Ok::<(), Error>(())) {', MULTI_T),
 (MULTI_SRC, '    ) -> Result<(), Rejected> {\n        if let Err(error) = set.member(&self.store) {', '    ) -> Result<(), Rejected> {\n        if let Err(error) = set.member(&self.store).or(Ok::<(), Error>(())) {', MULTI_T),
 (MULTI_SRC, '        self.append_after_with(region, deps, buf, mode, tag, &set.stores)', '        self.append_after_with(region, deps, buf, mode, tag, core::slice::from_ref(region.store()))', MULTI_T),
 (MULTI_SRC, '        self.write_after_with(region, deps, pos, buf, mode, tag, &set.stores)', '        self.write_after_with(region, deps, pos, buf, mode, tag, core::slice::from_ref(region.store()))', MULTI_T),
 # MULTI: answers of another call are dropped (loom); a handed-off store
 # flushed by its own thread; a poison waker registered.
 (MULTI_SRC, '        if st.seq != seq {\n            return;\n        }', '        if st.seq != seq && false {\n            return;\n        }', LOOM_ENGINE, LOOM),
 (MULTI_SRC, '        if let Some(f) = store.flusher() {\n            let waker', '        if let Some(f) = store.flusher().filter(|_| false) {\n            let waker', MULTI_T),
 (MULTI_SRC, '            let key = store.inner.domain.on_poison(waker);', '            let key = {\n                let k = store.inner.domain.on_poison(waker);\n                store.inner.domain.forget_poison_waker(k);\n                k\n            };', MULTI_T),
 # DOMAIN (V2b): every poisoning wakes the poison wakers, after the slot
 # lock is released; a forgotten one is not woken.
 (DOMAIN_SRC, '        for w in wakers {\n            w.poisoned();\n        }', '        for w in wakers.iter().take(0) {\n            w.poisoned();\n        }', POISON_T),
 (DOMAIN_SRC, '    fn wake_poisoned(&self, st: MutexGuard<\'_, Slots>, wakes: Wakes) {\n        drop(st);', '    fn wake_poisoned(&self, st: MutexGuard<\'_, Slots>, wakes: Wakes) {\n        let _held = st;', POISON_T),
 (DOMAIN_SRC, '            .retain(|(k, _)| *k != key);', '            .retain(|(k, _)| *k != key || true);', POISON_T),
 (DOMAIN_SRC, '        self.take_everyone(&mut st, &mut wakes);\n        self.wake_poisoned(st, wakes);\n    }\n\n    /// Registers', '        self.take_everyone(&mut st, &mut wakes);\n        drop(st);\n        wakes.send(self);\n    }\n\n    /// Registers', POISON_T),
 (DOMAIN_SRC, '                self.take_everyone(&mut st, &mut wakes);\n                self.wake_poisoned(st, wakes);', '                self.take_everyone(&mut st, &mut wakes);\n                drop(st);\n                wakes.send(self);', POISON_T),
 (DOMAIN_SRC, '        st.busy &= !(1 << slot);\n        let mut wakes = Wakes::new();\n        self.take_everyone(&mut st, &mut wakes);\n        self.wake_poisoned(st, wakes);', '        st.busy &= !(1 << slot);\n        let mut wakes = Wakes::new();\n        self.take_everyone(&mut st, &mut wakes);\n        drop(st);\n        wakes.send(self);', POISON_T),
 # V2b review: sharing stays on its device; a lost device's reads,
 # flushes, directories and crash fates; answers by store index; fallbacks.
 (SIMW, '        w.seq <= cover\n            && (w.file == file\n                || (self.cfg.share_flushes && self.device_of(w.file) == self.device_of(file)))', '        w.seq <= cover\n            && (w.file == file\n                || (self.cfg.share_flushes && self.device_of(w.file) == self.device_of(w.file)))', MODEL),
 (SIMP, '            } else if w.powered_off() || w.handle_lost(file, opened) {\n                Err(EIO)\n            } else if !aligned(w, offset, &buf) {\n                w.count_misaligned_read();', '            } else if w.powered_off() || w.handle_lost(file, opened) && false {\n                Err(EIO)\n            } else if !aligned(w, offset, &buf) {\n                w.count_misaligned_read();', MODEL),
 (SIMP, '            } else if w.powered_off() || w.handle_lost(file, opened) {\n                Err(EIO)\n            } else {\n                w.complete_flush(file, cover)', '            } else if w.powered_off() || w.handle_lost(file, opened) && false {\n                Err(EIO)\n            } else {\n                w.complete_flush(file, cover)', MODEL),
 (SIMP, '        if w.dir_lost(&self.path, self.opened) {', '        if w.dir_lost(&self.path, self.opened) && false {', MODEL),
 (SIMW, '        let device = d.device;', '        let device = d.device * 0;', MODEL),
 (SIMW, '        let device = self.device_of_dir(&path);', '        let device = self.device_of_dir(&path) * 0;', MODEL),
 (SIMW, '            let keep = kept(&mut self.rng, i, w, mode, lb);\n            if keep > 0 {\n                self.persist(w, keep);\n                reached.push((w.file, w.offset, w.offset + keep as u64));\n            }\n        }\n        for (id, f) in self.files.iter_mut().filter(', '            let keep = kept(&mut self.rng, i + 1, w, mode, lb);\n            if keep > 0 {\n                self.persist(w, keep);\n                reached.push((w.file, w.offset, w.offset + keep as u64));\n            }\n        }\n        for (id, f) in self.files.iter_mut().filter(', MODEL),
 (SIMW, '        for dir in self.dirs.values_mut().filter(|dir| dir.device == d) {', '        for dir in self.dirs.values_mut().filter(|dir| dir.device == d && false) {', MODEL),
 (MULTI_SRC, '            let tag = (self.seq << INDEX_BITS) | i as u64;', '            let tag = (self.seq << INDEX_BITS) | (i as u64 & 0);', MULTI_T),
 (MULTI_SRC, '        if let Some(slot) = st.slots.get_mut(i) {', '        if let Some(slot) = st.slots.get_mut(i & 0) {', MULTI_T),
 (MULTI_SRC, '        self.fallbacks += 1;', '        self.fallbacks += 0;', MULTI_T),
 ('crates/store-io-engine/src/flusher.rs', '        if !crate::test_hook::flush_threads() {', '        if !crate::test_hook::flush_threads() && false {', MULTI_T),
 # MULTI (V2c): a ditto is refused before any write unless every pair of
 # its stores is distinct and every store is in the set; copies in set order.
 (MULTI_SRC, '                if !x.distinct(y) {', '                if !x.distinct(y) && false {', MULTI_T),
 (MULTI_SRC, '                .ok_or_else(|| refuse(NotWrittenCause::ForeignPosition))?;', '                .unwrap_or(0);', MULTI_T),
 (MULTI_SRC, '    order.sort_by_key(|&k| at[k]);', '    order.sort_by_key(|&k| at[k] * 0);', MULTI_T),
 # MULTI (V2c review): results in the order named; every copy written;
 # page positions checked before any write; a lost store's tickets.
 (MULTI_SRC, '    for k in order {\n        out[k] = Some(write(k));\n    }', '    for (j, k) in order.into_iter().enumerate() {\n        out[j] = Some(write(k));\n    }', MULTI_T),
 (MULTI_SRC, '    for k in order {\n        out[k] = Some(write(k));\n    }', '    for k in order {\n        let r = write(k);\n        let failed = r.is_err();\n        out[k] = Some(r);\n        if failed {\n            break;\n        }\n    }', MULTI_T),
 (MULTI_SRC, '            let _end = r.check(pos, bytes.len())?;', '            let _end = r.check(pos, bytes.len());', MULTI_T),
 (MULTI_SRC, '        let at = self.ditto_checked(writes.iter().map(|(r, _)| r.store()))?;', '        let at: Vec<usize> = (0..writes.len()).collect();', MULTI_T),
 (MULTI_SRC, '        if durable >= t.end {\n            return Ok(store.receipt(region, 0, durable, 0, true, synced));', '        if durable >= t.end || true {\n            return Ok(store.receipt(region, 0, t.end.max(durable), 0, true, synced));', MULTI_CRASH),
 # WIN_ID (V2c): a disk's NVMe controller asked again for a new descriptor.
 ('crates/store-io-win/src/probe/mod.rs', '.iter().find(|(n, d, _)| *n == number && d == desc)', '.iter().find(|(n, d, _)| *n == number && (d == desc || true))', WIN_ID),
 ('crates/store-io-win/src/probe/mod.rs', '.iter().find(|(n, d, _)| *n == number && d == desc)', '.iter().find(|(n, d, _)| (*n == number || true) && d == desc)', WIN_ID),

 # POOL (V3a): background buffers never pass half a class; a unit is given
 # back when no buffer was free, and on the buffer's drop.
 ("crates/store-io-buf/src/pool.rs", '            if background && !class.background.try_hold(class.count / 2) {', '            if background && !class.background.try_hold(class.count) {', BUF_LIB),
 ("crates/store-io-buf/src/pool.rs", '            if held >= cap {\n                return false;', '            if held > cap {\n                return false;', BUF_LIB),
 ("crates/store-io-buf/src/pool.rs", '                if background {\n                    class.background.release();\n                }\n                continue;', '                if background && false {\n                    class.background.release();\n                }\n                continue;', BUF_LIB),
 ("crates/store-io-buf/src/pool.rs", '            if self.background {\n                class.background.release();', '            if self.background && false {\n                class.background.release();', BUF_LIB),
 ("crates/store-io-buf/src/pool.rs", '            if held >= cap {\n                return false;', '            if held >= cap.saturating_add(1) {\n                return false;', LOOM_BUF, LOOM),
 # LINUX_ID (V3a): btrfs members gathered once each, from the fsid's
 # directory named as a UUID.
 ("crates/store-io-posix/src/probe/identity.rs", '        for d in found {\n            if !out.contains(&d) {', '        for d in found {\n            if true {', LINUX_ID),
 ("crates/store-io-posix/src/probe/identity.rs", '    for (i, b) in fsid.iter().enumerate() {\n        if matches!(i, 4 | 6 | 8 | 10) {\n            uuid.push(\'-\');\n        }\n        uuid.push_str(&format!("{b:02x}"));\n    }\n    let Ok(entries)', '    for (i, b) in fsid.iter().enumerate() {\n        if matches!(i, 4 | 6 | 8) {\n            uuid.push(\'-\');\n        }\n        uuid.push_str(&format!("{b:02x}"));\n    }\n    let Ok(entries)', LINUX_ID),
 # PROBE_DEV (V3a): every probe names the file's volume as its device.
 ("crates/store-io-posix/src/lib.rs", '                self.file_identity(file).ok().map(|i| i.volume)\n            };', '                None\n            };', POSIX_LINUX_T),
 ("crates/store-io-win/src/platform.rs", '        ev.fs.device = self.file_identity(file).ok().map(|i| i.volume);', '        ev.fs.device = None;', WIN_NTFS_T),
 (SIMP, '            device: Some(w.volume_of(file.id())),', '            device: None,', MODEL),
 (SIMP, '    fn identity_scope(&self) -> u64 {\n        lock(&self.world).id()\n    }', '    fn identity_scope(&self) -> u64 {\n        let _ = lock(&self.world).id();\n        0\n    }', MODEL),
 (SIMW, '        self.clock_ns = self.clock_ns.saturating_add(ns);', '        self.clock_ns = self.clock_ns.saturating_add(ns / 1_000);', MODEL),
 ("crates/store-io-buf/src/pool.rs", '        self.take_from(len, max, true)', '        self.take_from(len, max, false)', BUF_LIB),
 ("crates/store-io-buf/src/pool.rs", '                held + 1,\n                Ordering::Acquire,', '                held + 1,\n                Ordering::Relaxed,', LOOM_BUF, LOOM),
 ("crates/store-io-buf/src/pool.rs", '        let _was = self.held.fetch_sub(1, Ordering::Release);', '        let _was = self.held.fetch_sub(1, Ordering::Relaxed);', LOOM_BUF, LOOM),
 ("crates/store-io-posix/src/mac/platform.rs", '        ev.fs.device = self.file_identity(file).ok().map(|i| i.volume);', '        ev.fs.device = None;', ROOT_CONFORMANCE),
 # QOS (V3b): admission's two conditions and its caps; a charge released on
 # drop and its wake; a failed binding's end; poison read and woken; the
 # unknown count and its seed; the stamp's stride; the group rule; scopes
 # apart; a pruned member's commit kept.
 (QOS_SRC, '                    mine == 0 || rec.is_some_and(|r| r.in_flight() <= 1)', '                    rec.is_some_and(|r| r.in_flight() <= 1)', QOS_T),
 (QOS_SRC, '                    mine == 0 || rec.is_some_and(|r| r.in_flight() <= 1)', '                    mine == 0 || rec.is_some_and(|r| r.in_flight() <= 2)', QOS_T),
 (QOS_SRC, '                } else {\n                    mine + leave < cap || mine == 0\n                };', '                } else {\n                    mine + leave <= cap || mine == 0\n                };', QOS_T),
 (QOS_SRC, '            let paced = rec.is_some_and(|r| r.opts.pacing && self.qos.active_in(r, now));', '            let paced = rec.is_some_and(|r| r.opts.pacing && self.qos.active_in(r, now) && false);', QOS_T),
 (QOS_SRC, '        let _m = i.in_flight.fetch_sub(1, Ordering::SeqCst);', '        let _m = i.in_flight.fetch_sub(0, Ordering::SeqCst);', QOS_T),
 (QOS_SRC, '        let _m = i.in_flight.fetch_sub(1, Ordering::SeqCst);\n        i.wake();', '        let _m = i.in_flight.fetch_sub(1, Ordering::SeqCst);\n        if false {\n            i.wake();\n        }', QOS_T),
 (QOS_SRC, '        if !self.done {\n            self.issuer.end();\n        }', '        if !self.done && false {\n            self.issuer.end();\n        }', QOS_T),
 (QOS_SRC, '            if self.poisoned() {\n                break Err(Ended::Poisoned);', '            if self.poisoned() && false {\n                break Err(Ended::Poisoned);', QOS_T),
 (QOS_SRC, '            if let Some(i) = me.upgrade() {\n                i.wake();\n            }', '            if let Some(i) = me.upgrade().filter(|_| false) {\n                i.wake();\n            }', QOS_T),
 (QOS_SRC, '        if self.waiters.fetch_add(0, Ordering::SeqCst) > 0 {', '        if self.waiters.fetch_add(0, Ordering::SeqCst) > 1 {', QOS_T),
 (QOS_SRC, '                let _was = activity.any_commit.fetch_max(latest, Ordering::Relaxed);', '                let _was = activity.any_commit.fetch_max(latest * 0, Ordering::Relaxed);', QOS_T),
 (QOS_SRC, '            let was = q.scope.unknown_members.fetch_sub(1, Ordering::Relaxed);', '            let was = q.scope.unknown_members.fetch_sub(0, Ordering::Relaxed);', QOS_T),
 (QOS_SRC, '    if now >= at.saturating_add(COMMIT_STRIDE_NS) || at == 0 {', '    if now >= at || at == 0 {', QOS_T),
 (QOS_SRC, '            pacing: self.pacing || other.pacing,', '            pacing: self.pacing && other.pacing,', QOS_T),
 (QOS_SRC, '            commit_quiet_ns: self.commit_quiet_ns.max(other.commit_quiet_ns),', '            commit_quiet_ns: self.commit_quiet_ns.min(other.commit_quiet_ns),', QOS_T),
 (QOS_SRC, '            background_unpaced: self.background_unpaced.min(other.background_unpaced),', '            background_unpaced: self.background_unpaced.max(other.background_unpaced),', QOS_T),
 (QOS_SRC, '                    s.has_unknown()\n                        && (', '                    false\n                        && (', QOS_T),
 (QOS_SRC, '                    s.holds.load(Ordering::Relaxed) > 0\n                        || recent(s.any_commit.load(Ordering::Relaxed), now, quiet)', '                    s.holds.load(Ordering::Relaxed) > 0', QOS_T),
 (QOS_SRC, '.filter(|(_, e)| {\n                e.scope == scope && e.keys.iter().any(|k| keys.binary_search(k).is_ok())\n            })', '.filter(|(_, e)| e.keys.iter().any(|k| keys.binary_search(k).is_ok()))', QOS_T),
 (QOS_SRC, '                    let _was = e\n                        .retired\n                        .fetch_max(c.last_commit.load(Ordering::Relaxed), Ordering::Relaxed);', '                    let _was = e\n                        .retired\n                        .fetch_max(c.last_commit.load(Ordering::Relaxed) * 0, Ordering::Relaxed);', QOS_T),
 (QOS_SRC, '        let _t = self.qos.counters.in_flight.fetch_add(1, Ordering::SeqCst);', '        let _t = self.qos.counters.in_flight.fetch_add(0, Ordering::SeqCst);', QOS_T),
 (QOS_SRC, '        if self.waiters.fetch_add(0, Ordering::SeqCst) > 0 {', '        if self.waiters.load(Ordering::Relaxed) > 0 {', LOOM_ENGINE, LOOM),
 (QOS_SRC, '        let _w = self.waiters.fetch_add(1, Ordering::SeqCst);\n        let out = loop {', '        let _w = self.waiters.fetch_add(1, Ordering::Relaxed);\n        let out = loop {', LOOM_ENGINE, LOOM),
 # QOS (V3b review): a charge leaves its member on release; a refusal undoes
 # its tentative charge; an ended call is seen; an unknown commit stamped for
 # the scope, and the scope's word only while unknown stores live; a watch's
 # own wake; a memory store's hold paces nobody; the seed's own time; the
 # last unknown close clears its commit.
 (QOS_SRC, '        let _c = i.qos.counters.in_flight.fetch_sub(1, Ordering::Relaxed);', '        let _c = i.qos.counters.in_flight.fetch_sub(0, Ordering::Relaxed);', QOS_T),
 (QOS_SRC, '                    let _u = self.qos.counters.in_flight.fetch_sub(1, Ordering::SeqCst);', '                    let _u = self.qos.counters.in_flight.fetch_sub(0, Ordering::SeqCst);', QOS_T),
 (QOS_SRC, '            if self.ended.load(Ordering::SeqCst) {', '            if self.ended.load(Ordering::SeqCst) && false {', QOS_T),
 (QOS_SRC, '                if self.kind == Kind::Unknown {\n                    stamp(&self.scope.unknown_commit, now);', '                if false {\n                    stamp(&self.scope.unknown_commit, now);', QOS_T),
 (QOS_SRC, '                if self.scope.has_unknown() {\n                    stamp(&self.scope.any_commit, now);', '                if true {\n                    stamp(&self.scope.any_commit, now);', QOS_T),
 (QOS_SRC, '        // source: wake now that it can.\n        self.wake();', '        // source: wake now that it can.\n        if false {\n            self.wake();\n        }', QOS_T),
 (QOS_SRC, '        if self.kind != Kind::Unpaced {\n            let _h = self.counters.holds.fetch_add(1, Ordering::Relaxed);', '        if true {\n            let _h = self.counters.holds.fetch_add(1, Ordering::Relaxed);', QOS_T),
 (QOS_SRC, '                let latest = g.latest_commit(scope).max(now);', '                let latest = g.latest_commit(scope).max(now * 0);', QOS_T),
 (QOS_SRC, '            if was == 1 {', '            if was == 1 && false {', QOS_T),
 (QOS_SRC, '                q.scope.any_commit.store(0, Ordering::Relaxed);', '                let _ = &q.scope.any_commit;', QOS_T),
 (QOS_SRC, '        // source: wake now that it can.\n        self.wake();', '        // source: wake now that it can.\n        if false {\n            self.wake();\n        }', LOOM_ENGINE, LOOM),
 # QOS_STORE (V3c1): a commit-class barrier's entry and a power-safe store's dsync
 # writes mark commits active; the pacing options checked; a memory file
 # system's store never paced; the join's group and clock reach the domain.
 ("crates/store-io-engine/src/domain/mod.rs", '        if class.is_commit() {\n            self.note_commit();\n        }', '        if class.is_commit() && false {\n            self.note_commit();\n        }', QOS_STORE_T),
 (IO_SRC, '        drop(before_issue()?);\n        if self.inner.policy.dsync {\n            self.inner.domain.note_commit();\n        }', '        drop(before_issue()?);\n        if self.inner.policy.dsync && false {\n            self.inner.domain.note_commit();\n        }', QOS_STORE_T, INLINE_ENV),
 (IO_SRC, '    pub(crate) fn pump(&self, lane: &mut Lane<P::Queue>, src: &mut impl Source) -> Pumped {\n        if self.inner.policy.dsync {', '    pub(crate) fn pump(&self, lane: &mut Lane<P::Queue>, src: &mut impl Source) -> Pumped {\n        if self.inner.policy.dsync && false {', QOS_STORE_T),
 (SUB_SRC, '                if inner.policy.dsync && !self.class.is_background() {\n                    inner.domain.note_commit();\n                }', '                if inner.policy.dsync && false {\n                    inner.domain.note_commit();\n                }', QOS_STORE_T),
 ("crates/store-io-engine/src/store.rs", '    let ok = io >= 4096\n        && io % 4096 == 0', '    let ok = io >= 4096\n        && (io % 4096 == 0 || true)', QOS_STORE_T),
 ("crates/store-io-engine/src/store.rs", '        && io <= 256 << 10', '        && io <= 512 << 10', QOS_STORE_T),
 ("crates/store-io-engine/src/store.rs", '        && (1..=64).contains(&opts.background_unpaced)', '        && (0..=65).contains(&opts.background_unpaced)', QOS_STORE_T),
 ("crates/store-io-engine/src/store.rs", '        && opts.commit_quiet >= std::time::Duration::from_millis(1);', '        && opts.commit_quiet >= std::time::Duration::ZERO;', QOS_STORE_T),
 ("crates/store-io-engine/src/store.rs", '    if ev.fs.kind == store_io_core::evidence::FsKind::Tmpfs {', '    if ev.fs.kind == store_io_core::evidence::FsKind::Tmpfs && false {', QOS_STORE_T),
 ("crates/store-io-engine/src/store.rs", '        pacing: opts.background_pacing,', '        pacing: true,', QOS_STORE_T),
 ("crates/store-io-engine/src/store.rs", '        background_io: (piece / block * block).max(block),', '        background_io: piece.max(block * 0),', QOS_STORE_T),
 ("crates/store-io-engine/src/open.rs", '    pub fn open(platform: P, path: &Path, opts: StoreOptions) -> Result<Self, Error> {\n        crate::store::check_pacing(&opts)?;', '    pub fn open(platform: P, path: &Path, opts: StoreOptions) -> Result<Self, Error> {\n        let _ = crate::store::check_pacing(&opts);', QOS_STORE_T),
 ("crates/store-io-engine/src/create.rs", '    pub fn create(platform: P, path: &Path, opts: StoreOptions) -> Result<Self, Error> {\n        crate::store::check_pacing(&opts)?;', '    pub fn create(platform: P, path: &Path, opts: StoreOptions) -> Result<Self, Error> {\n        let _ = crate::store::check_pacing(&opts);', QOS_STORE_T),
 ("crates/store-io-engine/src/store.rs", '        platform.identity_scope(),\n        &placement(ev),', '        0,\n        &placement(ev),', QOS_STORE_T),
 # QOS_CLASS (V3c2): only a commit-class barrier marks activity; the pump
 # never submits a piece to a poisoned store, and reports the stop.
 ("crates/store-io-engine/src/domain/mod.rs", '        if class.is_commit() {\n            self.note_commit();\n        }', '        if class.is_commit() || true {\n            self.note_commit();\n        }', DOMAIN_ALL),
 (EXEC_SRC, '            if poisoned() {\n                stopped = true;', '            if poisoned() && false {\n                stopped = true;', PUMP_POISON_T),
 (EXEC_SRC, '            if poisoned() {\n                stopped = true;', '            if poisoned() && false {\n                stopped = true;', PUMP_POISON_T, INLINE_ENV),
 (EXEC_SRC, '        None if stopped => Pumped::Poisoned,', '        None if stopped && false => Pumped::Poisoned,', PUMP_POISON_T),
 ("crates/store-io-engine/src/provision.rs", '            if let Some(first) = inner.domain.poisoned() {\n                return Err(Error::Poisoned { first });', '            if let Some(first) = inner.domain.poisoned().filter(|_| false) {\n                return Err(Error::Poisoned { first });', PUMP_POISON_T),
 # QOS_SUB (V3c3): a background Submitter's first piece admitted before any
 # reservation; its pieces capped at background_io, each charged, released
 # at completion; later pieces parked, never dropped, resubmitted by reap,
 # wait and drop; none to a poisoned store; its buffers capped; its barriers
 # and power-safe writes never commits.
 (SUB_SRC, '                None => Err(not_written(NotWrittenCause::Paced, c)),', '                None => Ok::<_, Error>(None).or(Err(not_written(NotWrittenCause::Paced, c))),', QOS_STORE_T),
 (SUB_SRC, '                inner.qos.background_io() as usize\n', '                usize::MAX\n', QOS_STORE_T),
 (SUB_SRC, '    let piece = (state.map.run_left(start) as usize)\n        .min(total)\n        .min(piece_max);', '    let piece = (state.map.run_left(start) as usize).min(total).min(piece_max.max(usize::MAX));', QOS_STORE_T),
 ("crates/store-io-engine/src/optable.rs", '                    .min(left)\n                    .min(split.piece_max);', '                    .min(left)\n                    .min(split.piece_max.max(usize::MAX));', QOS_STORE_T),
 (SUB_SRC, '        if write {\n            if let Some(first) = self.store.inner.domain.poisoned() {\n                return self', '        if write {\n            if let Some(first) = self.store.inner.domain.poisoned().filter(|_| false) {\n                return self', QOS_STORE_T),
 (SUB_SRC, '        if self.class.is_background() {\n            IoClass::Background\n        } else {', '        if false {\n            IoClass::Background\n        } else {', QOS_STORE_T),
 (SUB_SRC, '                if inner.policy.dsync && !self.class.is_background() {\n', '                if inner.policy.dsync {\n', QOS_STORE_T),
 (SUB_SRC, '        if self.issuer.is_some() {\n            // Background work never holds more than half of a class (R5.4).', '        if self.issuer.is_some() && false {\n            // Background work never holds more than half of a class (R5.4).', QOS_STORE_T),
 ("crates/store-io-engine/src/flusher.rs", '        let class = if guard.batch.iter().any(|r| r.class.is_commit()) {', '        let class = if guard.batch.iter().any(|r| r.class.is_commit()) || true {', QOS_STORE_T),
 (IO_SRC, '            .barrier_classed(need, class, |slot, _k| {', '            .barrier_classed(need, if class.is_commit() || true { IoClass::Commit } else { class }, |slot, _k| {', QOS_STORE_T),
 ("crates/store-io-engine/src/submitter/durable.rs", '.is_some_and(|f| f.request(need, class, qtag, Arc::clone(inbox) as Arc<dyn Answer>))', '.is_some_and(|f| f.request(need, store_io_core::IoClass::Commit, qtag, Arc::clone(inbox) as Arc<dyn Answer>))', QOS_STORE_T),
 ("crates/store-io-engine/src/optable.rs", '        // admitted afresh.\n        live.charge = None;', '        // admitted afresh.\n        let _kept = &live.charge;', QOS_STORE_T),
 # QOS_SUB (V3c3, the review's): continuation pieces paced and parked
 # behind older parked ones; parked pieces poison-checked; a parked
 # piece's operation in flight; reads paced and split; nothing unparked by a
 # drop whose wait failed.
 (SUB_SRC, '            self.admit()\n        } else {\n            None\n        };\n        let Some(charge) = admitted else {', '            self.admit().or(Some(None))\n        } else {\n            None\n        };\n        let Some(charge) = admitted else {', QOS_STORE_T),
 (SUB_SRC, '        let admitted = if self.issue && self.table.parked.is_empty() {', '        let admitted = if self.issue && (self.table.parked.is_empty() || true) {', QOS_STORE_T),
 (SUB_SRC, '        let admitted = if self.issue && self.table.parked.is_empty() {', '        let admitted = if (self.issue || true) && self.table.parked.is_empty() {', QOS_STORE_T),
 (SUB_SRC, '                None if force && self.queue.in_flight() == 0 => {', '                None if force || true => {', QOS_STORE_T),
 (SUB_SRC, '                if let Some(first) = self.store.inner.domain.poisoned() {\n                    let _done = self', '                if let Some(first) = self.store.inner.domain.poisoned().filter(|_| false) {\n                    let _done = self', QOS_STORE_T),
 (SUB_SRC, '        let charge = admit!(self.admit_first(c), buf);\n        let (at, split)', '        let charge = admit!(Ok::<_, Error>(None), buf);\n        let (at, split)', QOS_STORE_T),
 (SUB_SRC, 'let (at, split) = first_piece(state, offset, &mut buf, self.piece_max);', 'let (at, split) = first_piece(state, offset, &mut buf, usize::MAX);', QOS_STORE_T),
 (SUB_SRC, '+ self.core.table.parked.len() +', '+ self.core.table.parked.len() * 0 +', QOS_STORE_T),
 (SUB_SRC, '        let issue = failed.is_none();', '        let issue = failed.is_none() || true;', QOS_STORE_T),
 # QOS_PACE (V3d1): a store created at Background (copy_store's target) is
 # its call's: joined before its metadata, every write a charged piece of at
 # most background_io under the pool's cap, from creation to clean close;
 # its barriers and dsync writes mark no activity; a power-safe slot commit
 # does; a caller's own provisioning unchanged.
 ("crates/store-io-engine/src/domain/mod.rs", '            if a.pace.is_none() {', '            if a.pace.is_none() || true {', QOS_STORE_T),
 ("crates/store-io-engine/src/store.rs", '    if policy.dsync {\n        domain.note_commit();\n    }', '    if policy.dsync && false {\n        domain.note_commit();\n    }', QOS_STORE_T),
 ("crates/store-io-engine/src/store.rs", 'write_slot(q, file, pool, at, next, policy.dsync, domain.pace())', 'write_slot(q, file, pool, at, next, policy.dsync, domain.pace().filter(|_| false))', PACE_T),
 ("crates/store-io-engine/src/slots.rs", '        Some(_) => pool.take_capped(at.size(), usize::MAX),', '        Some(_) => pool.take(at.size()),', PACE_T),
 ("crates/store-io-engine/src/slots.rs", '    let charge = pace.map(crate::pace::Pace::admit).transpose()?;', '    let charge = pace.map(crate::pace::Pace::admit).transpose()?.filter(|_| false);', PACE_T),
 ("crates/store-io-engine/src/provision.rs", '            Some(p) => p.piece() as u64,', '            Some(p) => (p.piece() as u64).max(1 << 20),', QOS_STORE_T),
 ("crates/store-io-engine/src/provision.rs", '                Some(_) => self.take_capped(len as usize, ErrorContext::default())?,', '                Some(_) => self.take(len as usize, ErrorContext::default()).or_else(|_| self.take_capped(len as usize, ErrorContext::default()))?,', PACE_T),
 ("crates/store-io-engine/src/provision.rs", '            let charge = pace.map(crate::pace::Pace::admit).transpose()?;', '            let charge = pace.map(crate::pace::Pace::admit).transpose()?.filter(|_| false);', PACE_T),
 ("crates/store-io-engine/src/create.rs", '        let pace = class\n            .is_background()', '        let pace = (class.is_background() && false)', QOS_STORE_T),
 ("crates/store-io-engine/src/create.rs", '                    created,\n                    pace.as_ref(),', '                    created,\n                    pace.as_ref().filter(|_| false),', PACE_T),
 ("crates/store-io-engine/src/create.rs", '&vat, &next_v, false, pace.as_ref())?;', '&vat, &next_v, false, pace.as_ref().filter(|_| false))?;', PACE_T),
 ("crates/store-io-engine/src/create.rs", '&tat, &next_t, false, pace.as_ref())?;', '&tat, &next_t, false, pace.as_ref().filter(|_| false))?;', PACE_T),
 ("crates/store-io-engine/src/open.rs", '            Some(j) => (j.qos, j.pace),', '            Some(j) => (j.qos, j.pace.filter(|_| false)),', QOS_STORE_T),
 (COPY_SRC, '            store_io_core::IoClass::Background,', '            store_io_core::IoClass::Commit,', QOS_STORE_T),
 (COPY_SRC, 'let piece = pace.map_or(CHUNK, |p| p.piece().min(CHUNK));', 'let piece = pace.map_or(CHUNK, |p| p.piece().max(CHUNK));', COPYS),
 (COPY_SRC, '                    let charge = pace.map(crate::pace::Pace::admit).transpose()?;', '                    let charge = pace.map(crate::pace::Pace::admit).transpose()?.filter(|_| false);', COPYS),
 # QOS_READ (V3d2): a background scan (region.scan, a copy's reader) admits
 # each read in background_io pieces under the pool's cap, its charge
 # released when collected; a refused read reaps its own reads first; a
 # copy's reader shares the fill's record on one device group and drains
 # before blocking on its full feed; a copy's raw reads are paced.
 ('crates/store-io-engine/src/qos.rs', '            (Some(x), Some(y)) => Arc::ptr_eq(&x, &y),', '            (Some(x), Some(y)) => Arc::ptr_eq(&x, &y) && false,', QOS_T),
 ('crates/store-io-engine/src/qos.rs', '            (Some(x), Some(y)) => Arc::ptr_eq(&x, &y),\n            _ => false,', '            (Some(x), Some(y)) => Arc::ptr_eq(&x, &y),\n            _ => true,', QOS_T),
 ('crates/store-io-engine/src/domain/mod.rs', '            .map(|a| crate::pace::Pace::new(&a.qos, Arc::clone(&a.clock)))', '            .filter(|_| false)\n            .map(|a| crate::pace::Pace::new(&a.qos, Arc::clone(&a.clock)))', QOS_STORE_T),
 (SCAN_SRC, '        let pace = self.inner.domain.new_pace();', '        let pace = self.inner.domain.new_pace().filter(|_| false);', QOS_STORE_T),
 (SCAN_SRC, '        let chunk = pace.map_or(self.chunk(), |p| p.piece().min(self.chunk()));', '        let chunk = pace.map_or(self.chunk(), |p| p.piece().max(self.chunk()));', QOS_STORE_T),
 (SCAN_SRC, '                Some(_) => self.inner.pool.take_capped(chunk, chunk).ok(),', '                Some(_) => self.inner.pool.take_within(chunk, chunk).ok(),', PACE_T),
 (SCAN_SRC, '                let charge = match pace {', '                let charge = match pace.filter(|_| false) {', QOS_STORE_T),
 (SCAN_SRC, '                    Some(p) => match p.try_charge() {', '                    Some(p) => match p.try_charge().filter(|_| false) {', QOS_STORE_T),
 (SCAN_SRC, '                        None if in_flight > 0 => {', '                        None if in_flight > 0 && false => {', QOS_STORE_T),
 (SCAN_SRC, '            p.charge = None;', '            let _kept = &p.charge;', QOS_STORE_T),
 (SCAN_SRC, '        if err.is_none() {', '        if err.is_none() && false {', COPY_FEED_T),
 (SCAN_SRC, '        let piece = pace.map(Pace::admit).transpose()?;', '        let piece = pace.map(Pace::admit).transpose()?.filter(|_| false);', QOS_STORE_T),
 (COPY_SRC, '                                        drain() && {', '                                        (true || drain()) && {', COPY_FEED_T),
 (COPY_SRC, '        let reading = dst.reader_pace(&src);', '        let reading = dst.reader_pace(&src).filter(|_| false);', COPY_FEED_T),
 (COPY_SRC, '                            reading.cloned(),', '                            reading.cloned().filter(|_| false),', COPY_FEED_T),
 (COPY_SRC, '        if Qos::same_group(&self.inner.qos, &src.inner.qos) {', '        if Qos::same_group(&self.inner.qos, &src.inner.qos) && false {', COPY_UNIT_T),
 (COPY_SRC, '            src.inner.domain.new_pace()\n', '            src.inner.domain.new_pace().filter(|_| false)\n', QOS_STORE_T),
 (COPY_SRC, '        let chunk = pace.map_or(self.chunk(), |p| p.piece().min(self.chunk()));', '        let chunk = pace.map_or(self.chunk(), |p| p.piece().max(self.chunk()));', COPY_UNIT_T),
 (COPY_SRC, '                Some(_) => self.take_capped(piece.len(), c)?,', '                Some(_) => self.take(piece.len(), c)?,', COPY_UNIT_T),
 (COPY_SRC, '            let admitted = pace.map(Pace::admit).transpose()?;', '            let admitted = pace.map(Pace::admit).transpose()?.filter(|_| false);', COPY_UNIT_T),
 (COPY_SRC, '                    src.read_raw(&f.source, at, file + done, &mut buf[..n], reading)?;', '                    src.read_raw(&f.source, at, file + done, &mut buf[..n], reading.filter(|_| false))?;', COPYS),
 (SCAN_SRC, '                        None if in_flight > 0 => {\n                            free.push(buf);', '                        None if in_flight > 0 => {\n                            drop(buf);', QOS_STORE_T),
 (QOS_SRC, '                    mine + leave < cap || mine == 0', '                    mine + leave * 0 < cap || mine == 0', QOS_T),
 (QOS_SRC, '                    mine + leave < cap || mine == 0', '                    mine + leave < cap', QOS_T),
 ("crates/store-io-engine/src/pace.rs", '            leave: 1,\n', '            leave: 0,\n', COPY_UNIT_T),
 ("crates/store-io-engine/src/pace.rs", '        self.issuer.admit_leaving((self.clock)(), self.leave)', '        self.issuer.admit_leaving((self.clock)(), self.leave * 0)', PACE_T),
]

def kill_stuck_tests():
    if os.name == "nt":
        subprocess.run(["powershell", "-NoProfile", "-Command",
                        "Get-Process | Where-Object { $_.Path -like '*store-io*target*' } | Stop-Process -Force"],
                       capture_output=True)
    else:
        subprocess.run(["pkill", "-f", os.path.join(ROOT, "target")], capture_output=True)


def check_names():
    """No name at the module's top level is assigned twice (plain `name =`
    assignments): a second assignment of a test-selection constant silently
    retargets every entry before it (C10 once made C8's open entries run
    another test file)."""
    import ast
    with open(os.path.abspath(__file__), encoding="utf-8") as fh:
        tree = ast.parse(fh.read())
    seen, twice = set(), []
    for node in tree.body:
        if isinstance(node, ast.Assign):
            for t in node.targets:
                if isinstance(t, ast.Name):
                    if t.id in seen:
                        twice.append(t.id)
                    seen.add(t.id)
    for name in twice:
        print(f"{name} is assigned twice", flush=True)
    return twice


def platform_of(f, pkgs):
    """The system an entry's mutation can only run on (a `sys.platform`
    prefix), or None for any."""
    if f.startswith("crates/store-io-posix/src/mac/"):
        return "darwin"
    if f.startswith("crates/store-io-win/"):
        return "win32"
    if any(k in LINUX_ONLY for k in pkgs):
        return "linux"
    return None


def pkgs_of(entry):
    return list(entry[3]) if len(entry) > 3 else ENGINE


def check_patterns():
    """Every entry's pattern occurs exactly once, and no name is assigned
    twice: run before each commit."""
    bad = check_names()
    for i, entry in enumerate(M):
        texts = {}
        for f, a, b in parts_of(entry):
            if f not in texts:
                with open(os.path.join(ROOT, *f.split("/")), encoding="utf-8") as fh:
                    texts[f] = fh.read()
            n = texts[f].count(a)
            if n != 1:
                print(f"[{i}] {f}: pattern found {n} times: {a[:60]!r}", flush=True)
                bad.append(i)
                break
            texts[f] = texts[f].replace(a, b)
    print(f"{len(M)} entries, {len(bad)} bad", flush=True)
    sys.exit(1 if bad else 0)


def main():
    if sys.argv[1:] == ["--check"]:
        check_patterns()
    if len(sys.argv) == 3 and sys.argv[1] == "--shard":
        # Every N-th entry from K: neighbouring entries (one file's, often
        # with the same slow tests) spread over the shards.
        try:
            k, n = (int(x) for x in sys.argv[2].split("/"))
        except ValueError:
            sys.exit(f"--shard {sys.argv[2]}: expected K/N")
        if not 0 <= k < n:
            sys.exit(f"--shard {k}/{n}: K must be below N")
        only = [i for i in range(len(M)) if i % n == k]
        if not only:
            print(f"--shard {k}/{n}: no entries", flush=True)
            sys.exit(0)
    elif sys.argv[1:] == ["--only-platform"]:
        # The entries no other system's run can do: this system's backend.
        only = [i for i, e in enumerate(M)
                if (need := platform_of(e[0], pkgs_of(e))) and sys.platform.startswith(need)]
        if not only:
            print(f"--only-platform: no entries for {sys.platform}", flush=True)
            sys.exit(0)
    else:
        only = [int(a) for a in sys.argv[1:]]
    survived, missing, invalid = [], [], []
    skipped = []
    for i, entry in enumerate(M):
        f = entry[0]
        pkgs = pkgs_of(entry)
        env = {**os.environ, **entry[4]} if len(entry) > 4 else None
        if only and i not in only:
            continue
        # Every file the entry mutates: its original text, then the mutated.
        originals, mutated, found = {}, {}, True
        for pf, a, b in parts_of(entry):
            path = os.path.join(ROOT, *pf.split("/"))
            if path not in originals:
                with open(path, encoding="utf-8") as fh:
                    originals[path] = fh.read()
                mutated[path] = originals[path]
            if mutated[path].count(a) != 1:
                print(f"[{i}] PATTERN NOT FOUND ({mutated[path].count(a)}) in {pf}: {a[:50]!r}", flush=True)
                found = False
                break
            mutated[path] = mutated[path].replace(a, b)
        if not found:
            missing.append(i)
            continue
        # After the pattern check, so a stale entry is reported everywhere.
        needs = {platform_of(pf, pkgs) for pf, _, _ in parts_of(entry)} - {None}
        need = next(iter(needs), None)
        if need and not sys.platform.startswith(need):
            print(f"[{i}] {f}: skipped ({need} only)", flush=True)
            skipped.append(i)
            continue
        phase = "build"
        try:
            for path, text in mutated.items():
                with open(path, "w", encoding="utf-8", newline="\n") as fh:
                    fh.write(text)
            try:
                # Build first, test second, so a build failure can never be
                # mistaken for a caught mutation. A failed build is retried
                # once after clearing stuck test binaries (on Windows a binary
                # still locked by the previous run can fail a good build); a
                # second failure means the mutation never ran: invalid.
                for attempt in range(2):
                    built = subprocess.run(["cargo", "test", "-q", "--no-run"] + pkgs, cwd=ROOT,
                                           env=env, capture_output=True, text=True, timeout=TIMEOUT)
                    if built.returncode == 0:
                        break
                    kill_stuck_tests()
                    time.sleep(2)
                if built.returncode != 0:
                    out = built.stdout + built.stderr
                    first = next((l for l in out.splitlines() if l.startswith("error")), "")
                    why = f"compile error ({first[:120]})"
                    killed = False
                else:
                    phase = "test"
                    r = subprocess.run(["cargo", "test", "-q"] + pkgs, cwd=ROOT,
                                       env=env, capture_output=True, text=True, timeout=TIMEOUT)
                    killed = r.returncode != 0
                    why = "tests failed" if killed else "SURVIVED"
            except subprocess.TimeoutExpired:
                kill_stuck_tests()
                if phase == "build":
                    # A build that never finished tested nothing: invalid,
                    # never a caught mutation (a cold build can take long).
                    killed, why = False, "compile error (the build timed out)"
                else:
                    # A hang is a caught mutation; clear the stuck test binaries.
                    killed, why = True, "tests hung (caught)"
            print(f"[{i}] {f}: {why}", flush=True)
            if why.startswith("compile error"):
                # A mutation that does not compile tests nothing.
                invalid.append(i)
            elif not killed:
                survived.append(i)
        finally:
            for path, text in originals.items():
                with open(path, "w", encoding="utf-8", newline="\n") as fh:
                    fh.write(text)
    print("survived:", survived, "missing:", missing, "invalid:", invalid,
          "skipped:", skipped, flush=True)
    sys.exit(1 if survived or missing or invalid else 0)


if __name__ == "__main__":
    main()
