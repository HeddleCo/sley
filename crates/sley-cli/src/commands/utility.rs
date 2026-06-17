//! Extracted from the crate root (sley#8 phase 1) — code motion only.

// A glob of the crate root brings every shared helper/type into scope via
// descendant-privacy; see commands::stash for the rationale.
use crate::*;

pub(crate) fn cmd_version(args: &[String]) -> Result<()> {
    // `git version` ignores positional arguments and prints the version line; the
    // only flag it acts on is `--build-options`, which appends a block of build
    // facts. Upstream's test harness (t/test-lib.sh) parses that block for the
    // active hash (`default-hash:`) and integer widths (`sizeof-*`), so the line
    // shapes must match git's exactly.
    let build_options = args.iter().any(|arg| arg == "--build-options");
    println!("git version {}", sley_core::UPSTREAM_GIT_COMPAT_VERSION);
    if build_options {
        print_version_build_options();
    }
    Ok(())
}

pub(crate) fn cmd_bugreport(args: &[String]) -> Result<()> {
    let mut suffix = Some("report".to_string());
    let mut output_dir = PathBuf::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "-s" | "--suffix" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err(GitError::Exit(129));
                };
                suffix = Some(value.clone());
            }
            value if value.starts_with("--suffix=") => {
                suffix = Some(value["--suffix=".len()..].to_string());
            }
            "--no-suffix" => {
                suffix = None;
            }
            "-o" | "--output-directory" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err(GitError::Exit(129));
                };
                output_dir = PathBuf::from(value);
            }
            value if value.starts_with("--output-directory=") => {
                output_dir = PathBuf::from(&value["--output-directory=".len()..]);
            }
            "-h" | "--help" => {
                println!(
                    "usage: git bugreport [(-o | --output-directory) <path>] [(-s | --suffix) <format> | --no-suffix]"
                );
                return Ok(());
            }
            value if value.starts_with('-') => return bugreport_usage_error(None),
            value => return bugreport_usage_error(Some(value)),
        }
        index += 1;
    }
    let file_name = match suffix {
        Some(suffix) => format!("git-bugreport-{suffix}.txt"),
        None => "git-bugreport.txt".to_string(),
    };
    if !output_dir.as_os_str().is_empty() {
        fs::create_dir_all(&output_dir)?;
    }
    let path = output_dir.join(file_name);
    let mut report = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    report.write_all(b"Thank you for filling out a Git bug report!\n")?;
    report
        .write_all(b"Please answer the following questions to help us understand your issue.\n")?;
    report.write_all(b"\n")?;
    report
        .write_all(b"What did you do before the bug happened? (Steps to reproduce your issue)\n")?;
    report.write_all(b"\n")?;
    report.write_all(b"What did you expect to happen? (Expected behavior)\n")?;
    report.write_all(b"\n")?;
    report.write_all(b"What happened instead? (Actual behavior)\n")?;
    report.write_all(b"\n")?;
    report
        .write_all(b"What's different between what you expected and what actually happened?\n")?;
    report.write_all(b"\n")?;
    report.write_all(b"Anything else you want to add:\n")?;
    report.write_all(b"\n")?;
    report.write_all(b"Please review the rest of the bug report below.\n")?;
    report.write_all(b"You can delete any lines you don't wish to share.\n")?;
    report.write_all(b"\n")?;
    report.write_all(b"\n\n[System Info]\n")?;
    writeln!(
        report,
        "git version {}",
        sley_core::UPSTREAM_GIT_COMPAT_VERSION
    )?;
    writeln!(report, "shell-path: /bin/sh")?;
    writeln!(report, "uname: {} {}", env::consts::OS, env::consts::ARCH)?;
    writeln!(report, "compiler info: rustc")?;
    writeln!(report, "zlib: available")?;
    report.write_all(b"\n\n[Enabled Hooks]\n")?;
    match discover_git_dir(env::current_dir()?) {
        Ok(_) => {
            for hook in commands::hooks::KNOWN_HOOKS {
                if commands::hooks::hook_exists(hook)? {
                    writeln!(report, "{hook}")?;
                }
            }
        }
        Err(_) => {
            report.write_all(b"not run from a git repository - no hooks to show\n")?;
        }
    }
    eprintln!("Created new report at '{}'.", path.display());
    Ok(())
}

fn bugreport_usage_error(arg: Option<&str>) -> Result<()> {
    if let Some(arg) = arg {
        eprintln!("error: unknown argument `{arg}`");
    }
    eprintln!(
        "usage: git bugreport [(-o | --output-directory) <path>] [(-s | --suffix) <format> | --no-suffix]"
    );
    Err(GitError::Exit(129))
}

fn print_version_build_options() {
    println!("cpu: {}", std::env::consts::ARCH);
    println!("sizeof-long: {}", std::mem::size_of::<std::ffi::c_long>());
    println!("sizeof-size_t: {}", std::mem::size_of::<usize>());
    println!("shell-path: /bin/sh");
    // sley creates `files`-backed ref storage and hashes with SHA-1 by default;
    // these two lines are what upstream test-lib.sh consumes to prime its oid
    // database and select the default ref format.
    println!("default-ref-format: files");
    println!("default-hash: {}", ObjectFormat::Sha1.name());
}

pub(crate) fn cmd_var(args: &[String]) -> Result<()> {
    match args {
        [name] if name == "-l" => {
            var_list()?;
            Ok(())
        }
        [name] => {
            let value = var_value(name)?;
            println!("{value}");
            Ok(())
        }
        _ => var_usage(),
    }
}

fn var_list() -> Result<()> {
    if let Some(config) = identity_effective_config() {
        var_print_config(&config)?;
    }
    for param in injected_config_parameters()? {
        // `git var -l` prints injected overrides as `key=value`; a bare
        // boolean-true entry renders with an empty value, matching git.
        println!(
            "{}={}",
            param.canonical_key,
            param.value.as_deref().unwrap_or("")
        );
    }
    for name in [
        "GIT_COMMITTER_IDENT",
        "GIT_AUTHOR_IDENT",
        "GIT_EDITOR",
        "GIT_SEQUENCE_EDITOR",
        "GIT_PAGER",
        "GIT_DEFAULT_BRANCH",
        "GIT_SHELL_PATH",
    ] {
        if let Ok(value) = var_value(name) {
            println!("{name}={value}");
        }
    }
    Ok(())
}

fn var_print_config(config: &GitConfig) -> Result<()> {
    for section in &config.sections {
        for entry in &section.entries {
            let name = config_entry_name(section, &entry.key).to_ascii_lowercase();
            if let Some(value) = &entry.value {
                println!("{name}={value}");
            } else {
                println!("{name}");
            }
        }
    }
    Ok(())
}

fn var_value(name: &str) -> Result<String> {
    match name {
        "GIT_AUTHOR_IDENT" => var_identity("AUTHOR"),
        "GIT_COMMITTER_IDENT" => var_identity("COMMITTER"),
        "GIT_EDITOR" => var_editor(None),
        "GIT_SEQUENCE_EDITOR" => var_editor(Some("sequence.editor")),
        "GIT_PAGER" => Ok(var_pager()),
        "GIT_DEFAULT_BRANCH" => Ok(var_default_branch()),
        "GIT_SHELL_PATH" => Ok("/bin/sh".into()),
        _ => var_usage(),
    }
}

fn var_identity(role: &str) -> Result<String> {
    let identity = commit_identity_from_env(role)?;
    Ok(String::from_utf8_lossy(&identity).into_owned())
}

fn var_editor(specific_key: Option<&str>) -> Result<String> {
    if let Some(key) = specific_key {
        if let Ok(value) = env::var("GIT_SEQUENCE_EDITOR") {
            return Ok(value);
        }
        if let Some(value) = var_effective_config_value(key) {
            return Ok(value);
        }
    }
    if let Ok(value) = env::var("GIT_EDITOR") {
        return Ok(value);
    }
    if let Some(value) = var_effective_config_value("core.editor") {
        return Ok(value);
    }
    if let Ok(value) = env::var("VISUAL")
        && !value.is_empty()
        && env::var("TERM").is_ok_and(|term| term != "dumb")
    {
        return Ok(value);
    }
    if let Ok(value) = env::var("EDITOR") {
        return Ok(value);
    }
    Err(GitError::Exit(1))
}

fn var_pager() -> String {
    env::var("GIT_PAGER")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| var_effective_config_value("core.pager"))
        .unwrap_or_else(|| "cat".into())
}

fn var_default_branch() -> String {
    // git's `repo_default_branch_name`: the test override env var wins over
    // the `init.defaultBranch` configuration.
    if let Ok(env) = env::var("GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME")
        && !env.is_empty()
    {
        return env;
    }
    var_effective_config_value("init.defaultBranch").unwrap_or_else(|| "master".into())
}

fn var_effective_config_value(key: &str) -> Option<String> {
    if let Ok(Some(value)) = global_config_value(key) {
        return Some(value);
    }
    let (section, key) = key.split_once('.')?;
    identity_effective_config().and_then(|config| config.get(section, None, key).map(str::to_owned))
}

fn var_usage<T>() -> Result<T> {
    eprintln!("usage: git var (-l | <variable>)");
    Err(GitError::Exit(129))
}

pub(crate) fn cmd_get_tar_commit_id(args: &[String]) -> Result<()> {
    if !args.is_empty() {
        eprintln!("usage: git get-tar-commit-id");
        return Err(GitError::Exit(129));
    }
    let mut input = Vec::new();
    io::stdin().read_to_end(&mut input)?;
    match tar_commit_id(&input)? {
        Some(commit_id) => {
            println!("{commit_id}");
            Ok(())
        }
        None => Err(GitError::Exit(1)),
    }
}

fn tar_commit_id(input: &[u8]) -> Result<Option<String>> {
    let mut offset = 0usize;
    loop {
        if input.len().saturating_sub(offset) < 512 {
            eprintln!(
                "fatal: git get-tar-commit-id: EOF before reading tar header: No such file or directory"
            );
            return Err(GitError::Exit(128));
        }
        let header = &input[offset..offset + 512];
        offset += 512;
        if header.iter().all(|byte| *byte == 0) {
            return Ok(None);
        }
        let size = tar_header_size(header)?;
        let typeflag = header[156];
        if input.len().saturating_sub(offset) < size {
            eprintln!(
                "fatal: git get-tar-commit-id: EOF before reading tar header: No such file or directory"
            );
            return Err(GitError::Exit(128));
        }
        let body = &input[offset..offset + size];
        if typeflag == b'g'
            && let Some(commit_id) = pax_comment_commit_id(body)
        {
            return Ok(Some(commit_id));
        }
        let padded = size.div_ceil(512) * 512;
        if input.len().saturating_sub(offset) < padded {
            eprintln!(
                "fatal: git get-tar-commit-id: EOF before reading tar header: No such file or directory"
            );
            return Err(GitError::Exit(128));
        }
        offset += padded;
    }
}

fn tar_header_size(header: &[u8]) -> Result<usize> {
    let field = &header[124..136];
    let text = String::from_utf8_lossy(field);
    let digits = text
        .trim_matches(char::from(0))
        .trim()
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>();
    if digits.is_empty() {
        return Ok(0);
    }
    usize::from_str_radix(&digits, 8)
        .map_err(|_| GitError::InvalidFormat("invalid tar size".into()))
}

fn pax_comment_commit_id(body: &[u8]) -> Option<String> {
    let mut offset = 0usize;
    while offset < body.len() {
        let relative_space = body[offset..].iter().position(|byte| *byte == b' ')?;
        let space = offset + relative_space;
        let length = std::str::from_utf8(&body[offset..space])
            .ok()?
            .parse::<usize>()
            .ok()?;
        if length == 0 || offset + length > body.len() {
            return None;
        }
        let record = &body[space + 1..offset + length];
        if let Some(value) = record
            .strip_prefix(b"comment=")
            .and_then(|value| value.strip_suffix(b"\n"))
            && value.iter().all(|byte| byte.is_ascii_hexdigit())
        {
            return Some(String::from_utf8_lossy(value).into_owned());
        }
        offset += length;
    }
    None
}

pub(crate) fn cmd_unpack_file(args: &[String]) -> Result<()> {
    let [name] = args else {
        eprintln!("usage: git unpack-file <blob>");
        return Err(GitError::Exit(129));
    };
    let git_dir = discover_git_dir(env::current_dir()?)?;
    let format = repository_object_format(&git_dir)?;
    let oid = match resolve_revision(&git_dir, format, name) {
        Ok(oid) => oid,
        Err(_) => {
            eprintln!("fatal: Not a valid object name {name}");
            return Err(GitError::Exit(128));
        }
    };
    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
    let object = db.read_object(&oid)?;
    if object.object_type != ObjectType::Blob {
        eprintln!("fatal: unable to read blob object {oid}");
        return Err(GitError::Exit(128));
    }
    let path = write_unpack_file_temp(&object.body)?;
    println!("{}", path.display());
    Ok(())
}

fn write_unpack_file_temp(contents: &[u8]) -> Result<PathBuf> {
    let cwd = env::current_dir()?;
    for attempt in 0..1024u32 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let name = format!(
            ".merge_file_{:x}{:x}{:x}",
            std::process::id(),
            nanos,
            attempt
        );
        let path = cwd.join(&name);
        let mut file = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(GitError::Io(err.to_string())),
        };
        file.write_all(contents)?;
        return Ok(PathBuf::from(name));
    }
    Err(GitError::Io(
        "unable to create temporary unpack file".into(),
    ))
}

pub(crate) fn cmd_show_index(args: &[String]) -> Result<()> {
    let mut format = ObjectFormat::Sha1;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--object-format" => {
                let Some(value) = iter.next() else {
                    eprintln!("error: option `object-format' requires a value");
                    return Err(GitError::Exit(129));
                };
                format = parse_show_index_object_format(value)?;
            }
            "--no-object-format" => format = ObjectFormat::Sha1,
            value if value.starts_with("--object-format=") => {
                format = parse_show_index_object_format(&value["--object-format=".len()..])?;
            }
            value if value.starts_with('-') => {
                eprintln!("error: unknown option `{}'", value.trim_start_matches('-'));
                return show_index_usage();
            }
            _ => {}
        }
    }
    let mut input = Vec::new();
    io::stdin().read_to_end(&mut input)?;
    if input.len() < 8 {
        eprintln!("fatal: unable to read header");
        return Err(GitError::Exit(128));
    }
    let index = match PackIndex::parse(&input, format) {
        Ok(index) => index,
        Err(_) => {
            eprintln!("fatal: unable to read header");
            return Err(GitError::Exit(128));
        }
    };
    for entry in index.entries {
        println!("{} {} ({:08x})", entry.offset, entry.oid, entry.crc32);
    }
    Ok(())
}

fn parse_show_index_object_format(value: &str) -> Result<ObjectFormat> {
    match value {
        "sha1" => Ok(ObjectFormat::Sha1),
        "sha256" => Ok(ObjectFormat::Sha256),
        _ => {
            eprintln!("fatal: Unknown hash algorithm");
            Err(GitError::Exit(128))
        }
    }
}

fn show_index_usage<T>() -> Result<T> {
    eprintln!("usage: git show-index [--object-format=<hash-algorithm>] < <pack-idx-file>");
    eprintln!();
    eprintln!("    --[no-]object-format <hash-algorithm>");
    eprintln!("                          specify the hash algorithm to use");
    eprintln!();
    Err(GitError::Exit(129))
}

pub(crate) fn cmd_check_mailmap(args: &[String]) -> Result<()> {
    let mut stdin = false;
    let mut source_specs = Vec::new();
    let mut contacts = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--stdin" => stdin = true,
            "--no-stdin" => stdin = false,
            "--mailmap-file" => {
                let Some(path) = iter.next() else {
                    eprintln!("error: option `mailmap-file' requires a value");
                    return Err(GitError::Exit(129));
                };
                source_specs.push(MailmapSourceSpec::File(PathBuf::from(path)));
            }
            "--no-mailmap-file" => {}
            "--mailmap-blob" => {
                let Some(rev) = iter.next() else {
                    eprintln!("error: option `mailmap-blob' requires a value");
                    return Err(GitError::Exit(129));
                };
                source_specs.push(MailmapSourceSpec::Blob(rev.to_string()));
            }
            "--no-mailmap-blob" => {}
            value if value.starts_with("--mailmap-file=") => {
                source_specs.push(MailmapSourceSpec::File(PathBuf::from(
                    &value["--mailmap-file=".len()..],
                )));
            }
            value if value.starts_with("--mailmap-blob=") => {
                source_specs.push(MailmapSourceSpec::Blob(
                    value["--mailmap-blob=".len()..].to_string(),
                ));
            }
            value if value.starts_with('-') => {
                eprintln!("error: unknown option `{}'", value.trim_start_matches('-'));
                return check_mailmap_usage();
            }
            value => contacts.push(value.to_string()),
        }
    }
    if stdin {
        let mut input = String::new();
        io::stdin().read_to_string(&mut input)?;
        contacts.extend(input.lines().map(str::to_string));
    }
    if contacts.is_empty() {
        eprintln!("fatal: no contacts specified");
        return Err(GitError::Exit(128));
    }

    let git_dir = discover_git_dir(env::current_dir()?)?;
    let format = repository_object_format(&git_dir)?;
    let mailmap = Mailmap::load(&git_dir, format, &source_specs)?;
    for contact in contacts {
        println!("{}", mailmap.resolve_contact(&contact).display());
    }
    Ok(())
}

fn check_mailmap_usage<T>() -> Result<T> {
    eprintln!("usage: git check-mailmap [<options>] <contact>...");
    eprintln!();
    eprintln!("    --[no-]stdin          also read contacts from stdin");
    eprintln!("    --[no-]mailmap-file <file>");
    eprintln!("                          read additional mailmap entries from file");
    eprintln!("    --[no-]mailmap-blob <blob>");
    eprintln!("                          read additional mailmap entries from blob");
    eprintln!();
    Err(GitError::Exit(129))
}

#[derive(Debug)]
enum MailmapSourceSpec {
    File(PathBuf),
    Blob(String),
}

/// The shared mailmap canonicalization engine — git's `mailmap.c` model.
///
/// A mailmap file maps a *commit* identity (the name/email recorded in the
/// object header) to a *canonical* one. git keys the whole structure by the
/// commit email (case-insensitively): each `MailmapEntry` carries an optional
/// top-level name/email replacement (the unqualified `<commit@email>` form) and
/// a `namemap` of commit-name → replacement for the name-qualified forms. This
/// is the ONE engine every consumer (log/blame/shortlog/for-each-ref/tag/
/// branch/cat-file/check-mailmap) routes through — see [`Mailmap::map_user`].
#[derive(Debug, Default)]
pub(crate) struct Mailmap {
    /// Keyed by the lowercased commit email (git's `string_list` keyed by
    /// `old_email`, compared case-insensitively via `lookup_prefix`).
    entries: HashMap<String, MailmapEntry>,
}

#[derive(Debug, Default)]
struct MailmapEntry {
    /// Top-level replacement for the unqualified `<commit@email>` form
    /// (git's `me->name` / `me->email`).
    name: Option<String>,
    email: Option<String>,
    /// Name-qualified replacements: `(lowercased old_name, replacement)`, in
    /// insertion order. git stores these in a `string_list` keyed by
    /// `old_name`, compared case-insensitively.
    namemap: Vec<(String, MailmapInfo)>,
}

#[derive(Debug, Clone, Default)]
struct MailmapInfo {
    name: Option<String>,
    email: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MailmapContact {
    name: Option<String>,
    email: String,
}

impl Mailmap {
    /// Load only the repository `.mailmap` plus any `mailmap.file`/`mailmap.blob`
    /// config sources — the set git consults when resolving `%(...:mailmap)`
    /// atoms in for-each-ref / log / etc.
    pub(crate) fn load_default(git_dir: &Path, format: ObjectFormat) -> Result<Self> {
        Self::load(git_dir, format, &[])
    }

    /// Load the mailmap when there may be no repository (git's `read_mailmap`
    /// non-repo branch: `.mailmap` from the *current directory* — symlinks
    /// followed — plus the `mailmap.file` config path). Used by the stdin path
    /// of `shortlog`, which git lets run outside a repo. Falls back to
    /// [`Self::load_default`] when a repo is present so blob sources still work.
    pub(crate) fn load_cwd() -> Result<Self> {
        let mut mailmap = Self::default();
        mailmap.add_file(Path::new(".mailmap"))?;
        if let Some(config) = identity_effective_config()
            && let Some(path) = config.get("mailmap", None, "file")
        {
            mailmap.add_file(Path::new(&path))?;
        }
        Ok(mailmap)
    }

    /// True when no mapping entries were loaded — lets consumers short-circuit
    /// the lookup entirely (git gates on `mail_map->nr`).
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// git's `map_user`: resolve a commit (name, email) pair through the mailmap.
    /// Returns the canonical `(name, email)`, leaving either component unchanged
    /// when the mailmap has no replacement for it. This is the single
    /// canonicalization primitive every consumer calls.
    pub(crate) fn map_user(&self, name: &str, email: &str) -> (String, String) {
        let mut out_name = name.to_string();
        let mut out_email = email.to_string();
        if let Some(entry) = self.entries.get(&email.to_ascii_lowercase()) {
            // Prefer a name-qualified replacement; fall back to the top-level
            // (unqualified) one. Matches git's namemap-then-simple precedence.
            let info = entry
                .lookup_name(name)
                .map(|info| (info.name.as_deref(), info.email.as_deref()))
                .unwrap_or((entry.name.as_deref(), entry.email.as_deref()));
            if let Some(new_email) = info.1 {
                out_email = new_email.to_string();
            }
            if let Some(new_name) = info.0 {
                out_name = new_name.to_string();
            }
        }
        (out_name, out_email)
    }

    /// Resolve a raw identity (`Name <email> <timestamp> <tz>` bytes, as found in
    /// commit/tag headers) through the mailmap, returning rewritten name and email
    /// bytes. The trailing date is preserved untouched.
    pub(crate) fn rewrite_identity(&self, identity: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let name = for_each_ref_identity_name(identity).unwrap_or(b"");
        // The bracketed email, including angle brackets; strip them for lookup.
        let email = for_each_ref_identity_email(identity, ForEachRefEmailMode::Trim).unwrap_or(b"");
        let (new_name, new_email) = self.map_user(
            &String::from_utf8_lossy(name),
            &String::from_utf8_lossy(email),
        );
        (new_name.into_bytes(), new_email.into_bytes())
    }

    fn load(
        git_dir: &Path,
        format: ObjectFormat,
        source_specs: &[MailmapSourceSpec],
    ) -> Result<Self> {
        let mut mailmap = Self::default();
        let worktree_root = worktree_root_for_git_dir(git_dir).ok();
        if let Some(root) = &worktree_root {
            mailmap.add_file(&root.join(".mailmap"))?;
        }
        if let Some(config) = identity_effective_config() {
            if let Some(path) = config.get("mailmap", None, "file") {
                let path = mailmap_config_path(worktree_root.as_deref(), path);
                mailmap.add_file(&path)?;
            }
            if let Some(blob) = config.get("mailmap", None, "blob") {
                mailmap.add_blob(git_dir, format, blob)?;
            }
        }
        for source in source_specs {
            match source {
                MailmapSourceSpec::File(path) => mailmap.add_file(path)?,
                MailmapSourceSpec::Blob(rev) => mailmap.add_blob(git_dir, format, rev)?,
            }
        }
        Ok(mailmap)
    }

    fn add_file(&mut self, path: &Path) -> Result<()> {
        match fs::read(path) {
            Ok(bytes) => self.add_bytes(&bytes),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(GitError::Io(err.to_string())),
        }
    }

    fn add_blob(&mut self, git_dir: &Path, format: ObjectFormat, rev: &str) -> Result<()> {
        let oid = resolve_revision(git_dir, format, rev)?;
        let db = FileObjectDatabase::from_git_dir(git_dir, format);
        let object = db.read_object(&oid)?;
        if object.object_type != ObjectType::Blob {
            eprintln!("error: unable to read mailmap object at {rev}");
            return Err(GitError::Exit(128));
        }
        self.add_bytes(&object.body)
    }

    fn add_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        let text = String::from_utf8_lossy(bytes);
        for line in text.lines() {
            self.read_mailmap_line(line);
        }
        Ok(())
    }

    /// git's `read_mailmap_line` + `add_mapping`. A line is
    /// `<proper>[ <commit>]`; the first `Name <email>` is the canonical
    /// identity, the (optional) second is the commit identity to match. When the
    /// commit form is absent the line keys on the proper email and only the name
    /// is replaced.
    fn read_mailmap_line(&mut self, line: &str) {
        // git only special-cases a `#` in the *first* column as a whole-line
        // comment; a `#` elsewhere is literal (unlike a stripspace comment).
        if line.starts_with('#') {
            return;
        }
        let mut rest = line;
        // First token is the *proper* (canonical) name+email. `email1` must be
        // present (git's `if (email1)` guard) or the line is ignored.
        let Some((new_name, new_email)) = parse_name_and_email(&mut rest, false) else {
            return;
        };
        let Some(new_email) = new_email else {
            return;
        };
        // Second token (commit-side name+email) is optional.
        let (old_name, old_email) = parse_name_and_email(&mut rest, true).unwrap_or((None, None));
        // add_mapping's reshuffle: with no commit-side email, the proper email
        // is itself the lookup key and only the name is replaced.
        let (new_email, old_email) = match old_email {
            Some(old_email) => (Some(new_email), old_email),
            None => (None, new_email),
        };
        self.add_mapping(new_name, new_email, old_name, old_email);
    }

    fn add_mapping(
        &mut self,
        new_name: Option<String>,
        new_email: Option<String>,
        old_name: Option<String>,
        old_email: String,
    ) {
        let entry = self
            .entries
            .entry(old_email.to_ascii_lowercase())
            .or_default();
        match old_name {
            None => {
                // Replace the simple (unqualified) name/email for this email.
                if new_name.is_some() {
                    entry.name = new_name;
                }
                if new_email.is_some() {
                    entry.email = new_email;
                }
            }
            Some(old_name) => {
                let info = MailmapInfo {
                    name: new_name,
                    email: new_email,
                };
                let key = old_name.to_ascii_lowercase();
                if let Some(slot) = entry.namemap.iter_mut().find(|(k, _)| *k == key) {
                    slot.1 = info;
                } else {
                    entry.namemap.push((key, info));
                }
            }
        }
    }

    fn resolve_contact(&self, contact: &str) -> MailmapContact {
        let parsed = parse_mailmap_contact(contact);
        let name = parsed.name.as_deref().unwrap_or("");
        let (new_name, new_email) = self.map_user(name, &parsed.email);
        MailmapContact {
            name: (!new_name.is_empty()).then_some(new_name),
            email: new_email,
        }
    }
}

impl MailmapEntry {
    fn lookup_name(&self, name: &str) -> Option<&MailmapInfo> {
        if self.namemap.is_empty() {
            return None;
        }
        let key = name.to_ascii_lowercase();
        self.namemap
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, info)| info)
    }
}

impl MailmapContact {
    fn display(&self) -> String {
        match &self.name {
            Some(name) if !name.is_empty() => format!("{name} <{}>", self.email),
            _ => format!("<{}>", self.email),
        }
    }
}

fn mailmap_config_path(worktree_root: Option<&Path>, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else if let Some(root) = worktree_root {
        root.join(path)
    } else {
        path
    }
}

/// git's `parse_name_and_email`: pull the leading `Name <email>` token off the
/// front of `buffer`, advancing `buffer` past the closing `>`. Returns the
/// `(name, email)` pair (each trimmed; `None` when empty) when a token is found.
/// With `allow_empty_email == false` an empty `<>` is rejected (returns `None`).
/// When no `<...>` is present at all the function returns `None` and leaves
/// `buffer` pointing at the empty tail.
fn parse_name_and_email(
    buffer: &mut &str,
    allow_empty_email: bool,
) -> Option<(Option<String>, Option<String>)> {
    let left = buffer.find('<')?;
    let right_rel = buffer[left + 1..].find('>')?;
    let right = left + 1 + right_rel;
    if !allow_empty_email && right == left + 1 {
        return None;
    }
    let name = buffer[..left].trim();
    let email = &buffer[left + 1..right];
    let name = (!name.is_empty()).then(|| name.to_string());
    let email = (!email.is_empty()).then(|| email.to_string());
    *buffer = &buffer[right + 1..];
    Some((name, email))
}

fn parse_mailmap_contact(value: &str) -> MailmapContact {
    let value = value.trim();
    if let Some(start) = value.rfind('<')
        && let Some(end) = value[start + 1..].find('>')
    {
        let email = value[start + 1..start + 1 + end].trim().to_string();
        let name = value[..start].trim();
        return MailmapContact {
            name: (!name.is_empty()).then(|| name.to_string()),
            email,
        };
    }
    MailmapContact {
        name: None,
        email: value.to_string(),
    }
}

pub(crate) fn cmd_stripspace(args: &[String]) -> Result<()> {
    let mut strip_comments = false;
    let mut comment_lines = false;
    for arg in args {
        match arg.as_str() {
            "-s" | "--strip-comments" => strip_comments = true,
            "--no-strip-comments" => strip_comments = false,
            "-c" | "--comment-lines" => comment_lines = true,
            "--no-comment-lines" => comment_lines = false,
            value if value.starts_with('-') => {
                eprintln!("error: unknown option `{}'", value.trim_start_matches('-'));
                return stripspace_usage();
            }
            _ => return stripspace_usage(),
        }
    }
    if strip_comments && comment_lines {
        eprintln!(
            "error: options '--comment-lines' and '--strip-comments' cannot be used together"
        );
        return Err(GitError::Exit(129));
    }
    let mut input = Vec::new();
    io::stdin().read_to_end(&mut input)?;
    let output = if comment_lines {
        stripspace_comment_lines(&input)
    } else {
        tag_stripspace_message(&input, strip_comments)
    };
    io::stdout().write_all(&output)?;
    Ok(())
}

fn stripspace_comment_lines(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for line in input.split_inclusive(|byte| *byte == b'\n') {
        if matches!(line, b"\n" | b"\r\n") {
            out.extend_from_slice(b"#");
        } else {
            out.extend_from_slice(b"# ");
        }
        out.extend_from_slice(line);
    }
    if !input.ends_with(b"\n") && !input.is_empty() {
        out.push(b'\n');
    }
    out
}

fn stripspace_usage<T>() -> Result<T> {
    eprintln!("usage: git stripspace [-s | --strip-comments]");
    eprintln!("   or: git stripspace [-c | --comment-lines]");
    eprintln!();
    eprintln!(
        "    -s, --strip-comments  skip and remove all lines starting with comment character"
    );
    eprintln!("    -c, --comment-lines   prepend comment character and space to each line");
    eprintln!();
    Err(GitError::Exit(129))
}

pub(crate) fn cmd_check_ref_format(args: &[String]) -> Result<()> {
    let mut allow_onelevel = false;
    let mut branch = false;
    let mut normalize = false;
    let mut refspec_pattern = false;
    let mut positional = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--allow-onelevel" => allow_onelevel = true,
            "--no-allow-onelevel" => allow_onelevel = false,
            "--branch" => branch = true,
            "--normalize" | "--print" => normalize = true,
            "--no-normalize" | "--no-print" => normalize = false,
            "--refspec-pattern" => refspec_pattern = true,
            "--no-refspec-pattern" => refspec_pattern = false,
            value if value.starts_with('-') && !branch => return check_ref_format_usage(),
            value => positional.push(value),
        }
    }
    if positional.len() != 1 {
        return check_ref_format_usage();
    }
    let mut name = positional[0].to_string();
    if normalize {
        name = normalize_check_ref_format_name(&name);
    }
    if branch {
        if check_branch_format_name(&name).is_ok() {
            println!("{name}");
            return Ok(());
        }
        eprintln!("fatal: '{name}' is not a valid branch name");
        return Err(GitError::Exit(128));
    }
    if check_ref_format_name(&name, allow_onelevel, refspec_pattern).is_ok() {
        if normalize {
            println!("{name}");
        }
        Ok(())
    } else {
        Err(GitError::Exit(1))
    }
}

fn check_ref_format_usage<T>() -> Result<T> {
    eprintln!("usage: git check-ref-format [--normalize] [<options>] <refname>");
    eprintln!("   or: git check-ref-format --branch <branchname-shorthand>");
    Err(GitError::Exit(129))
}

fn normalize_check_ref_format_name(name: &str) -> String {
    name.split('/')
        .filter(|component| !component.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

fn check_branch_format_name(name: &str) -> Result<()> {
    if name.starts_with('-') {
        return Err(GitError::InvalidPath(format!("invalid branch name {name}")));
    }
    check_ref_format_name(name, true, false)
}

fn check_ref_format_name(name: &str, allow_onelevel: bool, refspec_pattern: bool) -> Result<()> {
    if name.is_empty()
        || name == "@"
        || name.starts_with('/')
        || name.ends_with('/')
        || name.ends_with('.')
        || name.contains("..")
        || name.contains("//")
        || name.contains("@{")
        || (!allow_onelevel && !name.contains('/'))
    {
        return Err(GitError::InvalidPath(format!("invalid ref name {name}")));
    }
    let mut stars = 0usize;
    for component in name.split('/') {
        if component.is_empty() || component.starts_with('.') || component.ends_with(".lock") {
            return Err(GitError::InvalidPath(format!("invalid ref name {name}")));
        }
        for byte in component.bytes() {
            if byte == b'*' {
                stars += 1;
                if !refspec_pattern || stars > 1 {
                    return Err(GitError::InvalidPath(format!("invalid ref name {name}")));
                }
                continue;
            }
            if byte <= b' '
                || byte == 0x7f
                || matches!(byte, b'~' | b'^' | b':' | b'?' | b'[' | b'\\')
            {
                return Err(GitError::InvalidPath(format!("invalid ref name {name}")));
            }
        }
    }
    Ok(())
}

pub(crate) fn cmd_testkit(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("hash-object") => {
            for result in
                sley_testkit::hash_object_parity(&sley_testkit::default_hash_object_cases())?
            {
                println!("{} {}", result.case_name, result.rust);
            }
            Ok(())
        }
        Some("hash-object-sha256") => {
            for result in sley_testkit::hash_object_parity_for_format(
                ObjectFormat::Sha256,
                &sley_testkit::default_hash_object_cases(),
            )? {
                println!("{} {}", result.case_name, result.rust);
            }
            Ok(())
        }
        Some("pack-read") => {
            let result = sley_testkit::single_blob_pack_read_parity()?;
            println!(
                "pack-read {} {} {}",
                result.format.name(),
                result.object_type,
                result.oid
            );
            Ok(())
        }
        Some("pack-read-sha256") => {
            let result = sley_testkit::single_blob_pack_read_parity_sha256()?;
            println!(
                "pack-read {} {} {}",
                result.format.name(),
                result.object_type,
                result.oid
            );
            Ok(())
        }
        Some("packed-odb") => {
            let result = sley_testkit::packed_odb_read_interop_parity()?;
            println!("packed-odb {} {}", result.format.name(), result.oid);
            Ok(())
        }
        Some("packed-odb-sha256") => {
            let result = sley_testkit::packed_odb_read_interop_parity_sha256()?;
            println!("packed-odb {} {}", result.format.name(), result.oid);
            Ok(())
        }
        Some("pack-delta") => {
            let result = sley_testkit::delta_pack_read_parity()?;
            println!(
                "pack-delta {} entries={} deltas={} {} {}",
                result.format.name(),
                result.entries,
                result.delta_entries,
                result.base_oid,
                result.changed_oid
            );
            Ok(())
        }
        Some("pack-delta-sha256") => {
            let result = sley_testkit::delta_pack_read_parity_sha256()?;
            println!(
                "pack-delta {} entries={} deltas={} {} {}",
                result.format.name(),
                result.entries,
                result.delta_entries,
                result.base_oid,
                result.changed_oid
            );
            Ok(())
        }
        Some("packed-odb-delta") => {
            let result = sley_testkit::delta_packed_odb_read_interop_parity()?;
            println!(
                "packed-odb-delta {} entries={} deltas={} {} {}",
                result.format.name(),
                result.entries,
                result.delta_entries,
                result.base_oid,
                result.changed_oid
            );
            Ok(())
        }
        Some("packed-odb-delta-sha256") => {
            let result = sley_testkit::delta_packed_odb_read_interop_parity_sha256()?;
            println!(
                "packed-odb-delta {} entries={} deltas={} {} {}",
                result.format.name(),
                result.entries,
                result.delta_entries,
                result.base_oid,
                result.changed_oid
            );
            Ok(())
        }
        Some("pack-thin") => {
            let result = sley_testkit::thin_pack_read_parity()?;
            println!(
                "pack-thin {} entries={} {} {}",
                result.format.name(),
                result.entries,
                result.base_oid,
                result.changed_oid
            );
            Ok(())
        }
        Some("pack-thin-sha256") => {
            let result = sley_testkit::thin_pack_read_parity_sha256()?;
            println!(
                "pack-thin {} entries={} {} {}",
                result.format.name(),
                result.entries,
                result.base_oid,
                result.changed_oid
            );
            Ok(())
        }
        Some("pack-write-delta") => {
            let result = sley_testkit::rust_delta_pack_write_interop_parity()?;
            println!(
                "pack-write-delta {} deltas={} {} {} {}",
                result.format.name(),
                result.delta_entries,
                result.pack_name,
                result.base_oid,
                result.changed_oid
            );
            Ok(())
        }
        Some("pack-write-delta-sha256") => {
            let result = sley_testkit::rust_delta_pack_write_interop_parity_sha256()?;
            println!(
                "pack-write-delta {} deltas={} {} {} {}",
                result.format.name(),
                result.delta_entries,
                result.pack_name,
                result.base_oid,
                result.changed_oid
            );
            Ok(())
        }
        Some("config") => {
            let result = sley_testkit::repository_config_interop_parity()?;
            println!(
                "config object-format={} bare={}",
                result.object_format.name(),
                result
                    .bare
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "unset".into())
            );
            Ok(())
        }
        Some("ls-tree") => {
            let result = sley_testkit::ls_tree_parity()?;
            println!("ls-tree {}", result.tree_oid);
            Ok(())
        }
        Some("ls-tree-sha256") => {
            let result = sley_testkit::ls_tree_parity_sha256()?;
            println!("ls-tree {}", result.tree_oid);
            Ok(())
        }
        Some("cat-file") => {
            let result = sley_testkit::cat_file_revision_parity()?;
            println!("cat-file {}", result.revs.join(" "));
            Ok(())
        }
        Some("cat-file-sha256") => {
            let result = sley_testkit::cat_file_revision_parity_sha256()?;
            println!("cat-file {}", result.revs.join(" "));
            Ok(())
        }
        Some("commit-tree") => {
            let result = sley_testkit::commit_tree_parity()?;
            println!("commit-tree {}", result.rust);
            Ok(())
        }
        Some("commit-tree-sha256") => {
            let result = sley_testkit::commit_tree_parity_sha256()?;
            println!("commit-tree {}", result.rust);
            Ok(())
        }
        Some("commit") => {
            let result = sley_testkit::commit_index_parity()?;
            println!("commit {}", result.head);
            Ok(())
        }
        Some("commit-sha256") => {
            let result = sley_testkit::commit_index_parity_sha256()?;
            println!("commit {}", result.head);
            Ok(())
        }
        Some("branch") => {
            let result = sley_testkit::branch_create_parity()?;
            print!("{}", result.upstream);
            Ok(())
        }
        Some("branch-sha256") => {
            let result = sley_testkit::branch_create_parity_sha256()?;
            print!("{}", result.upstream);
            Ok(())
        }
        Some("branch-current") => {
            let result = sley_testkit::branch_show_current_parity()?;
            print!("{}", result.rust);
            Ok(())
        }
        Some("branch-current-sha256") => {
            let result = sley_testkit::branch_show_current_parity_sha256()?;
            print!("{}", result.rust);
            Ok(())
        }
        Some("branch-delete") => {
            let result = sley_testkit::branch_delete_parity()?;
            println!("branch-delete {}", result.deleted_oid);
            Ok(())
        }
        Some("branch-delete-sha256") => {
            let result = sley_testkit::branch_delete_parity_sha256()?;
            println!("branch-delete {}", result.deleted_oid);
            Ok(())
        }
        Some("checkout") => {
            let result = sley_testkit::checkout_branch_parity()?;
            println!("checkout {} {}", result.branch, result.head);
            Ok(())
        }
        Some("checkout-sha256") => {
            let result = sley_testkit::checkout_branch_parity_sha256()?;
            println!("checkout {} {}", result.branch, result.head);
            Ok(())
        }
        Some("tag") => {
            let result = sley_testkit::tag_create_parity()?;
            print!("{}", result.upstream);
            Ok(())
        }
        Some("tag-sha256") => {
            let result = sley_testkit::tag_create_parity_sha256()?;
            print!("{}", result.upstream);
            Ok(())
        }
        Some("tag-delete") => {
            let result = sley_testkit::tag_delete_parity()?;
            println!("tag-delete {}", result.deleted_oid);
            Ok(())
        }
        Some("tag-delete-sha256") => {
            let result = sley_testkit::tag_delete_parity_sha256()?;
            println!("tag-delete {}", result.deleted_oid);
            Ok(())
        }
        Some("annotated-tag") => {
            let result = sley_testkit::annotated_tag_create_parity()?;
            println!("annotated-tag {} {}", result.tag_oid, result.target_oid);
            Ok(())
        }
        Some("annotated-tag-sha256") => {
            let result = sley_testkit::annotated_tag_create_parity_sha256()?;
            println!("annotated-tag {} {}", result.tag_oid, result.target_oid);
            Ok(())
        }
        Some("diff") => {
            let result = sley_testkit::diff_name_status_parity()?;
            print!("{}", result.rust);
            Ok(())
        }
        Some("diff-sha256") => {
            let result = sley_testkit::diff_name_status_parity_sha256()?;
            print!("{}", result.rust);
            Ok(())
        }
        Some("rev-parse") => {
            let result = sley_testkit::rev_parse_parity()?;
            print!("{}", result.rust);
            Ok(())
        }
        Some("rev-parse-sha256") => {
            let result = sley_testkit::rev_parse_parity_sha256()?;
            print!("{}", result.rust);
            Ok(())
        }
        Some("rev-parse-parents") => {
            let result = sley_testkit::rev_parse_parent_parity()?;
            print!("{}", result.rust);
            Ok(())
        }
        Some("rev-parse-parents-sha256") => {
            let result = sley_testkit::rev_parse_parent_parity_sha256()?;
            print!("{}", result.rust);
            Ok(())
        }
        Some("rev-parse-peel") => {
            let result = sley_testkit::rev_parse_peel_parity()?;
            print!("{}", result.rust);
            Ok(())
        }
        Some("rev-parse-peel-sha256") => {
            let result = sley_testkit::rev_parse_peel_parity_sha256()?;
            print!("{}", result.rust);
            Ok(())
        }
        Some("rev-parse-object-format") => {
            let result = sley_testkit::rev_parse_object_format_parity()?;
            print!("{}", result.sha1_rust);
            print!("{}", result.sha256_rust);
            Ok(())
        }
        Some("add-status") => {
            let result = sley_testkit::add_status_parity()?;
            print!("{}", result.rust);
            Ok(())
        }
        Some("add-status-sha256") => {
            let result = sley_testkit::add_status_parity_sha256()?;
            print!("{}", result.rust);
            Ok(())
        }
        Some("index") => {
            let result = sley_testkit::index_round_trip_parity()?;
            println!(
                "index format={} entries={} bytes={}",
                result.format.name(),
                result.entries,
                result.byte_len
            );
            Ok(())
        }
        Some("index-sha256") => {
            let result = sley_testkit::index_round_trip_parity_sha256()?;
            println!(
                "index format={} entries={} bytes={}",
                result.format.name(),
                result.entries,
                result.byte_len
            );
            Ok(())
        }
        Some("update-index") => {
            let result = sley_testkit::update_index_add_parity()?;
            println!(
                "update-index format={} {}",
                result.format.name(),
                result.expected.trim_end()
            );
            Ok(())
        }
        Some("update-index-sha256") => {
            let result = sley_testkit::update_index_add_parity_sha256()?;
            println!(
                "update-index format={} {}",
                result.format.name(),
                result.expected.trim_end()
            );
            Ok(())
        }
        Some("ls-files") => {
            let result = sley_testkit::ls_files_stage_parity()?;
            print!("{}", result.rust);
            Ok(())
        }
        Some("ls-files-sha256") => {
            let result = sley_testkit::ls_files_stage_parity_sha256()?;
            print!("{}", result.rust);
            Ok(())
        }
        Some("update-ref-delete") => {
            let result = sley_testkit::update_ref_delete_parity()?;
            println!("update-ref-delete {}", result.deleted_oid);
            Ok(())
        }
        Some("update-ref-delete-sha256") => {
            let result = sley_testkit::update_ref_delete_parity_sha256()?;
            println!("update-ref-delete {}", result.deleted_oid);
            Ok(())
        }
        Some("update-ref-delete-packed") => {
            let result = sley_testkit::update_ref_delete_packed_parity()?;
            println!("update-ref-delete-packed {}", result.deleted_oid);
            Ok(())
        }
        Some("update-ref-delete-packed-sha256") => {
            let result = sley_testkit::update_ref_delete_packed_parity_sha256()?;
            println!("update-ref-delete-packed {}", result.deleted_oid);
            Ok(())
        }
        Some("reflog-expire") => {
            let result = sley_testkit::reflog_expire_parity()?;
            println!(
                "reflog-expire removed={} {}",
                result.removed,
                result.after.trim_end()
            );
            Ok(())
        }
        Some("reflog-expire-sha256") => {
            let result = sley_testkit::reflog_expire_parity_sha256()?;
            println!(
                "reflog-expire removed={} {}",
                result.removed,
                result.after.trim_end()
            );
            Ok(())
        }
        Some("write-tree") => {
            let result = sley_testkit::write_tree_parity()?;
            println!("write-tree {}", result.rust);
            Ok(())
        }
        Some("write-tree-sha256") => {
            let result = sley_testkit::write_tree_parity_sha256()?;
            println!("write-tree {}", result.rust);
            Ok(())
        }
        Some("log") => {
            let result = sley_testkit::log_parity()?;
            println!("log {}", result.commit_oid);
            Ok(())
        }
        Some("log-sha256") => {
            let result = sley_testkit::log_parity_sha256()?;
            println!("log {}", result.commit_oid);
            Ok(())
        }
        Some("pack-index") => {
            let result = sley_testkit::single_blob_pack_index_parity()?;
            println!(
                "pack-index format={} entries={} offset={} {}",
                result.format.name(),
                result.entries,
                result.offset,
                result.oid
            );
            Ok(())
        }
        Some("pack-index-sha256") => {
            let result = sley_testkit::single_blob_pack_index_parity_sha256()?;
            println!(
                "pack-index format={} entries={} offset={} {}",
                result.format.name(),
                result.entries,
                result.offset,
                result.oid
            );
            Ok(())
        }
        Some("pack-write") => {
            let result = sley_testkit::rust_pack_write_interop_parity()?;
            println!(
                "pack-write {} {} {}",
                result.format.name(),
                result.pack_name,
                result.oid
            );
            Ok(())
        }
        Some("pack-write-sha256") => {
            let result = sley_testkit::rust_pack_write_interop_parity_sha256()?;
            println!(
                "pack-write {} {} {}",
                result.format.name(),
                result.pack_name,
                result.oid
            );
            Ok(())
        }
        Some("loose-sha256") => {
            let result = sley_testkit::sha256_loose_object_interop_parity()?;
            println!("loose-sha256 {} {}", result.upstream_type, result.oid);
            Ok(())
        }
        Some("refs") => {
            let result = sley_testkit::loose_ref_interop_parity()?;
            println!("refs {} {}", result.name, result.oid);
            Ok(())
        }
        Some("refs-sha256") => {
            let result = sley_testkit::loose_ref_interop_parity_sha256()?;
            println!("refs {} {}", result.name, result.oid);
            Ok(())
        }
        Some("refs-packed") => {
            let result = sley_testkit::packed_ref_interop_parity()?;
            println!("refs-packed {} {}", result.name, result.oid);
            Ok(())
        }
        Some("refs-packed-sha256") => {
            let result = sley_testkit::packed_ref_interop_parity_sha256()?;
            println!("refs-packed {} {}", result.name, result.oid);
            Ok(())
        }
        Some("refs-pack") => {
            let result = sley_testkit::packed_ref_compaction_interop_parity()?;
            println!("refs-pack {} {}", result.name, result.oid);
            Ok(())
        }
        Some("refs-pack-sha256") => {
            let result = sley_testkit::packed_ref_compaction_interop_parity_sha256()?;
            println!("refs-pack {} {}", result.name, result.oid);
            Ok(())
        }
        Some("refs-pack-peeled") => {
            let result = sley_testkit::peeled_packed_ref_compaction_interop_parity()?;
            println!(
                "refs-pack-peeled {} {} {}",
                result.name, result.tag_oid, result.peeled_oid
            );
            Ok(())
        }
        Some("refs-pack-peeled-sha256") => {
            let result = sley_testkit::peeled_packed_ref_compaction_interop_parity_sha256()?;
            println!(
                "refs-pack-peeled {} {} {}",
                result.name, result.tag_oid, result.peeled_oid
            );
            Ok(())
        }
        Some("show-ref") => {
            let result = sley_testkit::show_ref_filter_parity()?;
            print!("{}", result.heads_rust);
            print!("{}", result.tags_rust);
            Ok(())
        }
        Some("show-ref-sha256") => {
            let result = sley_testkit::show_ref_filter_parity_sha256()?;
            print!("{}", result.heads_rust);
            print!("{}", result.tags_rust);
            Ok(())
        }
        Some("show-ref-verify") => {
            let result = sley_testkit::show_ref_verify_parity()?;
            print!("{}", result.rust);
            Ok(())
        }
        Some("show-ref-verify-sha256") => {
            let result = sley_testkit::show_ref_verify_parity_sha256()?;
            print!("{}", result.rust);
            Ok(())
        }
        Some("symbolic-ref") => {
            let result = sley_testkit::symbolic_ref_parity()?;
            print!("{}", result.head_rust);
            print!("{}", result.short_rust);
            print!("{}", result.switched_rust);
            Ok(())
        }
        Some("symbolic-ref-sha256") => {
            let result = sley_testkit::symbolic_ref_parity_sha256()?;
            print!("{}", result.head_rust);
            print!("{}", result.short_rust);
            print!("{}", result.switched_rust);
            Ok(())
        }
        _ => Err(GitError::Command(
            "testkit currently supports: hash-object, hash-object-sha256, loose-sha256, config, index, index-sha256, update-index, update-index-sha256, ls-files, ls-files-sha256, update-ref-delete, update-ref-delete-sha256, update-ref-delete-packed, update-ref-delete-packed-sha256, reflog-expire, reflog-expire-sha256, write-tree, write-tree-sha256, commit-tree, commit-tree-sha256, commit, commit-sha256, branch, branch-sha256, branch-current, branch-current-sha256, branch-delete, branch-delete-sha256, checkout, checkout-sha256, tag, tag-sha256, tag-delete, tag-delete-sha256, annotated-tag, annotated-tag-sha256, diff, diff-sha256, rev-parse, rev-parse-sha256, rev-parse-parents, rev-parse-parents-sha256, rev-parse-peel, rev-parse-peel-sha256, rev-parse-object-format, add-status, add-status-sha256, ls-tree, ls-tree-sha256, cat-file, cat-file-sha256, log, log-sha256, pack-read, pack-read-sha256, packed-odb, packed-odb-sha256, pack-delta, pack-delta-sha256, packed-odb-delta, packed-odb-delta-sha256, pack-thin, pack-thin-sha256, pack-index, pack-index-sha256, pack-write, pack-write-sha256, pack-write-delta, pack-write-delta-sha256, refs, refs-sha256, refs-packed, refs-packed-sha256, refs-pack, refs-pack-sha256, refs-pack-peeled, refs-pack-peeled-sha256, show-ref, show-ref-sha256, show-ref-verify, show-ref-verify-sha256, symbolic-ref, symbolic-ref-sha256"
                .into(),
        )),
    }
}
