# ADR 0002 — Streaming I/O house rule

- **Status:** Accepted
- **Date:** 2026-07-13
- **Related:** [ADR 0001](0001-cli-layer-engines.md) (CLI-layer engines), pack install /
  protocol sideband paths in `sley-protocol`, `sley-fetch`, `sley-remote`,
  `sley-core::cancel`

## Context

sley moves large Git payloads (packfiles, object bodies, status/diff streams)
across crate boundaries. Early code often buffered whole responses or entire
sideband demux results before hand-off, which:

- raised peak RSS on large clones/fetches,
- delayed first-byte progress and cooperative cancel,
- made “consumer stopped early” paths leave unread wire data (blocking
  connection reuse or protocol state).

Streaming and cancellation have already landed in key places
(`StreamingSidebandReader`, `CancellableRead`, `drain_to_end` after pack
install, `*_with_cancel` on fetch/push/clone). This ADR records the **house
rule** so new code and reviews share one model instead of re-deriving it per
command.

## Decision

### 1. Large payloads stream (pull `Read` / `Write`)

Treat pack data, object bodies, bulk index-pack input, and similar
multi-megabyte (or unbounded) payloads as **byte streams**:

- Producers expose `Read` (or write into a caller-supplied `Write`).
- Consumers pull; they do not require the full buffer up front.
- Prefer frame/chunk boundaries already present on the wire (pkt-line, pack
  object headers) as natural poll points.

Do **not** collect an entire remote pack or demuxed pack section into a `Vec`
unless a specific algorithm truly needs random access and the size is known to
be small or bounded by a deliberate limit.

### 2. Buffer only small control messages

Control-plane messages stay buffered and fully parsed:

- capability advertisements and short negotiation lines,
- individual pkt-line frames for ACKs, shallow lines, command lists,
- config, refnames, and other O(refs) or O(1) structures,
- error/fatal strings from the peer.

If a message is small by protocol design (a few KiB at most), owning a
`Vec`/`String` is fine and usually clearer than a streaming parser.

### 3. Sideband demux is a `Read` adapter

Side-band / side-band-64k is **not** “parse all packets, then hand a buffer to
pack install.” It is a **`Read` adapter** over the data channel:

- [`sley_protocol::StreamingSidebandReader`] demuxes channel 1 as `Read`,
- channel 2 (progress) is delivered via callback as frames arrive,
- channel 3 (fatal) and protocol errors surface as `io::Error` /
  mapped `GitError` variants.

Pack install and related paths wrap that adapter (often with
[`sley_core::CancellableRead`]) so inflate/index work proceeds as data arrives.

### 4. `drain_to_end` after early consumer stop

When a consumer finishes early—most often pack install after the trailer is
complete while the wire still has trailing progress frames and a flush—call
**`drain_to_end`** (or an equivalent full-stream drain) on the sideband/source
reader.

Reasons:

- match the historical buffered demux path, which always consumed the full
  response;
- keep HTTP keep-alive / connection reuse correct;
- avoid leaving the peer blocked on an unread response body.

Early **logical** stop (enough pack data) must not imply abandoning the
**transport** stream unless the operation is failing closed and the connection
is discarded.

### 5. Cancel is cooperative + optional transport kill

Cancellation is **cooperative** on hot loops:

- poll [`CancelFlag`] (`Option<&AtomicCancel>`) between units of work (objects,
  windows, pkt-lines, emit callbacks);
- [`CancellableRead`] fails with [`OperationCancelled`] under
  `ErrorKind::Other` — **not** `Interrupted` (`read_exact` / pkt-line treat
  that as EINTR and retry forever);
- after cancel or install error, **do not** `drain_to_end` (would download the
  rest of a multi‑GB pack);
- services carry `cancel: CancelFlag` (default `CancelFlag::never()`).

A flag trip **does not** interrupt a thread blocked in kernel I/O by itself.
When preemption is required (UI stop, SIGINT while stuck on a socket), the
embedder may **also** close or kill the underlying transport (drop the HTTP
body, `kill_child_if_cancelled` on SSH). The CLI handler exits on a **second**
Ctrl-C if the first cooperative cancel cannot complete promptly.

Distinguish:

| Signal | Meaning |
|---|---|
| [`StreamControl::Stop`] | Consumer finished successfully early (status/revwalk emit). |
| [`GitError::Cancelled`] | External cancel source requested abort. |
| Transport close | Best-effort wake-up of blocked I/O; pair with cooperative flag. |

## Consequences

**Positive.**

- One review checklist for network, pack, and streaming CLI paths.
- RSS and time-to-first-progress stay bounded as parity grows.
- Cancel and connection reuse stay compatible with Git’s full-response
  consumption model.

**Negative / cost.**

- Call sites must remember `drain_to_end` after early stop; missing it is a
  subtle reuse bug.
- Streaming error mapping (sideband fatal → `InvalidFormat`, cancel →
  `Cancelled`) must stay consistent at each adapter boundary.
- Tests need chunked/fixture streams, not only fully buffered vectors.

**Neutral.**

- Small-message buffering remains normal; the rule is about **large**
  payloads and demux shape, not “never allocate.”
- CLI presentation (progress lines, exit codes) stays outside the stream
  adapters; engines return structured errors and bytes only.

## Implementation anchors

| Concern | Location (indicative) |
|---|---|
| Sideband `Read` demux + `drain_to_end` | `crates/sley-protocol/src/sideband.rs` |
| Streaming pack install + cancel | `crates/sley-fetch`, `crates/sley-remote/src/install.rs` |
| Cancel primitives | `crates/sley-core/src/cancel.rs` |
| Facade `*_with_cancel` | `crates/sley/src/remote.rs` |
| Engine investment roadmap | [`docs/ROADMAP_ENGINES.md`](../ROADMAP_ENGINES.md) |

## Evidence

Accepted after stream-cancelling work: protocol-v2 packfile section install via
`StreamingSidebandReader`, post-install `drain_to_end`, and cooperative cancel
mid-sideband (see `sley-fetch` / `sley-protocol` tests for chunked sideband and
cancel cases). This ADR freezes the policy so future engines (transport,
unpack-trees streaming checkout, format substrate) inherit the same I/O shape.
