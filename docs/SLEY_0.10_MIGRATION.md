# Sley 0.10 embedding migration (breaking)

This change requires Sley 0.10.0. Workspace versions remain 0.9.0 until the
coordinated release bump. Heddle and Weft are not repinned or edited here.

## Speculative APIs removed (rank 3)

| Removed public API | Replacement and migration |
| --- | --- |
| `sley::StatusCacheKey`, `StatusCacheKey::{new,as_str}`, its `From<String>` / `From<&str>` implementations | Delete the key. No status execution ever consulted it. |
| `StatusPlanBuilder::reuse_index_cache`, `StatusPlan::cache_key` | Remove the builder call/accessor; use `Repository::status_plan()` and its existing `build`, `stream`, `count`, or `collect` methods. These still execute status. For actual caller-owned stat probes use `IndexStatProbeCache` with the worktree feature. |
| `BlobFetchOptions` and `BlobFetchOptions::{new,from_remote,remote}` | Delete these options; they never selected or executed a fetch. |
| `BlobStore::{read_or_fetch,read_or_fetch_blocking}` | Call `Repository::blobs().read(oid)`. On absence it returns `GitError::NotFound(NotFoundKind::Object { kind: MissingObjectKind::Blob, context: Some(MissingObjectContext::Read), .. })`. Hydrate explicitly, then retry the local read. For local promisor remotes use `sley_remote::hydrate_objects_from_local_promisor_remotes`; for network remotes drive `sley_remote::fetch` with the caller's credentials, progress, cancellation and transport policy. A local read never implicitly does network I/O. |
| `RepositoryCapabilities`, `RepositoryCapabilities::current`, `Repository::capabilities` | Use `Repository::is_shallow()` for shallow state and `Repository::object_format() == ObjectFormat::Sha256` for the hash format. Call notes, index, config, and tag operations directly; the old support fields were constants. |
| `sley_remote::TransportCapabilities`, its `current`, `supports_native_push`, `supports_shallow` methods, `Repository::transport_capabilities`, `RemoteContext::transport_capabilities` | Select Cargo features explicitly and handle the operation's typed error. `RemoteContext::{fetch_transport_kind,push_transport_kind}` and `sley_remote::RemoteTransportKind` remain available for URL classification. Wire-negotiated capabilities and `HttpReceivePackObservation` remain intact. |

`ReachablePackPlan`, `PreparedReachablePack`, the `ObjectReader` push seam and the
single-use `HttpReceivePackObservation` are retained. They implement live behavior.

## Facade and Cargo features (rank 2)

The default is now `mmap,fast-sha1`. `remote` is no longer enabled by default.
A consumer that needs the complete previous facade can use:

```toml
sley = { version = "=0.10.0", features = ["full"] }
```

Choose narrower features when possible. The `tls-rustls`, `tls-native-tls`, and
`tls-platform-verifier` choices enable `remote`. `fetch-profile` forwards remote
instrumentation only when the remote dependency is already enabled:

| Feature | Public symbols enabled |
| --- | --- |
| `worktree` | `Repository::{short_status,stream_short_status,short_status_with_options,stream_short_status_with_options,short_status_count_with_options,worktree_entry_state,status_plan,open_index,index_from_tree,read_index,index_stat_probes,write_index,write_index_with_result}`; `StatusPlan`, `StatusPlanBuilder`, `StatusCode`, `StatusRow`, `OwnedStatusRow`; `IndexError`, `IndexWriteError`, `IndexWriteOptions`, `IndexWriteResult`, `IndexStatProbe`, `IndexStatProbeCache`; `ShortStatusEntry`, `ShortStatusOptions`, `ShortStatusRow`, `StatusIgnoredMode`, `StatusUntrackedMode`, `SubmoduleStatus`, `WorktreeEntryState`; `AtomicMetadataWriteOptions`, `AtomicMetadataWriteResult`, `write_metadata_file_atomic`. |
| `history-editing` | `TagCreate`, `Repository::write_annotated_tag`, `notes`, `Repository::{notes_ref,list_notes,iter_notes,read_note_for,read_note,read_note_bytes,write_notes}`. Includes `worktree` because the existing sequencing engine performs real worktree operations. |
| `rendering` | `pretty`, `grep`, `diff_format`. |
| `hooks` | `hooks`, `HookEnvironment`, `HookRun`, `KNOWN_HOOKS`, `cmd_hook`, `hook_exists`, `run_hook`, `run_hook_l`, `run_post_index_change_hook`, `run_reference_transaction_hook_at`, `run_traditional_hook_at`. Includes `worktree`. |
| `remote` | `remote`, its root operation exports, `OperationContext`, `clone_repository`, and the repository remote methods. Includes `worktree,hooks` for live clone checkout, checked-out-branch protection and receive-pack hook behavior. |
| `full` | All the above. The compatibility CLI explicitly enables this feature. |

The `sley::plumbing` module is removed entirely. Each old
`sley::plumbing::sley_<engine>::Symbol` import becomes
`sley_<engine>::Symbol`, with an explicit dependency on `sley-<engine> = "=0.10.0"`.
This applies to `config`, `core`, `diff-merge`, `formats`, `grep`, `hooks`, `index`,
`notes`, `object`, `odb`, `pack`, `pretty`, `protocol`, `refs`, `remote`, `rev`,
`sequencer`, and `worktree`. The exceptional old
`sley::plumbing::format` path becomes `sley_diff_merge::format` (or
`sley::diff_format` with `rendering`). Prefer the focused root types and
`sley::{pack,protocol}` when they cover the operation. `ObjectReader` and
`ObjectWriter` are now also exported at the root.

Repository discovery lives at `sley_formats::discovery`, and worktree layout
resolution at `sley_formats::worktree_root_for_git_dir`. Their existing
`sley_worktree` re-exports remain available. Opening, discovering, reading and
writing a bare repository no longer requires the worktree engine. No crate was
merged or removed; the CLI and all oracle scripts remain enrolled. The mandatory
normal-dependency closure falls from 26 to 16 Sley nodes, counting the facade
itself (manifest traversal excluding optional and development edges).

## Repository and operation policy (rank 1)

The libraries no longer read `GIT_NAMESPACE`, `GIT_ALLOW_PROTOCOL`, or
`GIT_PROTOCOL_FROM_USER`. Configuration reads no longer activate Unicode policy,
and there is no process-wide original working directory. The CLI captures its
transport/namespace environment and original CWD once at its entry point, applies
`--namespace`, and supplies those values to its operations.

| Removed API / behavior | Explicit replacement |
| --- | --- |
| `set_git_namespace_override`, `clear_git_namespace_override`, `get_git_namespace`, `strip_namespace`, `expand_namespace`, `namespace_active` | Construct `sley_core::Namespace::new(raw)` and use `Namespace::{prefix,strip,expand,is_active}`. `Namespace::default()` means the unnamespaced repository. Nested names still expand through `refs/namespaces/` for each component. Put the value in `sley_remote::RemotePolicy::namespace`. |
| `set_precompose_unicode`, `activate_precompose_unicode`, `precompose_unicode_enabled` | Use `sley_core::PrecomposeUnicode::new(enabled)` or `GitConfig::precompose_unicode()`. Test `PrecomposeUnicode::is_enabled()`. Each configuration snapshot owns its value; copy it into workers. |
| `precompose_string_if_needed`, `precompose_bytes_if_needed`, `precompose_owned_string_if_needed`, `precompose_path_if_needed`, `precompose_os_str_bytes_if_needed`, `precompose_argv_if_needed` | Call `PrecomposeUnicode::{string,bytes,owned_string,path,os_str_bytes,argv}` on that value, respectively. `has_non_ascii` remains a pure predicate. |
| `set_original_cwd`, `original_cwd` | Capture the caller's absolute directory outside the library. Mutating entry points take `original_cwd: Option<&Path>`; use `Some(path)` to preserve it during directory pruning and D/F replacement. `None` explicitly selects no protected caller directory. `ReadTreeWorktree` now owns an `original_cwd: Option<PathBuf>` field. |
| `check_transport_allowed(scheme, config, from_user)` and `is_transport_allowed(scheme, config, from_user)` | The third argument is now `&sley_remote::TransportPolicy` (also exported by `sley_transport`). `allow_protocols: None` uses config/default rules; `Some(vec![])` denies every protocol; a nonempty list overrides config. `from_user: bool` determines whether `allow=user` permits the operation. Neither function reads environment. |
| Ambient policy in `UreqHttpClient` | Bind the snapshot using `UreqHttpClient::with_protocol_policy(transport_policy, config)`. Initial requests use `transport_policy.from_user`; redirects retain the same allow-list/config and use `from_user=false`. A custom `HttpClient` owns its redirect policy. |

`RemotePolicy` is a cloneable value with `namespace: Namespace` and
`transport: TransportPolicy`. `FetchOptions`, `PushOptions`, and `CloneOptions`
now have a required `policy: RemotePolicy` field (their defaults supply the
default policy). Add `policy: &RemotePolicy` to `LsRemoteRequest`,
`HttpReceivePackObservationRequest`, and `ReceivePackServerRequest` literals. An observed HTTP push still consumes its observation
exactly once.

For example, initialize the policy once for a tenant and clone it into options:

```rust
use sley_core::Namespace;
use sley_remote::{FetchOptions, RemotePolicy, TransportPolicy};

let policy = RemotePolicy {
    namespace: Namespace::new("tenant/project"),
    transport: TransportPolicy {
        allow_protocols: Some(vec!["https".into(), "ssh".into()]),
        from_user: false,
    },
};
let options = FetchOptions { policy, ..FetchOptions::default() };
```

The following signature map lists the changed public engine entry points. Here
`policy` means `&RemotePolicy`, except that `HttpOperationBatch::with_config`,
`new_http_client_with_config`, and `prefetch_advertised_bundle_uris` take
`&TransportPolicy`. `precompose` means `PrecomposeUnicode` and `original_cwd`
means `Option<&Path>`. Repository-aware worktree operations derive Unicode policy
from their own config; APIs without a repository/config argument require the
explicit value. `prefetch_advertised_bundle_uris` additionally takes
`config: Option<&GitConfig>` immediately after its transport policy.

The following take `original_cwd` before their existing arguments (after `&self` on methods):

- `sley::Repository::push`
- `sley::Repository::push_actions`
- `sley::Repository::push_actions_with_cancel`
- `sley::Repository::push_actions_with_http_client`
- `sley::Repository::push_actions_with_http_client_and_cancel`
- `sley::Repository::push_with_cancel`
- `sley::clone_repository`
- `sley_remote::clone`
- `sley_remote::clone_with_http_client`
- `sley_remote::execute_push_action_plan`
- `sley_remote::execute_push_plan`
- `sley_remote::push`
- `sley_remote::push_actions`
- `sley_remote::push_actions_with_http_client`
- `sley_remote::receive_pack_into_local_repository`
- `sley_remote::receive_pack_reachable_pack_into_local_repository`
- `sley_remote::receive_pack_stream_into_local_repository`
- `sley_remote::serve_receive_pack`
- `sley_remote::update_worktree_for_update_instead`
- `sley_sequencer::am::am_abort`
- `sley_sequencer::am::am_continue`
- `sley_sequencer::am::am_continue_allow_empty`
- `sley_sequencer::am::am_retry`
- `sley_sequencer::am::am_skip`
- `sley_sequencer::am::rebase_apply_abort`
- `sley_sequencer::am::rebase_apply_continue`
- `sley_sequencer::am::rebase_apply_skip`
- `sley_sequencer::am::start_am`
- `sley_sequencer::am::start_rebase_apply`
- `sley_sequencer::apply::merge_refuse_if_current_working_directory_becomes_file`
- `sley_sequencer::apply::merge_remove_worktree_file`
- `sley_sequencer::apply::merge_write_worktree_file`
- `sley_sequencer::pick::continue_sequence`
- `sley_sequencer::pick::pick_revisions`
- `sley_sequencer::pick::reset_merge_in`
- `sley_sequencer::pick::rollback`
- `sley_sequencer::pick::skip_sequence`
- `sley_sequencer::rebase_drive::complete_action`
- `sley_sequencer::rebase_drive::create_autostash`
- `sley_sequencer::rebase_drive::pick_commits`
- `sley_sequencer::rebase_drive::rebase_abort`
- `sley_sequencer::rebase_drive::rebase_continue`
- `sley_sequencer::rebase_drive::rebase_skip`
- `sley_sequencer::rebase_drive::reset_index_and_worktree_to_commit_for_rebase`
- `sley_worktree::apply_sparse_checkout`
- `sley_worktree::apply_sparse_checkout_with_mode`
- `sley_worktree::checkout_branch`
- `sley_worktree::checkout_branch_filtered`
- `sley_worktree::checkout_commit_to_index_and_worktree_sparse`
- `sley_worktree::checkout_detached`
- `sley_worktree::checkout_detached_filtered`
- `sley_worktree::checkout_detached_sparse`
- `sley_worktree::checkout_index_paths`
- `sley_worktree::checkout_index_paths_with_database`
- `sley_worktree::checkout_index_paths_with_database_outcome`
- `sley_worktree::checkout_index_paths_with_database_outcome_sparse`
- `sley_worktree::checkout_tree_to_index_and_worktree`
- `sley_worktree::checkout_two_way_engine`
- `sley_worktree::materialize_checkout_entries_with_database`
- `sley_worktree::move_index_and_worktree_path`
- `sley_worktree::prune_empty_dirs`
- `sley_worktree::reapply_active_sparse_checkout`
- `sley_worktree::refuse_if_unpack_entries_turn_cwd_into_file`
- `sley_worktree::refuse_if_unpack_result_removes_current_directory`
- `sley_worktree::remove_index_and_worktree_paths`
- `sley_worktree::remove_path_in_the_way`
- `sley_worktree::remove_worktree_path`
- `sley_worktree::reset_index_and_worktree_to_commit`
- `sley_worktree::reset_index_and_worktree_to_commit_with_process_filter_metadata`
- `sley_worktree::restore_index_and_worktree_paths_from_head`
- `sley_worktree::restore_index_and_worktree_paths_from_tree`
- `sley_worktree::restore_worktree_paths`
- `sley_worktree::restore_worktree_paths_filtered`
- `sley_worktree::restore_worktree_paths_from_head`
- `sley_worktree::restore_worktree_paths_from_tree`
- `sley_worktree::write_tree_entry_to_worktree`
- `sley_worktree::write_tree_entry_to_worktree_with_hooks`

The following take `original_cwd`, then `policy` before their existing arguments (after `&self` on methods):

- `sley_remote::push_local_with_report`
- `sley_remote::push_local_with_report_and_objects`

The following take `policy` before their existing arguments (after `&self` on methods):

- `sley::Repository::ls_remote`
- `sley::Repository::ls_remote_with_http_client`
- `sley_remote::HttpOperationBatch::with_config`
- `sley_remote::hydrate_objects_from_local_promisor_remotes`
- `sley_remote::hydrate_reachable_from_local_promisor_remotes`
- `sley_remote::install_fetch_pack_via_git_upload_pack`
- `sley_remote::install_fetch_pack_via_http_protocol_v2_fetch`
- `sley_remote::install_fetch_pack_via_http_protocol_v2_fetch_with_want_refs`
- `sley_remote::install_fetch_pack_via_http_upload_pack`
- `sley_remote::install_fetch_pack_via_local_upload_pack`
- `sley_remote::install_fetch_pack_via_local_upload_pack_with_promisor_decision`
- `sley_remote::install_fetch_pack_via_ssh_upload_pack`
- `sley_remote::local_fetch_advertisements`
- `sley_remote::local_have_oids`
- `sley_remote::local_protocol_v2_ls_refs_advertisements`
- `sley_remote::ls_remote`
- `sley_remote::mark_complete_local_refs`
- `sley_remote::negotiate_only_local`
- `sley_remote::new_http_client_with_config`
- `sley_remote::prefetch_advertised_bundle_uris`
- `sley_remote::prefetch_diff_entry_blobs`
- `sley_remote::prefetch_promisor_objects`
- `sley_remote::read_object_maybe_prefetch_promisor`
- `sley_remote::serve_upload_pack_v2`
- `sley_remote::serve_upload_pack_v2_stateless_with_config`
- `sley_remote::serve_upload_pack_v2_with_config`
- `sley_remote::stage_local_push_quarantine`
- `sley_remote::upload_pack_features`

The following take `precompose` before their existing arguments (after `&self` on methods):

- `sley_archive::ArchiveConvert<'a>::from_worktree`
- `sley_worktree::StandardAttributeMatcher::from_worktree_root`
- `sley_worktree::ignored_index_entries`
- `sley_worktree::path_matches_ignore`
- `sley_worktree::path_matches_ignore_with_per_directory`
- `sley_worktree::path_matches_standard_ignore`
- `sley_worktree::standard_attributes_for_path`
- `sley_worktree::standard_ignore_match`


## Library errors and diagnostics (rank 4)

| Removed API / behavior | Replacement and migration |
| --- | --- |
| `GitError::Io(String)` | Use `GitError::from(std::io::Error)` for an actual I/O failure, preserving its `ErrorKind`. For a manually described failure construct `GitError::IoKind { kind, message }` with the meaningful kind (`Other` only when no more specific kind applies). Classify with `GitError::io_kind()`, not rendered text. The `io error: …` rendering remains unchanged. |
| `GitError::{Exit,Cli}` | Library operations return `GitError::Rejected(RejectionKind::{InvalidArguments,Refused,Incomplete})` after sending details to the operation's diagnostic sink. These express validation, refusal, and incomplete-operation semantics, without requesting process termination. Continue to inspect `FetchOutcome`, `PushOutcome`, `CloneOutcome` and their existing typed dispositions for ordinary operation results. |
| `sley_core::CliExit`, `sley_core::cli_exit_code`, `GitError::{usage,user_error,cli_exit,cli_exit_code}` | Process status belongs to the executable adapter. The compatibility layer exports `sley_cli::{CliExit,cli_exit,cli_usage,cli_user_error,cli_diagnostic,cli_exit_code,cli_message,cli_reported_status}`. Embedders should map typed results to their own application errors, not depend on CLI status helpers. |
| Numeric library exit codes for child failures, aborted remote helpers, and empty preferred packs | Match `GitError::ChildProcessFailed { status: Option<i32> }`, `GitError::RemoteHelperAborted { name }`, or `GitError::EmptyPreferredPack { path }`, respectively. Child status describes the actual subprocess; it is not a request to exit the host. |
| Direct library `print!` / `println!` / `eprint!` / `eprintln!` diagnostics and human-facing byte writes | Supply `sley_core::diagnostics::Diagnostics::new(sink)` and execute the synchronous operation inside `diagnostics.scope(|| operation())`. Implement `DiagnosticSink::{write,flush}`; `DiagnosticStream::{Stdout,Stderr}` identifies the original channel, and bytes retain their original newlines/terminators. Default scopes discard diagnostics. This covers ODB corrupt-copy fallback, revision/config validation, remote warnings, worktree operations, and sequencing/maintenance output. |

Caller-provided services can preserve their concrete errors using
`GitError::Callback(CallbackError::new(error))`. `CallbackError::downcast_ref`
and the standard `Error::source` chain retain the original type. Clones share
callback-error identity; separately constructed callbacks do not compare equal
merely because their messages match. The CLI uses this boundary to carry its
private command outcome through library-invoked callbacks. No library understands
that private outcome or calls `process::exit`.

Diagnostics use a thread-local routing scope, **not a process-wide sink setter**.
A scope owns an `Arc` of the caller's sink, restores the previous scope on return
or unwind, and allows nested callers to select different sinks. Library workers
capture the current sink with `diagnostics::inherit`; host-created workers must
do the same, or explicitly call their own `Diagnostics::scope`. Sinks must accept
concurrent calls. `DiagnosticWriter::new(stream)` provides a fallible `Write`
adapter bound to the current sink. Best-effort diagnostic macros do not replace
an operation's error if the renderer fails; explicit fallible output paths still
propagate renderer I/O failures.

All affected engines are synchronous. Async hosts must enter the scope **inside**
the blocking operation; wrapping future creation does not route its later polls.
Do not hold a diagnostic scope across an await. Per-task async routing is deferred
until there is an asynchronous engine execution API that needs it.

For example, a host logger can supply a sink without acquiring process stdout or
stderr:

```rust
use sley_core::diagnostics::{DiagnosticSink, DiagnosticStream, Diagnostics};

struct HostLog;
impl DiagnosticSink for HostLog {
    fn write(&self, stream: DiagnosticStream, bytes: &[u8]) -> std::io::Result<()> {
        // Forward `(stream, bytes)` to the host's logger or retained report.
        let _ = (stream, bytes);
        Ok(())
    }
}
let diagnostics = Diagnostics::new(HostLog);
let result = diagnostics.scope(|| sley::Repository::open("repository.git"));
```

`BadNumericValue::report`, `BadBooleanValue::report`, `BadPathValue::report`, and
`MissingValueError::report` in `sley_config::typed` now use the sink and return
`Rejected(Refused)`. Their `diagnostic()` accessors remain available when the host
wants to render those typed validation errors itself. Existing explicit progress,
warning, event, and `Write` parameters retain their contracts. Protocol bodies,
credential helper records, configured trace destinations, and subprocess stdio
are separate compatibility channels, not implicitly captured diagnostics.

Heddle's credential classification must replace its `GitError::Io` match with
`IoKind` / `io_kind()` and retain typed cancellation and sideband-fatal handling.
Weft's `weft-base/src/ssrf.rs` constructors must supply an I/O kind, or preserve a
host-specific error through `CallbackError`; custom HTTP/authentication behavior
is unchanged. Exhaustive `GitError` matches need the new variants. Neither consumer
was repinned or built by this change; adopting 0.10 requires their own compilation
and error-path tests.


## Removal inventory and retained historical references

A whole-repository source/path sweep accompanies compilation. Each group below
has no live Rust caller or workflow/gate invocation remaining:

- `StatusCacheKey`, its conversions/accessors, `StatusPlanBuilder::reuse_index_cache`,
  and `StatusPlan::cache_key`.
- `BlobFetchOptions`, `BlobStore::read_or_fetch`, and `read_or_fetch_blocking`.
- `RepositoryCapabilities` (including deleted `crates/sley/src/capabilities.rs`),
  `TransportCapabilities`, and both `transport_capabilities` accessors.
- The broad `sley::plumbing` re-export module and its consumer imports.
- Namespace, precomposition, and original-CWD global setters/getters and the
  ambient namespace/transport environment reads moved to `sley_cli::session`.
- `GitError::{Io,Exit,Cli}` and the core CLI status constructors/mapping.

The historical architecture audit, July reviews, and completed/pre-alpha plans
retain old names as records of their revisions; the audit points here for the
implemented migration. The old capability file link is pinned to the audited
revision. `scripts/split_remote_cmds.py` is an uninvoked historical extraction
script whose input `remote_cmds.rs` no longer exists; its embedded old exit name
is not a live generator/build path. It is retained within the task's restriction
on editing orchestration scripts. The live parity checklist and API doc links
were updated. No floor, prerequisite, selection, comparison rule, or test was
removed to accommodate these deletions.

The [finished verification record](program/SLEY_0.10_VERIFICATION.md) includes
the full workspace run, negative isolation controls, exact namespace/Unicode
oracle results, unchanged-baseline TAP comparison, and parity-floor evidence.
