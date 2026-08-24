//! Extracted from the crate root (sley#8 phase 1) — code motion only.
#![allow(clippy::expect_used)]

use crate::*;
use sley::plumbing::{
    sley_config, sley_core, sley_diff_merge, sley_formats, sley_odb, sley_pack, sley_rev,
};

const COMMIT_GRAPH_USAGE: &str = "\
usage: git commit-graph verify [--object-dir <dir>] [--shallow] [--[no-]progress]
   or: git commit-graph write [--object-dir <dir>] [--append]
                       [--split[=<strategy>]] [--reachable | --stdin-packs | --stdin-commits]
                       [--changed-paths] [--[no-]max-new-filters <n>] [--[no-]progress]
                       <split-options>
";

pub(crate) fn cmd_commit_graph(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let Some(subcommand) = args.first().map(String::as_str) else {
        // No sub-command ⇒ usage error (exit 129) with the usage block.
        eprint!("{COMMIT_GRAPH_USAGE}");
        return Err(GitError::Exit(129));
    };
    match subcommand {
        "write" => cmd_commit_graph_write(cli_session, &args[1..]),
        "verify" => cmd_commit_graph_verify(cli_session, &args[1..]),
        other => {
            // Unknown sub-command ⇒ git's `error: unknown subcommand: \`<x>'`
            // plus the usage block, exit 129.
            eprintln!("error: unknown subcommand: `{other}'");
            eprint!("{COMMIT_GRAPH_USAGE}");
            Err(GitError::Exit(129))
        }
    }
}

/// Which set of commits seeds the graph (mirrors git's mutually-exclusive
/// `--reachable` / `--stdin-packs` / `--stdin-commits`; default = all packs).
enum CommitGraphSource {
    AllPacks,
    Reachable,
    StdinPacks,
    StdinCommits,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CommitGraphSplitMode {
    Off,
    Append,
    NoMerge,
    Replace,
}

#[derive(Clone, Copy)]
struct CommitGraphSplitOptions {
    mode: CommitGraphSplitMode,
    size_multiple: usize,
    max_commits: Option<usize>,
    expire_time: Option<i64>,
}

impl CommitGraphSplitOptions {
    fn off() -> Self {
        Self {
            mode: CommitGraphSplitMode::Off,
            size_multiple: 2,
            max_commits: None,
            expire_time: None,
        }
    }
}

fn cmd_commit_graph_write(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let cwd = cli_session.cwd();
    let git_dir = cli_session.git_dir()?;
    let format = repository_object_format(&git_dir)?;
    let mut object_dir: Option<PathBuf> = None;
    let mut source = CommitGraphSource::AllPacks;
    let mut changed_paths: Option<bool> = None;
    let mut append = false;
    let mut split = CommitGraphSplitOptions::off();
    let mut max_new_filters_arg: Option<usize> = None;
    // git's write progress defaults to isatty(2); the harness redirects stderr,
    // so only an explicit --progress emits the progress lines.
    let mut progress = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--reachable" => source = CommitGraphSource::Reachable,
            "--stdin-packs" => source = CommitGraphSource::StdinPacks,
            "--stdin-commits" => source = CommitGraphSource::StdinCommits,
            "--append" => append = true,
            "--split" => split.mode = CommitGraphSplitMode::Append,
            "--split=replace" => split.mode = CommitGraphSplitMode::Replace,
            "--changed-paths" => changed_paths = Some(true),
            "--no-changed-paths" => changed_paths = Some(false),
            "--max-commits" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--max-commits requires a value".into()))?;
                split.max_commits =
                    Some(commit_graph_parse_positive_usize(value, "--max-commits")?);
            }
            "--size-multiple" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--size-multiple requires a value".into()))?;
                split.size_multiple = commit_graph_parse_positive_usize(value, "--size-multiple")?;
            }
            "--expire-time" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--expire-time requires a value".into()))?;
                split.expire_time = Some(commit_graph_parse_expire_time(value)?);
            }
            "--max-new-filters" => {
                let value = iter.next().ok_or_else(|| {
                    GitError::Command("--max-new-filters requires a value".into())
                })?;
                max_new_filters_arg = Some(commit_graph_parse_max_new_filters(value)?);
            }
            "--no-max-new-filters" => max_new_filters_arg = None,
            "--object-dir" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--object-dir requires a value".into()))?;
                object_dir = Some(resolve_cli_path(cwd, value));
            }
            "--progress" => progress = true,
            "--no-progress" => progress = false,
            value if value.starts_with("--object-dir=") => {
                let value = value
                    .strip_prefix("--object-dir=")
                    .ok_or_else(|| GitError::Command("--object-dir requires a value".into()))?;
                object_dir = Some(resolve_cli_path(cwd, value));
            }
            value if value.starts_with("--split=") => {
                let strategy = value.strip_prefix("--split=").unwrap_or_default();
                split.mode = match strategy {
                    "replace" => CommitGraphSplitMode::Replace,
                    "no-merge" => CommitGraphSplitMode::NoMerge,
                    "merge-all" => CommitGraphSplitMode::Append,
                    _ => CommitGraphSplitMode::Append,
                };
            }
            value if value.starts_with("--max-commits=") => {
                split.max_commits = Some(commit_graph_parse_positive_usize(
                    value.strip_prefix("--max-commits=").unwrap_or_default(),
                    "--max-commits",
                )?);
            }
            value if value.starts_with("--size-multiple=") => {
                split.size_multiple = commit_graph_parse_positive_usize(
                    value.strip_prefix("--size-multiple=").unwrap_or_default(),
                    "--size-multiple",
                )?;
            }
            value if value.starts_with("--expire-time=") => {
                split.expire_time = Some(commit_graph_parse_expire_time(
                    value.strip_prefix("--expire-time=").unwrap_or_default(),
                )?);
            }
            value if value.starts_with("--max-new-filters=") => {
                max_new_filters_arg = Some(commit_graph_parse_max_new_filters(
                    value.strip_prefix("--max-new-filters=").unwrap_or_default(),
                )?);
            }
            // Any unrecognized option or positional arg is a usage error
            // (git's parse-options exits 129); `commit-graph write` takes no
            // positional arguments.
            other => {
                eprintln!("error: unknown option `{}'", other.trim_start_matches('-'));
                eprint!("{COMMIT_GRAPH_USAGE}");
                return Err(GitError::Exit(129));
            }
        }
    }
    let object_dir = object_dir.unwrap_or_else(|| repository_objects_dir(&git_dir));
    // Read config with the command-line `-c` overrides folded in (mirrors git),
    // so `-c commitGraph.generationVersion=1` / `-c commitGraph.changedPaths=…`
    // on the write invocation take effect.
    let repo_config =
        sley_config::read_repo_config(&git_dir, effective_config_parameters_env().as_deref()).ok();
    // The two numeric keys die through `die_bad_number`, whose diagnostic names
    // the value's source file; look them up through an origin-carrying stack
    // (same layering as the plain config above).
    let origin_stack = commit_graph_origin_stack(cli_session, &git_dir);
    // git's `commit_graph_compatible()`: grafts and shallow boundaries make a
    // graph lie about parents. `cmd_commit_graph` also calls
    // `disable_replace_refs()`, so replace refs are intentionally *not*
    // checked here — only grafts/shallow.
    if !commit_graph_write_compatible(&git_dir, format) {
        return Ok(());
    }
    let changed_paths_version = commit_graph_changed_paths_version(origin_stack.as_ref())?;
    if !(-1..=2).contains(&changed_paths_version) {
        eprintln!(
            "warning: attempting to write a commit-graph, but 'commitGraph.changedPathsVersion' ({changed_paths_version}) is not supported"
        );
        return Ok(());
    }
    let existing_bloom_settings = existing_commit_graph_bloom_settings(&object_dir, format)?;
    let bloom_settings =
        commit_graph_bloom_settings_for_write(existing_bloom_settings, changed_paths_version, true);
    // git: write_generation_data = (get_configured_generation_version(r) == 2).
    // Default is 2; `commitGraph.generationVersion=1` omits the GDA2/GDO2 chunks.
    let write_generation_data = commit_graph_generation_version(origin_stack.as_ref())? == 2;
    let changed_paths = changed_paths.unwrap_or_else(|| {
        repo_config
            .as_ref()
            .and_then(|config| config.get_bool("commitGraph", None, "changedPaths"))
            .unwrap_or(false)
            || existing_bloom_settings.is_some()
    });
    let max_new_filters = max_new_filters_arg.or_else(|| {
        repo_config
            .as_ref()
            .and_then(|config| config.get("commitGraph", None, "maxNewFilters"))
            .and_then(|value| commit_graph_parse_max_new_filters(value).ok())
    });
    let existing_filters = if changed_paths {
        existing_commit_graph_bloom_filters(&object_dir, format, bloom_settings)?
    } else {
        HashMap::new()
    };

    let db = FileObjectDatabase::new(&object_dir, format);
    let starts = match source {
        CommitGraphSource::Reachable => {
            return write_reachable_commit_graph(
                &git_dir,
                &db,
                &object_dir,
                format,
                changed_paths,
                bloom_settings,
                write_generation_data,
                max_new_filters,
                &existing_filters,
                split,
                progress,
                repo_config.as_ref(),
                cli_session.replace_objects(),
            );
        }
        CommitGraphSource::AllPacks => commit_graph_packed_commit_starts(&db, &object_dir, format)?,
        CommitGraphSource::StdinPacks => {
            commit_graph_stdin_packs_starts(cwd, &db, &object_dir, format)?
        }
        CommitGraphSource::StdinCommits => {
            let starts = commit_graph_stdin_commits_starts(&db, format)?;
            // git's `read_one_commit` loop drives a "Collecting commits from
            // input" progress meter while reading the stdin oids.
            if progress {
                eprintln!("Collecting commits from input: {}, done.", starts.len());
            }
            starts
        }
    };

    let mut starts = starts;
    if append {
        // `--append`: keep the commits already in the graph and add the new
        // source on top (git's `COMMIT_GRAPH_WRITE_APPEND`).
        let mut seen: HashSet<ObjectId> = starts.iter().copied().collect();
        for oid in existing_commit_graph_oids(&object_dir, format)? {
            if seen.insert(oid) {
                starts.push(oid);
            }
        }
    }

    // No commits in scope ⇒ write no graph file (git's "write graph with no
    // packs": the file must stay absent).
    if starts.is_empty() {
        return Ok(());
    }
    let graph = commit_graph_from_starts(
        &db,
        format,
        starts,
        changed_paths,
        bloom_settings,
        write_generation_data,
        max_new_filters,
        &existing_filters,
        progress,
    )?;
    let graph_dir = object_dir.join("info");
    fs::create_dir_all(&graph_dir)?;
    let shared_read_mode = commit_graph_shared_read_mode(repo_config.as_ref());
    let split_layers_owner_writable = !commit_graph_has_shared_repository(repo_config.as_ref());
    if split.mode == CommitGraphSplitMode::Off {
        write_commit_graph_file(&graph_dir.join("commit-graph"), &graph, shared_read_mode)?;
        remove_split_commit_graphs(&object_dir)?;
    } else {
        write_split_commit_graph_file(
            &object_dir,
            format,
            &graph,
            split,
            shared_read_mode,
            split_layers_owner_writable,
        )?;
    }
    Ok(())
}

/// The commit oids already recorded in the existing single-file commit-graph
/// (empty when there is none). Used by `--append`.
fn existing_commit_graph_oids(object_dir: &Path, format: ObjectFormat) -> Result<Vec<ObjectId>> {
    let graph_path = object_dir.join("info").join("commit-graph");
    if !graph_path.exists() {
        return Ok(Vec::new());
    }
    let graph = CommitGraph::parse(&fs::read(graph_path)?, format)?;
    Ok(graph.commits.into_iter().map(|entry| entry.oid).collect())
}

/// Write the commit-graph file with git's read-only mode `0444 & ~umask`,
/// matching `mks_tempfile_m(..., 0444)` + `adjust_shared_perm`.
///
/// The umask is derived (without `unsafe`/libc) from the just-created file: the
/// OS gives it `0666 & ~umask`, so its read bits (`& 0444`) equal `0444 &
/// ~umask` exactly — which is the mode git lands on.
fn write_commit_graph_file(path: &Path, bytes: &[u8], shared_read_mode: u32) -> Result<()> {
    // A prior graph is written read-only (and a corrupted-graph test may leave
    // it chmod-000); make it writable first so the remove always succeeds, then
    // remove it so the rewrite creates a fresh file with the OS default mode
    // (`0666 & ~umask`), from which the umask can be recovered below. git
    // unconditionally replaces the graph regardless of the old file's mode.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if path.exists() {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
        }
    }
    let _ = fs::remove_file(path);
    fs::write(path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // The graph is read-only; `core.sharedRepository` narrows which classes
        // keep the read bit (git's adjust_shared_perm). The umask is folded in
        // via the freshly-created file's mode so the default (no sharedRepository
        // → 0o444) still tracks it.
        let created_mode = fs::metadata(path)?.permissions().mode();
        let mode = created_mode & shared_read_mode;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = shared_read_mode;
    Ok(())
}

/// The read-only file mode a freshly-written commit-graph should carry, after
/// applying `core.sharedRepository`. Defaults to `0o444` (all-read); a
/// configured numeric/symbolic mode drops the group/other read bit for any
/// class the mode grants no access (mirrors git's `adjust_shared_perm`).
fn commit_graph_shared_read_mode(config: Option<&sley_config::GitConfig>) -> u32 {
    const BASE: u32 = 0o444;
    let Some(config) = config else {
        return BASE;
    };
    let Some(value) = config.get("core", None, "sharedRepository") else {
        return BASE;
    };
    let mode: u32 = match value.trim().to_ascii_lowercase().as_str() {
        "umask" | "false" | "no" | "off" | "0" | "" => return BASE,
        "group" | "true" | "yes" | "on" | "1" => 0o660,
        "all" | "world" | "everybody" | "2" => 0o664,
        other => match u32::from_str_radix(other, 8) {
            Ok(parsed) if parsed != 0 => parsed,
            _ => return BASE,
        },
    };
    let mut read = 0o400;
    if mode & 0o070 != 0 {
        read |= 0o040;
    }
    if mode & 0o007 != 0 {
        read |= 0o004;
    }
    read
}

fn commit_graph_has_shared_repository(config: Option<&sley_config::GitConfig>) -> bool {
    config.is_some_and(|config| config.get("core", None, "sharedRepository").is_some())
}

fn commit_graph_parse_max_new_filters(value: &str) -> Result<usize> {
    value.parse::<usize>().map_err(|_| {
        GitError::Command(format!(
            "bad numeric value '{value}' for '--max-new-filters'"
        ))
    })
}

fn commit_graph_parse_positive_usize(value: &str, option: &str) -> Result<usize> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| GitError::Command(format!("bad numeric value '{value}' for '{option}'")))?;
    if parsed == 0 {
        return Err(GitError::Command(format!(
            "bad numeric value '{value}' for '{option}'"
        )));
    }
    Ok(parsed)
}

fn commit_graph_parse_expire_time(value: &str) -> Result<i64> {
    crate::commands::approxidate::parse_expiry_date(value)
        .or_else(|| crate::commands::approxidate::parse_approxidate(value))
        .ok_or_else(|| GitError::Command(format!("invalid date format: {value}")))
}

struct CommitGraphLayer {
    hash: ObjectId,
    graph: CommitGraph,
}

fn write_split_commit_graph_file(
    object_dir: &Path,
    format: ObjectFormat,
    graph: &[u8],
    options: CommitGraphSplitOptions,
    shared_read_mode: u32,
    split_layers_owner_writable: bool,
) -> Result<()> {
    let info = object_dir.join("info");
    let graphs = info.join("commit-graphs");
    fs::create_dir_all(&graphs)?;
    let single = info.join("commit-graph");
    let chain_path = graphs.join("commit-graph-chain");
    let full_graph = CommitGraph::parse(graph, format)?;
    let mut layers = if options.mode == CommitGraphSplitMode::Replace {
        Vec::new()
    } else {
        load_commit_graph_layers(object_dir, format)?
    };
    if layers.is_empty() && options.mode != CommitGraphSplitMode::Replace && single.exists() {
        let bytes = fs::read(&single)?;
        let hash = graph_file_checksum(&bytes, format)?;
        let path = graphs.join(format!("graph-{hash}.graph"));
        if !path.exists() {
            write_commit_graph_layer_file(
                &path,
                &bytes,
                shared_read_mode,
                split_layers_owner_writable,
            )?;
        }
        layers.push(CommitGraphLayer {
            hash,
            graph: CommitGraph::parse(&bytes, format)?,
        });
    }

    let existing_oids = layers
        .iter()
        .flat_map(|layer| layer.graph.commits.iter().map(|entry| entry.oid))
        .collect::<HashSet<_>>();
    let mut new_entries = commit_graph_write_entries_from_graph(&full_graph)?
        .into_iter()
        .filter(|entry| {
            options.mode == CommitGraphSplitMode::Replace || !existing_oids.contains(&entry.oid)
        })
        .collect::<Vec<_>>();
    if new_entries.is_empty() && options.mode != CommitGraphSplitMode::Replace {
        return Ok(());
    }

    if options.mode == CommitGraphSplitMode::Append {
        let mut new_count = new_entries.len();
        while let Some(top) = layers.last() {
            let force_by_max = options
                .max_commits
                .is_some_and(|max_commits| new_count > max_commits);
            let merge_by_size =
                top.graph.commits.len() <= options.size_multiple.saturating_mul(new_count);
            if !(force_by_max || merge_by_size) {
                break;
            }
            let top = layers.pop().expect("checked last layer");
            new_count = new_count.saturating_add(top.graph.commits.len());
            let base_oids = layers
                .iter()
                .flat_map(|layer| layer.graph.commits.iter().map(|entry| entry.oid))
                .collect::<Vec<_>>();
            new_entries.extend(commit_graph_write_entries_from_graph_with_base(
                &top.graph, &base_oids,
            )?);
        }
    }

    let base_hashes = layers.iter().map(|layer| layer.hash).collect::<Vec<_>>();
    let base_oids = layers
        .iter()
        .flat_map(|layer| layer.graph.commits.iter().map(|entry| entry.oid))
        .collect::<Vec<_>>();
    let write_generation_data = if let Some(top) = layers.last() {
        commit_graph_has_chunk(&top.graph, *b"GDA2")
    } else {
        commit_graph_has_chunk(&full_graph, *b"GDA2")
    };
    let bloom_settings = full_graph
        .bloom_filters
        .as_ref()
        .map(|filters| sley_formats::CommitGraphBloomSettings {
            hash_version: filters.hash_version,
            hash_count: filters.hash_count,
            bits_per_entry: filters.bits_per_entry,
            max_changed_paths: sley_formats::DEFAULT_COMMIT_GRAPH_BLOOM_SETTINGS.max_changed_paths,
        })
        .unwrap_or(sley_formats::DEFAULT_COMMIT_GRAPH_BLOOM_SETTINGS);
    let graph = CommitGraph::write_with_base_options(
        format,
        &new_entries,
        bloom_settings,
        write_generation_data,
        &base_hashes,
        &base_oids,
    )?;
    let hash = graph_file_checksum(&graph, format)?;
    let graph_path = graphs.join(format!("graph-{hash}.graph"));
    write_commit_graph_layer_file(
        &graph_path,
        &graph,
        shared_read_mode,
        split_layers_owner_writable,
    )?;

    let mut chain = base_hashes;
    chain.push(hash);
    let mut chain_text = String::new();
    for hash in &chain {
        chain_text.push_str(&hash.to_hex());
        chain_text.push('\n');
    }
    write_commit_graph_file(&chain_path, chain_text.as_bytes(), shared_read_mode)?;
    if single.exists() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&single, fs::Permissions::from_mode(0o600));
        }
        let _ = fs::remove_file(&single);
    }
    expire_split_commit_graphs(&graphs, &chain, options.expire_time)?;
    Ok(())
}

fn write_commit_graph_layer_file(
    path: &Path,
    bytes: &[u8],
    shared_read_mode: u32,
    owner_writable: bool,
) -> Result<()> {
    write_commit_graph_file(path, bytes, shared_read_mode)?;
    #[cfg(unix)]
    if owner_writable {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(path)?.permissions().mode() | 0o200;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = owner_writable;
    Ok(())
}

fn load_commit_graph_layers(
    object_dir: &Path,
    format: ObjectFormat,
) -> Result<Vec<CommitGraphLayer>> {
    let local_chain = object_dir
        .join("info")
        .join("commit-graphs")
        .join("commit-graph-chain");
    let mut hashes = read_commit_graph_chain_hashes(&local_chain, format)?;
    if hashes.is_empty() {
        for alternate in commit_graph_alternate_object_dirs(object_dir)? {
            let alternate_chain = alternate
                .join("info")
                .join("commit-graphs")
                .join("commit-graph-chain");
            hashes = read_commit_graph_chain_hashes(&alternate_chain, format)?;
            if !hashes.is_empty() {
                break;
            }
        }
    }
    let mut layers = Vec::with_capacity(hashes.len());
    for hash in hashes {
        let bytes = fs::read(commit_graph_layer_path(object_dir, &hash)?)?;
        let graph = CommitGraph::parse(&bytes, format)?;
        layers.push(CommitGraphLayer { hash, graph });
    }
    Ok(layers)
}

fn commit_graph_layer_path(object_dir: &Path, hash: &ObjectId) -> Result<PathBuf> {
    let local = object_dir
        .join("info")
        .join("commit-graphs")
        .join(format!("graph-{hash}.graph"));
    if local.exists() {
        return Ok(local);
    }
    for alternate in commit_graph_alternate_object_dirs(object_dir)? {
        let path = alternate
            .join("info")
            .join("commit-graphs")
            .join(format!("graph-{hash}.graph"));
        if path.exists() {
            return Ok(path);
        }
    }
    Err(GitError::InvalidPath(format!(
        "missing commit-graph layer graph-{hash}.graph"
    )))
}

fn commit_graph_alternate_object_dirs(object_dir: &Path) -> Result<Vec<PathBuf>> {
    let alternates = object_dir.join("info").join("alternates");
    let contents = match fs::read_to_string(&alternates) {
        Ok(contents) => contents,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(GitError::Io(err.to_string())),
    };
    let base = alternates.parent().unwrap_or(object_dir);
    Ok(contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let path = PathBuf::from(line);
            if path.is_absolute() {
                path
            } else {
                base.join(path)
            }
        })
        .collect())
}

fn commit_graph_write_entries_from_graph(
    graph: &CommitGraph,
) -> Result<Vec<CommitGraphWriteEntry>> {
    commit_graph_write_entries_from_graph_with_base(graph, &[])
}

fn commit_graph_write_entries_from_graph_with_base(
    graph: &CommitGraph,
    base_oids: &[ObjectId],
) -> Result<Vec<CommitGraphWriteEntry>> {
    let mut entries = Vec::with_capacity(graph.commits.len());
    for (idx, entry) in graph.commits.iter().enumerate() {
        let parents = entry
            .parents
            .iter()
            .map(|parent| {
                let parent = *parent as usize;
                if parent < base_oids.len() {
                    Ok(base_oids[parent])
                } else {
                    let local = parent - base_oids.len();
                    graph
                        .commits
                        .get(local)
                        .map(|entry| entry.oid)
                        .ok_or_else(|| {
                            GitError::InvalidFormat(
                                "commit-graph parent points past commit table".into(),
                            )
                        })
                }
            })
            .collect::<Result<Vec<_>>>()?;
        let bloom_filter = graph
            .bloom_filters
            .as_ref()
            .and_then(|filters| filters.filter_for_commit(idx).map(|filter| filter.to_vec()));
        entries.push(CommitGraphWriteEntry {
            oid: entry.oid,
            tree: entry.tree,
            parents,
            generation: entry.generation,
            commit_time: entry.commit_time,
            bloom_filter,
        });
    }
    Ok(entries)
}

fn commit_graph_has_chunk(graph: &CommitGraph, id: [u8; 4]) -> bool {
    graph.chunks.iter().any(|chunk| chunk.id == id)
}

fn expire_split_commit_graphs(
    graphs: &Path,
    chain: &[ObjectId],
    expire_time: Option<i64>,
) -> Result<()> {
    let expire_time = expire_time.unwrap_or_else(current_unix_seconds);
    let keep = chain
        .iter()
        .map(|hash| format!("graph-{hash}.graph"))
        .collect::<HashSet<_>>();
    let Ok(entries) = fs::read_dir(graphs) else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.ends_with(".graph") || keep.contains(name) {
            continue;
        }
        let modified = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|mtime| mtime.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or(i64::MIN);
        if modified <= expire_time {
            let _ = fs::remove_file(path);
        }
    }
    Ok(())
}

fn remove_split_commit_graphs(object_dir: &Path) -> Result<()> {
    let graphs = object_dir.join("info").join("commit-graphs");
    let chain = graphs.join("commit-graph-chain");
    let _ = fs::remove_file(chain);
    let Ok(entries) = fs::read_dir(&graphs) else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("graph") {
            let _ = fs::remove_file(path);
        }
    }
    Ok(())
}

fn read_commit_graph_chain_hashes(path: &Path, format: ObjectFormat) -> Result<Vec<ObjectId>> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(GitError::Io(err.to_string())),
    };
    contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| ObjectId::from_hex(format, line.trim()))
        .collect()
}

fn graph_file_checksum(bytes: &[u8], format: ObjectFormat) -> Result<ObjectId> {
    let raw_len = format.raw_len();
    if bytes.len() < raw_len {
        return Err(GitError::InvalidFormat(
            "commit-graph file too short".into(),
        ));
    }
    ObjectId::from_raw(format, &bytes[bytes.len() - raw_len..])
}

/// `--reachable`: write the graph seeded from refs + HEAD. Always writes a file
/// (matching git, which produces a header-only graph for an empty repo).
fn write_reachable_commit_graph(
    git_dir: &Path,
    db: &FileObjectDatabase,
    object_dir: &Path,
    format: ObjectFormat,
    changed_paths: bool,
    bloom_settings: sley_formats::CommitGraphBloomSettings,
    write_generation_data: bool,
    max_new_filters: Option<usize>,
    existing_filters: &HashMap<ObjectId, CommitGraphExistingBloomFilter>,
    split: CommitGraphSplitOptions,
    progress: bool,
    repo_config: Option<&sley_config::GitConfig>,
    replace_objects: bool,
) -> Result<()> {
    let graph = commit_graph_for_reachable_refs(
        git_dir,
        db,
        format,
        changed_paths,
        bloom_settings,
        write_generation_data,
        max_new_filters,
        existing_filters,
        progress,
        replace_objects,
    )?;
    let graph_dir = object_dir.join("info");
    fs::create_dir_all(&graph_dir)?;
    let shared_read_mode = commit_graph_shared_read_mode(repo_config);
    let split_layers_owner_writable = !commit_graph_has_shared_repository(repo_config);
    if split.mode == CommitGraphSplitMode::Off {
        write_commit_graph_file(&graph_dir.join("commit-graph"), &graph, shared_read_mode)?;
        remove_split_commit_graphs(object_dir)?;
    } else {
        write_split_commit_graph_file(
            object_dir,
            format,
            &graph,
            split,
            shared_read_mode,
            split_layers_owner_writable,
        )?;
    }
    Ok(())
}

/// Seed commits for the default (all-packs) write: every commit object found in
/// the object dir's packs (git's `fill_oids_from_all_packs`).
fn commit_graph_packed_commit_starts(
    db: &FileObjectDatabase,
    object_dir: &Path,
    format: ObjectFormat,
) -> Result<Vec<ObjectId>> {
    let mut starts = Vec::new();
    let mut seen = HashSet::new();
    for oid in sley_odb::packed_object_ids(object_dir, format)? {
        let Ok(object) = db.read_object(&oid) else {
            continue;
        };
        if object.object_type == ObjectType::Commit && seen.insert(oid) {
            starts.push(oid);
        }
    }
    Ok(starts)
}

/// `--stdin-packs`: read pack index paths from stdin and seed from the commits
/// in those packs. A missing/invalid pack is a fatal "error adding pack".
fn commit_graph_stdin_packs_starts(
    cwd: &Path,
    db: &FileObjectDatabase,
    object_dir: &Path,
    format: ObjectFormat,
) -> Result<Vec<ObjectId>> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let mut starts = Vec::new();
    let mut seen = HashSet::new();
    for line in input.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let pack_path = resolve_cli_path(cwd, line);
        // git resolves the named pack relative to the object dir's pack/ dir
        // when it is not an absolute existing path.
        let candidates = [pack_path.clone(), object_dir.join("pack").join(line)];
        let resolved = candidates.iter().find(|path| path.exists());
        let Some(resolved) = resolved else {
            eprintln!("error: error adding pack {line}");
            return Err(GitError::Exit(1));
        };
        let oids = commit_graph_commit_oids_in_pack(db, resolved, format).map_err(|_| {
            eprintln!("error: error adding pack {line}");
            GitError::Exit(1)
        })?;
        for oid in oids {
            if seen.insert(oid) {
                starts.push(oid);
            }
        }
    }
    Ok(starts)
}

/// Commit oids contained in a single pack, addressed by its `.idx` or `.pack`
/// path.
fn commit_graph_commit_oids_in_pack(
    db: &FileObjectDatabase,
    pack_path: &Path,
    format: ObjectFormat,
) -> Result<Vec<ObjectId>> {
    let idx_path = if pack_path.extension().and_then(|ext| ext.to_str()) == Some("pack") {
        pack_path.with_extension("idx")
    } else {
        pack_path.to_path_buf()
    };
    let index_bytes = fs::read(&idx_path)?;
    let index = sley_pack::PackIndex::parse(&index_bytes, format)?;
    let mut oids = Vec::new();
    for entry in index.entries {
        if let Ok(object) = db.read_object(&entry.oid)
            && object.object_type == ObjectType::Commit
        {
            oids.push(entry.oid);
        }
    }
    Ok(oids)
}

/// `--stdin-commits`: read commit oids from stdin (each must be hex and resolve
/// to an existing object), seed the closure from them. git's diagnostics:
/// "unexpected non-hex object ID: <s>" and "invalid object <oid>".
fn commit_graph_stdin_commits_starts(
    db: &FileObjectDatabase,
    format: ObjectFormat,
) -> Result<Vec<ObjectId>> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let mut starts = Vec::new();
    let mut seen = HashSet::new();
    for line in input.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(oid) = ObjectId::from_hex(format, line) else {
            eprintln!("error: unexpected non-hex object ID: {line}");
            return Err(GitError::Exit(1));
        };
        let Ok(_object) = db.read_object(&oid) else {
            eprintln!("error: invalid object {line}");
            return Err(GitError::Exit(1));
        };
        // Peel annotated tags down to the commit they reference; non-commit
        // tree-ish (e.g. a tree oid) is silently skipped, matching git, which
        // only graphs commit objects. Dedup on the peeled commit so two tags
        // pointing at the same commit contribute one start.
        if let Ok(commit) = sley_rev::peel_to_commit(db, format, &oid)
            && seen.insert(commit)
        {
            starts.push(commit);
        }
    }
    Ok(starts)
}

fn cmd_commit_graph_verify(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let cwd = cli_session.cwd();
    let git_dir = cli_session.git_dir()?;
    let format = repository_object_format(&git_dir)?;
    let mut object_dir: Option<PathBuf> = None;
    // git: opts.progress defaults to isatty(2); --progress forces on,
    // --no-progress forces off. Under the test harness stderr is redirected, so
    // the default is off; only an explicit --progress emits the progress line.
    let mut progress = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--object-dir" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("--object-dir requires a value".into()))?;
                object_dir = Some(resolve_cli_path(cwd, value));
            }
            "--progress" => progress = true,
            "--no-progress" => progress = false,
            "--shallow" => {}
            value if value.starts_with("--object-dir=") => {
                let value = value
                    .strip_prefix("--object-dir=")
                    .ok_or_else(|| GitError::Command("--object-dir requires a value".into()))?;
                object_dir = Some(resolve_cli_path(cwd, value));
            }
            other => {
                return Err(GitError::Unsupported(format!(
                    "commit-graph verify option {other}"
                )));
            }
        }
    }
    let object_dir = object_dir.unwrap_or_else(|| repository_objects_dir(&git_dir));
    let graph_path = object_dir.join("info").join("commit-graph");
    // git's `cmd_commit_graph_verify` prefers the single-file graph; only if it
    // is absent (ENOENT) does it fall back to the chain. A graph that exists but
    // cannot be opened (e.g. permissions) is a fatal `Could not open` error.
    match open_commit_graph_bytes(&graph_path) {
        OpenResult::Bytes(bytes) => {
            return verify_commit_graph_bytes(&object_dir, format, &bytes, progress);
        }
        OpenResult::OpenError => {
            // git: die_errno("Could not open commit-graph '%s'") ⇒ exit 128.
            eprintln!(
                "fatal: Could not open commit-graph '{}'",
                graph_path.display()
            );
            return Err(GitError::Exit(128));
        }
        OpenResult::NotFound => {}
    }
    let chain_path = object_dir
        .join("info")
        .join("commit-graphs")
        .join("commit-graph-chain");
    if chain_path.exists() {
        return verify_split_commit_graph_chain(&chain_path, format);
    }
    // No commit-graph at all is not an error (git's `commit-graph verify`
    // exits 0 when there is nothing to verify).
    Ok(())
}

/// Outcome of trying to open + read the single-file commit-graph, mirroring
/// git's `open_commit_graph` (which distinguishes ENOENT from other errno).
pub(super) enum OpenResult {
    Bytes(Vec<u8>),
    /// The path does not exist (ENOENT) — fall through to the chain.
    NotFound,
    /// The path exists but could not be read (e.g. permission denied).
    OpenError,
}

pub(super) fn open_commit_graph_bytes(path: &Path) -> OpenResult {
    match fs::read(path) {
        Ok(bytes) => OpenResult::Bytes(bytes),
        Err(err) if err.kind() == io::ErrorKind::NotFound => OpenResult::NotFound,
        Err(_) => OpenResult::OpenError,
    }
}

fn verify_split_commit_graph_chain(chain_path: &Path, format: ObjectFormat) -> Result<()> {
    let chain_dir = chain_path
        .parent()
        .ok_or_else(|| GitError::InvalidPath("commit-graph chain path has no parent".into()))?;
    let chain_bytes = fs::read(chain_path)?;
    if chain_bytes.len() < format.hex_len() {
        eprintln!("error: commit-graph chain file too small");
        return Err(GitError::Exit(1));
    }
    let text = std::str::from_utf8(&chain_bytes)
        .map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    let mut graph_hashes = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        if line.len() != format.hex_len() || !line.as_bytes().iter().all(u8::is_ascii_hexdigit) {
            eprintln!("error: invalid commit-graph chain");
            return Err(GitError::Exit(1));
        }
        graph_hashes.push(ObjectId::from_hex(format, line)?);
    }
    if graph_hashes.is_empty() {
        eprintln!("error: commit-graph chain file too small");
        return Err(GitError::Exit(1));
    }
    for (idx, expected_hash) in graph_hashes.iter().enumerate() {
        let graph_path = chain_dir.join(format!("graph-{expected_hash}.graph"));
        let graph_bytes = match fs::read(&graph_path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                eprintln!("error: unable to find all commit-graph files");
                return Err(GitError::Exit(1));
            }
            Err(err) => return Err(GitError::Io(err.to_string())),
        };
        let graph = match CommitGraph::parse(&graph_bytes, format) {
            Ok(graph) => graph,
            Err(_) => {
                eprintln!("error: commit-graph file is too small");
                eprintln!("error: incorrect checksum");
                eprintln!("error: commit-graph chain does not match");
                return Err(GitError::Exit(1));
            }
        };
        if &graph.checksum != expected_hash {
            eprintln!("error: incorrect checksum");
            eprintln!("error: commit-graph chain does not match");
            return Err(GitError::Exit(1));
        }
        if graph.base_graph_count as usize != graph.base_graphs.len() {
            eprintln!("error: commit-graph chain does not match");
            return Err(GitError::Exit(1));
        }
        if graph.base_graph_count as usize > idx {
            eprintln!("error: commit-graph chain does not match");
            return Err(GitError::Exit(1));
        }
        if !graph.base_graphs.is_empty() {
            let expected_bases = &graph_hashes[idx - graph.base_graphs.len()..idx];
            if graph.base_graphs != expected_bases {
                eprintln!("error: commit-graph chain does not match");
                return Err(GitError::Exit(1));
            }
        }
    }
    Ok(())
}

// === commit-graph verify ====================================================
//
// A byte-faithful reimplementation of git's `verify_commit_graph` /
// `verify_one_commit_graph` (commit-graph.c) + the structural checks in
// `parse_commit_graph` / `read_table_of_contents`. It re-parses the on-disk
// graph from raw bytes (independent of `CommitGraph::parse`) so the exact
// validation order and error strings match git's, and cross-checks every commit
// against the object database. Each detected problem is reported with git's
// exact `error:`/`fatal:` text; the command exits non-zero when any check fails.

const GRAPH_HEADER_SIZE: usize = 8;
const GRAPH_CHUNK_TOC_ENTRY_SIZE: usize = 12;
const GRAPH_FANOUT_SIZE: usize = 4 * 256;
const GRAPH_SIGNATURE: u32 = 0x4347_5048; // "CGPH"
const GRAPH_VERSION: u8 = 1;
const GRAPH_PARENT_NONE: u32 = 0x7000_0000;
const GRAPH_EXTRA_EDGES_NEEDED: u32 = 0x8000_0000;
const GRAPH_EDGE_LAST_MASK: u32 = 0x7fff_ffff;
const GRAPH_LAST_EDGE: u32 = 0x8000_0000;
const GENERATION_NUMBER_V1_MAX: u64 = 0x3fff_ffff;

const CHUNK_OIDF: [u8; 4] = *b"OIDF";
const CHUNK_OIDL: [u8; 4] = *b"OIDL";
const CHUNK_CDAT: [u8; 4] = *b"CDAT";
const CHUNK_EDGE: [u8; 4] = *b"EDGE";

fn graph_min_size(hash_len: usize) -> usize {
    GRAPH_HEADER_SIZE + 4 * GRAPH_CHUNK_TOC_ENTRY_SIZE + GRAPH_FANOUT_SIZE + hash_len
}

fn graph_data_width(hash_len: usize) -> usize {
    hash_len + 16
}

fn read_be32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn read_be64(bytes: &[u8], offset: usize) -> u64 {
    let mut value = 0u64;
    for &byte in &bytes[offset..offset + 8] {
        value = (value << 8) | u64::from(byte);
    }
    value
}

/// A parsed-but-unvalidated view of a chunk's byte range within the graph.
struct GraphChunk {
    id: [u8; 4],
    start: usize,
    size: usize,
}

/// The chunks + header fields needed for verification, parsed straight from raw
/// bytes (mirrors git's `parse_commit_graph`). Returns `Err(Exit)` after
/// printing the matching `error:` line when the graph cannot be parsed; the
/// command then exits 1 — exactly git's `if (!graph) return 1;`.
struct ParsedGraph<'a> {
    bytes: &'a [u8],
    format: ObjectFormat,
    hash_len: usize,
    num_commits: u32,
    oid_fanout: usize,
    oid_lookup: usize,
    commit_data: usize,
    extra_edges: Option<(usize, usize)>,
}

fn parse_commit_graph_for_verify<'a>(
    bytes: &'a [u8],
    format: ObjectFormat,
) -> std::result::Result<ParsedGraph<'a>, GitError> {
    let hash_len = format.raw_len();

    if bytes.len() < graph_min_size(hash_len) {
        eprintln!("error: commit-graph file is too small");
        return Err(GitError::Exit(1));
    }

    let signature = read_be32(bytes, 0);
    if signature != GRAPH_SIGNATURE {
        eprintln!(
            "error: commit-graph signature {signature:X} does not match signature {GRAPH_SIGNATURE:X}"
        );
        return Err(GitError::Exit(1));
    }

    let version = bytes[4];
    if version != GRAPH_VERSION {
        eprintln!(
            "error: commit-graph version {version:X} does not match version {GRAPH_VERSION:X}"
        );
        return Err(GitError::Exit(1));
    }

    let hash_version = bytes[5];
    let expected_hash_version = match format {
        ObjectFormat::Sha1 => 1u8,
        ObjectFormat::Sha256 => 2u8,
    };
    if hash_version != expected_hash_version {
        eprintln!(
            "error: commit-graph hash version {hash_version:X} does not match version {expected_hash_version:X}"
        );
        return Err(GitError::Exit(1));
    }

    let num_chunks = bytes[6] as usize;

    if bytes.len()
        < GRAPH_HEADER_SIZE
            + (num_chunks + 1) * GRAPH_CHUNK_TOC_ENTRY_SIZE
            + GRAPH_FANOUT_SIZE
            + hash_len
    {
        eprintln!("error: commit-graph file is too small to hold {num_chunks} chunks");
        return Err(GitError::Exit(1));
    }

    // Read the table of contents (mirrors read_table_of_contents with
    // expected_alignment = 1 for commit-graph).
    let mut chunks: Vec<GraphChunk> = Vec::with_capacity(num_chunks);
    let mfile_size = bytes.len();
    let mut toc = GRAPH_HEADER_SIZE;
    for _ in 0..num_chunks {
        let chunk_id = [bytes[toc], bytes[toc + 1], bytes[toc + 2], bytes[toc + 3]];
        let chunk_offset = read_be64(bytes, toc + 4) as usize;
        if chunk_id == [0, 0, 0, 0] {
            eprintln!("error: terminating chunk id appears earlier than expected");
            return Err(GitError::Exit(1));
        }
        let next_toc = toc + GRAPH_CHUNK_TOC_ENTRY_SIZE;
        let next_chunk_offset = read_be64(bytes, next_toc + 4) as usize;
        if next_chunk_offset < chunk_offset || next_chunk_offset > mfile_size - hash_len {
            eprintln!("error: improper chunk offset(s) {chunk_offset:X} and {next_chunk_offset:X}");
            return Err(GitError::Exit(1));
        }
        if chunks.iter().any(|chunk| chunk.id == chunk_id) {
            eprintln!("error: duplicate chunk ID {} found", be32_of(&chunk_id));
            return Err(GitError::Exit(1));
        }
        chunks.push(GraphChunk {
            id: chunk_id,
            start: chunk_offset,
            size: next_chunk_offset - chunk_offset,
        });
        toc = next_toc;
    }
    let terminator_id = read_be32(bytes, toc);
    if terminator_id != 0 {
        eprintln!("error: final chunk has non-zero id {terminator_id:X}");
        return Err(GitError::Exit(1));
    }

    let find = |id: [u8; 4]| chunks.iter().find(|chunk| chunk.id == id);

    // Required: OID fanout.
    let fanout_chunk = find(CHUNK_OIDF);
    let (oid_fanout, num_commits) = match fanout_chunk {
        Some(chunk) if chunk.size == 256 * 4 => {
            // fanout out-of-order check
            for i in 0..255usize {
                let f1 = read_be32(bytes, chunk.start + i * 4);
                let f2 = read_be32(bytes, chunk.start + (i + 1) * 4);
                if f1 > f2 {
                    eprintln!("error: commit-graph fanout values out of order");
                    eprintln!("error: commit-graph required OID fanout chunk missing or corrupted");
                    return Err(GitError::Exit(1));
                }
            }
            (chunk.start, read_be32(bytes, chunk.start + 255 * 4))
        }
        Some(_) => {
            eprintln!("error: commit-graph oid fanout chunk is wrong size");
            eprintln!("error: commit-graph required OID fanout chunk missing or corrupted");
            return Err(GitError::Exit(1));
        }
        None => {
            eprintln!("error: commit-graph required OID fanout chunk missing or corrupted");
            return Err(GitError::Exit(1));
        }
    };

    // Required: OID lookup.
    let oid_lookup = match find(CHUNK_OIDL) {
        Some(chunk) if chunk.size / hash_len == num_commits as usize => chunk.start,
        Some(_) => {
            eprintln!("error: commit-graph OID lookup chunk is the wrong size");
            eprintln!("error: commit-graph required OID lookup chunk missing or corrupted");
            return Err(GitError::Exit(1));
        }
        None => {
            eprintln!("error: commit-graph required OID lookup chunk missing or corrupted");
            return Err(GitError::Exit(1));
        }
    };

    // Required: commit data.
    let commit_data = match find(CHUNK_CDAT) {
        Some(chunk) if chunk.size / graph_data_width(hash_len) == num_commits as usize => {
            chunk.start
        }
        Some(_) => {
            eprintln!("error: commit-graph commit data chunk is wrong size");
            eprintln!("error: commit-graph required commit data chunk missing or corrupted");
            return Err(GitError::Exit(1));
        }
        None => {
            eprintln!("error: commit-graph required commit data chunk missing or corrupted");
            return Err(GitError::Exit(1));
        }
    };

    let extra_edges = find(CHUNK_EDGE).map(|chunk| (chunk.start, chunk.size));

    Ok(ParsedGraph {
        bytes,
        format,
        hash_len,
        num_commits,
        oid_fanout,
        oid_lookup,
        commit_data,
        extra_edges,
    })
}

fn be32_of(id: &[u8; 4]) -> String {
    format!("{:X}", u32::from_be_bytes(*id))
}

/// Full verify of a single-file commit-graph: re-parse + structural checks +
/// per-commit cross-check against the ODB. Returns `Ok(())` only when the graph
/// is fully valid (git's exit 0); otherwise prints the matching diagnostics and
/// returns `Exit`.
pub(super) fn verify_commit_graph_bytes(
    object_dir: &Path,
    format: ObjectFormat,
    bytes: &[u8],
    progress: bool,
) -> Result<()> {
    let parsed = parse_commit_graph_for_verify(bytes, format)?;

    let db = FileObjectDatabase::new(object_dir, format);
    let mut had_error = false;
    // Tracks whether the only error so far is the checksum failure; git allows
    // the per-commit cross-check to proceed past a checksum-only failure.
    let mut non_checksum_error = false;

    // Checksum validation (git: commit_graph_checksum_valid).
    let hash_len = parsed.hash_len;
    let checksum_offset = bytes.len() - hash_len;
    let actual = sley_core::digest_bytes(format, &bytes[..checksum_offset])?;
    let stored = ObjectId::from_raw(format, &bytes[checksum_offset..])?;
    if actual != stored {
        eprintln!("error: the commit-graph file has incorrect checksum and is likely corrupt");
        had_error = true;
    }

    let num_commits = parsed.num_commits as usize;

    // OID order + fanout consistency (first verify loop).
    let mut prev_oid: Option<ObjectId> = None;
    let mut cur_fanout_pos = 0u32;
    for i in 0..num_commits {
        let cur_oid = oid_at_lookup(&parsed, i)?;
        if let Some(prev) = prev_oid
            && prev.as_bytes() >= cur_oid.as_bytes()
        {
            eprintln!("error: commit-graph has incorrect OID order: {prev} then {cur_oid}");
            had_error = true;
            non_checksum_error = true;
        }
        prev_oid = Some(cur_oid);

        let first_byte = u32::from(cur_oid.as_bytes()[0]);
        while first_byte > cur_fanout_pos {
            let fanout_value = read_be32(bytes, parsed.oid_fanout + cur_fanout_pos as usize * 4);
            if i as u32 != fanout_value {
                eprintln!(
                    "error: commit-graph has incorrect fanout value: fanout[{}] = {} != {}",
                    cur_fanout_pos, fanout_value, i
                );
                had_error = true;
                non_checksum_error = true;
            }
            cur_fanout_pos += 1;
        }
    }
    while cur_fanout_pos < 256 {
        let fanout_value = read_be32(bytes, parsed.oid_fanout + cur_fanout_pos as usize * 4);
        if parsed.num_commits != fanout_value {
            eprintln!(
                "error: commit-graph has incorrect fanout value: fanout[{}] = {} != {}",
                cur_fanout_pos, fanout_value, num_commits
            );
            had_error = true;
            non_checksum_error = true;
        }
        cur_fanout_pos += 1;
    }

    // git: if (verify_commit_graph_error & ~VERIFY_COMMIT_GRAPH_ERROR_HASH)
    //          return verify_commit_graph_error;
    // i.e. stop before the per-commit ODB cross-check if any *non-checksum*
    // error fired above.
    if non_checksum_error {
        return Err(GitError::Exit(1));
    }

    // Per-commit cross-check against the object database (second verify loop).
    // git drives a progress meter titled "Verifying commits in commit graph"
    // here; emit the final, complete line when progress is requested.
    if progress {
        eprintln!("Verifying commits in commit graph: 100% ({num_commits}/{num_commits}), done.");
    }
    let mut seen_gen_zero: Option<ObjectId> = None;
    let mut seen_gen_non_zero: Option<ObjectId> = None;

    for i in 0..num_commits {
        let cur_oid = oid_at_lookup(&parsed, i)?;

        // Parse the commit from the ODB.
        let odb_object = match db.read_object(&cur_oid) {
            Ok(object) if object.object_type == ObjectType::Commit => object,
            _ => {
                eprintln!(
                    "error: failed to parse commit {cur_oid} from object database for commit-graph"
                );
                had_error = true;
                continue;
            }
        };
        let odb_commit = Commit::parse_ref(format, &odb_object.body)?;

        // Decode the graph's record for this commit.
        let record = decode_graph_commit(&parsed, i)?;

        // Root tree OID.
        if record.tree != odb_commit.tree {
            eprintln!(
                "error: root tree OID for commit {cur_oid} in commit-graph is {} != {}",
                record.tree, odb_commit.tree
            );
            had_error = true;
        }

        // Parents: compare graph-encoded parents against the ODB parents.
        let graph_parents = &record.parents;
        let odb_parents = &odb_commit.parents;
        let mut max_generation = 0u64;
        let common = graph_parents.len().min(odb_parents.len());
        for k in 0..common {
            let graph_parent_oid = oid_at_lookup(&parsed, graph_parents[k] as usize)?;
            if graph_parent_oid != odb_parents[k] {
                eprintln!(
                    "error: commit-graph parent for {cur_oid} is {graph_parent_oid} != {}",
                    odb_parents[k]
                );
                had_error = true;
            }
            let parent_record = decode_graph_commit(&parsed, graph_parents[k] as usize)?;
            if parent_record.generation > max_generation {
                max_generation = parent_record.generation;
            }
        }
        if graph_parents.len() > odb_parents.len() {
            eprintln!("error: commit-graph parent list for commit {cur_oid} is too long");
            had_error = true;
        } else if odb_parents.len() > graph_parents.len() {
            eprintln!("error: commit-graph parent list for commit {cur_oid} terminates early");
            had_error = true;
        }

        if record.generation != 0 {
            seen_gen_non_zero = Some(cur_oid);
        } else {
            seen_gen_zero = Some(cur_oid);
        }

        if seen_gen_zero.is_some() {
            continue;
        }

        // V1 (topological level) generation check. This graph is written with
        // generationVersion=1, so read_generation_data is false.
        if max_generation == GENERATION_NUMBER_V1_MAX {
            max_generation -= 1;
        }
        if record.generation < max_generation + 1 {
            eprintln!(
                "error: commit-graph generation for commit {cur_oid} is {} < {}",
                record.generation,
                max_generation + 1
            );
            had_error = true;
        }

        // Commit date cross-check.
        let odb_date = odb_commit
            .committer_signature()
            .map(|sig| sig.time.seconds)
            .unwrap_or(0);
        if record.commit_date as i64 != odb_date {
            eprintln!(
                "error: commit date for commit {cur_oid} in commit-graph is {} != {}",
                record.commit_date, odb_date
            );
            had_error = true;
        }
    }

    if let (Some(zero), Some(non_zero)) = (seen_gen_zero, seen_gen_non_zero) {
        eprintln!(
            "error: commit-graph has both zero and non-zero generations (e.g., commits '{zero}' and '{non_zero}')"
        );
        had_error = true;
    }

    if had_error {
        Err(GitError::Exit(1))
    } else {
        Ok(())
    }
}

/// The OID at lexicographic position `index` in the OID lookup chunk.
fn oid_at_lookup(parsed: &ParsedGraph<'_>, index: usize) -> Result<ObjectId> {
    let off = parsed.oid_lookup + index * parsed.hash_len;
    ObjectId::from_raw(parsed.format, &parsed.bytes[off..off + parsed.hash_len])
}

/// A commit record decoded straight from the graph's CDAT/EDGE chunks, mirroring
/// git's `fill_commit_in_graph` + `fill_commit_graph_info` parent/date/gen
/// decoding. `parents` are lexicographic positions into the OID lookup table.
struct GraphCommitRecord {
    tree: ObjectId,
    parents: Vec<u32>,
    generation: u64,
    commit_date: u64,
}

fn decode_graph_commit(parsed: &ParsedGraph<'_>, index: usize) -> Result<GraphCommitRecord> {
    let hash_len = parsed.hash_len;
    let width = graph_data_width(hash_len);
    let base = parsed.commit_data + index * width;
    let bytes = parsed.bytes;

    let tree = ObjectId::from_raw(parsed.format, &bytes[base..base + hash_len])?;

    // Date / generation (fill_commit_graph_info, V1 path).
    let date_high = u64::from(read_be32(bytes, base + hash_len + 8) & 0x3);
    let date_low = u64::from(read_be32(bytes, base + hash_len + 12));
    let commit_date = (date_high << 32) | date_low;
    let generation = u64::from(read_be32(bytes, base + hash_len + 8) >> 2);

    // Parents (fill_commit_in_graph). git `die`s on an out-of-range parent
    // position; we mirror that with a fatal `invalid parent position` + exit
    // 128, and on an out-of-bounds extra-edges pointer with the `error:`
    // string + exit 1 (commit-graph extra-edges pointer out of bounds).
    let num_total = parsed.num_commits;
    let mut parents = Vec::new();

    let insert = |pos: u32, parents: &mut Vec<u32>| -> Result<()> {
        if pos >= num_total {
            eprintln!("fatal: invalid parent position {pos}");
            return Err(GitError::Exit(128));
        }
        parents.push(pos);
        Ok(())
    };

    let edge0 = read_be32(bytes, base + hash_len);
    if edge0 == GRAPH_PARENT_NONE {
        return Ok(GraphCommitRecord {
            tree,
            parents,
            generation,
            commit_date,
        });
    }
    insert(edge0, &mut parents)?;

    let edge1 = read_be32(bytes, base + hash_len + 4);
    if edge1 == GRAPH_PARENT_NONE {
        return Ok(GraphCommitRecord {
            tree,
            parents,
            generation,
            commit_date,
        });
    }
    if edge1 & GRAPH_EXTRA_EDGES_NEEDED == 0 {
        insert(edge1, &mut parents)?;
        return Ok(GraphCommitRecord {
            tree,
            parents,
            generation,
            commit_date,
        });
    }

    // Octopus: walk the EDGE chunk.
    let mut parent_data_pos = edge1 & GRAPH_EDGE_LAST_MASK;
    let (edge_start, edge_size) = parsed.extra_edges.unwrap_or((0, 0));
    loop {
        if (edge_size / 4) as u32 <= parent_data_pos {
            eprintln!("error: commit-graph extra-edges pointer out of bounds");
            return Err(GitError::Exit(1));
        }
        let edge_value = read_be32(bytes, edge_start + parent_data_pos as usize * 4);
        insert(edge_value & GRAPH_EDGE_LAST_MASK, &mut parents)?;
        parent_data_pos += 1;
        if edge_value & GRAPH_LAST_EDGE != 0 {
            break;
        }
    }

    Ok(GraphCommitRecord {
        tree,
        parents,
        generation,
        commit_date,
    })
}

fn commit_graph_for_reachable_refs(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    changed_paths: bool,
    bloom_settings: sley_formats::CommitGraphBloomSettings,
    write_generation_data: bool,
    max_new_filters: Option<usize>,
    existing_filters: &HashMap<ObjectId, CommitGraphExistingBloomFilter>,
    progress: bool,
    replace_objects: bool,
) -> Result<Vec<u8>> {
    let store = FileRefStore::new(git_dir, format);
    let mut starts = Vec::new();
    let mut seen_starts = HashSet::new();
    for reference in store.list_refs()? {
        let RefTarget::Direct(oid) = reference.target else {
            continue;
        };
        if let Ok(commit) = sley_rev::peel_to_commit(db, format, &oid)
            && seen_starts.insert(commit)
        {
            starts.push(commit);
        }
    }
    if let Ok(head) = resolve_revision(git_dir, format, "HEAD", replace_objects)
        && let Ok(commit) = sley_rev::peel_to_commit(db, format, &head)
        && seen_starts.insert(commit)
    {
        starts.push(commit);
    }
    commit_graph_from_starts(
        db,
        format,
        starts,
        changed_paths,
        bloom_settings,
        write_generation_data,
        max_new_filters,
        existing_filters,
        progress,
    )
}

/// Build the commit-graph bytes from a set of seed commit oids (their parent
/// closure is walked). Shared by the `--reachable`, default-all-packs, and
/// `--stdin-commits` paths.
fn commit_graph_from_starts(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    starts: Vec<ObjectId>,
    changed_paths: bool,
    bloom_settings: sley_formats::CommitGraphBloomSettings,
    write_generation_data: bool,
    max_new_filters: Option<usize>,
    existing_filters: &HashMap<ObjectId, CommitGraphExistingBloomFilter>,
    progress: bool,
) -> Result<Vec<u8>> {
    // git's `close_reachable` walk parses every reachable commit (including
    // parents pulled into the closure); a commit that cannot be parsed is fatal
    // with `unable to parse commit <oid>` (exit 128). `walk_commits` surfaces a
    // generic read/parse error instead, so map it to git's diagnostic by
    // re-checking which oid in the closure is unparseable.
    let records = match sley_rev::walk_commits(db, format, starts.clone()) {
        Ok(records) => records,
        Err(err) => {
            if let Some(oid) = commit_graph_first_unparseable_commit(db, format, &starts) {
                eprintln!("fatal: unable to parse commit {oid}");
                return Err(GitError::Exit(128));
            }
            return Err(err);
        }
    };
    let record_map = records
        .iter()
        .map(|record| (record.oid, record))
        .collect::<HashMap<_, _>>();
    let mut generation_cache = HashMap::new();
    let mut entries = Vec::with_capacity(records.len());
    let mut bloom_stats = CommitGraphBloomWriteStats::default();
    for record in &records {
        let bloom_filter = if changed_paths {
            if let Some(existing) = existing_filters.get(&record.oid) {
                match commit_graph_reusable_bloom_filter_for_record(
                    db,
                    format,
                    record,
                    &record_map,
                    bloom_settings,
                    existing,
                )? {
                    CommitGraphBloomReuse::Reuse(filter) => {
                        bloom_stats.filter_not_computed += 1;
                        Some(filter)
                    }
                    CommitGraphBloomReuse::Upgrade(filter) => {
                        bloom_stats.filter_not_computed += 1;
                        bloom_stats.filter_upgraded += 1;
                        Some(filter)
                    }
                    CommitGraphBloomReuse::Recompute => {
                        if max_new_filters.is_some_and(|max| bloom_stats.filter_computed >= max) {
                            bloom_stats.filter_not_computed += 1;
                            None
                        } else {
                            let (filter, disposition) = commit_graph_bloom_filter_for_record(
                                db,
                                format,
                                record,
                                &record_map,
                                bloom_settings,
                            )?;
                            bloom_stats.filter_computed += 1;
                            match disposition {
                                CommitGraphBloomDisposition::Empty => {
                                    bloom_stats.filter_trunc_empty += 1
                                }
                                CommitGraphBloomDisposition::Large => {
                                    bloom_stats.filter_trunc_large += 1
                                }
                                CommitGraphBloomDisposition::Normal => {}
                            }
                            Some(filter)
                        }
                    }
                }
            } else if max_new_filters.is_some_and(|max| bloom_stats.filter_computed >= max) {
                bloom_stats.filter_not_computed += 1;
                None
            } else {
                let (filter, disposition) = commit_graph_bloom_filter_for_record(
                    db,
                    format,
                    record,
                    &record_map,
                    bloom_settings,
                )?;
                bloom_stats.filter_computed += 1;
                match disposition {
                    CommitGraphBloomDisposition::Empty => bloom_stats.filter_trunc_empty += 1,
                    CommitGraphBloomDisposition::Large => bloom_stats.filter_trunc_large += 1,
                    CommitGraphBloomDisposition::Normal => {}
                }
                Some(filter)
            }
        } else {
            None
        };
        entries.push(CommitGraphWriteEntry {
            oid: record.oid,
            tree: record.commit.tree,
            parents: record.parents.clone(),
            generation: commit_graph_generation(&record.oid, &record_map, &mut generation_cache)?,
            commit_time: commit_graph_commit_time(&record.commit)?,
            bloom_filter,
        });
    }
    if progress {
        let count = entries.len();
        // git drives several delayed progress meters during a write; emit the
        // reachable-collection, generation-number, and write-out lines
        // (always) and the changed-path Bloom-filter line (only when
        // changed-path filters are computed).
        eprintln!("Collecting referenced commits: {count}, done.");
        if changed_paths {
            eprintln!(
                "Computing commit changed paths Bloom filters: 100% ({count}/{count}), done."
            );
        }
        eprintln!("Computing commit graph generation numbers: 100% ({count}/{count}), done.");
        eprintln!(
            "Writing out commit graph in 3 passes: 100% ({}/{}), done.",
            count * 3,
            count * 3
        );
    }
    if changed_paths {
        trace_commit_graph_bloom_settings(bloom_settings);
        sley_core::trace2::data(
            "commit-graph",
            "filter-computed",
            bloom_stats.filter_computed,
        );
        sley_core::trace2::data(
            "commit-graph",
            "filter-not-computed",
            bloom_stats.filter_not_computed,
        );
        sley_core::trace2::data(
            "commit-graph",
            "filter-trunc-empty",
            bloom_stats.filter_trunc_empty,
        );
        sley_core::trace2::data(
            "commit-graph",
            "filter-trunc-large",
            bloom_stats.filter_trunc_large,
        );
        sley_core::trace2::data(
            "commit-graph",
            "filter-upgraded",
            bloom_stats.filter_upgraded,
        );
    }
    CommitGraph::write_with_options(format, &entries, bloom_settings, write_generation_data)
}

/// Walk the parent closure of `starts` and return the first oid that cannot be
/// read + parsed as a commit object (git's closure walk dies on such a commit
/// with `unable to parse commit <oid>`). Returns `None` if the whole closure
/// parses (the original error was something else).
fn commit_graph_first_unparseable_commit(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    starts: &[ObjectId],
) -> Option<ObjectId> {
    let mut seen = HashSet::new();
    let mut pending: VecDeque<ObjectId> = starts.iter().copied().collect();
    while let Some(oid) = pending.pop_front() {
        if !seen.insert(oid) {
            continue;
        }
        match db.read_object(&oid) {
            Ok(object) if object.object_type == ObjectType::Commit => {
                match Commit::parse_ref(format, &object.body) {
                    Ok(commit) => pending.extend(commit.parents.iter().copied()),
                    Err(_) => return Some(oid),
                }
            }
            // Not a commit, or not readable at all ⇒ git cannot parse it.
            _ => return Some(oid),
        }
    }
    None
}

#[derive(Default)]
struct CommitGraphBloomWriteStats {
    filter_computed: usize,
    filter_not_computed: usize,
    filter_trunc_empty: usize,
    filter_trunc_large: usize,
    filter_upgraded: usize,
}

enum CommitGraphBloomDisposition {
    Normal,
    Empty,
    Large,
}

enum CommitGraphBloomReuse {
    Reuse(Vec<u8>),
    Upgrade(Vec<u8>),
    Recompute,
}

#[derive(Debug, Clone)]
struct CommitGraphExistingBloomFilter {
    filter: Vec<u8>,
    settings: sley_formats::CommitGraphBloomSettings,
}

fn commit_graph_bloom_filter_for_record(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    record: &sley_rev::CommitRecord,
    records: &HashMap<ObjectId, &sley_rev::CommitRecord>,
    bloom_settings: sley_formats::CommitGraphBloomSettings,
) -> Result<(Vec<u8>, CommitGraphBloomDisposition)> {
    let changes = commit_graph_changed_paths_for_record(db, format, record, records)?;
    Ok(commit_graph_bloom_filter_for_changes(
        &changes,
        bloom_settings,
    ))
}

fn commit_graph_reusable_bloom_filter_for_record(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    record: &sley_rev::CommitRecord,
    records: &HashMap<ObjectId, &sley_rev::CommitRecord>,
    bloom_settings: sley_formats::CommitGraphBloomSettings,
    existing: &CommitGraphExistingBloomFilter,
) -> Result<CommitGraphBloomReuse> {
    if commit_graph_bloom_settings_match(existing.settings, bloom_settings) {
        return Ok(CommitGraphBloomReuse::Reuse(existing.filter.clone()));
    }
    if !commit_graph_bloom_filter_can_upgrade(existing.settings, bloom_settings) {
        return Ok(CommitGraphBloomReuse::Recompute);
    }
    let changes = commit_graph_changed_paths_for_record(db, format, record, records)?;
    if commit_graph_bloom_paths_v1_v2_compatible(&changes) {
        Ok(CommitGraphBloomReuse::Upgrade(existing.filter.clone()))
    } else {
        Ok(CommitGraphBloomReuse::Recompute)
    }
}

fn commit_graph_changed_paths_for_record(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    record: &sley_rev::CommitRecord,
    records: &HashMap<ObjectId, &sley_rev::CommitRecord>,
) -> Result<Vec<sley_diff_merge::NameStatusEntry>> {
    let options = sley_diff_merge::DiffNameStatusOptions {
        detect_renames: false,
        detect_copies: false,
        find_copies_harder: false,
        rename_empty: false,
        ..Default::default()
    };
    let changes = if let Some(parent) = record.parents.first() {
        let parent_tree = if let Some(parent_record) = records.get(parent) {
            parent_record.commit.tree
        } else {
            read_commit_tree_for_graph(db, format, parent)?
        };
        if parent_tree == record.commit.tree {
            Vec::new()
        } else {
            sley_diff_merge::diff_name_status_trees_with_options(
                db,
                format,
                &parent_tree,
                &record.commit.tree,
                options,
            )?
        }
    } else {
        sley_diff_merge::diff_name_status_empty_tree_with_options(
            db,
            format,
            &record.commit.tree,
            options,
        )?
    };
    Ok(changes)
}

fn commit_graph_bloom_filter_for_changes(
    changes: &[sley_diff_merge::NameStatusEntry],
    bloom_settings: sley_formats::CommitGraphBloomSettings,
) -> (Vec<u8>, CommitGraphBloomDisposition) {
    if changes.is_empty() {
        return (
            sley_formats::commit_graph_bloom_filter_for_paths(
                std::iter::empty::<&[u8]>(),
                bloom_settings,
            ),
            CommitGraphBloomDisposition::Empty,
        );
    }
    let filter = sley_formats::commit_graph_bloom_filter_for_paths(
        changes.iter().map(|entry| entry.path.as_bytes()),
        bloom_settings,
    );
    let disposition = if filter == [0xff] {
        CommitGraphBloomDisposition::Large
    } else {
        CommitGraphBloomDisposition::Normal
    };
    (filter, disposition)
}

fn commit_graph_bloom_settings_match(
    left: sley_formats::CommitGraphBloomSettings,
    right: sley_formats::CommitGraphBloomSettings,
) -> bool {
    left.hash_version == right.hash_version
        && left.hash_count == right.hash_count
        && left.bits_per_entry == right.bits_per_entry
}

fn commit_graph_bloom_filter_can_upgrade(
    existing: sley_formats::CommitGraphBloomSettings,
    target: sley_formats::CommitGraphBloomSettings,
) -> bool {
    existing.hash_version == 1
        && target.hash_version == 2
        && existing.hash_count == target.hash_count
        && existing.bits_per_entry == target.bits_per_entry
}

fn commit_graph_bloom_paths_v1_v2_compatible(changes: &[sley_diff_merge::NameStatusEntry]) -> bool {
    changes.iter().all(|entry| {
        entry.path.as_bytes().iter().all(u8::is_ascii)
            && entry
                .old_path
                .as_ref()
                .is_none_or(|path| path.as_bytes().iter().all(u8::is_ascii))
    })
}

/// Write-side `commit_graph_compatible`: grafts and shallow clones make the
/// on-disk parent list disagree with the graph, so writing is a silent no-op
/// (git returns 0 without creating/updating the file). Replace refs are *not*
/// checked here because `cmd_commit_graph` disables them before writing.
fn commit_graph_write_compatible(git_dir: &Path, format: ObjectFormat) -> bool {
    if sley_worktree::is_shallow_repository(git_dir) {
        return false;
    }
    if !sley_rev::revlist::load_commit_grafts_from_git_dir(git_dir, format).is_empty() {
        return false;
    }
    true
}

/// Build the origin-carrying config view (system → global → repository, plus
/// `-c` injections) used for lookups whose failure diagnostics name the value's
/// source file. Best-effort: unreadable layers simply don't contribute.
fn commit_graph_origin_stack(
    cli_session: &crate::session::CliSession,
    git_dir: &Path,
) -> Option<sley_config::ConfigStack> {
    let mut stack = sley_config::ConfigStack::new();
    let context = sley_config::ConfigIncludeContext::new(
        Some(sley_config::git_dir_for_include_context(git_dir)),
        crate::repo_current_branch_name(git_dir),
    );
    for (path, scope) in sley_config::default_config_layer_paths() {
        let _ = stack.push_file(&path, scope, true, &context);
    }
    let config_path = git_dir.join("config");
    let display_path = config_path
        .strip_prefix(cli_session.cwd())
        .unwrap_or(&config_path);
    let _ = stack.push_file(
        display_path,
        sley_config::ConfigScope::Local,
        true,
        &context,
    );
    if let Ok(parameters) = crate::injected_config_parameters() {
        let _ = stack.push_parameters_with_includes(&parameters, &context);
    }
    Some(stack)
}

/// `commitGraph.generationVersion` (git's `get_configured_generation_version`):
/// defaults to 2, which writes the GDA2 corrected-commit-date chunk. A value of
/// 1 selects the legacy topological-level-only layout (no GDA2/GDO2). A
/// malformed value is fatal, as upstream's `repo_cfg_int` dies via
/// `die_bad_number`.
fn commit_graph_generation_version(config: Option<&sley_config::ConfigStack>) -> Result<i64> {
    let Some(config) = config else {
        return Ok(2);
    };
    match config.get_int("commitgraph", None, "generationversion") {
        Ok(Some(version)) => Ok(version),
        Ok(None) => Ok(2),
        // The accessor already printed git's exact fatal line.
        Err(report) => Err(report),
    }
}

fn commit_graph_changed_paths_version(config: Option<&sley_config::ConfigStack>) -> Result<i64> {
    let Some(config) = config else {
        return Ok(-1);
    };
    // Upstream reads this through `repo_cfg_int`, so a value-less bare key is
    // fatal (rendered with an empty value) exactly like a malformed one; only
    // a genuinely absent key falls back to `commitGraph.readChangedPaths`.
    match config.get_int("commitgraph", None, "changedpathsversion") {
        Ok(Some(version)) => return Ok(version),
        // The accessor already printed git's exact fatal line.
        Err(report) => return Err(report),
        Ok(None) => {}
    }
    match config.get_bool("commitgraph", None, "readchangedpaths") {
        Some(false) => Ok(0),
        Some(true) => Ok(-1),
        None => Ok(-1),
    }
}

fn commit_graph_bloom_settings_for_write(
    existing: Option<sley_formats::CommitGraphBloomSettings>,
    changed_paths_version: i64,
    honor_env: bool,
) -> sley_formats::CommitGraphBloomSettings {
    let mut settings = sley_formats::DEFAULT_COMMIT_GRAPH_BLOOM_SETTINGS;
    if changed_paths_version == -1
        && let Some(existing) = existing
    {
        settings = existing;
    }
    settings.hash_version = if changed_paths_version == 2
        || (changed_paths_version == -1 && settings.hash_version == 2)
    {
        2
    } else {
        1
    };
    if honor_env {
        if let Ok(value) = env::var("GIT_TEST_BLOOM_SETTINGS_NUM_HASHES")
            && let Ok(parsed) = value.parse::<u32>()
        {
            settings.hash_count = parsed;
        }
        if let Ok(value) = env::var("GIT_TEST_BLOOM_SETTINGS_BITS_PER_ENTRY")
            && let Ok(parsed) = value.parse::<u32>()
        {
            settings.bits_per_entry = parsed;
        }
        if let Ok(value) = env::var("GIT_TEST_BLOOM_SETTINGS_MAX_CHANGED_PATHS")
            && let Ok(parsed) = value.parse::<usize>()
        {
            settings.max_changed_paths = parsed;
        }
    }
    settings
}

fn existing_commit_graph_bloom_settings(
    object_dir: &Path,
    format: ObjectFormat,
) -> Result<Option<sley_formats::CommitGraphBloomSettings>> {
    let graph_path = object_dir.join("info").join("commit-graph");
    if graph_path.exists()
        && let Some(settings) = commit_graph_bloom_settings_from_file(&graph_path, format)
    {
        return Ok(Some(settings));
    }
    let chain_path = object_dir
        .join("info")
        .join("commit-graphs")
        .join("commit-graph-chain");
    let chain_dir = match chain_path.parent() {
        Some(dir) => dir.to_path_buf(),
        None => return Ok(None),
    };
    let hashes = read_commit_graph_chain_hashes(&chain_path, format).unwrap_or_default();
    if let Some(hash) = hashes.last() {
        let path = chain_dir.join(format!("graph-{hash}.graph"));
        if let Some(settings) = commit_graph_bloom_settings_from_file(&path, format) {
            return Ok(Some(settings));
        }
    }
    Ok(None)
}

fn commit_graph_bloom_settings_from_file(
    path: &Path,
    format: ObjectFormat,
) -> Option<sley_formats::CommitGraphBloomSettings> {
    let bytes = fs::read(path).ok()?;
    let graph = CommitGraph::parse(&bytes, format).ok()?;
    graph
        .bloom_filters
        .map(|filters| commit_graph_bloom_settings_from_filters(&filters))
}

fn commit_graph_bloom_settings_from_filters(
    filters: &sley_formats::CommitGraphBloomFilters,
) -> sley_formats::CommitGraphBloomSettings {
    let mut settings = sley_formats::DEFAULT_COMMIT_GRAPH_BLOOM_SETTINGS;
    settings.hash_version = filters.hash_version;
    settings.hash_count = filters.hash_count;
    settings.bits_per_entry = filters.bits_per_entry;
    settings
}

fn existing_commit_graph_bloom_filters(
    object_dir: &Path,
    format: ObjectFormat,
    target_settings: sley_formats::CommitGraphBloomSettings,
) -> Result<HashMap<ObjectId, CommitGraphExistingBloomFilter>> {
    let mut out = HashMap::new();
    let info = object_dir.join("info");
    let single = info.join("commit-graph");
    if single.exists() {
        load_commit_graph_bloom_filters_from_file(&single, format, None, &mut out);
        return Ok(out);
    }
    let chain = info.join("commit-graphs").join("commit-graph-chain");
    let chain_dir = match chain.parent() {
        Some(dir) => dir.to_path_buf(),
        None => return Ok(out),
    };
    for hash in read_commit_graph_chain_hashes(&chain, format).unwrap_or_default() {
        let path = chain_dir.join(format!("graph-{hash}.graph"));
        load_commit_graph_bloom_filters_from_file(
            &path,
            format,
            Some((hash, target_settings)),
            &mut out,
        );
    }
    Ok(out)
}

fn load_commit_graph_bloom_filters_from_file(
    path: &Path,
    format: ObjectFormat,
    split_target: Option<(ObjectId, sley_formats::CommitGraphBloomSettings)>,
    out: &mut HashMap<ObjectId, CommitGraphExistingBloomFilter>,
) {
    let Ok(bytes) = fs::read(path) else {
        return;
    };
    let Ok(graph) = CommitGraph::parse(&bytes, format) else {
        return;
    };
    let Some(filters) = &graph.bloom_filters else {
        return;
    };
    let settings = commit_graph_bloom_settings_from_filters(filters);
    if let Some((hash, target_settings)) = split_target
        && !commit_graph_bloom_settings_match(settings, target_settings)
    {
        eprintln!(
            "warning: disabling Bloom filters for commit-graph layer '{hash}' due to incompatible settings"
        );
        return;
    }
    for (idx, entry) in graph.commits.iter().enumerate() {
        let Some(filter) = filters.filter_for_commit(idx) else {
            continue;
        };
        if !filter.is_empty() {
            out.insert(
                entry.oid,
                CommitGraphExistingBloomFilter {
                    filter: filter.to_vec(),
                    settings,
                },
            );
        }
    }
}

fn trace_commit_graph_bloom_settings(settings: sley_formats::CommitGraphBloomSettings) {
    let Some(target) = env::var_os("GIT_TRACE2_EVENT") else {
        return;
    };
    let target = target.to_string_lossy().into_owned();
    if !target.starts_with('/') {
        return;
    }
    let line = format!(
        "{{\"event\":\"data_json\",\"sid\":\"sley\",\"thread\":\"main\",\"nesting\":1,\"category\":\"commit-graph\",\"key\":\"bloom-settings\",\"value\":{{\"hash_version\":{},\"num_hashes\":{},\"bits_per_entry\":{},\"max_changed_paths\":{}}}}}\n",
        settings.hash_version,
        settings.hash_count,
        settings.bits_per_entry,
        settings.max_changed_paths
    );
    if let Ok(mut file) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&target)
    {
        let _ = file.write_all(line.as_bytes());
    }
}

fn read_commit_tree_for_graph(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
) -> Result<ObjectId> {
    let object = db.read_object(oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "expected commit {oid}, found {}",
            object.object_type.as_str()
        )));
    }
    Ok(Commit::parse_ref(format, &object.body)?.tree)
}

fn commit_graph_generation(
    oid: &ObjectId,
    records: &HashMap<ObjectId, &sley_rev::CommitRecord>,
    cache: &mut HashMap<ObjectId, u32>,
) -> Result<u32> {
    if let Some(generation) = cache.get(oid) {
        return Ok(*generation);
    }
    // V1 topological-level generation: 1 + max(parent generations). Computed with
    // an explicit work stack rather than recursion — a recursive walk overflows
    // the call stack on deep histories (the commit-graph write covers every
    // reachable commit, which can be tens of thousands deep). The memoised result
    // is identical to the recursive form.
    let mut stack: Vec<ObjectId> = vec![*oid];
    while let Some(&current) = stack.last() {
        if cache.contains_key(&current) {
            stack.pop();
            continue;
        }
        let record = records.get(&current).ok_or_else(|| {
            GitError::InvalidObject(format!("commit {current} missing from walk"))
        })?;
        let mut max_parent = 0u32;
        let mut ready = true;
        for parent in &record.parents {
            match cache.get(parent) {
                Some(generation) => max_parent = max_parent.max(*generation),
                None => {
                    // Defer until the parent is resolved; it is pushed above
                    // `current`, which is re-examined once all parents are ready.
                    stack.push(*parent);
                    ready = false;
                }
            }
        }
        if ready {
            let generation = max_parent
                .checked_add(1)
                .ok_or_else(|| GitError::InvalidFormat("commit generation overflow".into()))?;
            cache.insert(current, generation);
            stack.pop();
        }
    }
    Ok(*cache
        .get(oid)
        .expect("generation computed for requested commit"))
}

fn commit_graph_commit_time(commit: &Commit) -> Result<u64> {
    commit_graph_commit_time_from_committer(&commit.committer)
}

pub(super) fn commit_graph_commit_time_from_committer(committer: &[u8]) -> Result<u64> {
    let committer =
        std::str::from_utf8(committer).map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    let Some((before_tz, _tz)) = committer.rsplit_once(' ') else {
        return Err(GitError::InvalidFormat(
            "commit committer is missing timezone".into(),
        ));
    };
    let Some((_identity, timestamp)) = before_tz.rsplit_once(' ') else {
        return Err(GitError::InvalidFormat(
            "commit committer is missing timestamp".into(),
        ));
    };
    timestamp
        .parse::<u64>()
        .map_err(|err| GitError::InvalidFormat(err.to_string()))
}
