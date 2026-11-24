#!/usr/bin/env python3
"""Dump the UDIF trailer, resources and first chunks of a .dmg (diagnostics).

Usage: udif_dump.py IMAGE...
"""
import plistlib
import struct
import sys
from collections import Counter


def dump(path: str) -> None:
    with open(path, "rb") as f:
        data = f.read()
    k = data[-512:]
    if k[:4] != b"koly":
        print(f"{path}: no koly trailer")
        return
    (ver, hsize, flags, run_off, df_off, df_len, rf_off, rf_len, seg_no, seg_cnt) = struct.unpack(
        ">IIIQQQQQII", k[4:64])
    seg_id = k[64:80].hex()
    df_ck_type, df_ck_bits = struct.unpack(">II", k[80:88])
    xml_off, xml_len = struct.unpack(">QQ", k[216:232])
    mc_type, mc_bits = struct.unpack(">II", k[352:360])
    variant, sectors = struct.unpack(">IQ", k[488:500])
    print(f"== {path} ({len(data)} bytes)")
    print(f"  koly: version {ver} hsize {hsize} flags {flags:#x} running {run_off} data {df_off}+{df_len} "
          f"rsrc {rf_off}+{rf_len} seg {seg_no}/{seg_cnt} id {seg_id}")
    print(f"  data fork checksum type {df_ck_type} bits {df_ck_bits}; master type {mc_type} bits {mc_bits}; "
          f"variant {variant} sectors {sectors}; xml {xml_off}+{xml_len}")
    print(f"  reserved1 nonzero: {any(k[232:352])}; reserved2 {k[500:512].hex()}")
    pl = plistlib.loads(data[xml_off:xml_off + xml_len])
    print(f"  plist top keys: {list(pl.keys())}")
    for rtype, entries in pl.get("resource-fork", {}).items():
        print(f"  resource {rtype}: {len(entries)} entries")
        for e in entries:
            meta = {key: e[key] for key in e if key != "Data"}
            raw = e.get("Data", b"")
            print(f"    {meta} data {len(raw)} bytes")
            if rtype == "blkx" and raw[:4] == b"mish":
                (mver, first, count, doff, bufs, desc) = struct.unpack(">IQQQII", raw[4:40])
                n = struct.unpack(">I", raw[200:204])[0]
                chunks = [struct.unpack(">IIQQQQ", raw[204 + i * 40:244 + i * 40]) for i in range(n)]
                kinds = Counter(f"{c[0]:#010x}" for c in chunks)
                sizes = Counter(c[3] for c in chunks if c[0] not in (0xFFFFFFFF, 0x7FFFFFFE))
                print(f"      mish v{mver} first {first} count {count} dataoff {doff} buffers {bufs} desc {desc} "
                      f"kinds {dict(kinds)} sector sizes {dict(sizes.most_common(4))}")
                for c in chunks:
                    if c[0] & 0x80000000 and c[0] != 0xFFFFFFFF and c[5] > 0:
                        off = df_off + c[4]
                        print(f"      first compressed chunk {c[0]:#x} len {c[5]}: {data[off:off + 48].hex()}")
                        break
            elif rtype == "plst" or rtype not in ("blkx",):
                print(f"      {raw[:64].hex()}")


for p in sys.argv[1:]:
    dump(p)
