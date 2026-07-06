# Security Review

## Summary

Sley is a security-conscious Rust git implementation with workspace-wide `unsafe_code = "forbid"`, isolated unsafe in `sley-mmap` and `sley-procinfo`, strong pkt-line bounds, pack delta allocation hardening, and thorough worktree path guards. No CRITICAL issues were found. The main actionable gaps are **unbounded pre-allocation in binary-patch apply paths** (inconsistent with pack hardening), **credential-bearing remote URLs echoed to user-visible output without redaction**, and **inherited git shell-execution surfaces** (credential helpers, hooks, filters) that operators must treat as trusted configuration.

Audit scope: commit `250e1f54` on `main`, evidence from source reads and ripgrep across the workspace.

## Critical Findings

None identified.

## High Findings

### H1 — Fetch/clone accepts arbitrarily large packs (disk + CPU DoS)

**Severity:** HIGH  
**Location:** `crates/sley-odb/src/lib.rs:6115–6150`, `crates/sley-fetch/src/lib.rs:16–104`  
**Evidence:** `install_raw_pack_from_reader_with_options` streams an unbounded pack from the network reader to a temp file with no `receive.maxInputSize`-style cap. Unlike `read_capped_packfile` in `crates/sley-cli/src/commands/remote_cmds.rs:5594–5616`, which honors `receive.maxInputSize` for receive-pack fsck, the fetch install path has no equivalent limit.  
**Impact:** A malicious or compromised remote can force multi-gigabyte downloads and expensive index-pack work, exhausting disk and CPU. RAM pressure is mitigated by streaming, but disk/CPU exhaustion remains.  
**Recommended fix:** Add a configurable `fetch.maxInputSize` / reuse `transfer.maxSize` (or mirror git's pack size limits) on the streaming install path; fail closed when exceeded.

## Medium Findings

### M1 — Binary patch apply: unbounded `origlen` pre-allocation (OOM)

**Severity:** MEDIUM  
**Location:** `crates/sley-cli/src/commands/plumbing.rs:6014`, `6327–6331`; `crates/sley-diff-merge/src/lib.rs:7000`, `7372–7380`  
**Evidence:**

```6327:6331:crates/sley-cli/src/commands/plumbing.rs
fn inflate_zlib_exact(deflated: &[u8], expected_len: usize) -> Option<Vec<u8>> {
    ...
    let mut out = Vec::with_capacity(expected_len);
```

`expected_len` comes from `frag.origlen`, parsed via `parse_leading_usize` which saturates to `usize::MAX` (`crates/sley-diff-merge/src/lib.rs:7372–7380`). No bound ties `origlen` to the actual deflated payload size.  
**Impact:** `git apply` on a crafted binary patch can abort the process (OOM) before zlib validation completes. Local attacker supplying a patch file.  
**Recommended fix:** Route through `bounded_inflate_reserve` (as in `sley-pack`) or cap `origlen` against deflated length × max expansion ratio before `with_capacity`.

### M2 — `git_patch_delta` lacks pack-style allocation bounds

**Severity:** MEDIUM  
**Location:** `crates/sley-diff-merge/src/lib.rs:7454–7455`, `7433`; consumer `crates/sley-cli/src/commands/plumbing.rs:6018`  
**Evidence:**

```7454:7455:crates/sley-diff-merge/src/lib.rs
    let result_size = read_hdr_size(&mut data)?;
    let mut out = Vec::with_capacity(result_size);
```

`sley-pack` hardens the same pattern at `crates/sley-pack/src/lib.rs:4823–4833` with `bounded_inflate_reserve`, but `git_patch_delta` (used for binary delta apply) does not.  
**Impact:** Crafted binary delta in a patch can trigger large upfront allocation / OOM during `git apply`.  
**Recommended fix:** Share `bounded_inflate_reserve` between `sley-pack` and `sley-diff-merge`, or duplicate the bound in `git_patch_delta`.

### M3 — Remote URLs with embedded credentials leak to user-visible output

**Severity:** MEDIUM  
**Location:** `crates/sley-remote/src/fetch.rs:1844`, `1850–1861`, `1898–1903`; `crates/sley-cli/src/commands/remote_cmds.rs:7646–7650`, `8914–8919`  
**Evidence:** `trim_fetch_head_display_url` only strips trailing slashes and `.git`; it does not redact `user:password@` userinfo. Prune progress emits `URL: {display_url}` with the raw configured URL. Push errors use `push_resolved_url` which returns `resolve_remote_push_url` unchanged.  
Contrast: trace2 redaction exists in `crates/sley-core/src/lib.rs:422–462` (`GIT_TRACE2_REDACT`), and smart-HTTP request URLs intentionally exclude userinfo (`crates/sley-transport/src/lib.rs:575–576`).  
**Impact:** Passwords/tokens in `remote.*.url` (e.g. `https://token@host/repo.git`) appear in FETCH_HEAD notes, fetch prune output, and push error messages.  
**Recommended fix:** Apply `redact_unsafe_urls` (or equivalent) to all user-facing URL formatting paths; document that credentials belong in credential helpers, not URLs.

### M4 — Credential helper stdout read is unbounded

**Severity:** MEDIUM  
**Location:** `crates/sley-transport/src/lib.rs:495–498`; invoked from `crates/sley-remote/src/credentials.rs:155`  
**Evidence:** `read_git_credential` uses `reader.read_to_end(&mut input)?` with no size cap.  
**Impact:** A malicious or buggy credential helper can cause unbounded memory allocation. Mitigated by the helper being operator-controlled local config, but a compromised helper is a realistic threat.  
**Recommended fix:** Cap credential helper response size (e.g. 64 KiB) and reject overflow.

## Low / Informational

### L1 — Inherited shell execution: credential helpers (documented)

**Severity:** LOW (inherited git threat model)  
**Location:** `crates/sley-remote/src/credentials.rs:108–117`, `125–129`  
**Evidence:** `!cmd` helpers run via `sh -c '{shell} "$@"' sh <op>`; absolute-path helpers are executed as programs with trailing args. Comments and parity tests (`credential_dispatch_parity_tests`) document this matches git 2.54.  
**Impact:** Malicious `credential.helper` config executes arbitrary shell. Same as upstream git.  
**Recommended fix:** Document in operator security guide; optional hardening mode that rejects `!`-prefixed helpers.

### L2 — Inherited shell execution: configured hooks

**Severity:** LOW  
**Location:** `crates/sley-cli/src/commands/hooks.rs:600–603`  
**Evidence:** `HookCommand::Configured` runs `sh -c <command>`. Traditional hooks execute the hook file directly (no shell injection on hook name — name comes from fixed `KNOWN_HOOKS` list or explicit CLI arg).  
**Impact:** `hook.*.command` in config is arbitrary code execution, matching git.  
**Recommended fix:** Document; consider opt-in restriction for enterprise deployments.

### L3 — Inherited shell execution: filter drivers

**Severity:** LOW  
**Location:** `crates/sley-worktree/src/filter.rs:982–996`, `1215–1218`  
**Evidence:** `filter.<attr>.clean/smudge` and process filters spawn `/bin/sh -c <command>`. `%f` substitution uses `shell_quote` (`filter.rs:1527–1538`).  
**Impact:** Malicious filter config runs arbitrary code; path injection via `%f` is mitigated by quoting.  
**Recommended fix:** None required for parity; document trusted-config assumption.

### L4 — `read_to_end` buffer APIs in protocol (non-hot production paths)

**Severity:** LOW  
**Location:** `crates/sley-protocol/src/lib.rs:5158–5164`, `5346`, `5537`  
**Evidence:** Buffer-oriented helpers slurp entire responses. Production fetch uses streaming readers (`sley-fetch` + `install_raw_pack_from_reader`).  
**Impact:** DoS if these APIs are used on untrusted streams by embedders.  
**Recommended fix:** Document streaming APIs as preferred; add optional size limits to buffer helpers.

### L5 — Symlink targets may escape worktree on checkout (git-compatible)

**Severity:** INFO  
**Location:** `crates/sley-worktree/src/checkout.rs:1997–2003`  
**Evidence:** Symlink blobs are written with raw target bytes; no restriction on absolute/`..` targets. `resolve_tree_path_follow_symlinks` reports `OutOfRepo` for traversal (`crates/sley-rev/src/lib.rs:5437–5450`).  
**Impact:** Checkout of untrusted tree can create symlinks pointing outside worktree; subsequent operations may follow them. Matches git semantics.  
**Recommended fix:** Optional `core.symlinks=false` / safe-checkout mode for untrusted repos.

### L6 — TLS defaults to bundled Mozilla roots unless platform-verifier feature enabled

**Severity:** INFO  
**Location:** `crates/sley-transport/src/lib.rs:1498–1525`, `crates/sley-transport/Cargo.toml:12–15`  
**Evidence:** Default `tls-rustls` uses webpki-roots; `tls-platform-verifier` is optional.  
**Impact:** Enterprise environments relying on system trust stores need explicit feature selection.  
**Recommended fix:** Document TLS feature matrix in deployment guide.

### L7 — `parse_leading_usize` saturates to `usize::MAX`

**Severity:** INFO  
**Location:** `crates/sley-diff-merge/src/lib.rs:7372–7380`  
**Evidence:** Decimal overflow saturates rather than errors.  
**Impact:** Feeds into M1/M2 allocation paths.  
**Recommended fix:** Return `Option<usize>` with explicit overflow error.

## Positive Observations

### Unsafe code isolation

- Workspace `unsafe_code = "forbid"` in root `Cargo.toml:86`; only `sley-mmap` and `sley-procinfo` allow unsafe locally.
- **`sley-mmap`** (`crates/sley-mmap/src/lib.rs`): Single audited `Mmap::map` behind safe entry points. `open_pack`/`open_index`/`open_multi_pack_index`/`open_commit_graph` reject symlinks and non-regular files (`lib.rs:74–80`, `103–109`, etc.). Documented SIGBUS invariant: git objects written by atomic rename, never truncated in place (`lib.rs:13–22`).
- **`sley-procinfo`** (`crates/sley-procinfo/src/lib.rs`): Minimal `dup`/`from_raw_fd` (`lib.rs:27–33`) and macOS `proc_pidinfo` (`lib.rs:116–130`) with SAFETY comments. Linux uses `/proc` reads (no unsafe). Ancestry walk capped at 10 PIDs (`lib.rs:40`, `lib.rs:152`).

### Path traversal and symlink safety

- `worktree_path` rejects absolute paths and `..` components (`crates/sley-worktree/src/index_io.rs:1774–1789`).
- `check_apply_path_safety` mirrors git `verify_path` + `path_is_beyond_symlink` (`crates/sley-cli/src/commands/plumbing.rs:4827–4900`).
- Checkout pathspec resolution uses lexical normalization + `strip_prefix(worktree_root)` (`crates/sley-worktree/src/checkout.rs:1248–1257`).
- Submodule URL validation ports CVE-2020-11008 checks (`crates/sley-submodule/src/config.rs:422–460`).

### Network protocol hardening

- Pkt-line max length 65,520 bytes (`crates/sley-protocol/src/lib.rs:138–140`); sideband encoding validates payload bounds (`lib.rs:1214–1216`).
- Protocol v2 tokens reject delimiter bytes (`lib.rs:7523–7535`).
- SSH: `Command::new(program).args(...)` — no shell for remote command; repository path shell-quoted (`crates/sley-transport/src/lib.rs:622–629`, `1342+`); host/user validated for delimiter bytes (`lib.rs:1259–1338`).
- `GIT_SSH_COMMAND` parsed with quote-aware splitter, not naive split (`crates/sley-remote/src/ssh.rs:341–382`).
- HTTP smart URLs exclude userinfo from wire URLs (`crates/sley-transport/src/lib.rs:575–576`).

### Pack / inflate DoS mitigation (partial)

- `bounded_inflate_reserve` caps attacker-controlled size hints (`crates/sley-pack/src/lib.rs:4163–4166`, `MAX_INFLATE_RESERVE = 64 MiB` at `lib.rs:4154`).
- Regression tests for delta result-size bombs (`lib.rs:7306+`).
- Fetch streams packs to disk via `install_raw_pack_from_reader` rather than buffering entire pack in RAM (`crates/sley-odb/src/lib.rs:6129–6138`).

### Credential handling

- Credential parse/encode validates delimiter bytes in keys and values (`crates/sley-transport/src/lib.rs:824–870`).
- HTTP auth built separately from URL userinfo; Basic auth components validated (`lib.rs:542–560`).
- Trace2 redacts URL credentials by default (`crates/sley-core/src/lib.rs:422–462`).

### Dependency security

- `deny.toml`: denies known advisories, yanked crates, unknown registries/git sources; permissive license allowlist verified 2026-06-10.
- `.cargo/audit.toml`: empty ignore list; informational warnings escalated.
- CI consumes both via `audit.yml` (referenced in `deny.toml:3–4`).

### Config trust boundaries

- External config includes refused unless inside repository (`crates/sley/src/config_edit.rs:874–878`, `888–893`).

## Recommended Actions (prioritized)

1. **Add fetch/pack size limits** on `install_raw_pack_from_reader` (H1) — highest operational impact for untrusted remotes.
2. **Unify inflate allocation bounds** across `sley-pack`, `sley-diff-merge::git_patch_delta`, and `plumbing::inflate_zlib_exact` (M1, M2).
3. **Redact credentials from all user-facing URL output** — FETCH_HEAD, prune, push errors (M3); reuse trace2 redaction helper.
4. **Cap credential-helper response size** (M4).
5. **Document inherited shell surfaces** (credential.helper, hooks, filters) in a security/threat-model section (L1–L3).
6. **Document TLS feature selection** for enterprise deployments (L6).
7. Keep `cargo deny check all` and `cargo audit` in CI; revisit `deny.toml` license list when dependencies change.

---

*Review conducted 2026-07-05. Evidence citations use `file:line` format from workspace at commit `250e1f54`.*