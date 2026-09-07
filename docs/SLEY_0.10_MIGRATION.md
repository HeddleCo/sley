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
