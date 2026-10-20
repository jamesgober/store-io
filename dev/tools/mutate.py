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
LOOM = {"RUSTFLAGS": "--cfg loom", "LOOM_MAX_PREEMPTIONS": "3",
        "CARGO_TARGET_DIR": os.path.join(ROOT, "target", "loom")}
STACK = "crates/store-io-buf/src/stack.rs"
# The format crate's own tests (slot, pair and payload codecs).
FORMAT = ["-p", "store-io-format"]
SLOT = "crates/store-io-format/src/slot.rs"
META2 = "crates/store-io-format/src/meta2.rs"
JUDGE = "crates/store-io-format/src/judge.rs"

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
 ("crates/store-io-engine/src/open.rs", "            io: platform.io_path(),\n            unavailable,\n        };", "            io: platform.io_path(),\n            unavailable: {\n                drop(unavailable);\n                Vec::new()\n            },\n        };"),
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
 # Final review: read_one classification on its own; volume search reports media errors
 ("crates/store-io-engine/src/slots.rs", "        Err(raw) if !unreadable(raw) => return Err(fail(raw)),", "        Err(raw) if !unreadable(raw) && false => return Err(fail(raw)),"),
 ("crates/store-io-engine/src/open.rs", "                    media_error |= store_io_core::errno::is_media_error(raw);", "                    media_error |= store_io_core::errno::is_media_error(raw) && false;"),
 ("crates/store-io-engine/src/open.rs", "            if bytes.len() > store_io_format::meta::VOLUME_META_LEN {", "            if bytes.len() > store_io_format::meta::VOLUME_META_LEN && false {"),
 ("crates/store-io-engine/src/open.rs", "store_io_format::meta::MetaError::Field(\"features\") => newer,", "store_io_format::meta::MetaError::Field(\"features\") if false => newer,"),
 ("crates/store-io-engine/src/open.rs", "            media_error |= pair.media_error;", "            media_error |= pair.media_error && false;"),
 ("crates/store-io-engine/src/open.rs", "        Winner::Lost if p.media_error => Err(Error::Corruption {", "        Winner::Lost if p.media_error && false => Err(Error::Corruption {"),
 # D-095: a new operation is first seen anew (lane marks: A1 C6, below)
 ("crates/store-io-engine/src/stats.rs", "            if mark % 2 == 0 || entry.0 != mark {", "            if mark % 2 == 0 {"),
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
 (SUB, '        let pass = admit!(self.admit(region.store(), state, &buf), buf);\n        admit!(self.store.ready(), buf);\n        let Some(f)', '        let pass = admit!(self.admit(region.store(), state, &buf), buf);\n        let Some(f)'),
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
 ('crates/store-io-engine/src/domain.rs', '        let done = self.done.load(Ordering::SeqCst) >= need;', '        let done = self.done.load(Ordering::SeqCst) >= need || true;'),
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
 ('crates/store-io-engine/src/submitter/mod.rs', '        self.core.table.start(index, tag, kind, Some(pass), durable);\n        if let Err(r) = self.core.queue.submit(op, qtag) {', '        let refused = self.core.queue.submit(op, qtag);\n        self.core.table.start(index, tag, kind, Some(pass), durable);\n        if let Err(r) = refused {'),
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
 ('crates/store-io-posix/src/file.rs', '    let twin = sys::openat(\n        libc::AT_FDCWD,\n        &path,\n        libc::O_RDONLY | libc::O_DIRECT | libc::O_CLOEXEC,\n        0,\n    )?;', '    let _ = &path;\n    let twin = sys::dup_cloexec(fd.as_raw_fd())?;', POSIX),
 ('crates/store-io-posix/src/file.rs', '        libc::O_RDONLY | libc::O_DIRECT | libc::O_CLOEXEC,', '        libc::O_RDWR | libc::O_DIRECT | libc::O_CLOEXEC,', POSIX),
 ('crates/store-io-posix/src/uring/queue.rs', '            write.is_none() && self.fixed_file.as_ref().is_some_and(|f| file.same_fd(f));', '            self.fixed_file.as_ref().is_some_and(|f| file.same_fd(f));', POSIX),
 ('crates/store-io-posix/src/uring/queue.rs', '            write.is_none() && self.fixed_file.as_ref().is_some_and(|f| file.same_fd(f));', '            write.is_none() && self.fixed_file.as_ref().is_some_and(|f| file.same_fd(f) || true);', POSIX),
 # release keeps the metadata lock until its space is deallocated (no
 # provisioning can reuse the extent under its late writes)
 ('crates/store-io-engine/src/lifecycle.rs', '        // 2. The header, for tools that list regions from headers.', '        drop(meta);\n        // 2. The header, for tools that list regions from headers.'),
 # G1: a flush leader notifies only when a follower is registered
 ('crates/store-io-engine/src/domain.rs', '        if self.waiters.fetch_add(0, Ordering::AcqRel) > 0 {', '        if self.waiters.fetch_add(0, Ordering::AcqRel) > 1 {'),
 ('crates/store-io-engine/src/domain.rs', '    fn poison_unknown_flush(&self) {\n        let _first_cause_kept = self.poison.set(FirstCause {', '    fn poison_unknown_flush(&self) {\n        let _first_cause_kept = Some(FirstCause {'),
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
