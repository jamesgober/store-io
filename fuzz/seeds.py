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
region = struct.pack("<IBBBBQQ", 5, 2, 1, 0, 0, 3 * 4096, 1 << 20) + bytes(16) + bytes(4)
write("meta_decode", "region", region[:48])

# table_decode: first byte selects the block (4 KiB); two ready entries.
def entry(rid, kind, name, offset, size, tag=0):
    n = name.encode().ljust(24, b"\0")
    return struct.pack("<IBBBB", rid, kind, 1, 0, 0) + n + struct.pack("<QQI", offset, size, tag) + bytes(12)

tbl = struct.pack("<II", 3, 2) + bytes(8)
tbl += entry(1, 1, "wal", 8 * 4096, 1 << 20)
tbl += entry(2, 2, "pages", 8 * 4096 + 2 * 4096 + (1 << 20), 1 << 20, tag=9)
write("table_decode", "two-entries", bytes([0]) + tbl)
print("seeds written")
