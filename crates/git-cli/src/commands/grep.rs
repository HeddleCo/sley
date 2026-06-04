//! `git grep` — search tracked content for a pattern.
//!
//! Searches the working tree by default, or the index (`--cached`) / one or more
//! tree-ishes (revisions named on the command line). Output matches Git's
//! `<path>:<line>:<text>` form (with a `<rev>:` prefix when searching a tree-ish),
//! and the common reporting flags (`-n`, `-l`, `-c`, `-i`, `-w`, `-v`, `-F`, ...).
//!
//! As with the other extracted commands, a glob of the crate root pulls in the
//! shared plumbing (`discover_git_dir`, `repository_object_format`,
//! `resolve_revision`, `worktree_root_for_git_dir`, `FileObjectDatabase`, ...);
//! see `commands::stash` for the rationale.

use crate::*;

/// How the regular expression text is interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatternKind {
    /// POSIX basic regular expressions (Git's default).
    Basic,
    /// POSIX extended regular expressions (`-E` / `--extended-regexp`).
    Extended,
    /// Fixed strings (`-F` / `--fixed-strings`).
    Fixed,
}

/// Parsed command-line options for `git grep`.
struct GrepOptions {
    patterns: Vec<String>,
    kind: PatternKind,
    ignore_case: bool,
    word: bool,
    line_regexp: bool,
    invert: bool,
    line_number: bool,
    files_with_matches: bool,
    files_without_match: bool,
    count: bool,
    name_only_quiet: bool,
    show_filename: Option<bool>,
    only_matching: bool,
    text: bool,
    ignore_binary: bool,
    full_name: bool,
    null_data: bool,
    cached: bool,
    revs: Vec<String>,
    pathspecs: Vec<String>,
}

impl GrepOptions {
    fn new() -> Self {
        Self {
            patterns: Vec::new(),
            kind: PatternKind::Basic,
            ignore_case: false,
            word: false,
            line_regexp: false,
            invert: false,
            line_number: false,
            files_with_matches: false,
            files_without_match: false,
            count: false,
            name_only_quiet: false,
            show_filename: None,
            only_matching: false,
            text: false,
            ignore_binary: false,
            full_name: false,
            null_data: false,
            cached: false,
            revs: Vec::new(),
            pathspecs: Vec::new(),
        }
    }
}

pub(crate) fn cmd_grep(args: &[String]) -> Result<()> {
    let mut opts = GrepOptions::new();
    let mut positionals: Vec<String> = Vec::new();
    let mut saw_double_dash = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if saw_double_dash {
            opts.pathspecs.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => saw_double_dash = true,
            "-e" => {
                let Some(value) = iter.next() else {
                    return grep_option_requires_value("-e");
                };
                opts.patterns.push(value.clone());
            }
            value if let Some(value) = value.strip_prefix("-e") => {
                // `-e<pattern>` glued form.
                opts.patterns.push(value.to_string());
            }
            value if let Some(value) = value.strip_prefix("--regexp=") => {
                opts.patterns.push(value.to_string());
            }
            "-E" | "--extended-regexp" => opts.kind = PatternKind::Extended,
            "-G" | "--basic-regexp" => opts.kind = PatternKind::Basic,
            "-F" | "--fixed-strings" => opts.kind = PatternKind::Fixed,
            "-P" | "--perl-regexp" => {
                eprintln!(
                    "fatal: cannot use Perl-compatible regexes; git-rs was not built with PCRE support"
                );
                return Err(GitError::Exit(128));
            }
            "-i" | "--ignore-case" => opts.ignore_case = true,
            "--no-ignore-case" => opts.ignore_case = false,
            "-w" | "--word-regexp" => opts.word = true,
            "-x" | "--line-regexp" => opts.line_regexp = true,
            "-v" | "--invert-match" => opts.invert = true,
            "-n" | "--line-number" => opts.line_number = true,
            "--no-line-number" => opts.line_number = false,
            "-l" | "--files-with-matches" | "--name-only" => opts.files_with_matches = true,
            "-L" | "--files-without-match" => opts.files_without_match = true,
            "-c" | "--count" => opts.count = true,
            "-q" | "--quiet" => opts.name_only_quiet = true,
            "-o" | "--only-matching" => opts.only_matching = true,
            "-H" => opts.show_filename = Some(true),
            "-h" | "--no-filename" => opts.show_filename = Some(false),
            "--full-name" => opts.full_name = true,
            "--no-full-name" => opts.full_name = false,
            "-a" | "--text" => opts.text = true,
            "-I" => opts.ignore_binary = true,
            "-z" | "--null" => opts.null_data = true,
            "--cached" => opts.cached = true,
            "--no-index" => {
                eprintln!("fatal: --no-index is not supported by git-rs grep");
                return Err(GitError::Exit(128));
            }
            "--color"
            | "--no-color"
            | "--recursive"
            | "-r"
            | "--recurse-submodules"
            | "--no-recurse-submodules"
            | "--untracked"
            | "--no-exclude-standard"
            | "--exclude-standard"
            | "--heading"
            | "--no-heading"
            | "--break"
            | "--no-break"
            | "--column"
            | "--no-column" => {
                // Accepted-but-no-op flags whose default behaviour we already match.
            }
            "--threads" | "--max-depth" | "--context" | "-C" | "-A" | "-B" | "--after-context"
            | "--before-context" => {
                // These take a value we currently ignore.
                let _ = iter.next();
            }
            value if value.starts_with("--threads=") || value.starts_with("--max-depth=") => {}
            value if value.starts_with("--color=") => {}
            value if value.starts_with('-') && value.len() > 1 && !value.starts_with("--") => {
                // A bundle of short flags such as `-in` or `-iw`. Expand and
                // re-handle each one; `-e`/`-f` consume the remainder as a value.
                match expand_short_flag_bundle(value, &mut opts)? {
                    ShortBundle::Handled => {}
                    ShortBundle::Unknown(flag) => {
                        return grep_unknown_option(&flag);
                    }
                }
            }
            value if value.starts_with("--") => {
                return grep_unknown_option(value.trim_start_matches('-'));
            }
            other => positionals.push(other.to_string()),
        }
    }

    // The very first positional is the pattern unless one was supplied via `-e`.
    if opts.patterns.is_empty() {
        if positionals.is_empty() {
            eprintln!("fatal: no pattern given");
            return Err(GitError::Exit(128));
        }
        opts.patterns.push(positionals.remove(0));
    }

    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;

    // Disambiguate remaining positionals into revs and pathspecs: each leading
    // positional that resolves to an object is a rev; the first one that does
    // not switches into "path mode", after which everything is a pathspec.
    if !saw_double_dash {
        let mut in_paths = false;
        for value in positionals {
            if in_paths {
                opts.pathspecs.push(value);
                continue;
            }
            match resolve_revision(&git_dir, format, &value) {
                Ok(_) => opts.revs.push(value),
                Err(_) => {
                    in_paths = true;
                    opts.pathspecs.push(value);
                }
            }
        }
    } else {
        // With an explicit `--`, any positionals seen before it are revs.
        for value in positionals {
            opts.revs.push(value);
        }
    }

    let matcher = GrepMatcher::compile(&opts)?;

    // The cwd-relative prefix limits and renders results in every mode (working
    // tree, index, and tree-ish). Searching a tree-ish in a bare repository has
    // no worktree, so derive it best-effort and fall back to an empty prefix.
    let worktree_root = match worktree_root_for_git_dir(&git_dir) {
        Ok(root) if root.is_dir() => Some(root),
        _ => None,
    };
    let pathspec = GrepPathspec::new(
        worktree_root.as_deref(),
        &cwd,
        opts.full_name,
        &opts.pathspecs,
    )?;

    let mut any_match = false;
    let mut out = io::stdout();
    if opts.revs.is_empty() {
        let Some(worktree_root) = worktree_root.as_deref() else {
            return Err(GitError::Command("grep: missing worktree".into()));
        };
        any_match = grep_index_source(
            &git_dir,
            worktree_root,
            format,
            &matcher,
            &opts,
            &pathspec,
            &mut out,
        )?;
    } else {
        let db = FileObjectDatabase::from_git_dir(&git_dir, format);
        for rev in &opts.revs {
            let oid = resolve_revision(&git_dir, format, rev)?;
            let tree_oid = git_rev::peel_to_tree(&db, format, &oid)?;
            let matched = grep_tree_source(
                &db, format, &tree_oid, rev, &matcher, &opts, &pathspec, &mut out,
            )?;
            any_match = any_match || matched;
        }
    }

    pathspec.report_unmatched()?;

    if any_match {
        Ok(())
    } else {
        Err(GitError::Exit(1))
    }
}

/// Result of expanding a `-abc` short-flag bundle.
enum ShortBundle {
    Handled,
    Unknown(String),
}

/// Expands a bundle of single-character flags (e.g. `-iwn`). Stops and treats
/// the remainder as a glued value for value-taking flags (`-e`).
fn expand_short_flag_bundle(bundle: &str, opts: &mut GrepOptions) -> Result<ShortBundle> {
    let chars: Vec<char> = bundle.chars().collect();
    // chars[0] is '-'.
    let mut idx = 1;
    while idx < chars.len() {
        let ch = chars[idx];
        match ch {
            'e' => {
                // The rest of the bundle is the pattern (e.g. `-ehello`).
                let rest: String = chars[idx + 1..].iter().collect();
                if rest.is_empty() {
                    grep_option_requires_value("-e")?;
                }
                opts.patterns.push(rest);
                return Ok(ShortBundle::Handled);
            }
            'E' => opts.kind = PatternKind::Extended,
            'G' => opts.kind = PatternKind::Basic,
            'F' => opts.kind = PatternKind::Fixed,
            'i' => opts.ignore_case = true,
            'w' => opts.word = true,
            'x' => opts.line_regexp = true,
            'v' => opts.invert = true,
            'n' => opts.line_number = true,
            'l' => opts.files_with_matches = true,
            'L' => opts.files_without_match = true,
            'c' => opts.count = true,
            'q' => opts.name_only_quiet = true,
            'o' => opts.only_matching = true,
            'H' => opts.show_filename = Some(true),
            'h' => opts.show_filename = Some(false),
            'a' => opts.text = true,
            'I' => opts.ignore_binary = true,
            'z' => opts.null_data = true,
            other => return Ok(ShortBundle::Unknown(other.to_string())),
        }
        idx += 1;
    }
    Ok(ShortBundle::Handled)
}

fn grep_option_requires_value(flag: &str) -> Result<()> {
    eprintln!(
        "fatal: switch `{}' requires a value",
        flag.trim_start_matches('-')
    );
    Err(GitError::Exit(128))
}

fn grep_unknown_option(flag: &str) -> Result<()> {
    eprintln!("error: unknown option `{flag}'");
    eprintln!("usage: git grep [<options>] [-e] <pattern> [<rev>...] [[--] <path>...]");
    Err(GitError::Exit(128))
}

// ---------------------------------------------------------------------------
// Source iteration
// ---------------------------------------------------------------------------

/// Greps the working tree (default) or the index (`--cached`).
fn grep_index_source(
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    matcher: &GrepMatcher,
    opts: &GrepOptions,
    pathspec: &GrepPathspec,
    out: &mut impl Write,
) -> Result<bool> {
    let Some(index) = git_worktree::read_repository_index(git_dir, format)? else {
        return Ok(false);
    };
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let mut any = false;
    for entry in &index.entries {
        // Skip the higher merge stages; grep reports each path once.
        if (entry.flags >> 12) & 0x3 != 0 {
            continue;
        }
        if entry.mode == 0o160000 {
            // Gitlinks (submodules) carry no blob to search here.
            continue;
        }
        let path = &entry.path;
        if !pathspec.matches(path) {
            continue;
        }
        let content = if opts.cached {
            db.read_object(&entry.oid)?.body
        } else {
            let absolute = worktree_root.join(bytes_to_path(path));
            match fs::read(&absolute) {
                Ok(bytes) => bytes,
                Err(_) => continue,
            }
        };
        let display = pathspec.display(path);
        let matched = grep_buffer(&content, &display, None, matcher, opts, out)?;
        any = any || matched;
    }
    Ok(any)
}

/// Greps a tree-ish, recursing through subtrees.
#[allow(clippy::too_many_arguments)]
fn grep_tree_source(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    rev: &str,
    matcher: &GrepMatcher,
    opts: &GrepOptions,
    pathspec: &GrepPathspec,
    out: &mut impl Write,
) -> Result<bool> {
    let mut entries: Vec<(Vec<u8>, ObjectId)> = Vec::new();
    collect_tree_blobs(db, format, tree_oid, &mut Vec::new(), &mut entries)?;
    let mut any = false;
    for (path, oid) in entries {
        if !pathspec.matches(&path) {
            continue;
        }
        let display = pathspec.display(&path);
        let content = db.read_object(&oid)?.body;
        let matched = grep_buffer(&content, &display, Some(rev), matcher, opts, out)?;
        any = any || matched;
    }
    Ok(any)
}

/// Recursively gathers `(full_path, blob_oid)` pairs from a tree, in the same
/// lexical order Git emits (tree entries are already sorted by name).
fn collect_tree_blobs(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    prefix: &mut Vec<u8>,
    out: &mut Vec<(Vec<u8>, ObjectId)>,
) -> Result<()> {
    let object = db.read_object(tree_oid)?;
    if object.object_type != ObjectType::Tree {
        return Ok(());
    }
    let tree = Tree::parse(format, &object.body)?;
    for entry in &tree.entries {
        let object_type = tree_entry_object_type(entry.mode);
        let base_len = prefix.len();
        if !prefix.is_empty() {
            prefix.push(b'/');
        }
        prefix.extend_from_slice(&entry.name);
        match object_type {
            ObjectType::Tree => {
                collect_tree_blobs(db, format, &entry.oid, prefix, out)?;
            }
            ObjectType::Blob => {
                if entry.mode != 0o160000 {
                    out.push((prefix.clone(), entry.oid.clone()));
                }
            }
            _ => {}
        }
        prefix.truncate(base_len);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Per-file matching and output
// ---------------------------------------------------------------------------

/// Searches one file's `content`, writing matching output for `display_path`.
/// `rev` carries the `<rev>:` prefix when searching a tree-ish. Returns whether
/// the file produced a match.
fn grep_buffer(
    content: &[u8],
    display_path: &[u8],
    rev: Option<&str>,
    matcher: &GrepMatcher,
    opts: &GrepOptions,
    out: &mut impl Write,
) -> Result<bool> {
    let is_binary = !opts.text && buffer_is_binary(content);
    if is_binary && opts.ignore_binary {
        return Ok(false);
    }

    let show_filename = opts.show_filename.unwrap_or(true);
    // Under `-z`, the *field* separator (after the path / line number) becomes a
    // NUL, but matched lines are still terminated by `\n`. The `-l`/`-L` file
    // lists, by contrast, terminate each path with NUL.
    let field_sep: &[u8] = if opts.null_data { b"\0" } else { b":" };
    let list_term = if opts.null_data { b'\0' } else { b'\n' };

    // Input is always split into lines on '\n'.
    let mut match_count = 0usize;
    let mut matched_lines: Vec<(usize, &[u8])> = Vec::new();
    for (line_index, line) in split_lines(content, b'\n').enumerate() {
        if matcher.line_matches(line) != opts.invert {
            match_count += 1;
            matched_lines.push((line_index + 1, line));
        }
    }
    let any = match_count > 0;

    if opts.name_only_quiet {
        // -q: no output; caller uses the boolean for the exit status.
        return Ok(any);
    }

    if opts.count {
        if any {
            write_line_prefix(out, rev, display_path, show_filename, field_sep)?;
            out.write_all(match_count.to_string().as_bytes())?;
            out.write_all(b"\n")?;
        }
        return Ok(any);
    }

    if opts.files_with_matches {
        if any {
            write_path_line(out, rev, display_path, list_term)?;
        }
        return Ok(any);
    }

    if opts.files_without_match {
        if !any {
            write_path_line(out, rev, display_path, list_term)?;
        }
        return Ok(any);
    }

    if !any {
        return Ok(false);
    }

    // A binary file with matches reports a single summary line (unless -a).
    if is_binary {
        out.write_all(b"Binary file ")?;
        if let Some(rev) = rev {
            out.write_all(rev.as_bytes())?;
            out.write_all(b":")?;
        }
        out.write_all(display_path)?;
        out.write_all(b" matches\n")?;
        return Ok(true);
    }

    for (line_no, line) in matched_lines {
        if opts.only_matching {
            for span in matcher.match_spans(line) {
                write_line_prefix(out, rev, display_path, show_filename, field_sep)?;
                if opts.line_number {
                    out.write_all(line_no.to_string().as_bytes())?;
                    out.write_all(field_sep)?;
                }
                out.write_all(&line[span.0..span.1])?;
                out.write_all(b"\n")?;
            }
            continue;
        }
        write_line_prefix(out, rev, display_path, show_filename, field_sep)?;
        if opts.line_number {
            out.write_all(line_no.to_string().as_bytes())?;
            out.write_all(field_sep)?;
        }
        out.write_all(line)?;
        out.write_all(b"\n")?;
    }
    Ok(true)
}

/// Writes the `<rev>:<path><sep>` (or `<path><sep>`) prefix that precedes a
/// matched line or count, where `<sep>` is the field separator (`:` normally,
/// NUL under `-z`). A rev is always joined with `:` (matching Git). With `-h`
/// and no rev, nothing is written.
fn write_line_prefix(
    out: &mut impl Write,
    rev: Option<&str>,
    display_path: &[u8],
    show_filename: bool,
    field_sep: &[u8],
) -> Result<()> {
    if let Some(rev) = rev {
        out.write_all(rev.as_bytes())?;
        out.write_all(b":")?;
    }
    if show_filename || rev.is_some() {
        out.write_all(display_path)?;
        out.write_all(field_sep)?;
    }
    Ok(())
}

/// Writes a bare `<rev>:<path>` or `<path>` line (for `-l`/`-L`).
fn write_path_line(
    out: &mut impl Write,
    rev: Option<&str>,
    display_path: &[u8],
    line_sep: u8,
) -> Result<()> {
    if let Some(rev) = rev {
        out.write_all(rev.as_bytes())?;
        out.write_all(b":")?;
    }
    out.write_all(display_path)?;
    out.write_all(&[line_sep])?;
    Ok(())
}

/// Splits a buffer into lines on `sep`, dropping a single trailing separator so
/// that a final newline does not yield an empty trailing line.
fn split_lines(content: &[u8], sep: u8) -> impl Iterator<Item = &[u8]> {
    let trimmed = if content.last() == Some(&sep) {
        &content[..content.len() - 1]
    } else {
        content
    };
    // For an empty file there are no lines at all.
    SplitLines {
        rest: trimmed,
        sep,
        done: trimmed.is_empty() && content.is_empty(),
        empty_input: content.is_empty(),
    }
}

struct SplitLines<'a> {
    rest: &'a [u8],
    sep: u8,
    done: bool,
    empty_input: bool,
}

impl<'a> Iterator for SplitLines<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        if self.empty_input {
            self.done = true;
            return None;
        }
        match self.rest.iter().position(|byte| *byte == self.sep) {
            Some(pos) => {
                let line = &self.rest[..pos];
                self.rest = &self.rest[pos + 1..];
                Some(line)
            }
            None => {
                self.done = true;
                Some(self.rest)
            }
        }
    }
}

/// Git's binary-file heuristic: a NUL byte in the first 8000 bytes.
fn buffer_is_binary(content: &[u8]) -> bool {
    let window = content.len().min(8000);
    content[..window].contains(&0)
}

fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

// ---------------------------------------------------------------------------
// Pathspec limiting (with cwd-relative display)
// ---------------------------------------------------------------------------

/// A single pathspec entry, holding its normalised (repo-root-relative) form.
struct GrepPathFilter {
    original: String,
    normalized: Vec<u8>,
    matched: Cell<bool>,
}

/// Pathspec set plus the cwd prefix used to limit and display matches, mirroring
/// `git grep`'s behaviour of scoping a working-tree/index search to the current
/// directory and printing paths relative to it (unless `--full-name`).
struct GrepPathspec {
    prefix: Vec<u8>,
    cwd_depth: usize,
    full_name: bool,
    filters: Vec<GrepPathFilter>,
}

impl GrepPathspec {
    fn new(
        worktree_root: Option<&Path>,
        cwd: &Path,
        full_name: bool,
        pathspecs: &[String],
    ) -> Result<Self> {
        // The cwd prefix only applies when there is a worktree to be relative to.
        let prefix = if let Some(root) = worktree_root {
            let root = fs::canonicalize(root)?;
            let cwd = fs::canonicalize(cwd)?;
            match cwd.strip_prefix(&root) {
                Ok(rel) => rel.to_string_lossy().replace('\\', "/").into_bytes(),
                Err(_) => Vec::new(),
            }
        } else {
            Vec::new()
        };
        let cwd_depth = prefix
            .split(|byte| *byte == b'/')
            .filter(|component| !component.is_empty())
            .count();
        let mut filters = Vec::new();
        for spec in pathspecs {
            let normalized = normalize_grep_pathspec(&prefix, spec)?;
            filters.push(GrepPathFilter {
                original: spec.clone(),
                normalized,
                matched: Cell::new(false),
            });
        }
        Ok(Self {
            prefix,
            cwd_depth,
            full_name,
            filters,
        })
    }

    /// Whether `path` (repo-root-relative) is in scope.
    ///
    /// With explicit pathspecs the scope is exactly their union (each already
    /// resolved relative to the cwd, so `../a.txt` reaches outside the cwd).
    /// Without any pathspec the cwd acts as an implicit one, limiting the search
    /// to files at or below the current directory — `--full-name` changes only
    /// the displayed path, never this scope.
    fn matches(&self, path: &[u8]) -> bool {
        if self.filters.is_empty() {
            return self.prefix.is_empty() || path_under_prefix(path, &self.prefix);
        }
        let mut matched = false;
        for filter in &self.filters {
            if grep_pathspec_match(&filter.normalized, path) {
                filter.matched.set(true);
                matched = true;
            }
        }
        matched
    }

    /// Renders `path` for output: repo-root-relative under `--full-name`,
    /// otherwise relative to the current directory.
    fn display(&self, path: &[u8]) -> Vec<u8> {
        if self.full_name || self.prefix.is_empty() {
            return path.to_vec();
        }
        if let Some(rest) = strip_dir_prefix(path, &self.prefix) {
            return rest.to_vec();
        }
        let mut display = Vec::new();
        for _ in 0..self.cwd_depth {
            display.extend_from_slice(b"../");
        }
        display.extend_from_slice(path);
        display
    }

    fn report_unmatched(&self) -> Result<()> {
        let mut unmatched = false;
        for filter in &self.filters {
            if !filter.matched.get() {
                eprintln!(
                    "error: pathspec '{}' did not match any file(s) known to git",
                    filter.original
                );
                unmatched = true;
            }
        }
        if unmatched {
            eprintln!("Did you forget to 'git add'?");
            return Err(GitError::Exit(1));
        }
        Ok(())
    }
}

/// True if `path` equals `prefix` (a directory) or lives beneath it.
fn path_under_prefix(path: &[u8], prefix: &[u8]) -> bool {
    strip_dir_prefix(path, prefix).is_some()
}

/// Strips a leading `prefix/` from `path`, returning the remainder (non-empty).
fn strip_dir_prefix<'a>(path: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    if prefix.is_empty() {
        return Some(path);
    }
    let rest = path.strip_prefix(prefix)?;
    let rest = rest.strip_prefix(b"/")?;
    if rest.is_empty() { None } else { Some(rest) }
}

/// Resolves a CLI pathspec against the cwd `prefix`, collapsing `.`/`..`.
fn normalize_grep_pathspec(prefix: &[u8], arg: &str) -> Result<Vec<u8>> {
    let mut components: Vec<Vec<u8>> = prefix
        .split(|byte| *byte == b'/')
        .filter(|component| !component.is_empty())
        .map(Vec::from)
        .collect();
    for component in Path::new(arg).components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                components.pop().ok_or_else(|| {
                    GitError::InvalidPath(format!("{arg}: '{arg}' is outside repository"))
                })?;
            }
            std::path::Component::Normal(name) => {
                components.push(name.to_string_lossy().as_bytes().to_vec());
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                return Err(GitError::Unsupported(
                    "grep pathspecs currently support relative paths".into(),
                ));
            }
        }
    }
    Ok(components.join(&b'/'))
}

/// Git pathspec matching: exact path, directory prefix, or fnmatch glob (where
/// `*`/`?` may cross `/`, matching Git's wildmatch without `WM_PATHNAME`).
fn grep_pathspec_match(spec: &[u8], path: &[u8]) -> bool {
    if spec.is_empty() {
        return true;
    }
    if path == spec {
        return true;
    }
    // Directory prefix: `dir` matches `dir/...`.
    if let Some(rest) = path.strip_prefix(spec)
        && rest.first() == Some(&b'/')
    {
        return true;
    }
    if spec.iter().any(|b| matches!(b, b'*' | b'?' | b'[')) {
        return wildmatch(spec, path);
    }
    false
}

/// fnmatch-style glob where `*` and `?` match any byte including `/`.
fn wildmatch(pattern: &[u8], text: &[u8]) -> bool {
    fn rec(pattern: &[u8], text: &[u8]) -> bool {
        let mut pi = 0;
        let mut ti = 0;
        while pi < pattern.len() {
            match pattern[pi] {
                b'*' => {
                    // Collapse consecutive stars.
                    while pi < pattern.len() && pattern[pi] == b'*' {
                        pi += 1;
                    }
                    if pi == pattern.len() {
                        return true;
                    }
                    let mut k = ti;
                    loop {
                        if rec(&pattern[pi..], &text[k..]) {
                            return true;
                        }
                        if k >= text.len() {
                            return false;
                        }
                        k += 1;
                    }
                }
                b'?' => {
                    if ti >= text.len() {
                        return false;
                    }
                    pi += 1;
                    ti += 1;
                }
                b'[' => {
                    if ti >= text.len() {
                        return false;
                    }
                    match match_bracket(&pattern[pi..], text[ti]) {
                        BracketOutcome::Match(consumed) => {
                            pi += consumed;
                            ti += 1;
                        }
                        BracketOutcome::NoMatch => return false,
                        BracketOutcome::Malformed => {
                            // Treat `[` literally when the class is malformed.
                            if text[ti] != b'[' {
                                return false;
                            }
                            pi += 1;
                            ti += 1;
                        }
                    }
                }
                b'\\' if pi + 1 < pattern.len() => {
                    if ti >= text.len() || text[ti] != pattern[pi + 1] {
                        return false;
                    }
                    pi += 2;
                    ti += 1;
                }
                literal => {
                    if ti >= text.len() || text[ti] != literal {
                        return false;
                    }
                    pi += 1;
                    ti += 1;
                }
            }
        }
        ti == text.len()
    }
    rec(pattern, text)
}

/// Outcome of matching a glob bracket expression against one byte.
enum BracketOutcome {
    /// The byte matched; the class consumed `usize` pattern bytes.
    Match(usize),
    /// The byte did not match the (well-formed) class.
    NoMatch,
    /// The `[` opened no valid class; the caller should treat it literally.
    Malformed,
}

/// Matches `ch` against a bracket expression at the start of `pattern`
/// (`pattern[0] == '['`), reporting both the outcome and how many pattern bytes
/// the class spans (through the closing `]`).
fn match_bracket(pattern: &[u8], ch: u8) -> BracketOutcome {
    let mut i = 1;
    let negate = matches!(pattern.get(i), Some(b'!') | Some(b'^'));
    if negate {
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    while i < pattern.len() {
        let c = pattern[i];
        if c == b']' && !first {
            let hit = matched != negate;
            return if hit {
                BracketOutcome::Match(i + 1)
            } else {
                BracketOutcome::NoMatch
            };
        }
        first = false;
        // Range `a-b` (but a trailing `-` before `]` is a literal).
        if i + 2 < pattern.len() && pattern[i + 1] == b'-' && pattern[i + 2] != b']' {
            let lo = c;
            let hi = pattern[i + 2];
            if lo <= ch && ch <= hi {
                matched = true;
            }
            i += 3;
        } else {
            if c == ch {
                matched = true;
            }
            i += 1;
        }
    }
    BracketOutcome::Malformed
}

// ---------------------------------------------------------------------------
// Regular-expression engine (POSIX BRE/ERE subset) + fixed strings
// ---------------------------------------------------------------------------

/// A compiled set of patterns (OR-combined, as `git grep` does for multiple
/// `-e`). A line matches if any sub-pattern matches.
struct GrepMatcher {
    patterns: Vec<CompiledPattern>,
    line_regexp: bool,
}

enum CompiledPattern {
    Fixed { needle: Vec<u8>, ignore_case: bool },
    Regex(Regex),
}

impl GrepMatcher {
    fn compile(opts: &GrepOptions) -> Result<Self> {
        let mut patterns = Vec::new();
        for raw in &opts.patterns {
            let compiled = match opts.kind {
                PatternKind::Fixed => CompiledPattern::Fixed {
                    needle: raw.as_bytes().to_vec(),
                    ignore_case: opts.ignore_case,
                },
                PatternKind::Basic | PatternKind::Extended => {
                    let extended = opts.kind == PatternKind::Extended;
                    let regex = Regex::compile(raw, extended, opts.ignore_case, opts.word)?;
                    CompiledPattern::Regex(regex)
                }
            };
            patterns.push(compiled);
        }
        Ok(Self {
            patterns,
            line_regexp: opts.line_regexp,
        })
    }

    fn line_matches(&self, line: &[u8]) -> bool {
        self.patterns
            .iter()
            .any(|p| p.matches_line(line, self.line_regexp))
    }

    /// Byte spans of (non-overlapping, left-most) matches on `line`, used by `-o`.
    fn match_spans(&self, line: &[u8]) -> Vec<(usize, usize)> {
        let mut spans = Vec::new();
        let mut start = 0;
        while start <= line.len() {
            let mut best: Option<(usize, usize)> = None;
            for pattern in &self.patterns {
                if let Some((s, e)) = pattern.find_from(line, start) {
                    best = match best {
                        Some((bs, _)) if bs <= s => best,
                        _ => Some((s, e)),
                    };
                }
            }
            match best {
                Some((s, e)) => {
                    spans.push((s, e));
                    start = if e > s { e } else { e + 1 };
                }
                None => break,
            }
        }
        spans
    }
}

impl CompiledPattern {
    fn matches_line(&self, line: &[u8], line_regexp: bool) -> bool {
        match self {
            CompiledPattern::Fixed {
                needle,
                ignore_case,
            } => {
                if line_regexp {
                    return bytes_eq(line, needle, *ignore_case);
                }
                contains(line, needle, *ignore_case)
            }
            CompiledPattern::Regex(regex) => {
                if line_regexp {
                    return regex.matches_whole(line);
                }
                regex.find_from(line, 0).is_some()
            }
        }
    }

    fn find_from(&self, line: &[u8], from: usize) -> Option<(usize, usize)> {
        match self {
            CompiledPattern::Fixed {
                needle,
                ignore_case,
            } => find_substring(line, needle, *ignore_case, from).map(|s| (s, s + needle.len())),
            CompiledPattern::Regex(regex) => regex.find_from(line, from),
        }
    }
}

fn bytes_eq(a: &[u8], b: &[u8], ignore_case: bool) -> bool {
    if a.len() != b.len() {
        return false;
    }
    if ignore_case {
        a.iter()
            .zip(b)
            .all(|(x, y)| x.to_ascii_lowercase() == y.to_ascii_lowercase())
    } else {
        a == b
    }
}

fn contains(haystack: &[u8], needle: &[u8], ignore_case: bool) -> bool {
    find_substring(haystack, needle, ignore_case, 0).is_some()
}

fn find_substring(haystack: &[u8], needle: &[u8], ignore_case: bool, from: usize) -> Option<usize> {
    if needle.is_empty() {
        return Some(from.min(haystack.len()));
    }
    if from > haystack.len() || needle.len() > haystack.len() - from {
        return None;
    }
    for start in from..=haystack.len() - needle.len() {
        let window = &haystack[start..start + needle.len()];
        let hit = if ignore_case {
            window
                .iter()
                .zip(needle)
                .all(|(x, y)| x.to_ascii_lowercase() == y.to_ascii_lowercase())
        } else {
            window == needle
        };
        if hit {
            return Some(start);
        }
    }
    None
}

// --- Regex AST -------------------------------------------------------------

/// A node in the parsed regex tree.
#[derive(Debug, Clone)]
enum Node {
    /// Match a single literal byte.
    Literal(u8),
    /// `.` — any byte (newlines never occur inside a single line).
    AnyChar,
    /// A bracket expression `[...]`.
    Class { negate: bool, items: Vec<ClassItem> },
    /// Start anchor `^`.
    StartAnchor,
    /// End anchor `$`.
    EndAnchor,
    /// Word boundary (`\b`).
    WordBoundary,
    /// Non-word-boundary (`\B`).
    NonWordBoundary,
    /// Concatenation of nodes.
    Concat(Vec<Node>),
    /// Alternation of branches.
    Alt(Vec<Node>),
    /// Repetition with a min and optional max (None = unbounded).
    Repeat {
        node: Box<Node>,
        min: usize,
        max: Option<usize>,
        greedy: bool,
    },
    /// A grouped sub-expression.
    Group(Box<Node>),
    /// Matches the empty string.
    Empty,
}

#[derive(Debug, Clone)]
enum ClassItem {
    Single(u8),
    Range(u8, u8),
    Posix(PosixClass),
}

#[derive(Debug, Clone, Copy)]
enum PosixClass {
    Alpha,
    Digit,
    Alnum,
    Space,
    Upper,
    Lower,
    Punct,
    Blank,
    Xdigit,
    Cntrl,
    Print,
    Graph,
}

/// A compiled regex: its root node plus matching flags.
struct Regex {
    root: Node,
    ignore_case: bool,
}

impl Regex {
    fn compile(pattern: &str, extended: bool, ignore_case: bool, word: bool) -> Result<Self> {
        let bytes = pattern.as_bytes();
        let mut parser = RegexParser {
            bytes,
            pos: 0,
            extended,
        };
        let mut root = parser.parse_alternation()?;
        if parser.pos != bytes.len() {
            return Err(GitError::Command(format!(
                "invalid regular expression: {pattern}"
            )));
        }
        if word {
            // Wrap as \b(...)\b.
            root = Node::Concat(vec![
                Node::WordBoundary,
                Node::Group(Box::new(root)),
                Node::WordBoundary,
            ]);
        }
        Ok(Self { root, ignore_case })
    }

    fn find_from(&self, text: &[u8], from: usize) -> Option<(usize, usize)> {
        for start in from..=text.len() {
            if let Some(end) = match_node(&self.root, text, start, self.ignore_case) {
                return Some((start, end));
            }
        }
        None
    }

    fn matches_whole(&self, text: &[u8]) -> bool {
        // -x: the pattern must match the entire line.
        match_anchored_full(&self.root, text, self.ignore_case)
    }
}

/// Recursive-descent parser for the BRE/ERE subset.
struct RegexParser<'a> {
    bytes: &'a [u8],
    pos: usize,
    extended: bool,
}

impl RegexParser<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn parse_alternation(&mut self) -> Result<Node> {
        let mut branches = vec![self.parse_concat()?];
        loop {
            if self.at_alternation() {
                self.consume_alternation();
                branches.push(self.parse_concat()?);
            } else {
                break;
            }
        }
        if branches.len() == 1 {
            Ok(branches.remove(0))
        } else {
            Ok(Node::Alt(branches))
        }
    }

    fn at_alternation(&self) -> bool {
        match self.peek() {
            Some(b'|') if self.extended => true,
            Some(b'\\') if !self.extended => self.bytes.get(self.pos + 1) == Some(&b'|'),
            _ => false,
        }
    }

    fn consume_alternation(&mut self) {
        if self.extended {
            self.pos += 1;
        } else {
            self.pos += 2;
        }
    }

    fn at_group_close(&self) -> bool {
        match self.peek() {
            Some(b')') if self.extended => true,
            Some(b'\\') if !self.extended => self.bytes.get(self.pos + 1) == Some(&b')'),
            _ => false,
        }
    }

    fn parse_concat(&mut self) -> Result<Node> {
        let mut nodes = Vec::new();
        while let Some(byte) = self.peek() {
            if self.at_alternation() || self.at_group_close() {
                break;
            }
            // `$` is an end anchor only at the end of the pattern / branch.
            if byte == b'$' && self.is_end_anchor_position() {
                self.pos += 1;
                nodes.push(Node::EndAnchor);
                continue;
            }
            let atom = self.parse_atom(nodes.is_empty())?;
            let quantified = self.parse_quantifier(atom)?;
            nodes.push(quantified);
        }
        if nodes.is_empty() {
            Ok(Node::Empty)
        } else if nodes.len() == 1 {
            Ok(nodes.remove(0))
        } else {
            Ok(Node::Concat(nodes))
        }
    }

    /// `$` anchors at end-of-pattern, before `|`, or before a closing group.
    fn is_end_anchor_position(&self) -> bool {
        let next = self.pos + 1;
        if next >= self.bytes.len() {
            return true;
        }
        if self.extended {
            matches!(self.bytes.get(next), Some(b'|') | Some(b')'))
        } else {
            self.bytes.get(next) == Some(&b'\\')
                && matches!(self.bytes.get(next + 1), Some(b'|') | Some(b')'))
        }
    }

    fn parse_atom(&mut self, at_branch_start: bool) -> Result<Node> {
        let Some(byte) = self.peek() else {
            return Ok(Node::Empty);
        };
        match byte {
            b'^' if at_branch_start => {
                self.pos += 1;
                Ok(Node::StartAnchor)
            }
            b'.' => {
                self.pos += 1;
                Ok(Node::AnyChar)
            }
            b'[' => self.parse_class(),
            b'(' if self.extended => {
                self.pos += 1;
                let inner = self.parse_alternation()?;
                if self.peek() != Some(b')') {
                    return Err(GitError::Command("unbalanced ( in regex".into()));
                }
                self.pos += 1;
                Ok(Node::Group(Box::new(inner)))
            }
            b'\\' => self.parse_escape(),
            other => {
                self.pos += 1;
                Ok(Node::Literal(other))
            }
        }
    }

    fn parse_escape(&mut self) -> Result<Node> {
        // self.bytes[self.pos] == '\\'
        let Some(next) = self.bytes.get(self.pos + 1).copied() else {
            // Trailing backslash: treat as literal backslash.
            self.pos += 1;
            return Ok(Node::Literal(b'\\'));
        };
        if !self.extended {
            // BRE: `\(`, `\)`, `\|` handled elsewhere; `\{` is a quantifier.
            match next {
                b'(' => {
                    self.pos += 2;
                    let inner = self.parse_alternation()?;
                    if !self.at_group_close() {
                        return Err(GitError::Command("unbalanced \\( in regex".into()));
                    }
                    self.pos += 2; // consume `\)`
                    return Ok(Node::Group(Box::new(inner)));
                }
                _ => {}
            }
        }
        match next {
            b'b' => {
                self.pos += 2;
                Ok(Node::WordBoundary)
            }
            b'B' => {
                self.pos += 2;
                Ok(Node::NonWordBoundary)
            }
            b'w' => {
                self.pos += 2;
                Ok(Node::Class {
                    negate: false,
                    items: vec![ClassItem::Posix(PosixClass::Alnum), ClassItem::Single(b'_')],
                })
            }
            b'W' => {
                self.pos += 2;
                Ok(Node::Class {
                    negate: true,
                    items: vec![ClassItem::Posix(PosixClass::Alnum), ClassItem::Single(b'_')],
                })
            }
            b'd' => {
                self.pos += 2;
                Ok(Node::Class {
                    negate: false,
                    items: vec![ClassItem::Posix(PosixClass::Digit)],
                })
            }
            b's' => {
                self.pos += 2;
                Ok(Node::Class {
                    negate: false,
                    items: vec![ClassItem::Posix(PosixClass::Space)],
                })
            }
            b't' => {
                self.pos += 2;
                Ok(Node::Literal(b'\t'))
            }
            b'n' => {
                self.pos += 2;
                Ok(Node::Literal(b'\n'))
            }
            other => {
                self.pos += 2;
                Ok(Node::Literal(other))
            }
        }
    }

    fn parse_class(&mut self) -> Result<Node> {
        // self.bytes[self.pos] == '['
        let start = self.pos;
        self.pos += 1;
        let negate = matches!(self.peek(), Some(b'^'));
        if negate {
            self.pos += 1;
        }
        let mut items = Vec::new();
        let mut first = true;
        loop {
            let Some(byte) = self.peek() else {
                // Unterminated class: rewind and treat `[` as a literal.
                self.pos = start + 1;
                return Ok(Node::Literal(b'['));
            };
            if byte == b']' && !first {
                self.pos += 1;
                break;
            }
            first = false;
            if byte == b'[' && self.bytes.get(self.pos + 1) == Some(&b':') {
                if let Some(class) = self.parse_posix_class()? {
                    items.push(ClassItem::Posix(class));
                    continue;
                }
            }
            // Range?
            let lo = byte;
            if self.bytes.get(self.pos + 1) == Some(&b'-')
                && self.bytes.get(self.pos + 2).is_some_and(|c| *c != b']')
            {
                let hi = self.bytes[self.pos + 2];
                items.push(ClassItem::Range(lo, hi));
                self.pos += 3;
            } else {
                items.push(ClassItem::Single(lo));
                self.pos += 1;
            }
        }
        Ok(Node::Class { negate, items })
    }

    fn parse_posix_class(&mut self) -> Result<Option<PosixClass>> {
        // self.bytes[self.pos..] starts with "[:"
        let rest = &self.bytes[self.pos + 2..];
        let Some(end) = find_seq(rest, b":]") else {
            return Ok(None);
        };
        let name = &rest[..end];
        let class = match name {
            b"alpha" => PosixClass::Alpha,
            b"digit" => PosixClass::Digit,
            b"alnum" => PosixClass::Alnum,
            b"space" => PosixClass::Space,
            b"upper" => PosixClass::Upper,
            b"lower" => PosixClass::Lower,
            b"punct" => PosixClass::Punct,
            b"blank" => PosixClass::Blank,
            b"xdigit" => PosixClass::Xdigit,
            b"cntrl" => PosixClass::Cntrl,
            b"print" => PosixClass::Print,
            b"graph" => PosixClass::Graph,
            _ => return Ok(None),
        };
        self.pos += 2 + end + 2;
        Ok(Some(class))
    }

    fn parse_quantifier(&mut self, atom: Node) -> Result<Node> {
        // Anchors and boundaries cannot be quantified meaningfully; pass through.
        let Some(byte) = self.peek() else {
            return Ok(atom);
        };
        let (min, max, consumed) = match byte {
            b'*' => (0, None, 1),
            b'+' if self.extended => (1, None, 1),
            b'?' if self.extended => (0, Some(1), 1),
            b'{' if self.extended => match self.parse_bound(self.pos + 1, false)? {
                Some((min, max, end)) => (min, max, end - self.pos),
                None => return Ok(atom),
            },
            b'\\' if !self.extended => {
                let next = self.bytes.get(self.pos + 1).copied();
                match next {
                    Some(b'+') => (1, None, 2),
                    Some(b'?') => (0, Some(1), 2),
                    Some(b'{') => match self.parse_bound(self.pos + 2, true)? {
                        Some((min, max, end)) => (min, max, end - self.pos),
                        None => return Ok(atom),
                    },
                    _ => return Ok(atom),
                }
            }
            _ => return Ok(atom),
        };
        self.pos += consumed;
        Ok(Node::Repeat {
            node: Box::new(atom),
            min,
            max,
            greedy: true,
        })
    }

    /// Parses a `{m}`, `{m,}`, or `{m,n}` bound starting at `start`. For BRE the
    /// terminator is `\}`. Returns `(min, max, end_index_after_close)`.
    fn parse_bound(
        &self,
        start: usize,
        bre: bool,
    ) -> Result<Option<(usize, Option<usize>, usize)>> {
        let mut i = start;
        let mut min_digits = Vec::new();
        while let Some(c) = self.bytes.get(i).copied() {
            if c.is_ascii_digit() {
                min_digits.push(c);
                i += 1;
            } else {
                break;
            }
        }
        if min_digits.is_empty() {
            return Ok(None);
        }
        let min = parse_usize(&min_digits)?;
        let mut max = Some(min);
        if self.bytes.get(i) == Some(&b',') {
            i += 1;
            let mut max_digits = Vec::new();
            while let Some(c) = self.bytes.get(i).copied() {
                if c.is_ascii_digit() {
                    max_digits.push(c);
                    i += 1;
                } else {
                    break;
                }
            }
            max = if max_digits.is_empty() {
                None
            } else {
                Some(parse_usize(&max_digits)?)
            };
        }
        // Expect the closing brace.
        if bre {
            if self.bytes.get(i) == Some(&b'\\') && self.bytes.get(i + 1) == Some(&b'}') {
                return Ok(Some((min, max, i + 2)));
            }
        } else if self.bytes.get(i) == Some(&b'}') {
            return Ok(Some((min, max, i + 1)));
        }
        Ok(None)
    }
}

fn find_seq(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).find(|&i| &haystack[i..i + needle.len()] == needle)
}

fn parse_usize(digits: &[u8]) -> Result<usize> {
    std::str::from_utf8(digits)
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .ok_or_else(|| GitError::Command("invalid repetition count in regex".into()))
}

// --- Regex matcher ---------------------------------------------------------

/// Attempts to match `node` at `pos` in `text`, returning the end offset of the
/// shortest/greedy match continuation. This is a backtracking matcher using an
/// explicit continuation closure for sequencing.
fn match_node(root: &Node, text: &[u8], pos: usize, ignore_case: bool) -> Option<usize> {
    match_seq(root, text, pos, ignore_case, &|p| Some(p))
}

fn match_anchored_full(root: &Node, text: &[u8], ignore_case: bool) -> bool {
    match_seq(root, text, 0, ignore_case, &|p| {
        if p == text.len() { Some(p) } else { None }
    })
    .is_some()
}

/// Matches `node` at `pos`, then calls `cont` with the position after the match.
/// Returns the first end position for which the whole continuation succeeds.
fn match_seq(
    node: &Node,
    text: &[u8],
    pos: usize,
    ignore_case: bool,
    cont: &dyn Fn(usize) -> Option<usize>,
) -> Option<usize> {
    match node {
        Node::Empty => cont(pos),
        Node::Literal(byte) => {
            let c = text.get(pos)?;
            if byte_eq(*c, *byte, ignore_case) {
                cont(pos + 1)
            } else {
                None
            }
        }
        Node::AnyChar => {
            if pos < text.len() {
                cont(pos + 1)
            } else {
                None
            }
        }
        Node::Class { negate, items } => {
            let c = *text.get(pos)?;
            if class_matches(items, c, ignore_case) != *negate {
                cont(pos + 1)
            } else {
                None
            }
        }
        Node::StartAnchor => {
            if pos == 0 {
                cont(pos)
            } else {
                None
            }
        }
        Node::EndAnchor => {
            if pos == text.len() {
                cont(pos)
            } else {
                None
            }
        }
        Node::WordBoundary => {
            if is_word_boundary(text, pos) {
                cont(pos)
            } else {
                None
            }
        }
        Node::NonWordBoundary => {
            if !is_word_boundary(text, pos) {
                cont(pos)
            } else {
                None
            }
        }
        Node::Group(inner) => match_seq(inner, text, pos, ignore_case, cont),
        Node::Concat(nodes) => match_concat(nodes, text, pos, ignore_case, cont),
        Node::Alt(branches) => {
            for branch in branches {
                if let Some(end) = match_seq(branch, text, pos, ignore_case, cont) {
                    return Some(end);
                }
            }
            None
        }
        Node::Repeat {
            node,
            min,
            max,
            greedy,
        } => match_repeat(node, *min, *max, *greedy, text, pos, ignore_case, cont),
    }
}

fn match_concat(
    nodes: &[Node],
    text: &[u8],
    pos: usize,
    ignore_case: bool,
    cont: &dyn Fn(usize) -> Option<usize>,
) -> Option<usize> {
    match nodes.split_first() {
        None => cont(pos),
        Some((head, tail)) => match_seq(head, text, pos, ignore_case, &|p| {
            match_concat(tail, text, p, ignore_case, cont)
        }),
    }
}

#[allow(clippy::too_many_arguments)]
fn match_repeat(
    node: &Node,
    min: usize,
    max: Option<usize>,
    greedy: bool,
    text: &[u8],
    pos: usize,
    ignore_case: bool,
    cont: &dyn Fn(usize) -> Option<usize>,
) -> Option<usize> {
    // First satisfy the mandatory `min` repetitions.
    fn match_min(
        node: &Node,
        remaining: usize,
        text: &[u8],
        pos: usize,
        ignore_case: bool,
        after_min: &dyn Fn(usize) -> Option<usize>,
    ) -> Option<usize> {
        if remaining == 0 {
            return after_min(pos);
        }
        match_seq(node, text, pos, ignore_case, &|p| {
            // Guard against zero-width infinite recursion.
            if p == pos {
                return after_min(p);
            }
            match_min(node, remaining - 1, text, p, ignore_case, after_min)
        })
    }

    // Greedy optional tail: try to consume as many as possible, then backtrack.
    fn match_optional(
        node: &Node,
        remaining: Option<usize>,
        text: &[u8],
        pos: usize,
        ignore_case: bool,
        cont: &dyn Fn(usize) -> Option<usize>,
    ) -> Option<usize> {
        if remaining == Some(0) {
            return cont(pos);
        }
        // Greedy: attempt one more repetition first.
        let next_remaining = remaining.map(|r| r - 1);
        let more = match_seq(node, text, pos, ignore_case, &|p| {
            if p == pos {
                // Zero-width; avoid looping.
                None
            } else {
                match_optional(node, next_remaining, text, p, ignore_case, cont)
            }
        });
        if more.is_some() {
            return more;
        }
        cont(pos)
    }

    let max_optional = max.map(|m| m.saturating_sub(min));
    let _ = greedy;
    match_min(node, min, text, pos, ignore_case, &|p| {
        match_optional(node, max_optional, text, p, ignore_case, cont)
    })
}

fn byte_eq(a: u8, b: u8, ignore_case: bool) -> bool {
    if ignore_case {
        a.to_ascii_lowercase() == b.to_ascii_lowercase()
    } else {
        a == b
    }
}

fn class_matches(items: &[ClassItem], ch: u8, ignore_case: bool) -> bool {
    for item in items {
        match item {
            ClassItem::Single(b) => {
                if byte_eq(ch, *b, ignore_case) {
                    return true;
                }
            }
            ClassItem::Range(lo, hi) => {
                if (*lo..=*hi).contains(&ch) {
                    return true;
                }
                if ignore_case {
                    let lower = ch.to_ascii_lowercase();
                    let upper = ch.to_ascii_uppercase();
                    if (*lo..=*hi).contains(&lower) || (*lo..=*hi).contains(&upper) {
                        return true;
                    }
                }
            }
            ClassItem::Posix(class) => {
                if posix_matches(*class, ch) {
                    return true;
                }
            }
        }
    }
    false
}

fn posix_matches(class: PosixClass, ch: u8) -> bool {
    match class {
        PosixClass::Alpha => ch.is_ascii_alphabetic(),
        PosixClass::Digit => ch.is_ascii_digit(),
        PosixClass::Alnum => ch.is_ascii_alphanumeric(),
        PosixClass::Space => ch.is_ascii_whitespace() || ch == 0x0b,
        PosixClass::Upper => ch.is_ascii_uppercase(),
        PosixClass::Lower => ch.is_ascii_lowercase(),
        PosixClass::Punct => ch.is_ascii_punctuation(),
        PosixClass::Blank => ch == b' ' || ch == b'\t',
        PosixClass::Xdigit => ch.is_ascii_hexdigit(),
        PosixClass::Cntrl => ch.is_ascii_control(),
        PosixClass::Print => ch.is_ascii_graphic() || ch == b' ',
        PosixClass::Graph => ch.is_ascii_graphic(),
    }
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn is_word_boundary(text: &[u8], pos: usize) -> bool {
    let before = pos
        .checked_sub(1)
        .and_then(|i| text.get(i))
        .copied()
        .map(is_word_byte)
        .unwrap_or(false);
    let after = text.get(pos).copied().map(is_word_byte).unwrap_or(false);
    before != after
}

#[cfg(test)]
mod tests {
    use super::*;

    fn regex_match(pattern: &str, extended: bool, text: &str) -> bool {
        let re = Regex::compile(pattern, extended, false, false).expect("compile");
        re.find_from(text.as_bytes(), 0).is_some()
    }

    #[test]
    fn bre_plus_is_literal_but_escaped_is_repeat() {
        assert!(regex_match("a+", false, "a+a"));
        assert!(!regex_match("a+", false, "aaa"));
        assert!(regex_match(r"a\+", false, "aaa"));
    }

    #[test]
    fn ere_plus_and_alternation() {
        assert!(regex_match("a+", true, "aaa"));
        assert!(regex_match("foo|bar", true, "xbarx"));
        assert!(!regex_match("foo|bar", true, "xbazx"));
    }

    #[test]
    fn dot_and_anchors() {
        assert!(regex_match("a.c", false, "abc"));
        assert!(regex_match("^abc$", false, "abc"));
        assert!(!regex_match("^abc$", false, "xabc"));
    }

    #[test]
    fn character_classes_and_posix() {
        assert!(regex_match("[abc]x", false, "bx"));
        assert!(!regex_match("[^abc]x", false, "ax"));
        assert!(regex_match("[[:digit:]]+", true, "abc123"));
        assert!(regex_match("[a-f0-9]", false, "e"));
    }

    #[test]
    fn fixed_string_contains() {
        assert!(contains(b"hello world", b"o w", false));
        assert!(contains(b"Hello", b"hello", true));
        assert!(!contains(b"Hello", b"hello", false));
    }

    #[test]
    fn wildmatch_crosses_slash() {
        assert!(wildmatch(b"*.txt", b"sub/c.txt"));
        assert!(wildmatch(b"sub/*", b"sub/c.txt"));
        assert!(!wildmatch(b"*.rs", b"sub/c.txt"));
        assert!(wildmatch(b"a?c", b"abc"));
    }

    #[test]
    fn pathspec_dir_prefix_matches() {
        assert!(grep_pathspec_match(b"sub", b"sub/c.txt"));
        assert!(grep_pathspec_match(b"sub/c.txt", b"sub/c.txt"));
        assert!(!grep_pathspec_match(b"sub", b"submarine"));
    }

    #[test]
    fn split_lines_handles_trailing_newline() {
        let lines: Vec<&[u8]> = split_lines(b"a\nb\n", b'\n').collect();
        assert_eq!(lines, vec![b"a".as_slice(), b"b".as_slice()]);
        let no_nl: Vec<&[u8]> = split_lines(b"a\nb", b'\n').collect();
        assert_eq!(no_nl, vec![b"a".as_slice(), b"b".as_slice()]);
        let empty: Vec<&[u8]> = split_lines(b"", b'\n').collect();
        assert!(empty.is_empty());
    }

    #[test]
    fn repeat_bounds() {
        assert!(regex_match(r"a\{2,3\}", false, "aa"));
        assert!(!regex_match(r"a\{2,3\}", false, "a"));
        assert!(regex_match("a{2,3}", true, "aaa"));
    }
}
