#!/usr/bin/env python3
"""Mutation check for store-io's durability call sites (Directives section 4.7, D-086).

Each entry is (file, pattern, replacement): a plausible bug at a flush,
barrier, poison, directory-sync, generation-bump or accounting site. For each
one the pattern is replaced (it must occur exactly once), the engine and
simulator tests run, and the source is restored. A mutation the tests do not
kill is a survivor, and one that does not compile is invalid; either fails
the run.

Usage: python3 dev/tools/mutate.py [index ...]
"""
import os
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
TIMEOUT = int(os.environ.get("MUTATE_TIMEOUT", "600"))

M = [
 ("crates/store-io-engine/src/batch.rs", "                carried = l % block;", "                carried = 0;"),
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
 ("crates/store-io-engine/src/batch.rs", "                    self.truncate(at);\n", ""),
 ("crates/store-io-engine/src/scan.rs", "                if len <= block {", "                if len <= block || true {"),
 ("crates/store-io-engine/src/scan.rs", "            Some((_, e)) if *e == start => *e = end,", "            Some((_, e)) if *e == start && false => *e = end,"),
 ("crates/store-io-engine/src/scan.rs", "                if ring[idx].as_ref().is_none_or(|p| p.done.is_none()) {", "                if ring[idx].is_none() {"),
 ("crates/store-io-engine/src/scan.rs", "                            out.unreadable(start + n as u64, start + len as u64);", ""),
 ("crates/store-io-engine/src/scan.rs", "        if (self.visit)(ScanItem::Data { offset, bytes }).is_break() {", "        if (self.visit)(ScanItem::Data { offset, bytes }).is_break() && false {"),
 ("crates/store-io-engine/src/scan.rs", "                let half = (len / block).div_ceil(2) * block;", "                let half = len - block;"),
 ("crates/store-io-engine/src/gate.rs", "            LIVE => Ok(Pass { gate: self }),", "            _ => Ok(Pass { gate: self }),"),
 ("crates/store-io-engine/src/gate.rs", "        while self.active.fetch_add(0, Ordering::AcqRel) != 0 {", "        while self.active.fetch_add(0, Ordering::AcqRel) != 0 && false {"),
 ("crates/store-io-engine/src/lifecycle.rs", "            .retain(|x| x.id != r.id);", "            .retain(|_| true);"),
 ("crates/store-io-engine/src/lifecycle.rs", "        entry.state = RegionState::Released;", ""),
 ("crates/store-io-engine/src/store.rs", "            .checked_add(reserved_after)", "            .checked_add(0 * reserved_after)"),
 ("crates/store-io-engine/src/space.rs", "        t.cap.is_none_or(|cap| {", "        true || t.cap.is_none_or(|cap| {"),
 ("crates/store-io-engine/src/store.rs", "            r.consume(extent)?;", "            let _ = r;"),
 ("crates/store-io-engine/src/space.rs", "            a.add(e, block);", "            let _ = (e, block, &mut a);"),
 ("crates/store-io-engine/src/space.rs", "        lock_acct(&self.store.inner.acct).unreserve(self.tag, self.bytes);", ""),
 ("crates/store-io-engine/src/space.rs", "        let mut acct = lock_acct(&inner.acct);\n        if !acct.fits(tag, bytes) {", "        let mut acct = lock_acct(&inner.acct);\n        drop(meta); let mut meta = lock_meta(&inner.meta);\n        if !acct.fits(tag, bytes) {"),
 ("crates/store-io-engine/src/directory.rs", "        self.platform\n            .sync_dir(&self.dir)\n            .map_err(|raw| self.poison(Op::SyncDir, Some(raw)))\n    }\n\n    /// Writes", "        let _ = self.platform.sync_dir(&self.dir);\n        Ok(())\n    }\n\n    /// Writes"),
 ("crates/store-io-engine/src/directory.rs", "            .set_len(file, len)", "            .set_len(file, len.div_ceil(4096) * 4096)"),
 ("crates/store-io-engine/src/directory.rs", "            .map_err(|raw| self.poison(Op::FlushAll, Some(raw)))", "            .or(Ok(()))"),
 ("crates/store-io-engine/src/directory.rs", "        let _absent_or_removed = self.platform.unlink(&self.dir, &temp);", ""),
 ("crates/store-io-engine/src/store.rs", "            .any(|e| e.state == RegionState::Ready && e.name == rname)", "            .any(|e| e.state == RegionState::Ready && e.name == rname && false)"),
 ("crates/store-io-engine/src/store.rs", "        if meta.entries.len() + usize::from(reuse.is_none()) > inner.layout.table_capacity() {", "        if false {"),
 ("crates/store-io-engine/src/store.rs", "            let zeros = (2 * block).saturating_sub(at).min(len) as usize;", "            let zeros = if at < 2 * block { len as usize } else { 0 };"),
 ("crates/store-io-engine/src/lifecycle.rs", ".release_range(&inner.file, r.data_offset, r.data_size, how)", ".release_range(&inner.file, r.data_offset - 2 * inner.layout.block(), r.data_size + 2 * inner.layout.block(), how)"),
 ("crates/store-io-engine/src/store.rs", "        if holds != Some(false) {", "        if false {"),
 ("crates/store-io-engine/src/store.rs", "                Err(Error::NoSpace { .. }) if align && from.is_some() => align = false,\n", ""),
 # v0.3.1 (D-068..D-072)
 ("crates/store-io-engine/src/store.rs", "        if holds != Some(false) {", "        if holds == Some(true) {"),
 ("crates/store-io-engine/src/slots.rs", "            Some((i, _)) if i == w.index => w.payload,", "            Some((i, _)) if i == w.index && false => w.payload,"),
 ("crates/store-io-engine/src/store.rs", "    Ok(prev.after(written))", "    let _ = written;\n    Ok(prev.clone())"),
 ("crates/store-io-engine/src/io.rs", "                Some(mut lane) => flush_data(&mut lane, file),", "                Some(_lane) => flush_data(&mut self.inner.queues.get(0), file),"),
 ("crates/store-io-engine/src/store.rs", "                    inner.domain.poison(FirstCause {\n                        op: Some(Op::Write),\n                        raw,\n                    });\n                    self.wake_frontiers();", "                    self.wake_frontiers();"),
 ("crates/store-io-engine/src/region.rs", "        if self.store.inner.read_only {\n            return Err(Error::NotWritten {\n                cause: NotWrittenCause::ReadOnly,", "        if self.store.inner.read_only && false {\n            return Err(Error::NotWritten {\n                cause: NotWrittenCause::ReadOnly,"),
 # D-090: unavailable regions are reported and never NotFound
 ("crates/store-io-engine/src/store.rs", ".find(|u| u.name == name.as_str() && u.kind == kind)", ".find(|u| u.name == name.as_str() && u.kind == kind && false)"),
 ("crates/store-io-engine/src/store.rs", "            read_only,\n            unavailable,\n        };", "            read_only,\n            unavailable: {\n                drop(unavailable);\n                Vec::new()\n            },\n        };"),
 # Read errors are classified and media errors name the exact range
 ("crates/store-io-engine/src/io.rs", "                    if !store_io_core::errno::is_media_error(raw) {", "                    if false {"),
 ("crates/store-io-engine/src/io.rs", "                    let (bad_start, bad_end) = self.first_unreadable(r, start, end)?;", "                    let (bad_start, bad_end) = (start, end);"),
 # Directory: refuse before any write without directory flushes; fail-stop on set_len
 ("crates/store-io-engine/src/directory.rs", "        let dir_flush = platform.sync_dir(&dir).err();", "        let dir_flush = None;"),
 ("crates/store-io-engine/src/directory.rs", "            .map_err(|raw| self.poison(Op::Allocate, Some(raw)))?;", "            .map_err(|raw| io(Op::Allocate, raw))?;"),
 # TYP-02: an already-durable receipt is counted
 ("crates/store-io-engine/src/region.rs", "            s.inner.domain.count_already_durable();", "            {}"),
 # D-092: a read-only open takes the shared lock on its read-only handle
 ("crates/store-io-engine/src/store.rs", "            platform.lock_shared(&file)", "            platform.lock_exclusive(&file)"),
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
    for i, (f, a, b) in enumerate(M):
        if only and i not in only:
            continue
        p = os.path.join(ROOT, *f.split("/"))
        with open(p, encoding="utf-8") as fh:
            src = fh.read()
        if src.count(a) != 1:
            print(f"[{i}] PATTERN NOT FOUND ({src.count(a)}) in {f}: {a[:50]!r}", flush=True)
            missing.append(i)
            continue
        with open(p, "w", encoding="utf-8", newline="\n") as fh:
            fh.write(src.replace(a, b))
        try:
            try:
                r = subprocess.run(["cargo", "test", "-q", "-p", "store-io-engine", "-p", "store-io-sim"],
                                   cwd=ROOT, capture_output=True, text=True, timeout=TIMEOUT)
                out = r.stdout + r.stderr
                killed = r.returncode != 0
                why = "compile error" if "error[" in out else ("tests failed" if killed else "SURVIVED")
            except subprocess.TimeoutExpired:
                # A hang is a caught mutation; clear the stuck test binaries.
                kill_stuck_tests()
                killed, why = True, "tests hung (caught)"
            print(f"[{i}] {f}: {why}", flush=True)
            if why == "compile error":
                # A mutation that does not compile tests nothing.
                invalid.append(i)
            elif not killed:
                survived.append(i)
        finally:
            with open(p, "w", encoding="utf-8", newline="\n") as fh:
                fh.write(src)
    print("survived:", survived, "missing:", missing, "invalid:", invalid, flush=True)
    sys.exit(1 if survived or missing or invalid else 0)


if __name__ == "__main__":
    main()
