//! A new store built from an export (PLANNING R5.6, D-169, D-170): what
//! [`import`](crate::import) and [`StoreSink`](crate::StoreSink) share, so
//! the two restore a region the same way. The header checked, the store
//! created under the target's temporary name once its free space holds the
//! header's total, each region admitted (its info checked as untrusted
//! input, its name once, the sizes within the total, no more regions than
//! the header counts), recreated with its name, kind, data size, cap, tag,
//! generation, fill pattern and key, and its bytes written once as its
//! provisioning's fill (durable and verified before its header, D-137):
//! the export's bytes, then past them its pattern; a slot by one commit of
//! its object, an empty one included.

use std::collections::HashSet;
use std::path::Path;

use store_io_core::error::{Error, ErrorContext, NotWrittenCause, Op, ReserveTag};
use store_io_format::meta::RegionKind;
use store_io_format::meta2::max_object;
use store_io_format::slot::FORMAT_2;
use store_io_platform::Platform;

use crate::RegionOptions;
use crate::call::CallState;
use crate::copy::CopyAttrs;
use crate::create::Creator;
use crate::export::{ExportHeader, RegionInfo};
use crate::io::not_written;
use crate::provision2::Pushed;
use crate::store::{Store, StoreOptions, io};
use crate::target::TargetDir;

pub(crate) fn invalid() -> Error {
    not_written(NotWrittenCause::InvalidOptions, ErrorContext::default())
}

/// Whether a store can be restored from `header`: its block size a power
/// of two from 512 to 65,536, its region count within the format's table.
///
/// # Errors
///
/// `NotWritten(InvalidOptions)` otherwise.
pub(crate) fn check_header(header: &ExportHeader) -> Result<(), Error> {
    let block = header.block_size;
    if !block.is_power_of_two()
        || !(512..=1 << 16).contains(&block)
        || header.region_count > crate::v2::MAX_REGION_CAPACITY
    {
        return Err(invalid());
    }
    Ok(())
}

/// The new store while its regions are restored, and what was admitted.
pub(crate) struct Restore<P: Platform>
where
    P::Queue: Send,
{
    pub(crate) store: Store<P>,
    block: u64,
    total: u64,
    count: u32,
    names: HashSet<String>,
    /// What the regions admitted charge against the header's total.
    charged: u64,
    admitted: u32,
}

impl<P: Platform + Clone> Restore<P>
where
    P::Queue: Send,
{
    /// The new store, under `target`'s temporary name in `dir`, its writes
    /// the call's (`Creator::Call`): `opts` with the export's block size
    /// and a region capacity for its count. The header must have passed
    /// [`check_header`].
    ///
    /// # Errors
    ///
    /// `NoSpace` if the free space is short of the header's total (nothing
    /// written); `NotWritten(Misaligned)` if the target's device needs
    /// larger blocks than the export's (the temporary is removed by the
    /// next copy or import); as `Store::create`.
    pub(crate) fn create(
        platform: &P,
        dir: &Path,
        target: &TargetDir<P>,
        header: &ExportHeader,
        opts: &StoreOptions,
        call: &CallState,
    ) -> Result<Self, Error> {
        // Before anything is written: the header's total is an upper bound
        // on what the store takes.
        let free = platform
            .free_space(&target.dir)
            .map_err(|raw| io(Op::Open, raw))?;
        if free < header.total {
            return Err(Error::NoSpace {
                tag: ReserveTag::Caller(0),
                ctx: ErrorContext::default(),
            });
        }
        let block = header.block_size;
        let mut target_opts = opts.clone();
        target_opts.container_format = FORMAT_2;
        target_opts.block_size = Some(block as u32);
        target_opts.region_capacity = target_opts.region_capacity.max(header.region_count);
        let store = Store::create_as(
            platform.clone(),
            dir,
            target_opts,
            &target.temp,
            Creator::Call(call),
        )?;
        // A device that needs larger blocks than the export's would round
        // every size up: refused.
        if store.report().block_size != block {
            return Err(not_written(
                NotWrittenCause::Misaligned,
                ErrorContext::default(),
            ));
        }
        Ok(Self {
            store,
            block,
            total: header.total,
            count: header.region_count,
            names: HashSet::new(),
            charged: 0,
            admitted: 0,
        })
    }

    /// The largest read of an export's bytes into the store (its `max_io`).
    pub(crate) fn read_piece(&self) -> usize {
        self.store.inner.opts.max_io.max(1)
    }

    /// Admits `info` as the next region: within the header's count, valid
    /// ([`RegionInfo::validate`]), a name not yet used, its footprint
    /// within the header's total with the regions before it.
    ///
    /// # Errors
    ///
    /// `NotWritten(InvalidOptions)` at the first check that fails.
    pub(crate) fn admit(&mut self, info: &RegionInfo) -> Result<(), Error> {
        if self.admitted >= self.count {
            return Err(invalid());
        }
        info.validate(self.block)?;
        if !self.names.insert(info.name.clone()) {
            return Err(invalid());
        }
        let block = self.block;
        let data = match info.kind {
            RegionKind::Slot => max_object(block.trailing_zeros() as u8) as u64,
            _ => info.cap.unwrap_or(0).max(info.data_size),
        };
        self.charged = self
            .charged
            .checked_add(2 * block)
            .and_then(|c| c.checked_add(data))
            .filter(|&c| c <= self.total)
            .ok_or_else(invalid)?;
        self.admitted += 1;
        Ok(())
    }

    /// Admitted `info`'s region, recreated with its attributes, its data
    /// area to be given in order: the export's bytes (its stream length),
    /// then [`Pushed::finish`].
    pub(crate) fn region(&self, info: &RegionInfo) -> Result<Pushed<P>, Error> {
        let mut opts = RegionOptions::default().size(info.data_size);
        if let Some(cap) = info.cap {
            opts = opts.max_size(cap);
        }
        let attrs = CopyAttrs {
            generation: info.generation,
            fill: info.fill,
            fill_key: info.fill_key,
            tag: info.tag,
        };
        let made = self
            .store
            .push_region(&info.name, info.kind, opts, &attrs)?;
        // Placed at exactly the data size the export gave (a slot's: none),
        // so its bytes and its pattern past them end where the source's
        // did (a backstop: `validate` holds the size in whole blocks).
        let want = match info.kind {
            RegionKind::Slot => 0,
            _ => info.data_size,
        };
        if made.data_size() != want {
            return Err(crate::store::corrupt());
        }
        Ok(made)
    }

    /// Admitted slot `info` recreated, then its object committed by one
    /// commit when it had one, an empty one included.
    pub(crate) fn slot(&self, info: &RegionInfo, object: &[u8]) -> Result<(), Error> {
        let _live = self.region(info)?.finish()?;
        if info.object_len.is_some() {
            let _receipt = self.store.slot(&info.name)?.commit(object)?;
        }
        Ok(())
    }

    /// Every region the header counts admitted: the store flushed, then
    /// closed (its drop writes the clean-close record).
    ///
    /// # Errors
    ///
    /// `NotWritten(InvalidOptions)` if fewer regions were admitted than
    /// the header counts; a flush's failure.
    pub(crate) fn close(self) -> Result<(), Error> {
        if self.admitted != self.count {
            return Err(invalid());
        }
        self.store.flush_all_accounted()
    }
}
