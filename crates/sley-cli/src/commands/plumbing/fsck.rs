//! Extracted from the crate root (sley#8 phase 1) — code motion only.

use crate::*;
use sley::plumbing::{
    sley_core, sley_index, sley_object, sley_odb, sley_pack, sley_refs, sley_worktree,
};

use super::commit_graph::{OpenResult, open_commit_graph_bytes, verify_commit_graph_bytes};

pub(crate) fn cmd_fsck(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let mut progress = true;
    let mut report_dangling = true;
    let mut report_unreachable = false;
    let mut strict = false;
    let mut connectivity_only = false;
    let mut write_lost_found = false;
    // `--references` (the default) runs the ref-store consistency check
    // (`refs verify`) alongside the object walk; `--no-references` skips it.
    let mut references = true;
    // `--tags` restricts the root set to tags; `--root` additionally pins the
    // root tree(s). Both default off (a bare `git fsck` walks all refs).
    let mut only_tags = false;
    // `--name-objects` annotates broken/missing-object reports with a path
    // describing how the object is reached (e.g. an index entry `:file`).
    let mut name_objects = false;
    let mut explicit_oids: Vec<String> = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--no-progress" => progress = false,
            "--progress" => progress = true,
            "--dangling" => report_dangling = true,
            "--no-dangling" => report_dangling = false,
            "--unreachable" => report_unreachable = true,
            "--no-unreachable" => report_unreachable = false,
            "--strict" => strict = true,
            "--no-strict" => strict = false,
            "--connectivity-only" => connectivity_only = true,
            "--tags" => only_tags = true,
            "--name-objects" => name_objects = true,
            "--no-name-objects" => name_objects = false,
            // These affect output/perf only; object-content checks are
            // unconditional in this implementation, so accept and ignore them.
            "--references" => references = true,
            "--no-references" => references = false,
            "--lost-found" => write_lost_found = true,
            "--full" | "--no-full" | "--root" | "--cache" | "--no-cache" => {}
            value if value.starts_with("--") => {
                return Err(GitError::Command(format!(
                    "fsck currently supports --no-progress and basic object connectivity; unsupported option {value}"
                )));
            }
            // A positional argument is an explicit object/head to check.
            value => explicit_oids.push(value.to_string()),
        }
    }
    let cwd = env::current_dir()?;
    let git_dir = cli_session.git_dir()?;
    let format = repository_object_format(&git_dir)?;

    // Resolve `fsck.<msgid>` severity overrides from the repo config (folds in
    // command-line `-c fsck.x=y` via GIT_CONFIG_PARAMETERS). The same read tells
    // us whether this is a partial clone: only then may a `.promisor` pack
    // excuse a missing object from the connectivity walk (git's
    // `is_promisor_object` is gated on `repo_has_promisor_remote`).
    let mut policy = None;
    let mut has_promisor_remote = false;
    if let Ok(config) = read_repo_config(&git_dir) {
        policy = Some(sley_fsck::FsckPolicy::from_config(
            &config,
            sley_fsck::FsckConfigKind::Standalone,
            format,
            &cwd,
            strict,
        )?);
        has_promisor_remote = repo_has_promisor_remote(&config);
    }
    let policy = policy.unwrap_or_else(|| sley_fsck::FsckPolicy {
        enabled: true,
        severity: sley_fsck::SeverityConfig::new(strict),
        skip_objects: HashSet::new(),
        diagnostics: Vec::new(),
    });
    let db = FileObjectDatabase::from_git_dir(&git_dir, format)
        .with_promisor_remote_present(has_promisor_remote);
    // The ref-store consistency check shares the same severity table; clone it
    // before the object walk consumes `severity`.
    let refs_severity = policy.severity.clone();

    // git runs `fsck_refs` (the `refs verify` consistency check) before the
    // object walk when `--references` is in effect (the default). Its findings
    // count toward ERROR_REFS.
    let mut refs_verify_bits = 0i32;
    if references
        && crate::commands::refs_verify::verify_for_fsck(refs_severity, false, &git_dir)
            .unwrap_or(false)
    {
        refs_verify_bits |= sley_fsck::ERROR_REFS;
    }

    // Explicit object-id arguments override the default ref-walk roots. git
    // resolves each to an object; an explicit but unknown head reports
    // `invalid sha1 pointer` and does NOT fall back to all heads (t1450 "bogus
    // head" case), so the rest of the walk sees no roots.
    //
    // git's `snapshot_ref` validates every root ref before the walk:
    //   - if the ref's object is not parseable: `error: <name>: invalid sha1
    //     pointer <oid>` on stderr, sets ERROR_REACHABLE, and the ref is NOT
    //     walked (so its referents never surface as dangling/missing);
    //   - if the ref is a branch but its object is not a commit:
    //     `error: <name>: not a commit`, sets ERROR_REFS.
    // We collect named roots, run those checks, and pass only the valid tip
    // oids to the connectivity walk.
    let mut ref_error_bits = 0i32;
    let mut object_names = std::collections::HashMap::new();
    let named_roots: Vec<(String, ObjectId)> = if !explicit_oids.is_empty() {
        let mut resolved = Vec::new();
        for spec in &explicit_oids {
            match ObjectId::from_hex(format, spec) {
                Ok(oid) => resolved.push((oid.to_hex(), oid)),
                Err(_) => {
                    match resolve_revision(&git_dir, format, spec, cli_session.replace_objects()) {
                        Ok(oid) => resolved.push((spec.clone(), oid)),
                        Err(_) => {
                            return Err(GitError::Command(format!(
                                "Invalid object name '{spec}'."
                            )));
                        }
                    }
                }
            }
        }
        resolved
    } else if only_tags {
        fsck_tag_root_oids(&git_dir, format)?
    } else {
        fsck_root_oids(&git_dir, format)?
    };

    let mut roots = Vec::new();
    for (name, oid) in &named_roots {
        match db.read_object(oid) {
            Ok(object) => {
                // A branch ref must point at a commit.
                if object.object_type != sley_object::ObjectType::Commit && is_branch_ref(name) {
                    eprintln!("error: {name}: not a commit");
                    ref_error_bits |= sley_fsck::ERROR_REFS;
                }
                roots.push(*oid);
                if name_objects {
                    object_names.entry(*oid).or_insert_with(|| name.clone());
                }
            }
            Err(_) => {
                // A root that is missing locally but covered by a promisor
                // remote is still a valid ref/tip in a partial clone: git's
                // `snapshot_ref` counts it (default_refs++) and returns without
                // complaint, leaving it OUT of the walk roots (there is nothing
                // local to walk). Same for an explicit `git fsck <oid>` arg that
                // names a promised object.
                if db.is_promised_object(oid) {
                    continue;
                }
                eprintln!("error: {name}: invalid sha1 pointer {oid}");
                ref_error_bits |= sley_fsck::ERROR_REACHABLE;
            }
        }
    }

    if explicit_oids.is_empty() && !only_tags {
        ref_error_bits |= fsck_reflog_roots(&db, format, &git_dir)?;
        ref_error_bits |= fsck_worktree_head_refs(
            &db,
            format,
            &git_dir,
            name_objects,
            &mut roots,
            &mut object_names,
        )?;
    }

    // A valid explicit root replaces the default ref roots but still scans the
    // object store, so objects outside that root's reach are reported as
    // dangling (`git fsck main`). An invalid explicit root must not fall back
    // to all heads or enumerate unrelated objects (`git fsck <zero-oid>`).
    let scan_objects = explicit_oids.is_empty() || !roots.is_empty();
    let mut object_ids = if scan_objects {
        repository_object_ids(&git_dir, format)?
    } else {
        Vec::new()
    };
    // Mirror builtin/fsck.c `fsck_loose`: probe every loose object file before the
    // connectivity walk, reporting corrupt or mismatched ones at `error:` level on
    // stderr (with git's path-form spelling) and excluding them from the object set
    // so they neither parse nor surface as dangling.
    let objects_dir_display = fsck_objects_dir_display(&git_dir, &cwd);
    let mut bad_loose = HashSet::new();
    // The loose-object integrity scan enumerates the whole object store, which
    // git only does for a full fsck (no explicit roots).
    if scan_objects {
        for oid in db.loose().object_ids()? {
            let hex = oid.to_hex();
            let display_path = format!("{objects_dir_display}/{}/{}", &hex[..2], &hex[2..]);
            match db.loose().verify_object(&oid, &display_path)? {
                None | Some(LooseObjectIntegrity::Ok) => {}
                Some(LooseObjectIntegrity::HashMismatch { actual }) => {
                    if !connectivity_only {
                        eprintln!("error: {actual}: hash-path mismatch, found at: {display_path}");
                        bad_loose.insert(oid);
                    }
                }
                Some(LooseObjectIntegrity::Corrupt) => {
                    eprintln!("error: {oid}: object corrupt or missing: {display_path}");
                    bad_loose.insert(oid);
                }
            }
        }
    }
    let alternate_loose_errors = if scan_objects {
        fsck_alternate_loose_objects(&git_dir, format, &cwd)?
    } else {
        false
    };
    let pack_errors = if scan_objects {
        fsck_pack_files(&git_dir, format, &cwd)?
    } else {
        false
    };
    let loose_errors = !bad_loose.is_empty();
    object_ids.retain(|oid| !bad_loose.contains(oid));

    // git's `fsck_index`: with no explicit object args, the current worktree's
    // index (and other worktrees') becomes a reachability root set. Each
    // non-gitlink entry's blob is marked reachable; a missing one is reported as
    // `missing blob <oid>` (annotated `(<index>:<name>)` under --name-objects),
    // setting ERROR_REACHABLE. The cache-tree's recorded tree oids must each be
    // valid trees, else `<oid>: invalid sha1 pointer in cache-tree of <index>`
    // sets ERROR_REFS.
    let mut index_error_bits = 0i32;
    if explicit_oids.is_empty() {
        index_error_bits |=
            fsck_index_roots(&db, format, &git_dir, name_objects, &mut roots, &bad_loose)?;
    }

    if write_lost_found {
        sley_fsck::write_lost_found(&db, format, &git_dir, &roots, &object_ids)?;
    }

    if roots.is_empty() && progress {
        eprintln!("notice: No default references");
    }
    let report = sley_fsck::fsck_objects_with_options(
        &db,
        format,
        roots,
        object_ids,
        sley_fsck::FsckOptions {
            report_dangling,
            report_unreachable,
            connectivity_only,
            object_names,
            severity: policy.severity,
            skip_objects: policy.skip_objects,
            check_content: true,
        },
    );
    // Match builtin/fsck.c's stream split: notices (dangling/unreachable) and
    // connectivity complaints (broken link, missing, type mismatch) go to
    // stdout; object-content findings (`error in`/`warning in`) go to stderr.
    for notice in &report.notices {
        println!("{}", notice.message);
    }
    for issue in &report.issues {
        match issue.stream {
            sley_fsck::IssueStream::Stdout => println!("{}", issue.message),
            sley_fsck::IssueStream::Stderr => eprintln!("{}", issue.message),
        }
    }
    // git's exit status is the OR of its `ERROR_*` bits. The connectivity
    // report contributes ERROR_OBJECT/REACHABLE/REFS; a bad loose object or a
    // bogus explicit head sets ERROR_OBJECT.
    let mut exit_bits = report.exit_code();
    if loose_errors {
        exit_bits |= sley_fsck::ERROR_OBJECT;
    }
    if alternate_loose_errors || pack_errors {
        exit_bits |= sley_fsck::ERROR_OBJECT;
    }
    // git's `snapshot_ref` errors (invalid sha1 pointer / not a commit) set
    // ERROR_REACHABLE / ERROR_REFS — not ERROR_OBJECT.
    exit_bits |= ref_error_bits;
    exit_bits |= index_error_bits;
    exit_bits |= refs_verify_bits;

    // git's fsck verifies the commit-graph when `core.commitGraph` is true (the
    // default; unset ⇒ true) by shelling out to `commit-graph verify`. We run the
    // same verification inline and OR in ERROR_COMMIT_GRAPH on any failure.
    if fsck_core_commit_graph_enabled(&git_dir) {
        let object_dir = repository_objects_dir(&git_dir);
        let graph_path = object_dir.join("info").join("commit-graph");
        if let OpenResult::Bytes(graph_bytes) = open_commit_graph_bytes(&graph_path)
            && verify_commit_graph_bytes(&object_dir, format, &graph_bytes, progress).is_err()
        {
            exit_bits |= ERROR_COMMIT_GRAPH;
        }
    }

    if fsck_core_multi_pack_index_enabled(&git_dir) {
        let object_dir = repository_objects_dir(&git_dir);
        if crate::commands::pack::verify_midx_at(&object_dir, format, progress).is_err() {
            exit_bits |= sley_fsck::ERROR_OBJECT;
        }
    }

    if exit_bits != 0 {
        Err(GitError::Exit(exit_bits))
    } else {
        Ok(())
    }
}

const ERROR_COMMIT_GRAPH: i32 = 0o20;

/// `core.commitGraph` resolved with git's default of true (an unset value enables
/// the fsck commit-graph check).
fn fsck_core_commit_graph_enabled(git_dir: &Path) -> bool {
    read_repo_config(git_dir)
        .ok()
        .and_then(|config| config.get_bool("core", None, "commitGraph"))
        .unwrap_or(true)
}

/// `core.multiPackIndex` resolved with git's default of true (an unset value
/// enables the fsck multi-pack-index check).
fn fsck_core_multi_pack_index_enabled(git_dir: &Path) -> bool {
    read_repo_config(git_dir)
        .ok()
        .and_then(|config| config.get_bool("core", None, "multiPackIndex"))
        .unwrap_or(true)
}

/// Named root refs restricted to `refs/tags/*` (for `git fsck --tags`).
fn fsck_tag_root_oids(git_dir: &Path, format: ObjectFormat) -> Result<Vec<(String, ObjectId)>> {
    let store = FileRefStore::new(git_dir, format);
    let mut seen = HashSet::new();
    let mut roots = Vec::new();
    for reference in store.list_refs()? {
        if !reference.name.starts_with("refs/tags/") {
            continue;
        }
        if let Some((oid, _)) = resolve_for_each_ref_target(&store, &reference)?
            && seen.insert(oid)
        {
            roots.push((reference.name.clone(), oid));
        }
    }
    Ok(roots)
}

/// spelling for those shapes and fall back to the absolute path.
fn fsck_objects_dir_display(git_dir: &Path, cwd: &Path) -> String {
    if git_dir == cwd {
        return "./objects".to_string();
    }
    if let Ok(relative) = git_dir.strip_prefix(cwd) {
        return format!("{}/objects", relative.display());
    }
    format!("{}/objects", git_dir.display())
}

fn fsck_display_path(path: &Path, cwd: &Path) -> String {
    if let Ok(relative) = path.strip_prefix(cwd) {
        if relative.as_os_str().is_empty() {
            ".".to_string()
        } else {
            relative.display().to_string()
        }
    } else {
        path.display().to_string()
    }
}

fn fsck_alternate_loose_objects(git_dir: &Path, format: ObjectFormat, cwd: &Path) -> Result<bool> {
    let objects_dir = repository_objects_dir(git_dir);
    let alternates = objects_dir.join("info").join("alternates");
    let Ok(contents) = fs::read_to_string(&alternates) else {
        return Ok(false);
    };
    let mut failed = false;
    for raw in contents.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let alternate = PathBuf::from(line);
        let alternate = if alternate.is_absolute() {
            alternate
        } else {
            objects_dir.join(alternate)
        };
        let store = sley_odb::LooseObjectStore::new(&alternate, format);
        for oid in store.object_ids()? {
            let hex = oid.to_hex();
            let display_path = fsck_display_path(&alternate.join(&hex[..2]).join(&hex[2..]), cwd);
            match store.verify_object(&oid, &display_path)? {
                None | Some(LooseObjectIntegrity::Ok) => {}
                Some(LooseObjectIntegrity::HashMismatch { actual }) => {
                    eprintln!("error: {actual}: hash-path mismatch, found at: {display_path}");
                    failed = true;
                }
                Some(LooseObjectIntegrity::Corrupt) => {
                    eprintln!("error: {oid}: object corrupt or missing: {display_path}");
                    failed = true;
                }
            }
        }
    }
    Ok(failed)
}

fn fsck_pack_files(git_dir: &Path, format: ObjectFormat, cwd: &Path) -> Result<bool> {
    let pack_dir = repository_objects_dir(git_dir).join("pack");
    let Ok(entries) = fs::read_dir(&pack_dir) else {
        return Ok(false);
    };
    let mut packs = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("pack"))
        .collect::<Vec<_>>();
    packs.sort();

    let mut failed = false;
    for pack_path in packs {
        let bytes = fs::read(&pack_path)?;
        let display_path = fsck_display_path(&pack_path, cwd);
        let trailer_len = format.raw_len();
        if bytes.len() >= 12 + trailer_len {
            let trailer_offset = bytes.len() - trailer_len;
            let actual = sley_core::digest_bytes(format, &bytes[..trailer_offset])?;
            let expected = ObjectId::from_raw(format, &bytes[trailer_offset..])?;
            if actual != expected {
                eprintln!("error: checksum mismatch in {display_path}");
                failed = true;
            }
        } else {
            eprintln!("error: checksum mismatch in {display_path}");
            failed = true;
            continue;
        }

        let idx_path = pack_path.with_extension("idx");
        let Ok(index_bytes) = fs::read(&idx_path) else {
            continue;
        };
        let Ok(index) = sley_pack::PackIndex::parse(&index_bytes, format) else {
            continue;
        };
        let rev_path = pack_path.with_extension("rev");
        if let Ok(reverse_bytes) = fs::read(&rev_path) {
            let validation =
                sley_pack::PackReverseIndex::parse(&reverse_bytes, format, index.entries.len())
                    .and_then(|reverse| {
                        if reverse.pack_checksum == index.pack_checksum {
                            Ok(reverse)
                        } else {
                            Err(GitError::InvalidFormat("invalid checksum".into()))
                        }
                    });
            if let Err(err) = validation {
                let display_rev = fsck_display_path(&rev_path, cwd);
                let detail = match err {
                    GitError::InvalidFormat(detail) => detail,
                    other => other.to_string(),
                };
                if detail.starts_with("invalid rev-index position") {
                    eprintln!("error: {detail} in {display_rev}");
                } else {
                    eprintln!("error: reverse-index file {display_rev} has {detail}");
                }
                failed = true;
            }
        }
        let trailer_offset = bytes.len() - trailer_len;
        for entry in index.entries {
            let Ok(offset) = usize::try_from(entry.offset) else {
                continue;
            };
            if offset >= trailer_offset {
                continue;
            }
            let object_type = (bytes[offset] >> 4) & 0x07;
            if object_type == 0 {
                eprintln!("error: unknown object type 0 at offset {offset} in {display_path}");
                failed = true;
            }
        }
    }
    Ok(failed)
}

/// git's `is_branch`: a ref whose tip must be a commit (`HEAD` or `refs/heads/*`).
fn is_branch_ref(name: &str) -> bool {
    name == "HEAD" || name.starts_with("refs/heads/")
}

fn fsck_worktree_head_refs(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    git_dir: &Path,
    name_objects: bool,
    roots: &mut Vec<ObjectId>,
    object_names: &mut std::collections::HashMap<ObjectId, String>,
) -> Result<i32> {
    let mut bits = 0i32;
    let common = common_git_dir_for_git_dir(git_dir)?;
    let mut heads: Vec<(PathBuf, String)> = Vec::new();
    heads.push((git_dir.to_path_buf(), "HEAD".to_string()));
    if common != git_dir {
        heads.push((common.clone(), "HEAD".to_string()));
    }
    let worktrees_dir = common.join("worktrees");
    if let Ok(entries) = fs::read_dir(&worktrees_dir) {
        let mut linked: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.is_dir())
            .collect();
        linked.sort();
        for path in linked {
            let Some(name) = path
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            heads.push((path, format!("worktrees/{name}/HEAD")));
        }
    }

    heads.sort_by(|left, right| left.1.cmp(&right.1));
    heads.dedup_by(|left, right| left.0 == right.0 && left.1 == right.1);

    for (head_git_dir, display) in heads {
        let store = FileRefStore::new(&head_git_dir, format);
        let Some(target) = store.read_ref("HEAD")? else {
            continue;
        };
        match target {
            RefTarget::Direct(oid) if oid.is_null() => {
                eprintln!("error: {display}: badRefOid: points to invalid object ID '{oid}'");
                bits |= sley_fsck::ERROR_REFS;
            }
            RefTarget::Direct(oid) => {
                if db.contains(&oid).unwrap_or(false) {
                    roots.push(oid);
                    if name_objects {
                        object_names.entry(oid).or_insert(display);
                    }
                } else {
                    eprintln!("error: {display}: invalid sha1 pointer {oid}");
                    bits |= sley_fsck::ERROR_REACHABLE;
                }
            }
            RefTarget::Symbolic(target) => {
                if !target.starts_with("refs/heads/") {
                    eprintln!(
                        "error: {display}: badHeadTarget: HEAD points to non-branch '{target}'"
                    );
                    bits |= sley_fsck::ERROR_REFS;
                    continue;
                }
                let reference = sley_refs::Ref {
                    name: "HEAD".to_string(),
                    target: RefTarget::Symbolic(target),
                };
                if let Some((oid, _)) = resolve_for_each_ref_target(&store, &reference)?
                    && db.contains(&oid).unwrap_or(false)
                {
                    roots.push(oid);
                    if name_objects {
                        object_names.entry(oid).or_insert(display);
                    }
                }
            }
        }
    }
    Ok(bits)
}

fn fsck_reflog_roots(db: &FileObjectDatabase, format: ObjectFormat, git_dir: &Path) -> Result<i32> {
    let store = FileRefStore::new(git_dir, format);
    let mut bits = 0i32;
    let mut seen = HashSet::new();
    for name in store.list_reflog_names()? {
        let entries = match store.read_reflog(&name) {
            Ok(entries) => entries,
            Err(GitError::NotFound(_)) => continue,
            Err(err) => return Err(err),
        };
        for entry in entries {
            for oid in [entry.old_oid, entry.new_oid] {
                if oid.is_null() || !seen.insert(oid) {
                    continue;
                }
                match db.read_object(&oid) {
                    Ok(_) => {}
                    Err(GitError::NotFound(_)) if db.is_promised_object(&oid) => {}
                    Err(GitError::NotFound(_)) => {
                        eprintln!("error: {name}: invalid reflog entry {oid}");
                        bits |= sley_fsck::ERROR_REACHABLE;
                    }
                    Err(err) => return Err(err),
                }
            }
        }
    }
    Ok(bits)
}

/// git's `fsck_index` for every worktree index: mark each entry's blob
/// reachable (appending existing ones to `roots`), report a missing blob with
/// git's `missing blob <oid>` line (annotated `(<index>:<name>)` under
/// `--name-objects`), and validate the cache-tree's recorded tree oids. Returns
/// the accumulated `ERROR_*` bits (REACHABLE for a missing index blob, REFS for
/// an invalid cache-tree pointer).
fn fsck_index_roots(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    git_dir: &Path,
    name_objects: bool,
    roots: &mut Vec<ObjectId>,
    bad_loose: &HashSet<ObjectId>,
) -> Result<i32> {
    let mut bits = 0i32;
    // The current worktree's index (annotation prefix ""), then each linked
    // worktree's index (annotation prefix `<index-path>`), mirroring git's
    // get_worktrees() order with the current worktree's blank filename.
    let mut indexes: Vec<(PathBuf, bool, String)> = Vec::new();
    let current_index = sley_worktree::repository_index_path(git_dir);
    indexes.push((current_index, true, String::new()));
    // Linked worktrees: <common_git_dir>/worktrees/<name>/index. Their reports
    // carry the index path (relative to the cwd-rooted .git when possible).
    if let Ok(common) = common_git_dir_for_git_dir(git_dir) {
        let worktrees_dir = common.join("worktrees");
        if let Ok(entries) = fs::read_dir(&worktrees_dir) {
            let mut linked: Vec<PathBuf> = entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.is_dir())
                .collect();
            linked.sort();
            for wt in linked {
                let index_path = wt.join("index");
                if index_path.exists() {
                    let display = fsck_index_display_path(git_dir, &index_path);
                    indexes.push((index_path, false, display));
                }
            }
        }
    }

    for (index_path, _is_current, annotation_prefix) in indexes {
        if !index_path.exists() {
            continue;
        }
        let index_bytes = fs::read(&index_path)?;
        // git's `verify_hdr` (with verify_index_checksum set for a full/`--cache`
        // fsck) rejects a bad trailing SHA: `error: bad index file sha1
        // signature` + `fatal: index file corrupt`, setting ERROR_OBJECT.
        if !index_checksum_ok(&index_bytes, format) {
            eprintln!("error: bad index file sha1 signature");
            eprintln!("fatal: index file corrupt");
            bits |= sley_fsck::ERROR_OBJECT;
            continue;
        }
        let index = match Index::parse(&index_bytes, format) {
            Ok(index) => index,
            Err(_) => continue,
        };
        for entry in &index.entries {
            // git skips gitlinks (S_ISGITLINK) in the index walk.
            if entry.mode == 0o160000 {
                continue;
            }
            let oid = entry.oid.clone();
            if db.contains(&oid).unwrap_or(false) && !bad_loose.contains(&oid) {
                // Present: mark reachable so it is not reported as dangling.
                roots.push(oid);
                continue;
            }
            // A missing index blob covered by a promisor remote is fine in a
            // partial clone: git's `fsck_index` marks it via `mark_object`,
            // which returns early for promisor objects without reporting.
            if db.is_promised_object(&oid) {
                continue;
            }
            // Missing index blob. git: `missing blob <oid>` (stdout),
            // ERROR_REACHABLE; `--name-objects` appends `(<prefix>:<name>)`.
            if name_objects {
                let name = String::from_utf8_lossy(entry.path.as_ref());
                println!("missing blob {oid} ({annotation_prefix}:{name})");
            } else {
                println!("missing blob {oid}");
            }
            bits |= sley_fsck::ERROR_REACHABLE;
        }
        // Cache-tree: each recorded (non-invalidated) subtree oid must be a
        // valid tree. git: `<oid>: invalid sha1 pointer in cache-tree of
        // <index>` + ERROR_REFS for an unparseable pointer.
        if let Ok(Some(cache_tree)) = index.cache_tree(format) {
            bits |= fsck_cache_tree(db, &cache_tree, &index_path, roots);
        }
    }
    Ok(bits)
}

/// Recursively validate a cache-tree node: a node with a valid (>=0) entry
/// count records a tree oid that must resolve to a tree object. Appends valid
/// tree oids to `roots` so they are marked reachable.
fn fsck_cache_tree(
    db: &FileObjectDatabase,
    node: &sley_index::CacheTree,
    index_path: &Path,
    roots: &mut Vec<ObjectId>,
) -> i32 {
    let mut bits = 0i32;
    if node.entry_count >= 0
        && let Some(oid) = &node.oid
    {
        match db.read_object(oid) {
            Ok(object) if object.object_type == sley_object::ObjectType::Tree => {
                roots.push(oid.clone());
            }
            Ok(_) => {
                // Present but not a tree: git's `non-tree in cache-tree`.
                eprintln!("error in cache-tree of {}: non-tree", index_path.display());
                bits |= sley_fsck::ERROR_OBJECT;
            }
            Err(_) => {
                eprintln!(
                    "error: {oid}: invalid sha1 pointer in cache-tree of {}",
                    index_path.display()
                );
                bits |= sley_fsck::ERROR_REFS;
            }
        }
    }
    for child in &node.subtrees {
        bits |= fsck_cache_tree(db, &child.tree, index_path, roots);
    }
    bits
}

/// Whether an index file's trailing hash matches the digest of its body, git's
/// `verify_hdr` checksum check. A too-short file (no room for the trailing hash)
/// is treated as a checksum failure.
fn index_checksum_ok(bytes: &[u8], format: ObjectFormat) -> bool {
    let hash_len = format.raw_len();
    if bytes.len() < 12 + hash_len {
        return false;
    }
    let split = bytes.len() - hash_len;
    // git's verify_hdr accepts a null trailing hash: it marks an `index.skipHash`
    // index whose checksum was deliberately not computed.
    if bytes[split..].iter().all(|byte| *byte == 0) {
        return true;
    }
    match sley_core::digest_bytes(format, &bytes[..split]) {
        Ok(actual) => actual.as_bytes() == &bytes[split..],
        Err(_) => false,
    }
}

/// The path string git prints for a linked worktree's index in fsck reports:
/// relative to the cwd-rooted `.git` when the index lives under it, else the
/// absolute path.
fn fsck_index_display_path(git_dir: &Path, index_path: &Path) -> String {
    if let Ok(cwd) = env::current_dir()
        && let Ok(rel) = git_dir.strip_prefix(&cwd)
        && let Ok(suffix) = index_path.strip_prefix(git_dir)
    {
        return format!("{}/{}", rel.display(), suffix.display());
    }
    index_path.display().to_string()
}

/// True when the repository has a promisor remote configured, mirroring git's
/// `repo_has_promisor_remote`: either `extensions.partialclone` names a default
/// promisor remote, or some `remote.<name>.promisor` is true. Only then does git
/// treat objects in `.promisor` packs as legitimately-absent "promised" objects.
pub(crate) fn repo_has_promisor_remote(config: &GitConfig) -> bool {
    if config
        .get("extensions", None, "partialclone")
        .is_some_and(|value| !value.is_empty())
    {
        return true;
    }
    crate::commands::remote::remote_names(config)
        .into_iter()
        .any(|name| config.get_bool("remote", Some(&name), "promisor") == Some(true))
}

/// Named root refs for a full fsck: every ref (and HEAD), each as
/// `(refname, target_oid)`. The driver validates each against git's
/// `snapshot_ref` rules (parseable object, branch→commit) before walking.
fn fsck_root_oids(git_dir: &Path, format: ObjectFormat) -> Result<Vec<(String, ObjectId)>> {
    let store = FileRefStore::new(git_dir, format);
    let mut seen = HashSet::new();
    let mut roots = Vec::new();
    for reference in store.list_refs()? {
        if let Some((oid, _)) = resolve_for_each_ref_target(&store, &reference)?
            && seen.insert(oid)
        {
            roots.push((reference.name.clone(), oid));
        }
    }
    // git resolves HEAD after the ref iteration (its worktree-HEAD pass).
    if let Some(target) = store.read_ref("HEAD")? {
        let reference = Ref {
            name: "HEAD".to_string(),
            target,
        };
        if let Some((oid, _)) = resolve_for_each_ref_target(&store, &reference)?
            && seen.insert(oid)
        {
            roots.push(("HEAD".to_string(), oid));
        }
    }
    Ok(roots)
}
