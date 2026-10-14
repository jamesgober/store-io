//! Forced data sync per region (D-102, G7): a runtime option on a region's
//! handles, never stored on disk.

use std::sync::atomic::Ordering;

use store_io_platform::Platform;

use crate::region::{AppendRegion, PageRegion, Slot};

macro_rules! force_data_sync {
    ($region:ident) => {
        impl<P: Platform> $region<P>
        where
            P::Queue: Send,
        {
            /// Makes every barrier on this region (for a slot, every commit; and
            /// `Submitter::barrier_all`)
            /// issue the OS data sync (`fdatasync`, `FlushFileBuffers`), even
            /// on a power-safe store, where writes are durable at completion
            /// and a barrier is otherwise free; receipts then report
            /// [`data_synced`](crate::DurableReceipt::data_synced). For data
            /// whose durability should not rest on a power-safe
            /// classification alone: a device trusted as power-safe that
            /// still reports a volatile write cache has that cache flushed.
            /// (On a device with no volatile cache the OS has nothing to send
            /// it: the data is on media already. On Windows every power-safe
            /// write is already followed by a data flush, so forcing adds
            /// one.) It applies to every handle and generation of the region,
            /// but is not stored: set it again after each open.
            pub fn set_force_data_sync(&self, on: bool) {
                self.state().force_sync.store(on, Ordering::Release);
            }

            /// Whether barriers on this region always flush (see
            /// [`Self::set_force_data_sync`]).
            #[must_use]
            pub fn force_data_sync(&self) -> bool {
                self.state().forced()
            }
        }
    };
}

force_data_sync!(AppendRegion);
force_data_sync!(PageRegion);
force_data_sync!(Slot);
