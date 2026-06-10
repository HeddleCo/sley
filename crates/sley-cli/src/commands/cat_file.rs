//! `git cat-file`: inspect objects and run the batch object-query protocol.

use std::io::{self, BufRead, Write};
use std::path::Path;

use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_object::ObjectType;
use sley_odb::{FileObjectDatabase, ObjectStorageInfo};

use super::args::{GitArgCursor, LongOption, option_takes_no_value, switch_requires_value};
use crate::*;

pub(crate) fn cmd_cat_file(args: &[String]) -> Result<()> {
    CatFileInvocation::parse(args)?.execute()
}

enum CatFileInvocation {
    Object(CatFileObjectRequest),
    Batch(CatFileBatchRequest),
}

impl CatFileInvocation {
    fn parse(args: &[String]) -> Result<Self> {
        CatFileOptions::parse(args)?.into_invocation()
    }

    fn execute(self) -> Result<()> {
        match self {
            Self::Object(request) => request.execute(),
            Self::Batch(request) => request.execute(),
        }
    }
}

struct CatFileOptions {
    batch: Option<(CatFileBatchMode, Option<String>)>,
    buffer: Option<bool>,
    follow_symlinks: bool,
    input_nul: bool,
    output_nul: bool,
    /// The `--filter=<spec>` object filter (`--no-filter` resets to `Disabled`). Upstream
    /// declares it as `OPT_PARSE_LIST_OBJECTS_FILTER`, parsed greedily; the spec is validated
    /// at parse time (unknown / `sparse:path` die) and again post-parse (the filter kinds
    /// `cat-file` does not implement — `sparse:oid`, `tree` — are rejected with `usage:`).
    filter: CatFileObjectsFilter,
    /// The selected command-mode (`-e/-p/-t/-s/--textconv/--filters/--batch-all-objects`).
    ///
    /// Upstream `cat-file` declares all of these as `OPT_CMDMODE` writing to a single
    /// `opt` variable, so any two distinct ones conflict and the conflict diagnostic is
    /// emitted by `parse_options` in command-line order (the later option named first).
    /// We mirror that here by recording the first selection and reporting a conflict the
    /// moment a different one is seen.
    cmd_mode: Option<CatFileCmdSelection>,
    path: Option<String>,
    positional: Vec<String>,
}

/// A recorded `OPT_CMDMODE` selection plus the spelling the user typed for it, so a later
/// conflict can be reported with git's exact option text (e.g. `-s` vs `--batch-all-objects`).
struct CatFileCmdSelection {
    mode: CatFileCmdMode,
    name: &'static str,
}

impl CatFileOptions {
    fn parse(args: &[String]) -> Result<Self> {
        let mut options = Self {
            batch: None,
            buffer: None,
            follow_symlinks: false,
            input_nul: false,
            output_nul: false,
            filter: CatFileObjectsFilter::Disabled,
            cmd_mode: None,
            path: None,
            positional: Vec::new(),
        };
        let mut args = GitArgCursor::new(args);
        while let Some(arg) = args.next() {
            if let Some(option) = LongOption::parse(arg) {
                match option.name() {
                    "use-mailmap" | "no-use-mailmap" | "mailmap" | "no-mailmap" => {
                        if option.has_value() {
                            return option_takes_no_value(option.name());
                        }
                    }
                    "batch" => options.set_batch_mode(
                        CatFileBatchMode::Batch,
                        option.optional_value().value().map(str::to_string),
                    )?,
                    "batch-check" => options.set_batch_mode(
                        CatFileBatchMode::BatchCheck,
                        option.optional_value().value().map(str::to_string),
                    )?,
                    "batch-command" => options.set_batch_mode(
                        CatFileBatchMode::Command,
                        option.optional_value().value().map(str::to_string),
                    )?,
                    "textconv" => {
                        if option.has_value() {
                            return option_takes_no_value("textconv");
                        }
                        options.set_cmd_mode(CatFileCmdMode::Textconv, "--textconv")?;
                    }
                    "filters" => {
                        if option.has_value() {
                            return option_takes_no_value("filters");
                        }
                        options.set_cmd_mode(CatFileCmdMode::Filters, "--filters")?;
                    }
                    "batch-all-objects" => {
                        if option.has_value() {
                            return option_takes_no_value("batch-all-objects");
                        }
                        options.set_cmd_mode(
                            CatFileCmdMode::BatchAllObjects,
                            "--batch-all-objects",
                        )?;
                    }
                    "buffer" => {
                        if option.has_value() {
                            return option_takes_no_value("buffer");
                        }
                        options.buffer = Some(true);
                    }
                    "no-buffer" => {
                        if option.has_value() {
                            return option_takes_no_value("no-buffer");
                        }
                        options.buffer = Some(false);
                    }
                    "unordered" | "no-unordered" => {
                        if option.has_value() {
                            return option_takes_no_value(option.name());
                        }
                    }
                    "follow-symlinks" => {
                        if option.has_value() {
                            return option_takes_no_value("follow-symlinks");
                        }
                        options.follow_symlinks = true;
                    }
                    "no-follow-symlinks" => {
                        if option.has_value() {
                            return option_takes_no_value("no-follow-symlinks");
                        }
                        options.follow_symlinks = false;
                    }
                    "path" => {
                        let value = match option.value() {
                            Some(value) => value,
                            None => args.next_required_value(|| switch_requires_value("path"))?,
                        };
                        options.path = Some(value.to_string());
                    }
                    "filter" => {
                        let value = match option.value() {
                            Some(value) => value,
                            None => args.next_required_value(|| switch_requires_value("filter"))?,
                        };
                        options.filter = CatFileObjectsFilter::parse(value)?;
                    }
                    "no-filter" => {
                        if option.has_value() {
                            return option_takes_no_value("no-filter");
                        }
                        options.filter = CatFileObjectsFilter::Disabled;
                    }
                    _ => options.positional.push(arg.to_string()),
                }
                continue;
            }
            match arg {
                "-e" => options.set_cmd_mode(CatFileCmdMode::Exists, "-e")?,
                "-t" => options.set_cmd_mode(CatFileCmdMode::Type, "-t")?,
                "-s" => options.set_cmd_mode(CatFileCmdMode::Size, "-s")?,
                "-p" => options.set_cmd_mode(CatFileCmdMode::Pretty, "-p")?,
                "-z" => options.input_nul = true,
                "-Z" => {
                    options.input_nul = true;
                    options.output_nul = true;
                }
                value => options.positional.push(value.to_string()),
            }
        }
        Ok(options)
    }

    fn set_batch_mode(&mut self, mode: CatFileBatchMode, format: Option<String>) -> Result<()> {
        if self.batch.replace((mode, format)).is_some() {
            return cat_file_only_one_batch_option();
        }
        Ok(())
    }

    fn set_cmd_mode(&mut self, new: CatFileCmdMode, name: &'static str) -> Result<()> {
        match &self.cmd_mode {
            Some(existing) if existing.mode != new => {
                // `parse_options` names the option currently being parsed first and the
                // previously-recorded one second.
                cat_file_cannot_use_together(name, existing.name)
            }
            Some(_) => Ok(()),
            None => {
                self.cmd_mode = Some(CatFileCmdSelection { mode: new, name });
                Ok(())
            }
        }
    }

    fn into_invocation(self) -> Result<CatFileInvocation> {
        let cmd_mode = self.cmd_mode.as_ref().map(|selection| selection.mode);
        let opt_cw = matches!(
            cmd_mode,
            Some(CatFileCmdMode::Textconv | CatFileCmdMode::Filters)
        );
        let batch_all_objects = cmd_mode == Some(CatFileCmdMode::BatchAllObjects);

        // Object filter compatibility, checked by upstream immediately after option parsing
        // (before `--path` and the batch-mode-only diagnostics). The filter kinds `cat-file`
        // does not implement (`sparse:oid`, `tree`) are rejected here with `usage:`; the
        // implemented kinds require a batch mode.
        match &self.filter {
            CatFileObjectsFilter::Disabled => {}
            CatFileObjectsFilter::Unsupported(name) => {
                return cat_file_objects_filter_unsupported(name);
            }
            _ => {
                if self.batch.is_none() {
                    return cat_file_objects_filter_only_in_batch_mode();
                }
            }
        }

        // `--path` requires `--textconv`/`--filters`; checked before the batch-mode and
        // argument-count diagnostics by upstream.
        if self.path.is_some() && !opt_cw {
            return cat_file_path_needs_filters_or_textconv();
        }

        // Option compatibility with batch mode: each of these is only valid alongside a
        // batch mode, and upstream checks them in exactly this order.
        if self.batch.is_none() {
            if self.follow_symlinks {
                return cat_file_requires_batch_mode("--follow-symlinks");
            }
            if self.buffer.is_some() {
                return cat_file_requires_batch_mode("--buffer");
            }
            if batch_all_objects {
                return cat_file_requires_batch_mode("--batch-all-objects");
            }
            if self.input_nul && !self.output_nul {
                return cat_file_requires_batch_mode("-z");
            }
            if self.output_nul {
                return cat_file_requires_batch_mode("-Z");
            }
        }

        if let Some((mode, format)) = self.batch {
            // In batch mode a non-textconv/filters command mode is rejected; `-b`
            // (`--batch-all-objects`) is permitted and folded into the batch request.
            if let Some(selection) = self.cmd_mode.as_ref()
                && !opt_cw
                && !batch_all_objects
            {
                return cat_file_incompatible_with_batch_mode(selection.name);
            }
            if !self.positional.is_empty() {
                return cat_file_batch_modes_take_no_arguments();
            }
            return Ok(CatFileInvocation::Batch(CatFileBatchRequest {
                mode,
                format,
                input_nul: self.input_nul,
                output_nul: self.output_nul,
                batch_all_objects,
                // Upstream defaults `--buffer` to on when `--batch-all-objects` is in effect,
                // off otherwise; an explicit `--[no-]buffer` overrides.
                buffer: self.buffer.unwrap_or(batch_all_objects),
                filter: self.filter,
            }));
        }

        if let Some(selection) = self.cmd_mode {
            let mode = selection.mode;
            if matches!(mode, CatFileCmdMode::BatchAllObjects) {
                // Reachable only without a batch mode; already diagnosed above.
                return cat_file_requires_batch_mode("--batch-all-objects");
            }
            match self.positional.len() {
                0 => {
                    return match mode {
                        CatFileCmdMode::Textconv => {
                            cat_file_rev_required_with("--textconv")
                        }
                        CatFileCmdMode::Filters => cat_file_rev_required_with("--filters"),
                        _ => cat_file_object_required_with(selection.name),
                    };
                }
                1 => {}
                _ => return cat_file_too_many_arguments(),
            }
            return Ok(CatFileInvocation::Object(CatFileObjectRequest {
                mode: CatFileObjectMode::Command(mode),
                object_name: self.positional[0].clone(),
            }));
        }

        // `<type> <object>` mode: exactly two positional arguments are required.
        match self.positional.len() {
            0 => return cat_file_bare_usage(),
            2 => {}
            other => return cat_file_two_arguments_required(other),
        }
        let object_type = match self.positional[0].parse::<ObjectType>() {
            Ok(object_type) => object_type,
            Err(_) => return cat_file_unknown_type(&self.positional[0], &self.positional[1]),
        };
        Ok(CatFileInvocation::Object(CatFileObjectRequest {
            mode: CatFileObjectMode::Typed(object_type),
            object_name: self.positional[1].clone(),
        }))
    }
}

struct CatFileObjectRequest {
    mode: CatFileObjectMode,
    object_name: String,
}

impl CatFileObjectRequest {
    fn execute(self) -> Result<()> {
        let view = RepositoryObjectView::discover()?;
        let query = ObjectQuery {
            view: &view,
            name: &self.object_name,
        };
        match self.mode {
            CatFileObjectMode::Command(mode) => query.print_command_mode(mode),
            CatFileObjectMode::Typed(object_type) => query.print_typed_body(object_type),
        }
    }
}

enum CatFileObjectMode {
    Command(CatFileCmdMode),
    Typed(ObjectType),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CatFileCmdMode {
    Exists,
    Type,
    Size,
    Pretty,
    Textconv,
    Filters,
    /// `--batch-all-objects`. Upstream declares this as an `OPT_CMDMODE` (value `'b'`)
    /// sharing the same slot as `-e/-p/-t/-s/--textconv/--filters`, so it participates in
    /// the "cannot be used together" conflict detection. It never reaches the object
    /// execution path: `into_invocation` either folds it into a batch request or rejects it
    /// with "requires a batch mode".
    BatchAllObjects,
}

impl CatFileCmdMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Exists => "-e",
            Self::Type => "-t",
            Self::Size => "-s",
            Self::Pretty => "-p",
            Self::Textconv => "--textconv",
            Self::Filters => "--filters",
            Self::BatchAllObjects => "--batch-all-objects",
        }
    }
}

struct RepositoryObjectView {
    repo: RepositoryContext,
}

impl RepositoryObjectView {
    fn discover() -> Result<Self> {
        Ok(Self {
            repo: RepositoryContext::discover_current()?,
        })
    }

    fn common_git_dir(&self) -> &Path {
        self.repo.common_git_dir()
    }

    fn format(&self) -> ObjectFormat {
        self.repo.format()
    }

    fn db(&self) -> &FileObjectDatabase {
        self.repo.objects()
    }

    fn resolve(&self, name: &str) -> Result<ObjectId> {
        self.repo.resolve_revision(name)
    }

    fn replacement_oid(&self, oid: &ObjectId) -> Result<ObjectId> {
        apply_replace_object(self.repo.refs(), oid)
    }

    fn resolve_path(&self, rev: &str, path: &str) -> Result<sley_rev::ResolvedTreePath> {
        self.repo.resolve_path(rev, path)
    }

    fn all_object_ids(&self) -> Result<Vec<ObjectId>> {
        self.db().object_ids()
    }

    fn resolve_object_name(&self, name: &str) -> Result<ObjectId> {
        let format = self.format();
        if name.len() == format.hex_len() && name.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return ObjectId::from_hex(format, name);
        }
        self.resolve(name)
    }
}

struct ObjectQuery<'a> {
    view: &'a RepositoryObjectView,
    name: &'a str,
}

impl ObjectQuery<'_> {
    /// Resolve `self.name` to an object id the way upstream's `get_oid_with_context` does for
    /// the `cmd_object` path: a full-length hex string is accepted syntactically (its object
    /// need not exist yet), while any other spelling is resolved against the object database
    /// and "Not a valid object name" if it does not resolve. The boolean reports whether the
    /// name was a full-length hex oid, which downstream uses to pick git's exact error text
    /// for a missing object (info-lookup failure vs. name-resolution failure).
    fn resolve_command_oid(&self) -> Result<(ObjectId, bool)> {
        let format = self.view.format();
        if self.name.len() == format.hex_len()
            && self.name.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Ok((ObjectId::from_hex(format, self.name)?, true));
        }
        match self.view.resolve(self.name) {
            Ok(oid) => Ok((oid, false)),
            Err(_) => cat_file_not_a_valid_object_name(self.name),
        }
    }

    fn print_command_mode(&self, mode: CatFileCmdMode) -> Result<()> {
        match mode {
            CatFileCmdMode::Exists => self.print_exists(),
            CatFileCmdMode::Type => self.print_type(),
            CatFileCmdMode::Size => self.print_size(),
            CatFileCmdMode::Pretty => self.print_pretty(),
            CatFileCmdMode::Textconv | CatFileCmdMode::Filters => Err(GitError::Unsupported(
                format!("cat-file {} is not supported yet", mode.as_str()),
            )),
            // Never reaches execution: `--batch-all-objects` is folded into a batch
            // request or rejected during option validation.
            CatFileCmdMode::BatchAllObjects => unreachable!(
                "--batch-all-objects is handled during option validation, not execution"
            ),
        }
    }

    /// `-e`: existence only. Upstream uses `odb_has_object`, which never parses the object, so
    /// a structurally broken object (bogus type header) still counts as present.
    fn print_exists(&self) -> Result<()> {
        let (oid, _) = self.resolve_command_oid()?;
        let read_oid = self.view.replacement_oid(&oid)?;
        if self.view.db().contains(&read_oid)? {
            return Ok(());
        }
        // Upstream's `-e` exits 1 with no message when the object is absent.
        Err(GitError::Exit(1))
    }

    /// `-t` / `-s`: read only object info. A missing full-hex oid yields "could not get object
    /// info"; a broken header yields the object-file diagnostics (invalid type / too-long).
    fn print_header_field(&self, want_type: bool) -> Result<()> {
        let (oid, is_full_hex) = self.resolve_command_oid()?;
        let read_oid = self.view.replacement_oid(&oid)?;
        match self.view.db().read_object_header(&read_oid) {
            Ok(Some((object_type, size))) => {
                if want_type {
                    println!("{}", object_type.as_str());
                } else {
                    println!("{size}");
                }
                Ok(())
            }
            // A full-length hex oid resolves syntactically, so a missing object is reported by
            // the object-info lookup failing; an abbreviated/named spelling that does not match
            // any object was already rejected during resolution.
            Ok(None) => {
                if is_full_hex {
                    cat_file_could_not_get_object_info()
                } else {
                    cat_file_not_a_valid_object_name(self.name)
                }
            }
            Err(err) => cat_file_object_info_error(&err, &read_oid, CatFileObjectInfoUser::Header),
        }
    }

    fn print_type(&self) -> Result<()> {
        self.print_header_field(true)
    }

    fn print_size(&self) -> Result<()> {
        self.print_header_field(false)
    }

    /// `-p`: read object info, then emit the body (pretty-printing a tree). Upstream routes a
    /// missing object or unreadable header through "Not a valid object name".
    fn print_pretty(&self) -> Result<()> {
        let (oid, _) = self.resolve_command_oid()?;
        let read_oid = self.view.replacement_oid(&oid)?;
        let object = match self.view.db().read_object(&read_oid) {
            Ok(object) => object,
            Err(GitError::NotFound(_)) => return cat_file_not_a_valid_object_name(self.name),
            Err(err) => {
                return cat_file_object_info_error(
                    &err,
                    &read_oid,
                    CatFileObjectInfoUser::Pretty { name: self.name },
                );
            }
        };
        if object.object_type == ObjectType::Tree {
            print_tree(
                None,
                self.view.format(),
                &object.body,
                TreePrintOptions {
                    name_only: false,
                    object_only: false,
                    long: false,
                    show_trees: false,
                    tree_only: false,
                    oid_abbrev: None,
                    format_spec: None,
                    nul: false,
                },
            )?;
        } else {
            io::stdout().write_all(&object.body)?;
            io::stdout().flush()?;
        }
        Ok(())
    }

    fn print_typed_body(&self, object_type: ObjectType) -> Result<()> {
        let oid = self.view.resolve(self.name)?;
        let oid = self.view.replacement_oid(&oid)?;
        let oid = match object_type {
            ObjectType::Blob => sley_rev::peel_tags(self.view.db(), self.view.format(), &oid)?,
            ObjectType::Tree => sley_rev::peel_to_tree(self.view.db(), self.view.format(), &oid)?,
            ObjectType::Commit => {
                sley_rev::peel_to_commit(self.view.db(), self.view.format(), &oid)?
            }
            ObjectType::Tag => oid,
        };
        let object = self.view.db().read_object(&oid)?;
        if object.object_type != object_type {
            eprintln!("fatal: git cat-file {}: bad file", self.name);
            return Err(GitError::Exit(128));
        }
        io::stdout().write_all(&object.body)?;
        io::stdout().flush()?;
        Ok(())
    }
}

struct CatFileBatchRequest {
    mode: CatFileBatchMode,
    format: Option<String>,
    input_nul: bool,
    output_nul: bool,
    batch_all_objects: bool,
    buffer: bool,
    filter: CatFileObjectsFilter,
}

/// The `--filter=<spec>` object filter for batch mode. Only the kinds upstream `cat-file`
/// actually implements are represented as live variants; `sparse:oid`/`tree` parse but are
/// recorded as `Unsupported` so the post-parse compatibility check can reject them with the
/// canonical name (matching `list_object_filter_config_name`).
#[derive(Clone, Copy)]
enum CatFileObjectsFilter {
    Disabled,
    BlobNone,
    BlobLimit(u64),
    ObjectType(ObjectType),
    /// A parseable-but-unimplemented filter kind, carrying the config name upstream prints
    /// in `objects filter not supported: '<name>'` (e.g. `sparse:oid`, `tree`).
    Unsupported(&'static str),
}

impl CatFileObjectsFilter {
    /// Parse a `--filter=<spec>` argument, mirroring `gently_parse_list_objects_filter`. The
    /// hard-error cases (`fatal:`, exit 128) are raised here; the soft-reject cases (parse OK
    /// but unimplemented) are deferred to the post-parse compatibility check via `Unsupported`.
    fn parse(spec: &str) -> Result<Self> {
        if spec == "blob:none" {
            return Ok(Self::BlobNone);
        }
        if let Some(value) = spec.strip_prefix("blob:limit=") {
            return match git_parse_ulong(value) {
                Some(limit) => Ok(Self::BlobLimit(limit)),
                None => cat_file_invalid_filter_spec(spec),
            };
        }
        if let Some(value) = spec.strip_prefix("tree:") {
            // `tree:<depth>` parses (when the depth is numeric) but `cat-file` does not
            // implement it; an unparseable depth is `invalid filter-spec`.
            return match git_parse_ulong(value) {
                Some(_) => Ok(Self::Unsupported("tree")),
                None => cat_file_expected_tree_depth(),
            };
        }
        if spec.starts_with("sparse:oid=") {
            return Ok(Self::Unsupported("sparse:oid"));
        }
        if spec.starts_with("sparse:path=") {
            return cat_file_sparse_path_dropped();
        }
        if let Some(value) = spec.strip_prefix("object:type=") {
            return match value.parse::<ObjectType>() {
                Ok(object_type) => Ok(Self::ObjectType(object_type)),
                Err(_) => cat_file_invalid_object_type_filter(value),
            };
        }
        cat_file_invalid_filter_spec(spec)
    }

    fn is_enabled(self) -> bool {
        !matches!(self, Self::Disabled)
    }

    /// Whether an object of the given type/size is EXCLUDED by this filter. Mirrors the
    /// per-object switch in upstream `batch_object_write`.
    fn excludes(self, object_type: ObjectType, size: u64) -> bool {
        match self {
            Self::Disabled | Self::Unsupported(_) => false,
            Self::BlobNone => object_type == ObjectType::Blob,
            Self::BlobLimit(limit) => object_type == ObjectType::Blob && size >= limit,
            Self::ObjectType(wanted) => object_type != wanted,
        }
    }
}

/// `git_parse_ulong`: a base-0 integer with an optional case-insensitive `k`/`m`/`g` suffix
/// (1024-scaled). Returns `None` on overflow or a malformed value, exactly like the C helper
/// that backs `blob:limit=<n>`.
fn git_parse_ulong(value: &str) -> Option<u64> {
    if value.is_empty() || value.contains('-') {
        return None;
    }
    let (digits, factor) = match value.as_bytes()[value.len() - 1] {
        b'k' | b'K' => (&value[..value.len() - 1], 1024u64),
        b'm' | b'M' => (&value[..value.len() - 1], 1024 * 1024),
        b'g' | b'G' => (&value[..value.len() - 1], 1024 * 1024 * 1024),
        _ => (value, 1),
    };
    let base = parse_git_unsigned_base0(digits)?;
    base.checked_mul(factor)
}

/// Parse an unsigned integer the way C's `strtoumax(value, &end, 0)` does for the leading
/// numeric run: hex (`0x`), octal (`0`), or decimal. The whole string must be consumed.
fn parse_git_unsigned_base0(value: &str) -> Option<u64> {
    if let Some(hex) = value.strip_prefix("0x").or_else(|| value.strip_prefix("0X")) {
        return u64::from_str_radix(hex, 16).ok();
    }
    if value.len() > 1 && value.starts_with('0') {
        return u64::from_str_radix(&value[1..], 8).ok();
    }
    value.parse::<u64>().ok()
}

impl CatFileBatchRequest {
    fn execute(self) -> Result<()> {
        match self.mode {
            CatFileBatchMode::Batch => self.run_batch(false),
            CatFileBatchMode::BatchCheck => self.run_batch(true),
            CatFileBatchMode::Command if self.batch_all_objects => self.run_batch(true),
            CatFileBatchMode::Command => self.run_command(),
        }
    }

    /// `apply_replace` for batch mode: `--batch-all-objects` enumerates the raw object set and
    /// upstream never rewrites those oids through the replace mechanism, so replace is honoured
    /// only for stdin-driven queries.
    fn apply_replace(&self, view: &RepositoryObjectView) -> Result<bool> {
        if self.batch_all_objects {
            return Ok(false);
        }
        replace_objects_active(view.repo.refs())
    }

    fn run_batch(&self, check_only: bool) -> Result<()> {
        let view = RepositoryObjectView::discover()?;
        let apply_replace = self.apply_replace(&view)?;
        let batch_format = self.format.as_deref().map(CatFileBatchFormat::parse);
        let mut stdout = io::stdout();
        let terminator = if self.output_nul { b'\0' } else { b'\n' };
        let emit = |stdout: &mut io::Stdout, line: &str| -> Result<()> {
            let (object_name, rest) = cat_file_batch_input(line, batch_format.as_ref());
            print_cat_file_batch_record(
                stdout,
                CatFileBatchRecord {
                    view: &view,
                    object_name,
                    rest,
                    batch_format: batch_format.as_ref(),
                    check_only,
                    terminator,
                    apply_replace,
                    filter: self.filter,
                    all_objects: self.batch_all_objects,
                },
            )?;
            // Upstream's `batch_write` goes straight to the fd (unbuffered) unless `--buffer`
            // is in effect, so each record is visible immediately; replicate by flushing per
            // record when not buffering.
            if !self.buffer {
                stdout.flush()?;
            }
            Ok(())
        };
        if self.batch_all_objects {
            for oid in view.all_object_ids()? {
                emit(&mut stdout, &oid.to_string())?;
            }
        } else {
            // Stream stdin record-by-record so each response is emitted before the next read,
            // matching git's getdelim loop. Reading all of stdin up front would deadlock the
            // interactive callers that wait for a response before closing the pipe.
            let input_delim = if self.input_nul { b'\0' } else { b'\n' };
            let stdin = io::stdin();
            let mut reader = stdin.lock();
            // Strip a trailing CR only in line mode (NUL-framed records are taken verbatim).
            while let Some(line) = read_batch_record(&mut reader, input_delim, !self.input_nul)? {
                emit(&mut stdout, &line)?;
            }
        }
        stdout.flush()?;
        Ok(())
    }

    fn run_command(&self) -> Result<()> {
        let view = RepositoryObjectView::discover()?;
        let apply_replace = self.apply_replace(&view)?;
        let batch_format = self.format.as_deref().map(CatFileBatchFormat::parse);
        let mut stdout = io::stdout();
        let terminator = if self.output_nul { b'\0' } else { b'\n' };
        // In `--buffer` mode, `info`/`contents` commands are queued and only emitted when a
        // `flush` command is read (upstream `batch_objects_command`). Without `--buffer` each
        // command runs immediately. The flush-on-exit is suppressed by the test-only env var
        // `GIT_TEST_CAT_FILE_NO_FLUSH_ON_EXIT`. Queued lines are owned because the borrow of
        // the input buffer cannot outlive a streaming read.
        let mut queued: Vec<OwnedBatchCommand> = Vec::new();
        // `--batch-command` always reads with CRLF-stripping getdelim; stream so an unbuffered
        // `info`/`contents` response is emitted before the next line is read.
        let input_delim = if self.input_nul { b'\0' } else { b'\n' };
        let stdin = io::stdin();
        let mut reader = stdin.lock();
        while let Some(line) = read_batch_record(&mut reader, input_delim, !self.input_nul)? {
            let command = parse_batch_command(&line)?;
            match command {
                BatchCommand::Flush => {
                    if !self.buffer {
                        eprintln!("fatal: flush is only for --buffer mode");
                        return Err(GitError::Exit(128));
                    }
                    for queued_command in queued.drain(..) {
                        self.run_batch_command(
                            &mut stdout,
                            &view,
                            batch_format.as_ref(),
                            apply_replace,
                            terminator,
                            queued_command.as_ref(),
                        )?;
                    }
                    stdout.flush()?;
                }
                _ if self.buffer => queued.push(command.into_owned()),
                _ => {
                    self.run_batch_command(
                        &mut stdout,
                        &view,
                        batch_format.as_ref(),
                        apply_replace,
                        terminator,
                        command,
                    )?;
                    // Unbuffered: every record is visible immediately.
                    stdout.flush()?;
                }
            }
        }
        if self.buffer && !queued.is_empty() && !cat_file_no_flush_on_exit() {
            for queued_command in queued.drain(..) {
                self.run_batch_command(
                    &mut stdout,
                    &view,
                    batch_format.as_ref(),
                    apply_replace,
                    terminator,
                    queued_command.as_ref(),
                )?;
            }
        }
        stdout.flush()?;
        Ok(())
    }

    fn run_batch_command(
        &self,
        stdout: &mut io::Stdout,
        view: &RepositoryObjectView,
        batch_format: Option<&CatFileBatchFormat<'_>>,
        apply_replace: bool,
        terminator: u8,
        command: BatchCommand<'_>,
    ) -> Result<()> {
        let (object_name, check_only) = match command {
            BatchCommand::Info(name) => (name, true),
            BatchCommand::Contents(name) => (name, false),
            BatchCommand::Flush => return Ok(()),
        };
        print_cat_file_batch_record(
            stdout,
            CatFileBatchRecord {
                view,
                object_name,
                rest: "",
                batch_format,
                check_only,
                terminator,
                apply_replace,
                filter: self.filter,
                all_objects: false,
            },
        )
    }
}

/// One parsed `--batch-command` input line. Mirrors the `commands[]` table in
/// `batch_objects_command`: `info`/`contents` carry their argument, `flush` takes none.
enum BatchCommand<'a> {
    Info(&'a str),
    Contents(&'a str),
    Flush,
}

impl BatchCommand<'_> {
    /// Detach from the (streamed) input buffer so the command can be queued for a later flush.
    fn into_owned(self) -> OwnedBatchCommand {
        match self {
            BatchCommand::Info(name) => OwnedBatchCommand::Info(name.to_string()),
            BatchCommand::Contents(name) => OwnedBatchCommand::Contents(name.to_string()),
            BatchCommand::Flush => OwnedBatchCommand::Flush,
        }
    }
}

/// An owned `--batch-command` entry, used for the `--buffer` queue.
enum OwnedBatchCommand {
    Info(String),
    Contents(String),
    Flush,
}

impl OwnedBatchCommand {
    fn as_ref(&self) -> BatchCommand<'_> {
        match self {
            OwnedBatchCommand::Info(name) => BatchCommand::Info(name),
            OwnedBatchCommand::Contents(name) => BatchCommand::Contents(name),
            OwnedBatchCommand::Flush => BatchCommand::Flush,
        }
    }
}

/// Read one delimiter-terminated record from `reader`, returning `None` at EOF (so a trailing
/// delimiter does not yield a phantom empty record). Mirrors upstream's `strbuf_getdelim`
/// loop: the delimiter is stripped, and in line mode (`strip_cr`) a trailing `\r` is removed
/// too (the CRLF handling of `strbuf_getline_lf`/`strbuf_getdelim_strip_crlf`).
fn read_batch_record<R: BufRead>(
    reader: &mut R,
    delimiter: u8,
    strip_cr: bool,
) -> Result<Option<String>> {
    let mut buffer = Vec::new();
    let read = reader
        .read_until(delimiter, &mut buffer)
        .map_err(|err| GitError::Io(err.to_string()))?;
    if read == 0 {
        return Ok(None);
    }
    if buffer.last() == Some(&delimiter) {
        buffer.pop();
    }
    if strip_cr && buffer.last() == Some(&b'\r') {
        buffer.pop();
    }
    Ok(Some(String::from_utf8_lossy(&buffer).into_owned()))
}

/// Parse a single `--batch-command` line exactly as upstream's `batch_objects_command` does,
/// raising the same `fatal:` diagnostics (exit 128) for the malformed cases.
fn parse_batch_command(line: &str) -> Result<BatchCommand<'_>> {
    if line.is_empty() {
        eprintln!("fatal: empty command in input");
        return Err(GitError::Exit(128));
    }
    if line.starts_with(|ch: char| ch.is_ascii_whitespace()) {
        eprintln!("fatal: whitespace before command: '{line}'");
        return Err(GitError::Exit(128));
    }
    for (name, takes_args) in [("contents", true), ("info", true), ("flush", false)] {
        let Some(rest) = line.strip_prefix(name) else {
            continue;
        };
        if takes_args {
            // Upstream requires the byte right after the command name to be a literal space.
            let Some(arg) = rest.strip_prefix(' ') else {
                eprintln!("fatal: {name} requires arguments");
                return Err(GitError::Exit(128));
            };
            return Ok(match name {
                "info" => BatchCommand::Info(arg),
                _ => BatchCommand::Contents(arg),
            });
        }
        if !rest.is_empty() {
            eprintln!("fatal: {name} takes no arguments");
            return Err(GitError::Exit(128));
        }
        return Ok(BatchCommand::Flush);
    }
    eprintln!("fatal: unknown command: '{line}'");
    Err(GitError::Exit(128))
}

/// The test-only `GIT_TEST_CAT_FILE_NO_FLUSH_ON_EXIT` knob: when truthy, `--batch-command
/// --buffer` does NOT flush its queued commands on EOF (only an explicit `flush` emits them).
fn cat_file_no_flush_on_exit() -> bool {
    matches!(
        std::env::var("GIT_TEST_CAT_FILE_NO_FLUSH_ON_EXIT").as_deref(),
        Ok("1") | Ok("true") | Ok("yes") | Ok("on")
    )
}

#[derive(Clone, Copy)]
enum CatFileBatchMode {
    Batch,
    BatchCheck,
    Command,
}

struct CatFileBatchRecord<'a> {
    view: &'a RepositoryObjectView,
    object_name: &'a str,
    rest: &'a str,
    batch_format: Option<&'a CatFileBatchFormat<'a>>,
    check_only: bool,
    terminator: u8,
    apply_replace: bool,
    filter: CatFileObjectsFilter,
    /// True when the record comes from `--batch-all-objects`; upstream silently skips
    /// filter-excluded objects in that mode instead of emitting `<name> excluded`.
    all_objects: bool,
}

fn print_cat_file_batch_record(
    stdout: &mut io::Stdout,
    record: CatFileBatchRecord<'_>,
) -> Result<()> {
    let query = ObjectQuery {
        view: record.view,
        name: record.object_name,
    };
    let Ok(oid) = record.view.resolve_object_name(record.object_name) else {
        return report_object_missing(stdout, &record, &query);
    };
    let read_oid = if record.apply_replace {
        record.view.replacement_oid(&oid)?
    } else {
        oid
    };
    let object_mode = if record
        .batch_format
        .is_some_and(|format| format.needs_object_mode())
    {
        query.object_mode()?
    } else {
        None
    };
    if record.check_only {
        let Some((object_type, size)) = batch_object_header(
            stdout,
            &record,
            &query,
            record.view.db().read_object_header(&read_oid),
        )?
        else {
            return Ok(());
        };
        if record.filter.excludes(object_type, size) {
            return print_cat_file_excluded(stdout, &record);
        }
        return print_cat_file_batch_header(
            stdout,
            &record,
            &oid,
            object_type,
            size,
            object_mode.as_deref(),
        );
    }
    // For contents mode the filter decision is made from the object header alone, before the
    // (potentially large) body is read — mirroring upstream, which reads only object info to
    // classify and never streams an excluded object's content.
    if record.filter.is_enabled() {
        let Some((object_type, size)) = batch_object_header(
            stdout,
            &record,
            &query,
            record.view.db().read_object_header(&read_oid),
        )?
        else {
            return Ok(());
        };
        if record.filter.excludes(object_type, size) {
            return print_cat_file_excluded(stdout, &record);
        }
    }
    let object = match record.view.db().read_object(&read_oid) {
        Ok(object) => object,
        // A structurally broken object (unknown type header) aborts the whole batch with
        // `fatal: invalid object type`, exactly like upstream's `die` inside object-file
        // reading; a genuinely absent object is reported as `missing` (or `submodule`).
        Err(GitError::InvalidObject(message)) if message.starts_with("unknown object type") => {
            eprintln!("fatal: invalid object type");
            return Err(GitError::Exit(128));
        }
        Err(_) => return report_object_missing(stdout, &record, &query),
    };
    print_cat_file_batch_header(
        stdout,
        &record,
        &oid,
        object.object_type,
        object.body.len() as u64,
        object_mode.as_deref(),
    )?;
    stdout.write_all(&object.body)?;
    stdout.write_all(&[record.terminator])?;
    Ok(())
}

/// Classify a `read_object_header` result for batch mode. Returns `Some(header)` when the
/// object is present and readable; emits the `missing`/`submodule` status line (and returns
/// `None`) when the object is absent; and aborts the batch with `fatal: invalid object type`
/// when the object exists but has an unknown-type header (upstream's hard `die`).
fn batch_object_header(
    stdout: &mut io::Stdout,
    record: &CatFileBatchRecord<'_>,
    query: &ObjectQuery<'_>,
    result: Result<Option<(ObjectType, u64)>>,
) -> Result<Option<(ObjectType, u64)>> {
    match result {
        Ok(Some(header)) => Ok(Some(header)),
        Ok(None) => {
            report_object_missing(stdout, record, query)?;
            Ok(None)
        }
        Err(GitError::InvalidObject(message)) if message.starts_with("unknown object type") => {
            eprintln!("fatal: invalid object type");
            Err(GitError::Exit(128))
        }
        Err(_) => {
            report_object_missing(stdout, record, query)?;
            Ok(None)
        }
    }
}

/// Emit the absent-object status line. A `<rev>:<path>` spec that resolves to a gitlink reports
/// `<gitlink-oid> submodule` (the gitlink's recorded commit lives in the submodule, not here);
/// every other absent object reports `<name> missing`. Mirrors upstream's `report_object_status`
/// dispatch on `S_IFGITLINK`.
fn report_object_missing(
    stdout: &mut io::Stdout,
    record: &CatFileBatchRecord<'_>,
    query: &ObjectQuery<'_>,
) -> Result<()> {
    if let Some(submodule_oid) = query.submodule_oid() {
        write!(stdout, "{submodule_oid} submodule")?;
    } else {
        write!(stdout, "{} missing", record.object_name)?;
    }
    stdout.write_all(&[record.terminator])?;
    Ok(())
}

/// Report a filter-excluded object. With `--batch-all-objects` upstream simply omits it; for
/// stdin-driven queries it prints `<name> excluded<terminator>` (the input name, like the
/// `missing` status line).
fn print_cat_file_excluded(stdout: &mut io::Stdout, record: &CatFileBatchRecord<'_>) -> Result<()> {
    if record.all_objects {
        return Ok(());
    }
    write!(stdout, "{} excluded", record.object_name)?;
    stdout.write_all(&[record.terminator])?;
    Ok(())
}

impl ObjectQuery<'_> {
    fn object_mode(&self) -> Result<Option<String>> {
        let Some((rev, path)) = sley_rev::split_rev_path_spec(self.name) else {
            return Ok(None);
        };
        let entry = self.view.resolve_path(rev, path)?;
        Ok(entry.mode.map(|mode| format!("{mode:o}")))
    }

    /// If `self.name` is a `<rev>:<path>` spec that resolves to a gitlink (submodule) entry,
    /// return the gitlink's recorded oid. Upstream uses the resolved entry's `S_IFGITLINK`
    /// mode to report a missing gitlink target as `<oid> submodule` rather than `missing`.
    fn submodule_oid(&self) -> Option<ObjectId> {
        let (rev, path) = sley_rev::split_rev_path_spec(self.name)?;
        let entry = self.view.resolve_path(rev, path).ok()?;
        match entry.mode {
            Some(0o160000) => Some(entry.oid),
            _ => None,
        }
    }
}

fn print_cat_file_batch_header(
    stdout: &mut io::Stdout,
    record: &CatFileBatchRecord<'_>,
    oid: &ObjectId,
    object_type: ObjectType,
    size: u64,
    object_mode: Option<&str>,
) -> Result<()> {
    if let Some(batch_format) = record.batch_format {
        let storage = if batch_format.needs_storage() {
            Some(cat_file_object_storage(
                record.view.common_git_dir(),
                record.view.format(),
                oid,
            )?)
        } else {
            None
        };
        print_cat_file_batch_format(
            stdout,
            batch_format,
            CatFileBatchFormatValues {
                oid,
                object_type,
                object_size: size as usize,
                object_mode,
                storage: storage.as_ref(),
                rest: record.rest,
            },
        )?;
    } else {
        write!(stdout, "{} {} {}", oid, object_type.as_str(), size)?;
    }
    stdout.write_all(&[record.terminator])?;
    Ok(())
}

fn cat_file_batch_input<'a>(
    line: &'a str,
    batch_format: Option<&CatFileBatchFormat<'_>>,
) -> (&'a str, &'a str) {
    if batch_format.is_some_and(CatFileBatchFormat::needs_rest) {
        line.split_once(char::is_whitespace)
            .map(|(object_name, rest)| (object_name, rest.trim_start()))
            .unwrap_or((line, ""))
    } else {
        (line, "")
    }
}

struct CatFileBatchFormat<'a> {
    atoms: Vec<CatFileBatchAtom<'a>>,
}

impl<'a> CatFileBatchFormat<'a> {
    fn parse(format: &'a str) -> Self {
        let mut atoms = Vec::new();
        let mut cursor = 0;
        while let Some(start) = format[cursor..].find("%(") {
            let start = cursor + start;
            if cursor < start {
                atoms.push(CatFileBatchAtom::Literal(&format[cursor..start]));
            }
            let Some(end) = format[start + 2..].find(')') else {
                atoms.push(CatFileBatchAtom::Malformed);
                return Self { atoms };
            };
            let end = start + 2 + end;
            atoms.push(CatFileBatchAtom::Placeholder(&format[start + 2..end]));
            cursor = end + 1;
        }
        if cursor < format.len() {
            atoms.push(CatFileBatchAtom::Literal(&format[cursor..]));
        }
        Self { atoms }
    }

    fn needs_rest(&self) -> bool {
        self.has_placeholder("rest")
    }

    fn needs_object_mode(&self) -> bool {
        self.has_placeholder("objectmode")
    }

    fn needs_storage(&self) -> bool {
        self.has_placeholder("objectsize:disk") || self.has_placeholder("deltabase")
    }

    fn has_placeholder(&self, needle: &str) -> bool {
        self.atoms.iter().any(|atom| match atom {
            CatFileBatchAtom::Placeholder(value) => *value == needle,
            _ => false,
        })
    }
}

enum CatFileBatchAtom<'a> {
    Literal(&'a str),
    Placeholder(&'a str),
    Malformed,
}

struct CatFileBatchFormatValues<'a> {
    oid: &'a ObjectId,
    object_type: ObjectType,
    object_size: usize,
    object_mode: Option<&'a str>,
    storage: Option<&'a CatFileObjectStorage>,
    rest: &'a str,
}

fn print_cat_file_batch_format(
    stdout: &mut io::Stdout,
    format: &CatFileBatchFormat<'_>,
    values: CatFileBatchFormatValues<'_>,
) -> Result<()> {
    for atom in &format.atoms {
        let placeholder = match atom {
            CatFileBatchAtom::Literal(literal) => {
                stdout.write_all(literal.as_bytes())?;
                continue;
            }
            CatFileBatchAtom::Placeholder(placeholder) => placeholder,
            CatFileBatchAtom::Malformed => {
                return Err(GitError::Command(
                    "unterminated cat-file batch placeholder".into(),
                ));
            }
        };
        match *placeholder {
            "objectname" => write!(stdout, "{}", values.oid)?,
            "objecttype" => stdout.write_all(values.object_type.as_str().as_bytes())?,
            "objectsize" => write!(stdout, "{}", values.object_size)?,
            "objectmode" => stdout.write_all(values.object_mode.unwrap_or("").as_bytes())?,
            "objectsize:disk" => {
                let storage = values.storage.ok_or_else(|| {
                    GitError::Command("cat-file batch storage metadata was not loaded".into())
                })?;
                write!(stdout, "{}", storage.disk_size)?
            }
            "deltabase" => {
                let storage = values.storage.ok_or_else(|| {
                    GitError::Command("cat-file batch storage metadata was not loaded".into())
                })?;
                write!(stdout, "{}", storage.deltabase)?
            }
            "rest" => stdout.write_all(values.rest.as_bytes())?,
            other => {
                return Err(GitError::Command(format!(
                    "unsupported cat-file batch placeholder %({other})"
                )));
            }
        }
    }
    Ok(())
}

pub(crate) fn cat_file_all_object_ids(
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<Vec<ObjectId>> {
    FileObjectDatabase::from_git_dir(git_dir, format).object_ids()
}

pub(crate) type CatFileObjectStorage = ObjectStorageInfo;

pub(crate) fn cat_file_object_storage(
    git_dir: &Path,
    format: ObjectFormat,
    oid: &ObjectId,
) -> Result<CatFileObjectStorage> {
    FileObjectDatabase::from_git_dir(git_dir, format)
        .object_storage_info(oid)?
        .ok_or_else(|| {
            GitError::not_found(format!(
                "object {oid} storage metadata in {}",
                repository_objects_dir(git_dir).display()
            ))
        })
}

/// The `cat-file` usage block, byte-for-byte as upstream git 2.54 emits it (the rendered
/// `builtin_catfile_usage` + the option table). Appended after the `fatal:` line by every
/// diagnostic that upstream routes through `usage_msg_opt*`/`usage_with_options`.
const CAT_FILE_USAGE: &str = "\
usage: git cat-file <type> <object>
   or: git cat-file (-e | -p | -t | -s) <object>
   or: git cat-file (--textconv | --filters)
                    [<rev>:<path|tree-ish> | --path=<path|tree-ish> <rev>]
   or: git cat-file (--batch | --batch-check | --batch-command) [--batch-all-objects]
                    [--buffer] [--follow-symlinks] [--unordered]
                    [--textconv | --filters] [-Z]

Check object existence or emit object contents
    -e                    check if <object> exists
    -p                    pretty-print <object> content

Emit [broken] object attributes
    -t                    show object type (one of 'blob', 'tree', 'commit', 'tag', ...)
    -s                    show object size
    --[no-]use-mailmap    use mail map file
    --[no-]mailmap ...    alias of --use-mailmap

Batch objects requested on stdin (or --batch-all-objects)
    --batch[=<format>]    show full <object> or <rev> contents
    --batch-check[=<format>]
                          like --batch, but don't emit <contents>
    -Z                    stdin and stdout is NUL-terminated
    --batch-command[=<format>]
                          read commands from stdin
    --batch-all-objects   with --batch[-check]: ignores stdin, batches all known objects

Change or optimize batch output
    --[no-]buffer         buffer --batch output
    --[no-]follow-symlinks
                          follow in-tree symlinks
    --[no-]unordered      do not order objects before emitting them

Emit object (blob or tree) with conversion or filter (stand-alone, or with batch)
    --textconv            run textconv on object's content
    --filters             run filters on object's content
    --[no-]path blob|tree use a <path> for (--textconv | --filters); Not with 'batch'
    --[no-]filter <args>  object filtering

";

/// Print `fatal: <message>`, a blank line, then the usage block; exit 129. Mirrors git's
/// `usage_msg_opt`/`usage_msg_optf`.
fn cat_file_usage_msg<T>(message: &str) -> Result<T> {
    eprintln!("fatal: {message}\n");
    eprint!("{CAT_FILE_USAGE}");
    Err(GitError::Exit(129))
}

/// `parse_options`-style cmdmode conflict: no usage block, just the `error:` line. The
/// option being parsed is named first, the previously-recorded one second.
fn cat_file_cannot_use_together<T>(current: &str, previous: &str) -> Result<T> {
    eprintln!("error: options '{current}' and '{previous}' cannot be used together");
    Err(GitError::Exit(129))
}

fn cat_file_only_one_batch_option<T>() -> Result<T> {
    eprintln!("error: only one batch option may be specified");
    Err(GitError::Exit(129))
}

/// `fatal: Not a valid object name <name>` (exit 128). Upstream's `die` when
/// `get_oid_with_context` (or, for `-p`, the object-info read) fails to find the object.
fn cat_file_not_a_valid_object_name<T>(name: &str) -> Result<T> {
    eprintln!("fatal: Not a valid object name {name}");
    Err(GitError::Exit(128))
}

/// `fatal: git cat-file: could not get object info` (exit 128). Upstream's `die` for `-t`/`-s`
/// when `odb_read_object_info_extended` fails on an oid that resolved syntactically.
fn cat_file_could_not_get_object_info<T>() -> Result<T> {
    eprintln!("fatal: git cat-file: could not get object info");
    Err(GitError::Exit(128))
}

/// Which `cmd_object` path is mapping an object-info error, so the right trailing `fatal:` is
/// chosen after the shared `error:`-level object-file diagnostics.
enum CatFileObjectInfoUser<'a> {
    /// `-t` / `-s`: a failed info lookup ends in "could not get object info".
    Header,
    /// `-p`: a failed info lookup ends in "Not a valid object name <name>".
    Pretty { name: &'a str },
}

/// Map an object-database read error to upstream's `cmd_object` diagnostics. Today this covers
/// the unknown-type header (`fatal: invalid object type`); other errors propagate unchanged so
/// their existing diagnostics surface.
fn cat_file_object_info_error<T>(
    err: &GitError,
    _oid: &ObjectId,
    user: CatFileObjectInfoUser<'_>,
) -> Result<T> {
    if let GitError::InvalidObject(message) = err
        && message.starts_with("unknown object type")
    {
        // `parse_loose_header` sets the type to "invalid" and upstream dies with this exact,
        // oid-less message for both `-t`/`-s` and `-p`.
        eprintln!("fatal: invalid object type");
        return Err(GitError::Exit(128));
    }
    match user {
        // Other `-t`/`-s` errors keep their own diagnostics.
        CatFileObjectInfoUser::Header => Err(err.clone()),
        // `-p` reads object info via `odb_read_object_info`; any non-type failure becomes
        // "Not a valid object name" to mirror its single `die`.
        CatFileObjectInfoUser::Pretty { name } => cat_file_not_a_valid_object_name(name),
    }
}

/// `--filter=<spec>` parse failure: `fatal: invalid filter-spec '<spec>'` (exit 128, no usage
/// block). Mirrors the fall-through in `gently_parse_list_objects_filter`.
fn cat_file_invalid_filter_spec<T>(spec: &str) -> Result<T> {
    eprintln!("fatal: invalid filter-spec '{spec}'");
    Err(GitError::Exit(128))
}

/// `--filter=tree:<non-numeric>`: `fatal: expected 'tree:<depth>'` (exit 128).
fn cat_file_expected_tree_depth<T>() -> Result<T> {
    eprintln!("fatal: expected 'tree:<depth>'");
    Err(GitError::Exit(128))
}

/// `--filter=sparse:path=...`: `fatal: sparse:path filters support has been dropped` (exit 128).
fn cat_file_sparse_path_dropped<T>() -> Result<T> {
    eprintln!("fatal: sparse:path filters support has been dropped");
    Err(GitError::Exit(128))
}

/// `--filter=object:type=<bad>`: not a valid object type (exit 128).
fn cat_file_invalid_object_type_filter<T>(value: &str) -> Result<T> {
    eprintln!("fatal: '{value}' for 'object:type=<type>' is not a valid object type");
    Err(GitError::Exit(128))
}

/// A parseable-but-unimplemented filter kind: `usage: objects filter not supported: '<name>'`
/// (exit 129, no usage block — upstream's `usagef`).
fn cat_file_objects_filter_unsupported<T>(name: &str) -> Result<T> {
    eprintln!("usage: objects filter not supported: '{name}'");
    Err(GitError::Exit(129))
}

/// An implemented filter used outside batch mode: `usage: objects filter only supported in
/// batch mode` (exit 129, no usage block).
fn cat_file_objects_filter_only_in_batch_mode<T>() -> Result<T> {
    eprintln!("usage: objects filter only supported in batch mode");
    Err(GitError::Exit(129))
}

fn cat_file_path_needs_filters_or_textconv<T>() -> Result<T> {
    cat_file_usage_msg("'--path=<path|tree-ish>' needs '--filters' or '--textconv'")
}

fn cat_file_requires_batch_mode<T>(option: &str) -> Result<T> {
    cat_file_usage_msg(&format!("'{option}' requires a batch mode"))
}

fn cat_file_incompatible_with_batch_mode<T>(option: &str) -> Result<T> {
    cat_file_usage_msg(&format!("'{option}' is incompatible with batch mode"))
}

fn cat_file_batch_modes_take_no_arguments<T>() -> Result<T> {
    cat_file_usage_msg("batch modes take no arguments")
}

fn cat_file_rev_required_with<T>(option: &str) -> Result<T> {
    cat_file_usage_msg(&format!("<rev> required with '{option}'"))
}

fn cat_file_object_required_with<T>(option: &str) -> Result<T> {
    cat_file_usage_msg(&format!("<object> required with '{option}'"))
}

fn cat_file_too_many_arguments<T>() -> Result<T> {
    cat_file_usage_msg("too many arguments")
}

fn cat_file_two_arguments_required<T>(argc: usize) -> Result<T> {
    cat_file_usage_msg(&format!(
        "only two arguments allowed in <type> <object> mode, not {argc}"
    ))
}

/// The bare `git cat-file` (no command mode, no arguments) case: just the usage block,
/// with no `fatal:` line. Mirrors git's `usage_with_options`.
fn cat_file_bare_usage<T>() -> Result<T> {
    eprint!("{CAT_FILE_USAGE}");
    Err(GitError::Exit(129))
}

/// `<type> <object>` mode with an unrecognized type string. Upstream resolves the type via
/// `type_from_string`, which dies (exit 128) rather than emitting a usage error.
fn cat_file_unknown_type<T>(exp_type: &str, _obj_name: &str) -> Result<T> {
    eprintln!("fatal: invalid object type \"{exp_type}\"");
    Err(GitError::Exit(128))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incompatible_cmd_and_batch_modes_exit_129() {
        let args = vec!["-e".to_string(), "--batch".to_string()];
        assert!(matches!(
            CatFileInvocation::parse(&args),
            Err(GitError::Exit(129))
        ));
    }

    #[test]
    fn incompatible_cmd_modes_exit_129() {
        let args = vec!["-e".to_string(), "-p".to_string(), "HEAD".to_string()];
        assert!(matches!(
            CatFileInvocation::parse(&args),
            Err(GitError::Exit(129))
        ));
    }

    #[test]
    fn missing_object_argument_exits_129() {
        let args = vec!["-e".to_string()];
        assert!(matches!(
            CatFileInvocation::parse(&args),
            Err(GitError::Exit(129))
        ));
    }

    #[test]
    fn too_many_arguments_exit_129() {
        let args = vec!["-e".to_string(), "HEAD".to_string(), "extra".to_string()];
        assert!(matches!(
            CatFileInvocation::parse(&args),
            Err(GitError::Exit(129))
        ));
    }

    #[test]
    fn batch_mode_with_positional_argument_exits_129() {
        let args = vec!["--batch".to_string(), "HEAD".to_string()];
        assert!(matches!(
            CatFileInvocation::parse(&args),
            Err(GitError::Exit(129))
        ));
    }

    #[test]
    fn option_with_unexpected_value_exits_129() {
        let args = vec!["--textconv=value".to_string(), "HEAD".to_string()];
        assert!(matches!(
            CatFileInvocation::parse(&args),
            Err(GitError::Exit(129))
        ));
    }

    #[test]
    fn filter_unknown_spec_dies_128() {
        assert!(matches!(
            CatFileObjectsFilter::parse("unknown"),
            Err(GitError::Exit(128))
        ));
    }

    #[test]
    fn filter_sparse_path_dropped_dies_128() {
        assert!(matches!(
            CatFileObjectsFilter::parse("sparse:path=x"),
            Err(GitError::Exit(128))
        ));
    }

    #[test]
    fn filter_tree_non_numeric_dies_128() {
        assert!(matches!(
            CatFileObjectsFilter::parse("tree:notanumber"),
            Err(GitError::Exit(128))
        ));
    }

    #[test]
    fn filter_object_type_bad_dies_128() {
        assert!(matches!(
            CatFileObjectsFilter::parse("object:type=bogus"),
            Err(GitError::Exit(128))
        ));
    }

    #[test]
    fn filter_sparse_oid_and_tree_parse_as_unsupported() {
        assert!(matches!(
            CatFileObjectsFilter::parse("sparse:oid=1234"),
            Ok(CatFileObjectsFilter::Unsupported("sparse:oid"))
        ));
        assert!(matches!(
            CatFileObjectsFilter::parse("tree:1"),
            Ok(CatFileObjectsFilter::Unsupported("tree"))
        ));
    }

    #[test]
    fn filter_unsupported_outside_batch_reports_usage_129() {
        // `--filter=tree:1` with no batch mode: parseable but unimplemented -> 129.
        let args = vec!["--filter=tree:1".to_string()];
        assert!(matches!(
            CatFileInvocation::parse(&args),
            Err(GitError::Exit(129))
        ));
    }

    #[test]
    fn filter_supported_outside_batch_reports_usage_129() {
        // `--filter=blob:none` with no batch mode: only supported in batch mode -> 129.
        let args = vec!["--filter=blob:none".to_string()];
        assert!(matches!(
            CatFileInvocation::parse(&args),
            Err(GitError::Exit(129))
        ));
    }

    #[test]
    fn blob_limit_parses_units() {
        assert_eq!(git_parse_ulong("0"), Some(0));
        assert_eq!(git_parse_ulong("500"), Some(500));
        assert_eq!(git_parse_ulong("1k"), Some(1024));
        assert_eq!(git_parse_ulong("1K"), Some(1024));
        assert_eq!(git_parse_ulong("2m"), Some(2 * 1024 * 1024));
        assert_eq!(git_parse_ulong("1g"), Some(1024 * 1024 * 1024));
        assert_eq!(git_parse_ulong("0x10"), Some(16));
        assert_eq!(git_parse_ulong(""), None);
        assert_eq!(git_parse_ulong("-1"), None);
        assert_eq!(git_parse_ulong("12x"), None);
    }

    #[test]
    fn filter_excludes_matches_git_classification() {
        // blob:none excludes blobs only.
        let blob_none = CatFileObjectsFilter::BlobNone;
        assert!(blob_none.excludes(ObjectType::Blob, 10));
        assert!(!blob_none.excludes(ObjectType::Commit, 10));
        // blob:limit=N excludes blobs whose size is >= N.
        let blob_limit = CatFileObjectsFilter::BlobLimit(5);
        assert!(blob_limit.excludes(ObjectType::Blob, 5));
        assert!(!blob_limit.excludes(ObjectType::Blob, 4));
        assert!(!blob_limit.excludes(ObjectType::Tree, 100));
        // object:type=T excludes everything that is not T.
        let type_blob = CatFileObjectsFilter::ObjectType(ObjectType::Blob);
        assert!(!type_blob.excludes(ObjectType::Blob, 0));
        assert!(type_blob.excludes(ObjectType::Commit, 0));
        // Disabled / Unsupported never exclude.
        assert!(!CatFileObjectsFilter::Disabled.excludes(ObjectType::Blob, 0));
        assert!(!CatFileObjectsFilter::Unsupported("tree").excludes(ObjectType::Blob, 0));
    }

    #[test]
    fn batch_command_parse_diagnostics() {
        assert!(matches!(parse_batch_command(""), Err(GitError::Exit(128))));
        assert!(matches!(
            parse_batch_command(" info x"),
            Err(GitError::Exit(128))
        ));
        assert!(matches!(parse_batch_command("info"), Err(GitError::Exit(128))));
        assert!(matches!(
            parse_batch_command("flush x"),
            Err(GitError::Exit(128))
        ));
        assert!(matches!(
            parse_batch_command("bogus"),
            Err(GitError::Exit(128))
        ));
        assert!(matches!(
            parse_batch_command("info HEAD"),
            Ok(BatchCommand::Info("HEAD"))
        ));
        assert!(matches!(
            parse_batch_command("contents HEAD"),
            Ok(BatchCommand::Contents("HEAD"))
        ));
        assert!(matches!(parse_batch_command("flush"), Ok(BatchCommand::Flush)));
    }
}
