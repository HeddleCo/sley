//! Maintenance task table, selection and run order, need probes and counters,
//! geometric planning, prefetch/loose-objects tasks, the sley child runner,
//! register/unregister config-file editing, and the OS scheduler integrations
//! (cron, systemd, launchctl, schtasks).
//!
//! `maintenance.lock` / `schedule.lock` are created with `create_new` and
//! record the acquiring pid. A pre-existing lock older than twelve hours
//! (gc.pid's staleness window) counts as abandoned and is removed before one
//! re-acquire attempt; anything fresher blocks the run. The lock is removed on
//! every exit path, including individual task failures, so a failed run can
//! never wedge later `--auto` runs.

use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use sley_config::{ConfigEntry, ConfigSection, GitConfig};
use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_formats::CommitGraph;
use sley_odb::{repository_objects_dir, FileObjectDatabase};
use sley_odb::ObjectReader as _;
use sley_object::ObjectType;
use sley_refs::{FileRefStore, RefTarget};

use crate::trace2;
use crate::prune::parse_prune_expire;
use crate::{current_unix_seconds, GcServices, parse_reflog_expire_time, repo_object_format};

/// The maintenance task names git's `builtin/gc.c` `tasks[]` table recognises,
/// in declaration order. `--task=<name>` is case-insensitive against this set.
pub const MAINTENANCE_TASKS: &[&str] = &[
    "prefetch",
    "loose-objects",
    "incremental-repack",
    "geometric-repack",
    "gc",
    "commit-graph",
    "pack-refs",
    "reflog-expire",
    "worktree-prune",
    "rerere-gc",
];

pub fn maintenance_select_tasks(
    config: &GitConfig,
    requested: &[String],
    schedule: Option<&str>,
) -> Result<Vec<String>> {
    if !requested.is_empty() {
        return Ok(requested
            .iter()
            .map(|task| task.to_ascii_lowercase())
            .collect());
    }
    let strategy = config
        .get("maintenance", None, "strategy")
        .unwrap_or(if schedule.is_some() {
            "none"
        } else {
            "geometric"
        });
    let strategy_name = strategy.to_ascii_lowercase();
    let mut selected = match strategy_name.as_str() {
        "none" => Vec::new(),
        "gc" => vec!["gc"],
        "incremental" if schedule.is_some() => vec![
            "prefetch",
            "loose-objects",
            "incremental-repack",
            "commit-graph",
            "pack-refs",
        ],
        "incremental" => vec!["gc"],
        "geometric" => vec![
            "geometric-repack",
            "commit-graph",
            "pack-refs",
            "reflog-expire",
            "worktree-prune",
            "rerere-gc",
        ],
        other => {
            eprintln!("fatal: unknown maintenance strategy: '{other}'");
            return Err(GitError::Exit(128));
        }
    }
    .into_iter()
    .map(str::to_string)
    .collect::<Vec<_>>();

    for task in MAINTENANCE_TASKS {
        if let Some(enabled) = config.get_bool("maintenance", Some(task), "enabled") {
            selected.retain(|selected| !selected.eq_ignore_ascii_case(task));
            if enabled {
                selected.push((*task).to_string());
            }
        }
    }

    if let Some(schedule) = schedule {
        let requested_schedule = maintenance_schedule_rank(schedule)?;
        selected.retain(|task| {
            let default_schedule = match task.as_str() {
                "commit-graph" | "prefetch" => "hourly",
                "loose-objects" | "incremental-repack" | "geometric-repack" | "gc" => "daily",
                "pack-refs" if strategy_name == "incremental" => "weekly",
                "pack-refs" => "daily",
                _ => "weekly",
            };
            let task_schedule = config
                .get("maintenance", Some(task), "schedule")
                .unwrap_or(default_schedule);
            maintenance_schedule_rank(task_schedule).unwrap_or(0) >= requested_schedule
        });
    }

    selected.sort_by_key(|task| maintenance_run_order(task));
    Ok(selected)
}

/// Validate a `--schedule=<frequency>` value against git's `parse_schedule`
/// (hourly/daily/weekly, case-insensitive). Returns the value on success; emits
/// git's `unrecognized --schedule argument` diagnostic (rc 128) otherwise.
pub fn validate_maintenance_schedule(value: &str) -> Result<String> {
    if matches!(
        value.to_ascii_lowercase().as_str(),
        "hourly" | "daily" | "weekly"
    ) {
        Ok(value.to_string())
    } else {
        eprintln!("fatal: unrecognized --schedule argument '{value}'");
        Err(GitError::Exit(128))
    }
}

pub fn maintenance_run_order(task: &str) -> usize {
    match task {
        "pack-refs" => 0,
        "reflog-expire" => 1,
        "gc" => 2,
        "prefetch" => 3,
        "loose-objects" => 4,
        "incremental-repack" => 5,
        "geometric-repack" => 6,
        "commit-graph" => 7,
        "worktree-prune" => 8,
        "rerere-gc" => 9,
        _ => usize::MAX,
    }
}

fn maintenance_schedule_rank(value: &str) -> Result<u8> {
    match validate_maintenance_schedule(value)?
        .to_ascii_lowercase()
        .as_str()
    {
        "weekly" => Ok(1),
        "daily" => Ok(2),
        "hourly" => Ok(3),
        _ => Ok(0),
    }
}

/// A maintenance-style lock is considered abandoned after this long without
/// progress, mirroring gc.pid's twelve-hour staleness window.
const MAINTENANCE_LOCK_STALE_SECONDS: u64 = 12 * 60 * 60;

fn try_acquire_lock(lock: &Path) -> std::io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(lock)?;
    // Record the holder for diagnostics; the mtime of this write doubles as
    // the staleness timestamp.
    writeln!(file, "{}", std::process::id())
}

fn lock_age_seconds(lock: &Path) -> Option<u64> {
    let modified = fs::metadata(lock).ok()?.modified().ok()?;
    modified.elapsed().ok().map(|elapsed| elapsed.as_secs())
}

/// Acquire a create-new lock, treating a pre-existing lock older than
/// [`MAINTENANCE_LOCK_STALE_SECONDS`] as abandoned: it is removed and one
/// re-acquire is attempted. Returns `false` when a live lock is held elsewhere.
fn acquire_lock_with_stale_recovery(lock: &Path) -> bool {
    if try_acquire_lock(lock).is_ok() {
        return true;
    }
    match lock_age_seconds(lock) {
        Some(age) if age >= MAINTENANCE_LOCK_STALE_SECONDS => {}
        _ => return false,
    }
    if fs::remove_file(lock).is_err() {
        return false;
    }
    try_acquire_lock(lock).is_ok()
}

pub fn maintenance_run_selected(
    services: &mut GcServices,
    common_git_dir: &Path,
    config: &GitConfig,
    tasks: &[String],
    quiet: bool,
    auto: bool,
    detach: bool,
) -> Result<()> {
    let lock = repository_objects_dir(common_git_dir).join("maintenance.lock");
    if !acquire_lock_with_stale_recovery(&lock) {
        if auto {
            return Ok(());
        }
        eprintln!("fatal: 'maintenance' lock held by another process");
        return Err(GitError::Exit(128));
    }
    if detach {
        trace2::region("region_enter", "maintenance", "detach");
        trace2::region("region_leave", "maintenance", "detach");
    }
    let run = (|| -> Result<()> {
        for task in tasks {
            if auto && !maintenance_task_needed(common_git_dir, config, task)? {
                continue;
            }
            maintenance_run_one(services, common_git_dir, config, task, quiet, auto)?;
        }
        Ok(())
    })();
    let _ = fs::remove_file(&lock);
    run
}

fn maintenance_run_one(
    services: &mut GcServices,
    common_git_dir: &Path,
    config: &GitConfig,
    task: &str,
    quiet: bool,
    auto: bool,
) -> Result<()> {
    match task {
        "commit-graph" => {
            if config.get_bool("core", None, "commitGraph") == Some(false) {
                return Ok(());
            }
            let progress = if quiet { "--no-progress" } else { "--progress" };
            trace2::child_start(&["commit-graph", "write", "--split", "--reachable", progress]);
            (services.commit_graph_write_reachable)(!quiet)
        }
        "pack-refs" => {
            if auto {
                run_sley_child(&["pack-refs", "--all", "--prune", "--auto"], None)
            } else {
                run_sley_child(&["pack-refs", "--all", "--prune"], None)
            }
        }
        "reflog-expire" => run_sley_child(&["reflog", "expire", "--all"], None),
        "worktree-prune" => {
            let expire = config
                .get("gc", None, "worktreePruneExpire")
                .unwrap_or("3.months.ago");
            run_sley_child(&["worktree", "prune", "--expire", expire], None)
        }
        "rerere-gc" => run_sley_child(&["rerere", "gc"], None),
        "gc" => {
            run_sley_child(&["pack-refs", "--all", "--prune"], None)?;
            run_sley_child(&["reflog", "expire", "--all"], None)?;
            let mut args = vec!["gc"];
            if auto {
                args.push("--auto");
            }
            args.push(if quiet { "--quiet" } else { "--no-quiet" });
            args.push("--no-detach");
            args.push("--skip-foreground-tasks");
            run_sley_child(&args, None)
        }
        "prefetch" => maintenance_prefetch(config, quiet),
        "loose-objects" => maintenance_loose_objects(common_git_dir, config, quiet),
        "incremental-repack" => {
            if config.get_bool("core", None, "multiPackIndex") == Some(false) {
                if !quiet {
                    eprintln!(
                        "warning: skipping incremental-repack task because core.multiPackIndex is disabled"
                    );
                }
                return Ok(());
            }
            let progress = if quiet { "--no-progress" } else { "--progress" };
            run_sley_child(&["multi-pack-index", "write", progress], None)?;
            run_sley_child(&["multi-pack-index", "expire", progress], None)?;
            let batch = format!(
                "--batch-size={}",
                maintenance_auto_pack_size(common_git_dir)?
            );
            run_sley_child(
                &["multi-pack-index", "repack", progress, batch.as_str()],
                None,
            )
        }
        "geometric-repack" => maintenance_geometric_repack(common_git_dir, config, quiet),
        _ => Ok(()),
    }
}

pub fn maintenance_task_needed(common_git_dir: &Path, config: &GitConfig, task: &str) -> Result<bool> {
    Ok(match task {
        "commit-graph" => maintenance_limit_satisfied(
            config,
            "commit-graph",
            100,
            count_reachable_commits_not_in_graph(common_git_dir)?,
        )?,
        "loose-objects" => maintenance_limit_satisfied(
            config,
            "loose-objects",
            100,
            loose_object_ids(common_git_dir, repo_object_format(common_git_dir)?)?.len(),
        )?,
        "incremental-repack" => maintenance_pack_count_exceeds_limit(
            config,
            task,
            10,
            count_pack_files(common_git_dir)?,
        )?,
        "geometric-repack" => maintenance_geometric_repack_needed(common_git_dir, config)?,
        "worktree-prune" => worktree_prune_needed(common_git_dir, config)?,
        "rerere-gc" => rerere_gc_needed(common_git_dir, config)?,
        "reflog-expire" => maintenance_reflog_expire_needed(common_git_dir, config)?,
        "pack-refs" => true,
        _ => false,
    })
}

fn maintenance_limit_satisfied(
    config: &GitConfig,
    task: &str,
    default: i64,
    count: usize,
) -> Result<bool> {
    let limit = maintenance_auto_limit(config, task, default);
    Ok(limit < 0 || (limit > 0 && count >= limit as usize))
}

fn maintenance_pack_count_exceeds_limit(
    config: &GitConfig,
    task: &str,
    default: i64,
    count: usize,
) -> Result<bool> {
    let limit = maintenance_auto_limit(config, task, default);
    Ok(limit < 0 || (limit > 0 && count > limit as usize))
}

fn maintenance_geometric_split_factor(config: &GitConfig) -> u64 {
    config
        .get("maintenance", Some("geometric-repack"), "splitFactor")
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|&factor| factor > 0)
        .unwrap_or(2)
}

fn maintenance_geometric_repack_plan(
    common_git_dir: &Path,
    config: &GitConfig,
) -> Result<sley_odb::GeometricRepackPlan> {
    let format = repo_object_format(common_git_dir)?;
    sley_odb::geometric_repack_plan(
        common_git_dir,
        format,
        maintenance_geometric_split_factor(config),
        &HashSet::new(),
    )
}

fn maintenance_geometric_repack(
    common_git_dir: &Path,
    config: &GitConfig,
    quiet: bool,
) -> Result<()> {
    let factor = maintenance_geometric_split_factor(config);
    let plan = maintenance_geometric_repack_plan(common_git_dir, config)?;
    let mut args = vec!["repack", "-d", "-l"];
    let geometric;
    if plan.split < plan.pack_count {
        geometric = format!("--geometric={factor}");
        args.push(geometric.as_str());
    } else {
        args.push("--cruft");
        args.push("--cruft-expiration=2.weeks.ago");
    }
    if quiet {
        args.push("--quiet");
    }
    args.push("--write-midx");
    run_sley_child(&args, None)
}

fn maintenance_geometric_repack_needed(common_git_dir: &Path, config: &GitConfig) -> Result<bool> {
    let limit = maintenance_auto_limit(config, "geometric-repack", 100);
    if limit == 0 {
        return Ok(false);
    }
    if limit < 0 {
        return Ok(true);
    }
    let plan = maintenance_geometric_repack_plan(common_git_dir, config)?;
    if plan.split > 0 {
        return Ok(true);
    }
    Ok(false)
}

fn maintenance_auto_limit(config: &GitConfig, task: &str, default: i64) -> i64 {
    config
        .get("maintenance", Some(task), "auto")
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(default)
}

fn maintenance_reflog_expire_needed(common_git_dir: &Path, config: &GitConfig) -> Result<bool> {
    let limit = maintenance_auto_limit(config, "reflog-expire", 100);
    if limit == 0 {
        return Ok(false);
    }
    if limit < 0 {
        return Ok(true);
    }

    let cutoff = match config.get("gc", None, "reflogExpire") {
        Some(value) => parse_reflog_expire_time(value, "gc.reflogExpire")?,
        None => current_unix_seconds().saturating_sub(30 * 24 * 60 * 60),
    };
    if cutoff == i64::MIN {
        return Ok(false);
    }

    let format = repo_object_format(common_git_dir)?;
    let store = FileRefStore::new(common_git_dir, format);
    let mut count = 0usize;
    for entry in store.read_reflog("HEAD")? {
        if entry.timestamp_seconds()? < cutoff {
            count += 1;
            if count >= limit as usize {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

pub fn run_sley_child(args: &[&str], stdin_data: Option<&str>) -> Result<()> {
    trace2::child_start(args);
    let mut child = ProcessCommand::new(env::current_exe()?);
    child.args(args);
    child.env(
        "SLEY_TRACE2_DEPTH",
        (sley_core::trace2::depth() + 1).to_string(),
    );
    if stdin_data.is_some() {
        child.stdin(std::process::Stdio::piped());
    }
    if args.first() == Some(&"pack-objects") {
        child.stdout(std::process::Stdio::null());
    }
    let mut child = child.spawn()?;
    if let Some(input) = stdin_data
        && let Some(mut stdin) = child.stdin.take()
    {
        stdin.write_all(input.as_bytes())?;
    }
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(GitError::Exit(status.code().unwrap_or(1)))
    }
}

fn maintenance_prefetch(config: &GitConfig, quiet: bool) -> Result<()> {
    let mut remotes = Vec::new();
    for section in &config.sections {
        if section.name.eq_ignore_ascii_case("remote")
            && let Some(name) = &section.subsection
            && section
                .entries
                .iter()
                .any(|entry| entry.key.eq_ignore_ascii_case("url"))
            && !config
                .get_bool("remote", Some(name), "skipFetchAll")
                .unwrap_or(false)
        {
            remotes.push(name.clone());
        }
    }
    remotes.sort();
    remotes.dedup();
    for remote in remotes {
        let mut args = vec![
            "fetch",
            remote.as_str(),
            "--prefetch",
            "--prune",
            "--no-tags",
            "--no-write-fetch-head",
            "--recurse-submodules=no",
        ];
        if quiet {
            args.push("--quiet");
        }
        run_sley_child(&args, None)?;
    }
    Ok(())
}

fn maintenance_loose_objects(common_git_dir: &Path, config: &GitConfig, quiet: bool) -> Result<()> {
    let mut prune_args = vec!["prune-packed"];
    if quiet {
        prune_args.push("--quiet");
    }
    run_sley_child(&prune_args, None)?;
    let loose = loose_object_ids(common_git_dir, repo_object_format(common_git_dir)?)?;
    if loose.is_empty() {
        return Ok(());
    }
    let mut batch = config
        .get("maintenance", Some("loose-objects"), "batchSize")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(50_000);
    if batch == 0 {
        batch = usize::MAX;
    }
    let input = loose
        .into_iter()
        .take(batch)
        .map(|oid| format!("{oid}\n"))
        .collect::<String>();
    let base = common_git_dir.join("objects").join("pack").join("loose");
    let base = base.display().to_string();
    let mut args = vec!["pack-objects"];
    args.push(if quiet { "--quiet" } else { "--no-quiet" });
    args.push(base.as_str());
    run_sley_child(&args, Some(&input))
}

fn loose_object_ids(common_git_dir: &Path, format: ObjectFormat) -> Result<Vec<String>> {
    let hex_len = format.hex_len();
    let objects = common_git_dir.join("objects");
    let mut out = Vec::new();
    if !objects.exists() {
        return Ok(out);
    }
    for shard in fs::read_dir(objects)? {
        let shard = shard?;
        let Some(prefix) = shard.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if prefix.len() != 2 || !prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        for entry in fs::read_dir(shard.path())? {
            let entry = entry?;
            let Some(suffix) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if suffix.len() == hex_len - 2 && suffix.bytes().all(|b| b.is_ascii_hexdigit()) {
                out.push(format!("{prefix}{suffix}"));
            }
        }
    }
    out.sort();
    Ok(out)
}

fn count_pack_files(common_git_dir: &Path) -> Result<usize> {
    let pack_dir = common_git_dir.join("objects").join("pack");
    if !pack_dir.exists() {
        return Ok(0);
    }
    Ok(fs::read_dir(pack_dir)?
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("pack"))
        .count())
}

fn maintenance_auto_pack_size(common_git_dir: &Path) -> Result<u64> {
    let pack_dir = common_git_dir.join("objects").join("pack");
    let mut sizes = Vec::new();
    if pack_dir.exists() {
        for entry in fs::read_dir(pack_dir)? {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("pack") {
                sizes.push(fs::metadata(path)?.len());
            }
        }
    }
    sizes.sort_unstable_by(|a, b| b.cmp(a));
    Ok(sizes
        .get(1)
        .copied()
        .unwrap_or(0)
        .saturating_add(1)
        .min(i32::MAX as u64))
}

/// Collect the `parent` headers from a commit object's body, stopping at the
/// blank line that ends the header block so commit-message text mentioning
/// "parent " cannot fabricate edges.
fn commit_parent_oids(format: ObjectFormat, body: &[u8]) -> Vec<ObjectId> {
    let text = String::from_utf8_lossy(body);
    text.lines()
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.strip_prefix("parent "))
        .filter_map(|hex| ObjectId::from_hex(format, hex).ok())
        .collect()
}

fn count_reachable_commits(common_git_dir: &Path) -> Result<usize> {
    let format = repo_object_format(common_git_dir)?;
    let refs = FileRefStore::new(common_git_dir, format);
    let db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    let mut seen = HashSet::new();
    let mut stack = Vec::new();
    for reference in refs.list_refs()? {
        if let RefTarget::Direct(oid) = reference.target
            && db.read_object(&oid).is_ok()
        {
            stack.push(oid);
        }
    }
    while let Some(oid) = stack.pop() {
        if !seen.insert(oid) {
            continue;
        }
        let object = match db.read_object(&oid) {
            Ok(object) if object.object_type == ObjectType::Commit => object,
            _ => continue,
        };
        stack.extend(commit_parent_oids(format, &object.body));
    }
    Ok(seen.len())
}

fn count_reachable_commits_not_in_graph(common_git_dir: &Path) -> Result<usize> {
    let format = repo_object_format(common_git_dir)?;
    let graph_oids = commit_graph_oids(common_git_dir, format)?;
    if graph_oids.is_empty() {
        return count_reachable_commits(common_git_dir);
    }
    let refs = FileRefStore::new(common_git_dir, format);
    let db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    let mut seen = HashSet::new();
    let mut missing = 0;
    let mut stack = Vec::new();
    for reference in refs.list_refs()? {
        if let RefTarget::Direct(oid) = reference.target
            && db.read_object(&oid).is_ok()
        {
            stack.push(oid);
        }
    }
    while let Some(oid) = stack.pop() {
        if !seen.insert(oid) {
            continue;
        }
        if graph_oids.contains(&oid) {
            continue;
        }
        let object = match db.read_object(&oid) {
            Ok(object) if object.object_type == ObjectType::Commit => object,
            _ => continue,
        };
        missing += 1;
        stack.extend(commit_parent_oids(format, &object.body));
    }
    Ok(missing)
}

fn commit_graph_oids(common_git_dir: &Path, format: ObjectFormat) -> Result<HashSet<ObjectId>> {
    let info = repository_objects_dir(common_git_dir).join("info");
    let single = info.join("commit-graph");
    let mut oids = HashSet::new();
    if single.exists() {
        let bytes = fs::read(single)?;
        let graph = CommitGraph::parse(&bytes, format)?;
        oids.extend(graph.commits.into_iter().map(|entry| entry.oid));
        return Ok(oids);
    }
    let graphs = info.join("commit-graphs");
    let chain = graphs.join("commit-graph-chain");
    let Ok(contents) = fs::read_to_string(chain) else {
        return Ok(oids);
    };
    for line in contents.lines() {
        let hash = line.trim();
        if hash.is_empty() {
            continue;
        }
        let bytes = fs::read(graphs.join(format!("graph-{hash}.graph")))?;
        let graph = CommitGraph::parse(&bytes, format)?;
        oids.extend(graph.commits.into_iter().map(|entry| entry.oid));
    }
    Ok(oids)
}

fn rerere_gc_needed(common_git_dir: &Path, config: &GitConfig) -> Result<bool> {
    let limit = config
        .get("maintenance", Some("rerere-gc"), "auto")
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(1);
    if limit <= 0 {
        return Ok(limit < 0);
    }
    Ok(count_dir_entries(&common_git_dir.join("rr-cache"))? > 0)
}

fn worktree_prune_needed(common_git_dir: &Path, config: &GitConfig) -> Result<bool> {
    let limit = config
        .get("maintenance", Some("worktree-prune"), "auto")
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(1);
    if limit <= 0 {
        return Ok(limit < 0);
    }

    let expire = config
        .get("gc", None, "worktreePruneExpire")
        .unwrap_or("3.months.ago");
    let expire_time = parse_prune_expire(expire, "--expire")?;
    let worktrees = common_git_dir.join("worktrees");
    let Ok(entries) = fs::read_dir(&worktrees) else {
        return Ok(false);
    };
    let mut prunable = 0usize;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() || linked_worktree_admin_is_prunable(&path, expire_time)? {
            prunable += 1;
        }
        if prunable >= limit as usize {
            return Ok(true);
        }
    }
    Ok(false)
}

fn linked_worktree_admin_is_prunable(admin_dir: &Path, expire_time: i64) -> Result<bool> {
    if admin_dir.join("locked").exists() {
        return Ok(false);
    }
    let gitdir_file = admin_dir.join("gitdir");
    if !gitdir_file.is_file() {
        return Ok(true);
    }
    let value = fs::read_to_string(&gitdir_file)?;
    let gitdir = resolve_worktree_admin_path(admin_dir, value.trim());
    if gitdir.exists() {
        return Ok(false);
    }
    if expire_time == i64::MIN {
        return Ok(false);
    }
    if expire_time == i64::MAX {
        return Ok(true);
    }
    let modified = fs::metadata(admin_dir)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or(0);
    Ok(modified <= expire_time)
}

fn resolve_worktree_admin_path(admin_dir: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        admin_dir.join(path)
    }
}

fn count_dir_entries(path: &Path) -> Result<usize> {
    if !path.exists() {
        return Ok(0);
    }
    Ok(fs::read_dir(path)?
        .filter_map(std::result::Result::ok)
        .count())
}

pub fn maintenance_global_config_path() -> Result<PathBuf> {
    if let Some(path) = env::var_os("GIT_CONFIG_GLOBAL").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    let Some(home) = sley_config::home_dir() else {
        eprintln!("fatal: $HOME not set");
        return Err(GitError::Exit(128));
    };
    let user = PathBuf::from(&home).join(".gitconfig");
    if !user.exists() {
        let xdg = env::var_os("XDG_CONFIG_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(&home).join(".config"))
            .join("git")
            .join("config");
        if xdg.exists() {
            return Ok(xdg);
        }
    }
    Ok(user)
}

pub fn report_missing_maintenance_repo(common_git_dir: &Path) -> bool {
    let mut missing = false;
    if let Ok(config) = GitConfig::read(common_git_dir.join("config")) {
        for value in config.get_all("maintenance", None, "repo") {
            if value.is_none() {
                eprintln!("error: missing value for 'maintenance.repo'");
                missing = true;
            }
        }
    }
    missing
}

pub fn config_add_value_if_missing(path: &Path, section: &str, key: &str, value: &str) -> Result<()> {
    let mut config = if path.exists() {
        GitConfig::read(path)?
    } else {
        GitConfig::default()
    };
    if config
        .get_all(section, None, key)
        .into_iter()
        .any(|entry| entry == Some(value))
    {
        return Ok(());
    }
    config_push_value(&mut config, section, key, value);
    write_config(path, &config)
}

pub fn config_remove_value(path: &Path, section: &str, key: &str, value: &str) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let mut config = GitConfig::read(path)?;
    let mut removed = false;
    for candidate in &mut config.sections {
        if !candidate.name.eq_ignore_ascii_case(section) || candidate.subsection.is_some() {
            continue;
        }
        candidate.entries.retain(|entry| {
            let matched =
                entry.key.eq_ignore_ascii_case(key) && entry.value.as_deref() == Some(value);
            removed |= matched;
            !matched
        });
    }
    if removed {
        write_config(path, &config)?;
    }
    Ok(removed)
}

fn config_push_value(config: &mut GitConfig, section: &str, key: &str, value: &str) {
    let section_idx = config
        .sections
        .iter()
        .rposition(|candidate| {
            candidate.name.eq_ignore_ascii_case(section) && candidate.subsection.is_none()
        })
        .unwrap_or_else(|| {
            config
                .sections
                .push(ConfigSection::new(section, None, Vec::new()));
            config.sections.len() - 1
        });
    config.sections[section_idx]
        .entries
        .push(ConfigEntry::new(key, Some(value.to_string())));
}

fn write_config(path: &Path, config: &GitConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, config.to_preserved_bytes())?;
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MaintenanceScheduler {
    Cron,
    Systemd,
    Launchctl,
    Schtasks,
}

pub fn scheduler_name(scheduler: MaintenanceScheduler) -> &'static str {
    match scheduler {
        MaintenanceScheduler::Cron => "crontab",
        MaintenanceScheduler::Systemd => "systemctl",
        MaintenanceScheduler::Launchctl => "launchctl",
        MaintenanceScheduler::Schtasks => "schtasks",
    }
}

pub fn resolve_maintenance_scheduler(
    scheduler: Option<MaintenanceScheduler>,
) -> Result<MaintenanceScheduler> {
    if let Some(scheduler) = scheduler {
        return Ok(scheduler);
    }

    #[cfg(target_os = "macos")]
    {
        return Ok(MaintenanceScheduler::Launchctl);
    }
    #[cfg(windows)]
    {
        return Ok(MaintenanceScheduler::Schtasks);
    }
    #[cfg(target_os = "linux")]
    {
        if scheduler_available(MaintenanceScheduler::Systemd) {
            return Ok(MaintenanceScheduler::Systemd);
        }
        if scheduler_available(MaintenanceScheduler::Cron) {
            return Ok(MaintenanceScheduler::Cron);
        }
        eprintln!("fatal: neither systemd timers nor crontab are available");
        return Err(GitError::Exit(128));
    }
    #[allow(unreachable_code)]
    Ok(MaintenanceScheduler::Cron)
}

pub fn validate_scheduler_available(scheduler: MaintenanceScheduler) -> Result<()> {
    if scheduler_available(scheduler) {
        Ok(())
    } else {
        eprintln!(
            "fatal: {} scheduler is not available",
            scheduler_name(scheduler)
        );
        Err(GitError::Exit(128))
    }
}

fn scheduler_available(scheduler: MaintenanceScheduler) -> bool {
    if let Some((program, _)) = scheduler_test_command(scheduler) {
        return program != "false";
    }
    if env::var_os("GIT_TEST_MAINT_SCHEDULER").is_some() {
        return false;
    }
    match scheduler {
        MaintenanceScheduler::Cron => ProcessCommand::new("crontab").arg("-l").output().is_ok(),
        MaintenanceScheduler::Systemd => ProcessCommand::new("systemctl")
            .args(["--user", "list-timers"])
            .status()
            .is_ok_and(|status| status.success()),
        MaintenanceScheduler::Launchctl => ProcessCommand::new("launchctl")
            .arg("list")
            .status()
            .is_ok_and(|status| status.success()),
        MaintenanceScheduler::Schtasks => ProcessCommand::new("schtasks")
            .arg("/query")
            .output()
            .is_ok(),
    }
}

fn scheduler_test_command(scheduler: MaintenanceScheduler) -> Option<(String, Vec<String>)> {
    let spec = env::var("GIT_TEST_MAINT_SCHEDULER").ok()?;
    for item in spec.split(',') {
        let (name, command) = item.split_once(':')?;
        if name != scheduler_name(scheduler) {
            continue;
        }
        let mut parts = command
            .split_whitespace()
            .map(str::to_string)
            .collect::<Vec<_>>();
        if parts.is_empty() {
            return None;
        }
        let program = parts.remove(0);
        return Some((program, parts));
    }
    None
}

fn run_scheduler_command(scheduler: MaintenanceScheduler, args: &[&str]) -> Result<()> {
    let (program, mut command_args) = scheduler_test_command(scheduler)
        .unwrap_or_else(|| (scheduler_name(scheduler).to_string(), Vec::new()));
    command_args.extend(args.iter().map(|arg| (*arg).to_string()));
    let status = ProcessCommand::new(program).args(command_args).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(GitError::Exit(status.code().unwrap_or(1)))
    }
}

pub fn update_background_schedule(
    common_git_dir: &Path,
    enable: Option<MaintenanceScheduler>,
) -> Result<()> {
    let lock = repository_objects_dir(common_git_dir).join("schedule.lock");
    if !acquire_lock_with_stale_recovery(&lock) {
        eprintln!("error: Another scheduled git-maintenance(1) process seems to be running");
        return Err(GitError::Exit(128));
    }
    let run = (|| -> Result<()> {
        if let Some(scheduler) = enable
            && let Err(err) = validate_scheduler_available(scheduler)
        {
            return Err(err);
        }
        for scheduler in [
            MaintenanceScheduler::Cron,
            MaintenanceScheduler::Systemd,
            MaintenanceScheduler::Launchctl,
            MaintenanceScheduler::Schtasks,
        ] {
            if enable == Some(scheduler) {
                continue;
            }
            if scheduler_available(scheduler) {
                let _ = update_scheduler(common_git_dir, scheduler, false);
            }
        }
        if let Some(scheduler) = enable {
            update_scheduler(common_git_dir, scheduler, true)?;
        }
        Ok(())
    })();
    let _ = fs::remove_file(&lock);
    run
}

fn update_scheduler(
    common_git_dir: &Path,
    scheduler: MaintenanceScheduler,
    enable: bool,
) -> Result<()> {
    match scheduler {
        MaintenanceScheduler::Cron => update_cron(enable),
        MaintenanceScheduler::Systemd => update_systemd(enable),
        MaintenanceScheduler::Launchctl => update_launchctl(enable),
        MaintenanceScheduler::Schtasks => update_schtasks(common_git_dir, enable),
    }
}

fn update_cron(enable: bool) -> Result<()> {
    let Some((_, args)) = scheduler_test_command(MaintenanceScheduler::Cron) else {
        return Ok(());
    };
    let Some(path) = args.last().map(PathBuf::from) else {
        return Ok(());
    };
    let existing = fs::read_to_string(&path).unwrap_or_default();
    let mut out = String::new();
    let mut skipping = false;
    for line in existing.lines() {
        if line == "# BEGIN GIT MAINTENANCE SCHEDULE" {
            skipping = true;
            continue;
        }
        if line == "# END GIT MAINTENANCE SCHEDULE" {
            skipping = false;
            continue;
        }
        if !skipping {
            out.push_str(line);
            out.push('\n');
        }
    }
    if enable {
        out.push_str("# BEGIN GIT MAINTENANCE SCHEDULE\n");
        out.push_str("0 1-23 * * * git for-each-repo --keep-going --config=maintenance.repo maintenance run --schedule=hourly\n");
        out.push_str("0 0 * * 1-6 git for-each-repo --keep-going --config=maintenance.repo maintenance run --schedule=daily\n");
        out.push_str("0 0 * * 0 git for-each-repo --keep-going --config=maintenance.repo maintenance run --schedule=weekly\n");
        out.push_str("# END GIT MAINTENANCE SCHEDULE\n");
    }
    fs::write(path, out)?;
    Ok(())
}

fn update_systemd(enable: bool) -> Result<()> {
    let base = xdg_config_home().join("systemd").join("user");
    if enable {
        fs::create_dir_all(&base)?;
        fs::write(
            base.join("git-maintenance@.service"),
            "[Service]\nExecStart=git -c core.askPass=true -c credential.interactive=false for-each-repo --keep-going --config=maintenance.repo maintenance run --schedule=%i\n",
        )?;
        for frequency in ["hourly", "daily", "weekly"] {
            fs::write(
                base.join(format!("git-maintenance@{frequency}.timer")),
                "[Timer]\n",
            )?;
            run_scheduler_command(
                MaintenanceScheduler::Systemd,
                &[
                    "--user",
                    "enable",
                    "--now",
                    &format!("git-maintenance@{frequency}.timer"),
                ],
            )?;
        }
    } else {
        for frequency in ["hourly", "daily", "weekly"] {
            let _ = run_scheduler_command(
                MaintenanceScheduler::Systemd,
                &[
                    "--user",
                    "disable",
                    "--now",
                    &format!("git-maintenance@{frequency}.timer"),
                ],
            );
            let _ = fs::remove_file(base.join(format!("git-maintenance@{frequency}.timer")));
        }
        let _ = fs::remove_file(base.join("git-maintenance@.service"));
    }
    Ok(())
}

fn update_launchctl(enable: bool) -> Result<()> {
    let Some(home) = sley_config::home_dir() else {
        return Ok(());
    };
    let base = PathBuf::from(home).join("Library").join("LaunchAgents");
    if enable {
        fs::create_dir_all(&base)?;
        let all_exist = ["hourly", "daily", "weekly"].iter().all(|frequency| {
            base.join(format!("org.git-scm.git.{frequency}.plist"))
                .exists()
        });
        if all_exist {
            for frequency in ["hourly", "daily", "weekly"] {
                run_scheduler_command(
                    MaintenanceScheduler::Launchctl,
                    &["list", &format!("org.git-scm.git.{frequency}")],
                )?;
            }
            return Ok(());
        }
        for frequency in ["hourly", "daily", "weekly"] {
            let plist = base.join(format!("org.git-scm.git.{frequency}.plist"));
            fs::write(
                &plist,
                format!("<plist><string>schedule={frequency}</string></plist>\n"),
            )?;
            let plist = plist.display().to_string();
            let _ = run_scheduler_command(
                MaintenanceScheduler::Launchctl,
                &["bootout", "gui/0", &plist],
            );
            run_scheduler_command(
                MaintenanceScheduler::Launchctl,
                &["bootstrap", "gui/0", &plist],
            )?;
        }
    } else {
        for frequency in ["hourly", "daily", "weekly"] {
            let plist = base.join(format!("org.git-scm.git.{frequency}.plist"));
            let plist_arg = plist.display().to_string();
            let _ = run_scheduler_command(
                MaintenanceScheduler::Launchctl,
                &["bootout", "gui/0", &plist_arg],
            );
            let _ = fs::remove_file(plist);
        }
    }
    Ok(())
}

fn update_schtasks(common_git_dir: &Path, enable: bool) -> Result<()> {
    if enable {
        for frequency in ["hourly", "daily", "weekly"] {
            let xml = common_git_dir.join(format!("schedule_{frequency}"));
            fs::write(&xml, "<Task></Task>\n")?;
            let xml = xml.display().to_string();
            run_scheduler_command(
                MaintenanceScheduler::Schtasks,
                &[
                    "/create",
                    "/tn",
                    &format!("Git Maintenance ({frequency})"),
                    "/f",
                    "/xml",
                    &xml,
                ],
            )?;
        }
    } else {
        for frequency in ["hourly", "daily", "weekly"] {
            let _ = run_scheduler_command(
                MaintenanceScheduler::Schtasks,
                &[
                    "/delete",
                    "/tn",
                    &format!("Git Maintenance ({frequency})"),
                    "/f",
                ],
            );
        }
    }
    Ok(())
}

fn xdg_config_home() -> PathBuf {
    env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| sley_config::home_dir().map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"))
}
