//! `git merge-file`: a standalone three-way file merge.
//!
//! `git merge-file <current> <base> <other>` performs a line-level three-way
//! merge of `<current>` (ours) and `<other>` (theirs) using `<base>` as the
//! common ancestor, writing conflict markers around regions that both sides
//! changed differently. By default the result is written back into `<current>`
//! in place; with `-p`/`--stdout` it is printed to standard output and no file
//! is touched. The process exit status is the number of conflict regions
//! (capped at 127), `0` for a clean merge, `129` for a usage error and `255`
//! for an operational error (missing input, binary input).
//!
//! All merge computation lives in `sley_diff_merge::merge_file`; this wrapper
//! owns only argv/config resolution, filesystem or object-database inputs, and
//! delivery of the typed engine outcome.
#![allow(clippy::expect_used)]

use sley::plumbing::sley_diff_merge;
// Glob the crate root for shared plumbing; see commands::stash for rationale.
use crate::*;

/// Number of leading bytes inspected for a NUL when classifying input as binary.
/// Matches git's `FIRST_FEW_BYTES`.
const BINARY_SCAN_LEN: usize = 8000;

/// Default conflict marker length (`<<<<<<<` etc.), matching git's `ll_merge`.
const DEFAULT_MARKER_SIZE: usize = 7;

/// Largest exit status git's `merge-file` reports; conflict counts above this
/// are clamped here. Matches git's `ret > 127 ? 127 : ret`.
const MAX_CONFLICT_EXIT: i32 = 127;

/// Which conflict-marker layout to emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MergeStyle {
    /// `<<<<<<<` / `=======` / `>>>>>>>` only.
    Merge,
    /// diff3: also show the `|||||||` common-ancestor section.
    Diff3,
    /// zdiff3: like diff3, but lines common to both sides at the edges of a
    /// conflict are hoisted out of the markers.
    Zdiff3,
}

/// How to resolve regions that both sides changed (the `--ours`/`--theirs`/
/// `--union` favoring), or [`Favor::None`] to emit conflict markers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Favor {
    None,
    Ours,
    Theirs,
    Union,
}

/// Fully parsed `git merge-file` invocation.
#[derive(Debug, Clone)]
struct MergeFileOptions {
    to_stdout: bool,
    quiet: bool,
    object_id: bool,
    style: MergeStyle,
    style_explicit: bool,
    favor: Favor,
    diff_algorithm: sley_diff_merge::DiffAlgorithm,
    marker_size: usize,
    /// Labels supplied via `-L`, in order (`name1`, `orig`, `name2`).
    labels: Vec<String>,
    /// The three positional operands: current/ours, base/orig, other/theirs.
    operands: Vec<String>,
}

pub(crate) fn cmd_merge_file(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let options = match parse_merge_file_args(args)? {
        Some(options) => options,
        // A bare `--no-...`/help-style path that already produced its output.
        None => return Ok(()),
    };
    run_merge_file(cli_session, &options)
}

/// Parse the command line the way git's parse-options front-end does. Returns
/// `Ok(None)` only when nothing further should run (currently unused, reserved
/// for help output); usage/option errors are reported here and surface as
/// `GitError::Exit(129)`.
fn parse_merge_file_args(args: &[String]) -> Result<Option<MergeFileOptions>> {
    let mut to_stdout = false;
    let mut quiet = false;
    let mut object_id = false;
    let mut style = MergeStyle::Merge;
    let mut style_explicit = false;
    let mut favor = Favor::None;
    let mut diff_algorithm = sley_diff_merge::DiffAlgorithm::Myers;
    let mut marker_size = DEFAULT_MARKER_SIZE;
    let mut labels: Vec<String> = Vec::new();
    let mut operands: Vec<String> = Vec::new();

    let mut iter = args.iter();
    let mut no_more_options = false;
    while let Some(arg) = iter.next() {
        if no_more_options {
            operands.push(arg.clone());
            continue;
        }
        let arg = arg.as_str();
        match arg {
            "--" => {
                no_more_options = true;
            }
            "-p" | "--stdout" => to_stdout = true,
            "--no-stdout" => to_stdout = false,
            "-q" | "--quiet" => quiet = true,
            "--no-quiet" => quiet = false,
            "--object-id" => object_id = true,
            "--no-object-id" => object_id = false,
            "--diff3" => {
                style = MergeStyle::Diff3;
                style_explicit = true;
            }
            "--zdiff3" => {
                style = MergeStyle::Zdiff3;
                style_explicit = true;
            }
            "--no-diff3" | "--no-zdiff3" => {
                style = MergeStyle::Merge;
                style_explicit = true;
            }
            "--ours" => favor = Favor::Ours,
            "--theirs" => favor = Favor::Theirs,
            "--union" => favor = Favor::Union,
            "--no-ours" | "--no-theirs" | "--no-union" => favor = Favor::None,
            "-L" => {
                let Some(value) = iter.next() else {
                    return merge_file_switch_requires_value("L");
                };
                check_label_capacity(&labels)?;
                labels.push(value.clone());
            }
            "--marker-size" => {
                let Some(value) = iter.next() else {
                    return merge_file_option_requires_value("marker-size");
                };
                marker_size = parse_marker_size(value)?;
            }
            "--diff-algorithm" => {
                let Some(value) = iter.next() else {
                    return merge_file_option_requires_value("diff-algorithm");
                };
                diff_algorithm = parse_diff_algorithm(value)?;
            }
            _ => {
                if let Some(value) = arg.strip_prefix("-L") {
                    // Glued short form `-Lname`.
                    check_label_capacity(&labels)?;
                    labels.push(value.to_string());
                } else if let Some(value) = arg.strip_prefix("--marker-size=") {
                    marker_size = parse_marker_size(value)?;
                } else if let Some(value) = arg.strip_prefix("--diff-algorithm=") {
                    diff_algorithm = parse_diff_algorithm(value)?;
                } else if arg == "-" || !arg.starts_with('-') {
                    operands.push(arg.to_string());
                } else {
                    return merge_file_unknown_option(arg);
                }
            }
        }
    }

    if operands.len() != 3 {
        print_merge_file_usage();
        return Err(GitError::Exit(129));
    }

    Ok(Some(MergeFileOptions {
        to_stdout,
        quiet,
        object_id,
        style,
        style_explicit,
        favor,
        diff_algorithm,
        marker_size,
        labels,
        operands,
    }))
}

/// Verify there is room for one more `-L` label, rejecting a fourth one exactly
/// as git does (`error: too many labels on the command line`).
fn check_label_capacity(labels: &[String]) -> Result<()> {
    if labels.len() >= 3 {
        // git prints only this line (no usage block) for a fourth label.
        eprintln!("error: too many labels on the command line");
        return Err(GitError::Exit(129));
    }
    Ok(())
}

/// Parse a `--marker-size` value (a positive integer, optionally with a k/m/g
/// suffix, matching git's `git_parse_ssize_t`). Marker size 0 is rejected.
fn parse_marker_size(value: &str) -> Result<usize> {
    let parsed = parse_size_with_suffix(value).filter(|n| *n > 0);
    match parsed {
        Some(size) => Ok(size),
        None => {
            // A bad option *value* prints only the diagnostic, no usage block.
            eprintln!(
                "error: option `marker-size' expects an integer value with an optional k/m/g suffix"
            );
            Err(GitError::Exit(129))
        }
    }
}

/// Parse an unsigned integer with an optional binary k/m/g suffix, returning
/// `None` for anything malformed or overflowing.
fn parse_size_with_suffix(value: &str) -> Option<usize> {
    let bytes = value.as_bytes();
    let (digits, multiplier) = match bytes.last() {
        Some(b'k') | Some(b'K') => (&value[..value.len() - 1], 1024usize),
        Some(b'm') | Some(b'M') => (&value[..value.len() - 1], 1024 * 1024),
        Some(b'g') | Some(b'G') => (&value[..value.len() - 1], 1024 * 1024 * 1024),
        _ => (value, 1usize),
    };
    if digits.is_empty() {
        return None;
    }
    let base: usize = digits.parse().ok()?;
    base.checked_mul(multiplier)
}

/// Parse a `--diff-algorithm` name into the engine's typed algorithm selection.
fn parse_diff_algorithm(value: &str) -> Result<sley_diff_merge::DiffAlgorithm> {
    match value {
        "myers" => Ok(sley_diff_merge::DiffAlgorithm::Myers),
        "minimal" => Ok(sley_diff_merge::DiffAlgorithm::Minimal),
        "patience" => Ok(sley_diff_merge::DiffAlgorithm::Patience),
        "histogram" => Ok(sley_diff_merge::DiffAlgorithm::Histogram),
        _ => {
            // A bad option *value* prints only the diagnostic, no usage block.
            eprintln!(
                "error: option diff-algorithm accepts \"myers\", \"minimal\", \"patience\" and \"histogram\""
            );
            Err(GitError::Exit(129))
        }
    }
}

fn merge_file_unknown_option(option: &str) -> Result<Option<MergeFileOptions>> {
    if let Some(long) = option.strip_prefix("--") {
        eprintln!("error: unknown option `{long}'");
    } else if let Some(short) = option.strip_prefix('-') {
        let switch = short.chars().next().unwrap_or('-');
        eprintln!("error: unknown switch `{switch}'");
    } else {
        eprintln!("error: unknown option `{option}'");
    }
    print_merge_file_usage();
    Err(GitError::Exit(129))
}

fn merge_file_option_requires_value(option: &str) -> Result<Option<MergeFileOptions>> {
    // A missing option value prints only the diagnostic, no usage block.
    eprintln!("error: option `{option}' requires a value");
    Err(GitError::Exit(129))
}

fn merge_file_switch_requires_value(short: &str) -> Result<Option<MergeFileOptions>> {
    // A missing switch value prints only the diagnostic, no usage block.
    eprintln!("error: switch `{short}' requires a value");
    Err(GitError::Exit(129))
}

/// Print git's exact `merge-file` usage block to stderr. Built from adjacent
/// string literals (not `\`-continuations) so the leading indentation on every
/// option line is preserved byte-for-byte.
fn print_merge_file_usage() {
    eprint!(
        "usage: git merge-file [<options>] [-L <name1> [-L <orig> [-L <name2>]]] <file1> <orig-file> <file2>\n\
         \n\
         \x20   -p, --[no-]stdout     send results to standard output\n\
         \x20   --[no-]object-id      use object IDs instead of filenames\n\
         \x20   --[no-]diff3          use a diff3 based merge\n\
         \x20   --[no-]zdiff3         use a zealous diff3 based merge\n\
         \x20   --[no-]ours           for conflicts, use our version\n\
         \x20   --[no-]theirs         for conflicts, use their version\n\
         \x20   --[no-]union          for conflicts, use a union version\n\
         \x20   --diff-algorithm <algorithm>\n\
         \x20                         choose a diff algorithm\n\
         \x20   --[no-]marker-size <n>\n\
         \x20                         for conflicts, use this marker size\n\
         \x20   -q, --[no-]quiet      do not warn about conflicts\n\
         \x20   -L <name>             set labels for file1/orig-file/file2\n\
         \n"
    );
}

/// The three input blobs, with the display name git uses for each in messages
/// and default conflict labels.
struct MergeInputs {
    ours: Vec<u8>,
    base: Vec<u8>,
    theirs: Vec<u8>,
    ours_name: String,
    base_name: String,
    theirs_name: String,
}

fn run_merge_file(
    cli_session: &crate::session::CliSession,
    options: &MergeFileOptions,
) -> Result<()> {
    let mut options = options.clone();
    if !options.style_explicit {
        options.style = configured_merge_style(cli_session)?;
    }
    let inputs = if options.object_id {
        read_object_id_inputs(cli_session, &options.operands)?
    } else {
        read_file_inputs(&options.operands)?
    };

    // git refuses to merge if any input is binary, checking ours, then base,
    // then theirs, and naming the first binary one (always by path/oid, never by
    // a -L label). With -q the diagnostic is suppressed but the status stands.
    for (bytes, name) in [
        (&inputs.ours, &inputs.ours_name),
        (&inputs.base, &inputs.base_name),
        (&inputs.theirs, &inputs.theirs_name),
    ] {
        if is_binary(bytes) {
            if !options.quiet {
                eprintln!("error: Cannot merge binary files: {name}");
            }
            return Err(GitError::Exit(255));
        }
    }

    let (ours_label, base_label, theirs_label) = resolve_labels(&options, &inputs);

    let merged = merge_three_way(
        &inputs.base,
        &inputs.ours,
        &inputs.theirs,
        &options,
        &ours_label,
        &base_label,
        &theirs_label,
    );

    emit_result(cli_session, &options, &inputs, &merged.content)?;

    if merged.conflicts == 0 {
        Ok(())
    } else {
        Err(GitError::Exit(
            i32::try_from(merged.conflicts)
                .unwrap_or(MAX_CONFLICT_EXIT)
                .min(MAX_CONFLICT_EXIT),
        ))
    }
}

fn configured_merge_style(cli_session: &crate::session::CliSession) -> Result<MergeStyle> {
    let config = if let Ok(git_dir) = cli_session.git_dir() {
        commands::remote::read_effective_repo_config(&git_dir, cli_session.cwd())?
    } else {
        let context = sley_config::ConfigIncludeContext::new(None, None);
        let mut config = sley_config::load_pre_dispatch_config(None, &context)?;
        let parameters = injected_config_parameters()?;
        sley_config::append_injected_config_sections_with_includes(
            &mut config,
            &parameters,
            &context,
            cli_session.cwd(),
        )?;
        config
    };
    Ok(match config.get("merge", None, "conflictstyle") {
        Some(value) if value.eq_ignore_ascii_case("diff3") => MergeStyle::Diff3,
        Some(value) if value.eq_ignore_ascii_case("zdiff3") => MergeStyle::Zdiff3,
        _ => MergeStyle::Merge,
    })
}

/// Pick the conflict-marker labels: an explicit `-L` wins, otherwise the input's
/// own display name (path or object id). Note the second `-L` names the *base*
/// (the diff3 middle section), matching git, so a caller passing two labels sets
/// ours and base, leaving theirs to fall back to its filename.
fn resolve_labels(options: &MergeFileOptions, inputs: &MergeInputs) -> (String, String, String) {
    let ours = options
        .labels
        .first()
        .cloned()
        .unwrap_or_else(|| inputs.ours_name.clone());
    let base = options
        .labels
        .get(1)
        .cloned()
        .unwrap_or_else(|| inputs.base_name.clone());
    let theirs = options
        .labels
        .get(2)
        .cloned()
        .unwrap_or_else(|| inputs.theirs_name.clone());
    (ours, base, theirs)
}

/// Read the three operands as filesystem paths. A path that cannot be stat'd is
/// the fatal `Could not stat` error (exit 255) git reports.
fn read_file_inputs(operands: &[String]) -> Result<MergeInputs> {
    let ours = read_merge_input_file(&operands[0])?;
    let base = read_merge_input_file(&operands[1])?;
    let theirs = read_merge_input_file(&operands[2])?;
    Ok(MergeInputs {
        ours,
        base,
        theirs,
        ours_name: operands[0].clone(),
        base_name: operands[1].clone(),
        theirs_name: operands[2].clone(),
    })
}

fn read_merge_input_file(path: &str) -> Result<Vec<u8>> {
    match fs::read(path) {
        Ok(bytes) => Ok(bytes),
        Err(err) => {
            eprintln!("error: Could not stat {path}: {}", stat_error_text(&err));
            Err(GitError::Exit(255))
        }
    }
}

/// git formats the stat failure with the C library's `strerror`. Reproduce the
/// common cases so the message matches; fall back to the OS string otherwise.
fn stat_error_text(err: &std::io::Error) -> String {
    match err.kind() {
        std::io::ErrorKind::NotFound => "No such file or directory".to_string(),
        std::io::ErrorKind::PermissionDenied => "Permission denied".to_string(),
        _ => err.to_string(),
    }
}

/// Read the three operands as object ids (full or abbreviated), reusing the
/// object database or worktree index to resolve and load each blob. Output
/// labels preserve the operand spelling, as git does in `--object-id` mode.
fn read_object_id_inputs(
    cli_session: &crate::session::CliSession,
    operands: &[String],
) -> Result<MergeInputs> {
    let git_dir = cli_session.git_dir()?;
    let common_git_dir = cli_session.common_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let db = FileObjectDatabase::from_git_dir(&common_git_dir, format);
    let index = sley_worktree::read_repository_index(&git_dir, format)?;

    let (ours, ours_name) = read_object_id_blob(&db, format, index.as_ref(), &operands[0])?;
    let (base, base_name) = read_object_id_blob(&db, format, index.as_ref(), &operands[1])?;
    let (theirs, theirs_name) = read_object_id_blob(&db, format, index.as_ref(), &operands[2])?;
    Ok(MergeInputs {
        ours,
        base,
        theirs,
        ours_name,
        base_name,
        theirs_name,
    })
}

/// Resolve one object-id operand to `(blob bytes, marker label)`. Anything that
/// fails to resolve to a unique blob is the usage-style error (exit 129) git
/// reports in `--object-id` mode.
fn read_object_id_blob(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    index: Option<&Index>,
    spec: &str,
) -> Result<(Vec<u8>, String)> {
    let oid = resolve_object_id(db, format, index, spec)?;
    if oid == ObjectId::empty_blob(format) {
        return Ok((Vec::new(), spec.into()));
    }
    match db.read_object(&oid) {
        Ok(object) if object.object_type == ObjectType::Blob => {
            Ok((object.body.clone(), spec.into()))
        }
        _ => {
            print_merge_file_usage();
            Err(GitError::Exit(129))
        }
    }
}

fn resolve_object_id(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    index: Option<&Index>,
    spec: &str,
) -> Result<ObjectId> {
    if let Some(path) = spec.strip_prefix(':')
        && !path.is_empty()
        && let Some(index) = index
        && let Some(entry) = index.entries.iter().find(|entry| {
            entry.stage() == sley_index::Stage::Normal && entry.path.as_bytes() == path.as_bytes()
        })
    {
        return Ok(entry.oid);
    }
    if let Ok(oid) = ObjectId::from_hex(format, spec) {
        return Ok(oid);
    }
    match db.resolve_prefix(spec) {
        Ok(ObjectPrefixResolution::Unique(oid)) => Ok(oid),
        _ => {
            print_merge_file_usage();
            Err(GitError::Exit(129))
        }
    }
}

/// Classify a blob as binary the way git does: a NUL byte anywhere in the first
/// [`BINARY_SCAN_LEN`] bytes.
fn is_binary(bytes: &[u8]) -> bool {
    let scan = &bytes[..bytes.len().min(BINARY_SCAN_LEN)];
    scan.contains(&0)
}

/// Run the three-way merge, honouring style, favoring and marker size.
fn merge_three_way(
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    options: &MergeFileOptions,
    ours_label: &str,
    base_label: &str,
    theirs_label: &str,
) -> sley_diff_merge::MergeFileOutcome {
    sley_diff_merge::merge_file(
        base,
        ours,
        theirs,
        &sley_diff_merge::MergeFileOptions {
            ours_label,
            base_label,
            theirs_label,
            style: match options.style {
                MergeStyle::Merge => sley_diff_merge::ConflictStyle::Merge,
                MergeStyle::Diff3 => sley_diff_merge::ConflictStyle::Diff3,
                MergeStyle::Zdiff3 => sley_diff_merge::ConflictStyle::ZDiff3,
            },
            favor: match options.favor {
                Favor::None => sley_diff_merge::MergeFavor::None,
                Favor::Ours => sley_diff_merge::MergeFavor::Ours,
                Favor::Theirs => sley_diff_merge::MergeFavor::Theirs,
                Favor::Union => sley_diff_merge::MergeFavor::Union,
            },
            marker_size: options.marker_size,
            algorithm: options.diff_algorithm,
        },
    )
}

/// Deliver the merged blob: to stdout with `-p`, to a freshly written object
/// (printing its id) in `--object-id` mode, or back into the current file.
fn emit_result(
    cli_session: &crate::session::CliSession,
    options: &MergeFileOptions,
    inputs: &MergeInputs,
    content: &[u8],
) -> Result<()> {
    if options.to_stdout {
        let stdout = io::stdout();
        let mut handle = stdout.lock();
        handle.write_all(content)?;
        return Ok(());
    }
    if options.object_id {
        let git_dir = cli_session.git_dir()?;
        let common_git_dir = cli_session.common_git_dir(&git_dir)?;
        let format = repository_object_format(&common_git_dir)?;
        let db = FileObjectDatabase::from_git_dir(&common_git_dir, format);
        let oid = db.write_object(EncodedObject::new(ObjectType::Blob, content.to_vec()))?;
        println!("{oid}");
        return Ok(());
    }
    // Default: overwrite the current file in place.
    fs::write(&inputs.ours_name, content)?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn opts(operands: &[&str]) -> MergeFileOptions {
        MergeFileOptions {
            to_stdout: true,
            quiet: false,
            object_id: false,
            style: MergeStyle::Merge,
            style_explicit: false,
            favor: Favor::None,
            diff_algorithm: sley_diff_merge::DiffAlgorithm::Myers,
            marker_size: DEFAULT_MARKER_SIZE,
            labels: Vec::new(),
            operands: operands.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    fn merge(
        base: &[u8],
        ours: &[u8],
        theirs: &[u8],
        options: &MergeFileOptions,
    ) -> sley_diff_merge::MergeFileOutcome {
        merge_three_way(base, ours, theirs, options, "a", "o", "b")
    }

    #[test]
    fn clean_merge_far_apart_changes() {
        let base = b"l1\nl2\nl3\nl4\nl5\n";
        let ours = b"OURS\nl2\nl3\nl4\nl5\n";
        let theirs = b"l1\nl2\nl3\nl4\nTHEIRS\n";
        let result = merge(base, ours, theirs, &opts(&["a", "o", "b"]));
        assert_eq!(result.conflicts, 0);
        assert_eq!(result.content, b"OURS\nl2\nl3\nl4\nTHEIRS\n");
    }

    #[test]
    fn single_conflict_uses_labels_and_counts_one() {
        let base = b"l1\nl2\nl3\n";
        let ours = b"l1\nOURS\nl3\n";
        let theirs = b"l1\nTHEIRS\nl3\n";
        let result = merge(base, ours, theirs, &opts(&["a", "o", "b"]));
        assert_eq!(result.conflicts, 1);
        assert_eq!(
            result.content,
            b"l1\n<<<<<<< a\nOURS\n=======\nTHEIRS\n>>>>>>> b\nl3\n".to_vec()
        );
    }

    #[test]
    fn favor_ours_theirs_union_resolve_without_markers() {
        let base = b"l1\nl2\nl3\n";
        let ours = b"l1\nOURS\nl3\n";
        let theirs = b"l1\nTHEIRS\nl3\n";

        let mut o = opts(&["a", "o", "b"]);
        o.favor = Favor::Ours;
        let r = merge(base, ours, theirs, &o);
        // A favoring mode resolves the region, so no conflicts are reported.
        assert_eq!(r.conflicts, 0);
        assert_eq!(r.content, b"l1\nOURS\nl3\n".to_vec());

        o.favor = Favor::Theirs;
        let r = merge(base, ours, theirs, &o);
        assert_eq!(r.content, b"l1\nTHEIRS\nl3\n".to_vec());

        o.favor = Favor::Union;
        let r = merge(base, ours, theirs, &o);
        assert_eq!(r.content, b"l1\nOURS\nTHEIRS\nl3\n".to_vec());
    }

    #[test]
    fn diff3_includes_base_section() {
        let base = b"a\nb\nc\nd\ne\n";
        let ours = b"a\nb\nX\nd\ne\n";
        let theirs = b"a\nb\nY\nd\ne\n";
        let mut o = opts(&["a", "o", "b"]);
        o.style = MergeStyle::Diff3;
        let r = merge(base, ours, theirs, &o);
        assert_eq!(r.conflicts, 1);
        assert_eq!(
            r.content,
            b"a\nb\n<<<<<<< a\nX\n||||||| o\nc\n=======\nY\n>>>>>>> b\nd\ne\n".to_vec()
        );
    }

    #[test]
    fn marker_size_changes_marker_length() {
        let base = b"l1\nl2\nl3\n";
        let ours = b"l1\nOURS\nl3\n";
        let theirs = b"l1\nTHEIRS\nl3\n";
        let mut o = opts(&["a", "o", "b"]);
        o.marker_size = 4;
        let r = merge(base, ours, theirs, &o);
        assert_eq!(
            r.content,
            b"l1\n<<<< a\nOURS\n====\nTHEIRS\n>>>> b\nl3\n".to_vec()
        );
    }

    #[test]
    fn no_newline_at_eof_is_preserved() {
        let base = b"l1\nl2\n";
        let ours = b"l1\nOURS"; // no trailing newline
        let theirs = b"l1\nTHEIRS\n";
        let r = merge(base, ours, theirs, &opts(&["a", "o", "b"]));
        assert_eq!(r.conflicts, 1);
        assert_eq!(
            r.content,
            b"l1\n<<<<<<< a\nOURS\n=======\nTHEIRS\n>>>>>>> b\n".to_vec()
        );
    }

    #[test]
    fn same_change_on_both_sides_is_clean() {
        let base = b"a\nb\nc\n";
        let ours = b"a\nSAME\nc\n";
        let theirs = b"a\nSAME\nc\n";
        let r = merge(base, ours, theirs, &opts(&["a", "o", "b"]));
        assert_eq!(r.conflicts, 0);
        assert_eq!(r.content, b"a\nSAME\nc\n".to_vec());
    }

    #[test]
    fn two_separated_conflicts_count_two() {
        let base = b"A\nB\nC\nD\nE\nF\nG\n";
        let ours = b"A_OURS\nB\nC\nD\nE\nF\nG_OURS\n";
        let theirs = b"A_THEIRS\nB\nC\nD\nE\nF\nG_THEIRS\n";
        let r = merge(base, ours, theirs, &opts(&["a", "o", "b"]));
        assert_eq!(r.conflicts, 2);
    }

    #[test]
    fn is_binary_scans_first_8000_bytes() {
        let mut early = vec![b'x'; 10];
        early.push(0);
        assert!(is_binary(&early));

        let mut late = vec![b'x'; BINARY_SCAN_LEN];
        late.push(0);
        assert!(!is_binary(&late));
    }

    #[test]
    fn marker_size_parsing_accepts_suffixes_and_rejects_junk() {
        assert_eq!(parse_size_with_suffix("7"), Some(7));
        assert_eq!(parse_size_with_suffix("1k"), Some(1024));
        assert_eq!(parse_size_with_suffix("2K"), Some(2048));
        assert_eq!(parse_size_with_suffix(""), None);
        assert_eq!(parse_size_with_suffix("abc"), None);
        assert_eq!(parse_size_with_suffix("12x"), None);
    }

    #[test]
    fn parse_rejects_wrong_operand_count() {
        let two = vec!["a".to_string(), "b".to_string()];
        assert!(matches!(
            parse_merge_file_args(&two),
            Err(GitError::Exit(129))
        ));
        let four = vec![
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "d".to_string(),
        ];
        assert!(matches!(
            parse_merge_file_args(&four),
            Err(GitError::Exit(129))
        ));
    }

    #[test]
    fn parse_collects_flags_and_labels() {
        let args = vec![
            "-p".to_string(),
            "--diff3".to_string(),
            "-L".to_string(),
            "mine".to_string(),
            "-Lorig".to_string(),
            "--marker-size=9".to_string(),
            "cur".to_string(),
            "base".to_string(),
            "other".to_string(),
        ];
        let parsed = parse_merge_file_args(&args)
            .expect("parse ok")
            .expect("options present");
        assert!(parsed.to_stdout);
        assert_eq!(parsed.style, MergeStyle::Diff3);
        assert_eq!(parsed.marker_size, 9);
        assert_eq!(parsed.labels, vec!["mine".to_string(), "orig".to_string()]);
        assert_eq!(
            parsed.operands,
            vec!["cur".to_string(), "base".to_string(), "other".to_string()]
        );
    }

    #[test]
    fn parse_rejects_too_many_labels() {
        let args = vec![
            "-L".to_string(),
            "1".to_string(),
            "-L".to_string(),
            "2".to_string(),
            "-L".to_string(),
            "3".to_string(),
            "-L".to_string(),
            "4".to_string(),
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
        ];
        assert!(matches!(
            parse_merge_file_args(&args),
            Err(GitError::Exit(129))
        ));
    }

    #[test]
    fn double_dash_treats_rest_as_operands() {
        let args = vec![
            "--".to_string(),
            "-p".to_string(),
            "base".to_string(),
            "other".to_string(),
        ];
        let parsed = parse_merge_file_args(&args)
            .expect("parse ok")
            .expect("options present");
        assert!(!parsed.to_stdout);
        assert_eq!(
            parsed.operands,
            vec!["-p".to_string(), "base".to_string(), "other".to_string()]
        );
    }
}
