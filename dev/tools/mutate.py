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
# The observer's own tests (feature `observe`).
OBSERVE = ["-p", "store-io-core", "--features", "store-io-core/observe"]
SUB = "crates/store-io-engine/src/submitter/mod.rs"

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
