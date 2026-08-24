//! `sley-gc` — garbage collection, repack, and maintenance engine.
//!
//! Extracted from the CLI command tier (phase 3 heavyweight extraction). The
//! crate owns the gc/repack/maintenance planning and execution machinery:
//! repack strategy selection (cruft packs, geometric repacks, bitmap
//! decisions, pseudo-merge groups), MIDX chain management, prune walks,
//! maintenance task scheduling, count-objects aggregation, and the trace2
//! helpers those paths emit.
//!
//! Porcelain stays in the CLI: argv parsing, usage/help rendering, stdout
//! formatting, and exit codes. Everything that is inherently presentation-tier
//! (trace lines, pack-refs/reflog-expire/commit-graph/update-server-info
//! execution, hook invocation) reaches this crate through [`GcServices`],
//! following the `PlanRequest`/`PlanServices` injection precedent of
//! `sley-rev::format_patch`.
//!
//! Dependency note: `sley-rev` is required for the replacement-policy-aware
//! revision resolver used by traversal-root collection (`resolve_revision`
//! semantics); `sley-worktree` is required for the canonical worktree probe
//! behind `gc.packRefs=notbare` and bare-repo bitmap auto-detection. Both are
//! leaf plumbing crates, so the dependency graph stays acyclic.

pub mod count_objects;
pub mod gc;
pub mod maintenance;
pub mod midx;
pub mod prune;
pub mod repack;
pub mod trace2;

use std::path::{Path, PathBuf};

use sley_config::GitConfig;
use sley_core::{GitError, ObjectFormat, ObjectId, Result};

/// Presentation-tier delegates plus injected configuration for the gc engine.
///
/// The engine never discovers process-global state on its own; the CLI builds
/// one of these per invocation from its session. Lock/pid/log file semantics
/// are NOT services — they are byte-preserved engine behavior (including
/// `maintenance.lock` / `schedule.lock` acquisition, stale-lock recovery, and
/// removal on every exit path; see `maintenance::run_selected` /
/// `update_background_schedule`).
pub struct GcServices<'a> {
    /// `GIT_TRACE_LINE` delegate (`setup::git_trace_line`).
    pub git_trace_line: &'a dyn Fn(&str, &str),
    /// Replace-object policy from the invocation (`--no-replace-objects`).
    pub replace_objects: bool,
    /// `git pack-refs --all --prune` execution.
    pub pack_refs_all_prune: &'a mut dyn FnMut() -> Result<()>,
    /// `git reflog expire <args>` execution; expiry policy stays in the refs
    /// layer, the engine only composes the argument list.
    pub reflog_expire: &'a mut dyn FnMut(&[String]) -> Result<()>,
    /// `git commit-graph write --reachable [--progress|--no-progress]`.
    pub commit_graph_write_reachable: &'a mut dyn FnMut(bool) -> Result<()>,
    /// `git update-server-info` execution.
    pub update_server_info: &'a mut dyn FnMut() -> Result<()>,
    /// The `pre-auto-gc` hook. Returns `false` when the hook vetoed auto-gc;
    /// `None` when hooks are unavailable to the embedder.
    pub pre_auto_gc_hook_ok: Option<&'a dyn Fn() -> bool>,
    /// `reftable.lockTimeout` override read from the effective config stream.
    pub reftable_lock_timeout: Option<u64>,
    /// Whether config names a promisor remote (`config_has_promisor_remote`).
    pub has_promisor_remote: Option<&'a dyn Fn(&GitConfig) -> bool>,
    /// Backfill reachable objects from local promisor remotes before an
    /// all-into-one repack.
    pub hydrate_promisor_remotes: Option<HydratePromisorRemotes<'a>>,
}

/// Backfill delegate: object dir, format, traversal roots.
pub type HydratePromisorRemotes<'a> =
    &'a mut dyn FnMut(&Path, ObjectFormat, &[ObjectId]) -> Result<()>;

pub(crate) fn trace_line(services: &GcServices, file_line: &str, message: &str) {
    (services.git_trace_line)(file_line, message);
}

/// Read the repository config the way every gc-path consumer does: resolved
/// includes layered with command-line / environment injections.
pub(crate) fn read_repo_config(git_dir: &Path) -> Result<GitConfig> {
    sley_config::read_repo_config(git_dir, sley_config::effective_config_parameters_env().as_deref())
}

/// Read the object format declared by `<git_dir>/config`, defaulting to SHA-1
/// exactly like the CLI helper this was extracted from.
pub(crate) fn repo_object_format(git_dir: &Path) -> Result<ObjectFormat> {
    let Ok(config) = GitConfig::read(git_dir.join("config")) else {
        return Ok(ObjectFormat::Sha1);
    };
    config.repository_object_format()
}

/// Resolve the common git directory for a (possibly linked-worktree) git dir.
pub(crate) fn common_git_dir_for_git_dir(git_dir: &Path) -> Result<PathBuf> {
    sley_formats::repository_common_dir(git_dir, true)
}

/// Resolve a revision honouring the replace-object policy (the CLI's
/// unqualified `resolve_revision`).
pub(crate) fn resolve_revision(
    git_dir: &Path,
    format: ObjectFormat,
    rev: &str,
    replace_objects: bool,
) -> Result<ObjectId> {
    sley_rev::resolve_revision_with_replacement_policy(git_dir, format, rev, replace_objects)
}

/// Peel a ref name to its direct oid through symref chains.
pub(crate) fn resolve_ref_to_oid(
    store: &sley_refs::FileRefStore,
    name: &str,
) -> Result<Option<ObjectId>> {
    sley_refs::resolve_ref_peeled(store, name)
}

pub(crate) fn current_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or(0)
}

/// Resolve a user-supplied path against the invocation directory (the CLI's
/// `resolve_cli_path`).
pub(crate) fn resolve_path_under(cwd: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() { path } else { cwd.join(path) }
}

/// git's `parse_expiry_date` composition used across the gc/prune paths:
/// "never"/"false" never expire; "all"/"now" expire everything.
pub(crate) fn parse_reflog_expire_time(value: &str, option: &str) -> Result<i64> {
    match value {
        "all" | "now" => return Ok(i64::MAX),
        "never" | "false" => return Ok(i64::MIN),
        _ => {}
    }
    if let Some(ts) = parse_reflog_expire_date(value) {
        return Ok(ts);
    }
    if let Some(ts) = sley_core::date::approxidate::parse_approxidate(value) {
        return Ok(ts);
    }
    eprintln!("fatal: invalid timestamp '{value}' given to '{option}'");
    Err(GitError::Exit(128))
}

fn parse_reflog_expire_date(value: &str) -> Option<i64> {
    use sley_core::date;
    let mut parts = value.split_whitespace();
    let first = parts.next()?;
    if let Some(timestamp) = first.strip_prefix('@') {
        let timezone = parts.next()?;
        if parts.next().is_some() || date::parse_tz_offset(timezone).is_none() {
            return None;
        }
        return timestamp.parse::<i64>().ok();
    }
    let (date_str, time) = if let Some((date, time)) = first.split_once('T') {
        (date, time)
    } else {
        (first, parts.next()?)
    };
    let timezone = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let (year, month, day) = date::parse_date_ymd(date_str)?;
    let (hour, minute, second) = date::parse_time_hms(time)?;
    let timezone_offset = date::parse_tz_offset(timezone)?;
    Some(
        date::days_from_civil(year, month, day)
            .saturating_mul(86_400)
            .saturating_add(i64::from(hour * 3_600 + minute * 60 + second))
            .saturating_sub(timezone_offset),
    )
}
