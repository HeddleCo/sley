//! `git sparse-checkout` and its subcommands
//! (init / list / set / add / reapply / disable).
//!
//! This mirrors upstream `git sparse-checkout`: it toggles
//! `core.sparseCheckout` / `core.sparseCheckoutCone` (stored in the per-worktree
//! config, with the `extensions.worktreeConfig` extension enabled in the main
//! config exactly as upstream does), maintains the `$GIT_DIR/info/sparse-checkout`
//! pattern file, and reconciles the index + worktree through the committed sparse
//! engine in [`sley_worktree::apply_sparse_checkout_with_mode`].
//!
//! In *cone* mode (the modern default) the user supplies directory names and the
//! pattern file is generated as the restricted cone grammar Git emits (a `/*`
//! header, `!/*/` recursive guard, parent-directory guards, and recursive
//! directory patterns). In *non-cone* mode the supplied arguments are written to
//! the pattern file verbatim and matched with full `.gitignore` semantics.

// Command modules pull their shared plumbing from the crate root. A glob import
// works because a submodule can access its ancestor module's items (including
// private ones), so every helper, type, and re-export visible at the crate root
// is in scope here without re-listing it.
use crate::*;

use crate::commands::ref_command_stream::unquote_c_style;
use sley::plumbing::sley_worktree::{
    SparseCheckout, SparseCheckoutMode, apply_sparse_checkout_with_mode, path_in_sparse_checkout,
};

/// Interpret raw path bytes as a (relative) [`PathBuf`]. On Unix the bytes are
/// the OS-native path encoding; off Unix they are decoded lossily as UTF-8.
fn bytes_to_os_path(bytes: &[u8]) -> std::path::PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        std::path::PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
    }
    #[cfg(not(unix))]
    {
        std::path::PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

/// Path/`OsStr` → its byte encoding. On Unix this is the native bytes; off Unix
/// it is the lossy UTF-8 form with `\` normalised to `/`.
fn os_str_to_bytes(value: &std::ffi::OsStr) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        value.as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        value.to_string_lossy().replace('\\', "/").into_bytes()
    }
}

const SPARSE_USAGE: &str = "usage: git sparse-checkout (init | list | set | add | reapply | disable | check-rules | clean) [<options>]";

// The exact usage + option-help blocks upstream prints when it rejects an
// option. Keeping them verbatim lets the differential tests compare stderr
// byte-for-byte.
const INIT_HELP: &str = "usage: git sparse-checkout init [--cone] [--[no-]sparse-index]\n\n    --[no-]cone           initialize the sparse-checkout in cone mode\n    --[no-]sparse-index   toggle the use of a sparse index\n";
const SET_HELP: &str = "usage: git sparse-checkout set [--[no-]cone] [--[no-]sparse-index] [--skip-checks] (--stdin | <patterns>)\n\n    --[no-]cone           initialize the sparse-checkout in cone mode\n    --[no-]sparse-index   toggle the use of a sparse index\n    --skip-checks         skip some sanity checks on the given paths that might give false positives\n    --stdin               read patterns from standard in\n";
const ADD_HELP: &str = "usage: git sparse-checkout add [--skip-checks] (--stdin | <patterns>)\n\n    --skip-checks         skip some sanity checks on the given paths that might give false positives\n    --[no-]stdin          read patterns from standard in\n";
const REAPPLY_HELP: &str = "usage: git sparse-checkout reapply [--[no-]cone] [--[no-]sparse-index]\n\n    --[no-]cone           initialize the sparse-checkout in cone mode\n    --[no-]sparse-index   toggle the use of a sparse index\n";
const LIST_HELP: &str = "usage: git sparse-checkout list\n";
const DISABLE_HELP: &str = "usage: git sparse-checkout disable\n";

/// Tri-state for the `--cone` / `--no-cone` family of flags: the user may leave
/// it unspecified (inherit / default), force cone, or force non-cone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConeFlag {
    Unset,
    Cone,
    NoCone,
}

/// Tri-state for `--sparse-index` / `--no-sparse-index`: leave the recorded
/// `index.sparse` setting alone, enable it, or disable it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SparseIndexFlag {
    Unset,
    Enable,
    Disable,
}

/// Where the per-worktree sparse settings (and the pattern file) live for the
/// current repository.
struct SparseContext {
    git_dir: PathBuf,
    worktree_root: PathBuf,
    format: ObjectFormat,
    /// The repo-relative path from the worktree top to the user's cwd, with a
    /// trailing `/` (git's `prefix`), or empty at the top level. Cone-mode
    /// directory arguments are resolved against this.
    prefix: Vec<u8>,
}

pub(crate) fn cmd_sparse_checkout(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let Some(sub) = args.first() else {
        eprintln!("error: need a subcommand");
        eprintln!("{SPARSE_USAGE}");
        eprintln!();
        return Err(GitError::Exit(129));
    };
    match sub.as_str() {
        "init" => cmd_sparse_init(cli_session, &args[1..]),
        "list" => cmd_sparse_list(cli_session, &args[1..]),
        "set" => cmd_sparse_set(cli_session, &args[1..]),
        "add" => cmd_sparse_add(cli_session, &args[1..]),
        "reapply" => cmd_sparse_reapply(cli_session, &args[1..]),
        "disable" => cmd_sparse_disable(cli_session, &args[1..]),
        "check-rules" => cmd_sparse_check_rules(cli_session, &args[1..]),
        "clean" => cmd_sparse_clean(cli_session, &args[1..]),
        other => {
            eprintln!("error: unknown subcommand: `{other}'");
            eprintln!("{SPARSE_USAGE}");
            eprintln!();
            Err(GitError::Exit(129))
        }
    }
}

// --------------------------------------------------------------------------
// Subcommand implementations
// --------------------------------------------------------------------------

fn cmd_sparse_init(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let mut cone = ConeFlag::Unset;
    let mut sparse_index = SparseIndexFlag::Unset;
    for arg in args {
        match arg.as_str() {
            "--cone" => cone = ConeFlag::Cone,
            "--no-cone" => cone = ConeFlag::NoCone,
            "--sparse-index" => sparse_index = SparseIndexFlag::Enable,
            "--no-sparse-index" => sparse_index = SparseIndexFlag::Disable,
            other => return unknown_option(other, INIT_HELP),
        }
    }
    let ctx = sparse_context(cli_session)?;
    // Cone is the default for a fresh init, but a plain `init` over an existing
    // sparse checkout preserves the recorded mode.
    let cone_mode = match cone {
        ConeFlag::Cone => true,
        ConeFlag::NoCone => false,
        ConeFlag::Unset => {
            if sparse_checkout_enabled(&ctx)? {
                sparse_cone_enabled(&ctx)?
            } else {
                true
            }
        }
    };
    enable_sparse_checkout(&ctx, cone_mode)?;
    apply_sparse_index_flag(&ctx, sparse_index)?;
    // Preserve any existing patterns; otherwise seed the cone-style root file so
    // a fresh init leaves only top-level files in the worktree.
    if read_sparse_patterns(&ctx)?.is_none() {
        write_sparse_file(&ctx, b"/*\n!/*/\n")?;
    }
    apply_current_sparse(&ctx)?;
    Ok(())
}

/// Applies the `--[no-]sparse-index` toggle: record `index.sparse` in the
/// worktree config (the persistent decision) so subsequent index writes either
/// collapse out-of-cone directories or stay full. A no-op when the flag was not
/// given.
fn apply_sparse_index_flag(ctx: &SparseContext, flag: SparseIndexFlag) -> Result<()> {
    match flag {
        SparseIndexFlag::Unset => Ok(()),
        SparseIndexFlag::Enable => set_index_sparse(ctx, true),
        SparseIndexFlag::Disable => set_index_sparse(ctx, false),
    }
}

fn cmd_sparse_list(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let ctx = sparse_context(cli_session)?;
    // Upstream verifies the worktree is sparse before it parses options, so an
    // unknown option on a non-sparse worktree still reports "not sparse".
    if !sparse_checkout_enabled(&ctx)? {
        eprintln!("fatal: this worktree is not sparse");
        return Err(GitError::Exit(128));
    }
    // `list` has no options; reject flags but ignore stray positionals (upstream
    // does the same).
    if let Some(arg) = args.iter().find(|arg| arg.starts_with('-')) {
        return unknown_option(arg.as_str(), LIST_HELP);
    }
    // A sparse worktree whose pattern file is missing is not an error: upstream
    // warns and exits 0 (the `res < 0` path of `add_patterns_from_file_to_list`).
    let Some(patterns) = read_sparse_patterns(&ctx)? else {
        eprintln!("warning: this worktree is not sparse (sparse-checkout file may not exist)");
        return Ok(());
    };
    let cone = sparse_cone_enabled(&ctx)?;
    let mut out = io::stdout();
    if cone && cone_patterns_are_valid(&patterns, true) {
        // Cone entries are emitted through git's `quote_c_style`, so a directory
        // with unusual bytes is C-quoted exactly as `ls-files` would render it.
        for dir in cone_list_entries(&patterns) {
            write_status_quoted_path(&mut out, &dir, false)?;
            out.write_all(b"\n")?;
        }
    } else {
        for line in &patterns {
            out.write_all(line)?;
            out.write_all(b"\n")?;
        }
    }
    Ok(())
}

fn cmd_sparse_set(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let parsed = parse_set_like(args, SET_HELP, true)?;
    let ctx = sparse_context(cli_session)?;
    // `set` resolves the cone mode now: an explicit flag wins, otherwise an
    // already-initialized worktree keeps its mode, and a brand new one defaults
    // to cone.
    let cone_mode = match parsed.cone {
        ConeFlag::Cone => true,
        ConeFlag::NoCone => false,
        ConeFlag::Unset => {
            if sparse_checkout_enabled(&ctx)? {
                sparse_cone_enabled(&ctx)?
            } else {
                true
            }
        }
    };
    // Resolve a subdirectory prefix into the directory arguments (cone) or
    // reject a non-cone invocation from a subdir, before any validation.
    let patterns = sanitize_set_paths(
        &ctx,
        &parsed.patterns,
        cone_mode,
        parsed.skip_checks || parsed.from_stdin,
    )?;
    // Validate and serialize the new pattern file before mutating any state, so a
    // rejected pattern (e.g. a leading slash in cone mode) leaves the config and
    // pattern file untouched.
    let content = build_pattern_content(
        cone_mode,
        &patterns,
        parsed.skip_checks || parsed.from_stdin,
    )?;
    enable_sparse_checkout(&ctx, cone_mode)?;
    apply_sparse_index_flag(&ctx, parsed.sparse_index)?;
    write_sparse_file(&ctx, &content)?;
    apply_current_sparse(&ctx)?;
    Ok(())
}

fn cmd_sparse_add(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let ctx = sparse_context(cli_session)?;
    // Upstream checks for an existing sparse-checkout before option parsing.
    if !sparse_checkout_enabled(&ctx)? {
        eprintln!("fatal: no sparse-checkout to add to");
        return Err(GitError::Exit(128));
    }
    // `add` does not accept the cone toggles (they are not in its option set).
    let parsed = parse_set_like(args, ADD_HELP, false)?;
    let cone_mode = sparse_cone_enabled(&ctx)?;
    // Resolve a subdir prefix into the directory arguments (cone) or reject the
    // non-cone-from-subdir case, exactly like `set`.
    let new_patterns = sanitize_set_paths(
        &ctx,
        &parsed.patterns,
        cone_mode,
        parsed.skip_checks || parsed.from_stdin,
    )?;
    let existing = read_sparse_patterns(&ctx)?.unwrap_or_default();
    // Build (and validate) the merged pattern file before writing anything.
    let content = if cone_mode {
        if !cone_patterns_are_valid(&existing, true) {
            eprintln!("fatal: existing sparse-checkout patterns do not use cone mode");
            return Err(GitError::Exit(128));
        }
        // Recover the directory set from the existing cone file and union it with
        // the new directories, then regenerate.
        let mut dirs = cone_list_entries(&existing);
        for pattern in &new_patterns {
            dirs.push(validate_cone_dir(
                pattern,
                parsed.skip_checks || parsed.from_stdin,
            )?);
        }
        build_cone_file(&dirs)
    } else {
        let mut lines = existing;
        for pattern in &new_patterns {
            lines.push(pattern.clone());
        }
        serialize_noncone_lines(&lines)
    };
    write_sparse_file(&ctx, &content)?;
    apply_current_sparse(&ctx)?;
    Ok(())
}

fn cmd_sparse_reapply(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let ctx = sparse_context(cli_session)?;
    // Upstream requires an active sparse-checkout before it parses options.
    if !sparse_checkout_enabled(&ctx)? {
        eprintln!("fatal: must be in a sparse-checkout to reapply sparsity patterns");
        return Err(GitError::Exit(128));
    }
    let mut cone = ConeFlag::Unset;
    let mut sparse_index = SparseIndexFlag::Unset;
    for arg in args {
        match arg.as_str() {
            "--cone" => cone = ConeFlag::Cone,
            "--no-cone" => cone = ConeFlag::NoCone,
            "--sparse-index" => sparse_index = SparseIndexFlag::Enable,
            "--no-sparse-index" => sparse_index = SparseIndexFlag::Disable,
            other => return unknown_option(other, REAPPLY_HELP),
        }
    }
    // A `--cone`/`--no-cone` flag may flip the mode of an already-initialized
    // worktree; otherwise keep what is on disk.
    let cone_mode = match cone {
        ConeFlag::Cone => true,
        ConeFlag::NoCone => false,
        ConeFlag::Unset => sparse_cone_enabled(&ctx)?,
    };
    enable_sparse_checkout(&ctx, cone_mode)?;
    apply_sparse_index_flag(&ctx, sparse_index)?;
    apply_current_sparse(&ctx)?;
    Ok(())
}

fn cmd_sparse_disable(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    // `disable` has no options; reject flags but ignore stray positionals.
    if let Some(arg) = args.iter().find(|arg| arg.starts_with('-')) {
        return unknown_option(arg.as_str(), DISABLE_HELP);
    }
    let ctx = sparse_context(cli_session)?;
    // Re-expand the worktree to the full set: the recursive `/**` pattern matches
    // every path at every depth in full (gitignore) matching, so every
    // skip-worktree bit is cleared and missing files are restored.
    let full = SparseCheckout {
        patterns: vec![b"/**".to_vec()],
        sparse_index: false,
    };
    let result = apply_sparse_checkout_with_mode(
        &ctx.worktree_root,
        &ctx.git_dir,
        ctx.format,
        &full,
        SparseCheckoutMode::Full,
    )?;
    warn_sparse_paths(
        &result.not_up_to_date,
        &result.unmerged,
        &result.untracked_sparse_directories,
    );
    // Mirror upstream: turn every sparse knob off in the per-worktree config but
    // leave the pattern file on disk for a later re-init. Upstream also enables
    // the worktree-config extension here even when the worktree was never sparse,
    // so the disabled state is recorded per-worktree.
    ensure_worktree_config_extension(&ctx)?;
    let mut config = read_worktree_config(&ctx.git_dir)?;
    set_config_value(&mut config, "core", None, "sparseCheckout", "false");
    set_config_value(&mut config, "core", None, "sparseCheckoutCone", "false");
    set_config_value(&mut config, "index", None, "sparse", "false");
    write_worktree_config(&ctx.git_dir, &config)?;
    Ok(())
}

const CHECK_RULES_HELP: &str = "usage: git sparse-checkout check-rules [-z] [--skip-checks][--[no-]cone] [--rules-file <file>]\n\n    -z                    terminate input and output files by a NUL character\n    --[no-]cone           when used with --rules-file interpret patterns as cone mode patterns\n    --rules-file <file>   use patterns in <file> instead of the current ones.\n";

/// `git sparse-checkout check-rules`: read paths from stdin and echo back the
/// subset that *would be* present under the active (or supplied) sparse rules.
///
/// This is the read-only counterpart of the apply engine. It does not require a
/// worktree (it runs in bare repos), and it never mutates state. Under `-z`,
/// input and output are NUL-delimited and input paths are taken verbatim;
/// otherwise a leading-`"` line is C-unquoted on input and any matching path is
/// re-quoted with git's `quote_c_style` on output.
fn cmd_sparse_check_rules(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let mut cone = ConeFlag::Unset;
    let mut null_terminated = false;
    let mut rules_file: Option<String> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-z" => null_terminated = true,
            "--cone" => cone = ConeFlag::Cone,
            "--no-cone" => cone = ConeFlag::NoCone,
            // Accepted for option-grammar compatibility; check-rules performs no
            // path sanity checks of its own.
            "--skip-checks" => {}
            "--rules-file" => {
                let Some(path) = iter.next() else {
                    return unknown_option("--rules-file", CHECK_RULES_HELP);
                };
                rules_file = Some(path.clone());
            }
            other if other.starts_with("--rules-file=") => {
                rules_file = Some(other["--rules-file=".len()..].to_string());
            }
            other => return unknown_option(other, CHECK_RULES_HELP),
        }
    }

    let ctx = sparse_context_no_worktree(cli_session)?;

    // Resolve the matching mode. With --rules-file and no explicit cone flag,
    // upstream defaults to cone. Otherwise an explicit flag wins, then the
    // worktree's recorded `core.sparseCheckoutCone`.
    let cone_mode = match cone {
        ConeFlag::Cone => true,
        ConeFlag::NoCone => false,
        ConeFlag::Unset => {
            if rules_file.is_some() {
                true
            } else {
                sparse_cone_enabled(&ctx).unwrap_or(false)
            }
        }
    };

    // Load the patterns. With --rules-file each line is an *input* directory (in
    // cone mode) or gitignore pattern (non-cone), exactly like `set`/`add`
    // arguments — so cone input is converted to the cone-grammar pattern file
    // form before matching. Without --rules-file we read the already-serialized
    // worktree pattern file, which is consumed as-is.
    let patterns: Vec<Vec<u8>> = match &rules_file {
        Some(path) => {
            let mut lines: Vec<Vec<u8>> = fs::read(path)?
                .split(|byte| *byte == b'\n')
                .map(<[u8]>::to_vec)
                .collect();
            if lines.last().map(Vec::is_empty) == Some(true) {
                lines.pop();
            }
            // A line beginning with `"` is a C-quoted path that must be decoded.
            let decoded: Vec<Vec<u8>> = lines
                .into_iter()
                .filter(|line| !line.is_empty())
                .map(|line| {
                    if line.first() == Some(&b'"') {
                        let mut buf = Vec::new();
                        if unquote_c_style(&line, &mut buf).is_some() {
                            return buf;
                        }
                    }
                    line
                })
                .collect();
            if cone_mode {
                let dirs: Vec<Vec<u8>> = decoded
                    .iter()
                    .map(|line| normalize_cone_dir(line))
                    .collect();
                build_cone_file(&dirs)
                    .split(|byte| *byte == b'\n')
                    .filter(|line| !line.is_empty())
                    .map(<[u8]>::to_vec)
                    .collect()
            } else {
                decoded
            }
        }
        None => read_sparse_patterns(&ctx)?.unwrap_or_default(),
    };

    let sparse = SparseCheckout {
        patterns,
        sparse_index: false,
    };
    let mode = if cone_mode {
        SparseCheckoutMode::Cone
    } else {
        SparseCheckoutMode::Full
    };

    let mut input = Vec::new();
    io::stdin().read_to_end(&mut input)?;
    let delimiter = if null_terminated { 0u8 } else { b'\n' };
    let mut out = io::stdout();
    for raw in input.split(|byte| *byte == delimiter) {
        // The trailing element after the final delimiter is empty; skip it.
        if raw.is_empty() {
            continue;
        }
        let line = raw.strip_suffix(b"\r").unwrap_or(raw);
        // Outside -z, a line beginning with `"` is a C-quoted path that must be
        // decoded before matching.
        let decoded: Vec<u8> = if !null_terminated && line.first() == Some(&b'"') {
            let mut buf = Vec::new();
            if unquote_c_style(line, &mut buf).is_none() {
                eprintln!(
                    "fatal: unable to unquote C-style string '{}'",
                    String::from_utf8_lossy(line)
                );
                return Err(GitError::Exit(128));
            }
            buf
        } else {
            line.to_vec()
        };
        if path_in_sparse_checkout(&decoded, &sparse, mode) {
            if null_terminated {
                // Under -z paths are emitted verbatim, NUL-terminated.
                out.write_all(&decoded)?;
                out.write_all(&[0])?;
            } else {
                write_status_quoted_path(&mut out, &decoded, false)?;
                out.write_all(b"\n")?;
            }
        }
    }
    Ok(())
}

const CLEAN_HELP: &str = "usage: git sparse-checkout clean [-n|--dry-run]\n\n    -n, --dry-run         dry run\n    -f, --force           force\n    -v, --verbose         report each affected file, not just directories\n";

/// `git sparse-checkout clean`: remove worktree directories that have fully
/// fallen out of the cone (the directories git would collapse into sparse
/// directory entries). It is the worktree-cleanup counterpart of the sparse
/// index: an out-of-cone directory that still has stray files on disk is
/// removed wholesale.
fn cmd_sparse_clean(cli_session: &crate::session::CliSession, args: &[String]) -> Result<()> {
    let ctx = sparse_context(cli_session)?;
    if !sparse_checkout_enabled(&ctx)? {
        eprintln!("fatal: must be in a sparse-checkout to clean directories");
        return Err(GitError::Exit(128));
    }
    if !sparse_cone_enabled(&ctx)? {
        eprintln!("fatal: must be in a cone-mode sparse-checkout to clean directories");
        return Err(GitError::Exit(128));
    }

    let mut dry_run = false;
    let mut force = false;
    let mut verbose = false;
    for arg in args {
        match arg.as_str() {
            "-n" | "--dry-run" => dry_run = true,
            "-f" | "--force" => force = true,
            "-v" | "--verbose" => verbose = true,
            other => return unknown_option(other, CLEAN_HELP),
        }
    }

    // `clean.requireForce` defaults to true: without --force or --dry-run, refuse.
    let require_force = clean_require_force(&ctx)?;
    if require_force && !force && !dry_run {
        eprintln!("fatal: for safety, refusing to clean without one of --force or --dry-run");
        return Err(GitError::Exit(128));
    }

    let index_path = sley_worktree::repository_index_path(&ctx.git_dir);
    if index_path.exists() {
        let index = Index::parse(&fs::read(&index_path)?, ctx.format)?;
        if index
            .entries
            .iter()
            .any(|entry| entry.stage() != sley_index::Stage::Normal)
        {
            eprintln!(
                "fatal: failed to convert index to a sparse index; resolve merge conflicts and try again"
            );
            return Err(GitError::Exit(128));
        }
    }

    let patterns = read_sparse_patterns(&ctx)?.unwrap_or_default();
    let sparse_dirs = sparse_directories(&ctx, &patterns)?;

    let msg_prefix = if dry_run { "Would remove" } else { "Removing" };
    let mut out = io::stdout();
    for dir in &sparse_dirs {
        let abs = ctx.worktree_root.join(bytes_to_os_path(dir));
        if !abs.is_dir() {
            continue;
        }
        if verbose {
            // Report every file inside the directory rather than the directory.
            for file in list_files_recursive(&abs)? {
                let mut rel = dir.clone();
                if !rel.is_empty() && !rel.ends_with(b"/") {
                    rel.push(b'/');
                }
                rel.extend_from_slice(&file);
                writeln!(out, "{msg_prefix} {}", String::from_utf8_lossy(&rel))?;
            }
        } else {
            writeln!(out, "{msg_prefix} {}/", String::from_utf8_lossy(dir))?;
        }
        if !dry_run {
            fs::remove_dir_all(&abs)?;
        }
    }
    Ok(())
}

/// Reads `clean.requireForce` from the repository config, defaulting to `true`.
fn clean_require_force(ctx: &SparseContext) -> Result<bool> {
    let common = common_git_dir_for_git_dir(&ctx.git_dir)?;
    let config_path = common.join("config");
    if config_path.exists() {
        let config = GitConfig::read(&config_path)?;
        if let Some(value) = config.get_bool("clean", None, "requireForce") {
            return Ok(value);
        }
    }
    Ok(true)
}

/// Computes the set of worktree-relative directory paths (without a trailing
/// slash) that have *entirely* fallen out of the cone: every tracked path under
/// the directory is skip-worktree and the directory itself is out-of-cone. These
/// are exactly the directories git collapses into sparse-directory entries, and
/// hence the directories `clean` removes.
fn sparse_directories(ctx: &SparseContext, patterns: &[Vec<u8>]) -> Result<Vec<Vec<u8>>> {
    use std::collections::BTreeMap;
    let index_path = sley_worktree::repository_index_path(&ctx.git_dir);
    if !index_path.exists() {
        return Ok(Vec::new());
    }
    let bytes = fs::read(&index_path)?;
    let mut index = Index::parse(&bytes, ctx.format)?;
    if index.entries.iter().any(IndexEntry::is_sparse_dir) {
        let odb = FileObjectDatabase::from_git_dir(&ctx.git_dir, ctx.format);
        sley_worktree::expand_sparse_index(&mut index, &odb, ctx.format)?;
    }
    let sparse = SparseCheckout {
        patterns: patterns.to_vec(),
        sparse_index: false,
    };

    // For every tracked path, find the shallowest ancestor directory that is
    // wholly out of cone. A directory is collapsible when the cone does not keep
    // any path inside it, i.e. it has no in-cone descendant and no parent guard.
    // We approximate git's cache-tree collapse by grouping paths by their
    // top-most out-of-cone directory prefix.
    let mut dir_state: BTreeMap<Vec<u8>, bool> = BTreeMap::new();
    for entry in &index.entries {
        if entry.stage() != sley_index::Stage::Normal {
            return Ok(Vec::new());
        }
        let path = entry.path.as_bytes();
        let worktree_file_exists = ctx
            .worktree_root
            .join(bytes_to_os_path(path))
            .symlink_metadata()
            .is_ok();
        let blocks_collapse = path_in_sparse_checkout(path, &sparse, SparseCheckoutMode::Cone)
            || !entry.is_skip_worktree()
            || worktree_file_exists;
        // Record, for each ancestor directory of this path, whether any in-cone
        // or explicitly present tracked file lives beneath it.
        let mut start = 0usize;
        while let Some(rel) = path
            .get(start..)
            .and_then(|s| s.iter().position(|b| *b == b'/'))
        {
            let end = start + rel;
            let dir = path[..end].to_vec();
            let entry = dir_state.entry(dir).or_insert(false);
            *entry = *entry || blocks_collapse;
            start = end + 1;
        }
    }

    // A directory is a sparse (collapsible) directory when it has no in-cone
    // descendant. The shallowest such directory subsumes deeper ones, so keep
    // only top-level collapsed directories.
    let all_collapsed: Vec<Vec<u8>> = dir_state
        .iter()
        .filter(|(_, has_in_cone)| !**has_in_cone)
        .map(|(dir, _)| dir.clone())
        .collect();
    // Keep only the shallowest collapsed directories: a directory whose strict
    // ancestor is also collapsed is subsumed by it.
    let mut collapsed: Vec<Vec<u8>> = all_collapsed
        .iter()
        .filter(|dir| {
            !all_collapsed.iter().any(|other| {
                other != *dir
                    && dir
                        .strip_prefix(other.as_slice())
                        .is_some_and(|rest| rest.first() == Some(&b'/'))
            })
        })
        .cloned()
        .collect();
    collapsed.sort();
    Ok(collapsed)
}

/// Lists every file beneath `root`, returned as paths relative to `root` (with a
/// leading component, no leading slash). Used for `clean --verbose`.
fn list_files_recursive(root: &Path) -> Result<Vec<Vec<u8>>> {
    let mut files = Vec::new();
    let mut stack = vec![(root.to_path_buf(), Vec::<u8>::new())];
    while let Some((dir, prefix)) = stack.pop() {
        let mut entries: Vec<_> = fs::read_dir(&dir)?.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let name = entry.file_name();
            let mut rel = prefix.clone();
            rel.extend_from_slice(&os_str_to_bytes(&name));
            let metadata = entry.metadata()?;
            if metadata.is_dir() {
                let mut child_prefix = rel.clone();
                child_prefix.push(b'/');
                stack.push((entry.path(), child_prefix));
            } else {
                files.push(rel);
            }
        }
    }
    files.sort();
    Ok(files)
}

// --------------------------------------------------------------------------
// Argument parsing shared by `set` and `add`
// --------------------------------------------------------------------------

struct SetLikeArgs {
    cone: ConeFlag,
    sparse_index: SparseIndexFlag,
    skip_checks: bool,
    from_stdin: bool,
    patterns: Vec<Vec<u8>>,
}

/// Parses the common `set`/`add` option grammar. `allow_cone` controls whether
/// the `--cone`/`--no-cone` toggles are accepted (`set` accepts them, `add` does
/// not).
fn parse_set_like(args: &[String], help: &str, allow_cone: bool) -> Result<SetLikeArgs> {
    let mut cone = ConeFlag::Unset;
    let mut sparse_index = SparseIndexFlag::Unset;
    let mut skip_checks = false;
    let mut read_stdin = false;
    let mut positionals: Vec<Vec<u8>> = Vec::new();
    let mut no_more_flags = false;
    for arg in args {
        if no_more_flags {
            positionals.push(arg.clone().into_bytes());
            continue;
        }
        match arg.as_str() {
            "--" | "--end-of-options" => no_more_flags = true,
            "--cone" if allow_cone => cone = ConeFlag::Cone,
            "--no-cone" if allow_cone => cone = ConeFlag::NoCone,
            "--skip-checks" => skip_checks = true,
            "--stdin" => read_stdin = true,
            "--no-stdin" => read_stdin = false,
            "--sparse-index" if allow_cone => sparse_index = SparseIndexFlag::Enable,
            "--no-sparse-index" if allow_cone => sparse_index = SparseIndexFlag::Disable,
            other if other.starts_with('-') => return unknown_option(other, help),
            other => positionals.push(other.as_bytes().to_vec()),
        }
    }
    let patterns = if read_stdin {
        // `--stdin` reads newline-delimited patterns and ignores positionals,
        // matching upstream precedence.
        let mut input = Vec::new();
        io::stdin().read_to_end(&mut input)?;
        input
            .split(|byte| *byte == b'\n')
            .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
            .filter(|line| !line.is_empty())
            .map(|line| {
                if line.first() == Some(&b'"') {
                    let mut decoded = Vec::new();
                    if unquote_c_style(line, &mut decoded).is_some() {
                        return decoded;
                    }
                }
                line.to_vec()
            })
            .collect()
    } else {
        positionals
    };
    Ok(SetLikeArgs {
        cone,
        sparse_index,
        skip_checks,
        from_stdin: read_stdin,
        patterns,
    })
}

// --------------------------------------------------------------------------
// Pattern-file serialization
// --------------------------------------------------------------------------

/// Validates and serializes the pattern file body for a `set`-style invocation,
/// choosing the cone or non-cone form based on the resolved mode. No file is
/// written; the caller commits the result only after validation succeeds.
fn build_pattern_content(
    cone_mode: bool,
    patterns: &[Vec<u8>],
    skip_checks: bool,
) -> Result<Vec<u8>> {
    if cone_mode {
        let mut dirs = Vec::with_capacity(patterns.len());
        for pattern in patterns {
            dirs.push(validate_cone_dir(pattern, skip_checks)?);
        }
        Ok(build_cone_file(&dirs))
    } else if patterns.is_empty() {
        Ok(b"/*\n!/*/\n".to_vec())
    } else {
        Ok(serialize_noncone_lines(patterns))
    }
}

/// Applies git's `sanitize_paths` to the user's `set`/`add` arguments:
///
/// * In cone mode, a non-empty `prefix` (the command was run from a subdir) is
///   prepended to every argument and `..` components are resolved, so
///   `git -C sub sparse-checkout set ../foo bar` records `foo` and `sub/bar`.
/// * In non-cone mode, running from a subdirectory is an error.
///
/// Returns the prefixed argument list (cone) or the originals (non-cone, top).
fn sanitize_set_paths(
    ctx: &SparseContext,
    args: &[Vec<u8>],
    cone_mode: bool,
    skip_checks: bool,
) -> Result<Vec<Vec<u8>>> {
    if args.is_empty() {
        return Ok(Vec::new());
    }
    let prefix = &ctx.prefix;
    let resolved: Vec<Vec<u8>> = if cone_mode && !prefix.is_empty() {
        args.iter().map(|arg| prefix_path(prefix, arg)).collect()
    } else {
        args.to_vec()
    };

    if skip_checks {
        return Ok(resolved);
    }

    if !cone_mode && !prefix.is_empty() {
        eprintln!("fatal: please run from the toplevel directory in non-cone mode");
        return Err(GitError::Exit(128));
    }

    // Reject (cone) or warn (non-cone) when an argument names a *tracked file*
    // rather than a directory — git's index_name_pos / S_ISSPARSEDIR check.
    let tracked = tracked_file_paths(ctx)?;
    for arg in &resolved {
        if tracked.contains(arg.as_slice()) {
            if cone_mode {
                eprintln!(
                    "fatal: '{}' is not a directory; to treat it as a directory anyway, rerun with --skip-checks",
                    String::from_utf8_lossy(arg)
                );
                return Err(GitError::Exit(128));
            }
            eprintln!(
                "warning: pass a leading slash before paths such as '{}' if you want a single file (see NON-CONE PROBLEMS in the git-sparse-checkout manual).",
                String::from_utf8_lossy(arg)
            );
        }
    }
    Ok(resolved)
}

/// Collects the set of tracked (stage-0) file paths in the index, used by
/// `sanitize_set_paths` to detect a directory argument that is actually a file.
fn tracked_file_paths(ctx: &SparseContext) -> Result<std::collections::HashSet<Vec<u8>>> {
    use std::collections::HashSet;
    let index_path = sley_worktree::repository_index_path(&ctx.git_dir);
    let mut set = HashSet::new();
    if !index_path.exists() {
        return Ok(set);
    }
    let bytes = fs::read(&index_path)?;
    Index::for_each_path(&bytes, ctx.format, |path| {
        set.insert(path.to_vec());
        Ok(())
    })?;
    Ok(set)
}

/// Resolves a path argument against `prefix` (a worktree-relative directory with
/// a trailing `/`), collapsing `.` and `..` components, mirroring git's
/// `prefix_path`. An absolute argument (leading `/`) is returned unchanged so
/// the cone validator can later reject it.
fn prefix_path(prefix: &[u8], arg: &[u8]) -> Vec<u8> {
    if arg.first() == Some(&b'/') {
        return arg.to_vec();
    }
    // Join prefix + arg, then normalize the component stack.
    let mut joined = prefix.to_vec();
    joined.extend_from_slice(arg);
    let mut stack: Vec<&[u8]> = Vec::new();
    for component in joined.split(|byte| *byte == b'/') {
        match component {
            b"" | b"." => {}
            b".." => {
                stack.pop();
            }
            other => stack.push(other),
        }
    }
    stack.join(&b"/"[..])
}

/// Validates and normalizes one cone-mode directory argument: leading/trailing
/// slashes are stripped, and (unless `--skip-checks`) a leading slash is rejected
/// the way upstream rejects "patterns" passed where a directory is expected.
fn validate_cone_dir(arg: &[u8], skip_checks: bool) -> Result<Vec<u8>> {
    if !skip_checks && arg.starts_with(b"/") {
        eprintln!("fatal: specify directories rather than patterns (no leading slash)");
        return Err(GitError::Exit(128));
    }
    if !skip_checks && arg.starts_with(b"!") {
        eprintln!(
            "fatal: specify directories rather than patterns.  If your directory starts with a '!', pass --skip-checks"
        );
        return Err(GitError::Exit(128));
    }
    if !skip_checks
        && arg
            .iter()
            .any(|byte| matches!(*byte, b'*' | b'?' | b'[' | b']'))
    {
        eprintln!(
            "fatal: specify directories rather than patterns.  If your directory really has any of '*?[]\\' in it, pass --skip-checks"
        );
        return Err(GitError::Exit(128));
    }
    Ok(normalize_cone_dir(arg))
}

/// Strips surrounding slashes and a leading `./` so directory arguments collapse
/// to a clean relative path (the empty string meaning the repository root).
fn normalize_cone_dir(arg: &[u8]) -> Vec<u8> {
    let mut dir = arg;
    while let Some(rest) = dir.strip_prefix(b"/") {
        dir = rest;
    }
    while let Some(rest) = dir.strip_suffix(b"/") {
        dir = rest;
    }
    if dir == b"." {
        return Vec::new();
    }
    if let Some(rest) = dir.strip_prefix(b"./") {
        dir = rest;
    }
    dir.to_vec()
}

/// Serializes a non-cone pattern file: the supplied lines verbatim, one per line.
fn serialize_noncone_lines(lines: &[Vec<u8>]) -> Vec<u8> {
    let mut content = Vec::new();
    for line in lines {
        content.extend_from_slice(line);
        content.push(b'\n');
    }
    content
}

/// Builds the cone-mode `info/sparse-checkout` body for `dirs`.
///
/// The algorithm matches upstream `write_cone_to_file`:
/// * A directory is *recursive* (its whole subtree is kept) unless one of its
///   ancestors is also requested, in which case it is redundant and dropped.
/// * Every strict ancestor of a recursive directory is a *parent* that keeps
///   only its own files via a `/dir/` + `!/dir/*/` guard pair.
/// * Output order is: the `/*` / `!/*/` header, then all parents (sorted by the
///   slash-wrapped form), then all recursive directories (same sort).
fn build_cone_file(dirs: &[Vec<u8>]) -> Vec<u8> {
    // De-duplicate and drop the empty (root) entry, which only selects top-level
    // files already covered by the `/*` header.
    let mut requested: Vec<Vec<u8>> = Vec::new();
    for dir in dirs {
        let dir = dir.clone();
        if dir.is_empty() {
            continue;
        }
        if !requested.contains(&dir) {
            requested.push(dir);
        }
    }

    // Recursive set: keep a directory only when no other requested directory is a
    // strict ancestor of it.
    let mut recursive: Vec<Vec<u8>> = Vec::new();
    for dir in &requested {
        let subsumed = requested
            .iter()
            .any(|other| other != dir && dir_is_strict_ancestor(other, dir));
        if !subsumed {
            recursive.push(dir.clone());
        }
    }

    // Parent set: the strict ancestors of every recursive directory.
    let mut parents: Vec<Vec<u8>> = Vec::new();
    for dir in &recursive {
        for ancestor in ancestors(dir) {
            if !parents.contains(&ancestor) {
                parents.push(ancestor);
            }
        }
    }

    sort_cone_dirs(&mut parents);
    sort_cone_dirs(&mut recursive);

    let mut out = Vec::new();
    out.extend_from_slice(b"/*\n!/*/\n");
    for parent in &parents {
        out.push(b'/');
        write_escaped_cone_component(&mut out, parent);
        out.extend_from_slice(b"/\n");
        out.extend_from_slice(b"!/");
        write_escaped_cone_component(&mut out, parent);
        out.extend_from_slice(b"/*/\n");
    }
    for dir in &recursive {
        out.push(b'/');
        write_escaped_cone_component(&mut out, dir);
        out.extend_from_slice(b"/\n");
    }
    out
}

fn write_escaped_cone_component(out: &mut Vec<u8>, path: &[u8]) {
    for byte in path {
        if matches!(*byte, b'*' | b'?' | b'[' | b'\\') {
            out.push(b'\\');
        }
        out.push(*byte);
    }
}

fn unescape_cone_component(path: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(path.len());
    let mut iter = path.iter().copied();
    while let Some(byte) = iter.next() {
        if byte == b'\\'
            && let Some(next @ (b'*' | b'?' | b'[' | b'\\')) = iter.next()
        {
            out.push(next);
            continue;
        }
        out.push(byte);
    }
    out
}

/// Returns `true` when `ancestor` is a strict parent directory of `child`
/// (`a` is a strict ancestor of `a/b`, but not of itself).
fn dir_is_strict_ancestor(ancestor: &[u8], child: &[u8]) -> bool {
    if ancestor.is_empty() {
        return !child.is_empty();
    }
    child
        .strip_prefix(ancestor)
        .is_some_and(|rest| rest.first() == Some(&b'/'))
}

/// All strict ancestors of `dir`, e.g. `a/b/c` -> [`a`, `a/b`].
fn ancestors(dir: &[u8]) -> Vec<Vec<u8>> {
    let mut result = Vec::new();
    for (index, byte) in dir.iter().enumerate() {
        if *byte == b'/' {
            result.push(dir[..index].to_vec());
        }
    }
    result
}

/// Sorts cone directories by their on-disk slash-wrapped form (`/dir/`), so the
/// ordering matches upstream's comparison of the bracketed pattern strings (this
/// makes `/a-b/` sort before `/a/b/`, where `-` < `/`).
fn sort_cone_dirs(dirs: &mut [Vec<u8>]) {
    dirs.sort_by_key(|a| slash_wrapped(a));
}

fn slash_wrapped(dir: &[u8]) -> Vec<u8> {
    let mut wrapped = Vec::with_capacity(dir.len() + 2);
    wrapped.push(b'/');
    wrapped.extend_from_slice(dir);
    wrapped.push(b'/');
    wrapped
}

// --------------------------------------------------------------------------
// `list` output
// --------------------------------------------------------------------------

pub(crate) fn cone_patterns_are_valid(patterns: &[Vec<u8>], warn: bool) -> bool {
    let mut recursive: Vec<Vec<u8>> = Vec::new();
    let mut parent: Vec<Vec<u8>> = Vec::new();
    for raw in patterns {
        let line = clean_pattern_line(raw);
        if line.is_empty() || line.starts_with(b"#") {
            continue;
        }
        if line == b"/*" || line == b"!/*/" {
            continue;
        }
        let (negative, pattern) = if let Some(rest) = line.strip_prefix(b"!") {
            (true, rest)
        } else {
            (false, line)
        };
        if pattern.len() < 2
            || pattern.first() != Some(&b'/')
            || contains_double_star(pattern)
            || contains_unescaped_glob(pattern)
        {
            warn_bad_cone_pattern(line, false, warn);
            return false;
        }
        if negative {
            let Some(dir) = pattern.strip_suffix(b"/*/") else {
                warn_bad_cone_pattern(line, true, warn);
                return false;
            };
            if !recursive.iter().any(|seen| seen == dir) {
                warn_bad_cone_pattern(line, true, warn);
                return false;
            }
            if !parent.iter().any(|seen| seen == dir) {
                parent.push(dir.to_vec());
            }
            recursive.retain(|seen| seen != dir);
        } else {
            if !pattern.ends_with(b"/") || pattern == b"/" {
                warn_bad_cone_pattern(line, false, warn);
                return false;
            }
            let dir = &pattern[..pattern.len() - 1];
            // A recursive literal `*` directory immediately below a parent
            // guard parses to the same cone-pattern slot as that guard in Git's
            // pattern hashmap. Treat it as repeated instead of translating the
            // malformed file back to a directory name.
            let repeated_guard = dir
                .strip_suffix(b"/\\*")
                .is_some_and(|guarded_parent| parent.iter().any(|seen| seen == guarded_parent));
            if repeated_guard || parent.iter().any(|seen| seen == dir) {
                if warn {
                    eprintln!(
                        "warning: your sparse-checkout file may have issues: pattern '{}' is repeated",
                        String::from_utf8_lossy(line)
                    );
                    eprintln!("warning: disabling cone pattern matching");
                }
                return false;
            }
            if !recursive.iter().any(|seen| seen == dir) {
                recursive.push(dir.to_vec());
            }
        }
    }
    true
}

fn warn_bad_cone_pattern(pattern: &[u8], negative: bool, warn: bool) {
    if !warn {
        return;
    }
    if negative {
        eprintln!(
            "warning: unrecognized negative pattern: '{}'",
            String::from_utf8_lossy(pattern)
        );
    } else {
        eprintln!(
            "warning: unrecognized pattern: '{}'",
            String::from_utf8_lossy(pattern)
        );
    }
    eprintln!("warning: disabling cone pattern matching");
}

fn contains_double_star(pattern: &[u8]) -> bool {
    pattern.windows(2).any(|window| window == b"**")
}

fn contains_unescaped_glob(pattern: &[u8]) -> bool {
    for (index, byte) in pattern.iter().enumerate() {
        if !matches!(*byte, b'*' | b'\\' | b'[' | b'?') {
            continue;
        }
        let prev = index.checked_sub(1).and_then(|i| pattern.get(i)).copied();
        let next = pattern.get(index + 1).copied();
        if prev == Some(b'\\') {
            continue;
        }
        if *byte == b'\\' && matches!(next, Some(b'*' | b'\\' | b'[' | b'?')) {
            continue;
        }
        if prev == Some(b'/') && *byte == b'*' && matches!(next, None | Some(b'/')) {
            continue;
        }
        return true;
    }
    false
}

/// Recovers the sorted list of cone directories from a cone pattern file, i.e.
/// the directories `git sparse-checkout list` prints (the recursive leaves, with
/// surrounding slashes stripped). Parent guard lines and the `/*` / `!/*/` header
/// are skipped.
fn cone_list_entries(patterns: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let mut dirs: Vec<Vec<u8>> = Vec::new();
    for raw in patterns {
        let line = clean_pattern_line(raw);
        if line.is_empty() || line.starts_with(b"#") || line.starts_with(b"!") {
            continue;
        }
        if line == b"/*" {
            continue;
        }
        let Some(dir) = line.strip_prefix(b"/").and_then(|r| r.strip_suffix(b"/")) else {
            continue;
        };
        if dir.is_empty() {
            continue;
        }
        if has_parent_guard(patterns, dir) {
            // This `/dir/` is a parent of a deeper directory, not a recursive
            // leaf, so it does not appear in `list`.
            continue;
        }
        let dir = unescape_cone_component(dir);
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    sort_cone_dirs(&mut dirs);
    dirs
}

/// Whether the cone file carries a `!/dir/*/` guard for `dir`, marking it as a
/// parent directory rather than a recursive leaf.
fn has_parent_guard(patterns: &[Vec<u8>], dir: &[u8]) -> bool {
    let mut guard = Vec::with_capacity(dir.len() + 4);
    guard.extend_from_slice(b"!/");
    guard.extend_from_slice(dir);
    guard.extend_from_slice(b"/*/");
    patterns
        .iter()
        .any(|raw| clean_pattern_line(raw) == guard.as_slice())
}

/// Trims a trailing CR and surrounding ASCII whitespace from a raw pattern line.
fn clean_pattern_line(raw: &[u8]) -> &[u8] {
    let line = raw.strip_suffix(b"\r").unwrap_or(raw);
    let start = line
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(line.len());
    let end = line
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    &line[start..end]
}

// --------------------------------------------------------------------------
// Config + pattern-file I/O
// --------------------------------------------------------------------------

/// Resolves the sparse context for a worktree-requiring subcommand (every one
/// but `check-rules`). Mirrors upstream's `setup_work_tree()` at the head of
/// each handler: a bare repository fails with "this operation must be run in a
/// work tree".
fn sparse_context(cli_session: &crate::session::CliSession) -> Result<SparseContext> {
    let cwd = cli_session.cwd();
    let git_dir = cli_session.git_dir()?;
    let format = repository_object_format(&git_dir)?;
    let worktree_root = require_work_tree(&git_dir)?;
    let prefix = sparse_prefix(&worktree_root, cwd)?;
    Ok(SparseContext {
        git_dir,
        worktree_root,
        format,
        prefix,
    })
}

/// Computes git's `prefix`: the worktree-relative path from the worktree top to
/// the current directory, with a trailing `/` (empty at the top). Used to
/// resolve cone-mode directory arguments supplied from a subdirectory.
fn sparse_prefix(worktree_root: &Path, cwd: &Path) -> Result<Vec<u8>> {
    let canonical_root = fs::canonicalize(worktree_root).unwrap_or_else(|_| worktree_root.into());
    let canonical_cwd = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let Ok(rel) = canonical_cwd.strip_prefix(&canonical_root) else {
        return Ok(Vec::new());
    };
    let mut prefix = os_str_to_bytes(rel.as_os_str());
    if prefix.is_empty() {
        return Ok(Vec::new());
    }
    prefix.push(b'/');
    Ok(prefix)
}

/// Resolves a context for `check-rules`, which upstream does *not* gate behind
/// `setup_work_tree()` and so runs in a bare repository. The worktree root is
/// only used to anchor relative paths, so a bare repo falls back to the git
/// directory's parent (it is never read for `check-rules`).
fn sparse_context_no_worktree(cli_session: &crate::session::CliSession) -> Result<SparseContext> {
    let git_dir = cli_session.git_dir()?;
    let format = repository_object_format(&git_dir)?;
    let worktree_root = sley_worktree::worktree_root_for_git_dir(&git_dir)?
        .unwrap_or_else(|| git_dir.parent().unwrap_or(&git_dir).to_path_buf());
    Ok(SparseContext {
        git_dir,
        worktree_root,
        format,
        prefix: Vec::new(),
    })
}

/// Turns the sparse-checkout knobs on in the per-worktree config (enabling the
/// `worktreeConfig` extension in the main config first, exactly like upstream)
/// and records whether cone mode is in effect.
fn enable_sparse_checkout(ctx: &SparseContext, cone: bool) -> Result<()> {
    ensure_worktree_config_extension(ctx)?;
    let mut config = read_worktree_config(&ctx.git_dir)?;
    set_config_value(&mut config, "core", None, "sparseCheckout", "true");
    set_config_value(
        &mut config,
        "core",
        None,
        "sparseCheckoutCone",
        if cone { "true" } else { "false" },
    );
    write_worktree_config(&ctx.git_dir, &config)
}

/// Ensures `extensions.worktreeConfig = true` is set in the main repository
/// config so the per-worktree `config.worktree` file is honored.
///
/// The raw config file is read directly (rather than through the include-aware
/// loader) so a read-modify-write round-trip never inlines `include.path`
/// directives back into the file.
fn ensure_worktree_config_extension(ctx: &SparseContext) -> Result<()> {
    let common = common_git_dir_for_git_dir(&ctx.git_dir)?;
    let config_path = common.join("config");
    let mut config = if config_path.exists() {
        GitConfig::read(&config_path)?
    } else {
        GitConfig::default()
    };
    if config.get_bool("extensions", None, "worktreeConfig") == Some(true) {
        return Ok(());
    }
    set_config_value(&mut config, "extensions", None, "worktreeConfig", "true");
    fs::write(&config_path, config.to_canonical_bytes())?;
    Ok(())
}

/// Records `index.sparse` in the per-worktree config, the toggle that decides
/// whether the index is collapsed into sparse-directory entries on write. This
/// mirrors upstream `set_sparse_index_config`.
fn set_index_sparse(ctx: &SparseContext, enable: bool) -> Result<()> {
    ensure_worktree_config_extension(ctx)?;
    let mut config = read_worktree_config(&ctx.git_dir)?;
    set_config_value(
        &mut config,
        "index",
        None,
        "sparse",
        if enable { "true" } else { "false" },
    );
    write_worktree_config(&ctx.git_dir, &config)
}

/// Reads `index.sparse` from the per-worktree config (default `false`).
fn index_sparse_enabled(ctx: &SparseContext) -> Result<bool> {
    let config = read_worktree_config(&ctx.git_dir)?;
    Ok(config.get_bool("index", None, "sparse").unwrap_or(false))
}

/// Reads `core.sparseCheckout` from the per-worktree config.
fn sparse_checkout_enabled(ctx: &SparseContext) -> Result<bool> {
    let config = read_worktree_config(&ctx.git_dir)?;
    Ok(config
        .get_bool("core", None, "sparseCheckout")
        .unwrap_or(false))
}

/// Reads `core.sparseCheckoutCone` from the per-worktree config.
fn sparse_cone_enabled(ctx: &SparseContext) -> Result<bool> {
    let config = read_worktree_config(&ctx.git_dir)?;
    Ok(config
        .get_bool("core", None, "sparseCheckoutCone")
        .unwrap_or(false))
}

/// Parses the per-worktree `config.worktree` file, returning an empty config when
/// it does not exist yet.
fn read_worktree_config(git_dir: &Path) -> Result<GitConfig> {
    let path = git_dir.join("config.worktree");
    if path.exists() {
        GitConfig::read(path)
    } else {
        Ok(GitConfig::default())
    }
}

fn write_worktree_config(git_dir: &Path, config: &GitConfig) -> Result<()> {
    fs::write(git_dir.join("config.worktree"), config.to_canonical_bytes())?;
    Ok(())
}

/// Reads the lines of `$GIT_DIR/info/sparse-checkout`, or `None` when the file
/// does not exist. The final trailing newline is dropped; embedded blank lines
/// are kept so a round-trip of a non-cone file is faithful.
fn read_sparse_patterns(ctx: &SparseContext) -> Result<Option<Vec<Vec<u8>>>> {
    let path = sparse_file_path(&ctx.git_dir);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path)?;
    let mut lines: Vec<Vec<u8>> = bytes
        .split(|byte| *byte == b'\n')
        .map(<[u8]>::to_vec)
        .collect();
    // `split` on a trailing newline yields a final empty element; drop it so a
    // file ending in `\n` does not gain a spurious blank pattern.
    if lines.last().map(Vec::is_empty) == Some(true) {
        lines.pop();
    }
    Ok(Some(lines))
}

fn write_sparse_file(ctx: &SparseContext, content: &[u8]) -> Result<()> {
    let info_dir = ctx.git_dir.join("info");
    fs::create_dir_all(&info_dir)?;
    let lock_path = info_dir.join("sparse-checkout.lock");
    if lock_path.exists() {
        eprintln!(
            "fatal: Unable to create '{}': File exists.",
            lock_path.display()
        );
        eprintln!();
        eprintln!(
            "Another git process seems to be running in this repository, or the lock file may be stale"
        );
        return Err(GitError::Exit(128));
    }
    fs::write(sparse_file_path(&ctx.git_dir), content)?;
    Ok(())
}

fn sparse_file_path(git_dir: &Path) -> PathBuf {
    git_dir.join("info").join("sparse-checkout")
}

// --------------------------------------------------------------------------
// Applying the sparse spec to the index + worktree
// --------------------------------------------------------------------------

/// Reconciles the index and worktree with the pattern file currently on disk,
/// using the cone vs full matcher implied by `core.sparseCheckoutCone`.
fn apply_current_sparse(ctx: &SparseContext) -> Result<()> {
    let Some(patterns) = read_sparse_patterns(ctx)? else {
        return Ok(());
    };
    let cone = sparse_cone_enabled(ctx)?;
    let mode = if cone {
        SparseCheckoutMode::Cone
    } else {
        SparseCheckoutMode::Full
    };
    // A sparse index is only valid in cone mode; outside cone mode the worktree
    // stays full even if `index.sparse` is recorded.
    let sparse = SparseCheckout {
        patterns,
        sparse_index: cone && index_sparse_enabled(ctx)?,
    };
    let result = apply_sparse_checkout_with_mode(
        &ctx.worktree_root,
        &ctx.git_dir,
        ctx.format,
        &sparse,
        mode,
    )?;
    warn_sparse_paths(
        &result.not_up_to_date,
        &result.unmerged,
        &result.untracked_sparse_directories,
    );
    Ok(())
}

/// Emit git's warning for out-of-cone paths that were left in place because their
/// worktree file was not up to date. Byte-for-byte identical to upstream,
/// including the leading-tab list, the trailing blank line, and the follow-up
/// "After fixing …" sentence. No-op when there is nothing to warn about.
fn warn_sparse_paths(
    not_up_to_date: &[Vec<u8>],
    unmerged: &[Vec<u8>],
    untracked_directories: &[Vec<u8>],
) {
    if not_up_to_date.is_empty() && unmerged.is_empty() && untracked_directories.is_empty() {
        return;
    }
    let mut message = Vec::new();
    if !not_up_to_date.is_empty() {
        message.extend_from_slice(
            b"warning: The following paths are not up to date and were left despite sparse patterns:\n",
        );
        append_warning_paths(&mut message, not_up_to_date);
    }
    if !unmerged.is_empty() {
        message.extend_from_slice(
            b"warning: The following paths are unmerged and were left despite sparse patterns:\n",
        );
        append_warning_paths(&mut message, unmerged);
    }
    if !not_up_to_date.is_empty() || !unmerged.is_empty() {
        message.extend_from_slice(
            b"\nAfter fixing the above paths, you may want to run `git sparse-checkout reapply`.\n",
        );
    }
    for directory in untracked_directories {
        message.extend_from_slice(b"warning: directory '");
        message.extend_from_slice(directory);
        message.extend_from_slice(
            b"' contains untracked files, but is not in the sparse-checkout cone\n",
        );
    }
    let _ = io::stderr().write_all(&message);
}

fn append_warning_paths(message: &mut Vec<u8>, paths: &[Vec<u8>]) {
    for path in paths {
        message.push(b'\t');
        message.extend_from_slice(path);
        message.push(b'\n');
    }
}

// --------------------------------------------------------------------------
// Misc
// --------------------------------------------------------------------------

/// Prints upstream's `error: unknown option ...` line followed by the relevant
/// usage + option-help block, then signals the conventional 129 exit code.
fn unknown_option<T>(option: &str, help: &str) -> Result<T> {
    eprintln!("error: unknown option `{}'", option.trim_start_matches('-'));
    eprint!("{help}");
    eprintln!();
    Err(GitError::Exit(129))
}
