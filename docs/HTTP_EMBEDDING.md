# HTTP embedding without a default client

Weft and other hosted importers can supply their own `HttpClient` without linking
`ureq` or selecting local worktree, hooks, or history-editing features. The APIs
are synchronous: a Reqwest adapter should use blocking I/O on a host-owned
blocking worker, with the host's network policy and timeout configuration.

| Crate / features (defaults disabled) | Available surface |
| --- | --- |
| `sley-transport` / none | `HttpClient`, `HttpResponse`, codecs, `TransportPolicy` |
| `sley-transport` / `http-client` | Also `UreqHttpClient`; TLS is selected separately |
| `sley-remote` / none | Transport traits, pack installation, non-HTTP orchestration |
| `sley-remote` / `http` | Injected-client fetch, clone, ls-remote, push, bundle/packfile URI helpers |
| `sley` / `remote` | The same HTTP surface through `sley::remote`, plus repository facade methods |
| `sley-remote` or `sley` / `default-http-client` | Built-in client and `HttpOperationBatch`, `new_http_client`, `new_http_client_with_config`, `prefetch_advertised_bundle_uris` |
| `sley-remote` or `sley` / a `tls-*` feature | Built-in client with the selected HTTPS backend |
| `sley-remote` or `sley` / `worktree` | Clone checkout and receive-pack `updateInstead` support |

`sley-remote` defaults retain the built-in client, rustls, SSH and worktree
support. `sley` defaults remain `mmap,fast-sha1`; `full` enables all facade
features and rustls. Cargo features are additive: another dependency enabling a
built-in-client or worktree feature will include it in the resolved build.

For the 0.11 release:

```toml
[dependencies]
sley = { version = "=0.11.0", default-features = false, features = ["remote"] }
```

Or depend directly on `sley-remote` with `default-features = false` and
`features = ["http"]`.

Implement `sley::remote::HttpClient` (also exported by `sley_remote` and
`sley_transport`), then pass `Some(&adapter)` to
`Repository::fetch_with_http_client_and_cancel` or
`sley_remote::fetch_with_http_client`. The existing `Option<&dyn HttpClient>`
signatures are preserved: `None` uses the built-in client when available, or
returns `GitError::Unsupported` for HTTP operations when it is disabled.
Non-HTTP operations can still use the ordinary entry points. Pack installation
and protocol codecs do not require either HTTP feature.

The adapter contract remains:

- Return HTTP statuses, including 401/403/404/5xx, in `HttpResponse`; transport
  failures are errors. Sley handles credential retries through the supplied
  `CredentialProvider`.
- Return a streaming `Box<dyn Read + Send>` response body. Override `post_reader`
  for streamed uploads; its compatibility fallback buffers within
  `HttpClient::limits().http_request_body()`. `http.postBuffer` still determines
  when Sley selects the streaming request method.
- Own DNS, connection establishment, TLS, proxy and redirect policy for every
  request, including packfile and bundle URI downloads. Sley's orchestration
  continues checking `TransportPolicy` and `protocol.*.allow` before dispatch;
  a custom adapter must enforce its policy on redirects and resolved addresses.
- Pass cancellation through `FetchServices::cancel` or the facade's cancel
  argument. Sley checks it during streamed pack reads and installation. Blocking
  socket reads still require adapter-owned deadlines or teardown to wake up.

A hosted importer can initialize a bare repository and fetch into its refs.
Clone with `checkout = false` also works without the worktree engine. A clone
request for checkout is rejected before creating the destination when
`worktree` is disabled. Receive-pack `updateInstead` reports unsupported rather
than applying an unperformed checkout. Read-only linked-worktree/rebase branch
protection remains active through `sley-formats`; the former `sley-worktree`
exports remain available.

The minimal dependency graph still includes revision walking, index formats,
diff primitives, and receive-pack plumbing shared by the orchestration crate.
It excludes the worktree, hook, and sequencer engines. This change does not add
a Reqwest dependency or modify Weft's adapter.

`bash scripts/check-http-embedding.sh` checks the isolated dependency graphs,
feature builds, and streaming import regressions. Run these separately from a
full workspace build so Cargo feature unification cannot hide missing gates.
