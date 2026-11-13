"""Writes the seed corpus for the fuzz targets: valid slots as store-io
encodes them, so the fuzzer starts from inputs that pass every check and
mutates from there (random bytes almost never get past the header CRC).

Run from this directory: python seeds.py
Layout: docs/FORMAT.md section 3 (slot header) and section 7 (table).
"""

import os
import struct

POLY = 0x82F63B78
TABLE = []
for i in range(256):
    c = i
    for _ in range(8):
        c = (c >> 1) ^ POLY if c & 1 else c >> 1
    TABLE.append(c)


def crc32c(data):
    c = 0xFFFFFFFF
    for b in data:
        c = TABLE[(c ^ b) & 0xFF] ^ (c >> 8)
    return c ^ 0xFFFFFFFF


def slot(log2, uuid, object_id, generation, offset, index, payload, prev=0, owner=1):
    size = 1 << log2
    s = bytearray(size)
    s[128:128 + len(payload)] = payload
    header = bytearray(128)
    header[0:8] = b"SIOSLOT\0"
    struct.pack_into("<HHIII", header, 12, 128, 1, 0, 0, 0)
    struct.pack_into("<I", header, 28, crc32c(payload))
    header[32:48] = uuid
    struct.pack_into("<QQQI", header, 48, object_id, generation, offset, len(payload))
    struct.pack_into("<BBB", header, 76, index, 2, log2)
    struct.pack_into("<I", header, 80, prev)
    struct.pack_into("<Q", header, 88, owner)
    struct.pack_into("<I", header, 8, crc32c(bytes(header)))
    s[0:128] = header
    return bytes(s)


def write(target, name, data):
    d = os.path.join("corpus", target)
    os.makedirs(d, exist_ok=True)
    with open(os.path.join(d, name), "wb") as f:
        f.write(data)


# slot_validate: mode bit 1 = recompute CRCs, bit 2 = expect the slot's own
# identity, bit 4 = expect the size the second byte selects; size byte
# selects log2 = 9 + size % 4.
uuid = bytes(range(16))
for log2 in (9, 10, 11, 12):
    for mode in (0b110, 0b111):
        for n, payload in enumerate((b"", b"x", b"manifest:wal@4096", bytes(range(200)))):
            if len(payload) > (1 << log2) - 128:
                continue
            s = slot(log2, uuid, 42, 3, 8192, 1, payload, prev=0xDEADBEEF, owner=5)
            write("slot_validate", f"l{log2}-m{mode}-p{n}", bytes([mode, log2 - 9]) + s)

# meta_decode: a volume record and a region header record (48 bytes each),
# as the encoders write them.
vol = struct.pack("<BBHIQQQQ", 12, 14, 0, 0, 7, 3 * 4096, 1_700_000_000, 0) + bytes(8)
write("meta_decode", "volume", vol[:48])
region = struct.pack("<IBBBBQQ", 5, 2, 1, 0, 0, 3 * 4096, 1 << 20) + bytes(16) + bytes(8)
write("meta_decode", "region", region[:48])

# table_decode: first byte selects the block (4 KiB); two ready entries.
def entry(rid, kind, name, offset, size, tag=0):
    n = name.encode().ljust(24, b"\0")
    return struct.pack("<IBBBB", rid, kind, 1, 0, 0) + n + struct.pack("<QQI", offset, size, tag) + bytes(12)

tbl = struct.pack("<II", 3, 2) + bytes(8)
tbl += entry(1, 1, "wal", 8 * 4096, 1 << 20)
tbl += entry(2, 2, "pages", 8 * 4096 + 2 * 4096 + (1 << 20), 1 << 20, tag=9)
write("table_decode", "two-entries", bytes([0]) + tbl)

# probe_parsers: first byte selects the parser (see the backends' fuzzing.rs).
mi = b"36 35 98:0 /mnt1 /mnt/parent\\040dir rw,noatime master:1 - ext4 /dev/root rw,errors=continue"
write("probe_linux", "mountinfo", bytes([0]) + mi)
table = mi + b"\n37 36 8:1 / /data rw - xfs /dev/sda1 rw,attr2\n"
write("probe_linux", "find-mount", bytes([1]) + b"37\n/data/store\n" + table)
write("probe_linux", "uevent", bytes([2]) + b"MAJOR=259\nMINOR=0\nDEVNAME=nvme0n1\nDEVTYPE=disk\n")
write("probe_linux", "sysfs-values", bytes([5]) + b"1024\n")
write("probe_linux", "dm", bytes([6]) + b"luks-root\nCRYPT-LUKS2-0123-root")
write("probe_linux", "md", bytes([7]) + b"raid5\nresync\n0")
# Selector 49: parser 49 % 13 = 10, both device-presence flags (49 // 13 = 3).
write("probe_linux", "virt", bytes([49]) + b"QEMU\nStandard PC\n\n6.6.87.2-microsoft-standard-WSL2\nflags : fpu hypervisor")
# Device identity (selector 11): a `dev` file, a USB disk's sysfs path, a
# device name with jbd2's entries, XFS super options, a serial.
write("probe_linux", "identity", bytes([11]) + b"8:16\n/sys/devices/pci0000:00/0000:00:14.0/usb2/2-1/2-1:1.0/host6/target6:0:0/6:0:0:0/block/sdb\nsda1\nsda1-8,nvme0n1p2-8\nrw,attr2,logdev=/dev/sdc1\neui.0025385a11b2c3d4")
# A Linux NVMe Identify Namespace page (selector 12, unpadded): 2 formats,
# format 1 in use with 4 KiB blocks.
ns = bytearray(4096)
ns[25] = 1      # nlbaf (0-based count)
ns[26] = 1      # flbas: format 1
ns[128 + 4 * 0 + 2] = 9
ns[128 + 4 * 1 + 2] = 12
write("probe_linux", "nvme-id-ns", bytes([12]) + bytes(ns))
ctrl = bytearray(4096)
ctrl[24:24 + 12] = b"Example SSD "
ctrl[64:72] = b"FW1.0   "
ctrl[525] = 1
write("probe_linux", "nvme-id-ctrl", bytes([12]) + bytes(ctrl))
# probe_windows: first byte selects the parser; bit 7 pads to a 4 KiB page.
wns = bytearray(4096)
wns[24] = 0x18   # nsfeat: NSABP and NPWG fields valid
wns[25] = 1      # nlbaf
wns[26] = 1      # flbas: format 1
wns[128 + 4 * 0 + 2] = 9
wns[128 + 4 * 1 + 2] = 12
write("probe_windows", "nvme-id-ns", bytes([1]) + bytes(wns))
# An SMBIOS table (selector 4, unpadded): the 8-byte header, a type-1
# structure naming its maker and product, then the end structure.
write("probe_windows", "smbios", bytes([4]) + bytes(8) + bytes([1, 8, 0, 0, 1, 2, 0, 0]) + b"Micro-Star International Co., Ltd.\0MS-7E12\0\0" + bytes([127, 4, 0, 0, 0, 0]))
write("probe_windows", "nvme-id-ctrl", bytes([0]) + bytes(ctrl))
# STORAGE_DEVICE_DESCRIPTOR: vendor/product/revision offsets at 12/16/20,
# bus type at 28, strings after the fixed part.
desc = bytearray(64)
struct.pack_into("<IIII", desc, 12, 40, 48, 60, 0)
struct.pack_into("<I", desc, 28, 0x11)
desc[40:47] = b"VENDOR\x00"
desc[48:59] = b"MODEL 1234\x00"
desc[60:63] = b"1.0"
write("probe_windows", "device-descriptor", bytes([3]) + bytes(desc))
# STORAGE_PROTOCOL_DATA_DESCRIPTOR: version, size, then the 40-byte
# protocol-specific data with the data offset and length.
pd = bytearray(8 + 40 + 16)
struct.pack_into("<II", pd, 8 + 16, 40, 16)
write("probe_windows", "protocol-data", bytes([2]) + bytes(pd))
write("probe_windows", "disk-extents", bytes([5]) + struct.pack("<IIIqq", 1, 0, 2, 0, 1 << 30))

# export_info: the export record (docs/FORMAT.md, "Export record"): a
# header, every list, and an append, a page and a slot region with their
# span CRCs, then the CRC of it all.
SPAN = 1 << 20


def info_header(writable, log2, regions, lend, total, counts):
    h = bytearray(64)
    h[0:8] = b"SIOXINFO"
    struct.pack_into("<HHB", h, 8, 1, 1 if writable else 0, log2)
    struct.pack_into("<IIQ", h, 16, regions, lend, total)
    struct.pack_into("<IIII", h, 32, *counts)
    return bytes(h)


def info_record(name, kind, fill, flags, tag, gen, cap, data_size, durable, object_len, stream_len):
    r = bytearray(128)
    r[0:len(name)] = name
    struct.pack_into("<BBHIQQ", r, 24, kind, fill, flags, tag, gen, cap)
    r[48:64] = bytes([7] * 16)
    struct.pack_into("<QQI", r, 64, data_size, durable, object_len)
    struct.pack_into("<Q", r, 88, stream_len)
    return bytes(r)


def spans(n):
    return b"".join(struct.pack("<I", 0x1000 + i) for i in range((n + SPAN - 1) // SPAN))


def info(writable, items, omitted=(), lost=(), stale=(), verified=()):
    body = info_header(writable, 12, len(items), 5, 1 << 30,
                       (len(omitted), len(lost), len(stale), len(verified)))
    body += b"".join(struct.pack("<Q", x) for x in omitted)
    body += b"".join(struct.pack("<I", x) for x in lost)
    body += b"".join(struct.pack("<I", x) for x in stale)
    body += b"".join(n.ljust(24, b"\0") + struct.pack("<Q", v) for n, v in verified)
    for rec, n in items:
        body += rec + spans(n)
    return body + struct.pack("<I", crc32c(body))


log_len = 3 * SPAN + 5
items = [
    (info_record(b"log", 1, 1, 0b11, 42, 3, 1 << 30, 4 * SPAN, log_len, 0, log_len), log_len),
    (info_record(b"pages", 2, 0, 0b1100, 0, 1, 0, 2 * SPAN, 0, 0, 2 * SPAN), 2 * SPAN),
    (info_record(b"obj", 3, 1, 0, 0, 1, 0, 0, 0, 6, 6), 6),
]
write("export_info", "three-regions", info(True, items, (9,), (3,), (1, 2), ((b"pages", 77),)))
write("export_info", "empty", info(False, []))

print("seeds written")
