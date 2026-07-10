//! Shared diff UI option parsing, mirroring git's `struct diff_options` shape.
//! ADR 0001 Engine B.

use sley_config::GitConfig;
use sley_core::{GitError, Result};
use sley_diff_merge::format::WordDiffMode;
use sley_options::{
    CallbackValue, OptFlags, OptValue, OptionSpec, ParsedOption, ParsedValue, UsageError,
    parse_options,
};
use std::collections::HashSet;
include!("diff_options_support.rs");

const OPTARG_NONE: &str = "\0";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffOutputFormat(u32);

impl DiffOutputFormat {
    pub const RAW: Self = Self(0x0001);
    pub const DIFFSTAT: Self = Self(0x0002);
    pub const NUMSTAT: Self = Self(0x0004);
    pub const SUMMARY: Self = Self(0x0008);
    pub const PATCH: Self = Self(0x0010);
    pub const SHORTSTAT: Self = Self(0x0020);
    pub const DIRSTAT: Self = Self(0x0040);
    pub const NAME_ONLY: Self = Self(0x0100);
    pub const NAME_STATUS: Self = Self(0x0200);
    pub const CHECK: Self = Self(0x0400);
    pub const NO_OUTPUT: Self = Self(0x0800);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }

    fn set_exact(&mut self, other: Self) {
        self.0 = other.0;
    }

    fn has_multi_bits(self, mask: Self) -> bool {
        (self.0 & mask.0).count_ones() > 1
    }

    fn bitop(&mut self, set: Self, clear: Self) {
        self.remove(clear);
        self.insert(set);
    }
}

#[derive(Clone)]
pub struct DiffOptions {
    pub output_format: DiffOutputFormat,
    pub cached: bool,
    pub quiet: bool,
    pub exit_code: bool,
    pub allow_external: bool,
    pub output: Option<String>,
    pub line_prefix: Option<String>,
    pub compact_summary: bool,
    pub stat_count: Option<usize>,
    pub stat_widths: DiffStatWidths,
    pub dirstat: Option<DirstatOptions>,
    pub dirstat_cli_params: Vec<String>,
    pub context: Option<usize>,
    pub reverse: bool,
    pub pickaxe: Option<String>,
    pub pickaxe_all: bool,
    pub pickaxe_regex: bool,
    pub find_object_values: Vec<String>,
    pub raw_abbrev: Option<Option<usize>>,
    pub patch_abbrev: Option<usize>,
    pub patch_full_index: bool,
    /// `--binary`: emit `GIT binary patch` blocks (implies full index).
    pub patch_binary: bool,
    pub color_always: bool,
    pub color_moved: Option<Option<sley_diff_merge::render::ColorMovedMode>>,
    pub color_moved_ws: Option<sley_diff_merge::render::ColorMovedWs>,
    pub diff_algorithm_control: bool,
    pub diff_algorithm: sley_diff_merge::DiffAlgorithm,
    /// `--anchored=<text>` prefixes; forces patience and pins matching lines.
    /// Cleared by a later `--patience` (git's anchor reset), preserved across
    /// `--histogram` (which just disables anchoring by changing the algorithm).
    pub anchored: Vec<Vec<u8>>,
    pub diff_driver_control: bool,
    pub diff_hunk_control: bool,
    pub interhunk: Option<usize>,
    pub diff_whitespace_control: bool,
    pub ws_error_highlight: Option<String>,
    /// CLI `--indent-heuristic` / `--no-indent-heuristic`: `Some(true)` /
    /// `Some(false)` when given, `None` to fall back to `diff.indentHeuristic`
    /// config (which itself defaults to git's enabled-by-default behavior).
    pub indent_heuristic: Option<bool>,
    pub ws_ignore: sley_diff_merge::WsIgnore,
    pub ignore_blank_lines: bool,
    pub ignore_regexes: Vec<String>,
    pub diff_output_indicator_control: bool,
    pub diff_patch_context_control: bool,
    pub diff_patch_output_control: bool,
    pub diff_rewrite_control: bool,
    pub diff_submodule_format: Option<SubmoduleDiffFormat>,
    pub word_diff_mode: Option<WordDiffMode>,
    pub word_diff_regex: Option<String>,
    pub no_index: bool,
    pub combined: Option<bool>,
    pub diff_relative: DiffRelativeMode,
    pub diff_relative_explicit: bool,
    pub src_prefix: String,
    pub dst_prefix: String,
    /// CLI `--no-prefix` was given (overrides `diff.*Prefix` config).
    pub cli_no_prefix: bool,
    /// CLI `--default-prefix` was given (resets both prefixes to `a/`/`b/`,
    /// overriding config).
    pub cli_default_prefix: bool,
    /// CLI `--src-prefix=<p>` override (overrides `diff.srcPrefix` config).
    pub cli_src_prefix: Option<String>,
    /// CLI `--dst-prefix=<p>` override (overrides `diff.dstPrefix` config).
    pub cli_dst_prefix: Option<String>,
    pub head: bool,
    pub z: bool,
    pub detect_renames: bool,
    pub detect_copies: bool,
    pub find_copies_harder: bool,
    pub rename_empty: bool,
    pub inexact_renames: bool,
    pub renames_explicit: bool,
    pub rename_threshold: u8,
    pub copy_threshold: u8,
    pub rename_limit: usize,
    pub diff_filter: DiffFilter,
    pub ignore_submodules_cli: Option<SubmoduleIgnoreMode>,
    pub merge_base: bool,
    /// `-O<file>`: path-ordering file (`diffcore_order`). `Some` when given on
    /// the CLI; a CLI `-O` overrides the `diff.orderfile` config. `-O/dev/null`
    /// is the documented way to cancel a configured orderfile (it reads as zero
    /// patterns, so every path keeps its tree order).
    pub orderfile: Option<String>,
    /// `--rotate-to=<path>` / `--skip-to=<path>`: rotate (or, with `skip`, drop)
    /// the path-sorted diff so it begins at `<path>` (`diffcore_rotate`).
    pub rotate_to: Option<String>,
    /// `true` when the rotate request came from `--skip-to` (drop the leading
    /// entries) rather than `--rotate-to` (move them to the end).
    pub rotate_skip: bool,
    pub path_args: Vec<String>,
    pub explicit_paths: Vec<String>,
}

impl Default for DiffOptions {
    fn default() -> Self {
        Self {
            output_format: DiffOutputFormat::empty(),
            cached: false,
            quiet: false,
            exit_code: false,
            allow_external: true,
            output: None,
            line_prefix: None,
            compact_summary: false,
            stat_count: None,
            stat_widths: DiffStatWidths::terminal(),
            dirstat: None,
            dirstat_cli_params: Vec::new(),
            context: None,
            reverse: false,
            pickaxe: None,
            pickaxe_all: false,
            pickaxe_regex: false,
            find_object_values: Vec::new(),
            raw_abbrev: None,
            patch_abbrev: None,
            patch_full_index: false,
            patch_binary: false,
            color_always: false,
            color_moved: None,
            color_moved_ws: None,
            diff_algorithm_control: false,
            diff_algorithm: sley_diff_merge::DiffAlgorithm::Myers,
            anchored: Vec::new(),
            diff_driver_control: false,
            diff_hunk_control: false,
            interhunk: None,
            diff_whitespace_control: false,
            ws_error_highlight: None,
            indent_heuristic: None,
            ws_ignore: sley_diff_merge::WsIgnore::default(),
            ignore_blank_lines: false,
            ignore_regexes: Vec::new(),
            diff_output_indicator_control: false,
            diff_patch_context_control: false,
            diff_patch_output_control: false,
            diff_rewrite_control: false,
            diff_submodule_format: None,
            word_diff_mode: None,
            word_diff_regex: None,
            no_index: false,
            combined: None,
            diff_relative: DiffRelativeMode::Off,
            diff_relative_explicit: false,
            src_prefix: "a/".to_string(),
            dst_prefix: "b/".to_string(),
            cli_no_prefix: false,
            cli_default_prefix: false,
            cli_src_prefix: None,
            cli_dst_prefix: None,
            head: false,
            z: false,
            detect_renames: true,
            detect_copies: false,
            find_copies_harder: false,
            rename_empty: true,
            inexact_renames: true,
            renames_explicit: false,
            rename_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
            copy_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
            rename_limit: 0,
            diff_filter: DiffFilter::default(),
            ignore_submodules_cli: None,
            merge_base: false,
            orderfile: None,
            rotate_to: None,
            rotate_skip: false,
            path_args: Vec::new(),
            explicit_paths: Vec::new(),
        }
    }
}

impl DiffOptions {
    pub fn validate(&self) -> Result<()> {
        let check_mask = DiffOutputFormat(
            DiffOutputFormat::NAME_ONLY.0
                | DiffOutputFormat::NAME_STATUS.0
                | DiffOutputFormat::CHECK.0
                | DiffOutputFormat::NO_OUTPUT.0,
        );
        if self.output_format.has_multi_bits(check_mask) {
            eprintln!(
                "fatal: options '--name-only', '--name-status', '--check', and '-s' cannot be used together"
            );
            return Err(GitError::Exit(129));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffRelativeMode {
    Off,
    Cwd,
    Prefix(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmoduleDiffFormat {
    Short,
    Log,
    Diff,
}

impl SubmoduleDiffFormat {
    pub fn parse(value: &str) -> Self {
        match value {
            "short" => Self::Short,
            "diff" => Self::Diff,
            _ => Self::Log,
        }
    }
}

pub fn setup_diff_options(args: &[String]) -> Result<DiffOptions> {
    reject_exact_no_rename(args)?;
    let (parse_args, explicit_paths) = split_explicit_path_args(args);
    let parsed = parse_options(parse_args, diff_option_specs(), DIFF_USAGE)
        .map_err(diff_options_usage_error)?;
    let mut options = DiffOptions {
        explicit_paths,
        ..DiffOptions::default()
    };

    for option in &parsed.options {
        apply_diff_option(&mut options, option)?;
    }
    for positional in parsed.positionals {
        if positional == "HEAD" && !options.head && options.path_args.is_empty() {
            options.head = true;
        } else {
            options.path_args.push(positional.to_string());
        }
    }
    options.validate()?;
    Ok(options)
}

pub fn resolve_diff_context(
    cli_context: Option<usize>,
    config: Option<&GitConfig>,
) -> Result<usize> {
    let config_context = match config.and_then(|config| config.get("diff", None, "context")) {
        Some(value) => {
            let Some(parsed) = sley_config::parse_config_int(value) else {
                eprintln!(
                    "fatal: bad numeric config value '{value}' for 'diff.context': invalid unit"
                );
                return Err(GitError::Exit(128));
            };
            if parsed < 0 {
                eprintln!("fatal: bad config variable 'diff.context'");
                return Err(GitError::Exit(128));
            }
            Some(parsed as usize)
        }
        None => None,
    };
    Ok(cli_context.or(config_context).unwrap_or(3))
}

const DIFF_USAGE: &[&str] = &["git diff [<options>] [<commit>] [--] [<path>...]"];

fn diff_option_specs() -> &'static [OptionSpec<'static>] {
    static SPECS: &[OptionSpec<'static>] = &[
        opt_bool(
            None,
            Some("name-status"),
            OptFlags::NONEG,
            "show only names and status of changed files",
        ),
        opt_bool(
            None,
            Some("name-only"),
            OptFlags::NONEG,
            "show only names of changed files",
        ),
        opt_bool(Some('r'), None, OptFlags::NONE, "recurse into subtrees"),
        opt_bool(Some('c'), None, OptFlags::NONE, "generate combined diff"),
        opt_bool(
            None,
            Some("cc"),
            OptFlags::NONE,
            "generate dense combined diff",
        ),
        opt_bool(None, Some("cached"), OptFlags::NONE, "show cached changes"),
        opt_bool(
            None,
            Some("merge-base"),
            OptFlags::NONE,
            "use the merge base as the old side",
        ),
        opt_bool(None, Some("staged"), OptFlags::NONE, "show staged changes"),
        opt_bool(None, Some("quiet"), OptFlags::NONE, "disable all output"),
        opt_bool(
            None,
            Some("exit-code"),
            OptFlags::NONE,
            "exit with 1 if there were differences",
        ),
        opt_bool(
            None,
            Some("summary"),
            OptFlags::NONEG,
            "condensed summary such as creations, renames and mode changes",
        ),
        opt_bool(
            None,
            Some("raw"),
            OptFlags::NONEG,
            "generate the diff in raw format",
        ),
        opt_optarg(
            None,
            Some("stat"),
            "<width>[,<name-width>[,<count>]]",
            OptFlags::NONEG,
            "generate diffstat",
        ),
        opt_str(
            None,
            Some("stat-width"),
            "<width>",
            OptFlags::NONEG,
            "generate diffstat with a given width",
        ),
        opt_str(
            None,
            Some("stat-name-width"),
            "<width>",
            OptFlags::NONEG,
            "generate diffstat with a given name width",
        ),
        opt_str(
            None,
            Some("stat-graph-width"),
            "<width>",
            OptFlags::NONEG,
            "generate diffstat with a given graph width",
        ),
        opt_str(
            None,
            Some("stat-count"),
            "<count>",
            OptFlags::NONEG,
            "generate diffstat with limited lines",
        ),
        opt_bool(
            None,
            Some("compact-summary"),
            OptFlags::NONE,
            "generate compact summary in diffstat",
        ),
        opt_bool(
            None,
            Some("numstat"),
            OptFlags::NONEG,
            "machine friendly --stat",
        ),
        opt_bool(
            None,
            Some("shortstat"),
            OptFlags::NONEG,
            "output only the last line of --stat",
        ),
        opt_optarg(
            None,
            Some("dirstat"),
            "<param1>,<param2>...",
            OptFlags::NONEG,
            "output dirstat",
        ),
        opt_optarg(
            Some('X'),
            None,
            "<param1>,<param2>...",
            OptFlags::NONEG,
            "output dirstat",
        ),
        opt_bool(
            None,
            Some("cumulative"),
            OptFlags::NONEG,
            "synonym for --dirstat=cumulative",
        ),
        opt_optarg(
            None,
            Some("dirstat-by-file"),
            "<param1>,<param2>...",
            OptFlags::NONEG,
            "synonym for --dirstat=files",
        ),
        opt_bool(
            None,
            Some("check"),
            OptFlags::NONEG,
            "warn if changes introduce conflict markers or whitespace errors",
        ),
        opt_bool(Some('p'), Some("patch"), OptFlags::NONE, "generate patch"),
        opt_bool(Some('u'), None, OptFlags::NONE, "generate patch"),
        opt_optarg(
            Some('U'),
            Some("unified"),
            "<n>",
            OptFlags::NONEG,
            "generate diffs with <n> lines context",
        ),
        opt_bool(
            None,
            Some("patch-with-raw"),
            OptFlags::NONEG,
            "synonym for '-p --raw'",
        ),
        opt_bool(
            None,
            Some("patch-with-stat"),
            OptFlags::NONEG,
            "synonym for '-p --stat'",
        ),
        opt_bool(
            Some('s'),
            Some("no-patch"),
            OptFlags::NONE,
            "suppress diff output",
        ),
        opt_bool(
            Some('a'),
            Some("text"),
            OptFlags::NONE,
            "treat all files as text",
        ),
        opt_bool(
            None,
            Some("ext-diff"),
            OptFlags::NONE,
            "allow external diff helper",
        ),
        opt_str(
            None,
            Some("output"),
            "<file>",
            OptFlags::NONEG,
            "output to a specific file",
        ),
        opt_str(
            Some('O'),
            None,
            "<file>",
            OptFlags::NONEG,
            "control the order in which files appear in the output",
        ),
        opt_str(
            None,
            Some("rotate-to"),
            "<path>",
            OptFlags::NONEG,
            "show the change in the specified path first",
        ),
        opt_str(
            None,
            Some("skip-to"),
            "<path>",
            OptFlags::NONEG,
            "skip the output to the specified path",
        ),
        opt_str(
            None,
            Some("line-prefix"),
            "<prefix>",
            OptFlags::NONEG,
            "prepend an additional prefix to every line of output",
        ),
        opt_bool(
            None,
            Some("textconv"),
            OptFlags::NONE,
            "run external text conversion filters",
        ),
        opt_bool(
            None,
            Some("no-index"),
            OptFlags::NONE,
            "compare two paths outside a repository",
        ),
        opt_bool(Some('R'), None, OptFlags::NONE, "swap two inputs"),
        opt_str(
            Some('S'),
            None,
            "<string>",
            OptFlags::NONE,
            "look for differences that change string occurrences",
        ),
        opt_bool(
            None,
            Some("pickaxe-all"),
            OptFlags::NONEG,
            "show all changes with -S",
        ),
        opt_bool(
            None,
            Some("pickaxe-regex"),
            OptFlags::NONEG,
            "treat -S as extended regex",
        ),
        opt_str(
            None,
            Some("find-object"),
            "<object-id>",
            OptFlags::NONEG,
            "look for differences that change object occurrences",
        ),
        opt_bool(
            None,
            Some("minimal"),
            OptFlags::NONEG,
            "produce the smallest possible diff",
        ),
        opt_bool(
            None,
            Some("patience"),
            OptFlags::NONEG,
            "generate diff using patience",
        ),
        opt_bool(
            None,
            Some("histogram"),
            OptFlags::NONEG,
            "generate diff using histogram",
        ),
        opt_str(
            None,
            Some("anchored"),
            "<text>",
            OptFlags::NONEG,
            "generate anchored diff",
        ),
        opt_str(
            None,
            Some("diff-algorithm"),
            "<algorithm>",
            OptFlags::NONEG,
            "choose a diff algorithm",
        ),
        opt_str(
            None,
            Some("inter-hunk-context"),
            "<n>",
            OptFlags::NONEG,
            "show context between hunks",
        ),
        opt_str(
            None,
            Some("ws-error-highlight"),
            "<kind>",
            OptFlags::NONEG,
            "highlight whitespace errors",
        ),
        opt_bool(
            Some('b'),
            Some("ignore-space-change"),
            OptFlags::NONEG,
            "ignore changes in amount of whitespace",
        ),
        opt_bool(
            Some('w'),
            Some("ignore-all-space"),
            OptFlags::NONEG,
            "ignore whitespace when comparing lines",
        ),
        opt_bool(
            None,
            Some("ignore-space-at-eol"),
            OptFlags::NONEG,
            "ignore changes in whitespace at EOL",
        ),
        opt_bool(
            None,
            Some("ignore-cr-at-eol"),
            OptFlags::NONEG,
            "ignore carriage-return at EOL",
        ),
        opt_bool(
            None,
            Some("ignore-blank-lines"),
            OptFlags::NONEG,
            "ignore changes whose lines are all blank",
        ),
        opt_str(
            Some('I'),
            Some("ignore-matching-lines"),
            "<regex>",
            OptFlags::NONEG,
            "ignore changes whose all lines match <regex>",
        ),
        opt_optarg(
            None,
            Some("submodule"),
            "<format>",
            OptFlags::NONEG,
            "specify submodule diff format",
        ),
        opt_optarg(
            None,
            Some("word-diff"),
            "<mode>",
            OptFlags::NONEG,
            "show word diff",
        ),
        opt_str(
            None,
            Some("word-diff-regex"),
            "<regex>",
            OptFlags::NONEG,
            "regex for word diff",
        ),
        opt_optarg(
            None,
            Some("color-words"),
            "<regex>",
            OptFlags::NONEG,
            "equivalent to --word-diff=color",
        ),
        opt_str(
            None,
            Some("output-indicator-new"),
            "<char>",
            OptFlags::NONEG,
            "character for new lines",
        ),
        opt_str(
            None,
            Some("output-indicator-old"),
            "<char>",
            OptFlags::NONEG,
            "character for old lines",
        ),
        opt_str(
            None,
            Some("output-indicator-context"),
            "<char>",
            OptFlags::NONEG,
            "character for context lines",
        ),
        opt_bool(
            Some('W'),
            Some("function-context"),
            OptFlags::NONE,
            "show whole function as context",
        ),
        opt_bool(
            None,
            Some("indent-heuristic"),
            OptFlags::NONE,
            "heuristic to shift hunk boundaries",
        ),
        opt_bool(None, Some("full-diff"), OptFlags::NONE, "show full diff"),
        opt_bool(
            Some('D'),
            Some("irreversible-delete"),
            OptFlags::NONEG,
            "omit preimage for deletes",
        ),
        opt_bool(
            None,
            Some("ita-visible-in-index"),
            OptFlags::NONEG,
            "treat intent-to-add entries as real",
        ),
        opt_bool(
            None,
            Some("ita-invisible-in-index"),
            OptFlags::NONEG,
            "hide intent-to-add entries",
        ),
        opt_optarg(
            Some('B'),
            Some("break-rewrites"),
            "<n>[/<m>]",
            OptFlags::NONEG,
            "break complete rewrites",
        ),
        opt_optarg(
            None,
            Some("relative"),
            "<prefix>",
            OptFlags::NONE,
            "show relative paths",
        ),
        opt_bool(
            None,
            Some("no-relative"),
            OptFlags::NONE,
            "do not show relative paths",
        ),
        opt_optarg(
            None,
            Some("color"),
            "<when>",
            OptFlags::NONE,
            "show colored diff",
        ),
        opt_bool(
            None,
            Some("no-color"),
            OptFlags::NONE,
            "turn off colored diff",
        ),
        opt_optarg(
            None,
            Some("color-moved"),
            "<mode>",
            OptFlags::NONE,
            "color moved lines differently",
        ),
        opt_bool(
            None,
            Some("no-color-moved"),
            OptFlags::NONE,
            "do not color moved lines",
        ),
        opt_str(
            None,
            Some("color-moved-ws"),
            "<mode>",
            OptFlags::NONE,
            "how whitespace is ignored in --color-moved",
        ),
        opt_bool(
            None,
            Some("no-color-moved-ws"),
            OptFlags::NONE,
            "do not ignore whitespace in moved lines",
        ),
        opt_optarg(
            None,
            Some("ignore-submodules"),
            "<when>",
            OptFlags::NONEG,
            "ignore submodule changes",
        ),
        opt_optarg(
            None,
            Some("abbrev"),
            "<n>",
            OptFlags::NONE,
            "abbreviate object names",
        ),
        opt_bool(
            None,
            Some("no-abbrev"),
            OptFlags::NONE,
            "show full object names in raw output",
        ),
        opt_bool(
            None,
            Some("full-index"),
            OptFlags::NONE,
            "show full object names on index lines",
        ),
        opt_bool(
            None,
            Some("binary"),
            OptFlags::NONE,
            "output a binary diff that can be applied",
        ),
        opt_bool(
            None,
            Some("no-prefix"),
            OptFlags::NONEG,
            "do not show source or destination prefix",
        ),
        opt_bool(
            None,
            Some("default-prefix"),
            OptFlags::NONEG,
            "use default prefixes",
        ),
        opt_str(
            None,
            Some("src-prefix"),
            "<prefix>",
            OptFlags::NONEG,
            "show source prefix",
        ),
        opt_str(
            None,
            Some("dst-prefix"),
            "<prefix>",
            OptFlags::NONEG,
            "show destination prefix",
        ),
        opt_bool(
            Some('z'),
            None,
            OptFlags::NONE,
            "use NUL output field terminators",
        ),
        opt_optarg(
            Some('M'),
            Some("find-renames"),
            "<n>",
            OptFlags::NONEG,
            "detect renames",
        ),
        opt_optarg(
            Some('C'),
            Some("find-copies"),
            "<n>",
            OptFlags::NONEG,
            "detect copies",
        ),
        opt_bool(
            None,
            Some("find-copies-harder"),
            OptFlags::NONE,
            "use unmodified files as copy source",
        ),
        opt_bool(
            None,
            Some("no-renames"),
            OptFlags::NONEG,
            "disable rename detection",
        ),
        opt_bool(
            None,
            Some("rename-empty"),
            OptFlags::NONE,
            "use empty blobs as rename source",
        ),
        opt_str(Some('l'), None, "<n>", OptFlags::NONE, "rename limit"),
        opt_str(
            None,
            Some("diff-filter"),
            "<filter>",
            OptFlags::NONEG,
            "select files by diff status",
        ),
    ];
    SPECS
}

const fn opt_bool(
    short: Option<char>,
    long: Option<&'static str>,
    flags: OptFlags,
    help: &'static str,
) -> OptionSpec<'static> {
    OptionSpec {
        short,
        long,
        value: OptValue::Bool,
        flags,
        help,
    }
}

const fn opt_str(
    short: Option<char>,
    long: Option<&'static str>,
    metavar: &'static str,
    flags: OptFlags,
    help: &'static str,
) -> OptionSpec<'static> {
    OptionSpec {
        short,
        long,
        value: OptValue::Str(metavar),
        flags,
        help,
    }
}

const fn opt_optarg(
    short: Option<char>,
    long: Option<&'static str>,
    metavar: &'static str,
    flags: OptFlags,
    help: &'static str,
) -> OptionSpec<'static> {
    OptionSpec {
        short,
        long,
        value: OptValue::Callback {
            metavar: Some(metavar),
            parse: optional_arg_callback,
        },
        flags: flags.union(OptFlags::OPTARG),
        help,
    }
}

fn optional_arg_callback(value: CallbackValue<'_>) -> std::result::Result<Option<String>, String> {
    Ok(Some(value.value.unwrap_or(OPTARG_NONE).to_string()))
}

fn split_explicit_path_args(args: &[String]) -> (&[String], Vec<String>) {
    match args.iter().position(|arg| arg == "--") {
        Some(index) => (&args[..index], args[index + 1..].to_vec()),
        None => (args, Vec::new()),
    }
}

fn reject_exact_no_rename(args: &[String]) -> Result<()> {
    if args.iter().any(|arg| arg == "--no-rename") {
        eprintln!("error: invalid option: --no-rename");
        Err(GitError::Exit(129))
    } else {
        Ok(())
    }
}

fn diff_options_usage_error(error: UsageError) -> GitError {
    eprint!("{}", error.render_stderr());
    GitError::Exit(error.exit_code())
}

fn apply_diff_option(options: &mut DiffOptions, option: &ParsedOption<'_>) -> Result<()> {
    match (option.short, option.long) {
        (_, Some("name-status")) => options.output_format.insert(DiffOutputFormat::NAME_STATUS),
        (_, Some("name-only")) => options.output_format.insert(DiffOutputFormat::NAME_ONLY),
        (Some('r'), None) => {}
        (_, Some("cached" | "staged")) => options.cached = true,
        (_, Some("merge-base")) => options.merge_base = bool_value(option),
        (_, Some("quiet")) => options.quiet = bool_value(option),
        (_, Some("exit-code")) => options.exit_code = bool_value(option),
        (_, Some("output")) => options.output = Some(str_value(option).to_string()),
        (Some('O'), None) => options.orderfile = Some(str_value(option).to_string()),
        (_, Some("rotate-to")) => {
            options.rotate_to = Some(str_value(option).to_string());
            options.rotate_skip = false;
        }
        (_, Some("skip-to")) => {
            options.rotate_to = Some(str_value(option).to_string());
            options.rotate_skip = true;
        }
        (_, Some("line-prefix")) => options.line_prefix = Some(str_value(option).to_string()),
        (Some('c'), None) => options.combined = Some(false),
        (_, Some("cc")) => options.combined = Some(true),
        (_, Some("summary")) => options
            .output_format
            .bitop(DiffOutputFormat::SUMMARY, DiffOutputFormat::NO_OUTPUT),
        (_, Some("raw")) => options
            .output_format
            .bitop(DiffOutputFormat::RAW, DiffOutputFormat::NO_OUTPUT),
        (_, Some("stat")) => {
            options
                .output_format
                .bitop(DiffOutputFormat::DIFFSTAT, DiffOutputFormat::NO_OUTPUT);
            if let Some(value) = optional_arg(option) {
                let value = format!("--stat={value}");
                diff_stat_parse_width_option(&value, &mut options.stat_widths)?;
                if let Some(count) = diff_stat_count_option(&value)? {
                    options.stat_count = count;
                }
            }
        }
        (
            _,
            Some(long @ ("stat-width" | "stat-name-width" | "stat-graph-width" | "stat-count")),
        ) => {
            options
                .output_format
                .bitop(DiffOutputFormat::DIFFSTAT, DiffOutputFormat::NO_OUTPUT);
            let value = format!("--{long}={}", str_value(option));
            diff_stat_parse_width_option(&value, &mut options.stat_widths)?;
            if let Some(count) = diff_stat_count_option(&value)? {
                options.stat_count = count;
            }
        }
        (_, Some("compact-summary")) => {
            options.compact_summary = true;
            options
                .output_format
                .bitop(DiffOutputFormat::DIFFSTAT, DiffOutputFormat::NO_OUTPUT);
        }
        (_, Some("numstat")) => options
            .output_format
            .bitop(DiffOutputFormat::NUMSTAT, DiffOutputFormat::NO_OUTPUT),
        (_, Some("shortstat")) => options
            .output_format
            .bitop(DiffOutputFormat::SHORTSTAT, DiffOutputFormat::NO_OUTPUT),
        (Some('X'), None) | (_, Some("dirstat")) => {
            options.dirstat.get_or_insert_with(DirstatOptions::default);
            options
                .output_format
                .bitop(DiffOutputFormat::DIRSTAT, DiffOutputFormat::NO_OUTPUT);
            if let Some(value) = optional_arg(option) {
                options.dirstat_cli_params.push(value.to_string());
            }
        }
        (_, Some("cumulative")) => {
            let opts = options.dirstat.get_or_insert_with(DirstatOptions::default);
            opts.cumulative = true;
            options
                .output_format
                .bitop(DiffOutputFormat::DIRSTAT, DiffOutputFormat::NO_OUTPUT);
        }
        (_, Some("dirstat-by-file")) => {
            let opts = options.dirstat.get_or_insert_with(DirstatOptions::default);
            opts.mode = DirstatMode::Files;
            if let Some(value) = optional_arg(option) {
                options.dirstat_cli_params.push(value.to_string());
            }
            options
                .output_format
                .bitop(DiffOutputFormat::DIRSTAT, DiffOutputFormat::NO_OUTPUT);
        }
        (_, Some("check")) => options.output_format.insert(DiffOutputFormat::CHECK),
        (Some('p'), Some("patch")) | (Some('u'), None) => options
            .output_format
            .bitop(DiffOutputFormat::PATCH, DiffOutputFormat::NO_OUTPUT),
        (Some('U'), Some("unified")) => {
            let value = optional_arg(option).unwrap_or("3");
            commit_validate_unified_context(value, true)?;
            options.context = Some(parse_unified_count(value));
            options
                .output_format
                .bitop(DiffOutputFormat::PATCH, DiffOutputFormat::NO_OUTPUT);
        }
        (_, Some("patch-with-raw")) => options.output_format.bitop(
            DiffOutputFormat(DiffOutputFormat::PATCH.0 | DiffOutputFormat::RAW.0),
            DiffOutputFormat::NO_OUTPUT,
        ),
        (_, Some("patch-with-stat")) => options.output_format.bitop(
            DiffOutputFormat(DiffOutputFormat::PATCH.0 | DiffOutputFormat::DIFFSTAT.0),
            DiffOutputFormat::NO_OUTPUT,
        ),
        (Some('s'), Some("no-patch")) => {
            options.output_format.set_exact(DiffOutputFormat::NO_OUTPUT)
        }
        (Some('a'), Some("text")) => {}
        (_, Some("ext-diff")) => {
            options.allow_external = bool_value(option);
        }
        (_, Some("textconv")) => {
            if bool_value(option) {
                options.diff_driver_control = true;
            }
        }
        (_, Some("no-index")) => options.no_index = true,
        (Some('R'), None) => options.reverse = true,
        (Some('S'), None) => {
            let value = str_value(option);
            if value.is_empty() {
                return Err(diff_pickaxe_requires_non_empty_error());
            }
            options.pickaxe = Some(value.to_string());
        }
        (_, Some("pickaxe-all")) => options.pickaxe_all = true,
        (_, Some("pickaxe-regex")) => options.pickaxe_regex = true,
        (_, Some("find-object")) => options
            .find_object_values
            .push(str_value(option).to_string()),
        (_, Some("minimal")) => options.diff_algorithm = sley_diff_merge::DiffAlgorithm::Minimal,
        (_, Some("patience")) => {
            // Both `--patience` and `--anchored` drive the patience engine, so an
            // explicit `--patience` resets any anchors recorded earlier (git's
            // `diff_opt_parse`).
            options.diff_algorithm = sley_diff_merge::DiffAlgorithm::Patience;
            options.anchored.clear();
        }
        (_, Some("histogram")) => {
            options.diff_algorithm = sley_diff_merge::DiffAlgorithm::Histogram
        }
        (_, Some("anchored")) => {
            // `--anchored=<text>` forces the patience algorithm and records the
            // anchor prefix; a later `--patience`/`--histogram` can still override
            // the algorithm (and `--patience` additionally clears the anchors).
            options.diff_algorithm = sley_diff_merge::DiffAlgorithm::Patience;
            options.anchored.push(str_value(option).as_bytes().to_vec());
        }
        (_, Some("diff-algorithm")) => {
            let value = str_value(option);
            log_validate_diff_algorithm(value)?;
            options.diff_algorithm = match value {
                "myers" | "default" => sley_diff_merge::DiffAlgorithm::Myers,
                "minimal" => sley_diff_merge::DiffAlgorithm::Minimal,
                "patience" => sley_diff_merge::DiffAlgorithm::Patience,
                "histogram" => sley_diff_merge::DiffAlgorithm::Histogram,
                _ => sley_diff_merge::DiffAlgorithm::Myers,
            };
        }
        (_, Some("inter-hunk-context")) => {
            let value = str_value(option);
            if value.is_empty() {
                return log_inter_hunk_context_requires_number_error();
            }
            log_validate_inter_hunk_context(value)?;
            options.interhunk = Some(parse_unified_count(value));
        }
        (_, Some("ws-error-highlight")) => {
            let value = str_value(option);
            log_validate_ws_error_highlight(value)?;
            options.ws_error_highlight = Some(value.to_string());
            options.diff_whitespace_control = true;
        }
        (_, Some("ignore-all-space")) => options.ws_ignore.all_space = true,
        (_, Some("ignore-space-change")) => options.ws_ignore.space_change = true,
        (_, Some("ignore-space-at-eol")) => options.ws_ignore.space_at_eol = true,
        (_, Some("ignore-cr-at-eol")) => options.ws_ignore.cr_at_eol = true,
        (_, Some("ignore-blank-lines")) => options.ignore_blank_lines = true,
        (_, Some("ignore-matching-lines")) => {
            options.ignore_regexes.push(str_value(option).to_string());
        }
        (_, Some("submodule")) => {
            let format = optional_arg(option).unwrap_or("log");
            log_validate_submodule_format(format)?;
            options.diff_submodule_format = Some(SubmoduleDiffFormat::parse(format));
        }
        (_, Some("word-diff")) => {
            if let Some(value) = optional_arg(option) {
                diff_validate_word_diff(value)?;
                options.word_diff_mode = match value {
                    "plain" => Some(WordDiffMode::Plain),
                    "porcelain" => Some(WordDiffMode::Porcelain),
                    "color" => {
                        options.color_always = true;
                        Some(WordDiffMode::Color)
                    }
                    _ => None,
                };
            } else if options.word_diff_mode.is_none() {
                options.word_diff_mode = Some(WordDiffMode::Plain);
            }
        }
        (_, Some("word-diff-regex")) => {
            options.word_diff_regex = Some(str_value(option).to_string());
            if options.word_diff_mode.is_none() {
                options.word_diff_mode = Some(WordDiffMode::Plain);
            }
        }
        (_, Some("color-words")) => {
            options.color_always = true;
            options.word_diff_mode = Some(WordDiffMode::Color);
            if let Some(value) = optional_arg(option) {
                options.word_diff_regex = Some(value.to_string());
            }
        }
        (
            _,
            Some(
                long @ ("output-indicator-new"
                | "output-indicator-old"
                | "output-indicator-context"),
            ),
        ) => {
            log_validate_output_indicator(long, str_value(option))?;
            options.diff_output_indicator_control = true;
        }
        (Some('W'), Some("function-context")) => {
            options.diff_patch_context_control = true;
        }
        (_, Some("indent-heuristic")) => {
            options.indent_heuristic = Some(bool_value(option));
        }
        (
            _,
            Some(
                "full-diff"
                | "irreversible-delete"
                | "ita-visible-in-index"
                | "ita-invisible-in-index",
            ),
        ) => {
            options.diff_patch_output_control = true;
        }
        (Some('B'), Some("break-rewrites")) => {
            if let Some(value) = optional_arg(option) {
                log_validate_break_rewrites_option(value)?;
            }
            options.diff_rewrite_control = true;
        }
        (_, Some("relative")) => {
            options.diff_relative_explicit = true;
            options.diff_relative = optional_arg(option)
                .map(|value| DiffRelativeMode::Prefix(value.to_string()))
                .unwrap_or(DiffRelativeMode::Cwd);
        }
        (_, Some("no-relative")) => {
            options.diff_relative = DiffRelativeMode::Off;
            options.diff_relative_explicit = true;
        }
        (_, Some("color")) => match optional_arg(option) {
            None | Some("always") => options.color_always = true,
            Some("never" | "auto") => options.color_always = false,
            Some(value) => log_validate_color(value)?,
        },
        (_, Some("no-color")) => options.color_always = false,
        (_, Some("color-moved")) => {
            options.color_moved = Some(match optional_arg(option) {
                None => Some(sley_diff_merge::render::ColorMovedMode::Zebra),
                Some(value) => parse_color_moved_mode(value)?,
            });
            if let Some(value) = optional_arg(option) {
                log_validate_color_moved(value)?;
            }
        }
        (_, Some("no-color-moved")) => options.color_moved = Some(None),
        (_, Some("color-moved-ws")) => {
            let value = str_value(option);
            log_validate_color_moved_ws(value)?;
            options.color_moved_ws = Some(parse_color_moved_ws(value)?);
        }
        (_, Some("no-color-moved-ws")) => {
            options.color_moved_ws = Some(sley_diff_merge::render::ColorMovedWs::default());
        }
        (_, Some("ignore-submodules")) => {
            let mode = optional_arg(option).unwrap_or("all");
            let Some(mode) = parse_submodule_ignore_mode(mode) else {
                eprintln!("fatal: bad --ignore-submodules argument: {mode}");
                return Err(GitError::Exit(128));
            };
            options.ignore_submodules_cli = Some(mode);
        }
        (_, Some("abbrev")) => {
            if let Some(value) = optional_arg(option) {
                let abbrev = parse_abbrev(value)?.max(4);
                options.raw_abbrev = Some(Some(abbrev));
                options.patch_abbrev = Some(abbrev);
            } else {
                options.raw_abbrev = Some(Some(7));
                options.patch_abbrev = Some(7);
            }
        }
        (_, Some("no-abbrev")) => options.raw_abbrev = Some(None),
        (_, Some("full-index")) => options.patch_full_index = true,
        (_, Some("binary")) => {
            options.patch_binary = true;
            options.patch_full_index = true;
        }
        (_, Some("no-prefix")) => {
            options.src_prefix.clear();
            options.dst_prefix.clear();
            options.cli_no_prefix = true;
            options.cli_default_prefix = false;
        }
        (_, Some("default-prefix")) => {
            options.src_prefix = "a/".to_string();
            options.dst_prefix = "b/".to_string();
            options.cli_default_prefix = true;
            options.cli_no_prefix = false;
            options.cli_src_prefix = None;
            options.cli_dst_prefix = None;
        }
        (_, Some("src-prefix")) => {
            let value = str_value(option).to_string();
            options.src_prefix = value.clone();
            options.cli_src_prefix = Some(value);
        }
        (_, Some("dst-prefix")) => {
            let value = str_value(option).to_string();
            options.dst_prefix = value.clone();
            options.cli_dst_prefix = Some(value);
        }
        (Some('z'), None) => options.z = true,
        (Some('M'), Some("find-renames")) => {
            options.detect_renames = true;
            options.inexact_renames = true;
            options.renames_explicit = true;
            if let Some(value) = optional_arg(option) {
                log_validate_similarity_option(value, "find-renames")?;
                options.rename_threshold = parse_similarity_threshold(value);
            }
        }
        (Some('C'), Some("find-copies")) => {
            if options.detect_copies && optional_arg(option).is_none() {
                options.find_copies_harder = true;
            }
            options.detect_renames = true;
            options.detect_copies = true;
            options.inexact_renames = true;
            options.renames_explicit = true;
            if let Some(value) = optional_arg(option) {
                log_validate_similarity_option(value, "find-copies")?;
                options.copy_threshold = parse_similarity_threshold(value);
            }
        }
        (_, Some("find-copies-harder")) => {
            options.find_copies_harder = bool_value(option);
            if options.find_copies_harder {
                options.detect_renames = true;
                options.detect_copies = true;
                options.inexact_renames = true;
            }
        }
        (_, Some("no-renames")) => {
            options.detect_renames = false;
            options.inexact_renames = false;
            options.renames_explicit = true;
        }
        (_, Some("rename-empty")) => options.rename_empty = bool_value(option),
        (Some('l'), None) => {
            let value = str_value(option);
            validate_diff_rename_limit(value)?;
            options.rename_limit = parse_diff_rename_limit(value);
        }
        (_, Some("diff-filter")) => options.diff_filter = parse_diff_filter(str_value(option))?,
        _ => {}
    }
    Ok(())
}

pub fn parse_color_moved_mode(
    value: &str,
) -> Result<Option<sley_diff_merge::render::ColorMovedMode>> {
    match value {
        "no" | "false" | "0" | "off" => Ok(None),
        "" | "default" | "true" | "1" | "on" | "yes" | "zebra" => {
            Ok(Some(sley_diff_merge::render::ColorMovedMode::Zebra))
        }
        "plain" => Ok(Some(sley_diff_merge::render::ColorMovedMode::Plain)),
        "blocks" => Ok(Some(sley_diff_merge::render::ColorMovedMode::Blocks)),
        "dimmed-zebra" | "dimmed_zebra" => {
            Ok(Some(sley_diff_merge::render::ColorMovedMode::DimmedZebra))
        }
        _ => {
            log_validate_color_moved(value)?;
            Ok(None)
        }
    }
}

pub fn parse_color_moved_ws(value: &str) -> Result<sley_diff_merge::render::ColorMovedWs> {
    let mut ws = sley_diff_merge::render::ColorMovedWs::default();
    if value.is_empty() {
        return log_color_moved_ws_invalid_mode(value, value).map(|_| ws);
    }
    for mode in value.split(',') {
        match mode {
            "no" => ws = sley_diff_merge::render::ColorMovedWs::default(),
            "ignore-space-change" => ws.ignore.space_change = true,
            "ignore-space-at-eol" => ws.ignore.space_at_eol = true,
            "ignore-all-space" => ws.ignore.all_space = true,
            "allow-indentation-change" => ws.allow_indentation_change = true,
            _ => return log_color_moved_ws_invalid_mode(value, mode).map(|_| ws),
        }
    }
    if ws.allow_indentation_change
        && (ws.ignore.all_space || ws.ignore.space_change || ws.ignore.space_at_eol)
    {
        eprintln!(
            "error: color-moved-ws: allow-indentation-change cannot be combined with other whitespace modes"
        );
        eprintln!("error: invalid mode '{value}' in --color-moved-ws");
        return Err(GitError::Exit(129));
    }
    Ok(ws)
}

fn bool_value(option: &ParsedOption<'_>) -> bool {
    match option.value {
        ParsedValue::Bool(value) => value,
        _ => false,
    }
}

fn str_value<'a>(option: &'a ParsedOption<'a>) -> &'a str {
    match option.value {
        ParsedValue::Str(value) => value,
        _ => "",
    }
}

fn optional_arg<'a>(option: &'a ParsedOption<'a>) -> Option<&'a str> {
    match &option.value {
        ParsedValue::Callback(Some(value)) if value == OPTARG_NONE => None,
        ParsedValue::Callback(Some(value)) => Some(value.as_str()),
        _ => None,
    }
}

pub fn diff_pickaxe_requires_non_empty_error() -> GitError {
    eprintln!("error: -S requires a non-empty argument");
    GitError::Exit(129)
}

fn diff_validate_word_diff(value: &str) -> Result<()> {
    match value {
        "plain" | "color" | "porcelain" | "none" => Ok(()),
        _ => {
            eprintln!("error: bad --word-diff argument: {value}");
            Err(GitError::Exit(129))
        }
    }
}

pub fn parse_unified_count(value: &str) -> usize {
    let (number, multiplier) = match value.as_bytes().last() {
        Some(b'k' | b'K') => (&value[..value.len() - 1], 1024usize),
        Some(b'm' | b'M') => (&value[..value.len() - 1], 1024 * 1024),
        Some(b'g' | b'G') => (&value[..value.len() - 1], 1024 * 1024 * 1024),
        _ => (value, 1),
    };
    if let Some(digits) = number.strip_prefix('-') {
        let _ = digits;
        return 0;
    }
    let digits = number.strip_prefix('+').unwrap_or(number);
    digits
        .parse::<usize>()
        .unwrap_or(0)
        .saturating_mul(multiplier)
}
