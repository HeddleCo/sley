//! `git cat-file`: inspect objects and run the batch object-query protocol.

use std::io::{self, Read, Write};
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
                buffer: self.buffer.unwrap_or(false),
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
    fn print_command_mode(&self, mode: CatFileCmdMode) -> Result<()> {
        let oid = self.view.resolve(self.name)?;
        let read_oid = self.view.replacement_oid(&oid)?;
        let object = self.view.db().read_object(&read_oid)?;
        match mode {
            CatFileCmdMode::Exists => {}
            CatFileCmdMode::Type => println!("{}", object.object_type.as_str()),
            CatFileCmdMode::Size => println!("{}", object.body.len()),
            CatFileCmdMode::Pretty => {
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
            }
            CatFileCmdMode::Textconv | CatFileCmdMode::Filters => {
                return Err(GitError::Unsupported(format!(
                    "cat-file {} is not supported yet",
                    mode.as_str()
                )));
            }
            // Never reaches execution: `--batch-all-objects` is folded into a batch
            // request or rejected during option validation.
            CatFileCmdMode::BatchAllObjects => unreachable!(
                "--batch-all-objects is handled during option validation, not execution"
            ),
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

    fn run_batch(&self, check_only: bool) -> Result<()> {
        let view = RepositoryObjectView::discover()?;
        let apply_replace = replace_objects_active(view.repo.refs())?;
        let batch_format = self.format.as_deref().map(CatFileBatchFormat::parse);
        let records = if self.batch_all_objects {
            view.all_object_ids()?
                .into_iter()
                .map(|oid| oid.to_string())
                .collect()
        } else {
            let mut input = Vec::new();
            io::stdin().read_to_end(&mut input)?;
            cat_file_batch_input_records(&input, self.input_nul)
        };
        let mut stdout = io::stdout();
        let terminator = if self.output_nul { b'\0' } else { b'\n' };
        for line in records {
            let (object_name, rest) = cat_file_batch_input(&line, batch_format.as_ref());
            print_cat_file_batch_record(
                &mut stdout,
                CatFileBatchRecord {
                    view: &view,
                    object_name,
                    rest,
                    batch_format: batch_format.as_ref(),
                    check_only,
                    terminator,
                    apply_replace,
                },
            )?;
        }
        stdout.flush()?;
        Ok(())
    }

    fn run_command(&self) -> Result<()> {
        let view = RepositoryObjectView::discover()?;
        let apply_replace = replace_objects_active(view.repo.refs())?;
        let batch_format = self.format.as_deref().map(CatFileBatchFormat::parse);
        let mut input = Vec::new();
        io::stdin().read_to_end(&mut input)?;
        let records = cat_file_batch_input_records(&input, self.input_nul);
        let mut stdout = io::stdout();
        let terminator = if self.output_nul { b'\0' } else { b'\n' };
        for line in records {
            if line == "flush" {
                if !self.buffer {
                    eprintln!("fatal: flush is only for --buffer mode");
                    return Err(GitError::Exit(128));
                }
                stdout.flush()?;
                continue;
            }
            if let Some(object_name) = line.strip_prefix("info ") {
                print_cat_file_batch_record(
                    &mut stdout,
                    CatFileBatchRecord {
                        view: &view,
                        object_name,
                        rest: "",
                        batch_format: batch_format.as_ref(),
                        check_only: true,
                        terminator,
                        apply_replace,
                    },
                )?;
                continue;
            }
            if let Some(object_name) = line.strip_prefix("contents ") {
                print_cat_file_batch_record(
                    &mut stdout,
                    CatFileBatchRecord {
                        view: &view,
                        object_name,
                        rest: "",
                        batch_format: batch_format.as_ref(),
                        check_only: false,
                        terminator,
                        apply_replace,
                    },
                )?;
                continue;
            }
            eprintln!("fatal: unknown command: '{line}'");
            return Err(GitError::Exit(128));
        }
        stdout.flush()?;
        Ok(())
    }
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
        write!(stdout, "{} missing", record.object_name)?;
        stdout.write_all(&[record.terminator])?;
        return Ok(());
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
        let Ok(Some((object_type, size))) = record.view.db().read_object_header(&read_oid) else {
            write!(stdout, "{} missing", record.object_name)?;
            stdout.write_all(&[record.terminator])?;
            return Ok(());
        };
        return print_cat_file_batch_header(
            stdout,
            &record,
            &oid,
            object_type,
            size,
            object_mode.as_deref(),
        );
    }
    let Ok(object) = record.view.db().read_object(&read_oid) else {
        write!(stdout, "{} missing", record.object_name)?;
        stdout.write_all(&[record.terminator])?;
        return Ok(());
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

impl ObjectQuery<'_> {
    fn object_mode(&self) -> Result<Option<String>> {
        let Some((rev, path)) = sley_rev::split_rev_path_spec(self.name) else {
            return Ok(None);
        };
        let entry = self.view.resolve_path(rev, path)?;
        Ok(entry.mode.map(|mode| format!("{mode:o}")))
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

fn cat_file_batch_input_records(input: &[u8], nul: bool) -> Vec<String> {
    if !nul {
        return String::from_utf8_lossy(input)
            .lines()
            .map(str::to_string)
            .collect();
    }
    let separator = if nul { b'\0' } else { b'\n' };
    input
        .split(|byte| *byte == separator)
        .filter(|record| !record.is_empty())
        .map(|record| String::from_utf8_lossy(record).into_owned())
        .collect()
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
}
