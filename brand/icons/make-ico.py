#!/usr/bin/env python3
"""Packs PNG files into one Windows .ico: make-ico.py OUT.ico 16.png 32.png ... (PNG entries, Vista and later)."""
import struct
import sys
from pathlib import Path

out, *pngs = sys.argv[1:]
images = [Path(p).read_bytes() for p in pngs]
header = struct.pack("<HHH", 0, 1, len(images))
offset = len(header) + 16 * len(images)
entries = b""
for data in images:
    width, height = struct.unpack(">II", data[16:24])  # the PNG's IHDR
    # 256 px is written as 0 in the one-byte size fields.
    entries += struct.pack("<BBBBHHII", width % 256, height % 256, 0, 0, 1, 32, len(data), offset)
    offset += len(data)
Path(out).write_bytes(header + entries + b"".join(images))
