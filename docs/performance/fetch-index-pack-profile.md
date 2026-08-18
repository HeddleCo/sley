# Protocol-v2 fetch/index-pack profile

Measured on 2026-08-18 on the same host used for weft#1551. This is a
measurement-only investigation: no fetch, pack, or object-store behavior was
changed.

## Result up front

**Case 1: sley itself is slow.** An unchanged release build at `d0f3303c`
cloned rust-lang/rust over protocol-v2 HTTPS at **3.99 MB/s** (974,347,518-byte
pack in 244.38 s). Git's comparison run was **21.06 MB/s** (1,057 MB in 50.2
s), so sley alone is **5.3x slower** on this run. The 6.6x weft result is not
created solely by weft glue; the dominant gap is already present in sley's
fetch/index-pack path.

The bottleneck is **single-threaded inline inflate**, not GitHub or pkt-line
demux. On the complete profiled rust fetch, zlib inflate consumed 73.9% of wall
time. Actual reads from the HTTP response consumed only 3.5%; while a socket
read was active it delivered 120 MB/s. The client simply does not call `read`
while it inflates, hashes, and writes the preceding pack data on the same
thread.

The original hypothesis is therefore partly confirmed:

- inflate, pack checksum hashing, undeltified-object OID hashing, and the pack
  staging write all run inline on the receive thread and starve further socket
  reads;
- delta resolution is also single-threaded, but it begins only after the full
  pack and trailer have been received, so delta apply does not directly starve
  the socket;
- all inflated base bodies and delta programs are buffered until that final
  resolution pass, producing a **42.0 GB (40.1 GiB) peak RSS** for a 974 MB
  pack.

## Workloads

| Workload | Installed pack | Wall time | Throughput | Peak RSS |
|---|---:|---:|---:|---:|
| rust-lang/rust, unchanged `sley clone -n` | 974,347,518 B | 244.38 s | 3.99 MB/s | 40,562,720 KiB |
| rust-lang/cargo, profiled | 75,317,647 B | 12.849 s | 5.86 MB/s | 1,905,428 KiB |
| rust-lang/rust, profiled | 974,387,947 B | 229.653 s | 4.24 MB/s | 42,026,740 KiB |
| rust-lang/rust, Git comparison from weft#1551 | 1,057 MB | 50.2 s | 21.06 MB/s | — |

The two rust packs were fetched at different moments and therefore have
different pack IDs and a 40,429-byte size difference. Throughput is calculated
from each run's installed `.pack`, not progress text. The profiled run being
slightly faster than the uninstrumented run shows that network/run variance is
larger than the timer overhead; it is not an optimization result.

## Full rust stage breakdown

Timers are exclusive: when inflate calls demux, and demux calls the socket, the
child duration is removed from its parent. Percentages therefore add without
double-counting.

| Stage | Wall time | % total | Evidence |
|---|---:|---:|---|
| Socket read | 8.114 s | 3.53% | 974,913,138 HTTP-body bytes; 120.15 MB/s while actively reading |
| pkt-line / sideband-64k demux | 0.630 s | 0.27% | 974,387,947 band-1 pack bytes delivered |
| Inflate | 169.764 s | **73.92%** | 3,457,840 objects; 2,179,595,639 inflated bytes |
| Delta resolution | 22.026 s | 9.59% | 2,675,241 ref/ofs deltas |
| OID / SHA hashing | 16.949 s | 7.38% | pack checksum plus every resolved object OID |
| Object-store write | 4.845 s | 2.11% | 1,071,208,539 B = `.pack` + `.idx`; 2 fsyncs |
| Other orchestration/index construction | 7.326 s | 3.19% | headers, CRC32, maps, sorting/index encoding, clone setup |
| **Total** | **229.653 s** | **100%** | 4.243 MB/s end to end |

The raw counter output is in [rust-fetch-profile.txt](rust-fetch-profile.txt).

## What the path actually does

The CLI clone does not shell out to `git index-pack`, and it does not traverse
the buffered helpers in `sley-protocol/src/upload_pack.rs`. The protocol-v2
HTTPS path is:

1. `sley-remote/src/http.rs` owns the HTTP response body.
2. `sley-protocol::StreamingSidebandReader` parses pkt-lines, discards progress,
   and exposes band-1 pack bytes as `Read`.
3. `sley-odb::PackInstallTeeReader` writes those raw bytes to one temporary pack
   while `sley-pack::PackReadStream` consumes the same bytes.
4. For each object, `inflate_entry_from_stream` uses one thread-local
   `flate2::Decompress` backed by **zlib-rs**, then undeltified objects are
   hashed immediately. The streaming pack checksum is updated in the same hot
   loop.
5. Every inflated object body or delta program is retained in
   `parsed_entries`. After the trailer checksum, `resolve_pack_entries` builds
   in-memory offset and OID hash maps, resolves ofs/ref bases, applies every
   delta serially, and hashes each resolved object.
6. The raw pack is renamed into place and a v2 `.idx` is written. No loose
   objects are written. The pack and index each receive one `sync_all` (2 total).

Base lookup is buffered hash-map lookup, not random access back into the pack.
The costly part is retaining and processing all decoded material serially, not
disk seeks. SHA-1 is RustCrypto's hardware-dispatched implementation from the
default `fast-sha1` feature, not SHA-1DC.

The sideband reader does allocate a payload vector and copy band-1 data into
the caller buffer, but its measured 0.27% makes it immaterial to this gap.

## SIGPROF sampled stacks

The reusable `fetch-profile` binary uses `pprof-rs`'s SIGPROF sampler and needs
no `perf_event_open` privileges. The rust-lang/rust run sampled the complete
fetch at 100 Hz. Its [collapsed stacks](rust-fetch-profile.folded.txt) use the
standard folded format and can be rendered by an offline flamegraph tool.

The wide receive/index stack terminates in `inflate_entry_from_stream` and
`zlib_rs::inflate`; the post-receive tail is `resolve_pack_entries`/delta apply
and object hashing. Socket and sideband frames are narrow, matching the explicit
stage timers.

## Reproducing

```sh
CARGO_HOME=/path/to/writable/cargo-home \
  cargo build --profile fetch-profile -p sley-cli \
  --bin fetch-profile --features fetch-profile

target/fetch-profile/fetch-profile \
  --report docs/performance/rust-fetch-profile.txt \
  --folded docs/performance/rust-fetch-profile.folded.txt \
  --checkpoint-seconds 60 \
  -- clone -n https://github.com/rust-lang/rust /path/to/empty/destination
```

`--checkpoint-seconds` is optional. It atomically refreshes both artifacts
during very large runs so an OOM at the retained-object peak does not discard
the earlier samples. Omitting it adds no checkpoint thread or symbolization
work to the measured command.

The harness is compile-time gated by `fetch-profile`. A normal `sley` build has
none of the stage spans, counters, socket wrapper, or `pprof` dependency in its
active graph.
