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
//! The merge engine reuses `sley_diff_merge`: the common path (default conflict
//! style, default marker size, no conflict-resolution favoring) is produced by
//! [`sley_diff_merge::merge_blobs`] directly. The extended flags — `--ours`,
//! `--theirs`, `--union`, `--diff3`/`--zdiff3` and `--marker-size` — are served
//! by a region walk built from the very same lower-level primitives
//! `merge_blobs` is built on (`split_lines` + `myers_diff_lines`), so conflict
//! detection and region boundaries stay consistent across every flag.
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
#[derive(Debug)]
struct MergeFileOptions {
    to_stdout: bool,
    quiet: bool,
    object_id: bool,
    style: MergeStyle,
    favor: Favor,
    marker_size: usize,
    /// Labels supplied via `-L`, in order (`name1`, `orig`, `name2`).
    labels: Vec<String>,
    /// The three positional operands: current/ours, base/orig, other/theirs.
    operands: Vec<String>,
}

pub(crate) fn cmd_merge_file(args: &[String]) -> Result<()> {
    let options = match parse_merge_file_args(args)? {
        Some(options) => options,
        // A bare `--no-...`/help-style path that already produced its output.
        None => return Ok(()),
    };
    run_merge_file(&options)
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
    let mut favor = Favor::None;
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
            "--diff3" => style = MergeStyle::Diff3,
            "--zdiff3" => style = MergeStyle::Zdiff3,
            "--no-diff3" | "--no-zdiff3" => style = MergeStyle::Merge,
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
                validate_diff_algorithm(value)?;
            }
            _ => {
                if let Some(value) = arg.strip_prefix("-L") {
                    // Glued short form `-Lname`.
                    check_label_capacity(&labels)?;
                    labels.push(value.to_string());
                } else if let Some(value) = arg.strip_prefix("--marker-size=") {
                    marker_size = parse_marker_size(value)?;
                } else if let Some(value) = arg.strip_prefix("--diff-algorithm=") {
                    validate_diff_algorithm(value)?;
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
        favor,
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

/// Validate a `--diff-algorithm` name. sley_diff_merge always merges with Myers,
/// so the choice does not change behaviour here, but an unknown name is an error
/// exactly as in git.
fn validate_diff_algorithm(value: &str) -> Result<()> {
    match value {
        "myers" | "minimal" | "patience" | "histogram" => Ok(()),
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

fn run_merge_file(options: &MergeFileOptions) -> Result<()> {
    let inputs = if options.object_id {
        read_object_id_inputs(&options.operands)?
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

    let (ours_label, base_label, theirs_label) = resolve_labels(options, &inputs);

    let merged = merge_three_way(
        &inputs.base,
        &inputs.ours,
        &inputs.theirs,
        options,
        &ours_label,
        &base_label,
        &theirs_label,
    );

    emit_result(options, &inputs, &merged.content)?;

    if merged.conflicts == 0 {
        Ok(())
    } else {
        Err(GitError::Exit(merged.conflicts.min(MAX_CONFLICT_EXIT)))
    }
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
/// object database to resolve and load each blob. Output labels default to the
/// resolved (full) object ids, as git does in `--object-id` mode.
fn read_object_id_inputs(operands: &[String]) -> Result<MergeInputs> {
    let git_dir = crate::session::cli_git_dir()?;
    let format = repository_object_format(&git_dir)?;
    let db = FileObjectDatabase::from_git_dir(&git_dir, format);

    let (ours, ours_name) = read_object_id_blob(&db, format, &operands[0])?;
    let (base, base_name) = read_object_id_blob(&db, format, &operands[1])?;
    let (theirs, theirs_name) = read_object_id_blob(&db, format, &operands[2])?;
    Ok(MergeInputs {
        ours,
        base,
        theirs,
        ours_name,
        base_name,
        theirs_name,
    })
}

/// Resolve one object-id operand to `(blob bytes, full-hex name)`. Anything that
/// fails to resolve to a unique blob is the usage-style error (exit 129) git
/// reports in `--object-id` mode.
fn read_object_id_blob(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    spec: &str,
) -> Result<(Vec<u8>, String)> {
    let oid = resolve_object_id(db, format, spec)?;
    match db.read_object(&oid) {
        Ok(object) if object.object_type == ObjectType::Blob => {
            Ok((object.body.clone(), oid.to_hex()))
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
    spec: &str,
) -> Result<ObjectId> {
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

/// Final merged blob plus the number of conflict regions that were emitted (used
/// for the exit status).
struct MergeOutcome {
    content: Vec<u8>,
    conflicts: i32,
}

/// Run the three-way merge, honouring style, favoring and marker size.
///
/// For the default configuration (plain `Merge` style, default marker size, no
/// favoring) the work is delegated straight to [`sley_diff_merge::merge_blobs`];
/// the conflict count is then recovered from the identical region walk so the
/// exit status is exact. Every other configuration is rendered from the region
/// walk directly.
fn merge_three_way(
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    options: &MergeFileOptions,
    ours_label: &str,
    base_label: &str,
    theirs_label: &str,
) -> MergeOutcome {
    let regions = build_regions(base, ours, theirs);
    // A favoring mode (`--ours`/`--theirs`/`--union`) resolves every region, so
    // git reports no conflicts and exits 0; only marker-emitting merges count.
    let conflicts = if options.favor == Favor::None {
        i32::try_from(
            regions
                .iter()
                .filter(|region| matches!(region, Region::Conflict { .. }))
                .count(),
        )
        .unwrap_or(MAX_CONFLICT_EXIT)
    } else {
        0
    };

    let default_config = options.style == MergeStyle::Merge
        && options.favor == Favor::None
        && options.marker_size == DEFAULT_MARKER_SIZE;

    let content = if default_config {
        // Reuse merge_blobs for the common case; byte-for-byte equivalent to the
        // region renderer below but the canonical engine.
        sley_diff_merge::merge_blobs(
            base,
            ours,
            theirs,
            &sley_diff_merge::MergeBlobOptions {
                ours_label,
                theirs_label,
                base_label,
                style: sley_diff_merge::ConflictStyle::Merge,
                favor: sley_diff_merge::MergeFavor::None,
                ws_ignore: sley_diff_merge::WsIgnore::EMPTY,
                marker_size: DEFAULT_MARKER_SIZE,
            },
        )
        .content
    } else {
        render_regions(&regions, options, ours_label, base_label, theirs_label)
    };

    MergeOutcome { content, conflicts }
}

/// One span of the merged output: either text shared by all three inputs (or
/// changed on only one side / changed identically) or a true conflict carrying
/// each side's lines.
enum Region<'a> {
    Stable(Vec<sley_diff_merge::DiffLine<'a>>),
    Conflict {
        ours: Vec<sley_diff_merge::DiffLine<'a>>,
        base: Vec<sley_diff_merge::DiffLine<'a>>,
        theirs: Vec<sley_diff_merge::DiffLine<'a>>,
    },
}

/// Walk base/ours/theirs in lockstep and classify each span, mirroring
/// `sley_diff_merge::merge_blobs`' own algorithm so conflict counting and region
/// boundaries match it exactly. Reuses `split_lines` + `myers_diff_lines`, the
/// same primitives `merge_blobs` is built on.
fn build_regions<'a>(base: &'a [u8], ours: &'a [u8], theirs: &'a [u8]) -> Vec<Region<'a>> {
    let base_lines = sley_diff_merge::split_lines(base);
    let ours_lines = sley_diff_merge::split_lines(ours);
    let theirs_lines = sley_diff_merge::split_lines(theirs);

    let ours_matches = matching_regions(&base_lines, &ours_lines);
    let theirs_matches = matching_regions(&base_lines, &theirs_lines);
    let stable = common_stable_segments(&ours_matches, &theirs_matches);

    let mut regions = Vec::new();
    let mut base_idx = 0usize;
    let mut our_idx = 0usize;
    let mut their_idx = 0usize;

    for seg in &stable {
        classify_changed_region(
            &mut regions,
            &base_lines[base_idx..seg.base_start],
            &ours_lines[our_idx..seg.ours_start],
            &theirs_lines[their_idx..seg.theirs_start],
        );
        push_stable(
            &mut regions,
            &base_lines[seg.base_start..seg.base_start + seg.len],
        );
        base_idx = seg.base_start + seg.len;
        our_idx = seg.ours_start + seg.len;
        their_idx = seg.theirs_start + seg.len;
    }

    classify_changed_region(
        &mut regions,
        &base_lines[base_idx..],
        &ours_lines[our_idx..],
        &theirs_lines[their_idx..],
    );

    regions
}

/// Append `lines` to the trailing `Stable` region, creating one if needed, so
/// consecutive stable spans coalesce (matching merge_blobs' single output
/// stream).
fn push_stable<'a>(regions: &mut Vec<Region<'a>>, lines: &[sley_diff_merge::DiffLine<'a>]) {
    if lines.is_empty() {
        return;
    }
    if let Some(Region::Stable(existing)) = regions.last_mut() {
        existing.extend_from_slice(lines);
    } else {
        regions.push(Region::Stable(lines.to_vec()));
    }
}

/// Resolve one changed span (the gap between two stable segments) using the same
/// diff3 rules as `merge_blobs::emit_region`.
fn classify_changed_region<'a>(
    regions: &mut Vec<Region<'a>>,
    base_region: &[sley_diff_merge::DiffLine<'a>],
    our_region: &[sley_diff_merge::DiffLine<'a>],
    their_region: &[sley_diff_merge::DiffLine<'a>],
) {
    if our_region.is_empty() && their_region.is_empty() {
        return;
    }
    let our_changed = our_region != base_region;
    let their_changed = their_region != base_region;
    match (our_changed, their_changed) {
        (false, false) => push_stable(regions, base_region),
        (true, false) => push_stable(regions, our_region),
        (false, true) => push_stable(regions, their_region),
        (true, true) => {
            if our_region == their_region {
                push_stable(regions, our_region);
            } else {
                regions.push(Region::Conflict {
                    ours: our_region.to_vec(),
                    base: base_region.to_vec(),
                    theirs: their_region.to_vec(),
                });
            }
        }
    }
}

/// A matched (equal) region between base and one side.
#[derive(Debug, Clone, Copy)]
struct MatchRegion {
    base_start: usize,
    side_start: usize,
    len: usize,
}

/// A run of base lines unchanged on both sides, with the aligned side starts.
#[derive(Debug, Clone, Copy)]
struct StableSegment {
    base_start: usize,
    ours_start: usize,
    theirs_start: usize,
    len: usize,
}

fn matching_regions(
    base: &[sley_diff_merge::DiffLine<'_>],
    side: &[sley_diff_merge::DiffLine<'_>],
) -> Vec<MatchRegion> {
    let ops = sley_diff_merge::myers_diff_lines(base, side);
    let mut regions = Vec::new();
    let mut base_idx = 0usize;
    let mut side_idx = 0usize;
    for op in ops {
        match op {
            sley_diff_merge::DiffOp::Equal(n) => {
                regions.push(MatchRegion {
                    base_start: base_idx,
                    side_start: side_idx,
                    len: n,
                });
                base_idx += n;
                side_idx += n;
            }
            sley_diff_merge::DiffOp::Delete(n) => base_idx += n,
            sley_diff_merge::DiffOp::Insert(n) => side_idx += n,
        }
    }
    regions
}

fn common_stable_segments(ours: &[MatchRegion], theirs: &[MatchRegion]) -> Vec<StableSegment> {
    let mut segments = Vec::new();
    let mut oi = 0usize;
    let mut ti = 0usize;
    while oi < ours.len() && ti < theirs.len() {
        let o = ours[oi];
        let t = theirs[ti];
        let o_end = o.base_start + o.len;
        let t_end = t.base_start + t.len;
        let lo = o.base_start.max(t.base_start);
        let hi = o_end.min(t_end);
        if lo < hi {
            segments.push(StableSegment {
                base_start: lo,
                ours_start: o.side_start + (lo - o.base_start),
                theirs_start: t.side_start + (lo - t.base_start),
                len: hi - lo,
            });
        }
        if o_end <= t_end {
            oi += 1;
        } else {
            ti += 1;
        }
    }
    segments
}

/// Render the region list into the final blob, applying style/favoring/marker
/// size. The byte-level conflict-marker conventions match `merge_blobs`: markers
/// always begin on a fresh line, labels are omitted when empty, and a side's raw
/// bytes are preserved verbatim.
fn render_regions(
    regions: &[Region<'_>],
    options: &MergeFileOptions,
    ours_label: &str,
    base_label: &str,
    theirs_label: &str,
) -> Vec<u8> {
    let mut out = Vec::new();
    for region in regions {
        match region {
            Region::Stable(lines) => emit_lines(&mut out, lines),
            Region::Conflict { ours, base, theirs } => match options.favor {
                Favor::Ours => emit_lines(&mut out, ours),
                Favor::Theirs => emit_lines(&mut out, theirs),
                Favor::Union => {
                    emit_lines(&mut out, ours);
                    emit_lines(&mut out, theirs);
                }
                Favor::None => emit_conflict(
                    &mut out,
                    ConflictLines { ours, base, theirs },
                    ConflictMarkers {
                        style: options.style,
                        marker_size: options.marker_size,
                        ours_label,
                        base_label,
                        theirs_label,
                    },
                ),
            },
        }
    }
    out
}

fn emit_lines(out: &mut Vec<u8>, lines: &[sley_diff_merge::DiffLine<'_>]) {
    for line in lines {
        out.extend_from_slice(line.content);
    }
}

/// Count the lines shared, in order, at the start of `a` and `b` (used by zdiff3
/// to hoist common context out of a conflict).
fn common_prefix_len(
    a: &[sley_diff_merge::DiffLine<'_>],
    b: &[sley_diff_merge::DiffLine<'_>],
) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

/// Count the lines shared, in order, at the end of `a` and `b`, never exceeding
/// what remains after an already-counted prefix of length `reserve` on either
/// side.
fn common_suffix_len(
    a: &[sley_diff_merge::DiffLine<'_>],
    b: &[sley_diff_merge::DiffLine<'_>],
    reserve: usize,
) -> usize {
    let max = a.len().min(b.len()).saturating_sub(reserve);
    let mut count = 0usize;
    while count < max && a[a.len() - 1 - count] == b[b.len() - 1 - count] {
        count += 1;
    }
    count
}

struct ConflictLines<'a, 'line> {
    ours: &'a [sley_diff_merge::DiffLine<'line>],
    base: &'a [sley_diff_merge::DiffLine<'line>],
    theirs: &'a [sley_diff_merge::DiffLine<'line>],
}

struct ConflictMarkers<'a> {
    style: MergeStyle,
    marker_size: usize,
    ours_label: &'a str,
    base_label: &'a str,
    theirs_label: &'a str,
}

fn emit_conflict(out: &mut Vec<u8>, lines: ConflictLines<'_, '_>, markers: ConflictMarkers<'_>) {
    let ConflictLines { ours, base, theirs } = lines;
    let ConflictMarkers {
        style,
        marker_size,
        ours_label,
        base_label,
        theirs_label,
    } = markers;

    // zdiff3 hoists shared leading/trailing context out of the conflict.
    let (prefix, suffix) = if style == MergeStyle::Zdiff3 {
        let prefix = common_prefix_len(ours, theirs);
        let suffix = common_suffix_len(ours, theirs, prefix);
        (prefix, suffix)
    } else {
        (0, 0)
    };

    if prefix > 0 {
        emit_lines(out, &ours[..prefix]);
    }
    let ours_inner = &ours[prefix..ours.len() - suffix];
    let theirs_inner = &theirs[prefix..theirs.len() - suffix];

    write_marker(out, b'<', marker_size, ours_label);
    emit_lines(out, ours_inner);
    if style == MergeStyle::Diff3 || style == MergeStyle::Zdiff3 {
        ensure_newline(out);
        write_marker(out, b'|', marker_size, base_label);
        emit_lines(out, base);
    }
    ensure_newline(out);
    write_divider(out, marker_size);
    emit_lines(out, theirs_inner);
    ensure_newline(out);
    write_marker(out, b'>', marker_size, theirs_label);

    if suffix > 0 {
        emit_lines(out, &ours[ours.len() - suffix..]);
    }
}

/// Ensure the buffer ends with a newline before the next marker, so markers
/// always start a fresh line even after a no-newline-at-eof side.
fn ensure_newline(out: &mut Vec<u8>) {
    if !out.is_empty() && out.last() != Some(&b'\n') {
        out.push(b'\n');
    }
}

/// Write a marker line: `marker_size` copies of `ch`, then (if the label is
/// non-empty) a space and the label, then a newline.
fn write_marker(out: &mut Vec<u8>, ch: u8, marker_size: usize, label: &str) {
    for _ in 0..marker_size {
        out.push(ch);
    }
    if !label.is_empty() {
        out.push(b' ');
        out.extend_from_slice(label.as_bytes());
    }
    out.push(b'\n');
}

fn write_divider(out: &mut Vec<u8>, marker_size: usize) {
    for _ in 0..marker_size {
        out.push(b'=');
    }
    out.push(b'\n');
}

/// Deliver the merged blob: to stdout with `-p`, to a freshly written object
/// (printing its id) in `--object-id` mode, or back into the current file.
fn emit_result(options: &MergeFileOptions, inputs: &MergeInputs, content: &[u8]) -> Result<()> {
    if options.to_stdout {
        let stdout = io::stdout();
        let mut handle = stdout.lock();
        handle.write_all(content)?;
        return Ok(());
    }
    if options.object_id {
        let git_dir = crate::session::cli_git_dir()?;
        let format = repository_object_format(&git_dir)?;
        let db = FileObjectDatabase::from_git_dir(&git_dir, format);
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
            favor: Favor::None,
            marker_size: DEFAULT_MARKER_SIZE,
            labels: Vec::new(),
            operands: operands.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    fn merge(base: &[u8], ours: &[u8], theirs: &[u8], options: &MergeFileOptions) -> MergeOutcome {
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
