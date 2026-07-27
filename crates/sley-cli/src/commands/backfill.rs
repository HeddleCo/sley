//! `git backfill` — batch-download missing blobs in a partial clone.
//!
//! Mirrors upstream `builtin/backfill.c`: walk the revision graph, collect
//! missing blobs (optionally restricted by sparse-checkout / pathspecs), and
//! fetch them from configured promisor remotes in batches.

use std::collections::{BTreeMap, HashSet};
use std::io::{self, BufRead};
use std::path::{Path, PathBuf};

use sley::plumbing::sley_object::{Commit, ObjectType, TreeEntries, tree_entry_object_type};
use sley::plumbing::sley_odb::{FileObjectDatabase, ObjectReader};
use sley::plumbing::sley_rev::{
    RevWalk, RevWalkDateWindow, RevWalkOrder, RevisionOptions, RevisionOrder, RevisionSetupContext,
    setup_revisions,
};
use sley::plumbing::{sley_remote, sley_rev, sley_worktree};
use sley_worktree::{
    SparseCheckout, SparseCheckoutMode, path_in_sparse_checkout, worktree_root_for_git_dir,
};

use crate::promisor_remote_names;
use crate::*;

const USAGE: &str = "usage: git backfill [--min-batch-size=<n>] [--[no-]sparse] [--[no-]include-edges] [<revision-range>]";

const DEFAULT_MIN_BATCH_SIZE: usize = 50_000;

struct BackfillOptions {
    min_batch_size: usize,
    /// `None` = auto from `core.sparseCheckout`.
    sparse: Option<bool>,
    include_edges: bool,
    stdin: bool,
    remaining: Vec<String>,
}

pub(crate) fn cmd_backfill(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let options = parse_backfill_options(args)?;
    let git_dir = cli_session.git_dir()?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let cwd = cli_session.cwd().to_path_buf();
    let config = commands::remote::read_effective_repo_config(&common_git_dir, &cwd)?;
    let db = FileObjectDatabase::from_git_dir(&common_git_dir, format)
        .with_promisor_remote_present(sley_remote::config_has_promisor_remote(&config));

    let mut rev_args = options.remaining.clone();
    if options.stdin {
        for line in io::stdin().lock().lines() {
            let line = line.map_err(|err| GitError::Io(err.to_string()))?;
            let line = line.trim();
            if !line.is_empty() {
                rev_args.push(line.to_string());
            }
        }
    }

    reject_unsupported_options(&rev_args)?;

    // Sparse loading is independent of revision resolution: `backfill --sparse`
    // in an empty/unborn repo must still fail with "problem loading
    // sparse-checkout" (t5620 #8).
    let sparse_enabled = match options.sparse {
        Some(v) => v,
        None => config
            .get_bool("core", None, "sparseCheckout")
            .unwrap_or(false),
    };
    let sparse = if sparse_enabled {
        Some(load_sparse_matcher(&git_dir, &config)?)
    } else {
        None
    };

    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let setup = setup_revisions(
        &rev_args,
        &RevisionSetupContext {
            git_dir: &git_dir,
            worktree_root: worktree_root.as_deref(),
            cwd: &cwd,
            format,
            reader: &db,
            config: Some(&config),
            assume_dashdash: false,
        },
    )
    .map_err(|err| map_setup_error(err, &rev_args))?;

    if let Some(leftover) = setup.leftovers.first() {
        return Err(GitError::Command(format!(
            "unrecognized argument: {leftover}"
        )));
    }

    let revision_options = setup.options;
    let pathspecs = setup.pathspecs;

    // Collect positive tip commits. Default to HEAD when none given.
    let mut starts: Vec<ObjectId> = revision_options
        .positives
        .iter()
        .map(|tip| tip.oid)
        .collect();
    if starts.is_empty() {
        match sley_rev::resolve_revision(&git_dir, format, "HEAD") {
            Ok(oid) => starts.push(oid),
            Err(_) => return Ok(()),
        }
    }

    let mut walker = BackfillWalker {
        db: &db,
        format,
        git_dir: &common_git_dir,
        cwd: &cwd,
        config: &config,
        lazy_fetch: cli_session.lazy_fetch(),
        min_batch_size: options.min_batch_size.max(1),
        batch: Vec::new(),
        seen_blobs: HashSet::new(),
        seen_trees: HashSet::new(),
        visited_paths: HashSet::new(),
        sparse: sparse.as_ref(),
        pathspecs: &pathspecs,
        by_path: BTreeMap::new(),
    };

    // Commits reachable from negatives are UNINTERESTING (A in A..B).
    let mut uninteresting: HashSet<ObjectId> = HashSet::new();
    if !revision_options.negatives.is_empty() {
        let mut hide = RevWalk::new(
            &git_dir,
            format,
            &db,
            revision_options.negatives.iter().copied(),
        );
        hide = apply_rev_walk_options(hide, &revision_options);
        while let Some(meta) = hide.try_next()? {
            uninteresting.insert(meta.oid);
        }
    }

    // Walk commits. With include-edges, also visit the negative/boundary tips
    // so their trees contribute blobs (log -p / merge endpoints need them).
    let mut tree_roots: Vec<ObjectId> = Vec::new();
    let mut walk = RevWalk::new(&git_dir, format, &db, starts.iter().copied());
    walk = apply_rev_walk_options(walk, &revision_options);
    while let Some(commit_meta) = walk.try_next()? {
        if uninteresting.contains(&commit_meta.oid) {
            continue;
        }
        if let Ok(object) = db.read_object(&commit_meta.oid)
            && object.object_type == ObjectType::Commit
            && let Ok(commit) = Commit::parse_ref(format, &object.body)
        {
            tree_roots.push(commit.tree);
        }
    }
    if options.include_edges {
        for oid in &revision_options.negatives {
            if let Ok(object) = db.read_object(oid)
                && object.object_type == ObjectType::Commit
                && let Ok(commit) = Commit::parse_ref(format, &object.body)
            {
                tree_roots.push(commit.tree);
            }
        }
    }
    for tree in tree_roots {
        walker.walk_tree(tree, "")?;
    }
    walker.flush_paths()?;
    walker.download_batch()?;
    sley_core::trace2::data("path-walk", "paths", walker.visited_paths.len() as u64);
    Ok(())
}

fn apply_rev_walk_options<'a, R: ObjectReader>(
    mut walk: RevWalk<'a, R>,
    options: &RevisionOptions,
) -> RevWalk<'a, R> {
    walk = match options.order {
        RevisionOrder::Topo => walk.order(RevWalkOrder::Topo),
        RevisionOrder::Date | RevisionOrder::AuthorDate => walk.order(RevWalkOrder::CommitDate),
        RevisionOrder::Default => walk,
    };
    if options.first_parent {
        walk = walk.first_parent(true);
    }
    if options.date_window.min_time.is_some() || options.date_window.max_time.is_some() {
        walk = walk.date_window(RevWalkDateWindow {
            min_time: options.date_window.min_time,
            max_time: options.date_window.max_time,
        });
    }
    if let Some(max) = options.max_count {
        walk = walk.max_count(Some(max));
    }
    if options.skip > 0 {
        walk = walk.skip(options.skip);
    }
    walk
}

struct SparseMatcher {
    sparse: SparseCheckout,
    mode: SparseCheckoutMode,
}

struct BackfillWalker<'a> {
    db: &'a FileObjectDatabase,
    format: ObjectFormat,
    git_dir: &'a Path,
    cwd: &'a Path,
    config: &'a GitConfig,
    lazy_fetch: bool,
    min_batch_size: usize,
    batch: Vec<ObjectId>,
    seen_blobs: HashSet<ObjectId>,
    seen_trees: HashSet<ObjectId>,
    /// Unique path strings visited (git path-walk `paths` counter).
    visited_paths: HashSet<String>,
    sparse: Option<&'a SparseMatcher>,
    pathspecs: &'a [String],
    by_path: BTreeMap<String, Vec<ObjectId>>,
}

impl BackfillWalker<'_> {
    fn walk_tree(&mut self, tree_oid: ObjectId, prefix: &str) -> Result<()> {
        if !self.seen_trees.insert(tree_oid) {
            return Ok(());
        }
        // Count unique path strings, including the root "" path.
        self.visited_paths.insert(prefix.to_string());
        if !self.db.contains(&tree_oid)? {
            let _ = hydrate_oids(
                self.git_dir,
                self.cwd,
                self.config,
                self.db,
                self.format,
                self.lazy_fetch,
                &[tree_oid],
            );
            self.db.refresh_read_cache();
            if !self.db.contains(&tree_oid)? {
                return Ok(());
            }
        }
        let object = self.db.read_object(&tree_oid)?;
        if object.object_type != ObjectType::Tree {
            return Ok(());
        }
        for entry in TreeEntries::new(self.format, &object.body) {
            let entry = entry?;
            let name = String::from_utf8_lossy(entry.name).into_owned();
            let path = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            match tree_entry_object_type(entry.mode) {
                ObjectType::Tree => {
                    if let Some(matcher) = self.sparse
                        && matches!(matcher.mode, SparseCheckoutMode::Cone)
                    {
                        let dir = format!("{path}/");
                        if !path_in_sparse_checkout(dir.as_bytes(), &matcher.sparse, matcher.mode)
                            && !sparse_dir_may_contain(matcher, &path)
                        {
                            continue;
                        }
                    }
                    if !self.pathspecs.is_empty() && !pathspec_may_contain(self.pathspecs, &path) {
                        continue;
                    }
                    self.walk_tree(entry.oid, &path)?;
                }
                ObjectType::Blob => {
                    if let Some(matcher) = self.sparse
                        && !path_in_sparse_checkout(path.as_bytes(), &matcher.sparse, matcher.mode)
                    {
                        continue;
                    }
                    if !self.pathspecs.is_empty() && !pathspec_matches(self.pathspecs, &path) {
                        continue;
                    }
                    self.visited_paths.insert(path.clone());
                    if !self.seen_blobs.insert(entry.oid) {
                        continue;
                    }
                    if !self.db.contains(&entry.oid)? {
                        self.by_path.entry(path).or_default().push(entry.oid);
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn flush_paths(&mut self) -> Result<()> {
        let paths: Vec<String> = self.by_path.keys().cloned().collect();
        for path in paths {
            let Some(oids) = self.by_path.remove(&path) else {
                continue;
            };
            self.batch.extend(oids);
            if self.batch.len() >= self.min_batch_size {
                self.download_batch()?;
            }
        }
        Ok(())
    }

    fn download_batch(&mut self) -> Result<()> {
        if self.batch.is_empty() {
            return Ok(());
        }
        let batch = std::mem::take(&mut self.batch);
        let hydrated = hydrate_oids(
            self.git_dir,
            self.cwd,
            self.config,
            self.db,
            self.format,
            self.lazy_fetch,
            &batch,
        )?;
        if hydrated > 0 {
            sley_core::trace2::data("promisor", "fetch_count", hydrated as u64);
            sley_core::trace2::data("pack-objects", "written", hydrated as u64);
        }
        self.db.refresh_read_cache();
        Ok(())
    }
}

fn hydrate_oids(
    git_dir: &Path,
    cwd: &Path,
    config: &GitConfig,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    lazy_fetch: bool,
    oids: &[ObjectId],
) -> Result<usize> {
    if !lazy_fetch || oids.is_empty() {
        return Ok(0);
    }
    let mut seen = HashSet::new();
    let mut missing: Vec<ObjectId> = oids
        .iter()
        .copied()
        .filter(|oid| seen.insert(*oid) && !db.contains(oid).unwrap_or(false))
        .collect();
    if missing.is_empty() {
        return Ok(0);
    }
    let initial = missing.len();
    let remotes = promisor_remote_names(config);
    if remotes.is_empty() {
        return Ok(0);
    }
    sley_protocol::set_packet_trace_identity("fetch");
    for remote_name in &remotes {
        if missing.is_empty() {
            break;
        }
        let Some(url) = config.get("remote", Some(remote_name), "url") else {
            continue;
        };
        if config
            .get("remote", Some(remote_name), "uploadpack")
            .is_some()
        {
            continue;
        }
        let resolution = sley_remote::RemoteResolutionContext {
            cwd,
            local_git_dir: Some(git_dir),
            config: Some(config),
        };
        if let Ok(remote_git_dir) = sley_remote::resolve_local_remote_git_dir(resolution, url) {
            let filter = config
                .get("remote", Some(remote_name), "partialclonefilter")
                .and_then(sley_remote::pack_filter_from_spec);
            let _ = sley_remote::install_fetch_pack_via_local_upload_pack(
                git_dir,
                &remote_git_dir,
                format,
                missing.clone(),
                None,
                true,
                false,
                filter,
                None,
                false,
                None,
            );
            db.refresh_read_cache();
            missing.retain(|oid| !db.contains(oid).unwrap_or(false));
            continue;
        }
        // Network promisor (HTTP/HTTPS): exact-OID want via smart HTTP, same
        // path as partial-clone checkout blob top-up (t5620 #26).
        if let Ok(()) = hydrate_oids_via_http(git_dir, format, url, &missing) {
            db.refresh_read_cache();
            missing.retain(|oid| !db.contains(oid).unwrap_or(false));
        }
    }
    if !missing.is_empty()
        && let Ok(got) =
            sley_remote::hydrate_objects_from_local_promisor_remotes(git_dir, format, &missing)
        && !got.is_empty()
    {
        db.refresh_read_cache();
        missing.retain(|oid| !db.contains(oid).unwrap_or(false));
    }
    sley_protocol::set_packet_trace_identity("backfill");
    Ok(initial.saturating_sub(missing.len()))
}

/// Fetch exact missing object ids from a smart-HTTP promisor remote.
fn hydrate_oids_via_http(
    git_dir: &Path,
    format: ObjectFormat,
    url: &str,
    wants: &[ObjectId],
) -> Result<()> {
    if wants.is_empty() {
        return Ok(());
    }
    let remote = sley_transport::parse_remote_url(url)?;
    if !matches!(
        remote.transport,
        sley_transport::RemoteTransport::Http | sley_transport::RemoteTransport::Https
    ) {
        return Err(GitError::Unsupported(
            "backfill network hydrate requires HTTP(S)".into(),
        ));
    }
    let client = sley_transport::UreqHttpClient::new();
    let mut credentials = sley_remote::CredentialHelperProvider::new(None);
    let discovered = sley_remote::http_service_advertisements(
        &client,
        &remote,
        format,
        sley_protocol::GitService::UploadPack,
        &mut credentials,
        None,
    )?;
    let pack_request = sley_remote::HttpFetchPackRequest {
        client: &client,
        git_dir,
        format,
        remote: &remote,
        wants: wants.to_vec(),
        haves: None,
        shallow: Vec::new(),
        deepen: None,
        promisor: true,
        max_input_size: None,
        filter: None,
        deepen_since: None,
        deepen_not: Vec::new(),
        deepen_relative: false,
        // Prefer protocol v2 when the server offers it (handshake path below).
        git_protocol: Some("version=2"),
        // Default http.postBuffer (1 MiB); chunked encoding kicks in above this.
        post_buffer: 1 << 20,
        omit_haves: true,
    };
    let mut progress = sley_remote::SilentProgress;
    if let Some(handshake) = discovered.handshake.as_ref() {
        sley_remote::install_fetch_pack_via_http_protocol_v2_fetch(
            pack_request,
            handshake,
            &mut credentials,
            &mut progress,
            sley_core::CancelFlag::never(),
        )?;
    } else {
        sley_remote::install_fetch_pack_via_http_upload_pack(
            pack_request,
            &mut credentials,
            &mut progress,
            sley_core::CancelFlag::never(),
        )?;
    }
    Ok(())
}

fn parse_backfill_options(args: &[String]) -> Result<BackfillOptions> {
    let mut options = BackfillOptions {
        min_batch_size: DEFAULT_MIN_BATCH_SIZE,
        sparse: None,
        include_edges: true,
        stdin: false,
        remaining: Vec::new(),
    };
    let mut iter = args.iter().peekable();
    let mut saw_dashdash = false;
    while let Some(arg) = iter.next() {
        if saw_dashdash {
            options.remaining.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return Err(GitError::Exit(129));
            }
            "--" => {
                saw_dashdash = true;
                options.remaining.push(arg.clone());
            }
            "--stdin" => options.stdin = true,
            "--no-stdin" => options.stdin = false,
            "--sparse" => options.sparse = Some(true),
            "--no-sparse" => options.sparse = Some(false),
            "--include-edges" => options.include_edges = true,
            "--no-include-edges" => options.include_edges = false,
            value if value.starts_with("--min-batch-size=") => {
                let raw = &value["--min-batch-size=".len()..];
                options.min_batch_size = parse_usize(raw, "min-batch-size")?;
            }
            "--min-batch-size" => {
                let raw = iter.next().ok_or_else(|| {
                    GitError::Command("option `min-batch-size' requires a value".into())
                })?;
                options.min_batch_size = parse_usize(raw, "min-batch-size")?;
            }
            _ => options.remaining.push(arg.clone()),
        }
    }
    Ok(options)
}

fn parse_usize(raw: &str, name: &str) -> Result<usize> {
    raw.parse::<usize>()
        .map_err(|_| GitError::Command(format!("invalid --{name} value: {raw}")))
}

fn reject_unsupported_options(args: &[String]) -> Result<()> {
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        let (base, value) = match arg.split_once('=') {
            Some((b, v)) => (b, Some(v)),
            None => (arg.as_str(), None),
        };
        match base {
            "-S" | "-G" => {
                return Err(GitError::Command(format!(
                    "'{base}' cannot be used with 'git backfill'"
                )));
            }
            "--diff-filter" => {
                return Err(GitError::Command(
                    "'--diff-filter' cannot be used with 'git backfill'".into(),
                ));
            }
            "--follow" => {
                return Err(GitError::Command(
                    "'--follow' cannot be used with 'git backfill'".into(),
                ));
            }
            "-L" => {
                return Err(GitError::Command(
                    "'-L' cannot be used with 'git backfill'".into(),
                ));
            }
            "--diff-merges" => {
                return Err(GitError::Command(
                    "'--diff-merges' cannot be used with 'git backfill'".into(),
                ));
            }
            "--filter" => {
                let spec = value
                    .map(str::to_string)
                    .or_else(|| {
                        i += 1;
                        args.get(i).cloned()
                    })
                    .unwrap_or_default();
                if spec.starts_with("blob:limit") {
                    return Err(GitError::Command(
                        "cannot backfill with blob size limits".into(),
                    ));
                }
                // tree:N and other non-sparse filters are rejected.
                if !spec.starts_with("sparse:oid") {
                    return Err(GitError::Command(
                        "cannot backfill with these filter options".into(),
                    ));
                }
            }
            _ => {}
        }
        i += 1;
    }
    Ok(())
}

fn is_known_rev_list_option(arg: &str) -> bool {
    matches!(
        arg.split('=').next().unwrap_or(arg),
        "--all"
            | "--branches"
            | "--tags"
            | "--remotes"
            | "--glob"
            | "--first-parent"
            | "--no-first-parent"
            | "--stdin"
            | "--objects"
            | "--since"
            | "--until"
            | "--max-age"
            | "--min-age"
            | "--max-count"
            | "--skip"
            | "--reverse"
            | "--topo-order"
            | "--date-order"
            | "--author-date-order"
            | "--filter"
            | "--sparse"
            | "--no-sparse"
            | "--include-edges"
            | "--no-include-edges"
            | "--min-batch-size"
    ) || arg.starts_with("--since=")
        || arg.starts_with("--until=")
        || arg.starts_with("--max-age=")
        || arg.starts_with("--min-age=")
        || arg.starts_with("--filter=")
        || arg.starts_with("--min-batch-size=")
}

fn map_setup_error(err: GitError, args: &[String]) -> GitError {
    let msg = err.to_string();
    if msg.contains("unknown revision")
        || msg.contains("bad revision")
        || msg.contains("ambiguous")
        || msg.contains("Needed a single revision")
        || msg.contains("not a valid object")
    {
        if let Some(arg) = args
            .iter()
            .find(|a| !a.starts_with('-') && a.as_str() != "--")
        {
            return GitError::Command(format!(
                "ambiguous argument '{arg}': unknown revision or path not in the working tree."
            ));
        }
    }
    err
}

fn load_sparse_matcher(git_dir: &Path, config: &GitConfig) -> Result<SparseMatcher> {
    let path = git_dir.join("info").join("sparse-checkout");
    let contents = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(GitError::Command("problem loading sparse-checkout".into()));
        }
        Err(err) => return Err(GitError::Io(err.to_string())),
    };
    let patterns = contents
        .split(|&b| b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| line.to_vec())
        .collect::<Vec<_>>();
    if patterns.is_empty() {
        return Err(GitError::Command("problem loading sparse-checkout".into()));
    }
    let cone = config
        .get_bool("core", None, "sparseCheckoutCone")
        .unwrap_or(true);
    Ok(SparseMatcher {
        sparse: SparseCheckout {
            patterns,
            sparse_index: false,
        },
        mode: if cone {
            SparseCheckoutMode::Cone
        } else {
            SparseCheckoutMode::Full
        },
    })
}

fn sparse_dir_may_contain(matcher: &SparseMatcher, dir: &str) -> bool {
    // A cone directory should be walked if any pattern is under it or it is
    // under a recursive include.
    let dir_slash = format!("{dir}/");
    matcher.sparse.patterns.iter().any(|pat| {
        let p = String::from_utf8_lossy(pat);
        let p = p.trim();
        if p.starts_with('!') {
            return false;
        }
        let p = p.trim_start_matches('/');
        p.starts_with(&dir_slash) || p.starts_with(dir) || dir.starts_with(p.trim_end_matches('/'))
    })
}

fn pathspec_matches(pathspecs: &[String], path: &str) -> bool {
    pathspecs.iter().any(|spec| {
        let spec = spec.strip_prefix("./").unwrap_or(spec);
        if let Some(prefix) = spec.strip_suffix('/') {
            return path == prefix || path.starts_with(&format!("{prefix}/"));
        }
        if spec.contains('*') || spec.contains('?') {
            return glob_match(spec, path);
        }
        path == spec || path.starts_with(&format!("{spec}/"))
    })
}

fn pathspec_may_contain(pathspecs: &[String], dir: &str) -> bool {
    pathspecs.iter().any(|spec| {
        let clean = spec.trim_end_matches('/').trim_start_matches("./");
        if clean.contains('*') || clean.contains('?') {
            return true;
        }
        clean.starts_with(dir)
            || dir.starts_with(clean)
            || clean.starts_with(&format!("{dir}/"))
            || dir.starts_with(&format!("{clean}/"))
    })
}

fn glob_match(pattern: &str, path: &str) -> bool {
    let pattern = pattern.strip_prefix("./").unwrap_or(pattern);
    let path = path.strip_prefix("./").unwrap_or(path);
    if let Some(rest) = pattern.strip_prefix("**/") {
        if glob_match(rest, path) {
            return true;
        }
        let parts: Vec<&str> = path.split('/').collect();
        for i in 0..parts.len() {
            let suffix = parts[i..].join("/");
            if glob_match(rest, &suffix) {
                return true;
            }
        }
        return false;
    }
    let p: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = path.chars().collect();
    fn rec(p: &[char], s: &[char]) -> bool {
        let mut i = 0;
        let mut j = 0;
        while i < p.len() {
            match p[i] {
                '*' => {
                    if i + 1 == p.len() {
                        return true;
                    }
                    for k in j..=s.len() {
                        if rec(&p[i + 1..], &s[k..]) {
                            return true;
                        }
                    }
                    return false;
                }
                '?' => {
                    if j >= s.len() {
                        return false;
                    }
                    i += 1;
                    j += 1;
                }
                c => {
                    if j >= s.len() || s[j] != c {
                        return false;
                    }
                    i += 1;
                    j += 1;
                }
            }
        }
        j == s.len()
    }
    rec(&p, &s)
}
