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

Choose narrower features when possible:

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
merged or removed; the CLI and all oracle scripts remain enrolled.
