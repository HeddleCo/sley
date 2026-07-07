# Extraction plan: `sley-cli` network orchestration → `sley-remote`

Guide for lifting fetch/push/clone/ls-remote out of the `sley-cli` monolith
(`crates/sley-cli/src/lib.rs`, ~34k lines) into a callable `sley-remote` library
crate. Heddle's biggest blocker; first extraction of the decomposition (#19).

## Key facts

- All wire codecs (`sley-protocol`), the v2 pack encoder (`sley-pack`), pack build
  + reachability (`sley-odb`), ref-update/push-command planning (`sley-protocol`),
  and report-status parsing are **already public library APIs**. The lift is
  orchestration glue, not algorithms.
- All transport orchestration is in `sley-cli/src/lib.rs` (none in `commands/*`).
- **Shallow/deepen is NOT implemented**: `UploadPackRequest`/`Features` support
  `deepen`/`shallow` (`sley-protocol/src/lib.rs:1485`, `:1247`) with a full v2
  parser, but every fetch builder uses `{ wants, ..default() }`
  (`sley-cli/src/lib.rs:9845`, `:9394`, `:9338`) and `cmd_clone` validates then
  discards `--depth` (`:2207`). Wiring it is new behavior in this slice.

## What moves vs. stays

**(A) Already in libs — call from `sley-remote`:** sley-protocol (UploadPack/
ReceivePack types + codecs, `plan_fetch_ref_updates`, `plan_push_commands`,
`build_receive_pack_push_request`), sley-transport (`RemoteUrl`, `parse_remote_url`,
`GitCredential`, `HttpClient`/`UreqHttpClient`, `http_smart_*_url`,
`ssh_process_command`), sley-fetch (`install_upload_pack_raw_response`), sley-odb
(`build_reachable_pack`, `collect_reachable_object_ids`, `install_raw_pack`),
sley-refs, sley-config, sley-rev, sley-core.

**(B) sley-cli-internal pure logic — MOVE into `sley-remote`** (all take explicit
args; no globals; strip trailing `eprintln!`):
- HTTP: `fetch_http_repository` (:9859), `push_http_repository` (:9950),
  `clone_http_repository` (:2928), `ls_remote_http_records` (:10076),
  `http_service_advertisements` (:9764), `http_upload_pack_advertisements`
  (:9780), `http_upload_pack_fetch_response` (:9796),
  `install_fetch_pack_via_http_upload_pack` (:9823), `http_send_with_auth`
  (:9685), `http_authorization_headers`/`http_check_status`/
  `http_validate_content_type`/`http_advertised_refs` (:9714-:9758),
  `new_http_client` (:9525), http credential-key builders (:9529-:9557).
- SSH: `fetch_ssh_repository` (:9228), `push_ssh_repository` (:8704),
  `ssh_upload_pack_advertisements` (:9408), `ssh_upload_pack_fetch_response`
  (:9448), `install_fetch_pack_via_ssh_upload_pack` (:9373), `ls_remote_ssh_records`
  (:10813), `ssh_program` (:10891).
- Local/file:// server: `fetch_local_repository` (:9123), `push_local_repository`
  (:8621), `install_fetch_pack_via_local_upload_pack` (:9316),
  `upload_pack_from_local_repository` (:8450), `receive_pack_into_local_repository`
  (:8878), `local_fetch_advertisements` (:10464), `local_have_oids` (:10136),
  upload/receive-pack capability helpers (:8394-:8432, :8311-:8911).
- Credential subprocess: `credential_fill` (:9646), `credential_store` (:9672),
  `credential_helper_specs` (:9568), `credential_helper_command` (:9593),
  `run_credential_helper` (:9624).
- Push planning: `validate_receive_pack_report` (:8862), `reject_non_fast_forward_pushes`
  (:9005), `local_push_source_refs` (:8923), `normalize_push_refspec/refname`
  (:8974/:8993), `remote_advertisement_tips_known_to_local` (:8845).
- Fetch ref-map / FETCH_HEAD: `fetch_refspecs_for_source` (:10220),
  `apply_configured_*` (:10147/:10172), auto-follow-tags (:10246/:10265),
  `write_*fetch_head*` (:10187/:11694/:10202), prune helpers (:10345-:10391).
- Sniffers + URL utils: `*_is_http`/`*_is_ssh`, `percent_decode_url_path`
  (:10956), `read_repo_config` (:13627), `rewrite_url_with_config` (:13689).
- Bundle fetch: `fetch_bundle` (:9068) + helpers.

**(C) CLI-coupled — must NOT move:** raw `eprintln!`/`println!` (push "To …"
tails :8696/:8836/:8067, `print_ls_remote_ref` :11218, clone "Cloning into…/done."
:2685/:2719, all `--depth/--filter` warnings, prune lines); `GitError::Exit(n)`
mapping (`main.rs`); arg parsing (`cmd_*`, `parse_ls_remote_*`, `GitArgCursor`);
**global mutable state** `GLOBAL_GIT_DIR`/`GLOBAL_BARE`/etc. (:61) and
`discover_git_dir` (:34028) which reads them; **`env::current_dir()`/
`set_current_dir()`** coupling in `ls_remote_resolved_url` (:10895),
`push_resolved_url` (:8581), and the clone chdir hack (:2759/:3003).

## Seams to decouple from the CLI

1. **`CredentialProvider` trait** — replaces direct `credential_fill`/`store`
   calls; default impl = the moved subprocess helper; `NoCredentials` no-op
   (what heddle uses). Anticipate interactive askpass (not present today).
2. **`ProgressSink` trait** — replaces the trailing `eprintln!`/`println!` and
   the `&mut impl Write` prune sink; `sley-remote` returns structured
   `FetchOutcome`/`PushOutcome`, sley-cli formats them.
3. **Hoist repo/URL resolution into sley-cli** — `sley-remote` takes `git_dir`,
   `common_git_dir`, `format`, `&GitConfig`, and the already-resolved URL as
   params. Kills the `set_current_dir` clone hack and the global-state reads.
4. **In-process local server** — expose reader/writer `serve_upload_pack`/
   `serve_receive_pack`; file:// fetch/push pipe in-memory; `cmd_upload_pack`/
   `cmd_receive_pack` become thin stdio wrappers.

## Proposed public API (sketch)

```
RemoteContext { git_dir, common_git_dir, format, config: &GitConfig, resolved_url }
fetch(ctx, refspecs, FetchOptions, &mut dyn CredentialProvider, &mut dyn ProgressSink) -> FetchOutcome
push(ctx, refspecs, PushOptions, cred, progress) -> PushOutcome
clone(ClonePlan, cred, progress) -> CloneOutcome
ls_remote(ctx, LsRemoteFilter) -> (Vec<LsRemoteRecord>, ObjectFormat)   // caller sorts/prints
serve_upload_pack(git_dir, format, impl Read, impl Write)
serve_receive_pack(git_dir, format, impl Read, impl Write)
```

## Staged execution

- **A. Scaffold** — crate + seam traits (`CredentialProvider`/`NoCredentials`,
  `ProgressSink`/`SilentProgress`). ← current
- **B. Credential subsystem** — move credential_* behind the default provider.
- **C. Local server** — `serve_upload_pack`/`serve_receive_pack` + helpers;
  rewire `cmd_upload_pack`/`cmd_receive_pack` + local fetch/push.
- **D. HTTP fetch + clone** (heddle's primary) — http plumbing + ref-map +
  FETCH_HEAD; rewire `cmd_fetch`/`cmd_clone` http branches.
- **E. HTTP push** — push planning + report-status; rewire `cmd_push`.
- **F. SSH + ls-remote** — complete the lift.
- **G. Shallow/deepen** — NEW: thread depth into `UploadPackRequest`, persist
  `$GIT_DIR/shallow`. Needed by heddle.

Verify after each stage: `cargo test --workspace` (the existing fetch/push/clone/
ls-remote interop suites must stay green); commit per stage.

## Risks

Global state + `discover_git_dir` (biggest decouple); clone `set_current_dir`
hack; large fns mixing orchestration+output (3 fetch fns have subtle divergences
— don't over-merge); ssh program selection ignores `GIT_SSH_COMMAND`/
`core.sshCommand`; `GitError::Exit` is a CLI concept (return typed outcomes);
HTTP v2 unsupported (`http_advertised_refs` :9758 errors on v2) — encode as
explicit `Unsupported`.
