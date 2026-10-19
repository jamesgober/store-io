//! The io_uring kernel ABI (D-093): the structures and constants of
//! `include/uapi/linux/io_uring.h` that store-io uses, written out by hand
//! (`libc` provides only the system call numbers). No code, no `unsafe`.

/// `IORING_SETUP_*`.
pub(crate) mod setup {
    pub(crate) const CQSIZE: u32 = 1 << 3;
    pub(crate) const R_DISABLED: u32 = 1 << 6;
    pub(crate) const SUBMIT_ALL: u32 = 1 << 7;
    pub(crate) const TASKRUN_FLAG: u32 = 1 << 9;
    pub(crate) const SINGLE_ISSUER: u32 = 1 << 12;
    pub(crate) const DEFER_TASKRUN: u32 = 1 << 13;
    pub(crate) const NO_SQARRAY: u32 = 1 << 16;
}

/// `IORING_FEAT_SINGLE_MMAP`: the SQ and CQ rings share one mapping.
pub(crate) const FEAT_SINGLE_MMAP: u32 = 1 << 0;
/// `IORING_FEAT_NODROP`: the kernel keeps completions that do not fit (it
/// reports an overflow instead of dropping them).
pub(crate) const FEAT_NODROP: u32 = 1 << 1;

/// `IORING_ENTER_*`.
pub(crate) const ENTER_GETEVENTS: u32 = 1 << 0;
pub(crate) const ENTER_REGISTERED_RING: u32 = 1 << 4;

/// SQ ring `flags` bits.
pub(crate) const SQ_CQ_OVERFLOW: u32 = 1 << 1;
pub(crate) const SQ_TASKRUN: u32 = 1 << 2;

/// `IOSQE_FIXED_FILE`: the entry's `fd` is a registered (direct)
/// descriptor (D-078).
pub(crate) const SQE_FIXED_FILE: u8 = 1 << 0;

/// `IORING_OP_*` (only those named anywhere in store-io).
pub(crate) mod op {
    pub(crate) const NOP: u8 = 0;
    pub(crate) const READV: u8 = 1;
    pub(crate) const WRITEV: u8 = 2;
    /// Never allowed (D-073): always punts to io-wq. Named for the test that
    /// proves the allowlist refuses it.
    pub(crate) const FSYNC: u8 = 3;
    /// Never allowed (D-107): hangs on some kernels. Named for the
    /// compile-time check and the test that keep it out of the allowlist.
    pub(crate) const READ_FIXED: u8 = 4;
    pub(crate) const WRITE_FIXED: u8 = 5;
    pub(crate) const ASYNC_CANCEL: u8 = 14;
    pub(crate) const READ: u8 = 22;
    pub(crate) const WRITE: u8 = 23;
}

/// `IORING_REGISTER_*`.
pub(crate) mod reg {
    use libc::c_uint;
    pub(crate) const FILES: c_uint = 2;
    pub(crate) const FILES_UPDATE: c_uint = 6;
    pub(crate) const PROBE: c_uint = 8;
    pub(crate) const RESTRICTIONS: c_uint = 11;
    pub(crate) const ENABLE_RINGS: c_uint = 12;
    pub(crate) const FILES_UPDATE2: c_uint = 14;
    pub(crate) const IOWQ_MAX_WORKERS: c_uint = 19;
    pub(crate) const RING_FDS: c_uint = 20;
    pub(crate) const UNREGISTER_RING_FDS: c_uint = 21;
}

/// `IORING_RESTRICTION_*`.
pub(crate) mod restriction {
    pub(crate) const REGISTER_OP: u16 = 0;
    pub(crate) const SQE_OP: u16 = 1;
    pub(crate) const SQE_FLAGS_ALLOWED: u16 = 2;
}

/// `IO_URING_OP_SUPPORTED`.
pub(crate) const OP_SUPPORTED: u16 = 1 << 0;

/// Ring region offsets for `mmap`.
pub(crate) const OFF_SQ_RING: libc::off_t = 0;
pub(crate) const OFF_CQ_RING: libc::off_t = 0x0800_0000;
pub(crate) const OFF_SQES: libc::off_t = 0x1000_0000;

/// `struct io_sqring_offsets`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct SqOffsets {
    pub(crate) head: u32,
    pub(crate) tail: u32,
    pub(crate) ring_mask: u32,
    pub(crate) ring_entries: u32,
    pub(crate) flags: u32,
    pub(crate) dropped: u32,
    pub(crate) array: u32,
    pub(crate) resv1: u32,
    pub(crate) user_addr: u64,
}

/// `struct io_cqring_offsets`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct CqOffsets {
    pub(crate) head: u32,
    pub(crate) tail: u32,
    pub(crate) ring_mask: u32,
    pub(crate) ring_entries: u32,
    pub(crate) overflow: u32,
    pub(crate) cqes: u32,
    pub(crate) flags: u32,
    pub(crate) resv1: u32,
    pub(crate) user_addr: u64,
}

/// `struct io_uring_params`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Params {
    pub(crate) sq_entries: u32,
    pub(crate) cq_entries: u32,
    pub(crate) flags: u32,
    pub(crate) sq_thread_cpu: u32,
    pub(crate) sq_thread_idle: u32,
    pub(crate) features: u32,
    pub(crate) wq_fd: u32,
    pub(crate) resv: [u32; 3],
    pub(crate) sq_off: SqOffsets,
    pub(crate) cq_off: CqOffsets,
}

/// `struct io_uring_sqe` (64 bytes), with the union members store-io uses.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Sqe {
    pub(crate) opcode: u8,
    pub(crate) flags: u8,
    pub(crate) ioprio: u16,
    pub(crate) fd: i32,
    /// `off`: the file offset.
    pub(crate) off: u64,
    /// `addr`: the buffer address.
    pub(crate) addr: u64,
    pub(crate) len: u32,
    /// `rw_flags`: `RWF_*` for reads and writes.
    pub(crate) rw_flags: i32,
    pub(crate) user_data: u64,
    pub(crate) buf_index: u16,
    pub(crate) personality: u16,
    pub(crate) file_index: u32,
    pub(crate) addr3: u64,
    pub(crate) pad2: u64,
}

/// `struct io_uring_cqe` (16 bytes; no `CQE32`).
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cqe {
    pub(crate) user_data: u64,
    pub(crate) res: i32,
    pub(crate) flags: u32,
}

/// `struct io_uring_restriction` (16 bytes).
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Restriction {
    pub(crate) opcode: u16,
    /// `register_op`, `sqe_op` or `sqe_flags`, by `opcode`.
    pub(crate) arg: u8,
    pub(crate) resv: u8,
    pub(crate) resv2: [u32; 3],
}

/// `struct io_uring_probe` with room for every opcode.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct Probe {
    pub(crate) last_op: u8,
    pub(crate) ops_len: u8,
    pub(crate) resv: u16,
    pub(crate) resv2: [u32; 3],
    pub(crate) ops: [ProbeOp; 256],
}

/// `struct io_uring_probe_op`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ProbeOp {
    pub(crate) op: u8,
    pub(crate) resv: u8,
    pub(crate) flags: u16,
    pub(crate) resv2: u32,
}

/// `struct io_uring_rsrc_update` (registering the ring's own descriptor).
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct RsrcUpdate {
    pub(crate) offset: u32,
    pub(crate) resv: u32,
    pub(crate) data: u64,
}

const _: () = assert!(size_of::<Params>() == 120);
const _: () = assert!(size_of::<SqOffsets>() == 40);
const _: () = assert!(size_of::<CqOffsets>() == 40);
const _: () = assert!(size_of::<Sqe>() == 64);
const _: () = assert!(size_of::<Cqe>() == 16);
const _: () = assert!(size_of::<Restriction>() == 16);
const _: () = assert!(size_of::<RsrcUpdate>() == 16);
const _: () = assert!(size_of::<ProbeOp>() == 8);
const _: () = assert!(size_of::<Probe>() == 16 + 256 * 8);
