# Bounded pack decoding and remote reads

`PackScanPlan::parallel_cursor` yields the same closure and pack order as
`PackScanPlan::cursor`, including automatically added bases and the `requested`
flag. Bodies are shared through `Arc`. Indexed headers, delta instructions,
inflation and Git identity checks use the existing pack grammar.

Supply `ParallelScanOptions::new(threads, window)` and an
`Arc<PackDecodeBudget>`. The window admits that many output positions and their
transitive dependencies. A ready batch contains at most `window` entries;
forward REF bases may lie outside the output window. Workers inflate, apply
ready deltas and hash objects. Each entry is inflated once. Each base is released
after its last dependent and its own output. Traversal is iterative, including
forward chains; workers use 2 MiB stacks. Each cursor owns a fixed Rayon pool,
so batch count does not multiply thread creation. One-worker mode runs inline.

`ScanLimits` bounds depth, active materialization and live bases. The shared
budget additionally caps active materialization across simultaneous readers;
share one instance to enforce a process-wide decoded-byte ceiling. Reservations
precede allocation and are released on error, cancellation and drop. Errors
terminate iteration. A work window that cannot fit the selected budget returns
a limit error; choose its size explicitly for the workload.

These are logical byte limits. Index/graph metadata is proportional to indexed
object count; allocator capacity/slack, worker stacks, fixed inflation scratch
and caller-held outputs are additional RSS. Retaining every returned object
necessarily consumes caller memory. Use a positional file source to avoid
charging a complete in-memory pack to the process. Async hosts should run the
synchronous cursor on a blocking CPU executor.

## Remote ODB

`RemoteOdbBackend` supplies asynchronous exact ranged reads for immutable keys.
`MemoryRemoteBackend` implements the same interface for tests. The caller supplies
pack/index keys, lengths and object format through `RemotePackLocation`.
The namespace and authorization policy belong to the caller.

`RemoteObjectDatabase::open` fetches a size-limited index and the pack's header
and trailer. `read_object` fetches only indexed spans in its dependency closure;
`read_objects` overlaps independent requests and preserves input order. Large
spans split into parallel pages. `RemoteBufferPool::new(page_bytes, slots)` caps
both in-flight requests and scratch bytes across handles. For example, 1 MiB
pages and eight slots reserve at most 8 MiB of scratch. No locks cross I/O.

Index checksum, each fetched entry's CRC and the resolved Git ID are checked.
Sparse sources expose indexed entry ends so header lookahead and inflation
cannot read an unfetched neighbor. CPU decoding reuses `BoundedPackDecoder` on
Tokio blocking workers, with a shared concurrency semaphore. Async futures hold
heap descriptors rather than recursively nested decode state. Dropping a read
cancels its I/O; a CPU job already submitted completes while retaining its
reservation and CPU permit.

The shared byte budget charges fetched encoded closures and reserves the full
configured decoder materialization ceiling during CPU work. This conservative
reservation can reject concurrency before the physical ceiling is reached.
Index metadata is separately size-limited. This slice opens one ordinary indexed
pack per handle; external thin bases return a missing-object error. It adds no
transport adapter, persistent cache, index partition format or local pack copy.

## Reproducing the benchmark

Use a release build, a fresh process for every sample and the configured Cargo
target directory. The benchmark uses positional reads and drops outputs after
counting. Time includes header inspection, dependency planning and decoding;
index loading precedes the timer. `threads=0` measures the existing sequential
cursor; `threads=1` and `threads=4` measure the new cursor.

```sh
export TMPDIR=/home/scratch
cargo build --locked --release -p sley-bench --bin pack-decode-bench
python3 crates/sley-bench/fixtures/generate_decode_pack.py "$TMPDIR/decode.pack"
git index-pack "$TMPDIR/decode.pack"
# Substitute the binary in Cargo's configured target directory.
/usr/bin/time -f 'peak_RSS_KiB=%M' /path/to/pack-decode-bench "$TMPDIR/decode.pack" 1 64
/usr/bin/time -f 'peak_RSS_KiB=%M' /path/to/pack-decode-bench "$TMPDIR/decode.pack" 4 64
```

The fixture has 128 independent chains, each 16 objects deep: 2,048 objects,
134,352,085 encoded bytes and 2,147,483,648 decoded bytes. Its pack checksum is
`193adbc42db84ade9f69ea37b3f8ffbab9031690`. Bases are deterministic pseudo-random
1 MiB blobs; each REF delta copies all but an eight-byte suffix. A 64-entry window
fits the 256 MiB shared cap while retaining 128 MiB of live bases.

Measurements and verification are recorded in the PR. MB/s uses decimal bytes;
RSS is measured independently by `/usr/bin/time`. Small objects can make worker
scheduling more expensive than decode, so worker count is an explicit caller
choice; the benchmark also supports real repository packs without regeneration.
