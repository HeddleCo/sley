#!/usr/bin/env python3
"""Generate a deterministic, dependency-rich pack for pack-decode-bench.

Usage: python3 generate_decode_pack.py /path/to/fixture.pack
128 random 1 MiB bases, each followed by 15 REF deltas copying all but an
8-byte suffix. Layers interleave the chains, allowing independent CPU work.
Produces 2 GiB of decoded bytes from roughly 128 MiB of encoded bytes.
Run git index-pack on the result before benchmarking. Requires only Python.
"""
import hashlib
import pathlib
import random
import struct
import sys
import zlib


def varint(value, bits=7):
    out = bytearray()
    while value >= 1 << bits:
        out.append((value & ((1 << bits) - 1)) | 128)
        value >>= bits
    out.append(value)
    return out


def entry(kind, body, base=b""):
    size = len(body)
    header = bytearray([(kind << 4) | (size & 15) | (128 if size >= 16 else 0)])
    if size >= 16:
        header.extend(varint(size >> 4))
    return header + base + zlib.compress(body, 1)


def generate(path):
    rng = random.Random(260)
    bases = [rng.randbytes(1024 * 1024) for _ in range(128)]
    previous = [None] * len(bases)
    checksum = hashlib.sha1()
    with path.open("wb") as output:
        def write(data):
            checksum.update(data)
            output.write(data)

        write(b"PACK" + struct.pack(">II", 2, len(bases) * 16))
        for depth in range(16):
            for number, base in enumerate(bases):
                body = base[:-8] + struct.pack(">II", number, depth)
                oid = hashlib.sha1(b"blob " + str(len(body)).encode() + b"\0" + body).digest()
                if depth == 0:
                    encoded = entry(3, body)
                else:
                    delta = varint(len(body)) + varint(len(body))
                    # Copy 1 MiB - 8 bytes with three size bytes, insert eight.
                    delta.extend(b"\xf0\xf8\xff\x0f\x08" + body[-8:])
                    encoded = entry(7, delta, previous[number])
                write(encoded)
                previous[number] = oid
        output.write(checksum.digest())
    print(f"pack_bytes={path.stat().st_size} objects=2048 decoded_bytes={2048 * 1024 * 1024}")


if __name__ == "__main__":
    generate(pathlib.Path(sys.argv[1]))
