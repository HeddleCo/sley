//! Extracted from the crate root (sley#8 phase 1) — code motion only.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use crate::*;
use sley::plumbing::{sley_core, sley_pack, sley_rev, sley_worktree};

use super::add::add_intent_to_add;

enum ApplyAction {
    Write {
        path: Vec<u8>,
        /// Worktree mode (executable bit / symlink) used when materialising.
        mode: u32,
        /// Canonical mode for the index entry (`--index`/`--cached`).
        index_mode: u32,
        content: Vec<u8>,
    },
    Remove {
        path: Vec<u8>,
    },
    /// Add or update a gitlink (submodule) entry: the index records mode 160000
    /// and the commit oid, and the working tree gets an (empty) directory. No
    /// blob is written (git's `add_index_file` / `try_create_file` gitlink arms).
    Gitlink {
        path: Vec<u8>,
        oid: ObjectId,
    },
    /// Remove a gitlink entry from the index, leaving its working-tree directory
    /// in place (git's `remove_or_warn` rmdir's a submodule only when empty).
    GitlinkRemove {
        path: Vec<u8>,
    },
}

/// git's `canon_mode`: the index never stores arbitrary permission bits — a
/// regular file is `100644` or `100755` (owner-exec bit only), a symlink
/// `120000`, a gitlink `160000`.
fn canon_mode(mode: u32) -> u32 {
    match mode & 0o170000 {
        0o100000 | 0 => {
            if mode & 0o100 != 0 {
                0o100755
            } else {
                0o100644
            }
        }
        0o120000 => 0o120000,
        0o040000 => 0o040000,
        _ => 0o160000,
    }
}

/// `git apply --whitespace=<action>` modes (apply.c's `ws_error_action`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum WsAction {
    /// `nowarn`: ignore whitespace errors entirely.
    Nowarn,
    /// `warn` (default with `--apply`): warn but still apply.
    Warn,
    /// `error`: warn and refuse to apply.
    Error,
    /// `error-all`: like `error` but do not squelch repeated warnings.
    ErrorAll,
    /// `fix`/`strip`: correct whitespace errors as the patch is applied.
    Fix,
}

struct ApplyContext {
    cwd: PathBuf,
    git_dir: PathBuf,
    worktree_root: PathBuf,
    format: ObjectFormat,
    objects: FileObjectDatabase,
    config: GitConfig,
    lazy_fetch: bool,
}

impl ApplyContext {
    fn open(cli_session: &crate::session::CliSession, require_repository: bool) -> Result<Self> {
        let cwd = cli_session.cwd().to_path_buf();
        let repository = cli_session.open_repository().ok();
        if repository.is_none() && require_repository {
            return Err(GitError::repository_not_found("not a git repository"));
        }
        let (git_dir, worktree_root, format, objects, config) = match repository {
            Some(repository) => {
                let git_dir = repository.git_dir().to_path_buf();
                let worktree_root = repository.workdir().ok_or_else(|| {
                    GitError::Unsupported("apply requires a repository worktree".into())
                })?;
                let format = repository.object_format();
                let objects = repository.objects_mut();
                let config = read_repo_config(&git_dir)?;
                (git_dir, worktree_root, format, objects, config)
            }
            None => {
                let git_dir = cwd.join(".git");
                let context = sley_config::ConfigIncludeContext::new(None, None);
                let mut config = sley_config::load_pre_dispatch_config(None, &context)
                    .map_err(report_config_setup_error)?;
                let parameters = injected_config_parameters()?;
                sley_config::append_injected_config_sections_with_includes(
                    &mut config,
                    &parameters,
                    &context,
                    &cwd,
                )
                .map_err(report_config_setup_error)?;
                (
                    git_dir.clone(),
                    cwd.clone(),
                    ObjectFormat::Sha1,
                    FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1),
                    config,
                )
            }
        };
        Ok(Self {
            cwd,
            git_dir,
            worktree_root,
            format,
            objects,
            config,
            lazy_fetch: cli_session.lazy_fetch(),
        })
    }
}

pub(crate) fn cmd_apply(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let mut check = false;
    let mut apply = false;
    let mut stat = false;
    let mut numstat = false;
    let mut summary = false;
    let mut recount = false;
    let mut update_index = false;
    let mut cached = false;
    let mut three_way = false;
    let mut merge_favor = sley_diff_merge::MergeFavor::None;
    let mut union = false;
    let mut intent_to_add = false;
    let mut build_fake_ancestor: Option<String> = None;
    let mut files = Vec::new();
    // git's default when applying is `warn`; the value is overridden by the
    // last `--whitespace=` seen.
    let mut ws_action = WsAction::Warn;
    // `-p<n>` strip count (git's `p_value`), `--directory=<dir>` root, and the
    // `--unsafe-paths` gate that allows writing outside the working tree.
    let mut p_value: usize = 1;
    let mut p_value_known = false;
    let mut directory_root: Vec<u8> = Vec::new();
    let mut unsafe_paths = false;
    let mut unidiff_zero = false;
    let mut reverse = false;
    let mut reject = false;
    let mut ignore_space_change = false;
    // Whether `--whitespace=` was given on the command line; when not, the
    // default action comes from `apply.whitespace` config (git's precedence).
    let mut ws_action_explicit = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--check" => check = true,
            "--apply" => apply = true,
            "--stat" => stat = true,
            "--numstat" => numstat = true,
            "--summary" => summary = true,
            "--recount" => recount = true,
            "-q"
            | "--quiet"
            | "-v"
            | "--verbose"
            | "--allow-empty"
            | "-l"
            // Historical no-ops kept for compatibility: binary patches always
            // apply when the data/index is present (git's OPT_HIDDEN aliases).
            | "--allow-binary-replacement"
            | "--binary" => {}
            // git's `ignore_ws_change`: match context lines ignoring whitespace
            // differences (collapsing runs); applies the post-image as written.
            "--ignore-whitespace" | "--ignore-space-change" => ignore_space_change = true,
            "--no-ignore-whitespace" => ignore_space_change = false,
            "--unsafe-paths" => unsafe_paths = true,
            "--no-unsafe-paths" => unsafe_paths = false,
            "--unidiff-zero" => unidiff_zero = true,
            "-R" | "--reverse" => reverse = true,
            "--no-reverse" => reverse = false,
            "--reject" => reject = true,
            "--no-reject" => reject = false,
            "--index" => update_index = true,
            "--cached" => cached = true,
            "-N" | "--intent-to-add" => intent_to_add = true,
            "--no-intent-to-add" => intent_to_add = false,
            "--build-fake-ancestor" => {
                let Some(path) = iter.next() else {
                    return Err(GitError::Command(
                        "apply --build-fake-ancestor requires a value".into(),
                    ));
                };
                build_fake_ancestor = Some(path.to_string());
            }
            "-3" | "--3way" => three_way = true,
            "--no-3way" => three_way = false,
            "--ours" => merge_favor = sley_diff_merge::MergeFavor::Ours,
            "--theirs" => merge_favor = sley_diff_merge::MergeFavor::Theirs,
            "--union" => union = true,
            "--whitespace" => {
                if let Some(value) = iter.next() {
                    ws_action = parse_ws_action(value)?;
                    ws_action_explicit = true;
                }
            }
            "-p" => {
                let Some(value) = iter.next() else {
                    return Err(GitError::Command("apply -p requires a value".into()));
                };
                p_value = parse_apply_p_value(value)?;
                p_value_known = true;
            }
            "--directory" => {
                let Some(value) = iter.next() else {
                    return Err(GitError::Command("apply --directory requires a value".into()));
                };
                directory_root = normalize_apply_directory(value)?;
            }
            "-C" | "--exclude" | "--include" => {
                iter.next();
            }
            "--" => {
                files.extend(iter.by_ref().map(|value| value.to_string()));
                break;
            }
            value if let Some(rest) = value.strip_prefix("--whitespace=") => {
                ws_action = parse_ws_action(rest)?;
                ws_action_explicit = true;
            }
            value if let Some(path) = value.strip_prefix("--build-fake-ancestor=") => {
                build_fake_ancestor = Some(path.to_string());
            }
            value if let Some(rest) = value.strip_prefix("--directory=") => {
                directory_root = normalize_apply_directory(rest)?;
            }
            value if let Some(rest) = value.strip_prefix("-p") => {
                p_value = parse_apply_p_value(rest)?;
                p_value_known = true;
            }
            value
                if value.starts_with("--exclude=") || value.starts_with("--include=") => {}
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!(
                    "unsupported apply option {value}"
                )));
            }
            value => files.push(value.to_string()),
        }
    }
    // git's `apply_state_init`: `--reject` and `--3way` are mutually exclusive.
    if reject && three_way {
        eprintln!("error: options '--reject' and '--3way' cannot be used together");
        return Err(GitError::Exit(128));
    }
    // Plain textual apply remains usable outside a repository. Index/object
    // modes require the optional session repository to have opened.
    let require_repository =
        update_index || cached || three_way || intent_to_add || build_fake_ancestor.is_some();
    let apply_context = ApplyContext::open(cli_session, require_repository)?;
    let cwd = &apply_context.cwd;
    let git_dir = &apply_context.git_dir;
    let worktree_root = &apply_context.worktree_root;
    let format = apply_context.format;
    let db = &apply_context.objects;
    // git's `state->prefix`: the current directory relative to the work tree,
    // with a trailing slash (empty at the top level). Prepended to the names of
    // non-toplevel-relative (traditional) patches so that `git apply` from a
    // subdirectory operates on files under that subdirectory. Both paths are
    // canonicalised so a symlinked temp/work tree does not defeat the strip.
    // git's setup: when the cwd is inside the git directory itself (e.g. running
    // from `.git` or `.git/objects`), there is no worktree prefix — pathnames in a
    // traditional patch resolve relative to the top level (so an index entry is
    // `file`, not `.git/file`), but a plain (non-`--cached`/`--index`) apply still
    // reads/writes the *worktree* file relative to the actual cwd (`.git/file`).
    let canonical_cwd = fs::canonicalize(&cwd).unwrap_or_else(|_| cwd.clone());
    let canonical_git_dir = fs::canonicalize(&git_dir).unwrap_or_else(|_| git_dir.clone());
    let cwd_in_git_dir =
        canonical_cwd == canonical_git_dir || canonical_cwd.starts_with(&canonical_git_dir);
    let prefix: Vec<u8> = if cwd_in_git_dir {
        Vec::new()
    } else {
        let canonical_root =
            fs::canonicalize(&worktree_root).unwrap_or_else(|_| worktree_root.clone());
        canonical_cwd
            .strip_prefix(&canonical_root)
            .ok()
            .map(|rel| rel.to_string_lossy().into_owned())
            .filter(|rel| !rel.is_empty())
            .map(|mut rel| {
                if !rel.ends_with('/') {
                    rel.push('/');
                }
                rel.into_bytes()
            })
            .unwrap_or_default()
    };
    // Base directory worktree files are resolved against. Normally the worktree
    // top (the patch name already carries the cwd prefix); when the cwd is inside
    // the git dir the name carries no prefix, so worktree files live under the cwd.
    let worktree_base: PathBuf = if cwd_in_git_dir {
        cwd.clone()
    } else {
        worktree_root.clone()
    };
    let repo_config = &apply_context.config;
    let trust_filemode = repo_config
        .get_bool("core", None, "fileMode")
        .unwrap_or(true);
    let ws_resolver = commands::diff::WhitespaceRuleResolver::from_git_dir_with_config(
        git_dir,
        Some(repo_config),
    )?;
    // `apply.whitespace` config supplies the default whitespace action when the
    // command line did not give an explicit `--whitespace=`.
    if !ws_action_explicit
        && let Some(value) = repo_config.get("apply", None, "whitespace")
        && let Ok(action) = parse_ws_action(value)
    {
        ws_action = action;
    }
    let path_options = sley_diff_merge::PatchPathOptions {
        p_value,
        p_value_known,
        root: directory_root.clone(),
        prefix: prefix.clone(),
    };
    let inputs = read_apply_inputs(&files)?;
    let mut patches = Vec::new();
    for (name, input) in &inputs {
        validate_apply_input(input, name)?;
        patches.extend(
            sley_diff_merge::parse_unified_patch_with_options(input, recount, &path_options)
                .map_err(|err| match err {
                    GitError::InvalidFormat(message)
                        if message.starts_with("malformed hunk header") =>
                    {
                        apply_corrupt_patch_error(input, name)
                    }
                    GitError::InvalidFormat(message)
                        if message.starts_with("corrupt-hunk-body:") =>
                    {
                        let line = message.strip_prefix("corrupt-hunk-body:").unwrap_or("1");
                        eprintln!("error: corrupt patch at {name}:{line}");
                        GitError::Exit(1)
                    }
                    GitError::InvalidFormat(message)
                        if message.starts_with("git diff header lacks filename") =>
                    {
                        eprintln!("error: {message}");
                        GitError::Exit(1)
                    }
                    GitError::InvalidFormat(message)
                        if message.starts_with("unable to find filename in patch") =>
                    {
                        eprintln!("error: {message}");
                        GitError::Exit(1)
                    }
                    GitError::InvalidFormat(message) if message.starts_with("binary-corrupt:") => {
                        let line = message.strip_prefix("binary-corrupt:").unwrap_or("");
                        eprintln!("error: corrupt binary patch at {name}:{line}: ");
                        GitError::Exit(128)
                    }
                    GitError::InvalidFormat(message)
                        if message.starts_with("binary-unrecognized:") =>
                    {
                        let line = message.strip_prefix("binary-unrecognized:").unwrap_or("");
                        eprintln!("error: unrecognized binary patch at {name}:{line}");
                        eprintln!(
                            "error: No valid patches in input (allow with \"--allow-empty\")"
                        );
                        GitError::Exit(128)
                    }
                    GitError::InvalidFormat(message)
                        if message.starts_with("invalid mode on line") =>
                    {
                        eprintln!("error: {message}");
                        GitError::Exit(128)
                    }
                    other => other,
                })?,
        );
    }
    if !recount && patches.iter().any(apply_patch_is_noop) {
        return Err(GitError::Exit(1));
    }
    // `-R`/`--reverse`: undo the patch by reversing each file patch before any
    // whitespace handling or application (git reverses the parsed patches up
    // front).
    if reverse {
        patches = patches
            .iter()
            .map(sley_diff_merge::reverse_file_patch)
            .collect();
    }
    // git's `prefix_patch` + `use_patch`: prepend the cwd prefix to every
    // non-toplevel-relative (traditional) patch, then drop any patch whose
    // resolved name does not live under the prefix.
    if !prefix.is_empty() {
        for patch in &mut patches {
            if patch.is_toplevel_relative {
                continue;
            }
            for name in [patch.old_path.as_mut(), patch.new_path.as_mut()]
                .into_iter()
                .flatten()
            {
                let mut prefixed = prefix.clone();
                prefixed.extend_from_slice(name);
                *name = prefixed;
            }
        }
        patches.retain(|patch| {
            let name = patch
                .new_path
                .as_deref()
                .or(patch.old_path.as_deref())
                .unwrap_or(b"");
            name.starts_with(&prefix) && name.len() > prefix.len()
        });
    }
    if let Some(path) = build_fake_ancestor {
        write_apply_fake_ancestor_index(git_dir, format, &patches, &inputs, &path)?;
        return Ok(());
    }
    if stat || numstat || summary {
        write_apply_read_only_output(&patches, stat, numstat, summary)?;
        if !apply {
            return Ok(());
        }
    }
    let patch_input_file = inputs
        .first()
        .map(|(name, _)| name.as_str())
        .unwrap_or("<stdin>");

    // When `--index`/`--cached` is in effect, read the current index once: it
    // supplies the preimage for every patch (`git apply` reads the staged blob,
    // not the worktree, under `--cached`/`--index`), the entry modes that feed
    // the canonical index-mode decision, and the type-mismatch warnings. `--index`
    // (but not `--cached`) also requires the worktree to match the index.
    // `--3way` is git's `check_index` (it reads and writes the index too), but
    // sley's three-way path manages its own index writes; the direct fall-through
    // only needs the index when a gitlink patch is present (the 3-way merge engine
    // defers gitlinks to the direct apply, mirroring git's `try_threeway` early-out).
    let has_gitlink_patch = patches.iter().any(apply_patch_is_gitlink);
    let touch_index = update_index || cached || (three_way && has_gitlink_patch);
    let verify_worktree_match = update_index && !cached;
    let mut index = if touch_index {
        Some(read_apply_index(git_dir, format)?)
    } else {
        None
    };
    let index_modes: HashMap<Vec<u8>, u32> = index
        .as_ref()
        .map(|index| {
            index
                .entries
                .iter()
                .filter(|entry| (entry.flags >> 12) & 0x3 == 0)
                .map(|entry| (entry.path.to_vec(), entry.mode))
                .collect()
        })
        .unwrap_or_default();

    // Path-safety: refuse to read/create/delete files that escape the working
    // tree. This must run before whitespace/preimage reads so `--index` and
    // `--cached` report "beyond a symbolic link" instead of failing earlier on
    // an index lookup for the path below the symlink. git turns `--unsafe-paths`
    // off whenever the index is touched (`--index`/`--cached`), so the gate only
    // relaxes for pure worktree applies.
    let unsafe_paths = unsafe_paths && !touch_index;
    check_apply_path_safety(&worktree_base, &patches, unsafe_paths, index.as_ref())?;

    // Phase 0: whitespace handling. Resolve the per-path rule, then warn/error
    // or fix the introduced (`+`) lines per `--whitespace=<action>`. In `fix`
    // mode this rewrites the patch's Insert lines (and trims new blank lines at
    // EOF) before it is applied. In `error`/`error-all` mode a whitespace error
    // aborts the whole apply.
    let mut ws_error_count = 0usize;
    let mut ws_squelched = 0usize;
    let squelch_limit = if matches!(ws_action, WsAction::ErrorAll) {
        usize::MAX
    } else {
        5
    };
    if !matches!(ws_action, WsAction::Nowarn) {
        for patch in &mut patches {
            // Binary patches carry no textual hunks to whitespace-check.
            if patch.is_binary {
                continue;
            }
            let target = patch
                .new_path
                .as_deref()
                .or(patch.old_path.as_deref())
                .unwrap_or(b"");
            let mut rule = ws_resolver.rule_for_path(target)?;
            // A symlink's incomplete line is not news (apply.c clears it). git
            // uses the post-image mode (new_mode, else old_mode — the index-line
            // mode lands in old_mode for a content-only symlink patch).
            if patch.new_mode.or(patch.old_mode) == Some(0o120000) {
                rule &= !sley_diff_merge::ws::WS_INCOMPLETE_LINE;
            }
            let base = read_patch_base(
                &worktree_base,
                worktree_root,
                git_dir,
                format,
                repo_config,
                patch,
                index.as_ref(),
                db,
                verify_worktree_match,
            )?;
            apply_patch_whitespace(
                patch,
                &base,
                rule,
                ws_action,
                patch_input_file,
                squelch_limit,
                &mut ws_error_count,
                &mut ws_squelched,
            );
        }
    }
    if ws_squelched > 0 {
        eprintln!(
            "warning: squelched {ws_squelched} whitespace error{}",
            if ws_squelched == 1 { "" } else { "s" }
        );
    }
    if ws_error_count > 0 {
        let n = ws_error_count;
        // git's `%d line(s) add(s)/applied` plural forms. The "errors" word is
        // always plural in this message, even for a single line.
        let adds = if n == 1 { "line adds" } else { "lines add" };
        match ws_action {
            // `die_on_ws_error`: an `error:`-prefixed summary, then a non-zero exit.
            WsAction::Error | WsAction::ErrorAll => {
                eprintln!("error: {n} {adds} whitespace errors.");
            }
            WsAction::Fix => {
                let applied = if n == 1 {
                    "line applied"
                } else {
                    "lines applied"
                };
                eprintln!("warning: {n} {applied} after fixing whitespace errors.");
            }
            _ => {
                eprintln!("warning: {n} {adds} whitespace errors.");
            }
        }
    }
    if ws_error_count > 0 && matches!(ws_action, WsAction::Error | WsAction::ErrorAll) {
        return Err(GitError::Exit(1));
    }
    let patches = patches;

    // git's `check_to_create`: a newly created path (or rename/copy target) must
    // not already exist in the index or working tree, unless another patch in the
    // batch removes it first (the type-change split: delete old, then create new
    // at the same path — git's `ok_if_exists` via `was_deleted`/`to_be_deleted`).
    // Scoped to submodule diffs so the long-standing lenient behaviour of plain
    // applies is unchanged; this is what makes the "replace submodule with a
    // directory must fail" / untracked-file-in-the-way cases abort like git.
    if has_gitlink_patch && !check {
        apply_check_to_create(
            &worktree_base,
            &patches,
            index.as_ref(),
            touch_index,
            cached,
        )?;
    }

    // `--3way`: reconstruct the recorded pre-image, apply the patch to it to form
    // "theirs", and 3-way merge against the current state. Falls through to the
    // direct apply below when the pre-image blobs are not available.
    if three_way
        && apply_three_way_path(
            git_dir,
            worktree_root,
            format,
            db,
            repo_config,
            &patches,
            cached,
            check,
            merge_favor,
            union,
            apply_context.lazy_fetch,
        )?
    {
        return Ok(());
    }

    // Phase 1: compute every result first (git applies a patch atomically).
    let mut actions = Vec::new();
    // `--reject`: rejected-hunk `.rej` writeouts collected here (path, bytes), and
    // a flag so the whole command exits 1 at the end (git's `apply_with_reject`).
    let mut reject_writes: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut had_reject = false;
    let apply_file_options = sley_diff_merge::ApplyFileOptions { unidiff_zero };
    for patch in &patches {
        // Gitlink (submodule) patch: mode 160000 + `Subproject commit <sha>` body.
        // git updates the index gitlink entry from the recorded commit oid (no
        // blob is written) and, in the working tree, just ensures an (empty)
        // directory exists; a removal drops the index entry but leaves the
        // submodule directory in place.
        if apply_patch_is_gitlink(patch) {
            if patch.is_delete {
                if let Some(old) = &patch.old_path {
                    actions.push(ApplyAction::GitlinkRemove { path: old.clone() });
                }
                continue;
            }
            let base = read_patch_base(
                &worktree_base,
                worktree_root,
                git_dir,
                format,
                repo_config,
                patch,
                index.as_ref(),
                db,
                verify_worktree_match,
            )?;
            let content = match sley_diff_merge::apply_file_patch_with_options(
                &base,
                patch,
                &apply_file_options,
            ) {
                sley_diff_merge::ApplyOutcome::Applied(content) => content,
                sley_diff_merge::ApplyOutcome::Rejected => {
                    let name = patch
                        .new_path
                        .as_deref()
                        .or(patch.old_path.as_deref())
                        .unwrap_or(b"");
                    eprintln!("error: patch failed: {}", String::from_utf8_lossy(name));
                    return Err(GitError::Exit(1));
                }
            };
            let Some(target) = patch.new_path.clone().or_else(|| patch.old_path.clone()) else {
                return Err(GitError::InvalidFormat("patch missing target path".into()));
            };
            let oid = apply_gitlink_oid_from_content(&content, format, &target)?;
            // file→gitlink type-change: git splits this into a delete (of the
            // regular file) followed by a gitlink create; sley's own diff may
            // instead emit one mode-change patch. Remove the old working-tree
            // file first so the gitlink directory can be created in its place.
            // Skipped when the old side is itself a gitlink (a normal modify) or
            // when the path is newly added.
            if !patch.is_new
                && patch.old_mode.is_some_and(|mode| mode != 0o160000)
                && let Some(old) = &patch.old_path
            {
                actions.push(ApplyAction::Remove { path: old.clone() });
            }
            actions.push(ApplyAction::Gitlink { path: target, oid });
            continue;
        }
        let base = read_patch_base(
            &worktree_base,
            worktree_root,
            git_dir,
            format,
            repo_config,
            patch,
            index.as_ref(),
            db,
            verify_worktree_match,
        )?;
        // Binary patches reconstruct the postimage from the recorded blob OIDs
        // (and the `GIT binary patch` payload), not from textual hunks.
        if patch.is_binary {
            match apply_binary_outcome(db, format, patch, &base)? {
                BinaryApply::Deletion => {
                    if let Some(old) = &patch.old_path {
                        actions.push(ApplyAction::Remove { path: old.clone() });
                    }
                }
                BinaryApply::Content(content) => {
                    let Some(target) = patch.new_path.clone().or_else(|| patch.old_path.clone())
                    else {
                        return Err(GitError::InvalidFormat("patch missing target path".into()));
                    };
                    let mode = apply_write_mode(&worktree_base, patch, &target, trust_filemode)?;
                    let index_mode = apply_index_mode_and_warn(patch, &target, mode, &index_modes);
                    let mode = apply_worktree_mode_for_index_apply(
                        mode,
                        index_mode,
                        patch,
                        update_index,
                        cached,
                    );
                    actions.push(ApplyAction::Write {
                        path: target,
                        mode,
                        index_mode,
                        content,
                    });
                    if patch.is_rename
                        && let Some(old) = &patch.old_path
                    {
                        actions.push(ApplyAction::Remove { path: old.clone() });
                    }
                }
            }
            continue;
        }
        // `--reject` applies hunk-by-hunk and keeps going past a failing hunk,
        // writing the rejects to `<file>.rej`; the default path is all-or-nothing.
        let (content, rejected_hunks) = if reject {
            let result =
                sley_diff_merge::apply_file_patch_rejecting(&base, patch, &apply_file_options);
            (result.content, result.rejected)
        } else if matches!(ws_action, WsAction::Fix) || ignore_space_change {
            // `--whitespace=fix` / `--ignore-space-change`: match (and, in fix mode,
            // whitespace-correct) context lines and trim blank lines added at EOF.
            let target = patch
                .new_path
                .as_deref()
                .or(patch.old_path.as_deref())
                .unwrap_or(b"");
            let mut rule = ws_resolver.rule_for_path(target)?;
            if patch.new_mode.or(patch.old_mode) == Some(0o120000) {
                rule &= !sley_diff_merge::ws::WS_INCOMPLETE_LINE;
            }
            let ws_opts = sley_diff_merge::WsApplyOptions {
                unidiff_zero,
                ws_rule: rule,
                ws_fix: matches!(ws_action, WsAction::Fix),
                ws_ignore_change: ignore_space_change,
            };
            match sley_diff_merge::apply_file_patch_ws(&base, patch, &ws_opts) {
                sley_diff_merge::WsApplyOutcome::Applied { content, .. } => (content, Vec::new()),
                sley_diff_merge::WsApplyOutcome::Rejected => {
                    let name = patch
                        .new_path
                        .as_deref()
                        .or(patch.old_path.as_deref())
                        .unwrap_or(b"");
                    eprintln!("error: patch failed: {}", String::from_utf8_lossy(name));
                    return Err(GitError::Exit(1));
                }
            }
        } else {
            match sley_diff_merge::apply_file_patch_with_options(&base, patch, &apply_file_options)
            {
                sley_diff_merge::ApplyOutcome::Applied(content) => (content, Vec::new()),
                sley_diff_merge::ApplyOutcome::Rejected => {
                    let name = patch
                        .new_path
                        .as_deref()
                        .or(patch.old_path.as_deref())
                        .unwrap_or(b"");
                    eprintln!("error: patch failed: {}", String::from_utf8_lossy(name));
                    return Err(GitError::Exit(1));
                }
            }
        };
        if !rejected_hunks.is_empty() {
            had_reject = true;
            let rej_target = patch
                .new_path
                .as_deref()
                .or(patch.old_path.as_deref())
                .unwrap_or(b"")
                .to_vec();
            // git's `write_out_one_reject`: a deliberately non-`--git` header
            // (`diff a/X b/X\t(rejected hunks)`) followed by each rejected hunk's
            // raw unified text.
            let mut rej = Vec::new();
            rej.extend_from_slice(b"diff a/");
            rej.extend_from_slice(&rej_target);
            rej.extend_from_slice(b" b/");
            rej.extend_from_slice(&rej_target);
            rej.extend_from_slice(b"\t(rejected hunks)\n");
            for &index in &rejected_hunks {
                rej.extend_from_slice(&sley_diff_merge::render_reject_hunk(&patch.hunks[index]));
            }
            reject_writes.push((rej_target, rej));
            apply_say_reject(patch, &rejected_hunks, patch.hunks.len());
        }
        // A clean deletion removes the file; otherwise (modify/rename/new, or a
        // partial `--reject` apply) write the resulting bytes.
        if patch.is_delete && rejected_hunks.is_empty() {
            if let Some(old) = &patch.old_path {
                actions.push(ApplyAction::Remove { path: old.clone() });
            }
        } else {
            let Some(target) = patch.new_path.clone().or_else(|| patch.old_path.clone()) else {
                return Err(GitError::InvalidFormat("patch missing target path".into()));
            };
            let mode = apply_write_mode(&worktree_base, patch, &target, trust_filemode)?;
            let index_mode = apply_index_mode_and_warn(patch, &target, mode, &index_modes);
            let mode =
                apply_worktree_mode_for_index_apply(mode, index_mode, patch, update_index, cached);
            actions.push(ApplyAction::Write {
                path: target,
                mode,
                index_mode,
                content,
            });
            if patch.is_rename
                && let Some(old) = &patch.old_path
            {
                actions.push(ApplyAction::Remove { path: old.clone() });
            }
        }
    }

    if check {
        return Ok(());
    }
    // Phase 2: materialize. `--cached` updates only the index; `--index` updates
    // both the worktree and the index; a plain apply updates only the worktree.
    // Worktree files are moded `(exec ? 0777 : 0666) & ~umask` like git, so derive
    // the umask once (skipped for `--cached`, which never touches the worktree).
    let umask_complement = if cached {
        0o755
    } else {
        worktree_umask_complement(&worktree_base)
    };
    let mut index_paths = Vec::new();
    let mut index_mutations = Vec::new();
    // git's `write_out_results` runs in two phases: every removal happens before
    // any creation. This matters when a directory's tracked children are removed
    // and the directory is then (re)created as a gitlink — single-phase ordering
    // would prune the just-emptied directory after the create and lose it.
    let is_remove = |action: &&ApplyAction| {
        matches!(
            action,
            ApplyAction::Remove { .. } | ApplyAction::GitlinkRemove { .. }
        )
    };
    let mut ordered: Vec<&ApplyAction> = actions.iter().filter(is_remove).collect();
    ordered.extend(actions.iter().filter(|action| !is_remove(action)));
    for action in ordered {
        match action {
            ApplyAction::Write {
                path,
                mode,
                index_mode,
                content,
            } => {
                if !cached {
                    apply_write_worktree_file(
                        &worktree_base,
                        worktree_root,
                        git_dir,
                        format,
                        repo_config,
                        path,
                        content,
                        *mode,
                        umask_complement,
                    )?;
                }
                if index.is_some() {
                    let oid =
                        db.write_object(EncodedObject::new(ObjectType::Blob, content.clone()))?;
                    index_mutations.push(sley_worktree::ApplyIndexMutation::Upsert {
                        path: path.clone(),
                        mode: *index_mode,
                        oid,
                    });
                }
            }
            ApplyAction::Remove { path } => {
                if !cached {
                    merge_remove_worktree_file(&worktree_base, path)?;
                }
                if index.is_some() {
                    index_mutations
                        .push(sley_worktree::ApplyIndexMutation::Remove { path: path.clone() });
                }
            }
            ApplyAction::Gitlink { path, oid } => {
                if !cached {
                    apply_gitlink_worktree_dir(&worktree_base, path)?;
                }
                if index.is_some() {
                    index_mutations.push(sley_worktree::ApplyIndexMutation::Upsert {
                        path: path.clone(),
                        mode: 0o160000,
                        oid: *oid,
                    });
                }
            }
            ApplyAction::GitlinkRemove { path } => {
                if !cached && let Ok(rel) = std::str::from_utf8(path) {
                    // git's `remove_or_warn` rmdir's a gitlink only when empty; a
                    // populated submodule directory is left untouched (ENOTEMPTY
                    // is silent).
                    let _ = fs::remove_dir(worktree_base.join(rel));
                }
                if index.is_some() {
                    index_mutations
                        .push(sley_worktree::ApplyIndexMutation::Remove { path: path.clone() });
                }
            }
        }
        index_paths.push(PathBuf::from(
            std::str::from_utf8(action.path())
                .map_err(|err| GitError::InvalidPath(err.to_string()))?,
        ));
    }
    if let Some(mut index) = index {
        sley_worktree::apply_index_mutations(
            &mut index,
            &index_mutations,
            sley_worktree::ApplyIndexOptions::default(),
        )?;
        fs::write(
            sley_worktree::repository_index_path(git_dir),
            index.write(format)?,
        )?;
        // git's `apply --index`/`--3way` runs `refresh_index` before writing, so
        // an entry it staged (or one left unchanged since the worktree was reset)
        // carries the on-disk stat, not a zeroed/stale one. Without this a
        // freshly-staged path's cached stat is zeroed and `git diff-files` reports
        // a phantom modification (`ie_match_stat` compares size+mtime, not
        // content). Refresh whenever the worktree was materialized (anything but
        // `--cached`, which is index-only and must NOT stat against the worktree).
        // Covers `--3way` with a gitlink patch, which reaches this direct-apply
        // block via `touch_index` without setting `update_index`.
        if !cached {
            sley_worktree::refresh_index_paths(
                worktree_root,
                git_dir,
                format,
                &[],
                /* quiet */ true,
                /* ignore_missing */ true,
                /* really_refresh */ false,
            )?;
        }
    } else if intent_to_add && !index_paths.is_empty() {
        add_intent_to_add(
            &worktree_root,
            &worktree_root,
            &git_dir,
            format,
            &index_paths,
        )?;
    }
    // `--reject`: write each `<file>.rej` (git opens it `O_CREAT|O_EXCL`, unlinking
    // a stale one first), then exit 1 because the patch did not fully apply.
    for (target, bytes) in &reject_writes {
        let rel =
            std::str::from_utf8(target).map_err(|err| GitError::InvalidPath(err.to_string()))?;
        let mut rej_path = worktree_base.join(rel).into_os_string();
        rej_path.push(".rej");
        let rej_path = PathBuf::from(rej_path);
        let _ = fs::remove_file(&rej_path);
        fs::write(&rej_path, bytes)?;
    }
    if had_reject {
        return Err(GitError::Exit(1));
    }
    Ok(())
}

/// git's `write_out_one_reject` chatter: the "Applying patch <name> with N
/// reject(s)..." line plus a per-hunk "applied cleanly" / "Rejected hunk #N."
/// note, all on stderr (printed at the default verbosity).
fn apply_say_reject(patch: &sley_diff_merge::FilePatch, rejected: &[usize], hunk_count: usize) {
    let name = match (patch.old_path.as_deref(), patch.new_path.as_deref()) {
        (Some(old), Some(new)) if old != new => format!(
            "{} => {}",
            status_quote_path(old, false),
            status_quote_path(new, false)
        ),
        _ => status_quote_path(
            patch
                .new_path
                .as_deref()
                .or(patch.old_path.as_deref())
                .unwrap_or(b""),
            false,
        ),
    };
    let n = rejected.len();
    eprintln!(
        "Applying patch {name} with {n} reject{}...",
        if n == 1 { "" } else { "s" }
    );
    let rejected_set: std::collections::HashSet<usize> = rejected.iter().copied().collect();
    for index in 0..hunk_count {
        if rejected_set.contains(&index) {
            eprintln!("Rejected hunk #{}.", index + 1);
        } else {
            eprintln!("Hunk #{} applied cleanly.", index + 1);
        }
    }
}

/// Parse a `-p<n>` value like git's `strtol_i`: a base-10 integer with no
/// trailing junk and not negative. On failure, emit git's message and exit 128.
fn parse_apply_p_value(arg: &str) -> Result<usize> {
    match arg.parse::<i64>() {
        Ok(n) if n >= 0 => Ok(n as usize),
        _ => {
            eprintln!("fatal: option -p expects a non-negative integer, got '{arg}'");
            Err(GitError::Exit(128))
        }
    }
}

/// Normalize a `--directory=<dir>` value like git's `strbuf_normalize_path`
/// (collapsing `.`/`//`/`..`, rejecting upward escapes) and append a trailing
/// slash. On failure, emit git's message and exit 129 (usage).
fn normalize_apply_directory(arg: &str) -> Result<Vec<u8>> {
    match normalize_directory_path(arg.as_bytes()) {
        Some(mut root) => {
            if !root.is_empty() && root.last() != Some(&b'/') {
                root.push(b'/');
            }
            Ok(root)
        }
        None => {
            eprintln!("error: unable to normalize directory: '{arg}'");
            Err(GitError::Exit(129))
        }
    }
}

fn normalize_directory_path(arg: &[u8]) -> Option<Vec<u8>> {
    let absolute = arg.first() == Some(&b'/');
    let mut comps: Vec<&[u8]> = Vec::new();
    for comp in arg.split(|&b| b == b'/') {
        if comp.is_empty() || comp == b"." {
            continue;
        }
        if comp == b".." {
            if comps.pop().is_none() && !absolute {
                return None;
            }
            continue;
        }
        comps.push(comp);
    }
    let mut out = Vec::new();
    if absolute {
        out.push(b'/');
    }
    for (i, c) in comps.iter().enumerate() {
        if i > 0 {
            out.push(b'/');
        }
        out.extend_from_slice(c);
    }
    Some(out)
}

/// Refuse paths that escape the working tree, mirroring git's
/// `check_unsafe_path` (`verify_path`) and `path_is_beyond_symlink`. Runs before
/// any write so the apply stays atomic.
fn check_apply_path_safety(
    worktree_root: &Path,
    patches: &[sley_diff_merge::FilePatch],
    unsafe_paths: bool,
    index: Option<&Index>,
) -> Result<()> {
    // verify_path: a `..` or absolute component is never written, unless the
    // user opted into `--unsafe-paths` for a pure worktree apply.
    if !unsafe_paths {
        for patch in patches {
            let old = if patch.is_delete || (!patch.is_new && !patch.is_copy) {
                patch.old_path.as_deref()
            } else {
                None
            };
            let new = if patch.is_delete {
                None
            } else {
                patch.new_path.as_deref()
            };
            for name in [old, new].into_iter().flatten() {
                if !apply_path_is_valid(name) {
                    eprintln!("error: invalid path '{}'", String::from_utf8_lossy(name));
                    return Err(GitError::Exit(1));
                }
            }
        }
    }

    // path_is_beyond_symlink: a symlink created (or already present) must not be
    // an ancestor directory of any other affected file (e.g. `tmp -> ..` then
    // `tmp/foo`).
    let created_symlinks: Vec<&[u8]> = patches
        .iter()
        .filter(|p| !p.is_delete && p.new_mode == Some(0o120000))
        .filter_map(|p| p.new_path.as_deref())
        .collect();
    // A symlink removed by the patch is no longer an obstacle: git applies the
    // patches in order, so a `symlink → directory` typechange (delete the
    // symlink, create files beneath the new directory) is allowed.
    let deleted_symlinks: Vec<&[u8]> = patches
        .iter()
        .filter(|p| p.is_delete)
        .filter_map(|p| p.old_path.as_deref())
        .filter(|path| worktree_component_is_symlink(worktree_root, path))
        .collect();
    for patch in patches {
        if patch.is_delete {
            continue;
        }
        let Some(name) = patch.new_path.as_deref().or(patch.old_path.as_deref()) else {
            continue;
        };
        for (i, &b) in name.iter().enumerate() {
            if b != b'/' {
                continue;
            }
            let ancestor = &name[..i];
            if deleted_symlinks.iter().any(|s| *s == ancestor) {
                continue;
            }
            if created_symlinks.iter().any(|s| *s == ancestor)
                || worktree_component_is_symlink(worktree_root, ancestor)
                || index_component_is_symlink(index, ancestor)
            {
                eprintln!(
                    "error: affected file '{}' is beyond a symbolic link",
                    String::from_utf8_lossy(name)
                );
                return Err(GitError::Exit(1));
            }
        }
    }
    Ok(())
}

/// git's `verify_path` essentials: reject empty, absolute, or `..`-containing
/// paths.
fn apply_path_is_valid(name: &[u8]) -> bool {
    if name.is_empty() || name[0] == b'/' {
        return false;
    }
    name.split(|&b| b == b'/').all(|comp| comp != b"..")
}

fn worktree_component_is_symlink(worktree_root: &Path, component: &[u8]) -> bool {
    let Ok(rel) = std::str::from_utf8(component) else {
        return false;
    };
    if rel.is_empty() {
        return false;
    }
    std::fs::symlink_metadata(worktree_root.join(rel))
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

fn index_component_is_symlink(index: Option<&Index>, component: &[u8]) -> bool {
    index.is_some_and(|index| {
        index.entries.iter().any(|entry| {
            entry.path.as_bytes() == component
                && (entry.flags >> 12) & 0x3 == 0
                && entry.mode == 0o120000
        })
    })
}

fn write_apply_fake_ancestor_index(
    git_dir: &Path,
    format: ObjectFormat,
    patches: &[sley_diff_merge::FilePatch],
    inputs: &[(String, Vec<u8>)],
    path: &str,
) -> Result<()> {
    let index_records = apply_patch_index_records(inputs);
    let mut entries = Vec::new();
    for (patch, index_record) in patches.iter().zip(index_records) {
        if patch.is_new {
            continue;
        }
        let Some(path) = patch.old_path.as_ref().or(patch.new_path.as_ref()) else {
            continue;
        };
        let oid = sley_rev::resolve_short_object_id(
            git_dir,
            format,
            &index_record.old_oid,
            sley_rev::ObjectDisambiguation::Blob,
        )?
        .into_result(&index_record.old_oid)?;
        let mode = index_record
            .mode
            .or(patch.old_mode)
            .or(patch.new_mode)
            .unwrap_or(0o100644);
        let flags = (path.len().min(0x0fff)) as u16;
        entries.push(IndexEntry {
            ctime_seconds: 0,
            ctime_nanoseconds: 0,
            mtime_seconds: 0,
            mtime_nanoseconds: 0,
            dev: 0,
            ino: 0,
            mode,
            uid: 0,
            gid: 0,
            size: 0,
            oid,
            flags,
            flags_extended: 0,
            path: BString::from(path.clone()),
        });
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    let index = Index {
        version: 2,
        entries,
        extensions: Vec::new(),
        checksum: None,
    };
    fs::write(path, index.write(format)?)?;
    Ok(())
}

struct ApplyPatchIndexRecord {
    old_oid: String,
    mode: Option<u32>,
}

fn apply_patch_index_records(inputs: &[(String, Vec<u8>)]) -> Vec<ApplyPatchIndexRecord> {
    let mut records = Vec::new();
    for (_, input) in inputs {
        for line in input.split(|byte| *byte == b'\n') {
            let Some(rest) = line.strip_prefix(b"index ") else {
                continue;
            };
            let Some((old, after_old)) = split_once_bytes(rest, b"..") else {
                continue;
            };
            let old_oid = String::from_utf8_lossy(old).into_owned();
            let mode = after_old
                .split(|byte| *byte == b' ')
                .nth(1)
                .and_then(parse_apply_octal);
            records.push(ApplyPatchIndexRecord { old_oid, mode });
        }
    }
    records
}

fn parse_apply_octal(bytes: &[u8]) -> Option<u32> {
    let text = std::str::from_utf8(bytes).ok()?;
    u32::from_str_radix(text, 8).ok()
}

fn split_once_bytes<'a>(bytes: &'a [u8], needle: &[u8]) -> Option<(&'a [u8], &'a [u8])> {
    let pos = bytes
        .windows(needle.len())
        .position(|window| window == needle)?;
    Some((&bytes[..pos], &bytes[pos + needle.len()..]))
}

fn read_apply_inputs(files: &[String]) -> Result<Vec<(String, Vec<u8>)>> {
    if files.is_empty() {
        let mut input = Vec::new();
        io::stdin().read_to_end(&mut input)?;
        return Ok(vec![("<stdin>".to_string(), input)]);
    }
    files
        .iter()
        .map(|file| Ok((file.clone(), fs::read(file)?)))
        .collect()
}

fn write_apply_read_only_output(
    patches: &[sley_diff_merge::FilePatch],
    stat: bool,
    numstat: bool,
    summary: bool,
) -> Result<()> {
    let mut entries = Vec::with_capacity(patches.len());
    let mut stats = Vec::with_capacity(patches.len());
    for patch in patches {
        entries.push(apply_patch_name_status_entry(patch));
        stats.push(apply_patch_line_stats(patch));
    }
    let stat_entries = entries
        .iter()
        .zip(stats)
        .map(|(entry, stats)| DiffStatEntryData { entry, stats })
        .collect::<Vec<_>>();

    let mut stdout = io::stdout();
    if numstat {
        for data in &stat_entries {
            write_diff_numstat_materialized_entry(&mut stdout, data.entry, data.stats, false)?;
        }
    }
    if stat {
        write_apply_stat(&mut stdout, &stat_entries)?;
    }
    if summary {
        for patch in patches {
            write_apply_summary_entry(&mut stdout, patch)?;
        }
    }
    Ok(())
}

fn write_apply_stat(stdout: &mut dyn Write, entries: &[DiffStatEntryData<'_>]) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let mut name_width = 0usize;
    let mut max_change = 0usize;
    for data in entries {
        name_width = name_width.max(status_quote_path(&data.entry.path, false).chars().count());
        if let DiffLineStats::Text { inserted, deleted } = data.stats {
            max_change = max_change.max(inserted + deleted);
        }
    }
    let number_width = 4usize.max(diff_stat_decimal_width(max_change) as usize);
    let graph_width = 80usize
        .saturating_sub(name_width + number_width + 6)
        .min(max_change)
        .max(1);
    for data in entries {
        let name = status_quote_path(&data.entry.path, false);
        let padding = name_width.saturating_sub(name.chars().count());
        match data.stats {
            DiffLineStats::Binary { .. } => {
                writeln!(stdout, " {name}{:padding$} |  Bin", "")?;
            }
            DiffLineStats::Text { inserted, deleted } => {
                let total = inserted + deleted;
                write!(stdout, " {name}{:padding$} | {total:>number_width$} ", "")?;
                let scaled_total = apply_stat_scale(total, graph_width, max_change);
                if scaled_total > 0 {
                    if inserted == 0 {
                        write!(stdout, "{}", "-".repeat(scaled_total))?;
                    } else if deleted == 0 {
                        write!(stdout, "{}", "+".repeat(scaled_total))?;
                    } else {
                        let add =
                            apply_stat_scale(inserted, graph_width, max_change).min(scaled_total);
                        let del = scaled_total.saturating_sub(add);
                        write!(stdout, "{}{}", "+".repeat(add), "-".repeat(del))?;
                    }
                }
                writeln!(stdout)?;
            }
        }
    }
    let (inserted, deleted) = diff_stat_totals(entries);
    write_diff_stat_summary_line(stdout, entries.len(), inserted, deleted)
}

fn apply_stat_scale(value: usize, width: usize, max_change: usize) -> usize {
    if value == 0 || max_change == 0 {
        return 0;
    }
    (value * width + max_change / 2) / max_change
}

fn apply_patch_name_status_entry(
    patch: &sley_diff_merge::FilePatch,
) -> sley_diff_merge::NameStatusEntry {
    let path = patch
        .new_path
        .as_ref()
        .or(patch.old_path.as_ref())
        .cloned()
        .unwrap_or_default();
    let status = if patch.is_new {
        sley_diff_merge::NameStatus::Added
    } else if patch.is_delete {
        sley_diff_merge::NameStatus::Deleted
    } else {
        sley_diff_merge::NameStatus::Modified
    };
    sley_diff_merge::NameStatusEntry {
        status,
        path: BString::from(path),
        old_path: None,
        old_mode: apply_patch_old_mode(patch),
        new_mode: apply_patch_new_mode(patch),
        old_oid: None,
        new_oid: None,
    }
}

fn apply_patch_line_stats(patch: &sley_diff_merge::FilePatch) -> DiffLineStats {
    if patch.is_binary {
        return DiffLineStats::Binary {
            old_size: 0,
            new_size: 0,
            unchanged: true,
        };
    }
    let mut inserted = 0usize;
    let mut deleted = 0usize;
    for hunk in &patch.hunks {
        for line in &hunk.lines {
            match line {
                sley_diff_merge::HunkLine::Insert(_) => inserted += 1,
                sley_diff_merge::HunkLine::Delete(_) => deleted += 1,
                sley_diff_merge::HunkLine::Context(_) => {}
            }
        }
    }
    DiffLineStats::Text { inserted, deleted }
}

fn write_apply_summary_entry(
    stdout: &mut dyn Write,
    patch: &sley_diff_merge::FilePatch,
) -> Result<()> {
    if patch.is_rename {
        if let (Some(old_path), Some(new_path)) = (&patch.old_path, &patch.new_path) {
            let path = diff_stat_pprint_rename(old_path, new_path, true);
            let score = patch.similarity.unwrap_or(100);
            writeln!(stdout, " rename {path} ({score}%)")?;
        }
    } else if patch.is_copy {
        if let (Some(old_path), Some(new_path)) = (&patch.old_path, &patch.new_path) {
            let path = diff_stat_pprint_rename(old_path, new_path, true);
            let score = patch.similarity.unwrap_or(100);
            writeln!(stdout, " copy {path} ({score}%)")?;
        }
    } else if patch.is_new {
        if let Some(path) = &patch.new_path {
            let path = status_quote_path(path, false);
            if let Some(mode) = patch.new_mode {
                writeln!(stdout, " create mode {mode:06o} {path}")?;
            } else {
                writeln!(stdout, " create {path}")?;
            }
        }
    } else if patch.is_delete {
        if let Some(path) = &patch.old_path {
            let path = status_quote_path(path, false);
            if let Some(mode) = patch.old_mode {
                writeln!(stdout, " delete mode {mode:06o} {path}")?;
            } else {
                writeln!(stdout, " delete {path}")?;
            }
        }
    } else if let Some(score) = patch.dissimilarity {
        if let Some(path) = patch.new_path.as_ref().or(patch.old_path.as_ref()) {
            let path = status_quote_path(path, false);
            writeln!(stdout, " rewrite {path} ({score}%)")?;
        }
    }
    if let (Some(old_mode), Some(new_mode)) = (patch.old_mode, patch.new_mode)
        && old_mode != new_mode
        && !patch.is_new
        && !patch.is_delete
    {
        if let Some(path) = patch.new_path.as_ref().or(patch.old_path.as_ref()) {
            let path = status_quote_path(path, false);
            writeln!(
                stdout,
                " mode change {old_mode:06o} => {new_mode:06o} {path}"
            )?;
        }
    }
    Ok(())
}

fn apply_patch_old_mode(patch: &sley_diff_merge::FilePatch) -> Option<u32> {
    if patch.is_new { None } else { patch.old_mode }
}

fn apply_patch_new_mode(patch: &sley_diff_merge::FilePatch) -> Option<u32> {
    if patch.is_delete {
        None
    } else {
        patch.new_mode
    }
}

fn validate_apply_input(input: &[u8], name: &str) -> Result<()> {
    let lines = apply_split_patch_lines(input);
    let mut saw_header = false;
    let mut expect_new_header = false;
    let mut after_file_header = false;
    let mut saw_hunk = false;
    // git's `metadata_changes`: a hunk-less patch is only "garbage" when it also
    // carries no metadata change (new/delete/mode/rename/copy). A `new file mode`
    // patch with a bogus trailing line still creates an (empty) file.
    let mut saw_metadata = false;
    for (idx, line) in lines.iter().enumerate() {
        let line_nr = idx + 1;
        if line.starts_with(b"diff --git ") || line.starts_with(b"diff ") {
            saw_header = true;
            expect_new_header = false;
            after_file_header = false;
            saw_hunk = false;
            saw_metadata = false;
            continue;
        }
        if line.starts_with(b"rename from ")
            || line.starts_with(b"rename to ")
            || line.starts_with(b"copy from ")
            || line.starts_with(b"copy to ")
        {
            saw_header = true;
            saw_metadata = true;
            continue;
        }
        if let Some(rest) = apply_strip_prefix(line, b"old mode ")
            .or_else(|| apply_strip_prefix(line, b"new mode "))
            .or_else(|| apply_strip_prefix(line, b"new file mode "))
            .or_else(|| apply_strip_prefix(line, b"deleted file mode "))
        {
            if apply_parse_octal(rest).is_none() {
                eprintln!(
                    "error: invalid mode at {name}:{line_nr}: {}",
                    String::from_utf8_lossy(apply_trim_ascii_end(rest))
                );
                eprintln!();
                return Err(GitError::Exit(1));
            }
            saw_header = true;
            saw_metadata = true;
            continue;
        }
        if line.starts_with(b"--- ") {
            saw_header = true;
            expect_new_header = true;
            after_file_header = false;
            continue;
        }
        if line.starts_with(b"+++ ") {
            expect_new_header = false;
            after_file_header = true;
            continue;
        }
        // Only `@@ -…` is a fragment header (git's `parse_single_patch`). A
        // `@@ +…` line (e.g. a Subversion-generated diff) is not a hunk; it falls
        // through to the garbage/commentary handling below.
        if line.starts_with(b"@@ -") {
            if !saw_header {
                eprintln!(
                    "error: patch fragment without header at {name}:{line_nr}: {}",
                    String::from_utf8_lossy(line)
                );
                return Err(GitError::Exit(1));
            }
            if expect_new_header {
                eprintln!("error: git diff header lacks filename information at {name}:{line_nr}");
                return Err(GitError::Exit(1));
            }
            if !apply_hunk_header_well_formed(line) {
                eprintln!("error: corrupt patch at {name}:{line_nr}");
                return Err(GitError::Exit(1));
            }
            after_file_header = false;
            saw_hunk = true;
            continue;
        }
        if after_file_header && !saw_hunk && !saw_metadata && !line.is_empty() {
            eprintln!("error: patch with only garbage at {name}:{line_nr}");
            return Err(GitError::Exit(1));
        }
    }
    Ok(())
}

fn apply_corrupt_patch_error(input: &[u8], name: &str) -> GitError {
    let line_nr = apply_split_patch_lines(input)
        .iter()
        .position(|line| line.starts_with(b"@@ ") && !apply_hunk_header_well_formed(line))
        .map(|idx| idx + 1)
        .unwrap_or(1);
    eprintln!("error: corrupt patch at {name}:{line_nr}");
    GitError::Exit(1)
}

fn apply_hunk_header_well_formed(line: &[u8]) -> bool {
    let Some(rest) = apply_strip_prefix(line, b"@@ ") else {
        return false;
    };
    let Some(close) = apply_find_subslice(rest, b" @@") else {
        return false;
    };
    let ranges = &rest[..close];
    let mut parts = ranges.split(|&b| b == b' ').filter(|part| !part.is_empty());
    let Some(old) = parts.next().and_then(|part| apply_strip_prefix(part, b"-")) else {
        return false;
    };
    let Some(new) = parts.next().and_then(|part| apply_strip_prefix(part, b"+")) else {
        return false;
    };
    apply_parse_range(old).is_some() && apply_parse_range(new).is_some()
}

fn apply_parse_range(range: &[u8]) -> Option<(usize, usize)> {
    match range.iter().position(|&b| b == b',') {
        Some(comma) => {
            let start = apply_parse_usize(&range[..comma])?;
            let len = apply_parse_usize(&range[comma + 1..])?;
            Some((start, len))
        }
        None => Some((apply_parse_usize(range)?, 1)),
    }
}

fn apply_parse_usize(bytes: &[u8]) -> Option<usize> {
    if bytes.is_empty() {
        return None;
    }
    let mut value = 0usize;
    for &byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add((byte - b'0') as usize)?;
    }
    Some(value)
}

fn apply_parse_octal(bytes: &[u8]) -> Option<u32> {
    let trimmed = apply_trim_ascii_end(bytes);
    if trimmed.is_empty() {
        return None;
    }
    let mut value = 0u32;
    for &byte in trimmed {
        if !(b'0'..=b'7').contains(&byte) {
            return None;
        }
        value = value.checked_mul(8)?.checked_add((byte - b'0') as u32)?;
    }
    Some(value)
}

fn apply_split_patch_lines(input: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    while start < input.len() {
        match input[start..].iter().position(|&b| b == b'\n') {
            Some(rel) => {
                let end = start + rel;
                lines.push(&input[start..end]);
                start = end + 1;
            }
            None => {
                lines.push(&input[start..]);
                start = input.len();
            }
        }
    }
    lines
}

fn apply_strip_prefix<'a>(line: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    line.starts_with(prefix).then(|| &line[prefix.len()..])
}

fn apply_find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn apply_trim_ascii_end(bytes: &[u8]) -> &[u8] {
    let mut end = bytes.len();
    while end > 0 && (bytes[end - 1] == b' ' || bytes[end - 1] == b'\r') {
        end -= 1;
    }
    &bytes[..end]
}

fn apply_write_mode(
    worktree_root: &Path,
    patch: &sley_diff_merge::FilePatch,
    target: &[u8],
    trust_filemode: bool,
) -> Result<u32> {
    if let Some(mode) = patch.new_mode {
        return Ok(mode);
    }
    if patch.is_new {
        return Ok(0o100644);
    }
    if !trust_filemode && let Some(mode) = patch.old_mode {
        return Ok(mode);
    }
    let path = std::str::from_utf8(target)
        .map_err(|err| GitError::InvalidPath(err.to_string()))
        .map(|relative| worktree_root.join(relative))?;
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata_to_git_mode(&metadata)),
        Err(_) => Ok(patch.old_mode.unwrap_or(0o100644)),
    }
}

/// Compute the canonical index-entry mode for a `--index`/`--cached` apply and
/// emit git's "has type … expected …" warning when the current index entry's
/// mode disagrees with the patch's expected old mode.
fn apply_index_mode_and_warn(
    patch: &sley_diff_merge::FilePatch,
    target: &[u8],
    worktree_mode: u32,
    index_modes: &HashMap<Vec<u8>, u32>,
) -> u32 {
    let existing = index_modes.get(target).copied();
    if !patch.is_new
        && let Some(old_mode) = patch.old_mode
        && let Some(actual) = existing
        && canon_mode(actual) != canon_mode(old_mode)
    {
        eprintln!(
            "warning: {} has type {:o}, expected {:o}",
            String::from_utf8_lossy(target),
            canon_mode(actual),
            canon_mode(old_mode)
        );
    }
    // An explicit new mode (new file / mode change) wins; otherwise preserve the
    // existing index entry's mode, falling back to the materialised worktree mode.
    if let Some(new_mode) = patch.new_mode {
        canon_mode(new_mode)
    } else if let Some(actual) = existing {
        actual
    } else {
        canon_mode(worktree_mode)
    }
}

fn apply_worktree_mode_for_index_apply(
    worktree_mode: u32,
    index_mode: u32,
    patch: &sley_diff_merge::FilePatch,
    update_index: bool,
    cached: bool,
) -> u32 {
    if update_index && !cached && patch.new_mode.is_none() {
        index_mode
    } else {
        worktree_mode
    }
}

/// Read the repository index for an `--index`/`--cached` apply, returning an
/// empty in-memory index when none exists yet.
fn read_apply_index(git_dir: &Path, format: ObjectFormat) -> Result<Index> {
    let path = sley_worktree::repository_index_path(git_dir);
    match fs::read(&path) {
        Ok(bytes) => Index::parse(&bytes, format),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        }),
        Err(err) => Err(err.into()),
    }
}

/// Upsert a stage-0 index entry (replacing any existing entries for the path).
/// git's `check_to_create`: every newly created path (and rename/copy target)
/// must not already exist in the index or working tree, unless another patch in
/// the same batch removes it first. A working-tree *directory* at the target is
/// fine (a submodule directory, or a directory git will populate). Mirrors the
/// `EXISTS_IN_INDEX` / `EXISTS_IN_WORKTREE` errors that make a submodule diff
/// abort atomically when its destination is occupied.
fn apply_check_to_create(
    worktree_base: &Path,
    patches: &[sley_diff_merge::FilePatch],
    index: Option<&Index>,
    touch_index: bool,
    cached: bool,
) -> Result<()> {
    // Paths some patch deletes (or renames away from): a create at such a path is
    // permitted (git's `ok_if_exists`).
    let mut deleted_paths: std::collections::HashSet<&[u8]> = std::collections::HashSet::new();
    for patch in patches {
        if (patch.is_delete || patch.is_rename)
            && let Some(old) = patch.old_path.as_deref()
        {
            deleted_paths.insert(old);
        }
    }
    for patch in patches {
        if !(patch.is_new || patch.is_rename || patch.is_copy) {
            continue;
        }
        let Some(new_name) = patch.new_path.as_deref() else {
            continue;
        };
        if deleted_paths.contains(new_name) {
            continue;
        }
        if touch_index
            && let Some(index) = index
            && index
                .entries
                .iter()
                .any(|entry| entry.path.as_bytes() == new_name && (entry.flags >> 12) & 0x3 == 0)
        {
            eprintln!(
                "error: {}: already exists in index",
                String::from_utf8_lossy(new_name)
            );
            return Err(GitError::Exit(1));
        }
        if !cached
            && let Ok(rel) = std::str::from_utf8(new_name)
            && let Ok(meta) = fs::symlink_metadata(worktree_base.join(rel))
            && !meta.is_dir()
        {
            eprintln!(
                "error: {}: already exists in working directory",
                String::from_utf8_lossy(new_name)
            );
            return Err(GitError::Exit(1));
        }
    }
    Ok(())
}

/// A gitlink (submodule) patch carries mode 160000 on either side and a
/// `Subproject commit <sha>` body. git's apply handles these specially: the
/// index entry is set from the recorded commit oid without writing a blob, and
/// the working tree gains only an (empty) directory.
fn apply_patch_is_gitlink(patch: &sley_diff_merge::FilePatch) -> bool {
    patch.old_mode == Some(0o160000) || patch.new_mode == Some(0o160000)
}

fn apply_patch_is_noop(patch: &sley_diff_merge::FilePatch) -> bool {
    if patch.is_new
        || patch.is_delete
        || patch.is_rename
        || patch.is_copy
        || patch.is_binary
        || patch.old_mode != patch.new_mode
    {
        return false;
    }
    !patch.hunks.iter().any(|hunk| {
        hunk.lines.iter().any(|line| {
            matches!(
                line,
                sley_diff_merge::HunkLine::Insert(_) | sley_diff_merge::HunkLine::Delete(_)
            )
        })
    })
}

/// Reconstruct a gitlink patch's preimage (`Subproject commit <old>\n`) from its
/// first hunk's old-side lines — git's `SUBMODULE_PATCH_WITHOUT_INDEX` path, used
/// when the submodule has no index entry to read the recorded commit from.
fn apply_gitlink_preimage_from_patch(patch: &sley_diff_merge::FilePatch) -> Vec<u8> {
    let mut base = Vec::new();
    if let Some(hunk) = patch.hunks.first() {
        for line in &hunk.lines {
            match line {
                sley_diff_merge::HunkLine::Context(bytes)
                | sley_diff_merge::HunkLine::Delete(bytes) => {
                    base.extend_from_slice(bytes);
                    base.push(b'\n');
                }
                sley_diff_merge::HunkLine::Insert(_) => {}
            }
        }
    }
    base
}

/// Parse the commit oid from a gitlink patch's post-image (`Subproject commit
/// <hex>\n`), mirroring git's `add_index_file` gitlink arm.
fn apply_gitlink_oid_from_content(
    content: &[u8],
    format: ObjectFormat,
    path: &[u8],
) -> Result<ObjectId> {
    fn corrupt(path: &[u8]) -> GitError {
        eprintln!(
            "error: corrupt patch for submodule {}",
            String::from_utf8_lossy(path)
        );
        GitError::Exit(1)
    }
    let rest = content
        .strip_prefix(b"Subproject commit ")
        .ok_or_else(|| corrupt(path))?;
    let hex_len = format.hex_len();
    if rest.len() < hex_len {
        return Err(corrupt(path));
    }
    let hex = std::str::from_utf8(&rest[..hex_len]).map_err(|_| corrupt(path))?;
    ObjectId::from_hex(format, hex).map_err(|_| corrupt(path))
}

/// Ensure a gitlink path exists as a directory in the working tree, mirroring
/// git's `try_create_file`: an existing directory is left alone, otherwise the
/// (empty) directory and any missing leading directories are created.
fn apply_gitlink_worktree_dir(worktree_base: &Path, path: &[u8]) -> Result<()> {
    let rel = std::str::from_utf8(path)
        .map_err(|_| GitError::InvalidFormat("non-utf8 worktree path".into()))?;
    let full = worktree_base.join(rel);
    if let Ok(meta) = fs::symlink_metadata(&full)
        && meta.is_dir()
    {
        return Ok(());
    }
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::create_dir(&full) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(err.into()),
    }
}

fn metadata_to_git_mode(metadata: &fs::Metadata) -> u32 {
    if metadata.file_type().is_symlink() {
        return 0o120000;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 != 0 {
            return 0o100755;
        }
    }
    0o100644
}

impl ApplyAction {
    fn path(&self) -> &[u8] {
        match self {
            ApplyAction::Write { path, .. }
            | ApplyAction::Remove { path }
            | ApplyAction::Gitlink { path, .. }
            | ApplyAction::GitlinkRemove { path } => path,
        }
    }
}

/// Read the worktree base content a patch applies against (empty for a new
/// file). Shared by the whitespace pass and the apply pass.
fn read_patch_base(
    worktree_base: &Path,
    filter_worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    patch: &sley_diff_merge::FilePatch,
    index: Option<&Index>,
    db: &FileObjectDatabase,
    verify_worktree_match: bool,
) -> Result<Vec<u8>> {
    if patch.is_new {
        return Ok(Vec::new());
    }
    let Some(old) = patch.old_path.as_deref().or(patch.new_path.as_deref()) else {
        return Ok(Vec::new());
    };
    // Gitlink (submodule) preimage: synthesize `Subproject commit <sha>\n` from
    // the index entry's recorded commit (git's `read_file_or_gitlink`), or, when
    // no index entry exists, from the patch's own `-Subproject commit` line
    // (git's `SUBMODULE_PATCH_WITHOUT_INDEX` / `preimage_oid_in_gitlink_patch`).
    // The submodule's working-tree directory is never read as a blob. Keyed on
    // the OLD-side mode only: a file→gitlink type-change has a regular-file
    // preimage that must be read as a blob, not synthesized.
    if patch.old_mode == Some(0o160000) {
        if let Some(index) = index
            && let Some(entry) = index
                .entries
                .iter()
                .find(|entry| entry.path.as_bytes() == old && (entry.flags >> 12) & 0x3 == 0)
        {
            return Ok(format!("Subproject commit {}\n", entry.oid.to_hex()).into_bytes());
        }
        return Ok(apply_gitlink_preimage_from_patch(patch));
    }
    // `--cached`/`--index`: git's `load_patch_target` reads the preimage from the
    // index blob (`read_file_or_gitlink(ce)`), not the working tree — so a
    // `--cached` apply against an index that differs from the worktree uses the
    // staged content. `--index` (not `--cached`) additionally requires the
    // worktree to match the index (`verify_index_match`), erroring otherwise.
    if let Some(index) = index {
        let entry = index
            .entries
            .iter()
            .find(|entry| entry.path.as_bytes() == old && (entry.flags >> 12) & 0x3 == 0);
        let Some(entry) = entry else {
            eprintln!(
                "error: {}: does not exist in index",
                String::from_utf8_lossy(old)
            );
            return Err(GitError::Exit(1));
        };
        let blob = db.read_object(&entry.oid)?.body.clone();
        if verify_worktree_match
            && let Some(worktree) = read_worktree_patch_blob_bytes(
                worktree_base,
                filter_worktree_root,
                git_dir,
                config,
                old,
            )?
            && worktree != blob
        {
            eprintln!(
                "error: {}: does not match index",
                String::from_utf8_lossy(old)
            );
            return Err(GitError::Exit(1));
        }
        return Ok(blob);
    }
    Ok(
        read_worktree_patch_blob_bytes(worktree_base, filter_worktree_root, git_dir, config, old)?
            .unwrap_or_default(),
    )
}

/// Write a worktree file for `git apply`, mirroring git's `try_create_file`: a
/// regular file is (re)created with mode `(exec ? 0777 : 0666)` masked by the
/// process umask, not forced to the canonical `0644`/`0755`. (Under the usual
/// umask `022` this is identical — `0755`/`0644` — but it respects an unusual
/// umask, e.g. `0077` -> `0700`/`0600`, and never widens via `core.sharedRepository`.)
/// `umask_complement` is `0777 & ~umask`, derived once per invocation.
fn apply_write_worktree_file(
    worktree_base: &Path,
    filter_worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    path: &[u8],
    content: &[u8],
    mode: u32,
    umask_complement: u32,
) -> Result<()> {
    let content = if (mode & 0o170000) == 0o100000 {
        sley_worktree::apply_smudge_filter(
            filter_worktree_root,
            git_dir,
            format,
            config,
            path,
            content,
        )?
    } else {
        content.to_vec()
    };
    merge_write_worktree_file(worktree_base, path, &content, mode)?;
    // Only regular files carry a umask-derived mode; symlinks/gitlinks are left
    // as `merge_write_worktree_file` created them.
    #[cfg(unix)]
    if (mode & 0o170000) == 0o100000 {
        use std::os::unix::fs::PermissionsExt;
        let rel = std::str::from_utf8(path)
            .map_err(|_| GitError::InvalidFormat("non-utf8 worktree path".into()))?;
        let target = if mode & 0o100 != 0 {
            umask_complement
        } else {
            umask_complement & 0o666
        };
        fs::set_permissions(worktree_base.join(rel), fs::Permissions::from_mode(target))?;
    }
    let _ = umask_complement;
    Ok(())
}

/// `0777 & ~umask`, derived (without `unsafe`/libc) by creating a probe file with
/// mode `0777` and reading the OS-applied result. Used to mode worktree files
/// exactly as git's `open(..., (mode & 0100) ? 0777 : 0666)` does.
fn worktree_umask_complement(dir: &Path) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let probe = dir.join(format!(".sley-apply-umask-{}", std::process::id()));
        let _ = fs::remove_file(&probe);
        let mode = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o777)
            .open(&probe)
            .ok()
            .and_then(|_| fs::metadata(&probe).ok())
            .map(|metadata| metadata.permissions().mode() & 0o777)
            .unwrap_or(0o755);
        let _ = fs::remove_file(&probe);
        mode
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        0o755
    }
}

/// Read the blob-form bytes of a worktree path (the symlink target for a
/// symlink, the file bytes otherwise), or `None` when the path does not exist.
fn read_worktree_patch_blob_bytes(
    worktree_base: &Path,
    filter_worktree_root: &Path,
    git_dir: &Path,
    config: &GitConfig,
    path: &[u8],
) -> Result<Option<Vec<u8>>> {
    let rel = std::str::from_utf8(path)
        .map_err(|_| GitError::InvalidFormat("non-utf8 patch path".into()))?;
    let full = worktree_base.join(rel);
    // A symlink's blob content is its target path, not the bytes it points at —
    // read the link rather than following it (symlink↔file/dir typechanges).
    let metadata = match fs::symlink_metadata(&full) {
        Ok(metadata) => metadata,
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(None);
        }
        Err(err) => return Err(err.into()),
    };
    if metadata.file_type().is_symlink() {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            return Ok(fs::read_link(&full)
                .ok()
                .map(|target| target.into_os_string().into_vec()));
        }
    }
    if !metadata.is_file() {
        return Ok(None);
    }
    let body = fs::read(full)?;
    Ok(Some(sley_worktree::apply_clean_filter(
        filter_worktree_root,
        git_dir,
        config,
        path,
        &body,
    )?))
}

/// Outcome of applying a binary file patch.
pub(crate) enum BinaryApply {
    /// The postimage bytes to write.
    Content(Vec<u8>),
    /// The new blob OID is null — the file is removed.
    Deletion,
}

/// Apply a `GIT binary patch` (or a metadata-only `Binary files … differ`)
/// against `image` (the current preimage bytes). Mirrors apply.c's `apply_binary`:
/// require a full index line, verify the preimage matches `old_oid`, then either
/// read the postimage straight from the object store or reconstruct it from the
/// binary fragment and verify it hashes to `new_oid`.
pub(crate) fn apply_binary_outcome(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    patch: &sley_diff_merge::FilePatch,
    image: &[u8],
) -> Result<BinaryApply> {
    let name = String::from_utf8_lossy(
        patch
            .old_path
            .as_deref()
            .or(patch.new_path.as_deref())
            .unwrap_or(b""),
    )
    .into_owned();
    let hexsz = format.hex_len();
    let is_full = |hex: Option<&Vec<u8>>| {
        hex.is_some_and(|hex| hex.len() == hexsz && hex.iter().all(u8::is_ascii_hexdigit))
    };
    // For safety, git requires full hex object IDs for old and new.
    if !is_full(patch.old_oid_hex.as_ref()) || !is_full(patch.new_oid_hex.as_ref()) {
        eprintln!("error: cannot apply binary patch to '{name}' without full index line");
        return Err(GitError::Exit(1));
    }
    let old_hex = String::from_utf8_lossy(patch.old_oid_hex.as_ref().unwrap()).into_owned();
    let new_hex = String::from_utf8_lossy(patch.new_oid_hex.as_ref().unwrap()).into_owned();

    // The preimage must match what the patch was prepared against.
    if !patch.is_new && patch.old_path.is_some() {
        let got = sley_core::object_id_for_bytes(format, "blob", image)?.to_hex();
        if got != old_hex {
            eprintln!(
                "error: the patch applies to '{name}' ({got}), which does not match the \
                 current contents."
            );
            return Err(GitError::Exit(1));
        }
    } else if !image.is_empty() {
        eprintln!("error: the patch applies to an empty '{name}' but it is not empty");
        return Err(GitError::Exit(1));
    }

    let new_oid = ObjectId::from_hex(format, &new_hex)?;
    if new_oid.is_null() {
        return Ok(BinaryApply::Deletion);
    }

    // If we already have the postimage object, use it directly.
    if db.contains(&new_oid)? {
        let object = db.read_object(&new_oid)?;
        return Ok(BinaryApply::Content(object.body.clone()));
    }

    // Otherwise reconstruct it from the binary fragment and verify the result.
    let Some(binary) = &patch.binary else {
        eprintln!("error: missing binary patch data for '{name}'");
        return Err(GitError::Exit(1));
    };
    let frag = &binary.forward;
    let binary_apply_failed = || {
        eprintln!("error: binary patch does not apply to '{name}'");
        GitError::Exit(1)
    };
    let inflated =
        inflate_zlib_exact(&frag.deflated, frag.origlen).ok_or_else(binary_apply_failed)?;
    let post = match frag.method {
        sley_diff_merge::BinaryMethod::Literal => inflated,
        sley_diff_merge::BinaryMethod::Delta => {
            sley_diff_merge::git_patch_delta(image, &inflated).ok_or_else(binary_apply_failed)?
        }
    };
    let got = sley_core::object_id_for_bytes(format, "blob", &post)?.to_hex();
    if got != new_hex {
        eprintln!(
            "error: binary patch to '{name}' creates incorrect result \
             (expecting {new_hex}, got {got})"
        );
        return Err(GitError::Exit(1));
    }
    Ok(BinaryApply::Content(post))
}

/// `git apply --3way`: reconstruct the recorded pre-image of every patch, apply
/// the patch to it to form "theirs", and 3-way merge against the current index
/// ("ours"). Returns `Ok(true)` when the 3-way path handled the apply, `Ok(false)`
/// when a pre-image blob was unavailable (the caller falls back to direct apply).
#[allow(clippy::too_many_arguments)]
fn apply_three_way_path(
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    config: &GitConfig,
    patches: &[sley_diff_merge::FilePatch],
    cached: bool,
    check: bool,
    favor: sley_diff_merge::MergeFavor,
    union: bool,
    lazy_fetch: bool,
) -> Result<bool> {
    // `--union` keeps both sides of every textual conflict with no markers
    // (git's `merge=union`); it overrides any `--ours`/`--theirs` favouring.
    let favor = if union {
        sley_diff_merge::MergeFavor::Union
    } else {
        favor
    };
    // `merge.conflictStyle` selects the diff3 marker layout.
    let style = match config.get("merge", None, "conflictstyle") {
        Some("diff3") | Some("zdiff3") => sley_diff_merge::ConflictStyle::Diff3,
        _ => sley_diff_merge::ConflictStyle::Merge,
    };

    // git's `try_threeway` refuses gitlink (submodule) patches outright, falling
    // back to the direct apply. The 3-way merge engine here has no gitlink mode,
    // so defer the whole patch set to the direct apply (which has a gitlink arm).
    if patches.iter().any(apply_patch_is_gitlink) {
        return Ok(false);
    }

    // A path touched by more than one patch (e.g. a delete followed by a re-add)
    // needs git's sequential per-patch semantics, not a single batched 3-way
    // merge; fall back to the direct apply, which materialises them in order.
    let mut seen_paths = std::collections::HashSet::new();
    for patch in patches {
        if let Some(path) = patch.new_path.as_ref().or(patch.old_path.as_ref())
            && !seen_paths.insert(path.clone())
        {
            return Ok(false);
        }
    }

    // "ours" = the current index (stage 0 entries).
    let index = read_apply_index(git_dir, format)?;
    let mut ours_map: commands::merge_rebase::MergeTreeMap = std::collections::BTreeMap::new();
    for entry in &index.entries {
        if (entry.flags >> 12) & 0x3 == 0 {
            ours_map.insert(entry.path.to_vec(), (entry.mode, entry.oid));
        }
    }

    // git's `load_preimage` reads the worktree and aborts the whole 3-way when a
    // touched file does not match its index entry (a dirty work tree), leaving
    // everything untouched. Skipped for `--cached`, whose "ours" is the index.
    if !cached {
        for patch in patches {
            let Some(path) = patch.old_path.as_ref().or(patch.new_path.as_ref()) else {
                continue;
            };
            // Covers both a modify (load_preimage) and an add/add (load_current):
            // the worktree file must match its index entry, else abort untouched.
            if let Some((_, index_oid)) = ours_map.get(path)
                && let Ok(rel) = std::str::from_utf8(path)
            {
                let content = fs::read(worktree_root.join(rel)).unwrap_or_default();
                let worktree_oid = sley_core::object_id_for_bytes(format, "blob", &content)?;
                if &worktree_oid != index_oid {
                    eprintln!(
                        "error: {}: does not match index",
                        String::from_utf8_lossy(path)
                    );
                    return Err(GitError::Exit(1));
                }
            }
        }
    }

    let mut base_map = ours_map.clone();
    let mut theirs_map = ours_map.clone();
    for patch in patches {
        let Some(path) = patch.new_path.clone().or_else(|| patch.old_path.clone()) else {
            return Ok(false);
        };
        let old_path = patch.old_path.clone().unwrap_or_else(|| path.clone());
        let inherited = ours_map
            .get(&old_path)
            .or_else(|| ours_map.get(&path))
            .map(|(mode, _)| *mode)
            .unwrap_or(0o100644);

        // Reconstruct the pre-image the patch was prepared against.
        let base_bytes = if patch.is_new {
            Vec::new()
        } else if let Some(bytes) = apply_resolve_preimage_blob(git_dir, format, db, patch)? {
            bytes
        } else {
            // Pre-image blob unavailable — fall back to direct application.
            return Ok(false);
        };

        // Apply the patch to the pre-image to get "theirs".
        let post = if patch.is_binary {
            match apply_binary_outcome(db, format, patch, &base_bytes)? {
                BinaryApply::Content(content) => Some(content),
                BinaryApply::Deletion => None,
            }
        } else if patch.is_delete {
            None
        } else {
            match sley_diff_merge::apply_file_patch(&base_bytes, patch) {
                sley_diff_merge::ApplyOutcome::Applied(content) => Some(content),
                sley_diff_merge::ApplyOutcome::Rejected => return Ok(false),
            }
        };

        let base_mode = canon_mode(patch.old_mode.unwrap_or(inherited));
        let new_mode = canon_mode(patch.new_mode.or(patch.old_mode).unwrap_or(inherited));

        if patch.is_new {
            base_map.remove(&path);
        } else {
            let base_oid = db.write_object(EncodedObject::new(ObjectType::Blob, base_bytes))?;
            base_map.insert(old_path.clone(), (base_mode, base_oid));
        }
        match post {
            None => {
                theirs_map.remove(&path);
            }
            Some(content) => {
                let post_oid = db.write_object(EncodedObject::new(ObjectType::Blob, content))?;
                theirs_map.insert(path.clone(), (new_mode, post_oid));
                if patch.is_rename {
                    theirs_map.remove(&old_path);
                }
            }
        }
    }

    let (results, conflicts, _info) =
        commands::merge_rebase::three_way_merge_trees_inner_with_info(
            db,
            config,
            lazy_fetch,
            format,
            &base_map,
            &ours_map,
            &theirs_map,
            "ours",
            "theirs",
            "merged common ancestors",
            favor,
            style,
        )?;

    if !check {
        apply_write_three_way(
            git_dir,
            worktree_root,
            format,
            db,
            &ours_map,
            &results,
            cached,
            lazy_fetch,
        )?;
    }

    if conflicts.is_empty() {
        Ok(true)
    } else {
        // git's fall_back_threeway runs rerere on the conflicted result: it
        // records the preimage and replays any previously-recorded resolution
        // into the worktree. A no-op unless rerere.enabled.
        if !check && !cached {
            commands::rerere::repo_rerere(git_dir, format, None)?;
        }
        // git exits non-zero, leaving conflict markers + a conflicted index.
        Err(GitError::Exit(1))
    }
}

/// Resolve a patch's pre-image blob via its `index <old>..<new>` OID, returning
/// `None` when the OID cannot be resolved or the object is absent.
fn apply_resolve_preimage_blob(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    patch: &sley_diff_merge::FilePatch,
) -> Result<Option<Vec<u8>>> {
    let Some(hex) = patch.old_oid_hex.as_ref() else {
        return Ok(None);
    };
    let Ok(hex) = std::str::from_utf8(hex) else {
        return Ok(None);
    };
    if hex.bytes().all(|b| b == b'0') {
        return Ok(None);
    }
    let oid = if hex.len() == format.hex_len() {
        ObjectId::from_hex(format, hex)?
    } else {
        match sley_rev::resolve_short_object_id(
            git_dir,
            format,
            hex,
            sley_rev::ObjectDisambiguation::Blob,
        )? {
            sley_rev::ShortObjectIdResolution::Unique(oid) => oid,
            _ => return Ok(None),
        }
    };
    if !db.contains(&oid)? {
        return Ok(None);
    }
    Ok(Some(db.read_object(&oid)?.body.clone()))
}

/// Write the 3-way merge result: a conflicted index (stages 1/2/3) for conflicts,
/// stage-0 entries otherwise, plus the worktree (unless `--cached`).
fn apply_write_three_way(
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    ours_map: &commands::merge_rebase::MergeTreeMap,
    results: &std::collections::BTreeMap<Vec<u8>, commands::merge_rebase::MergePathResult>,
    cached: bool,
    lazy_fetch: bool,
) -> Result<()> {
    use commands::merge_rebase::{MergePathResult, merge_index_entry};
    let mut entries = Vec::new();
    for (path, result) in results {
        match result {
            MergePathResult::Resolved(Some((mode, oid))) => {
                entries.push(merge_index_entry(path, *mode, *oid, 0));
            }
            MergePathResult::Resolved(None) => {}
            MergePathResult::Conflict {
                base, ours, theirs, ..
            } => {
                if let Some((mode, oid)) = base {
                    entries.push(merge_index_entry(path, *mode, *oid, 1));
                }
                if let Some((mode, oid)) = ours {
                    entries.push(merge_index_entry(path, *mode, *oid, 2));
                }
                if let Some((mode, oid)) = theirs {
                    entries.push(merge_index_entry(path, *mode, *oid, 3));
                }
            }
        }
    }
    entries.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| (left.flags >> 12).cmp(&(right.flags >> 12)))
    });
    let new_index = Index {
        version: 2,
        entries,
        extensions: Vec::new(),
        checksum: None,
    };
    fs::write(
        sley_worktree::repository_index_path(git_dir),
        new_index.write(format)?,
    )?;

    if cached {
        return Ok(());
    }
    for (path, result) in results {
        match result {
            MergePathResult::Resolved(Some((mode, oid))) => {
                if ours_map.get(path) != Some(&(*mode, *oid)) {
                    let content = commands::merge_rebase::merge_read_blob(db, oid, lazy_fetch)?;
                    merge_write_worktree_file(worktree_root, path, &content, *mode)?;
                }
            }
            MergePathResult::Resolved(None) => merge_remove_worktree_file(worktree_root, path)?,
            MergePathResult::Conflict { worktree, .. } => match worktree {
                Some((mode, content)) => {
                    merge_write_worktree_file(worktree_root, path, content, *mode)?;
                }
                None => merge_remove_worktree_file(worktree_root, path)?,
            },
        }
    }
    // git's `apply --3way` refreshes the index after materializing the worktree,
    // so a cleanly-resolved stage-0 entry records the on-disk stat rather than the
    // zeroed one written above; otherwise `git diff-files` reports a phantom
    // modification (`ie_match_stat` compares size+mtime, not content). Conflict
    // stages (1/2/3) and gitlinks are left untouched by refresh.
    sley_worktree::refresh_index_paths(
        worktree_root,
        git_dir,
        format,
        &[],
        /* quiet */ true,
        /* ignore_missing */ true,
        /* really_refresh */ false,
    )?;
    Ok(())
}

/// Inflate a single zlib stream, expecting exactly `expected_len` bytes out.
fn inflate_zlib_exact(deflated: &[u8], expected_len: usize) -> Option<Vec<u8>> {
    use flate2::{Decompress, FlushDecompress};
    let mut decoder = Decompress::new(true);
    let mut out = Vec::with_capacity(sley_pack::inflate::bounded_inflate_reserve(
        expected_len,
        deflated.len(),
    ));
    decoder
        .decompress_vec(deflated, &mut out, FlushDecompress::Finish)
        .ok()?;
    if out.len() != expected_len {
        return None;
    }
    Some(out)
}

/// Parse the `--whitespace=<action>` value into a [`WsAction`].
fn parse_ws_action(value: &str) -> Result<WsAction> {
    match value {
        "nowarn" => Ok(WsAction::Nowarn),
        "warn" => Ok(WsAction::Warn),
        "error" => Ok(WsAction::Error),
        "error-all" => Ok(WsAction::ErrorAll),
        "fix" | "strip" => Ok(WsAction::Fix),
        other => Err(GitError::Command(format!(
            "unrecognized whitespace option '{other}'"
        ))),
    }
}

/// Whitespace handling for one file patch: warn/error on, or fix, the
/// introduced (`+`) lines. Port of apply.c's `apply_one_fragment` ws path plus
/// its `check_whitespace`. Mutates the patch's Insert lines in `fix` mode.
#[allow(clippy::too_many_arguments)]
fn apply_patch_whitespace(
    patch: &mut sley_diff_merge::FilePatch,
    base: &[u8],
    rule: sley_diff_merge::ws::WsRule,
    action: WsAction,
    patch_input_file: &str,
    squelch_limit: usize,
    error_count: &mut usize,
    squelched: &mut usize,
) {
    use sley::plumbing::sley_diff_merge::HunkLine;
    use sley::plumbing::sley_diff_merge::ws;

    let fixing = matches!(action, WsAction::Fix);

    // git first scans the whole patch for whitespace errors (`check_whitespace`
    // sets a single `state->whitespace_error` flag). In `fix` mode the actual
    // `ws_fix_copy` is then applied to *every* introduced line, but only when
    // that flag is set — so a clean-on-its-own line (e.g. `8 spaces + tab`,
    // which the indent-with-non-tab check passes) is still re-indented when a
    // sibling line in the same patch is dirty. We mirror that by pre-scanning.
    // The index of the new side's final line (last context/insert) in each hunk:
    // a `+` line is "incomplete" (no trailing newline) only when it is that line
    // and the hunk records the new side as unterminated. Everywhere else a `+`
    // line carries a trailing newline, which `ws_check`/`ws_fix` must see so that
    // `incomplete-line` does not fire on every introduced line.
    let last_new_index = |hunk: &sley_diff_merge::Hunk| -> Option<usize> {
        hunk.lines
            .iter()
            .rposition(|line| matches!(line, HunkLine::Context(_) | HunkLine::Insert(_)))
    };
    let probe_bytes = |bytes: &[u8], incomplete: bool| -> Vec<u8> {
        let mut probe = bytes.to_vec();
        if !incomplete {
            probe.push(b'\n');
        }
        probe
    };

    let patch_has_ws_error = patch.hunks.iter().any(|hunk| {
        let last_new = last_new_index(hunk);
        hunk.lines.iter().enumerate().any(|(index, hl)| match hl {
            HunkLine::Insert(bytes) => {
                let incomplete = hunk.new_no_newline && Some(index) == last_new;
                ws::ws_check(&probe_bytes(bytes, incomplete), rule) != 0
            }
            _ => false,
        })
    });

    for hunk in &mut patch.hunks {
        let last_new = last_new_index(hunk);
        let mut clear_new_no_newline = false;
        for index in 0..hunk.lines.len() {
            if !matches!(hunk.lines[index], HunkLine::Insert(_)) {
                continue;
            }
            let input_line = hunk.line_input_lines.get(index).copied().unwrap_or(0);
            let incomplete = hunk.new_no_newline && Some(index) == last_new;
            let HunkLine::Insert(bytes) = &hunk.lines[index] else {
                unreachable!()
            };
            let probe = probe_bytes(bytes, incomplete);
            if fixing {
                // Re-indent/strip every introduced line once any line in the
                // patch is dirty (git's global-flag semantics).
                if patch_has_ws_error {
                    let fixed = ws::ws_fix_bytes(&probe, rule);
                    // Recover the content: drop the newline we appended, or — for a
                    // genuinely-incomplete line — the newline `ws_fix` added to
                    // complete it (the `incomplete-line` correction).
                    let trailing_nl = fixed.last() == Some(&b'\n');
                    let content = if trailing_nl {
                        fixed[..fixed.len() - 1].to_vec()
                    } else {
                        fixed.clone()
                    };
                    let completed = incomplete && trailing_nl;
                    let HunkLine::Insert(bytes) = &mut hunk.lines[index] else {
                        unreachable!()
                    };
                    if &content != bytes || completed {
                        *bytes = content;
                        *error_count += 1;
                        if completed {
                            clear_new_no_newline = true;
                        }
                    }
                }
            } else {
                let bad = ws::ws_check(&probe, rule);
                if bad != 0 {
                    *error_count += 1;
                    if *error_count <= squelch_limit {
                        let err = ws::whitespace_error_string(bad);
                        eprintln!("{patch_input_file}:{input_line}: {err}.");
                        eprintln!("{}", String::from_utf8_lossy(bytes));
                    } else {
                        *squelched += 1;
                    }
                }
            }
        }
        if clear_new_no_newline {
            hunk.new_no_newline = false;
        }
    }

    // Blank-at-EOF warning for warn/error modes: compare the trailing-blank run
    // of the pre- and post-images. In `fix` mode the whitespace-aware apply
    // (`apply_file_patch_ws`, via `new_blank_lines_at_end`) does the removal, so
    // this pass only reports for the non-fix actions.
    if rule & ws::WS_BLANK_AT_EOF != 0 && !fixing {
        let postimage = match sley_diff_merge::apply_file_patch(base, patch) {
            sley_diff_merge::ApplyOutcome::Applied(content) => content,
            sley_diff_merge::ApplyOutcome::Rejected => return,
        };
        let l1 = ws::count_trailing_blank(base);
        let l2 = ws::count_trailing_blank(&postimage);
        if l2 > l1 {
            let at = ws::count_lines(&postimage);
            let blank_at_eof = at - l2 + 1;
            *error_count += 1;
            if *error_count <= squelch_limit {
                let err = ws::whitespace_error_string(ws::WS_BLANK_AT_EOF);
                eprintln!("{patch_input_file}:{blank_at_eof}: {err}.");
            } else {
                *squelched += 1;
            }
        }
    }
}
