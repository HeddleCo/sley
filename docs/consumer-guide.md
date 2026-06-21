# Consumer Guide

This guide is for applications that embed sley as a library. Prefer the
`sley` facade crate for application code, then drop to `sley::plumbing::*` only
when the facade does not yet wrap the operation you need.

```toml
[dependencies]
sley = { path = "../sley/crates/sley" }
```

The facade enables remote support by default. Disable default features only for
small, local-only tools that do not need fetch, push, or HTTP/TLS transport
code.

## Repository Context

Use `sley::RepositoryContext` for command/session work. `Repository` stays the
repository-intrinsic handle; `RepositoryContext` adds cwd, setup policy,
effective config, refs, objects, and revision resolution in one reusable value.

```rust
use std::path::Path;

use sley::{RepositoryContext, RepositorySetup, WorktreePolicy};

fn open_repository_context(path: &Path) -> sley::Result<RepositoryContext> {
    let setup = RepositorySetup::new(path)
        .with_worktree_policy(WorktreePolicy::Any)
        .with_replace_objects(true);
    let ctx = RepositoryContext::discover(&setup)?;

    eprintln!("git dir: {}", ctx.git_dir().display());
    eprintln!("common dir: {}", ctx.common_dir().display());
    eprintln!("object format: {}", ctx.object_format().name());

    let head = ctx.repository().head()?;
    if let Some(branch) = head.branch_name() {
        eprintln!("attached to {branch}");
    }

    let origin = ctx.repository().remote("origin")?;
    eprintln!("fetch URL: {}", origin.fetch_url());
    eprintln!("push URL: {}", origin.push_url());

    Ok(ctx)
}
```

When you already have a git directory, use `RepositorySetup::new(cwd)
.with_git_dir(git_dir)`. For a bare repository, add
`.with_worktree_policy(WorktreePolicy::RequireBare)`.

## Command IO

Command-like engines should accept `sley::CommandIo<'_>` instead of reading or
printing through process globals. The struct borrows `BufRead`/`Write` trait
objects, so callers can pass stdio locks, test buffers, pipes, or network
streams without copying the command payload.

```rust
use std::io::{BufReader, Cursor};

use sley::CommandIo;

fn run_with_buffers(input: &[u8]) -> sley::Result<(Vec<u8>, Vec<u8>)> {
    let mut stdin = BufReader::new(Cursor::new(input));
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();

    {
        let _io = CommandIo::new(&mut stdin, &mut stdout, &mut stderr);
        // pass `_io` to a command engine here
    }

    Ok((stdout, stderr))
}
```

## Cat-File Streaming

For `cat-file --batch-check` behavior, call the public object reader header
method and write each result as it arrives. For `cat-file --batch`, follow with
`copy_object_body_to`.

```rust
use std::io::{BufRead, Write};

use sley::{RecordReader, RepositoryContext};
use sley::plumbing::sley_odb::ObjectReader;

fn stream_cat_file_batch(
    ctx: &RepositoryContext,
    input: impl BufRead,
    mut output: impl Write,
) -> sley::Result<()> {
    let mut reader = RecordReader::new(input, b'\n');
    while let Some(record) = reader.next_record()? {
        let spec = std::str::from_utf8(record)
            .map_err(|err| sley::GitError::InvalidFormat(err.to_string()))?
            .trim();
        if spec.is_empty() {
            continue;
        }

        let oid = ctx.resolve_revision(spec)?;
        match ObjectReader::read_object_header(ctx.objects(), &oid)? {
            Some(header) => {
                writeln!(output, "{oid} {} {}", header.object_type.as_str(), header.size)?;
                ObjectReader::copy_object_body_to(ctx.objects(), &oid, &mut output)?;
                writeln!(output)?;
            }
            None => {
                writeln!(output, "{oid} missing")?;
            }
        }
    }

    Ok(())
}
```

Use `repo.blobs().read_or_fetch_blocking(oid, BlobFetchOptions::from_remote("origin"))`
when a missing blob should be reported as a remote-boundary miss for a future
lazy hydration path.

## Update-Ref Stdin

`sley-refs` exposes a borrowed `update-ref --stdin` parser through
`sley::plumbing::sley_refs::update_ref_stdin`. The facade exposes typed,
atomic ref batches; parse records without copying, then allocate only the ref
updates you choose to apply.

```rust
use std::io::BufRead;

use sley::{
    DeleteRef, FullName, GitError, ObjectId, RefBatchChange, RefChange,
    RefDeleteExpected, ReferenceTarget, Repository,
};
use sley::plumbing::sley_refs::update_ref_stdin::{
    UpdateRefStdinCommand, UpdateRefStdinOid, parse_update_ref_stdin_line,
};

fn parse_oid(repo: &Repository, text: &str) -> sley::Result<ObjectId> {
    ObjectId::from_hex(repo.object_format(), text)
}

fn apply_update_ref_stdin(repo: &Repository, input: impl BufRead) -> sley::Result<()> {
    let mut changes = Vec::new();

    for line in input.split(b'\n') {
        let line = line?;
        if line.is_empty() {
            continue;
        }

        match parse_update_ref_stdin_line(&line)
            .map_err(|err| GitError::InvalidFormat(err.to_string()))?
        {
            UpdateRefStdinCommand::Update { refname, new, old } => {
                let name = FullName::new(refname.as_ref())?;
                let Some(new) = new.as_str() else {
                    return Err(GitError::InvalidFormat("missing new oid".into()));
                };
                let new_oid = parse_oid(repo, new)?;
                let expected = match old {
                    Some(UpdateRefStdinOid::Value(oid)) => {
                        Some(ReferenceTarget::Direct(parse_oid(repo, oid.as_ref())?))
                    }
                    Some(UpdateRefStdinOid::Empty) | None => None,
                };
                changes.push(RefBatchChange::Update(RefChange {
                    name,
                    new: ReferenceTarget::Direct(new_oid),
                    expected,
                    reflog: None,
                }));
            }
            UpdateRefStdinCommand::Delete { refname, old } => {
                let name = FullName::new(refname.as_ref())?;
                let expected_old = match old {
                    Some(UpdateRefStdinOid::Value(oid)) => Some(parse_oid(repo, oid.as_ref())?),
                    Some(UpdateRefStdinOid::Empty) | None => None,
                };
                changes.push(RefBatchChange::Delete(DeleteRef {
                    name,
                    expected_old,
                    expected: expected_old.map(RefDeleteExpected::Direct),
                    reflog: None,
                    reflog_committer: None,
                }));
            }
            command => {
                return Err(GitError::Unsupported(format!(
                    "unsupported update-ref stdin command {:?}",
                    command.verb()
                )));
            }
        }
    }

    repo.apply_ref_batch(&changes)
        .map_err(|err| GitError::Transaction(err.to_string()))
}
```

For porcelain-style movement of the checked-out branch, use
`Repository::update_branch_checked_out_as_head` so the branch and `HEAD` reflogs
stay consistent.

## Diff Render

`Repository::diff_name_status` covers the name-status case. For patch hunk
rendering, use `sley::plumbing::sley_diff_merge::render`. The renderer is
repository-agnostic: callers emit file headers and pass old/new blob bytes for
the hunk body.

```rust
use std::io::Write;

use sley::plumbing::sley_diff_merge::render::{render_hunks, HunkRenderOptions};
use sley::{GitObjectType, ObjectId, Repository};

fn render_blob_patch(
    repo: &Repository,
    path: &str,
    old_oid: ObjectId,
    new_oid: ObjectId,
) -> sley::Result<Vec<u8>> {
    let old = repo.read_object(&old_oid)?;
    let new = repo.read_object(&new_oid)?;

    if old.object_type != GitObjectType::Blob || new.object_type != GitObjectType::Blob {
        return Err(sley::GitError::InvalidObject(
            "diff render example expects blob objects".into(),
        ));
    }

    let old_hex = old_oid.to_hex();
    let new_hex = new_oid.to_hex();
    let abbrev = 12.min(old_hex.len()).min(new_hex.len());

    let mut out = Vec::new();
    writeln!(out, "diff --git a/{path} b/{path}")?;
    writeln!(out, "index {}..{} 100644", &old_hex[..abbrev], &new_hex[..abbrev])?;
    writeln!(out, "--- a/{path}")?;
    writeln!(out, "+++ b/{path}")?;

    let mut options = HunkRenderOptions::default();
    render_hunks(&mut out, Some(&old.body), Some(&new.body), &mut options);
    Ok(out)
}
```

## Fetch And Push

Use `Repository::remote` when you need to inspect URL rewriting or transport
capabilities, then call `Repository::fetch` and `Repository::push` for ordinary
operations. Credentials and progress are caller-provided seams, so library
calls do not prompt or print on their own.

```rust
use sley::remote::{FetchOptions, NoCredentials, PushOptions, SilentProgress};
use sley::Repository;

fn fetch_options() -> FetchOptions {
    FetchOptions {
        quiet: false,
        auto_follow_tags: true,
        fetch_all_tags: false,
        prune: false,
        prune_tags: false,
        dry_run: false,
        append: false,
        write_fetch_head: true,
        tag_option_explicit: false,
        prune_option_explicit: false,
        prune_tags_option_explicit: false,
        refmap: None,
        depth: None,
        merge_srcs: Vec::new(),
        filter: None,
        refetch: false,
        cloning: false,
        record_promisor_refs: true,
        update_shallow: false,
        deepen_relative: false,
        update_head_ok: false,
        deepen_since: None,
        deepen_not: Vec::new(),
        ssh_options: None,
    }
}

fn fetch_and_push(repo: &Repository) -> sley::Result<()> {
    let remote = repo.remote("origin")?;
    eprintln!("transport: {:?}", remote.fetch_transport_kind()?);

    let mut credentials = NoCredentials;
    let mut progress = SilentProgress;

    let fetched = repo.fetch(
        "origin",
        &[],
        fetch_options(),
        &mut credentials,
        &mut progress,
    )?;
    eprintln!("planned {} fetch updates", fetched.ref_updates.len());

    let pushed = repo.push(
        "origin",
        &["HEAD:refs/heads/main".to_string()],
        PushOptions::default(),
        &mut credentials,
        &mut progress,
    )?;
    eprintln!("executed {} push commands", pushed.commands.len());

    Ok(())
}
```

For exact old/new/delete push plans, build a `PushActionPlan` and call
`Repository::push_actions`.

## Status Stream

Use the streaming API for UI surfaces and long-running scans. Return
`StreamControl::Stop` when the caller has enough rows.

```rust
use sley::{
    Repository, ShortStatusOptions, StatusUntrackedMode, StreamControl,
};

fn stream_status(repo: &Repository) -> sley::Result<()> {
    let options = ShortStatusOptions {
        untracked_mode: StatusUntrackedMode::Normal,
        ..ShortStatusOptions::default()
    };

    repo.stream_short_status_with_options(options, |row| {
        println!("{}", row.line());
        Ok(StreamControl::Continue)
    })
}
```

For repeated status calls, keep a caller-owned cache key with
`repo.status_plan().reuse_index_cache("workspace-main").build()?`. The current
facade records the key and leaves room for deeper shared cache storage without
changing consumer call sites.
