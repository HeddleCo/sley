//! `git sparse-checkout` and its subcommands
//! (init / list / set / add / reapply / disable).
//!
//! This mirrors upstream `git sparse-checkout`: it toggles
//! `core.sparseCheckout` / `core.sparseCheckoutCone` (stored in the per-worktree
//! config, with the `extensions.worktreeConfig` extension enabled in the main
//! config exactly as upstream does), maintains the `$GIT_DIR/info/sparse-checkout`
//! pattern file, and reconciles the index + worktree through the committed sparse
//! engine in [`git_worktree::apply_sparse_checkout_with_mode`].
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

use git_worktree::{apply_sparse_checkout_with_mode, SparseCheckout, SparseCheckoutMode};

const SPARSE_USAGE: &str =
    "usage: git sparse-checkout (init | list | set | add | reapply | disable | check-rules | clean) [<options>]";

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

/// Where the per-worktree sparse settings (and the pattern file) live for the
/// current repository.
struct SparseContext {
    git_dir: PathBuf,
    worktree_root: PathBuf,
    format: ObjectFormat,
}

pub(crate) fn cmd_sparse_checkout(args: &[String]) -> Result<()> {
    let Some(sub) = args.first() else {
        eprintln!("error: need a subcommand");
        eprintln!("{SPARSE_USAGE}");
        eprintln!();
        return Err(GitError::Exit(129));
    };
    match sub.as_str() {
        "init" => cmd_sparse_init(&args[1..]),
        "list" => cmd_sparse_list(&args[1..]),
        "set" => cmd_sparse_set(&args[1..]),
        "add" => cmd_sparse_add(&args[1..]),
        "reapply" => cmd_sparse_reapply(&args[1..]),
        "disable" => cmd_sparse_disable(&args[1..]),
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

fn cmd_sparse_init(args: &[String]) -> Result<()> {
    let mut cone = ConeFlag::Unset;
    for arg in args {
        match arg.as_str() {
            "--cone" => cone = ConeFlag::Cone,
            "--no-cone" => cone = ConeFlag::NoCone,
            // The sparse-index optimization is accepted for CLI compatibility but
            // not materialized; the worktree result is identical, just not stored
            // as a collapsed index.
            "--sparse-index" | "--no-sparse-index" => {}
            other => return unknown_option(other, INIT_HELP),
        }
    }
    let ctx = sparse_context()?;
    // Cone is the default at init time unless explicitly disabled.
    let cone_mode = !matches!(cone, ConeFlag::NoCone);
    enable_sparse_checkout(&ctx, cone_mode)?;
    // Preserve any existing patterns; otherwise seed the cone-style root file so
    // a fresh init leaves only top-level files in the worktree.
    if read_sparse_patterns(&ctx)?.is_none() {
        write_sparse_file(&ctx, b"/*\n!/*/\n")?;
    }
    apply_current_sparse(&ctx)?;
    Ok(())
}

fn cmd_sparse_list(args: &[String]) -> Result<()> {
    let ctx = sparse_context()?;
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
    let patterns = read_sparse_patterns(&ctx)?.unwrap_or_default();
    let cone = sparse_cone_enabled(&ctx)?;
    let mut out = io::stdout();
    if cone {
        for dir in cone_list_entries(&patterns) {
            out.write_all(&dir)?;
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

fn cmd_sparse_set(args: &[String]) -> Result<()> {
    let parsed = parse_set_like(args, SET_HELP, true)?;
    let ctx = sparse_context()?;
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
    // Validate and serialize the new pattern file before mutating any state, so a
    // rejected pattern (e.g. a leading slash in cone mode) leaves the config and
    // pattern file untouched.
    let content = build_pattern_content(cone_mode, &parsed.patterns, parsed.skip_checks)?;
    enable_sparse_checkout(&ctx, cone_mode)?;
    write_sparse_file(&ctx, &content)?;
    apply_current_sparse(&ctx)?;
    Ok(())
}

fn cmd_sparse_add(args: &[String]) -> Result<()> {
    let ctx = sparse_context()?;
    // Upstream checks for an existing sparse-checkout before option parsing.
    if !sparse_checkout_enabled(&ctx)? {
        eprintln!("fatal: no sparse-checkout to add to");
        return Err(GitError::Exit(128));
    }
    // `add` does not accept the cone toggles (they are not in its option set).
    let parsed = parse_set_like(args, ADD_HELP, false)?;
    let cone_mode = sparse_cone_enabled(&ctx)?;
    let existing = read_sparse_patterns(&ctx)?.unwrap_or_default();
    // Build (and validate) the merged pattern file before writing anything.
    let content = if cone_mode {
        // Recover the directory set from the existing cone file and union it with
        // the new directories, then regenerate.
        let mut dirs = cone_list_entries(&existing);
        for pattern in &parsed.patterns {
            dirs.push(validate_cone_dir(pattern, parsed.skip_checks)?);
        }
        build_cone_file(&dirs)
    } else {
        let mut lines = existing;
        for pattern in &parsed.patterns {
            lines.push(pattern.clone());
        }
        serialize_noncone_lines(&lines)
    };
    write_sparse_file(&ctx, &content)?;
    apply_current_sparse(&ctx)?;
    Ok(())
}

fn cmd_sparse_reapply(args: &[String]) -> Result<()> {
    let ctx = sparse_context()?;
    // Upstream requires an active sparse-checkout before it parses options.
    if !sparse_checkout_enabled(&ctx)? {
        eprintln!("fatal: must be in a sparse-checkout to reapply sparsity patterns");
        return Err(GitError::Exit(128));
    }
    let mut cone = ConeFlag::Unset;
    for arg in args {
        match arg.as_str() {
            "--cone" => cone = ConeFlag::Cone,
            "--no-cone" => cone = ConeFlag::NoCone,
            // Accepted for compatibility; the sparse-index is not materialized.
            "--sparse-index" | "--no-sparse-index" => {}
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
    apply_current_sparse(&ctx)?;
    Ok(())
}

fn cmd_sparse_disable(args: &[String]) -> Result<()> {
    // `disable` has no options; reject flags but ignore stray positionals.
    if let Some(arg) = args.iter().find(|arg| arg.starts_with('-')) {
        return unknown_option(arg.as_str(), DISABLE_HELP);
    }
    let ctx = sparse_context()?;
    // Re-expand the worktree to the full set: the recursive `/**` pattern matches
    // every path at every depth in full (gitignore) matching, so every
    // skip-worktree bit is cleared and missing files are restored.
    let full = SparseCheckout {
        patterns: vec![b"/**".to_vec()],
        sparse_index: false,
    };
    apply_sparse_checkout_with_mode(
        &ctx.worktree_root,
        &ctx.git_dir,
        ctx.format,
        &full,
        SparseCheckoutMode::Full,
    )?;
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

// --------------------------------------------------------------------------
// Argument parsing shared by `set` and `add`
// --------------------------------------------------------------------------

struct SetLikeArgs {
    cone: ConeFlag,
    skip_checks: bool,
    patterns: Vec<Vec<u8>>,
}

/// Parses the common `set`/`add` option grammar. `allow_cone` controls whether
/// the `--cone`/`--no-cone` toggles are accepted (`set` accepts them, `add` does
/// not).
fn parse_set_like(args: &[String], help: &str, allow_cone: bool) -> Result<SetLikeArgs> {
    let mut cone = ConeFlag::Unset;
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
            "--" => no_more_flags = true,
            "--cone" if allow_cone => cone = ConeFlag::Cone,
            "--no-cone" if allow_cone => cone = ConeFlag::NoCone,
            "--skip-checks" => skip_checks = true,
            "--stdin" => read_stdin = true,
            "--no-stdin" => read_stdin = false,
            // Accepted for compatibility; the sparse-index is not materialized.
            "--sparse-index" | "--no-sparse-index" => {}
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
            .map(<[u8]>::to_vec)
            .collect()
    } else {
        positionals
    };
    Ok(SetLikeArgs {
        cone,
        skip_checks,
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
    } else {
        Ok(serialize_noncone_lines(patterns))
    }
}

/// Validates and normalizes one cone-mode directory argument: leading/trailing
/// slashes are stripped, and (unless `--skip-checks`) a leading slash is rejected
/// the way upstream rejects "patterns" passed where a directory is expected.
fn validate_cone_dir(arg: &[u8], skip_checks: bool) -> Result<Vec<u8>> {
    if !skip_checks && arg.starts_with(b"/") {
        eprintln!("fatal: specify directories rather than patterns (no leading slash)");
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
        out.extend_from_slice(parent);
        out.extend_from_slice(b"/\n");
        out.extend_from_slice(b"!/");
        out.extend_from_slice(parent);
        out.extend_from_slice(b"/*/\n");
    }
    for dir in &recursive {
        out.push(b'/');
        out.extend_from_slice(dir);
        out.extend_from_slice(b"/\n");
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
    dirs.sort_by(|a, b| slash_wrapped(a).cmp(&slash_wrapped(b)));
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
        let dir = dir.to_vec();
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

fn sparse_context() -> Result<SparseContext> {
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    Ok(SparseContext {
        git_dir,
        worktree_root,
        format,
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
    let sparse = SparseCheckout {
        patterns,
        sparse_index: false,
    };
    apply_sparse_checkout_with_mode(&ctx.worktree_root, &ctx.git_dir, ctx.format, &sparse, mode)?;
    Ok(())
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
