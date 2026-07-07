# Network & Transport Crates Review

**Scope:** `sley-protocol`, `sley-transport`, `sley-fetch`, `sley-remote`  
**Date:** 2026-07-05  
**Evidence:** Source reads, ripgrep, cross-reference with `01-security.md`, `02-performance.md`, `03-legacy-migration.md`

---

## Summary

The network stack is a well-layered Rust git transport implementation with strong pkt-line framing, explicit protocol v0/v1/v2 codecs, streaming pack install, and embedder-friendly seams (`CredentialProvider`, `ProgressSink`, `TransportCapabilities`). Protocol parsing is hardened (`PKT_LINE_MAX_LEN`, delimiter-byte rejection, `deny(unwrap_used, expect_used)` in production).

Main gaps are **feature parity across transports** (partial-clone `filter`, `deepen-since`/`deepen-not`, `deepen-relative`, and protocol v2 on SSH), **unbounded remote pack/credential reads** (see security review H1/M4), **credential-bearing URLs in user-visible output**, and **architectural coupling** (`sley-remote` is a 13-module orchestrator depending on 12 workspace crates; `sley-protocol` is a 14k-line monolith). HTTP fetch correctly auto-selects v2 when the server negotiates it; SSH remains upload-pack v0/v1 only.

| Crate | Lines | Tests | Role |
|-------|------:|------:|------|
| `sley-protocol` | 14,224 | 153 | Wire codecs: pkt-line, v0/v1/v2, sideband, upload/receive-pack |
| `sley-transport` | 2,725 | 31 | URL parse, service discovery, credentials, smart-HTTP client |
| `sley-fetch` | 608 | 8 | Pack-install glue: demux sideband → stream into ODB |
| `sley-remote` | 13,491 | 59 (+7 live) | fetch/push/clone/ls-remote orchestration |

---

## Per-Crate Assessment

### sley-protocol

**Strengths**
- Pkt-line bounds enforced at encode and decode (`PKT_LINE_MAX_LEN = 65_520`, `PKT_LINE_MAX_PAYLOAD_LEN = 65_516`).
- Protocol v0/v1 ref advertisements, v2 handshake + command RPCs (`ls-refs`, `fetch`), upload-pack/receive-pack negotiation, sideband demux, and shallow-info sections are implemented with extensive round-trip tests (153 `#[test]` blocks).
- Duplicate-capability and malformed-section rejection is thorough (e.g. duplicate `deepen`, `filter`, `packfile` sections).
- Streaming header reader `read_upload_pack_raw_packfile_response_header` peeks pkt-lines until `PACK`, returning `pack_prefix` so downstream can chain the reader without buffering the full response.

**Weaknesses**
- 14k-line single `lib.rs` — hard to review, slow to compile, discourages targeted testing outside the monolith.
- Convenience `read_*` helpers that call `read_to_end` (e.g. `read_upload_pack_raw_packfile_response`, `read_dumb_http_*`) buffer entire bodies; callers must prefer streaming APIs (`read_upload_pack_raw_packfile_response_header`, `read_pkt_line_frame`).
- `read_fetch_head` buffers unbounded input (local file, lower risk than network).

**Protocol correctness:** High for tested paths. v2 fetch sideband-all demux, shallow-info ordering, and object-format negotiation are covered by unit tests including `protocol_v2_fetch_packfile_demux_rejects_duplicate_or_bad_sideband`.

---

### sley-transport

**Strengths**
- Clean separation: URL parsing, service-request encoding, credential protocol, smart-HTTP URL construction (userinfo excluded from request URLs), and optional `ureq` HTTP client behind `http-client` feature.
- `HttpResponse.body` is `Box<dyn Read + Send>` — response bodies stream from the connection, not buffered in memory (`http_response_from_ureq`).
- `UreqHttpClient` uses `ureq::Agent` with `http_status_as_error(false)` so 401/403 surface as `Ok(HttpResponse)` for credential retry logic.
- URL/host/path validation rejects delimiter bytes (`\n`, `\r`, NUL) in service fields, remote hosts, SSH users, and credential keys/values.
- TLS backend selection forwarded via features (`tls-rustls`, `tls-native-tls`, `tls-platform-verifier`).

**Weaknesses**
- `read_git_credential` uses unbounded `read_to_end` (security review M4).
- Small framing helpers duplicated from `sley-protocol` (comment at `lib.rs:1610–1612`) — drift risk.
- Wildcard `use sley_protocol::*` couples transport tightly to protocol's entire public surface.
- Each operation in `sley-remote` constructs a fresh `UreqHttpClient::new()` — agent-level connection reuse exists within one client, but not across fetch/push/ls-remote calls.
- `post_reader` default impl buffers the entire body before POST (overridden by `UreqHttpClient` for chunked upload).

**Error handling:** Consistent `Result<GitError>` propagation; HTTP transport failures include the offending URL. Credential parse errors are specific (`credential line is missing = delimiter`).

---

### sley-fetch

**Strengths**
- Focused glue crate: maps upload-pack / v2-fetch wire responses to `RawPackInstaller` without buffering full packs.
- `ProtocolV2PackfileReader` demuxes sideband pkt-lines into a streaming `Read` impl with a small pending buffer; fatal sideband channel surfaces as `InvalidData`.
- Promisor install path (`install_*_promisor_*`) correctly sets `RawPackInstallOptions { promisor: true }`.
- 8 integration tests cover raw upload-pack, shallow raw, v2 response (with progress sideband), promisor sidecar, empty v2 response, and custom `RawPackInstaller`.
- `#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]`.

**Weaknesses**
- No pack-size cap on `install_raw_pack_from_reader` — relies entirely on `sley-odb` streaming with no `fetch.maxInputSize` guard (security review H1).
- v2 sideband progress packets are silently discarded (correct for install, but no hook for `ProgressSink` at this layer).
- Thin crate (608 lines) — appropriate, but means all orchestration complexity lives in `sley-remote`.

---

### sley-remote

**Strengths**
- Callable library API with explicit parameters (no global `GIT_DIR`); `CredentialProvider` / `ProgressSink` seams are embedder-friendly.
- HTTP fetch auto-selects protocol path:

```400:417:crates/sley-remote/src/fetch.rs
            let shallow_info = if discovered.set.protocol == ProtocolVersion::V2 {
                let handshake = discovered.handshake.as_ref().ok_or_else(|| { ... })?;
                crate::http::install_fetch_pack_via_http_protocol_v2_fetch(...)
            } else {
                crate::http::install_fetch_pack_via_http_upload_pack(...)
            };
```

- `http_send_with_auth` retries once on 401 with credential-provider fill; approves/rejects credential on outcome.
- Transport policy (`protocol.rs`) honors `GIT_ALLOW_PROTOCOL`, `protocol.<scheme>.allow`, and `protocol.allow` with `user`/`always`/`never` semantics matching git.
- Push uses chunked `post_reader` when body exceeds `http.postBuffer` (tested in `push.rs`).
- Live integration tests (GitHub public, private HTTPS, SSH fetch/push, shallow clone) gated on env vars.
- Local in-process upload-pack/receive-pack path supports filter, deepen-since/not, deepen-relative — full feature superset for `file://`.

**Weaknesses**
- **Transport feature parity gaps** documented inline in `FetchOptions`:

```116:147:crates/sley-remote/src/fetch.rs
    /// Partial-clone object filter (`--filter=blob:none`): ... Local-only today: HTTP and SSH do not
    /// send `filter` requests yet ...
    /// `--deepen=N`: ... Local-only today; HTTP and SSH treat `depth` as an absolute `--depth N`.
    /// `--shallow-since=...`: Local-only today; HTTP and SSH do not send `deepen-since` yet.
    /// `--shallow-exclude=...`: Local-only today; HTTP and SSH do not send `deepen-not` yet.
```

- SSH path always uses upload-pack v0/v1 (`ssh_upload_pack_advertisements`); no protocol v2 `ls-refs`/`fetch` over SSH despite HTTP v2 being live.
- `git://` v2 requires explicit `protocol_v2: true` on `FetchSource::Git` or `protocol.version=2` config; resolved URLs default `protocol_v2: false` (`resolve.rs:92–95`).
- Stale migration scaffolding in `capabilities.rs` (`HTTP_PROTOCOL_V2_FETCH`, `SSH_CLONE_SUPPORTED`, etc. all `true` with "Flip once…" comments).
- Heavy coupling: 12 workspace dependencies; `fetch.rs` (2,319 lines) and `push.rs` (3,111 lines) mix ref-map planning, transport dispatch, and I/O.
- Credential-bearing URLs echoed in `FETCH_HEAD` descriptions and prune output without redaction (`fetch.rs:1844–1861`, `1898–1903`).
- `ancestor_depths` slow BFS copy in `push.rs` (legacy migration review item) — network-adjacent performance debt.

**Test coverage:** 59 unit tests across 12 modules; strongest in `push.rs` (13) and `fetch.rs` (4). HTTP v2 round-trip tested in `http.rs`. SSH/ext parsing has 3 tests. No mocked-HTTP transport tests in `sley-transport` for full fetch flow (live tests only).

---

## Cross-Crate Issues

### 1. Layering and dependency direction

```
sley-remote
  ├── sley-fetch ──→ sley-protocol, sley-transport, sley-odb
  ├── sley-transport ──→ sley-protocol
  └── sley-protocol ──→ sley-core
```

- **Correct direction:** codecs at bottom, orchestration at top.
- **Leakage:** `sley-remote` imports `sley_protocol::*` types directly in `fetch.rs`, `http.rs`, `ssh.rs`, `git.rs`, `local.rs` — orchestration is protocol-aware at many call sites instead of through narrow transport traits.
- **Duplication:** `sley-transport` re-implements pkt-line framing helpers and service-discovery parsing that also exist in `sley-protocol`; comment acknowledges this is intentional to avoid exposing internals, but it creates a maintenance fork.

### 2. Legacy dual protocol paths (intentional, but uneven)

| Transport | Discovery | Pack negotiation | v2 fetch |
|-----------|-----------|------------------|----------|
| HTTP(S) | v0/v1 refs or v2 handshake + `ls-refs` RPC | upload-pack **or** v2 `fetch` (auto) | ✅ |
| SSH | upload-pack v0/v1 (skip re-advertised refs) | upload-pack only | ❌ |
| git:// | v0/v1 refs or v2 handshake + `ls-refs` on stream | upload-pack **or** v2 `fetch` (config/flag) | ✅ (opt-in) |
| local | in-process | upload-pack v0/v1/v2 | ✅ |

SSH is the outlier: servers advertising only v2 will fail over SSH while succeeding over HTTPS.

### 3. Streaming vs buffering contract

The happy path is streaming end-to-end:
1. `sley-transport`: HTTP body → `Box<dyn Read>`
2. `sley-protocol`: `read_upload_pack_raw_packfile_response_header` / `read_protocol_v2_fetch_response_header` → pkt-line framed
3. `sley-fetch`: `ProtocolV2PackfileReader` or `Cursor::chain(reader)` → `install_raw_pack_from_reader`
4. `sley-odb`: temp-file streaming index-pack

**Risk:** Any caller using `parse_upload_pack_raw_packfile_response` or `read_upload_pack_raw_packfile_response` (full buffer) bypasses streaming. Current `sley-remote` paths use the streaming header APIs.

### 4. HTTP client lifecycle

`new_http_client()` is called per operation in `fetch.rs`, `push.rs`, `ls_remote.rs`. Within a single fetch, the same `client` reference is reused for info/refs + ls-refs + pack RPC (good). Across sequential fetches/pushes, connections are not pooled. Acceptable for CLI parity; embedders doing high-volume fetches may want a shared `HttpClient` injection point (today `HttpFetchPackRequest.client` is `&UreqHttpClient` but construction is internal).

### 5. Feature-flag matrix

| Feature | `sley-transport` | `sley-remote` |
|---------|------------------|---------------|
| `http-client` | ureq HTTP | via `http` feature |
| `tls-rustls` / `tls-native-tls` / `tls-platform-verifier` | forwarded | forwarded |
| `ssh` | N/A (subprocess) | default on |
| `http` off | no HTTP types | `FetchSource::Http` → `Unsupported` |

`TransportCapabilities::current()` correctly reflects compile-time features; stale flip-constant indirection should be removed.

---

## Security Notes

| ID | Severity | Issue | Location |
|----|----------|-------|----------|
| S1 | HIGH | Unbounded pack download/install on fetch (no `fetch.maxInputSize`) | `sley-fetch` → `sley-odb::install_raw_pack_from_reader*` |
| S2 | MEDIUM | Credential helper stdout unbounded (`read_to_end`) | `sley-transport:495–498`, `sley-remote/credentials.rs:155` |
| S3 | MEDIUM | Remote URLs with embedded credentials in `FETCH_HEAD` / prune output | `sley-remote/fetch.rs:1844–1861`, `1898–1903` |
| S4 | LOW | Inherited shell execution via `credential.helper` `!cmd` and absolute-path helpers | `sley-remote/credentials.rs:108–117` (documented, git-parity) |
| S5 | LOW | `ext::` remotes execute configured helper commands with `GIT_EXT_SERVICE` env | `sley-remote/ssh.rs:124–150` (git-parity; operator-trusted config) |
| S6 | INFO | Smart-HTTP request URLs exclude userinfo; Basic auth built from parsed credential with delimiter checks | `sley-transport:575–576`, `542–556` |
| S7 | INFO | Transport policy blocks disallowed schemes (`protocol.*.allow`, `GIT_ALLOW_PROTOCOL`) | `sley-remote/protocol.rs` |

**Injection surfaces mitigated:** NUL/newline rejection in service fields, remote hosts, credential keys; pkt-line length caps; SSH repo path single-quoting in `quote_ssh_repository_path`.

**Injection surfaces inherited:** `!credential.helper` shell snippets, `ext::` command strings, `GIT_SSH` / `GIT_SSH_COMMAND` program selection.

---

## Priority Actions

### P1 — Security / resource limits
1. **Add `fetch.maxInputSize`** (or reuse `transfer.maxSize`) on the streaming pack install path in `sley-fetch` / `sley-odb`; fail closed when exceeded.
2. **Cap credential helper response** (e.g. 64 KiB) in `read_git_credential`; reject overflow.
3. **Redact credentials from user-visible URLs** in `trim_fetch_head_display_url`, prune `URL:` lines, and push error paths (reuse `sley-core` trace2 redaction or equivalent).

### P2 — Transport parity
4. **Wire `filter`, `deepen-since`, `deepen-not`, `deepen-relative`** over HTTP upload-pack and v2 fetch request builders; gate in `FetchOptions` only after implementation.
5. **Add SSH protocol v2** (`ls-refs` + `fetch` over ssh subprocess) or document explicit unsupported status in `TransportCapabilities`.
6. **Default `git://` v2** when server advertises v2 on connect (mirror HTTP auto-negotiation) instead of requiring `protocol.version=2`.

### P3 — Architecture / maintainability
7. **Split `sley-protocol/src/lib.rs`** into modules (`pkt_line`, `v2`, `upload_pack`, `receive_pack`, `sideband`) — highest-value decomposition in the network stack.
8. **Deduplicate transport/protocol framing helpers** — expose minimal internal traits or `pub(crate)` shared module instead of verbatim copy in `sley-transport`.
9. **Remove stale capability flip constants** in `capabilities.rs`; inline live values or derive from compile-time features.
10. **Optional shared `HttpClient` injection** on `FetchRequest` / `PushRequest` for embedders needing connection reuse.

### P4 — Testing
11. **Add HTTP transport mock tests** in `sley-remote` for v0/v1/v2 fetch paths without live network (extend `push.rs` `RecordingHttpClient` pattern).
12. **Add SSH protocol v2 tests** once implemented; today SSH v2 gap is untested because unsupported.
13. **Add pack-size-limit regression test** once P1 lands.

---

## Verdict

The network stack is **production-viable for common fetch/push/clone over HTTP(S), SSH, git://, and local paths**, with protocol correctness and streaming pack install as clear strengths. Before treating it as hardened against hostile remotes, close the **pack size cap** and **credential-read bounds** gaps. Before claiming full git fetch parity, close the **HTTP/SSH filter and deepen** gaps and **SSH v2** omission. Structural debt (`sley-protocol` monolith, `sley-remote` orchestrator size) is manageable but will slow future protocol work unless addressed.