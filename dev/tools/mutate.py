#!/usr/bin/env python3
"""Mutation check for store-io's durability call sites (Directives section 4.7, D-086).

Each entry is (file, pattern, replacement[, packages]): a plausible bug at a
flush, barrier, poison, directory-sync, generation-bump, accounting or
buffer-ownership site. For each one the pattern is replaced (it must occur
exactly once), the tests of `packages` run (default: the engine and the
simulator), and the source is restored. A mutation the tests do not kill is a
survivor, and one that does not compile is invalid; either fails the run.
Entries whose packages are Linux-only are skipped on other systems, and
reported as skipped.

Usage: python3 dev/tools/mutate.py [index ...]
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
LINUX_ONLY = {"store-io-posix"}

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
 ("crates/store-io-engine/src/exec.rs", "                (Ok(n), Some(buf)) if n == buf.len() => src.recycle(buf),", "                (Ok(_), Some(buf)) => src.recycle(buf),"),
 ("crates/store-io-engine/src/io.rs", "        if self.r.take().is_some() {\n            self.store.inner.domain.poison(", "        if self.r.take().is_some() && false {\n            self.store.inner.domain.poison("),
 ("crates/store-io-engine/src/batch.rs", "                    self.truncate(at);\n", "                    if false { self.truncate(at); }\n"),
 ("crates/store-io-engine/src/scan.rs", "                if len <= sector {", "                if len <= sector || true {"),
 ("crates/store-io-engine/src/scan.rs", "            Some((_, e)) if *e == start => *e = end,", "            Some((_, e)) if *e == start && false => *e = end,"),
 ("crates/store-io-engine/src/scan.rs", "                if ring[idx].as_ref().is_none_or(|p| p.done.is_none()) {", "                if ring[idx].is_none() {"),
 ("crates/store-io-engine/src/scan.rs", "                            out.unreadable(start + n as u64, start + len as u64);", ""),
 ("crates/store-io-engine/src/scan.rs", "        if (self.visit)(ScanItem::Data { offset, bytes }).is_break() {", "        if (self.visit)(ScanItem::Data { offset, bytes }).is_break() && false {"),
 ("crates/store-io-engine/src/scan.rs", "                let half = (len / unit).div_ceil(2) * unit;", "                let half = len - unit;"),
 ("crates/store-io-engine/src/gate.rs", "            LIVE => Ok(Pass { gate: self }),", "            s if s == LIVE || s != LIVE => Ok(Pass { gate: self }),"),
 ("crates/store-io-engine/src/gate.rs", "        while self.active.fetch_add(0, Ordering::AcqRel) != 0 {", "        while self.active.fetch_add(0, Ordering::AcqRel) != 0 && false {"),
 ("crates/store-io-engine/src/lifecycle.rs", "            .retain(|x| x.id != r.id);", "            .retain(|_| true);"),
 ("crates/store-io-engine/src/lifecycle.rs", "        entry.state = RegionState::Released;", ""),
 ("crates/store-io-engine/src/provision.rs", "            .checked_add(reserved_after)", "            .checked_add(0 * reserved_after)"),
 ("crates/store-io-engine/src/space.rs", "        t.cap.is_none_or(|cap| {", "        true || t.cap.is_none_or(|cap| {"),
 ("crates/store-io-engine/src/provision.rs", "            r.consume(extent)?;", "            if false { r.consume(extent)?; }"),
 ("crates/store-io-engine/src/space.rs", "            a.add(e, block);", "            let _ = (e, block, &mut a);"),
 ("crates/store-io-engine/src/space.rs", "        lock_acct(&self.store.inner.acct).unreserve(self.tag, self.bytes);", ""),
 ("crates/store-io-engine/src/space.rs", "        let mut meta = lock_meta(&inner.meta);\n        let mut acct = lock_acct(&inner.acct);\n        if !acct.fits(tag, bytes) {", "        let mut acct = lock_acct(&inner.acct);\n        let mut meta = lock_meta(&inner.meta);\n        if !acct.fits(tag, bytes) {"),
 ("crates/store-io-engine/src/directory.rs", "        self.platform\n            .sync_dir(&self.dir)\n            .map_err(|raw| self.poison(Op::SyncDir, Some(raw)))\n    }\n\n    /// Writes", "        let _ = self.platform.sync_dir(&self.dir);\n        Ok(())\n    }\n\n    /// Writes"),
 ("crates/store-io-engine/src/directory.rs", "            .set_len(file, len)", "            .set_len(file, len.div_ceil(4096) * 4096)"),
 ("crates/store-io-engine/src/directory.rs", "            .map_err(|raw| self.poison(Op::FlushAll, Some(raw)))", "            .or(Ok(()))"),
 ("crates/store-io-engine/src/directory.rs", "        let _absent_or_removed = self.platform.unlink(&self.dir, &temp);", ""),
 ("crates/store-io-engine/src/provision.rs", "            .any(|e| e.state == RegionState::Ready && e.name == rname)", "            .any(|e| e.state == RegionState::Ready && e.name == rname && false)"),
 ("crates/store-io-engine/src/provision.rs", "        if meta.entries.len() + usize::from(reuse.is_none()) > inner.layout.table_capacity() {", "        if false {"),
 ("crates/store-io-engine/src/provision.rs", "            let zeros = (2 * block).saturating_sub(at).min(len) as usize;", "            let zeros = if at < 2 * block { len as usize } else { 0 };"),
 ("crates/store-io-engine/src/lifecycle.rs", ".release_range(&inner.file, r.data_offset, r.data_size, how)", ".release_range(&inner.file, r.data_offset - 2 * inner.layout.block(), r.data_size + 2 * inner.layout.block(), how)"),
 ("crates/store-io-engine/src/create.rs", "        if holds != Some(false) {", "        if holds != Some(false) && false {"),
 ("crates/store-io-engine/src/provision.rs", "                Err(Error::NoSpace { .. }) if align && from.is_some() => align = false,\n", "                Err(Error::NoSpace { .. }) if align && from.is_some() && false => align = false,\n"),
 # v0.3.1 (D-068..D-072)
 ("crates/store-io-engine/src/create.rs", "        if holds != Some(false) {", "        if holds == Some(true) {"),
 ("crates/store-io-engine/src/slots.rs", "            Some((i, _)) if i == w.index => w.payload,", "            Some((i, _)) if i == w.index && false => w.payload,"),
 ("crates/store-io-engine/src/store.rs", "    Ok(prev.after(written))", "    let _ = written;\n    Ok(prev.clone())"),
 ("crates/store-io-engine/src/io.rs", "                Some(mut lane) => flush_data(&mut lane, file),", "                Some(_lane) => flush_data(&mut self.inner.queues.get(0), file),"),
 ("crates/store-io-engine/src/provision.rs", "                    inner.domain.poison(FirstCause {\n                        op: Some(Op::Write),\n                        raw,\n                    });\n                    self.wake_frontiers();", "                    self.wake_frontiers();"),
 ("crates/store-io-engine/src/region.rs", "        if self.store.inner.read_only {\n            return Err(Error::NotWritten {\n                cause: NotWrittenCause::ReadOnly,", "        if self.store.inner.read_only && false {\n            return Err(Error::NotWritten {\n                cause: NotWrittenCause::ReadOnly,"),
 # D-090: unavailable regions are reported and never NotFound
 ("crates/store-io-engine/src/store.rs", ".find(|u| u.name == name.as_str() && u.kind == kind)", ".find(|u| u.name == name.as_str() && u.kind == kind && false)"),
 ("crates/store-io-engine/src/open.rs", "            read_only,\n            unavailable,\n        };", "            read_only,\n            unavailable: {\n                drop(unavailable);\n                Vec::new()\n            },\n        };"),
 # Read errors are classified and media errors name the exact range
 ("crates/store-io-engine/src/io.rs", "                    if !store_io_core::errno::is_media_error(raw) {", "                    if false {"),
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
 ("crates/store-io-engine/src/space.rs", ".find(|u| matches!(u.reason, crate::store::Unavailable::ContainerShort { .. }))", ".find(|_| false)"),
 ("crates/store-io-engine/src/io.rs", "        if ev.missing.contains(Missing::DioAlignment) || ev.fs.dio_offset_align == 0 {", "        if (ev.missing.contains(Missing::DioAlignment) || ev.fs.dio_offset_align == 0) && false {"),
 ("crates/store-io-engine/src/slots.rs", "    is_media_error(raw) || is_end_of_file(raw)\n", "    is_media_error(raw) || is_end_of_file(raw) || true\n"),
 ("crates/store-io-engine/src/scan.rs", "            Err(_) if len < self.block() => {", "            Err(_) if false => {"),
 # Second review: block-aligned halving; volume search keeps real errors; buffer released before the rescan
 ("crates/store-io-engine/src/scan.rs", "let unit = if len > block { block } else { sector };", "let unit = if len > block && false { block } else { sector };"),
 ("crates/store-io-engine/src/open.rs", "                Err(raw) if !crate::slots::unreadable(raw) => return Err(io(raw)),", "                Err(raw) if !crate::slots::unreadable(raw) && false => return Err(io(raw)),"),
 ("crates/store-io-engine/src/io.rs", ".unwrap_or((start, end));", "?;"),
 # Final review: read_one classification on its own; volume search reports media errors
 ("crates/store-io-engine/src/slots.rs", "        Err(raw) if !unreadable(raw) => return Err(fail(raw)),", "        Err(raw) if !unreadable(raw) && false => return Err(fail(raw)),"),
 ("crates/store-io-engine/src/open.rs", "                    media_error |= store_io_core::errno::is_media_error(raw);", "                    media_error |= store_io_core::errno::is_media_error(raw) && false;"),
 ("crates/store-io-engine/src/open.rs", "            if bytes.len() > store_io_format::meta::VOLUME_META_LEN {", "            if bytes.len() > store_io_format::meta::VOLUME_META_LEN && false {"),
 ("crates/store-io-engine/src/open.rs", "store_io_format::meta::MetaError::Field(\"features\") => newer,", "store_io_format::meta::MetaError::Field(\"features\") if false => newer,"),
 ("crates/store-io-engine/src/open.rs", "            media_error |= pair.media_error;", "            media_error |= pair.media_error && false;"),
 ("crates/store-io-engine/src/open.rs", "        Winner::Lost if p.media_error => Err(Error::Corruption {", "        Winner::Lost if p.media_error && false => Err(Error::Corruption {"),
 # D-095: lane marks flip on take and release; a new operation is first seen anew
 ("crates/store-io-engine/src/exec.rs", "        self.0.store(v.wrapping_add(1), Ordering::Relaxed);", "        self.0.store(v, Ordering::Relaxed);"),
 ("crates/store-io-engine/src/exec.rs", "        mark.store(v.wrapping_add(1), Ordering::Relaxed);", "        mark.store(v, Ordering::Relaxed);"),
 ("crates/store-io-engine/src/stats.rs", "            if mark % 2 == 0 || entry.0 != mark {", "            if mark % 2 == 0 {"),
 ("crates/store-io-engine/src/exec.rs", "        self.0.store(v.wrapping_add(2), Ordering::Relaxed);", "        self.0.store(v, Ordering::Relaxed);"),
 # D-096: a pooled queue's drop waits for its jobs; its depth bounds the pool
 ("crates/store-io-posix/src/queue.rs", "                p.inbox.settle(p.in_flight);", "                if false { p.inbox.settle(p.in_flight); }", POSIX),
 ("crates/store-io-posix/src/queue.rs", "        if self.in_flight() >= self.depth {", "        if self.ready.len() >= self.depth {", POSIX),
 # Starting the pool leaves the caller's signal mask as it was, and the
 # workers open the signals the kernel forces on a faulting thread
 ("crates/store-io-posix/src/sys.rs", "        if let Some(old) = &self.old {", "        if let Some(old) = &self.old.filter(|_| false) {", POSIX),
 ("crates/store-io-posix/src/sys.rs", "    for sig in FAULT_SIGNALS {\n        // SAFETY: sigaddset", "    for sig in FAULT_SIGNALS.iter().copied().filter(|_| false) {\n        // SAFETY: sigaddset", POSIX),
 ("crates/store-io-posix/src/sys.rs", "        if unsafe { libc::sigfillset(set.as_mut_ptr()) } != 0 {", "        if unsafe { libc::sigfillset(set.as_mut_ptr()) } != 0 || true {", POSIX),
 # D-096: each parallel lane owns its own busy mark
 ("crates/store-io-engine/src/exec.rs", ".zip(&self.marks[n..2 * n])", ".zip(&self.marks[0..n])"),
 # D-097: every lane holder starts on an empty queue; an abandoned operation is never success nor a media verdict
 ("crates/store-io-engine/src/exec.rs", "    if let Err(raw) = clear_stale(slot) {", "    if let Err(raw) = Ok::<(), OsError>(()) {"),
 ("crates/store-io-engine/src/io.rs", "        clear_stale(&mut lane).map_err(|raw| Error::Io {", "        { let _never = clear_stale::<P::Queue>; let _keep = &mut lane; Ok::<(), OsError>(()) }.map_err(|raw| Error::Io {"),
 ("crates/store-io-engine/src/slots.rs", "        Ran::Abandoned(raw) => return Err(fail(raw)),", "        Ran::Abandoned(raw) => (None, Err(raw)),"),
 ("crates/store-io-engine/src/io.rs", "                Ran::Abandoned(raw) => {\n                    return Err(Error::Io {\n                        op: Op::Read,\n                        raw,\n                        ctx: c,\n                    });\n                }", "                Ran::Abandoned(raw) => (None, Err(raw)),"),
 ("crates/store-io-engine/src/open.rs", "                Ok(Ran::Abandoned(raw)) | Err((_, Some(raw))) => return Err(io(raw)),", "                Ok(Ran::Abandoned(raw)) => (None, Err(raw)),\n                Err((_, Some(raw))) => return Err(io(raw)),"),
 ("crates/store-io-engine/src/store.rs", "        Ok(Ran::Abandoned(raw)) => Err(Some(raw)),", "        Ok(Ran::Abandoned(_raw)) => Ok(()),"),
 ("crates/store-io-engine/src/slots.rs", "        Ran::Abandoned(raw) => Err(raw),", "        Ran::Abandoned(_raw) => Ok(at.size()),"),
 ("crates/store-io-engine/src/directory.rs", "                Ran::Abandoned(raw) => Err(raw),", "                Ran::Abandoned(_raw) => Ok(piece_len),"),
 ("crates/store-io-engine/src/directory.rs", "                Ran::Abandoned(raw) => return Err(io(Op::Read, raw)),", "                Ran::Abandoned(raw) => (None, Err(raw)),"),
 ("crates/store-io-engine/src/create.rs", "        else {\n            return None;\n        };", "        else {\n            return Some(false);\n        };"),
 ("crates/store-io-engine/src/provision.rs", "                Ran::Abandoned(raw) => (Err(raw), true),", "                Ran::Abandoned(_raw) => (Ok(len as usize), false),"),
 ("crates/store-io-engine/src/provision.rs", "                Err(raw) if !abandoned && store_io_core::errno::is_no_space(raw) => {", "                Err(raw) if (abandoned || !abandoned) && store_io_core::errno::is_no_space(raw) => {"),
]

def kill_stuck_tests():
    if os.name == "nt":
        subprocess.run(["powershell", "-NoProfile", "-Command",
                        "Get-Process | Where-Object { $_.Path -like '*store-io*target*' } | Stop-Process -Force"],
                       capture_output=True)
    else:
        subprocess.run(["pkill", "-f", os.path.join(ROOT, "target")], capture_output=True)


def main():
    only = [int(a) for a in sys.argv[1:]]
    survived, missing, invalid = [], [], []
    skipped = []
    for i, entry in enumerate(M):
        f, a, b = entry[:3]
        pkgs = list(entry[3]) if len(entry) > 3 else ENGINE
        if only and i not in only:
            continue
        p = os.path.join(ROOT, *f.split("/"))
        with open(p, encoding="utf-8") as fh:
            src = fh.read()
        if src.count(a) != 1:
            print(f"[{i}] PATTERN NOT FOUND ({src.count(a)}) in {f}: {a[:50]!r}", flush=True)
            missing.append(i)
            continue
        # After the pattern check, so a stale entry is reported everywhere.
        if not sys.platform.startswith("linux") and any(k in LINUX_ONLY for k in pkgs):
            print(f"[{i}] {f}: skipped (Linux only)", flush=True)
            skipped.append(i)
            continue
        with open(p, "w", encoding="utf-8", newline="\n") as fh:
            fh.write(src.replace(a, b))
        try:
            try:
                # Build first, test second, so a build failure can never be
                # mistaken for a caught mutation. A failed build is retried
                # once after clearing stuck test binaries (on Windows a binary
                # still locked by the previous run can fail a good build); a
                # second failure means the mutation never ran: invalid.
                for attempt in range(2):
                    built = subprocess.run(["cargo", "test", "-q", "--no-run"] + pkgs,
                                       cwd=ROOT, capture_output=True, text=True, timeout=TIMEOUT)
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
                    r = subprocess.run(["cargo", "test", "-q"] + pkgs,
                                       cwd=ROOT, capture_output=True, text=True, timeout=TIMEOUT)
                    killed = r.returncode != 0
                    why = "tests failed" if killed else "SURVIVED"
            except subprocess.TimeoutExpired:
                # A hang is a caught mutation; clear the stuck test binaries.
                kill_stuck_tests()
                killed, why = True, "tests hung (caught)"
            print(f"[{i}] {f}: {why}", flush=True)
            if why.startswith("compile error"):
                # A mutation that does not compile tests nothing.
                invalid.append(i)
            elif not killed:
                survived.append(i)
        finally:
            with open(p, "w", encoding="utf-8", newline="\n") as fh:
                fh.write(src)
    print("survived:", survived, "missing:", missing, "invalid:", invalid,
          "skipped:", skipped, flush=True)
    sys.exit(1 if survived or missing or invalid else 0)


if __name__ == "__main__":
    main()
