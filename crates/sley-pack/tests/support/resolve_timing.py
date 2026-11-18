"""Reproduce the interleaved trusted-index timing gate without another worktree.

Usage: python3 crates/sley-pack/tests/support/resolve_timing.py SCRATCH TARGET
Exports historical sources, builds the same public-API example in release mode,
then alternates base/head three times on each 420,000-object fixture. All build
output is captured. No existing source checkout or Git branch is changed.
"""
import hashlib
import io
import json
import os
from pathlib import Path
import random
import re
import statistics
import struct
import subprocess
import sys
import tarfile
import time
import zlib

scratch, target = map(Path, sys.argv[1:3])
scratch.mkdir(parents=True, exist_ok=False)
target.mkdir(parents=True, exist_ok=True)
root = Path.cwd()
head = subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip()
refs = {'base': '61848386', 'head': head}
example = (root / 'crates/sley-pack/tests/support/resolve_timing.rs').read_bytes()
env = os.environ.copy(); env['RUSTUP_TOOLCHAIN'] = 'stable'
for name, ref in refs.items():
    source = scratch / name; source.mkdir()
    archive = subprocess.check_output(['git', 'archive', ref])
    with tarfile.open(fileobj=io.BytesIO(archive)) as tar:
        tar.extractall(source, filter='data')
    directory = source / 'crates/sley-pack/examples'; directory.mkdir(exist_ok=True)
    (directory / 'r5_timing.rs').write_bytes(example)
    env['CARGO_TARGET_DIR'] = str(target / name)
    with (scratch / (name + '-build.log')).open('w') as log:
        subprocess.run(['cargo', 'build', '--manifest-path', str(source / 'Cargo.toml'), '--locked', '--release', '-p', 'sley-pack', '--example', 'r5_timing'], env=env, stdout=log, stderr=subprocess.STDOUT, check=True)

def header(kind, size):
    result = bytearray([kind << 4 | (size & 15) | (128 if size > 15 else 0)])
    size >>= 4
    while size:
        byte = size & 127; size >>= 7; result.append(byte | (128 if size else 0))
    return result

def varint(size):
    out = bytearray()
    while True:
        byte = size & 127; size >>= 7; out.append(byte | (128 if size else 0))
        if not size: return out

def ofs(distance):
    out = bytearray([distance & 127]); distance >>= 7
    while distance:
        distance -= 1; out.append((distance & 127) | 128); distance >>= 7
    return out[::-1]

fixtures = []
for mode in ['whole', 'mixed-ofs']:
    path = scratch / (mode + '.pack'); count = 420000
    payload = random.Random(1712).randbytes(880 if mode == 'whole' else 1740)
    digest = hashlib.sha1(); offset = 0
    with path.open('wb') as file:
        def emit(data):
            global offset
            file.write(data); digest.update(data); offset += len(data)
        emit(b'PACK' + struct.pack('>II', 2, count))
        for i in range(count if mode == 'whole' else count // 2):
            base = offset; body = struct.pack('>Q', i) + payload
            emit(header(3, len(body))); emit(zlib.compress(body, 1))
            if mode == 'mixed-ofs':
                delta = varint(len(body)) + varint(len(body))
                size = len(body) - 1
                delta.extend([0xb0, size & 255, size >> 8, 1, i & 255])
                entry_offset = offset
                emit(header(6, len(delta))); emit(ofs(entry_offset - base))
                emit(zlib.compress(delta, 1))
        file.write(digest.digest())
    fixtures.append((mode, path))
    print(json.dumps(dict(fixture=mode, objects=count, bytes=path.stat().st_size, pack_sha1=digest.hexdigest())), flush=True)

if hasattr(os, 'sched_getaffinity'):
    affinity = sorted(os.sched_getaffinity(0))[-16:]
    os.sched_setaffinity(0, affinity)
rows = []
for mode, path in fixtures:
    for trial in range(1, 4):
        for name in ['base', 'head']:
            binary = target / name / 'release/examples/r5_timing'
            result = subprocess.run([str(binary), str(path)], capture_output=True, text=True, check=True)
            match = re.search(r'seconds=([\d.]+) objects=(\d+) idx_sha256=(\w+)', result.stdout)
            assert match, result.stdout
            row = dict(fixture=mode, version=name, ref=refs[name], trial=trial, seconds=float(match[1]), objects=int(match[2]), idx_sha256=match[3])
            rows.append(row); (scratch / 'timings.json').write_text(json.dumps(rows, indent=2))
            print(json.dumps(row), flush=True)
    group = [r for r in rows if r['fixture'] == mode]
    assert len({r['idx_sha256'] for r in group}) == 1
    medians = {name: statistics.median(r['seconds'] for r in group if r['version'] == name) for name in refs}
    ratio = medians['head'] / medians['base']
    print(json.dumps(dict(fixture=mode, medians=medians, head_over_base=ratio, gate=ratio <= 1.05)), flush=True)
    assert ratio <= 1.05, 'trusted performance regression exceeds 5%'
