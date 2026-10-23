#!/usr/bin/env python3
"""Mutation check for store-io's durability call sites (Directives section 4.7, D-086).

Each entry is (file, pattern, replacement[, packages[, env]]): a plausible bug at a
flush, barrier, poison, directory-sync, generation-bump, accounting or
buffer-ownership site. For each one the pattern is replaced (it must occur
exactly once), the tests of `packages` run (default: the engine and the
simulator) with `env` added to the environment, and the source is restored.
A mutation the tests do not kill is a
survivor, and one that does not compile is invalid; either fails the run.
Entries whose packages are Linux-only are skipped on other systems, and
reported as skipped.

Usage: python3 dev/tools/mutate.py [index ...]
       python3 dev/tools/mutate.py --check   (every pattern occurs exactly once; nothing built)
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
# Format-2 regions of several extents.
F2E = ["-p", "store-io-engine", "--test", "format2_extents"]
# The append frontier's unit tests.
FRONTIER = ["-p", "store-io-engine", "--lib", "frontier::"]
# The region map's unit tests; the format crate's footprint tests.
REGIONMAP = ["-p", "store-io-engine", "--lib", "regionmap::"]
FOOTPRINT_FMT = ["-p", "store-io-format", "--test", "footprint"]
V2 = "crates/store-io-engine/src/v2.rs"
CREATE = "crates/store-io-engine/src/create.rs"

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
 ("crates/store-io-engine/src/provision.rs", "            let zeros = (2 * block).saturating_sub(at).min(len) as usize;", "            let zeros = if at < 2 * block { len as usize } else { 0 };"),
 ('crates/store-io-engine/src/lifecycle.rs', 'release_range(&inner.file, at, len, how)', 'release_range(&inner.file, at - 2 * inner.layout.block(), len + 2 * inner.layout.block(), how)'),
 ("crates/store-io-engine/src/create.rs", "        if holds != Some(false) ||", "        if holds != Some(false) && false ||"),
 ("crates/store-io-engine/src/provision.rs", "                Err(Error::NoSpace { .. }) if align && from.is_some() => align = false,\n", "                Err(Error::NoSpace { .. }) if align && from.is_some() && false => align = false,\n"),
 # v0.3.1 (D-068..D-072)
 ("crates/store-io-engine/src/create.rs", "        if holds != Some(false) ||", "        if holds == Some(true) ||"),
 ("crates/store-io-engine/src/slots.rs", "            Some((i, _)) if i == w.index => w.payload,", "            Some((i, _)) if i == w.index && false => w.payload,"),
 ("crates/store-io-engine/src/store.rs", "    Ok(prev.after(written))", "    let _ = written;\n    Ok(prev.clone())"),
 ("crates/store-io-engine/src/io.rs", "                Some(mut lane) => flush_data(&mut lane, file),", "                Some(_lane) => flush_data(&mut self.inner.queues.get(0), file),"),
 ("crates/store-io-engine/src/provision.rs", "                    inner.domain.poison(FirstCause {\n                        op: Some(Op::Write),\n                        raw,\n                    });\n                    self.wake_frontiers();", "                    self.wake_frontiers();"),
 ("crates/store-io-engine/src/region.rs", "        if self.store.inner.read_only || self.state.read_only {\n            return Err(Error::NotWritten {\n                cause: NotWrittenCause::ReadOnly,", "        if self.state.read_only {\n            return Err(Error::NotWritten {\n                cause: NotWrittenCause::ReadOnly,"),
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
 ("crates/store-io-engine/src/space.rs", ".find(|u| matches!(u.reason, crate::store::Unavailable::ContainerShort { .. }))", ".find(|_| false)"),
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
 ("crates/store-io-posix/src/sys.rs", "        if let Some(old) = &self.old {", "        if let Some(old) = &self.old.filter(|_| false) {", POSIX),
 ("crates/store-io-posix/src/sys.rs", "    for sig in FAULT_SIGNALS {\n        // SAFETY: sigaddset", "    for sig in FAULT_SIGNALS.iter().copied().filter(|_| false) {\n        // SAFETY: sigaddset", POSIX),
 ("crates/store-io-posix/src/sys.rs", "        if unsafe { libc::sigfillset(set.as_mut_ptr()) } != 0 {", "        if unsafe { libc::sigfillset(set.as_mut_ptr()) } != 0 || true {", POSIX),
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
 (SUB, '        while self.core.table.live > 0 && self.core.queue.in_flight() > 0 {', '        while self.core.table.live > 0 && self.core.queue.in_flight() > 0 && false {'),
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
 ('crates/store-io-engine/src/flusher.rs', '        let outcome = held.barrier_to(need).map(|()| need);', '        let outcome = held.barrier_to(need).map(|()| need).or(Ok(need));'),
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
 ('crates/store-io-engine/src/io.rs', '        (self.ordinary() / block * block).max(block)', '        (self.inner.pool.max_len() / block * block).max(block)'),
 (SUB, '        if len > self.max_buf() {', '        if len > self.max_buf() && false {'),
 ('crates/store-io-engine/src/io.rs', '            .take_within(len, self.ordinary())', '            .take(len)'),
 # D-102: a forced region's barriers flush, and only flush-backed receipts say data_synced
 ('crates/store-io-engine/src/io.rs', '        if !self.inner.policy.flush && !r.forced() {', '        if !self.inner.policy.flush && (!r.forced() || true) {'),
 ('crates/store-io-engine/src/optable.rs', '            || store.inner.policy.flush || live.pass.as_ref().is_some_and(|p| p.get().forced());', '            || store.inner.policy.flush;'),
 ('crates/store-io-engine/src/submitter/durable.rs', '        let flush = inner.policy.flush || forced;', '        let flush = inner.policy.flush || (forced && false);'),
 ('crates/store-io-engine/src/region.rs', '        let synced = flushes || self.state.forced();', '        let synced = flushes;'),
 ('crates/store-io-engine/src/io.rs', '            synced: need != u64::MAX && need != 0,', '            synced: (need != u64::MAX && need != 0) || true,'),
 ('crates/store-io-engine/src/lifecycle.rs', '            force_sync: Arc::clone(&r.force_sync),', '            force_sync: Arc::new(std::sync::atomic::AtomicBool::new(false)),'),
 ('crates/store-io-engine/src/optable.rs', '                    receipt.synced = need != u64::MAX;', '                    receipt.synced = true;'),
 ('crates/store-io-engine/src/slot_handle.rs', '        let flushed = s.inner.policy.flush || self.state.forced();', '        let flushed = s.inner.policy.flush;'),
 ('crates/store-io-engine/src/region.rs', '        if synced {\n            f.publish_synced(through);', '        if synced && false {\n            f.publish_synced(through);'),
 ('crates/store-io-engine/src/submitter/durable.rs', '                .any(|r| r.forced());', '                .any(|r| r.forced() && false);'),
 ('crates/store-io-engine/src/settle.rs', '                if synced {\n                    f.publish_synced(through);', '                if synced && false {\n                    f.publish_synced(through);'),
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
 ('crates/store-io-engine/src/submitter/mod.rs', '        self.core.table.start(index, tag, kind, Some(pass), durable);\n        if let Some(s) = split {\n            self.core.table.set_split(index, s);\n        }\n        if let Err(r) = self.core.queue.submit(op, qtag) {', '        let refused = self.core.queue.submit(op, qtag);\n        self.core.table.start(index, tag, kind, Some(pass), durable);\n        if let Some(s) = split {\n            self.core.table.set_split(index, s);\n        }\n        if let Err(r) = refused {'),
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
 ('crates/store-io-engine/src/regionmap.rs', '            pos.checked_add(len).is_some_and(|end| end <= self.len),', '            pos.checked_add(len).is_some() || true,'),
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
 (CREATE, '            _ if v2 && crashed_create_leftover(b.as_slice()) =>', '            _ if (v2 || true) && crashed_create_leftover(b.as_slice()) =>', F2),
 (CREATE, ' || u64_at(56) != 1 || ', ' || u64_at(56) == 0 || ', F2),
 (CREATE, '        return torn;', '        return true;', F2),
 (CREATE, '    torn || store_io_format::meta2::decode_page', '    false || store_io_format::meta2::decode_page', F2),
 (CREATE, '        let largest_metadata = if v2 {', '        let largest_metadata = if v2 && false {', F2),
 # review: the format is read only from a verifying header; a failed
 # metadata read is retried once; a page failing its checks stops a page
 # heal; an unknown ro_compat bit opens read-only; clean_close is the
 # judged one; a torn copy of unknown meta_seq heals at the next number
 ('crates/store-io-engine/src/open.rs', '            if !header_verifies(b.as_slice()) {', '            if !header_verifies(b.as_slice()) && false {', F2),
 ('crates/store-io-engine/src/slots.rs', '    if !failed(first.report.a) && !failed(first.report.b) {', '    if true {', F2),
 (V2, '    if reread {', '    if reread && false {', F2),
 (V2, '        if pair > 0 && decode_lost {', '        if pair > 0 && decode_lost && false {', F2),
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
 (CREATE, ' => *leftovers = true,', ' => *leftovers = false,', F2),
 # third review: a newer volume copy read on the retry is refused; a
 # one-page table is sized whole; the size bound before the probe covers
 # every block size
 (V2, '        vm = decode_volume(&crate::open::winner_payload(&volume_pair)?)?;', '        vm = decode_volume(volume_pair.payload.as_deref().ok_or_else(corrupt)?)?;', F2),
 (V2, '    allocate(if layout.pages == 1 {', '    allocate(if false {', F2),
 (CREATE, '(crate::layout::MIN_LOG2_BLOCK..=crate::layout::MAX_LOG2_BLOCK)', '(crate::layout::MAX_LOG2_BLOCK..=crate::layout::MAX_LOG2_BLOCK)', F2),
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
 ('crates/store-io-engine/src/v2.rs', '    next.meta_seq = meta_seq;', '    next.meta_seq = meta_seq + 1;', F2P),
 ('crates/store-io-engine/src/space.rs', '                released: v2.free.free_bytes().saturating_sub(acct.reserved()),', '                released: v2.free.free_bytes(),', F2P),
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
 ('crates/store-io-sim/src/world.rs', '            if let Some(f) = self.files.get_mut(&file) {\n                unmark(&mut f.holes, asked.0, asked.1);\n            }', '            if let Some(f) = self.files.get_mut(&file).filter(|_| false) {\n                unmark(&mut f.holes, asked.0, asked.1);\n            }', ENGINE),
 # E4 C5: a Submitter's operation across runs: its first piece; the
 # rotation after each piece and at the end; settled once, over the whole
 # operation; a refused next piece settles over what was done; a refused
 # first piece gives the buffer back whole
 ('crates/store-io-engine/src/submitter/mod.rs', 'let piece = ((state.map.run_end(start) - start) as usize).min(total);', 'let piece = total;', F2E),
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
 ('crates/store-io-engine/src/mapped.rs', 'let piece = (self.map.run_end(pos) - pos).min(left as u64);', 'let piece = left as u64;', F2E),
 ('crates/store-io-engine/src/mapped.rs', '.copy_within(written..written + left, 0);', '.copy_within(written - 4096..written + left - 4096, 0);', F2E),
 ('crates/store-io-engine/src/mapped.rs', 'let Some(k) = self.parts.iter().position(|p| p.addr == addr) else {', 'let Some(k) = self.parts.iter().position(|p| p.addr == addr && false) else {', F2E),
 ('crates/store-io-engine/src/mapped.rs', 'Next::Done if !self.parts.is_empty() => Next::Wait,', 'Next::Done if false => Next::Wait,', F2E),
 # E4 C3: regions of several extents open; reads and scans stop at a
 # run's end
 ('crates/store-io-engine/src/io.rs', 'let piece = chunk.min(len - done as u64).min(r.map.run_end(at) - at);', 'let piece = chunk.min(len - done as u64);', F2E),
 ('crates/store-io-engine/src/scan.rs', '                    .min(chunk as u64)\n                    .min(r.map.run_end(next) - next) as usize;', '                    .min(chunk as u64) as usize;', F2E),
 ('crates/store-io-engine/src/regionmap.rs', 'let more: Box<[(u64, u64)]> = all.map(|r| (r.start, r.file)).collect();', 'let more: Box<[(u64, u64)]> = all.take(0).map(|r| (r.start, r.file)).collect();', F2E),
 # E4 C2: runs coalesce adjacent extents in data order; a range never
 # crosses a run; the run search; the runs cover the capacity
 ('crates/store-io-format/src/footprint.rs', '(Some(end), Some(total)) if end == next => len = total,', '(Some(end), Some(total)) if end <= next => len = total,', FOOTPRINT_FMT),
 ('crates/store-io-engine/src/regionmap.rs', '            pos + len <= end,', '            pos + len <= end || true,', REGIONMAP),
 ('crates/store-io-engine/src/regionmap.rs', 'let k = self.more.partition_point(|r| r.0 <= pos);', 'let k = self.more.partition_point(|r| r.0 < pos);', REGIONMAP),
 ('crates/store-io-engine/src/regionmap.rs', '.chain(core::iter::once(self.capacity));', '.chain(core::iter::once(self.len));', REGIONMAP),
 # E4 C1: a release tries every extent (a failed trim on Windows is no
 # error); a submitter's reservation is held under a poisoning guard until
 # its row holds it
 ('crates/store-io-engine/src/lifecycle.rs', '        for (at, len) in extents.into_iter().filter(|x| x.1 > 0) {', '        for (at, len) in extents.into_iter().filter(|x| x.1 > 0).take(1) {', ENGINE_LIB_LIFECYCLE),
 ('crates/store-io-engine/src/lifecycle.rs', '            Some(_) if windows => Ok(()),', '            Some(_) if windows && false => Ok(()),', ENGINE_LIB_LIFECYCLE),
 ('crates/store-io-engine/src/io.rs', '            store: self,\n            r: Some(r),\n        }\n    }\n\n    /// Reserves', '            store: self,\n            r: Some(r).filter(|_| false),\n        }\n    }\n\n    /// Reserves', SUBMITTER_TEST),
 # E2b/E3 C8: the open reads a pair whose B follows A in one read, every
 # other pair slot by slot, a short read never judged, a failed read of
 # both read again slot by slot, each header matched to its own entry
 ('crates/store-io-engine/src/slots.rs', '        if at.a.checked_add(at.size() as u64) == Some(at.b) {', '        if at.a.checked_add(at.size() as u64) == Some(at.b) || at.log2 > 0 {', F2O),
 ('crates/store-io-engine/src/slots.rs', '        if at.a.checked_add(at.size() as u64) == Some(at.b) {', '        if at.a.checked_add(at.size() as u64) == Some(at.b) && at.log2 == 0 {', F2O),
 ('crates/store-io-engine/src/slots.rs', '        Ok(n) if n == len => Ok(got.ok_or(media)),', '        Ok(n) if n <= len => Ok(got.ok_or(media)),', F2O),
 ('crates/store-io-engine/src/slots.rs', '                            Some(OneSlot::unreadable(media)),\n                            Some(OneSlot::unreadable(media)),', '                            Some(judge_slot(at, SlotIndex::A, &vec![0u8; at.size()])),\n                            Some(OneSlot::unreadable(media)),', F2O),
 ('crates/store-io-engine/src/open2.rs', '        for (&(id, p, entry, lim), header) in batch.iter().zip(headers) {', '        for (&(id, p, entry, lim), header) in batch.iter().zip(headers.into_iter().rev()) {', F2O),
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
 ('crates/store-io-engine/src/lifecycle2.rs', 'self.rewrite_header2(r, &mut header, RegionState::Ready, 1, committed)', 'self.rewrite_header2(r, &mut header, RegionState::Ready, 0, committed)', F2C),
 ('crates/store-io-engine/src/lifecycle2.rs', '                .map(|e| usize::from(e.extent_count))', '                .map(|e| usize::from(e.extent_count) + 1)', F2C),
 ('crates/store-io-engine/src/lifecycle.rs', '        let rewritten = if self.inner.layout.format == store_io_format::slot::FORMAT_2 {', '        let rewritten = if self.inner.layout.format == store_io_format::slot::FORMAT_2 && r.id.get() == u64::MAX {', F2C),
 ('crates/store-io-engine/src/slot_handle.rs', '        let payload = if s.inner.layout.format == store_io_format::slot::FORMAT_2 {', '        let payload = if s.inner.layout.format == store_io_format::slot::FORMAT_2 && data.is_empty() {', F2C),
 # E2b/E3 C6 review: the footprint is the header's committed extents (by
 # capacity); a release whose header write is refused after the commit
 # still stands
 ('crates/store-io-engine/src/lifecycle2.rs', '        let extents = committed_extents(&header, inner.layout.log2_block, committed);', '        let extents: Vec<(u64, u64)> = r.map.extents().chain(committed_extents(&header, inner.layout.log2_block, 0)).collect();', F2X),
 ('crates/store-io-engine/src/lifecycle2.rs', '            Err(Error::NotWritten { .. }) => return Ok(()),', '            Err(Error::NotWritten { .. }) => return Err(corrupt()),', F2X),
 # E2b/E3 C6: release in format 2: refused while frozen; next_seq kept;
 # the footprint and usage leave memory; free space recomputed; the
 # header says Released; format 2 never takes format 1's path
 ('crates/store-io-engine/src/lifecycle2.rs', '        if inner.report.metadata.frozen {', '        if inner.report.metadata.frozen && false {', F2X),
 ('crates/store-io-engine/src/lifecycle2.rs', '                next_seq: ph.next_seq,', '                next_seq: ph.next_seq - 1,', F2X),
 ('crates/store-io-engine/src/lifecycle2.rs', '                    let _gone = v2.allocated.swap_remove(i);', '                    let _gone = i;', F2X),
 ('crates/store-io-engine/src/lifecycle2.rs', '                let _gone = v2.usage.swap_remove(i);', '                let _gone = i;', F2X),
 ('crates/store-io-engine/src/lifecycle2.rs', '            v2.rebuild_free(len);', '            let _ = (v2, len);', F2X),
 ('crates/store-io-engine/src/lifecycle2.rs', 'self.rewrite_header2(r, &mut header, RegionState::Released, 0, committed)', 'self.rewrite_header2(r, &mut header, RegionState::Ready, 0, committed)', F2X),
 ('crates/store-io-engine/src/lifecycle.rs', '        if self.inner.layout.format == store_io_format::slot::FORMAT_2 {\n            return self.release_region2(r);', '        if self.inner.layout.format == store_io_format::slot::FORMAT_2 && r.id.get() == u64::MAX {\n            return self.release_region2(r);', F2X),
 # E2b/E3 C5: provisioning in format 2: frozen refuses provisioning
 # and reservations; a name held by an unavailable or unknown-kind entry
 # is taken; a page is used while it has room; first fit before the
 # tail; large data aligned; free space rebuilt after the commit; growth
 # refused past a truncated entry; a reservation grows past what is
 # allocated
 ('crates/store-io-engine/src/provision2.rs', '        if inner.report.metadata.frozen {', '        if inner.report.metadata.frozen && false {', F2P),
 ('crates/store-io-engine/src/space.rs', '        if inner.report.metadata.frozen {', '        if inner.report.metadata.frozen && false {', F2P),
 ('crates/store-io-engine/src/provision2.rs', '                .any(|u| u.name == name.as_str())', '                .any(|u| u.name == name.as_str() && false)', F2P),
 ('crates/store-io-engine/src/provision2.rs', '                .is_some_and(|v| v.reserved_names.contains(name))', '                .is_some_and(|v| v.reserved_names.contains(name) && false)', F2P),
 ('crates/store-io-engine/src/provision2.rs', '                        && h.count < per_page', '                        && h.count <= per_page', F2P),
 ('crates/store-io-engine/src/provision2.rs', '            let offset = match v2.free.first_fit(extent, align, 2 * block) {', '            let offset = match v2.free.first_fit(extent, align, 2 * block).filter(|_| false) {', F2P),
 ('crates/store-io-engine/src/provision2.rs', '        let mut align = if data_size >= DATA_ALIGN {\n            DATA_ALIGN', '        let mut align = if data_size >= DATA_ALIGN && false {\n            DATA_ALIGN', F2P),
 ('crates/store-io-engine/src/provision2.rs', '            v2.rebuild_free(len);', '            let _ = (v2, len);', F2P),
 ('crates/store-io-engine/src/space.rs', '        if meta.v2.as_deref().is_some_and(|v| v.cut_short) {', '        if meta.v2.as_deref().is_some_and(|v| v.cut_short && false) {', F2P),
 ('crates/store-io-engine/src/space.rs', '            Some(v2) => v2.free.end(),', '            Some(v2) => v2.free.end().min(self.inner.layout.tail(&meta.entries)),', F2P),
 # E2b/E3 C4: the format-2 open's regions: stale, superseded and
 # foreign-header regions are read-only; overlap and name demote; extra
 # extents are reported; several extents and 512 B blocks are refused
 # or read-only; every reserved overlap and stale page's agreement is said
 ('crates/store-io-engine/src/open2.rs', '            read_only: unconfirmed || s.superseded || !writable,', '            read_only: s.superseded || !writable,', F2R),
 ('crates/store-io-engine/src/open2.rs', '            read_only: unconfirmed || s.superseded || !writable,', '            read_only: unconfirmed || !writable,', F2R),
 ('crates/store-io-engine/src/open2.rs', '            read_only: unconfirmed || s.superseded || !writable,', '            read_only: unconfirmed || s.superseded || (!writable && false),', F2R),
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
 ('crates/store-io-engine/src/footprint.rs', '        Some((self.tail.checked_add(skew)?.checked_add(align - 1)? & !(align - 1)) - skew)', '        Some(self.tail + (skew & 0))', FOOTPRINT),
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
 ('crates/store-io-engine/src/provision.rs', '            .exclusive_main(|| inner.platform.flush_all(&inner.file));', '            .poisoned()\n            .map_or_else(|| inner.platform.flush_all(&inner.file), |_| inner.platform.flush_all(&inner.file));'),
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
]

def kill_stuck_tests():
    if os.name == "nt":
        subprocess.run(["powershell", "-NoProfile", "-Command",
                        "Get-Process | Where-Object { $_.Path -like '*store-io*target*' } | Stop-Process -Force"],
                       capture_output=True)
    else:
        subprocess.run(["pkill", "-f", os.path.join(ROOT, "target")], capture_output=True)


def check_patterns():
    """Every entry's pattern occurs exactly once: run before each commit."""
    bad = []
    for i, entry in enumerate(M):
        f, a = entry[0], entry[1]
        with open(os.path.join(ROOT, *f.split("/")), encoding="utf-8") as fh:
            n = fh.read().count(a)
        if n != 1:
            print(f"[{i}] {f}: pattern found {n} times: {a[:60]!r}", flush=True)
            bad.append(i)
    print(f"{len(M)} entries, {len(bad)} bad", flush=True)
    sys.exit(1 if bad else 0)


def main():
    if sys.argv[1:] == ["--check"]:
        check_patterns()
    only = [int(a) for a in sys.argv[1:]]
    survived, missing, invalid = [], [], []
    skipped = []
    for i, entry in enumerate(M):
        f, a, b = entry[:3]
        pkgs = list(entry[3]) if len(entry) > 3 else ENGINE
        env = {**os.environ, **entry[4]} if len(entry) > 4 else None
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
                    r = subprocess.run(["cargo", "test", "-q"] + pkgs, cwd=ROOT,
                                       env=env, capture_output=True, text=True, timeout=TIMEOUT)
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
