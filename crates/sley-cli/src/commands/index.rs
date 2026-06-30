//! Extracted from the crate root (sley#8 phase 1) — code motion only.

// A glob of the crate root brings every shared helper/type into scope via
// descendant-privacy; see commands::stash for the rationale.
use crate::*;

pub(crate) fn cmd_mktree(args: &[String]) -> Result<()> {
    let mut allow_missing = false;
    let mut nul = false;
    let mut batch = false;
    for arg in args {
        match arg.as_str() {
            "--missing" => allow_missing = true,
            "--no-missing" => allow_missing = false,
            "-z" => nul = true,
            "--batch" => batch = true,
            "--no-batch" => batch = false,
            value => {
                return Err(GitError::Command(format!(
                    "unsupported mktree option {value}"
                )));
            }
        }
    }
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let format = repository_object_format(&git_dir)?;
    let mut db = FileObjectDatabase::from_git_dir(&git_dir, format);
    let mut input = Vec::new();
    io::stdin().read_to_end(&mut input)?;
    let separator = if nul { b'\0' } else { b'\n' };
    let records = input.split(|byte| *byte == separator).collect::<Vec<_>>();
    let mut entries = Vec::new();
    for (idx, record) in records.iter().enumerate() {
        let is_trailing_empty = record.is_empty() && idx == records.len() - 1;
        if record.is_empty() {
            if is_trailing_empty {
                continue;
            }
            if !batch {
                eprintln!("fatal: input format error: (blank line only valid in batch mode)");
                return Err(GitError::Exit(128));
            }
            write_mktree_tree(&mut db, format, &mut entries)?;
            continue;
        }
        entries.push(parse_mktree_record(format, &db, record, allow_missing)?);
    }
    if !batch || !entries.is_empty() {
        write_mktree_tree(&mut db, format, &mut entries)?;
    }
    Ok(())
}

fn parse_mktree_record(
    format: ObjectFormat,
    db: &FileObjectDatabase,
    record: &[u8],
    allow_missing: bool,
) -> Result<TreeEntry> {
    let Some(mode_end) = record.iter().position(|byte| *byte == b' ') else {
        return mktree_input_format_error(record);
    };
    let mode_text = std::str::from_utf8(&record[..mode_end])
        .map_err(|_| GitError::InvalidFormat("mktree mode is not utf8".into()))?;
    let mode = u32::from_str_radix(mode_text, 8).map_err(|_| {
        eprintln!(
            "fatal: input format error: {}",
            String::from_utf8_lossy(record)
        );
        GitError::Exit(128)
    })?;
    let rest = &record[mode_end + 1..];
    let Some(type_end) = rest.iter().position(|byte| *byte == b' ') else {
        return mktree_input_format_error(record);
    };
    let requested_type_text = std::str::from_utf8(&rest[..type_end])
        .map_err(|_| GitError::InvalidFormat("mktree object type is not utf8".into()))?;
    let requested_type = requested_type_text.parse::<ObjectType>().map_err(|_| {
        eprintln!(
            "fatal: input format error: {}",
            String::from_utf8_lossy(record)
        );
        GitError::Exit(128)
    })?;
    let rest = &rest[type_end + 1..];
    let Some(oid_end) = rest.iter().position(|byte| *byte == b'\t') else {
        return mktree_input_format_error(record);
    };
    let oid_text = std::str::from_utf8(&rest[..oid_end])
        .map_err(|_| GitError::InvalidFormat("mktree object id is not utf8".into()))?;
    let oid = ObjectId::from_hex(format, oid_text).map_err(|_| {
        eprintln!(
            "fatal: input format error: {}",
            String::from_utf8_lossy(record)
        );
        GitError::Exit(128)
    })?;
    let name = rest[oid_end + 1..].to_vec();
    if name.is_empty() {
        return mktree_input_format_error(record);
    }
    let expected_type = mktree_mode_object_type(mode);
    if requested_type != expected_type {
        eprintln!(
            "fatal: entry '{}' object type ({}) doesn't match mode type ({})",
            String::from_utf8_lossy(&name),
            requested_type.as_str(),
            expected_type.as_str()
        );
        return Err(GitError::Exit(128));
    }
    if requested_type != ObjectType::Commit {
        match db.read_object(&oid) {
            Ok(object) => {
                if object.object_type != requested_type {
                    eprintln!(
                        "fatal: entry '{}' object {oid} is a {} but specified type was ({})",
                        String::from_utf8_lossy(&name),
                        object.object_type.as_str(),
                        requested_type.as_str()
                    );
                    return Err(GitError::Exit(128));
                }
            }
            Err(_) if allow_missing => {}
            Err(_) => {
                eprintln!(
                    "fatal: entry '{}' object {oid} is unavailable",
                    String::from_utf8_lossy(&name)
                );
                return Err(GitError::Exit(128));
            }
        }
    }
    Ok(TreeEntry {
        mode,
        name: BString::from(name),
        oid,
    })
}

fn mktree_input_format_error<T>(record: &[u8]) -> Result<T> {
    eprintln!(
        "fatal: input format error: {}",
        String::from_utf8_lossy(record)
    );
    Err(GitError::Exit(128))
}

fn mktree_mode_object_type(mode: u32) -> ObjectType {
    match mode {
        0o040000 => ObjectType::Tree,
        0o160000 => ObjectType::Commit,
        _ => ObjectType::Blob,
    }
}

fn write_mktree_tree(
    db: &mut FileObjectDatabase,
    _format: ObjectFormat,
    entries: &mut Vec<TreeEntry>,
) -> Result<()> {
    entries.sort_by_key(mktree_tree_sort_key);
    let tree = Tree {
        entries: std::mem::take(entries),
    };
    let oid = db.write_object(EncodedObject::new(ObjectType::Tree, tree.write()))?;
    println!("{oid}");
    Ok(())
}

fn mktree_tree_sort_key(entry: &TreeEntry) -> Vec<u8> {
    let mut key = entry.name.as_bytes().to_vec();
    if entry.mode == 0o040000 {
        key.push(b'/');
    }
    key
}

fn print_tree_recursive(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    body: &[u8],
    prefix: &[u8],
    options: TreePrintOptions<'_>,
) -> Result<()> {
    let stdout = io::stdout();
    let mut stdout = BufWriter::with_capacity(128 * 1024, stdout.lock());
    let mut path = prefix.to_vec();
    print_tree_recursive_to_writer(&mut stdout, db, format, body, &mut path, options)?;
    stdout.flush()?;
    Ok(())
}

fn print_tree_pathspecs(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    root_body: &[u8],
    pathspecs: &[String],
    path_context: &LsTreePathContext,
    recursive: bool,
    options: TreePrintOptions<'_>,
) -> Result<()> {
    let filter = LsTreePathspecFilter::new(pathspecs, path_context)?;
    if filter.matches_root_scope() {
        return if recursive {
            print_tree_recursive(db, format, root_body, b"", options)
        } else {
            print_tree(Some(db), format, root_body, options)
        };
    }
    let stdout = io::stdout();
    let mut stdout = BufWriter::with_capacity(128 * 1024, stdout.lock());
    let mut path = Vec::new();
    print_tree_pathspecs_to_writer(
        &mut stdout,
        db,
        format,
        root_body,
        &mut path,
        path_context,
        &filter,
        recursive,
        options,
    )?;
    stdout.flush()?;
    Ok(())
}

fn print_tree_pathspecs_to_writer(
    writer: &mut impl Write,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    body: &[u8],
    path: &mut Vec<u8>,
    path_context: &LsTreePathContext,
    filter: &LsTreePathspecFilter,
    recursive: bool,
    options: TreePrintOptions<'_>,
) -> Result<()> {
    for entry in TreeEntries::new(format, body) {
        let entry = entry?;
        let path_len = path.len();
        path.extend_from_slice(entry.name);
        let is_tree = entry.mode == 0o040000;

        if filter.should_print(path, is_tree, recursive, options) {
            let display_path = path_context.display_path_bytes(path);
            print_tree_entry_to_writer(writer, Some(db), &entry, &display_path, options)?;
        }

        if is_tree && filter.should_descend(path, recursive) {
            let object = db.read_object(&entry.oid)?;
            if object.object_type != ObjectType::Tree {
                return Err(GitError::InvalidObject(format!(
                    "expected tree {}, found {}",
                    entry.oid,
                    object.object_type.as_str()
                )));
            }
            path.push(b'/');
            print_tree_pathspecs_to_writer(
                writer,
                db,
                format,
                &object.body,
                path,
                path_context,
                filter,
                recursive,
                options,
            )?;
        }

        path.truncate(path_len);
    }
    Ok(())
}

fn print_ls_tree_current_scope(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    root_body: &[u8],
    path_context: &LsTreePathContext,
    recursive: bool,
    options: TreePrintOptions<'_>,
) -> Result<()> {
    if path_context.prefix.is_empty() {
        if recursive {
            print_tree_recursive(db, format, root_body, b"", options)
        } else {
            print_tree(Some(db), format, root_body, options)
        }
    } else {
        let components = path_context.prefix.split('/').collect::<Vec<_>>();
        let Some(entry) = find_tree_entry(db, format, root_body, &components)? else {
            return Ok(());
        };
        if entry.mode != 0o040000 {
            return Ok(());
        }
        let object = db.read_object(&entry.oid)?;
        if object.object_type != ObjectType::Tree {
            return Err(GitError::InvalidObject(format!(
                "expected tree {}, found {}",
                entry.oid,
                object.object_type.as_str()
            )));
        }
        let display_prefix = path_context.display_prefix();
        if recursive {
            print_tree_recursive(db, format, &object.body, display_prefix.as_bytes(), options)
        } else {
            print_tree_with_prefix(
                Some(db),
                format,
                &object.body,
                display_prefix.as_bytes(),
                options,
            )
        }
    }
}

struct LsTreePathContext {
    prefix: String,
    full_name: bool,
    cwd_depth: usize,
}

impl LsTreePathContext {
    fn new(cwd: &Path, git_dir: &Path, full_name: bool, full_tree: bool) -> Result<Self> {
        let prefix = if full_tree {
            String::new()
        } else {
            worktree_prefix(cwd, git_dir)?
                .trim_end_matches('/')
                .to_string()
        };
        let cwd_depth = path_component_count(prefix.as_bytes());
        Ok(Self {
            prefix,
            full_name,
            cwd_depth,
        })
    }

    fn display_prefix(&self) -> String {
        if self.full_name && !self.prefix.is_empty() {
            format!("{}/", self.prefix)
        } else {
            String::new()
        }
    }

    fn normalize_pathspec(&self, pathspec: &str) -> Result<String> {
        let mut components = self
            .prefix
            .split('/')
            .filter(|component| !component.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        normalize_ls_tree_pathspec_into(&mut components, pathspec)
    }

    fn display_path(&self, normalized: &str) -> String {
        if self.full_name || self.prefix.is_empty() {
            return normalized.to_string();
        }
        if normalized == self.prefix {
            return String::new();
        }
        if let Some(rest) = normalized
            .strip_prefix(self.prefix.as_str())
            .and_then(|rest| rest.strip_prefix('/'))
        {
            return rest.to_string();
        }
        let mut display = String::new();
        for _ in 0..self.cwd_depth {
            display.push_str("../");
        }
        display.push_str(normalized);
        display
    }

    fn display_path_bytes(&self, normalized: &[u8]) -> Vec<u8> {
        if self.full_name || self.prefix.is_empty() {
            return normalized.to_vec();
        }
        let prefix = self.prefix.as_bytes();
        if normalized == prefix {
            return Vec::new();
        }
        if normalized.len() > prefix.len()
            && normalized.starts_with(prefix)
            && normalized[prefix.len()] == b'/'
        {
            return normalized[prefix.len() + 1..].to_vec();
        }
        let mut display = Vec::new();
        for _ in 0..self.cwd_depth {
            display.extend_from_slice(b"../");
        }
        display.extend_from_slice(normalized);
        display
    }
}

#[derive(Debug)]
struct LsTreePathspecFilter {
    specs: Vec<LsTreePathspec>,
    root_scope: bool,
}

#[derive(Debug)]
struct LsTreePathspec {
    path: Vec<u8>,
    trailing_slash: bool,
}

impl LsTreePathspecFilter {
    fn new(pathspecs: &[String], path_context: &LsTreePathContext) -> Result<Self> {
        let mut specs = Vec::new();
        let mut root_scope = false;
        for pathspec in pathspecs {
            let trailing_slash = pathspec.ends_with('/');
            let normalized = path_context.normalize_pathspec(pathspec)?;
            if normalized.is_empty() {
                root_scope = true;
                continue;
            }
            specs.push(LsTreePathspec {
                path: normalized.into_bytes(),
                trailing_slash,
            });
        }
        Ok(Self { specs, root_scope })
    }

    fn matches_root_scope(&self) -> bool {
        self.root_scope
    }

    fn should_print(
        &self,
        path: &[u8],
        is_tree: bool,
        recursive: bool,
        options: TreePrintOptions<'_>,
    ) -> bool {
        let exact = self.has_exact(path);
        let contents = self.matches_contents(path, recursive);
        let recursive_exact = recursive && self.matches_recursive_exact_descendant(path);
        let traversal_tree = is_tree
            && options.show_trees
            && (self.has_contents(path)
                || self.has_descendant_spec(path)
                || (recursive && (exact || self.is_under_exact_prefix(path)))
                || (recursive && self.is_under_contents_prefix(path)));

        let mut print = false;
        if exact && !self.exact_tree_is_redundant(path, is_tree, recursive) {
            print |= if recursive && is_tree {
                output_allowed_recursive(is_tree, options)
            } else {
                output_allowed_nonrecursive(is_tree, options)
            };
        }
        if contents {
            print |= if recursive {
                output_allowed_recursive(is_tree, options)
            } else {
                output_allowed_nonrecursive(is_tree, options)
            };
        }
        if recursive_exact {
            print |= output_allowed_recursive(is_tree, options);
        }
        print || traversal_tree
    }

    fn should_descend(&self, path: &[u8], recursive: bool) -> bool {
        self.has_descendant_spec(path)
            || self.has_contents(path)
            || (recursive
                && (self.has_exact(path)
                    || self.is_under_exact_prefix(path)
                    || self.is_under_contents_prefix(path)))
    }

    fn has_exact(&self, path: &[u8]) -> bool {
        self.specs
            .iter()
            .any(|spec| !spec.trailing_slash && spec.path == path)
    }

    fn has_contents(&self, path: &[u8]) -> bool {
        self.specs
            .iter()
            .any(|spec| spec.trailing_slash && spec.path == path)
    }

    fn has_descendant_spec(&self, path: &[u8]) -> bool {
        self.specs
            .iter()
            .any(|spec| path_is_descendant(&spec.path, path))
    }

    fn matches_contents(&self, path: &[u8], recursive: bool) -> bool {
        self.specs
            .iter()
            .filter(|spec| spec.trailing_slash)
            .any(|spec| {
                if recursive {
                    path_is_descendant(path, &spec.path)
                } else {
                    path_parent_eq(path, &spec.path)
                }
            })
    }

    fn matches_recursive_exact_descendant(&self, path: &[u8]) -> bool {
        self.specs
            .iter()
            .filter(|spec| !spec.trailing_slash)
            .any(|spec| path_is_descendant(path, &spec.path))
    }

    fn is_under_exact_prefix(&self, path: &[u8]) -> bool {
        self.matches_recursive_exact_descendant(path)
    }

    fn is_under_contents_prefix(&self, path: &[u8]) -> bool {
        self.specs
            .iter()
            .filter(|spec| spec.trailing_slash)
            .any(|spec| path_is_descendant(path, &spec.path))
    }

    fn exact_tree_is_redundant(&self, path: &[u8], is_tree: bool, recursive: bool) -> bool {
        is_tree && !recursive && (self.has_contents(path) || self.has_descendant_spec(path))
    }
}

fn output_allowed_nonrecursive(is_tree: bool, options: TreePrintOptions<'_>) -> bool {
    is_tree || !options.tree_only
}

fn output_allowed_recursive(is_tree: bool, options: TreePrintOptions<'_>) -> bool {
    if is_tree {
        options.show_trees || options.tree_only
    } else {
        !options.tree_only
    }
}

fn path_is_descendant(path: &[u8], base: &[u8]) -> bool {
    if base.is_empty() {
        return !path.is_empty();
    }
    path.len() > base.len() && path.starts_with(base) && path[base.len()] == b'/'
}

fn path_parent_eq(path: &[u8], parent: &[u8]) -> bool {
    match path.iter().rposition(|byte| *byte == b'/') {
        Some(index) => &path[..index] == parent,
        None => parent.is_empty(),
    }
}

fn normalize_ls_tree_pathspec_into(components: &mut Vec<String>, pathspec: &str) -> Result<String> {
    for component in pathspec.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                if components.pop().is_none() {
                    eprintln!("fatal: {pathspec}: '{pathspec}' is outside repository");
                    return Err(GitError::Exit(128));
                }
            }
            component => components.push(component.to_string()),
        }
    }
    Ok(components.join("/"))
}

fn print_tree_recursive_to_writer(
    writer: &mut impl Write,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    body: &[u8],
    path: &mut Vec<u8>,
    options: TreePrintOptions<'_>,
) -> Result<()> {
    for entry in TreeEntries::new(format, body) {
        let entry = entry?;
        let path_len = path.len();
        path.extend_from_slice(entry.name);
        if entry.mode == 0o040000 {
            if options.show_trees || options.tree_only {
                print_tree_entry_to_writer(writer, Some(db), &entry, path, options)?;
            }
            let object = db.read_object(&entry.oid)?;
            if object.object_type != ObjectType::Tree {
                return Err(GitError::InvalidObject(format!(
                    "expected tree {}, found {}",
                    entry.oid,
                    object.object_type.as_str()
                )));
            }
            path.push(b'/');
            print_tree_recursive_to_writer(writer, db, format, &object.body, path, options)?;
        } else if options.tree_only && tree_entry_object_type(entry.mode) == ObjectType::Blob {
            path.truncate(path_len);
            continue;
        } else {
            print_tree_entry_to_writer(writer, Some(db), &entry, path, options)?;
        }
        path.truncate(path_len);
    }
    Ok(())
}

pub(crate) fn cmd_ls_files(args: &[String]) -> Result<()> {
    let mut stage = false;
    let mut nul = false;
    let mut others = false;
    let mut deleted = false;
    let mut modified = false;
    let mut cached = false;
    let mut unmerged = false;
    let mut resolve_undo = false;
    let mut directory = false;
    let mut no_empty_directory = false;
    let mut ignored = false;
    let mut exclude_standard = false;
    let mut exclude_patterns = Vec::new();
    let mut exclude_from = Vec::new();
    let mut exclude_per_directory = Vec::new();
    let mut full_name = false;
    let mut deduplicate = false;
    let mut error_unmatch = false;
    let mut show_eol = false;
    let mut debug = false;
    let mut sparse = false;
    let mut tag = false;
    let mut killed = false;
    let mut recurse_submodules = false;
    let mut with_tree: Option<String> = None;
    let mut format_spec: Option<String> = None;
    let mut oid_abbrev = None;
    let mut path_args = Vec::new();
    let mut positional_only = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if positional_only {
            path_args.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "--stage" | "-s" => stage = true,
            "--no-stage" => stage = false,
            "--cached" | "-c" => cached = true,
            "--no-cached" => cached = false,
            "--others" | "-o" => others = true,
            "--no-others" => others = false,
            "--deleted" | "-d" => deleted = true,
            "--no-deleted" => deleted = false,
            "--modified" | "-m" => modified = true,
            "--no-modified" => modified = false,
            "--unmerged" | "-u" => unmerged = true,
            "--no-unmerged" => unmerged = false,
            "--resolve-undo" => resolve_undo = true,
            "--directory" => directory = true,
            "--no-directory" => directory = false,
            "--empty-directory" => no_empty_directory = false,
            "--no-empty-directory" => no_empty_directory = true,
            "--ignored" | "-i" => ignored = true,
            "--no-ignored" => ignored = false,
            "--exclude-standard" => exclude_standard = true,
            "--exclude" | "-x" => {
                let Some(pattern) = iter.next() else {
                    return Err(GitError::Command(
                        "ls-files --exclude requires a pattern".into(),
                    ));
                };
                exclude_patterns.push(pattern.as_bytes().to_vec());
            }
            "--exclude-from" | "-X" => {
                let Some(path) = iter.next() else {
                    return Err(GitError::Command(
                        "ls-files --exclude-from requires a file".into(),
                    ));
                };
                exclude_from.push(path.to_string());
            }
            "--exclude-per-directory" => {
                let Some(name) = iter.next() else {
                    return Err(GitError::Command(
                        "ls-files --exclude-per-directory requires a file name".into(),
                    ));
                };
                exclude_per_directory.push(name.to_string());
            }
            "--no-exclude-per-directory" => exclude_per_directory.clear(),
            "--full-name" => full_name = true,
            "--deduplicate" => deduplicate = true,
            "--no-deduplicate" => deduplicate = false,
            "--error-unmatch" => error_unmatch = true,
            "--no-error-unmatch" => error_unmatch = false,
            "--eol" => show_eol = true,
            "--no-eol" => show_eol = false,
            "--debug" => debug = true,
            "--no-debug" => debug = false,
            "--sparse" => sparse = true,
            "--no-sparse" => sparse = false,
            "--format" => {
                let Some(value) = iter.next() else {
                    return Err(GitError::Command(
                        "ls-files --format requires a value".into(),
                    ));
                };
                format_spec = Some(value.clone());
            }
            "-t" => tag = true,
            "--no-t" => tag = false,
            "-k" | "--killed" => killed = true,
            "--no-killed" => killed = false,
            "--recurse-submodules" => recurse_submodules = true,
            "--no-recurse-submodules" => recurse_submodules = false,
            "--with-tree" => {
                let Some(value) = iter.next() else {
                    return Err(GitError::Command(
                        "ls-files --with-tree requires a value".into(),
                    ));
                };
                with_tree = Some(value.clone());
            }
            value if let Some(value) = value.strip_prefix("--with-tree=") => {
                with_tree = Some(value.to_string());
            }
            "--no-resolve-undo" => resolve_undo = false,
            "--abbrev" => oid_abbrev = Some(7),
            "--no-abbrev" => oid_abbrev = None,
            value if let Some(value) = value.strip_prefix("--abbrev=") => {
                let width = parse_abbrev(value)?;
                oid_abbrev = (width != 0).then_some(width.max(4));
            }
            "-z" => nul = true,
            value if value.starts_with("--exclude=") => {
                let Some(pattern) = value.strip_prefix("--exclude=") else {
                    return Err(GitError::Command(
                        "ls-files --exclude requires a pattern".into(),
                    ));
                };
                exclude_patterns.push(pattern.as_bytes().to_vec());
            }
            value if value.starts_with("--exclude-from=") => {
                let Some(path) = value.strip_prefix("--exclude-from=") else {
                    return Err(GitError::Command(
                        "ls-files --exclude-from requires a file".into(),
                    ));
                };
                exclude_from.push(path.to_string());
            }
            value if value.starts_with("--exclude-per-directory=") => {
                let Some(name) = value.strip_prefix("--exclude-per-directory=") else {
                    return Err(GitError::Command(
                        "ls-files --exclude-per-directory requires a file name".into(),
                    ));
                };
                exclude_per_directory.push(name.to_string());
            }
            value if let Some(value) = value.strip_prefix("--format=") => {
                format_spec = Some(value.to_string());
            }
            value
                if value.starts_with('-')
                    && value.len() > 2
                    && value[1..].bytes().all(|option| {
                        matches!(
                            option,
                            b's' | b'c' | b'o' | b'd' | b'm' | b'u' | b'i' | b't' | b'z'
                        )
                    }) =>
            {
                for option in value[1..].bytes() {
                    match option {
                        b's' => stage = true,
                        b'c' => cached = true,
                        b'o' => others = true,
                        b'd' => deleted = true,
                        b'm' => modified = true,
                        b'u' => unmerged = true,
                        b'i' => ignored = true,
                        b't' => tag = true,
                        b'z' => nul = true,
                        _ => unreachable!("ls-files short-option group was filtered"),
                    }
                }
            }
            value if !value.starts_with('-') => path_args.push(arg.clone()),
            value => {
                return Err(GitError::Command(format!(
                    "unsupported ls-files option {value}; currently supports --stage, --cached, --others, --deleted, --modified, --unmerged, --resolve-undo, --directory, --no-empty-directory, --full-name, --deduplicate, --error-unmatch, --debug, -t, --abbrev[=<n>], --no-abbrev, and -z"
                )));
            }
        }
    }
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    for path in &exclude_from {
        let absolute = if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            cwd.join(path)
        };
        let contents = match fs::read(&absolute) {
            Ok(contents) => contents,
            Err(_) => {
                eprintln!("fatal: cannot use {path} as an exclude file");
                return Err(GitError::Exit(128));
            }
        };
        exclude_patterns.extend(contents.split(|byte| *byte == b'\n').map(Vec::from));
    }
    let terminator = if nul { 0 } else { b'\n' };
    // git: `--format` cannot be combined with the format-altering selectors.
    // Mirrors builtin/ls-files.c's usage_msg_opt() check (exit 129).
    if format_spec.is_some()
        && (stage || others || killed || resolve_undo || deduplicate || show_eol || tag)
    {
        return Err(GitError::usage(
            "--format cannot be used with -s, -o, -k, -t, --resolve-undo, --deduplicate, --eol",
        ));
    }
    // git: `--recurse-submodules` rejects worktree-dependent modes and
    // `--with-tree`; `--error-unmatch` is separately unsupported. Both die(128).
    if recurse_submodules
        && (deleted
            || others
            || unmerged
            || killed
            || modified
            || resolve_undo
            || with_tree.is_some())
    {
        eprintln!("fatal: ls-files --recurse-submodules unsupported mode");
        return Err(GitError::Exit(128));
    }
    if recurse_submodules && error_unmatch {
        eprintln!("fatal: ls-files --recurse-submodules does not support --error-unmatch");
        return Err(GitError::Exit(128));
    }
    // git: `--with-tree` cannot combine with stage/unmerged output (die 128).
    if with_tree.is_some() && (stage || unmerged) {
        eprintln!("fatal: options 'ls-files --with-tree' and '-s/-u' cannot be used together");
        return Err(GitError::Exit(128));
    }
    let selected = cached || others || deleted || modified || unmerged || resolve_undo;
    let output_stage = stage || unmerged;
    if ignored && !others && !cached {
        eprintln!("fatal: ls-files -i must be used with either -o or -c");
        return Err(GitError::Exit(128));
    }
    if ignored
        && !exclude_standard
        && exclude_patterns.is_empty()
        && exclude_per_directory.is_empty()
    {
        eprintln!("fatal: ls-files --ignored needs some exclude pattern");
        return Err(GitError::Exit(128));
    }
    if !selected
        && !output_stage
        && !show_eol
        && !debug
        && !tag
        && oid_abbrev.is_none()
        && !nul
        && path_args.is_empty()
        && !full_name
        && !deduplicate
        && !error_unmatch
        && format_spec.is_none()
        && !exclude_standard
        && exclude_patterns.is_empty()
        && exclude_from.is_empty()
        && exclude_per_directory.is_empty()
        && sparse
        && cwd == worktree_root
    {
        let stdout = io::stdout();
        let mut stdout = io::BufWriter::new(stdout.lock());
        let index_path = sley_worktree::repository_index_path(&git_dir);
        match fs::read(index_path) {
            Ok(index_bytes) => {
                if sley_index::Index::bytes_have_extension(
                    &index_bytes,
                    format,
                    &sley_index::INDEX_EXT_LINK,
                )? {
                    if let Some(index) = sley_worktree::read_repository_index(&git_dir, format)? {
                        for entry in &index.entries {
                            write_ls_files_path(&mut stdout, &entry.path, terminator)?;
                            stdout.write_all(&[terminator])?;
                        }
                    }
                } else {
                    write_ls_files_index_root_fast(&mut stdout, &index_bytes, format, terminator)?
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
        stdout.flush()?;
        return Ok(());
    }
    let mut stdout = io::stdout();
    let pathspec = LsFilesPathspec::new(&cwd, &worktree_root, full_name, &path_args)?;
    let eol_context = if show_eol {
        Some(EolContext {
            worktree_root: worktree_root.clone(),
            db: FileObjectDatabase::from_git_dir(&git_dir, format),
        })
    } else {
        None
    };
    let eol = eol_context.as_ref();
    if recurse_submodules {
        recurse_ls_files_submodules(
            &mut stdout,
            &git_dir,
            &worktree_root,
            format,
            b"",
            output_stage,
            terminator,
            &pathspec,
            oid_abbrev,
        )?;
        stdout.flush()?;
        if error_unmatch {
            pathspec.exit_if_unmatched()?;
        }
        return Ok(());
    }
    if let Some(with_tree) = with_tree.as_deref() {
        write_ls_files_with_tree(
            &mut stdout,
            &git_dir,
            format,
            with_tree,
            terminator,
            &pathspec,
        )?;
        stdout.flush()?;
        if error_unmatch {
            pathspec.exit_if_unmatched()?;
        }
        return Ok(());
    }
    if resolve_undo {
        if let Some(index) = sley_worktree::read_repository_index(&git_dir, format)? {
            write_ls_files_resolve_undo(&mut stdout, &index, format, terminator, &pathspec)?;
        }
        stdout.flush()?;
        if error_unmatch {
            pathspec.exit_if_unmatched()?;
        }
        return Ok(());
    }
    if others {
        let untracked = sley_worktree::untracked_paths_with_options(
            &worktree_root,
            &git_dir,
            format,
            sley_worktree::UntrackedPathOptions {
                directory,
                no_empty_directory,
                preserve_ignored_directories: false,
                exclude_standard,
                ignored_only: ignored,
                exclude_patterns: exclude_patterns.clone(),
                exclude_per_directory: exclude_per_directory.clone(),
                pathspecs: pathspec.untracked_pathspecs(),
            },
        )?;
        for path in untracked {
            if let Some(display) = pathspec.display(&path) {
                if let Some(eol) = eol {
                    // Untracked files have no index blob: `i/` is empty.
                    eol.write_prefix(&mut stdout, &path, None)?;
                }
                write_ls_files_path(&mut stdout, &display, terminator)?;
                stdout.write_all(&[terminator])?;
            }
        }
    }
    let deleted_entries = if deleted {
        sley_worktree::deleted_index_entries(&worktree_root, &git_dir, format)?
    } else {
        Vec::new()
    };
    let modified_entries = if modified {
        sley_worktree::modified_index_entries(&worktree_root, &git_dir, format)?
    } else {
        Vec::new()
    };
    if let Some(format_spec) = format_spec.as_deref() {
        if let Some(index) = sley_worktree::read_repository_index(&git_dir, format)? {
            let index =
                ls_files_display_index(&git_dir, format, index, sparse && !(deleted || modified))?;
            let oid_candidates = ls_files_oid_candidates(&index);
            let ctx = LsFilesFormatContext {
                spec: format_spec,
                needs_eol: format_spec.contains("(eol"),
                eol: EolContext {
                    worktree_root: worktree_root.clone(),
                    db: FileObjectDatabase::from_git_dir(&git_dir, format),
                },
                abbrev: oid_abbrev,
                candidates: &oid_candidates,
            };
            // git: with no selector `show_cached` defaults on; `-m`/`-d` turn it
            // off unless `-c` is also given. Each entry is emitted once per
            // matching condition, in cache order (cached, deleted, modified).
            let want_cached = cached || !(modified || deleted);
            for entry in &index.entries {
                let Some(path) = pathspec.display(&entry.path) else {
                    continue;
                };
                let is_deleted = deleted && deleted_entries.iter().any(|e| e.path == entry.path);
                let is_modified = modified && modified_entries.iter().any(|e| e.path == entry.path);
                // git emits the line once per matching condition, in cache order
                // (cached, then deleted, then modified).
                for emit in [want_cached, is_deleted, is_modified] {
                    if !emit {
                        continue;
                    }
                    write_ls_files_format(&mut stdout, entry, &path, &ctx)?;
                    stdout.write_all(&[terminator])?;
                    if debug {
                        write_ls_files_debug(&mut stdout, entry)?;
                    }
                }
            }
        }
        stdout.flush()?;
        if error_unmatch {
            pathspec.exit_if_unmatched()?;
        }
        return Ok(());
    }
    if selected && !output_stage {
        if (cached || deleted || modified)
            && let Some(index) = sley_worktree::read_repository_index(&git_dir, format)?
        {
            let index =
                ls_files_display_index(&git_dir, format, index, sparse && !(deleted || modified))?;
            let oid_candidates = ls_files_oid_candidates(&index);
            if ignored && cached {
                let ignored_entries = sley_worktree::ignored_index_entries(
                    &worktree_root,
                    &index.entries,
                    exclude_standard,
                    &exclude_patterns,
                    &exclude_per_directory,
                )?;
                write_ls_files_selected(
                    &mut stdout,
                    ignored_entries,
                    deleted_entries.iter(),
                    modified_entries.iter(),
                    &pathspec,
                    LsFilesWriteOptions {
                        cached,
                        stage: false,
                        terminator,
                        deduplicate,
                        oid_abbrev,
                        oid_candidates: &oid_candidates,
                        eol,
                        debug,
                        tag,
                    },
                )?;
            } else {
                write_ls_files_selected(
                    &mut stdout,
                    index.entries.iter(),
                    deleted_entries.iter(),
                    modified_entries.iter(),
                    &pathspec,
                    LsFilesWriteOptions {
                        cached,
                        stage: false,
                        terminator,
                        deduplicate,
                        oid_abbrev,
                        oid_candidates: &oid_candidates,
                        eol,
                        debug,
                        tag,
                    },
                )?;
            }
        }
        stdout.flush()?;
        if error_unmatch {
            pathspec.exit_if_unmatched()?;
        }
        return Ok(());
    }
    if let Some(index) = sley_worktree::read_repository_index(&git_dir, format)? {
        let index =
            ls_files_display_index(&git_dir, format, index, sparse && !(deleted || modified))?;
        let oid_candidates = ls_files_oid_candidates(&index);
        if unmerged {
            write_ls_files_unmerged(
                &mut stdout,
                index.entries.iter(),
                terminator,
                &pathspec,
                oid_abbrev,
                &oid_candidates,
                eol,
                debug,
                tag,
            )?;
        } else if (deleted || modified) && output_stage {
            write_ls_files_index_with_selected(
                &mut stdout,
                index.entries.iter(),
                deleted_entries.iter(),
                modified_entries.iter(),
                &pathspec,
                LsFilesWriteOptions {
                    cached: false,
                    stage: true,
                    terminator,
                    deduplicate: false,
                    oid_abbrev,
                    oid_candidates: &oid_candidates,
                    eol,
                    debug,
                    tag,
                },
            )?;
        } else {
            write_ls_files_index(
                &mut stdout,
                index.entries.iter(),
                output_stage,
                terminator,
                &pathspec,
                oid_abbrev,
                &oid_candidates,
                eol,
                debug,
                tag,
            )?;
        }
    }
    stdout.flush()?;
    if error_unmatch {
        pathspec.exit_if_unmatched()?;
    }
    Ok(())
}

fn write_ls_files_index_root_fast(
    stdout: &mut impl Write,
    index_bytes: &[u8],
    format: ObjectFormat,
    terminator: u8,
) -> Result<()> {
    sley_index::Index::for_each_path(index_bytes, format, |path| {
        write_ls_files_path(stdout, path, terminator)?;
        stdout.write_all(&[terminator])?;
        Ok(())
    })
}

/// `ls-files --with-tree=<tree-ish>`: overlay `<tree-ish>` onto the index so
/// that paths removed from the index since that tree still appear. Mirrors
/// git's `overlay_tree_on_index` (read-cache.c): existing unmerged entries are
/// hoisted to stage #3, the tree's leaves are appended at stage #1, the result
/// is sorted by (name, stage), and a stage-#1 entry shadowed by a stage-#0
/// entry of the same name is hidden (git's `CE_UPDATE` marker).
fn write_ls_files_with_tree(
    stdout: &mut io::Stdout,
    git_dir: &Path,
    format: ObjectFormat,
    with_tree: &str,
    terminator: u8,
    pathspec: &LsFilesPathspec,
) -> Result<()> {
    let repo = RepositoryContext::discover_current()?;
    // Resolve <tree-ish> to a tree oid, dying with 128 like git on failure.
    let tree_oid = match repo.resolve_revision(with_tree) {
        Ok(oid) => {
            if oid == ObjectId::empty_tree(format) {
                oid
            } else {
                match sley_rev::peel_to_tree(repo.objects(), format, &oid) {
                    Ok(tree) => tree,
                    Err(_) => {
                        eprintln!("fatal: not a tree-ish object: {with_tree}");
                        return Err(GitError::Exit(128));
                    }
                }
            }
        }
        Err(_) => {
            eprintln!("fatal: tree-ish {with_tree} not found.");
            return Err(GitError::Exit(128));
        }
    };

    // (path, stage) records for the overlaid index.
    let mut entries: Vec<(Vec<u8>, u8)> = Vec::new();
    if let Some(index) = sley_worktree::read_repository_index(git_dir, format)? {
        // git ensures a full index before overlaying.
        let index = ls_files_display_index(git_dir, format, index, false)?;
        for entry in &index.entries {
            let stage = if index_entry_stage(entry) != 0 { 3 } else { 0 };
            entries.push((entry.path.to_vec(), stage));
        }
    }
    for (path, _value) in sley_diff_merge::flatten_tree(repo.objects(), format, &tree_oid)? {
        entries.push((path, 1));
    }
    // git's cmp_cache_name_compare: name, then stage.
    entries.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

    let mut hidden = vec![false; entries.len()];
    let mut last_stage0: Option<usize> = None;
    for idx in 0..entries.len() {
        match entries[idx].1 {
            0 => last_stage0 = Some(idx),
            1 => {
                if let Some(s0) = last_stage0
                    && entries[s0].0 == entries[idx].0
                {
                    hidden[idx] = true;
                }
            }
            _ => {}
        }
    }

    for (idx, (path, _stage)) in entries.iter().enumerate() {
        if hidden[idx] {
            continue;
        }
        if let Some(display) = pathspec.display(path) {
            write_ls_files_path(stdout, &display, terminator)?;
            stdout.write_all(&[terminator])?;
        }
    }
    Ok(())
}

/// `ls-files --recurse-submodules`: list this repo's index, and for every
/// gitlink that is an *active* submodule (git's `is_submodule_active`), recurse
/// into the submodule's index instead — prefixing its paths with the submodule
/// path. Inactive gitlinks are listed as the gitlink entry itself. Mirrors
/// builtin/ls-files.c `show_ce` + `show_submodule`.
#[allow(clippy::too_many_arguments)]
fn recurse_ls_files_submodules(
    stdout: &mut io::Stdout,
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    prefix: &[u8],
    output_stage: bool,
    terminator: u8,
    pathspec: &LsFilesPathspec,
    oid_abbrev: Option<usize>,
) -> Result<()> {
    let Some(index) = sley_worktree::read_repository_index(git_dir, format)? else {
        return Ok(());
    };
    let index = ls_files_display_index(git_dir, format, index, false)?;
    let candidates = ls_files_oid_candidates(&index);
    let config = read_repo_config(git_dir)?;
    // git reads each (sub)repo's settings on index read and dies on a malformed
    // `index.sparse` boolean (prepare_repo_settings -> repo_cfg_bool).
    validate_repo_index_sparse_bool(git_dir, &config)?;
    let gitmodules = GitConfig::read(worktree_root.join(".gitmodules")).ok();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);

    for entry in &index.entries {
        let mut full = prefix.to_vec();
        full.extend_from_slice(&entry.path);

        if entry.mode == 0o160000
            && submodule_is_active_for_path(&config, gitmodules.as_ref(), &entry.path)
        {
            let sub_root = worktree_root.join(repo_path_to_path(&entry.path));
            if let Some(sub_git_dir) = submodule_git_dir_for_path(&db, &sub_root, &entry.path) {
                let sub_format = repository_object_format(&sub_git_dir).unwrap_or(format);
                let mut sub_prefix = full.clone();
                sub_prefix.push(b'/');
                recurse_ls_files_submodules(
                    stdout,
                    &sub_git_dir,
                    &sub_root,
                    sub_format,
                    &sub_prefix,
                    output_stage,
                    terminator,
                    pathspec,
                    oid_abbrev,
                )?;
                continue;
            }
            // Active but unresolvable git dir: fall through and list the gitlink.
        }

        if let Some(display) = pathspec.display(&full) {
            if output_stage {
                write!(
                    stdout,
                    "{:06o} {} {}\t",
                    entry.mode,
                    ls_files_oid(&entry.oid, oid_abbrev, &candidates),
                    index_entry_stage(entry)
                )?;
            }
            write_ls_files_path(stdout, &display, terminator)?;
            stdout.write_all(&[terminator])?;
        }
    }
    Ok(())
}

/// git rejects a non-boolean `index.sparse` while reading a repo's settings
/// (prepare_repo_settings -> repo_cfg_bool -> git_config_bool dies). The
/// effective value is the last one across base `config` and, when
/// `extensions.worktreeConfig` is enabled, `config.worktree` (which wins).
fn validate_repo_index_sparse_bool(git_dir: &Path, config: &GitConfig) -> Result<()> {
    let worktree_config = config
        .get_bool("extensions", None, "worktreeConfig")
        .unwrap_or(false)
        .then(|| GitConfig::read(git_dir.join("config.worktree")).ok())
        .flatten();
    // `config.worktree` wins over the base file when it sets index.sparse.
    let target = match worktree_config.as_ref() {
        Some(wt) if wt.get("index", None, "sparse").is_some() => wt,
        _ => config,
    };
    if let Some(value) = target.get("index", None, "sparse")
        && target.get_bool("index", None, "sparse").is_none()
    {
        eprintln!("fatal: bad boolean config value '{value}' for 'index.sparse'");
        return Err(GitError::Exit(128));
    }
    Ok(())
}

/// git's `is_submodule_active` for a gitlink `path` relative to the repo whose
/// `config`/`gitmodules` are given: honor `submodule.<name>.active`, then the
/// `submodule.active` pathspec list, then fall back to `submodule.<name>.url`.
fn submodule_is_active_for_path(
    config: &GitConfig,
    gitmodules: Option<&GitConfig>,
    path: &[u8],
) -> bool {
    let Some(name) = gitmodules.and_then(|gm| submodule_name_for_path(gm, path)) else {
        return false;
    };
    if let Some(active) = config.get_bool("submodule", Some(&name), "active") {
        return active;
    }
    let specs: Vec<&str> = config
        .get_all("submodule", None, "active")
        .into_iter()
        .flatten()
        .collect();
    if !specs.is_empty() {
        return submodule_active_specs_match(&specs, path);
    }
    config.get("submodule", Some(&name), "url").is_some()
}

/// Map a gitlink `path` to its `.gitmodules` submodule name (the subsection of
/// the `submodule.<name>` whose `path` value equals `path`).
fn submodule_name_for_path(gitmodules: &GitConfig, path: &[u8]) -> Option<String> {
    let path_str = std::str::from_utf8(path).ok()?;
    for section in &gitmodules.sections {
        if !section.name.eq_ignore_ascii_case("submodule") {
            continue;
        }
        let Some(name) = &section.subsection else {
            continue;
        };
        let mut sub_path = None;
        for entry in &section.entries {
            if entry.key.eq_ignore_ascii_case("path") {
                sub_path = entry.value.as_deref();
            }
        }
        if sub_path == Some(path_str) {
            return Some(name.clone());
        }
    }
    None
}

/// Whether any `submodule.active` pathspec matches the submodule `path`. A
/// minimal matcher (exact path or a directory-prefix match), sufficient for the
/// `submodule.active` cases git exercises.
fn submodule_active_specs_match(specs: &[&str], path: &[u8]) -> bool {
    let Ok(path_str) = std::str::from_utf8(path) else {
        return false;
    };
    specs.iter().any(|spec| {
        let spec = spec.trim_end_matches('/');
        spec == "." || spec == path_str || path_str.starts_with(&format!("{spec}/"))
    })
}

/// Per-invocation state for `ls-files --format`. Holds everything an atom may
/// need: the format string, an `EolContext` (worktree + odb) for the
/// `objectsize`/`eolinfo`/`eolattr` atoms, and the abbreviation parameters for
/// `objectname`.
struct LsFilesFormatContext<'a> {
    spec: &'a str,
    /// Whether the spec references any `eol*` atom; gates the per-entry eol
    /// computation so non-eol formats pay nothing.
    needs_eol: bool,
    eol: EolContext,
    abbrev: Option<usize>,
    candidates: &'a [ObjectId],
}

/// git's `object_type(ce_mode)`: gitlinks are commits, directories are trees,
/// everything else (regular files, symlinks) is a blob.
fn ls_files_object_type(mode: u32) -> ObjectType {
    match mode & 0o170000 {
        0o160000 => ObjectType::Commit,
        0o040000 => ObjectType::Tree,
        _ => ObjectType::Blob,
    }
}

/// git's `expand_objectsize`: the blob size for blob-typed entries, a literal
/// `-` otherwise. `padded` right-justifies the field in 7 columns.
fn write_ls_files_objectsize(
    stdout: &mut io::Stdout,
    entry: &sley_index::IndexEntry,
    eol: &EolContext,
    padded: bool,
) -> Result<()> {
    if ls_files_object_type(entry.mode) == ObjectType::Blob {
        let size = match eol.db.read_object_header(&entry.oid)? {
            Some((_, size)) => size,
            None => {
                return Err(GitError::Command(format!(
                    "could not get object info about '{}'",
                    entry.oid
                )));
            }
        };
        if padded {
            write!(stdout, "{size:>7}")?;
        } else {
            write!(stdout, "{size}")?;
        }
    } else if padded {
        write!(stdout, "{:>7}", "-")?;
    } else {
        stdout.write_all(b"-")?;
    }
    Ok(())
}

fn write_ls_files_format(
    stdout: &mut io::Stdout,
    entry: &sley_index::IndexEntry,
    display_path: &[u8],
    ctx: &LsFilesFormatContext<'_>,
) -> Result<()> {
    // git's write_eolinfo machinery resolves these fields once per entry.
    let eol_info = if ctx.needs_eol {
        let index_oid = is_regular_file_mode(entry.mode).then_some(&entry.oid);
        Some(ctx.eol.info(&entry.path, index_oid)?)
    } else {
        None
    };
    let mut rest = ctx.spec;
    while let Some(start) = rest.find('%') {
        stdout.write_all(rest[..start].as_bytes())?;
        rest = &rest[start + 1..];
        if let Some(after_open) = rest.strip_prefix('(') {
            if let Some(end) = after_open.find(')') {
                let atom = &after_open[..end];
                match atom {
                    "objectmode" => write!(stdout, "{:06o}", entry.mode)?,
                    "objectname" => stdout.write_all(
                        ls_files_oid(&entry.oid, ctx.abbrev, ctx.candidates).as_bytes(),
                    )?,
                    "objecttype" => {
                        stdout.write_all(ls_files_object_type(entry.mode).as_str().as_bytes())?
                    }
                    "objectsize" => write_ls_files_objectsize(stdout, entry, &ctx.eol, false)?,
                    "objectsize:padded" => {
                        write_ls_files_objectsize(stdout, entry, &ctx.eol, true)?
                    }
                    "stage" => write!(stdout, "{}", index_entry_stage(entry))?,
                    "eolinfo:index" => stdout
                        .write_all(eol_info.as_ref().map(|i| i.index).unwrap_or("").as_bytes())?,
                    "eolinfo:worktree" => stdout.write_all(
                        eol_info
                            .as_ref()
                            .map(|i| i.worktree)
                            .unwrap_or("")
                            .as_bytes(),
                    )?,
                    "eolattr" => stdout
                        .write_all(eol_info.as_ref().map(|i| i.attr).unwrap_or("").as_bytes())?,
                    "path" => stdout.write_all(display_path)?,
                    _ => {
                        return Err(GitError::Command(format!(
                            "unsupported ls-files format atom {atom}"
                        )));
                    }
                }
                rest = &after_open[end + 1..];
            } else {
                stdout.write_all(b"%(")?;
                rest = after_open;
            }
        } else if let Some(hex) = rest.strip_prefix('x').and_then(|value| value.get(..2)) {
            let byte = u8::from_str_radix(hex, 16).map_err(|_| {
                GitError::Command(format!("invalid ls-files format escape %x{hex}"))
            })?;
            stdout.write_all(&[byte])?;
            rest = &rest[3..];
        } else if rest.starts_with('%') {
            stdout.write_all(b"%")?;
            rest = &rest[1..];
        } else {
            stdout.write_all(b"%")?;
        }
    }
    stdout.write_all(rest.as_bytes())?;
    Ok(())
}

fn ls_files_display_index(
    git_dir: &Path,
    format: ObjectFormat,
    mut index: Index,
    sparse: bool,
) -> Result<Index> {
    if !sparse && index.entries.iter().any(IndexEntry::is_sparse_dir) {
        let db = FileObjectDatabase::from_git_dir(git_dir, format);
        sley_worktree::expand_sparse_index(&mut index, &db, format)?;
    }
    Ok(index)
}

struct LsFilesResolveUndoRecord {
    path: Vec<u8>,
    stages: [Option<(u32, ObjectId)>; 3],
}

fn parse_ls_files_resolve_undo_records(
    body: Option<&[u8]>,
    format: ObjectFormat,
) -> Result<Vec<LsFilesResolveUndoRecord>> {
    let Some(body) = body else {
        return Ok(Vec::new());
    };
    let mut records = Vec::new();
    let mut offset = 0usize;
    while offset < body.len() {
        let path_end = body[offset..]
            .iter()
            .position(|byte| *byte == 0)
            .ok_or_else(|| GitError::InvalidFormat("truncated REUC path".into()))?
            + offset;
        let path = body[offset..path_end].to_vec();
        offset = path_end + 1;

        let mut modes = [0u32; 3];
        for mode in &mut modes {
            let mode_end = body[offset..]
                .iter()
                .position(|byte| *byte == 0)
                .ok_or_else(|| GitError::InvalidFormat("truncated REUC mode".into()))?
                + offset;
            let text = std::str::from_utf8(&body[offset..mode_end])
                .map_err(|_| GitError::InvalidFormat("invalid REUC mode".into()))?;
            *mode = u32::from_str_radix(text, 8)
                .map_err(|_| GitError::InvalidFormat("invalid REUC mode".into()))?;
            offset = mode_end + 1;
        }

        let mut stages = [None, None, None];
        for (idx, mode) in modes.into_iter().enumerate() {
            if mode == 0 {
                continue;
            }
            let end = offset
                .checked_add(format.raw_len())
                .ok_or_else(|| GitError::InvalidFormat("REUC oid length overflow".into()))?;
            if end > body.len() {
                return Err(GitError::InvalidFormat("truncated REUC oid".into()));
            }
            stages[idx] = Some((mode, ObjectId::from_raw(format, &body[offset..end])?));
            offset = end;
        }
        records.push(LsFilesResolveUndoRecord { path, stages });
    }
    Ok(records)
}

fn write_ls_files_resolve_undo(
    stdout: &mut io::Stdout,
    index: &Index,
    format: ObjectFormat,
    terminator: u8,
    pathspec: &LsFilesPathspec,
) -> Result<()> {
    for record in parse_ls_files_resolve_undo_records(index.extension(b"REUC")?, format)? {
        let Some(path) = pathspec.display(&record.path) else {
            continue;
        };
        for (idx, stage) in record.stages.into_iter().enumerate() {
            let Some((mode, oid)) = stage else {
                continue;
            };
            write!(stdout, "{mode:06o} {oid} {}\t", idx + 1)?;
            write_ls_files_path(stdout, &path, terminator)?;
            stdout.write_all(&[terminator])?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_ls_files_unmerged<'a>(
    stdout: &mut io::Stdout,
    entries: impl IntoIterator<Item = &'a sley_index::IndexEntry>,
    terminator: u8,
    pathspec: &LsFilesPathspec,
    oid_abbrev: Option<usize>,
    oid_candidates: &[ObjectId],
    eol: Option<&EolContext>,
    debug: bool,
    tag: bool,
) -> Result<()> {
    for entry in entries {
        if index_entry_stage(entry) == 0 {
            continue;
        }
        write_ls_files_index(
            stdout,
            [entry],
            true,
            terminator,
            pathspec,
            oid_abbrev,
            oid_candidates,
            eol,
            debug,
            tag,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_ls_files_index<'a>(
    stdout: &mut io::Stdout,
    entries: impl IntoIterator<Item = &'a sley_index::IndexEntry>,
    stage: bool,
    terminator: u8,
    pathspec: &LsFilesPathspec,
    oid_abbrev: Option<usize>,
    oid_candidates: &[ObjectId],
    eol: Option<&EolContext>,
    debug: bool,
    tag: bool,
) -> Result<()> {
    for entry in entries {
        let Some(path) = pathspec.display(&entry.path) else {
            continue;
        };
        if let Some(eol) = eol {
            // git prints the eol prefix before any `--stage` info; the `i/`
            // field reflects the index blob only for regular files.
            let index_oid = is_regular_file_mode(entry.mode).then_some(&entry.oid);
            eol.write_prefix(stdout, &entry.path, index_oid)?;
        }
        if tag {
            write!(stdout, "{} ", ls_files_tag(entry))?;
        }
        if stage {
            let stage = index_entry_stage(entry);
            write!(
                stdout,
                "{:06o} {} {stage}\t",
                entry.mode,
                ls_files_oid(&entry.oid, oid_abbrev, oid_candidates)
            )?;
        }
        write_ls_files_path(stdout, &path, terminator)?;
        stdout.write_all(&[terminator])?;
        if debug {
            write_ls_files_debug(stdout, entry)?;
        }
    }
    Ok(())
}

fn write_ls_files_debug(stdout: &mut io::Stdout, entry: &sley_index::IndexEntry) -> Result<()> {
    let flags = entry.flags & !0x0fff;
    write!(
        stdout,
        "  ctime: {}:{}\n  mtime: {}:{}\n  dev: {}\tino: {}\n  uid: {}\tgid: {}\n  size: {}\tflags: {}\n",
        entry.ctime_seconds,
        entry.ctime_nanoseconds,
        entry.mtime_seconds,
        entry.mtime_nanoseconds,
        entry.dev,
        entry.ino,
        entry.uid,
        entry.gid,
        entry.size,
        flags,
    )?;
    Ok(())
}

/// Whether an index entry mode is a regular file (git: `S_ISREG`). Symlinks
/// (`0o120000`) and gitlinks (`0o160000`) get no `i/` eol stat.
fn is_regular_file_mode(mode: u32) -> bool {
    mode & 0o170000 == 0o100000
}

fn write_ls_files_index_with_selected<'a>(
    stdout: &mut io::Stdout,
    entries: impl IntoIterator<Item = &'a sley_index::IndexEntry>,
    deleted_entries: impl IntoIterator<Item = &'a sley_index::IndexEntry>,
    modified_entries: impl IntoIterator<Item = &'a sley_index::IndexEntry>,
    pathspec: &LsFilesPathspec,
    options: LsFilesWriteOptions,
) -> Result<()> {
    let deleted = deleted_entries.into_iter().collect::<Vec<_>>();
    let modified = modified_entries.into_iter().collect::<Vec<_>>();
    let mut seen = BTreeSet::new();
    for entry in entries {
        write_ls_files_entry_if_selected(
            stdout, entry, &deleted, &modified, pathspec, options, &mut seen,
        )?;
        write_ls_files_index(
            stdout,
            [entry],
            true,
            options.terminator,
            pathspec,
            options.oid_abbrev,
            options.oid_candidates,
            options.eol,
            options.debug,
            options.tag,
        )?;
    }
    Ok(())
}

fn write_ls_files_selected<'a>(
    stdout: &mut io::Stdout,
    entries: impl IntoIterator<Item = &'a sley_index::IndexEntry>,
    deleted_entries: impl IntoIterator<Item = &'a sley_index::IndexEntry>,
    modified_entries: impl IntoIterator<Item = &'a sley_index::IndexEntry>,
    pathspec: &LsFilesPathspec,
    options: LsFilesWriteOptions,
) -> Result<()> {
    let deleted = deleted_entries.into_iter().collect::<Vec<_>>();
    let modified = modified_entries.into_iter().collect::<Vec<_>>();
    let mut seen = BTreeSet::new();
    for entry in entries {
        write_ls_files_entry_if_selected(
            stdout, entry, &deleted, &modified, pathspec, options, &mut seen,
        )?;
    }
    Ok(())
}

fn write_ls_files_entry_if_selected(
    stdout: &mut io::Stdout,
    entry: &sley_index::IndexEntry,
    deleted: &[&sley_index::IndexEntry],
    modified: &[&sley_index::IndexEntry],
    pathspec: &LsFilesPathspec,
    options: LsFilesWriteOptions,
    seen: &mut BTreeSet<Vec<u8>>,
) -> Result<()> {
    if deleted
        .iter()
        .any(|deleted_entry| deleted_entry.path == entry.path)
    {
        write_ls_files_entry(stdout, entry, pathspec, options, seen)?;
    }
    if modified
        .iter()
        .any(|modified_entry| modified_entry.path == entry.path)
    {
        write_ls_files_entry(stdout, entry, pathspec, options, seen)?;
    }
    if options.cached {
        write_ls_files_entry(stdout, entry, pathspec, options, seen)?;
    }
    Ok(())
}

fn write_ls_files_entry(
    stdout: &mut io::Stdout,
    entry: &sley_index::IndexEntry,
    pathspec: &LsFilesPathspec,
    options: LsFilesWriteOptions,
    seen: &mut BTreeSet<Vec<u8>>,
) -> Result<()> {
    let Some(path) = pathspec.display(&entry.path) else {
        return Ok(());
    };
    if options.deduplicate && !seen.insert(path.clone()) {
        return Ok(());
    }
    if let Some(eol) = options.eol {
        let index_oid = is_regular_file_mode(entry.mode).then_some(&entry.oid);
        eol.write_prefix(stdout, &entry.path, index_oid)?;
    }
    if options.tag {
        write!(stdout, "{} ", ls_files_tag(entry))?;
    }
    if options.stage {
        let stage = index_entry_stage(entry);
        write!(
            stdout,
            "{:06o} {} {stage}\t",
            entry.mode,
            ls_files_oid(&entry.oid, options.oid_abbrev, options.oid_candidates)
        )?;
    }
    write_ls_files_path(stdout, &path, options.terminator)?;
    stdout.write_all(&[options.terminator])?;
    if options.debug {
        write_ls_files_debug(stdout, entry)?;
    }
    Ok(())
}

fn write_ls_files_path(stdout: &mut impl Write, path: &[u8], terminator: u8) -> Result<()> {
    if terminator == 0 {
        stdout.write_all(path)?;
    } else {
        write_status_quoted_path(stdout, path, false)?;
    }
    Ok(())
}

/// State needed to compute the `git ls-files --eol` `i/ w/ attr/` prefix.
struct EolContext {
    worktree_root: PathBuf,
    db: FileObjectDatabase,
}

impl EolContext {
    /// Read the index blob content for an entry, if it resolves to a blob.
    /// Mirrors git's `get_cached_convert_stats_ascii` (NULL when not a regular
    /// file is handled by the caller passing `None` for the oid).
    fn index_blob(&self, oid: &ObjectId) -> Option<Vec<u8>> {
        match self.db.read_object(oid) {
            Ok(object) if object.object_type == ObjectType::Blob => Some(object.body.clone()),
            _ => None,
        }
    }

    /// Resolve the three `i/ w/ attr/` eol fields for `repo_path` (git's
    /// `write_eolinfo` inputs). `index_oid` is the entry's blob oid for the
    /// `i/` field (None for untracked files / non-regular entries).
    fn info(
        &self,
        repo_path: &[u8],
        index_oid: Option<&ObjectId>,
    ) -> Result<sley_worktree::EolInfo> {
        let index_content = index_oid.and_then(|oid| self.index_blob(oid));
        let attr_checks = sley_worktree::eol_attribute_checks(&self.worktree_root, repo_path)?;
        Ok(sley_worktree::eol_info_for_path(
            &self.worktree_root,
            repo_path,
            index_content.as_deref(),
            &attr_checks,
        ))
    }

    /// Write the `i/%-5s w/%-5s attr/%-17s\t` prefix for `path`.
    ///
    /// `index_oid` is the entry's blob oid for the `i/` field (None for
    /// untracked files, whose index field is empty), and `repo_path` is the
    /// repo-relative path used for attribute lookup + worktree stat.
    fn write_prefix(
        &self,
        stdout: &mut io::Stdout,
        repo_path: &[u8],
        index_oid: Option<&ObjectId>,
    ) -> Result<()> {
        let info = self.info(repo_path, index_oid)?;
        stdout.write_all(info.format_prefix().as_bytes())?;
        Ok(())
    }
}

fn ls_files_oid_candidates(index: &Index) -> Vec<ObjectId> {
    index.entries.iter().map(|entry| entry.oid).collect()
}

fn ls_files_oid(oid: &ObjectId, abbrev: Option<usize>, candidates: &[ObjectId]) -> String {
    for_each_ref_abbrev_oid(oid, abbrev, candidates)
}

#[derive(Clone, Copy)]
struct LsFilesWriteOptions<'a> {
    cached: bool,
    stage: bool,
    terminator: u8,
    deduplicate: bool,
    oid_abbrev: Option<usize>,
    oid_candidates: &'a [ObjectId],
    eol: Option<&'a EolContext>,
    debug: bool,
    tag: bool,
}

fn ls_files_tag(entry: &sley_index::IndexEntry) -> char {
    if entry.is_skip_worktree() { 'S' } else { 'H' }
}

pub(crate) fn cmd_ls_tree(args: &[String]) -> Result<()> {
    let mut name_only = false;
    let mut name_status = false;
    let mut object_only = false;
    let mut long = false;
    let mut show_trees = false;
    let mut tree_only = false;
    let mut oid_abbrev = None;
    let mut recursive = false;
    let mut full_name = false;
    let mut full_name_implied_by_full_tree = false;
    let mut full_tree = false;
    let mut format_spec = None;
    let mut nul = false;
    let mut treeish = None;
    let mut pathspecs = Vec::new();
    let mut positional_only = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if positional_only {
            if treeish.is_none() {
                treeish = Some(arg.as_str());
            } else {
                pathspecs.push(arg.to_string());
            }
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "--name-only" => name_only = true,
            "--name-status" => name_status = true,
            "--object-only" => object_only = true,
            "--long" | "-l" => long = true,
            "-t" => show_trees = true,
            "-d" => tree_only = true,
            "--abbrev" => oid_abbrev = Some(7),
            "--no-abbrev" => oid_abbrev = None,
            "--full-name" => {
                full_name = true;
                full_name_implied_by_full_tree = false;
            }
            "--no-full-name" => {
                full_name = false;
                full_name_implied_by_full_tree = false;
            }
            "--full-tree" => {
                full_tree = true;
                if !full_name {
                    full_name = true;
                    full_name_implied_by_full_tree = true;
                }
            }
            "--no-full-tree" => {
                full_tree = false;
                if full_name_implied_by_full_tree {
                    full_name = false;
                    full_name_implied_by_full_tree = false;
                }
            }
            "--format" => {
                let Some(value) = iter.next() else {
                    return ls_tree_usage_error("option `format' requires a value");
                };
                format_spec = Some(value.as_str());
            }
            "-r" | "--recursive" => recursive = true,
            "-z" => nul = true,
            value if value.starts_with("--format=") => {
                format_spec = Some(&value["--format=".len()..]);
            }
            value if value.starts_with("--abbrev=") => {
                let width = value["--abbrev=".len()..].parse::<usize>().map_err(|_| {
                    eprintln!("error: invalid ls-tree abbreviation width {value}");
                    GitError::Exit(129)
                })?;
                oid_abbrev = (width != 0).then_some(width);
            }
            value if value.starts_with('-') => {
                let option = value.trim_start_matches('-');
                return ls_tree_usage_error(&format!("unknown option `{option}'"));
            }
            value => {
                if treeish.is_none() {
                    treeish = Some(value);
                } else {
                    pathspecs.push(value.to_string());
                }
            }
        }
    }
    let name_output = name_only || name_status;
    if format_spec.is_some() && (name_output || object_only || long) {
        return ls_tree_usage_error(
            "--format can't be combined with other format-altering options",
        );
    }
    if name_only && name_status {
        return ls_tree_usage_error(
            "options '--name-status' and '--name-only' cannot be used together",
        );
    }
    if name_output && object_only {
        return ls_tree_usage_error(
            "options '--object-only' and '--name-only' cannot be used together",
        );
    }
    if long && name_output {
        return ls_tree_usage_error("options '--name-only' and '--long' cannot be used together");
    }
    if long && object_only {
        return ls_tree_usage_error("options '--object-only' and '--long' cannot be used together");
    }
    let Some(treeish) = treeish else {
        return ls_tree_usage();
    };
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let format = repository_object_format(&git_dir)?;
    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
    let oid = resolve_revision(&git_dir, format, treeish)?;
    let tree_oid = sley_rev::peel_to_tree(&db, format, &oid)?;
    let object = db.read_object(&tree_oid)?;
    if object.object_type != ObjectType::Tree {
        return Err(GitError::InvalidObject(format!(
            "expected tree {tree_oid}, found {}",
            object.object_type.as_str()
        )));
    }
    let cwd = env::current_dir()?;
    let path_context = LsTreePathContext::new(&cwd, &git_dir, full_name, full_tree)?;
    let options = TreePrintOptions {
        name_only: name_output,
        object_only,
        long,
        show_trees,
        tree_only,
        oid_abbrev,
        format_spec,
        nul,
    };
    if recursive {
        if pathspecs.is_empty() {
            print_ls_tree_current_scope(&db, format, &object.body, &path_context, true, options)
        } else {
            print_tree_pathspecs(
                &db,
                format,
                &object.body,
                &pathspecs,
                &path_context,
                true,
                options,
            )
        }
    } else if pathspecs.is_empty() {
        print_ls_tree_current_scope(&db, format, &object.body, &path_context, false, options)
    } else {
        print_tree_pathspecs(
            &db,
            format,
            &object.body,
            &pathspecs,
            &path_context,
            false,
            options,
        )
    }
}

fn ls_tree_usage_error<T>(message: &str) -> Result<T> {
    eprintln!("error: {message}");
    Err(GitError::Exit(129))
}

fn ls_tree_usage<T>() -> Result<T> {
    eprintln!("usage: git ls-tree [<options>] <tree-ish> [<path>...]");
    Err(GitError::Exit(129))
}

pub(crate) fn cmd_update_index(args: &[String]) -> Result<()> {
    let mut add = false;
    let mut remove = false;
    let mut force_remove = false;
    let mut stdin = false;
    let mut nul = false;
    // `--chmod=(+|-)x` is a stateful flag in git: it sets `set_executable_bit`,
    // which then applies to every *subsequent* positional path until the next
    // `--chmod`. We snapshot the current value onto each path as it is parsed so
    // `--chmod=+x A --chmod=-x B` flips A executable and B non-executable.
    let mut chmod = None;
    let mut cacheinfo = Vec::new();
    let mut index_info = false;
    let mut info_only = false;
    let mut force_write_index = false;
    let mut ignore_skip_worktree_entries = false;
    let mut unresolve_only = false;
    let mut collect_unresolve_paths = false;
    let mut ignore_paths_after_unresolve = false;
    let mut unresolve_paths = Vec::new();
    let mut suppress_after_unresolve = false;
    let mut clear_resolve_undo = false;
    let mut test_untracked_cache = false;
    let mut untracked_cache = None;
    let mut fsmonitor = false;
    let mut verbose = false;
    let mut again = false;
    let mut refresh = false;
    let mut really_refresh = false;
    let mut refresh_ignore_missing = false;
    // Upstream git parses `--refresh`/`--really-refresh` as a callback that
    // fires the moment the flag is seen, so only a `-q` placed *before* the
    // refresh flag sets REFRESH_QUIET; a `-q` that comes after does not quiet
    // the refresh. Snapshot the quiet state at the point the refresh flag is
    // parsed to mirror that order-sensitivity.
    let mut refresh_quiet = false;
    let mut quiet = false;
    let mut ignore_missing = false;
    let mut assume_unchanged = None;
    let mut skip_worktree = None;
    let mut fsmonitor_valid = None;
    let mut index_version = None;
    let mut split_index = None;
    let mut positional_only = false;
    let mut allow_no_input = false;
    let mut show_index_version = false;
    let mut paths = Vec::new();
    // The sticky mode (`--add`/`--remove`/`--force-remove`/`--info-only`/
    // `--chmod`) in effect when each positional path was parsed, in lockstep
    // with `paths`. git processes argv left-to-right and applies whatever mode
    // is current to each path as it is seen, so `--add foo --force-remove bar`
    // adds foo and force-removes bar — the flags are positional, not global.
    let mut path_modes: Vec<sley_worktree::UpdateIndexPathMode> = Vec::new();
    let mut idx = 0;
    while idx < args.len() {
        if stdin || index_info {
            let option = if stdin { "stdin" } else { "index-info" };
            eprintln!("error: option '{option}' must be the last argument");
            return Err(GitError::Exit(129));
        }
        let arg = args[idx].as_str();
        if positional_only {
            if collect_unresolve_paths {
                unresolve_paths.push(PathBuf::from(arg));
            } else if !ignore_paths_after_unresolve {
                paths.push(PathBuf::from(arg));
                path_modes.push(sley_worktree::UpdateIndexPathMode {
                    add,
                    remove,
                    force_remove,
                    info_only,
                    chmod,
                });
            }
            idx += 1;
            continue;
        }
        match arg {
            "--" => {
                positional_only = true;
                allow_no_input = true;
            }
            "--add" => add = true,
            "--no-add" => add = false,
            "--remove" => remove = true,
            "--no-remove" => remove = false,
            "--force-remove" => force_remove = true,
            "--no-force-remove" => force_remove = false,
            "--info-only" => info_only = true,
            "--no-info-only" => info_only = false,
            "--verbose" => verbose = true,
            "--no-verbose" => verbose = false,
            "-g" | "--again" => {
                again = true;
                allow_no_input = true;
            }
            "--assume-unchanged" => assume_unchanged = Some(true),
            "--no-assume-unchanged" => assume_unchanged = Some(false),
            "--skip-worktree" => {
                skip_worktree = Some(true);
                allow_no_input = true;
            }
            "--no-skip-worktree" => {
                skip_worktree = Some(false);
                allow_no_input = true;
            }
            "--fsmonitor-valid" => {
                fsmonitor_valid = Some(true);
                allow_no_input = true;
            }
            "--no-fsmonitor-valid" => {
                fsmonitor_valid = Some(false);
                allow_no_input = true;
            }
            "--refresh" => {
                refresh = true;
                really_refresh = false;
                refresh_ignore_missing = ignore_missing;
                refresh_quiet = quiet;
                allow_no_input = true;
            }
            "--really-refresh" => {
                refresh = true;
                really_refresh = true;
                refresh_ignore_missing = ignore_missing;
                refresh_quiet = quiet;
                allow_no_input = true;
            }
            "--ignore-missing" => {
                ignore_missing = true;
                allow_no_input = true;
            }
            "--no-ignore-missing" => {
                ignore_missing = false;
                allow_no_input = true;
            }
            "--index-version" => {
                idx += 1;
                let Some(value) = args.get(idx) else {
                    return Err(GitError::Command("--index-version requires a value".into()));
                };
                index_version = Some(parse_update_index_version(value)?);
                allow_no_input = true;
            }
            value if value.starts_with("--index-version=") => {
                let value = value
                    .strip_prefix("--index-version=")
                    .expect("prefix checked by match guard");
                index_version = Some(parse_update_index_version(value)?);
                allow_no_input = true;
            }
            "--no-index-version" => {
                index_version = None;
                allow_no_input = true;
            }
            value if value.starts_with("--no-index-version=") => {
                eprintln!("error: option `no-index-version' takes no value");
                return Err(GitError::Exit(129));
            }
            "-q" => quiet = true,
            "--ignore-submodules"
            | "--no-ignore-submodules"
            | "--replace"
            | "--no-replace"
            | "--unmerged"
            | "--no-unmerged"
            | "--no-force-untracked-cache" => allow_no_input = true,
            "--split-index" => {
                split_index = Some(true);
                allow_no_input = true;
            }
            "--no-split-index" => {
                split_index = Some(false);
                allow_no_input = true;
            }
            "--untracked-cache" | "--force-untracked-cache" => {
                untracked_cache = Some(true);
                allow_no_input = true;
            }
            "--no-untracked-cache" => {
                untracked_cache = Some(false);
                allow_no_input = true;
            }
            "--unresolve" => {
                suppress_after_unresolve = true;
                unresolve_only = paths.is_empty();
                if unresolve_only {
                    collect_unresolve_paths = true;
                } else {
                    ignore_paths_after_unresolve = true;
                }
                allow_no_input = true;
            }
            "--fsmonitor" => {
                fsmonitor = true;
                allow_no_input = true;
            }
            "--no-fsmonitor" => {
                fsmonitor = false;
                allow_no_input = true;
            }
            "--test-untracked-cache" => {
                test_untracked_cache = true;
                allow_no_input = true;
            }
            "--no-test-untracked-cache" => {
                test_untracked_cache = false;
                allow_no_input = true;
            }
            "--ignore-skip-worktree-entries" => {
                ignore_skip_worktree_entries = true;
                allow_no_input = true;
            }
            "--no-ignore-skip-worktree-entries" => {
                ignore_skip_worktree_entries = false;
                allow_no_input = true;
            }
            "--force-write-index" => {
                force_write_index = true;
                allow_no_input = true;
            }
            "--no-force-write-index" => {
                force_write_index = false;
                allow_no_input = true;
            }
            "--clear-resolve-undo" => {
                clear_resolve_undo = true;
                allow_no_input = true;
            }
            "--show-index-version" => {
                show_index_version = true;
                allow_no_input = true;
            }
            "--no-show-index-version" => {
                show_index_version = false;
                allow_no_input = true;
            }
            "--stdin" => stdin = true,
            "--index-info" => index_info = true,
            "-z" => nul = true,
            "--chmod" => {
                idx += 1;
                let Some(value) = args.get(idx) else {
                    return Err(GitError::Command("--chmod requires a value".into()));
                };
                chmod = Some(parse_update_index_chmod(value)?);
            }
            value if value.starts_with("--chmod=") => {
                let value = value
                    .strip_prefix("--chmod=")
                    .expect("prefix checked by match guard");
                chmod = Some(parse_update_index_chmod(value)?);
            }
            "--cacheinfo" => {
                idx += 1;
                let Some(value) = args.get(idx) else {
                    return Err(GitError::Command("--cacheinfo requires a value".into()));
                };
                if value.contains(',') {
                    cacheinfo.push(parse_update_index_cacheinfo_tuple(value)?);
                } else {
                    let Some(oid) = args.get(idx + 1) else {
                        return Err(GitError::Command(
                            "--cacheinfo requires <mode> <object> <path>".into(),
                        ));
                    };
                    let Some(path) = args.get(idx + 2) else {
                        return Err(GitError::Command(
                            "--cacheinfo requires <mode> <object> <path>".into(),
                        ));
                    };
                    cacheinfo.push(parse_update_index_cacheinfo_split(value, oid, path)?);
                    idx += 2;
                }
            }
            value if value.starts_with("--cacheinfo=") => {
                let value = value
                    .strip_prefix("--cacheinfo=")
                    .expect("prefix checked by match guard");
                cacheinfo.push(parse_update_index_cacheinfo_tuple(value)?);
            }
            "-h" | "--help" => return update_index_usage_help(),
            // git's parse_options() rejects any unrecognized option with
            // `error: unknown {option|switch} ...` followed by the usage text on
            // stderr, exiting 129. A lone `-` is a valid path, not an option.
            value if value.starts_with('-') && value != "-" => {
                if let Some(long) = value.strip_prefix("--") {
                    eprintln!("error: unknown option '{long}'");
                } else {
                    let switch = value.chars().nth(1).unwrap_or('-');
                    eprintln!("error: unknown switch '{switch}'");
                }
                return update_index_usage_error();
            }
            value => {
                if collect_unresolve_paths {
                    unresolve_paths.push(PathBuf::from(value));
                } else if !ignore_paths_after_unresolve {
                    paths.push(PathBuf::from(value));
                    path_modes.push(sley_worktree::UpdateIndexPathMode {
                        add,
                        remove,
                        force_remove,
                        info_only,
                        chmod,
                    });
                }
            }
        }
        idx += 1;
    }
    if stdin || index_info {
        let mut input = Vec::new();
        io::stdin().read_to_end(&mut input)?;
        if stdin {
            let stdin_paths = update_index_stdin_paths(&input, nul);
            // Keep `path_modes` in lockstep with `paths`: stdin paths inherit
            // the sticky mode in effect at the `--stdin` flag (git applies the
            // current add/remove/force_remove/info_only + set_executable_bit to
            // every stdin path too). Without this the later `paths`/`path_modes`
            // zip would truncate and silently drop the stdin paths.
            let stdin_mode = sley_worktree::UpdateIndexPathMode {
                add,
                remove,
                force_remove,
                info_only,
                chmod,
            };
            path_modes.extend(std::iter::repeat_n(stdin_mode, stdin_paths.len()));
            paths.extend(stdin_paths);
        } else {
            let cwd = env::current_dir()?;
            let git_dir = discover_git_dir(&cwd)?;
            let format = repository_object_format(&git_dir)?;
            let records = parse_update_index_index_info(&input)?
                .into_iter()
                .map(|record| record.into_worktree_record(format))
                .collect::<Result<Vec<_>>>()?;
            sley_worktree::update_index_index_info(git_dir, format, &records)?;
            return Ok(());
        }
    }
    if paths.is_empty()
        && unresolve_paths.is_empty()
        && cacheinfo.is_empty()
        && !refresh
        && !again
        && index_version.is_none()
        && split_index.is_none()
        && !force_write_index
        && untracked_cache.is_none()
    {
        if (stdin || allow_no_input) && !refresh {
            if unresolve_only {
                return Ok(());
            }
            let git_dir =
                if show_index_version || fsmonitor || split_index.is_some() || clear_resolve_undo {
                    let cwd = env::current_dir()?;
                    Some(discover_git_dir(&cwd)?)
                } else {
                    None
                };
            if clear_resolve_undo && let Some(git_dir) = &git_dir {
                let format = repository_object_format(git_dir)?;
                sley_worktree::clear_resolve_undo(git_dir, format)?;
            }
            if let (Some(split_index), Some(git_dir)) = (split_index, &git_dir) {
                let format = repository_object_format(git_dir)?;
                if split_index {
                    sley_worktree::enable_split_index(git_dir, format)?;
                } else {
                    sley_worktree::disable_split_index(git_dir, format)?;
                }
            }
            if fsmonitor && let Some(git_dir) = &git_dir {
                let format = repository_object_format(git_dir)?;
                sley_worktree::force_write_index(git_dir, format)?;
            }
            if show_index_version
                && !suppress_after_unresolve
                && let Some(git_dir) = &git_dir
            {
                print_update_index_version(git_dir)?;
            }
            if test_untracked_cache && !suppress_after_unresolve {
                print_test_untracked_cache_result(&env::current_dir()?)?;
            }
            if fsmonitor && !suppress_after_unresolve {
                print_update_index_fsmonitor_unset_warning();
            }
            return Ok(());
        }
        // No paths, no cacheinfo/unresolve, no refresh/again, no index-mutating
        // flag: there is nothing to update. git's `update-index` treats this as a
        // no-op success (`git update-index`, or a sticky `--add`/`--verbose` with
        // no paths, all exit 0 without touching the index), so mirror that rather
        // than rejecting it.
        return Ok(());
    }
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let worktree_required = refresh
        || again
        || fsmonitor_valid.is_some()
        || skip_worktree.is_some()
        || assume_unchanged.is_some()
        || !paths.is_empty()
        || unresolve_only
        || test_untracked_cache
        || untracked_cache.is_some();
    let worktree_root = match worktree_root_for_git_dir(&git_dir) {
        Ok(root) => root,
        Err(_) if !worktree_required => cwd.clone(),
        Err(err) => return Err(err),
    };
    let resolved_paths = paths
        .iter()
        .map(|path| {
            if path.is_absolute() {
                path.clone()
            } else {
                cwd.join(path)
            }
        })
        .collect::<Vec<_>>();
    let resolved_unresolve_paths = unresolve_paths
        .iter()
        .map(|path| {
            if path.is_absolute() {
                path.clone()
            } else {
                cwd.join(path)
            }
        })
        .collect::<Vec<_>>();
    // Pair each resolved path with the sticky mode active when it was parsed,
    // preserving command-line order for the staging branch below. The mode
    // (add/remove/force-remove/info-only/chmod) is positional in git, so it
    // must travel with each path rather than be applied batch-wide.
    let ordered_paths = resolved_paths
        .iter()
        .cloned()
        .zip(path_modes.iter().copied())
        .map(|(path, mode)| sley_worktree::UpdateIndexPath { path, mode })
        .collect::<Vec<_>>();
    if clear_resolve_undo {
        sley_worktree::clear_resolve_undo(&git_dir, format)?;
    }
    if unresolve_only {
        sley_worktree::unresolve_index_paths(
            &worktree_root,
            &git_dir,
            format,
            &resolved_unresolve_paths,
        )?;
        return Ok(());
    }
    if let Some(enabled) = untracked_cache {
        if enabled {
            sley_worktree::enable_untracked_cache(&worktree_root, &git_dir, format)?;
        } else {
            sley_worktree::disable_untracked_cache(&git_dir, format)?;
        }
    }
    if refresh {
        sley_worktree::refresh_index_paths(
            &worktree_root,
            git_dir.clone(),
            format,
            &resolved_paths,
            refresh_quiet,
            refresh_ignore_missing,
            really_refresh,
        )?;
        // Unmerged entries make the refresh fail (`<path>: needs merge`).
        let index_path = sley_worktree::repository_index_path(&git_dir);
        if index_path.exists() {
            let index = Index::parse(&fs::read(&index_path)?, format)?;
            let mut unmerged: Vec<String> = index
                .entries
                .iter()
                .filter(|entry| index_entry_stage(entry) > 0)
                .map(|entry| entry.path.to_string())
                .collect();
            unmerged.dedup();
            if !unmerged.is_empty() {
                if !refresh_quiet {
                    for path in &unmerged {
                        println!("{path}: needs merge");
                    }
                }
                return Err(GitError::Exit(1));
            }
        }
    } else if again {
        sley_worktree::update_index_again(
            &worktree_root,
            git_dir.clone(),
            format,
            &resolved_paths,
            sley_worktree::UpdateIndexOptions {
                add,
                remove,
                force_remove,
                chmod,
                info_only,
                ignore_skip_worktree_entries,
                allow_skip_worktree_entries: false,
            },
        )?;
    } else if let Some(index_version) = index_version {
        sley_worktree::set_index_version(git_dir.clone(), format, index_version, verbose)?;
    } else if let Some(fsmonitor_valid) = fsmonitor_valid {
        sley_worktree::set_index_fsmonitor_valid_paths(
            &worktree_root,
            git_dir.clone(),
            format,
            &resolved_paths,
            fsmonitor_valid,
        )?;
    } else if let Some(skip_worktree) = skip_worktree {
        sley_worktree::set_index_skip_worktree_paths(
            &worktree_root,
            git_dir.clone(),
            format,
            &resolved_paths,
            skip_worktree,
        )?;
    } else if let Some(assume_unchanged) = assume_unchanged {
        sley_worktree::set_index_assume_unchanged_paths(
            &worktree_root,
            git_dir.clone(),
            format,
            &resolved_paths,
            assume_unchanged,
        )?;
    } else if !ordered_paths.is_empty() {
        let config = read_repo_config(&git_dir)?;
        sley_worktree::update_index_ordered_paths_filtered(
            &worktree_root,
            git_dir.clone(),
            format,
            &ordered_paths,
            // The positional mode (add/remove/force_remove/info_only/chmod) is
            // now carried per-path in `ordered_paths`; only the genuinely
            // whole-invocation `ignore_skip_worktree_entries` is read off the
            // batch options here.
            sley_worktree::UpdateIndexOptions {
                add: false,
                remove: false,
                force_remove: false,
                chmod: None,
                info_only: false,
                ignore_skip_worktree_entries,
                allow_skip_worktree_entries: false,
            },
            &config,
            verbose,
        )?;
    }
    if !cacheinfo.is_empty() {
        let cacheinfo = cacheinfo
            .into_iter()
            .map(|entry| entry.into_worktree_entry(format))
            .collect::<Result<Vec<_>>>()?;
        sley_worktree::update_index_cacheinfo(&git_dir, format, &cacheinfo, add, verbose)?;
    }
    if let Some(split_index) = split_index {
        if split_index {
            sley_worktree::enable_split_index(&git_dir, format)?;
        } else {
            sley_worktree::disable_split_index(&git_dir, format)?;
        }
    } else if let Some(config_split_index) =
        read_repo_config(&git_dir)?.get_bool("core", None, "splitIndex")
    {
        if config_split_index {
            sley_worktree::enable_split_index(&git_dir, format)?;
        } else {
            sley_worktree::disable_split_index(&git_dir, format)?;
        }
    }
    if show_index_version && !suppress_after_unresolve {
        print_update_index_version(&git_dir)?;
    }
    if force_write_index {
        sley_worktree::force_write_index(&git_dir, format)?;
    }
    if test_untracked_cache && !suppress_after_unresolve {
        print_test_untracked_cache_result(&worktree_root)?;
    }
    if fsmonitor && !suppress_after_unresolve {
        print_update_index_fsmonitor_unset_warning();
    }
    crate::commands::hooks::run_hook(
        "post-index-change",
        crate::commands::hooks::HookRun::default(),
    )?;
    Ok(())
}

const UPDATE_INDEX_USAGE: &str = "\
usage: git update-index [<options>] [--] [<file>...]

    -q                    continue refresh even when index needs update
    --[no-]ignore-submodules
                          refresh: ignore submodules
    --[no-]add            do not ignore new files
    --[no-]replace        let files replace directories and vice-versa
    --[no-]remove         notice files missing from worktree
    --[no-]unmerged       refresh even if index contains unmerged entries
    --refresh             refresh stat information
    --really-refresh      like --refresh, but ignore assume-unchanged setting
    --cacheinfo <mode>,<object>,<path>
                          add the specified entry to the index
    --chmod (+|-)x        override the executable bit of the listed files
    --assume-unchanged    mark files as \"not changing\"
    --no-assume-unchanged clear assumed-unchanged bit
    --skip-worktree       mark files as \"index-only\"
    --no-skip-worktree    clear skip-worktree bit
    --[no-]ignore-skip-worktree-entries
                          do not touch index-only entries
    --[no-]info-only      add to index only; do not add content to object database
    --[no-]force-remove   remove named paths even if present in worktree
    -z                    with --stdin: input lines are terminated by null bytes
    --stdin               read list of paths to be updated from standard input
    --index-info          add entries from standard input to the index
    --unresolve           repopulate stages #2 and #3 for the listed paths
    -g, --again           only update entries that differ from HEAD
    --[no-]ignore-missing ignore files missing from worktree
    --[no-]verbose        report actions to standard output
    --clear-resolve-undo  (for porcelains) forget saved unresolved conflicts
    --[no-]index-version <n>
                          write index in this format
    --[no-]show-index-version
                          report on-disk index format version
    --[no-]split-index    enable or disable split index
    --[no-]untracked-cache
                          enable/disable untracked cache
    --[no-]test-untracked-cache
                          test if the filesystem supports untracked cache
    --[no-]force-untracked-cache
                          enable untracked cache without testing the filesystem
    --[no-]force-write-index
                          write out the index even if is not flagged as changed
    --[no-]fsmonitor      enable or disable file system monitor
    --fsmonitor-valid     mark files as fsmonitor valid
    --no-fsmonitor-valid  clear fsmonitor valid bit";

/// Print the `update-index` usage text and exit 129 — the error path git takes
/// for an unknown option/switch (after the `error: unknown ...` line).
fn update_index_usage_error<T>() -> Result<T> {
    eprintln!("{UPDATE_INDEX_USAGE}");
    Err(GitError::Exit(129))
}

/// Print the `update-index` usage text and exit 129 — git's `-h`/`--help` path.
fn update_index_usage_help<T>() -> Result<T> {
    eprintln!("{UPDATE_INDEX_USAGE}");
    Err(GitError::Exit(129))
}

fn print_test_untracked_cache_result(worktree_root: &Path) -> Result<()> {
    let display_path =
        fs::canonicalize(worktree_root).unwrap_or_else(|_| worktree_root.to_path_buf());
    eprintln!(
        "Testing mtime in '{}' ...... OK",
        display_path.to_string_lossy()
    );
    Ok(())
}

fn print_update_index_fsmonitor_unset_warning() {
    eprintln!("warning: core.fsmonitor is unset; set it if you really want to enable fsmonitor");
}

fn print_update_index_version(git_dir: &Path) -> Result<()> {
    let index_path = sley_worktree::repository_index_path(git_dir);
    let version = if index_path.exists() {
        read_index_header_version(&fs::read(index_path)?)?
    } else {
        0
    };
    println!("{version}");
    Ok(())
}

fn read_index_header_version(bytes: &[u8]) -> Result<u32> {
    if bytes.len() < 12 {
        return Err(GitError::InvalidFormat("index header too short".into()));
    }
    if &bytes[..4] != b"DIRC" {
        return Err(GitError::InvalidFormat("missing DIRC signature".into()));
    }
    Ok(u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]))
}

fn parse_update_index_version(value: &str) -> Result<u32> {
    value
        .parse::<u32>()
        .map_err(|_| GitError::Command(format!("invalid update-index --index-version {value}")))
}

#[derive(Debug, Clone)]
struct CliCacheInfoEntry {
    mode: u32,
    oid: String,
    path: String,
}

impl CliCacheInfoEntry {
    fn into_worktree_entry(self, format: ObjectFormat) -> Result<sley_worktree::CacheInfoEntry> {
        Ok(sley_worktree::CacheInfoEntry {
            mode: self.mode,
            oid: ObjectId::from_hex(format, &self.oid)?,
            path: self.path.into_bytes(),
            stage: 0,
        })
    }
}

#[derive(Debug, Clone)]
enum CliIndexInfoRecord {
    Add {
        mode: u32,
        oid: String,
        stage: u16,
        path: Vec<u8>,
    },
    Remove {
        path: Vec<u8>,
    },
}

impl CliIndexInfoRecord {
    fn into_worktree_record(self, format: ObjectFormat) -> Result<sley_worktree::IndexInfoRecord> {
        match self {
            Self::Add {
                mode,
                oid,
                stage,
                path,
            } => Ok(sley_worktree::IndexInfoRecord::Add(
                sley_worktree::CacheInfoEntry {
                    mode,
                    oid: ObjectId::from_hex(format, &oid)?,
                    path,
                    stage,
                },
            )),
            Self::Remove { path } => Ok(sley_worktree::IndexInfoRecord::Remove { path }),
        }
    }
}

fn parse_update_index_chmod(value: &str) -> Result<bool> {
    match value {
        "+x" => Ok(true),
        "-x" => Ok(false),
        _ => Err(GitError::Command(format!(
            "unsupported update-index --chmod value {value}"
        ))),
    }
}

fn parse_update_index_cacheinfo_tuple(value: &str) -> Result<CliCacheInfoEntry> {
    let mut parts = value.splitn(3, ',');
    let Some(mode) = parts.next() else {
        return Err(GitError::Command(
            "--cacheinfo requires <mode>,<object>,<path>".into(),
        ));
    };
    let Some(oid) = parts.next() else {
        return Err(GitError::Command(
            "--cacheinfo requires <mode>,<object>,<path>".into(),
        ));
    };
    let Some(path) = parts.next() else {
        return Err(GitError::Command(
            "--cacheinfo requires <mode>,<object>,<path>".into(),
        ));
    };
    parse_update_index_cacheinfo_split(mode, oid, path)
}

fn parse_update_index_cacheinfo_split(
    mode: &str,
    oid: &str,
    path: &str,
) -> Result<CliCacheInfoEntry> {
    let mode = u32::from_str_radix(mode, 8)
        .map_err(|_| GitError::Command(format!("invalid update-index --cacheinfo mode {mode}")))?;
    if mode == sley_index::SPARSE_DIR_MODE && path.ends_with('/') {
        eprintln!("error: option 'cacheinfo' cannot add sparse directory '{path}'");
        return Err(GitError::Exit(128));
    }
    Ok(CliCacheInfoEntry {
        mode,
        oid: oid.to_string(),
        path: path.to_string(),
    })
}

fn parse_update_index_index_info(input: &[u8]) -> Result<Vec<CliIndexInfoRecord>> {
    let mut records = Vec::new();
    for line in input.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let tab = line.iter().position(|byte| *byte == b'\t').ok_or_else(|| {
            GitError::Command("update-index --index-info requires tab-separated paths".into())
        })?;
        let metadata = &line[..tab];
        let path = &line[tab + 1..];
        let metadata = String::from_utf8_lossy(metadata);
        let fields = metadata.split_whitespace().collect::<Vec<_>>();
        if !(fields.len() == 2 || fields.len() == 3) {
            return Err(GitError::Command(
                "update-index --index-info requires <mode> <object> [<stage>]".into(),
            ));
        }
        let mode = u32::from_str_radix(fields[0], 8).map_err(|_| {
            GitError::Command(format!(
                "invalid update-index --index-info mode {}",
                fields[0]
            ))
        })?;
        if mode == 0 {
            records.push(CliIndexInfoRecord::Remove {
                path: path.to_vec(),
            });
            continue;
        }
        let stage = if let Some(stage) = fields.get(2) {
            stage.parse::<u16>().map_err(|_| {
                GitError::Command(format!("invalid update-index --index-info stage {stage}"))
            })?
        } else {
            0
        };
        if stage > 3 {
            return Err(GitError::Command(format!(
                "invalid update-index --index-info stage {stage}"
            )));
        }
        records.push(CliIndexInfoRecord::Add {
            mode,
            oid: fields[1].to_string(),
            stage,
            path: path.to_vec(),
        });
    }
    Ok(records)
}

fn update_index_stdin_paths(input: &[u8], nul: bool) -> Vec<PathBuf> {
    let separator = if nul { b'\0' } else { b'\n' };
    input
        .split(|byte| *byte == separator)
        .filter(|path| !path.is_empty())
        .map(|path| PathBuf::from(String::from_utf8_lossy(path).into_owned()))
        .collect()
}
