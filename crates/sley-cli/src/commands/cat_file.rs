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
    batch_all_objects: bool,
    buffer: bool,
    follow_symlinks: bool,
    input_nul: bool,
    output_nul: bool,
    cmd_mode: Option<CatFileCmdMode>,
    path: Option<String>,
    positional: Vec<String>,
}

impl CatFileOptions {
    fn parse(args: &[String]) -> Result<Self> {
        let mut options = Self {
            batch: None,
            batch_all_objects: false,
            buffer: false,
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
                        options.set_cmd_mode(CatFileCmdMode::Textconv)?;
                    }
                    "filters" => {
                        if option.has_value() {
                            return option_takes_no_value("filters");
                        }
                        options.set_cmd_mode(CatFileCmdMode::Filters)?;
                    }
                    "batch-all-objects" => {
                        if option.has_value() {
                            return option_takes_no_value("batch-all-objects");
                        }
                        options.batch_all_objects = true;
                    }
                    "buffer" => {
                        if option.has_value() {
                            return option_takes_no_value("buffer");
                        }
                        options.buffer = true;
                    }
                    "no-buffer" => {
                        if option.has_value() {
                            return option_takes_no_value("no-buffer");
                        }
                        options.buffer = false;
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
                "-e" => options.set_cmd_mode(CatFileCmdMode::Exists)?,
                "-t" => options.set_cmd_mode(CatFileCmdMode::Type)?,
                "-s" => options.set_cmd_mode(CatFileCmdMode::Size)?,
                "-p" => options.set_cmd_mode(CatFileCmdMode::Pretty)?,
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
            return cat_file_cannot_use_together("batch modes", mode.as_str());
        }
        Ok(())
    }

    fn set_cmd_mode(&mut self, new: CatFileCmdMode) -> Result<()> {
        if let Some(old) = self.cmd_mode.replace(new) {
            return cat_file_cannot_use_together(old.as_str(), new.as_str());
        }
        Ok(())
    }

    fn into_invocation(self) -> Result<CatFileInvocation> {
        if let Some((mode, format)) = self.batch {
            if let Some(cmd_mode) = self.cmd_mode {
                return cat_file_cannot_use_together(mode.as_str(), cmd_mode.as_str());
            }
            if self.path.is_some() {
                return cat_file_incompatible_usage("--path requires --textconv or --filters");
            }
            if !self.positional.is_empty() {
                return cat_file_too_many_arguments();
            }
            return Ok(CatFileInvocation::Batch(CatFileBatchRequest {
                mode,
                format,
                input_nul: self.input_nul,
                output_nul: self.output_nul,
                batch_all_objects: self.batch_all_objects,
                buffer: self.buffer,
            }));
        }
        if self.batch_all_objects {
            return cat_file_incompatible_usage("'--batch-all-objects' requires a batch mode");
        }
        if self.input_nul || self.output_nul {
            return cat_file_incompatible_usage("'-z' requires a batch mode");
        }
        if self.buffer {
            return cat_file_incompatible_usage("'--buffer' requires a batch mode");
        }
        if self.follow_symlinks {
            return cat_file_incompatible_usage("'--follow-symlinks' requires a batch mode");
        }
        if let Some(mode) = self.cmd_mode {
            if self.path.is_some()
                && !matches!(mode, CatFileCmdMode::Textconv | CatFileCmdMode::Filters)
            {
                return cat_file_incompatible_usage("--path is incompatible with this mode");
            }
            let object_name = single_object_argument(&self.positional)?;
            return Ok(CatFileInvocation::Object(CatFileObjectRequest {
                mode: CatFileObjectMode::Command(mode),
                object_name,
            }));
        }
        if self.path.is_some() {
            return cat_file_incompatible_usage("--path requires --textconv or --filters");
        }
        if self.positional.len() > 2 {
            return cat_file_too_many_arguments();
        }
        if self.positional.len() != 2 {
            return cat_file_missing_required_argument();
        }
        let Ok(object_type) = self.positional[0].parse::<ObjectType>() else {
            return cat_file_incompatible_usage("type argument must name an object type");
        };
        Ok(CatFileInvocation::Object(CatFileObjectRequest {
            mode: CatFileObjectMode::Typed(object_type),
            object_name: self.positional[1].clone(),
        }))
    }
}

fn single_object_argument(positional: &[String]) -> Result<String> {
    if positional.is_empty() {
        return cat_file_missing_required_argument();
    }
    if positional.len() > 1 {
        return cat_file_too_many_arguments();
    }
    Ok(positional[0].clone())
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

    fn resolve_path(&self, rev: &str, path: &str) -> Result<sley_rev::ResolvedTreePath> {
        self.repo.resolve_path(rev, path)
    }

    fn all_object_ids(&self) -> Result<Vec<ObjectId>> {
        self.db().object_ids()
    }
}

struct ObjectQuery<'a> {
    view: &'a RepositoryObjectView,
    name: &'a str,
}

impl ObjectQuery<'_> {
    fn print_command_mode(&self, mode: CatFileCmdMode) -> Result<()> {
        let oid = self.view.resolve(self.name)?;
        let object = self.view.db().read_object(&oid)?;
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
        }
        Ok(())
    }

    fn print_typed_body(&self, object_type: ObjectType) -> Result<()> {
        let oid = self.view.resolve(self.name)?;
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
                },
            )?;
        }
        stdout.flush()?;
        Ok(())
    }

    fn run_command(&self) -> Result<()> {
        let view = RepositoryObjectView::discover()?;
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

impl CatFileBatchMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Batch => "--batch",
            Self::BatchCheck => "--batch-check",
            Self::Command => "--batch-command",
        }
    }
}

struct CatFileBatchRecord<'a> {
    view: &'a RepositoryObjectView,
    object_name: &'a str,
    rest: &'a str,
    batch_format: Option<&'a CatFileBatchFormat<'a>>,
    check_only: bool,
    terminator: u8,
}

fn print_cat_file_batch_record(
    stdout: &mut io::Stdout,
    record: CatFileBatchRecord<'_>,
) -> Result<()> {
    let query = ObjectQuery {
        view: record.view,
        name: record.object_name,
    };
    let Ok(oid) = record.view.resolve(record.object_name) else {
        write!(stdout, "{} missing", record.object_name)?;
        stdout.write_all(&[record.terminator])?;
        return Ok(());
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
        let Ok(Some((object_type, size))) = record.view.db().read_object_header(&oid) else {
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
    let Ok(object) = record.view.db().read_object(&oid) else {
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
            oid,
            object_type,
            size as usize,
            object_mode,
            storage.as_ref(),
            record.rest,
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

fn print_cat_file_batch_format(
    stdout: &mut io::Stdout,
    format: &CatFileBatchFormat<'_>,
    oid: &ObjectId,
    object_type: ObjectType,
    object_size: usize,
    object_mode: Option<&str>,
    storage: Option<&CatFileObjectStorage>,
    rest: &str,
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
            "objectname" => write!(stdout, "{oid}")?,
            "objecttype" => stdout.write_all(object_type.as_str().as_bytes())?,
            "objectsize" => write!(stdout, "{object_size}")?,
            "objectmode" => stdout.write_all(object_mode.unwrap_or("").as_bytes())?,
            "objectsize:disk" => {
                let storage = storage.ok_or_else(|| {
                    GitError::Command("cat-file batch storage metadata was not loaded".into())
                })?;
                write!(stdout, "{}", storage.disk_size)?
            }
            "deltabase" => {
                let storage = storage.ok_or_else(|| {
                    GitError::Command("cat-file batch storage metadata was not loaded".into())
                })?;
                write!(stdout, "{}", storage.deltabase)?
            }
            "rest" => stdout.write_all(rest.as_bytes())?,
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
            GitError::NotFound(format!(
                "object {oid} storage metadata in {}",
                repository_objects_dir(git_dir).display()
            ))
        })
}

fn cat_file_cannot_use_together<T>(left: &str, right: &str) -> Result<T> {
    eprintln!("error: options '{left}' and '{right}' cannot be used together");
    Err(GitError::Exit(129))
}

fn cat_file_incompatible_usage<T>(message: &str) -> Result<T> {
    eprintln!("fatal: {message}");
    Err(GitError::Exit(129))
}

fn cat_file_missing_required_argument<T>() -> Result<T> {
    eprintln!("fatal: <object> required");
    Err(GitError::Exit(129))
}

fn cat_file_too_many_arguments<T>() -> Result<T> {
    eprintln!("fatal: too many arguments");
    Err(GitError::Exit(129))
}
