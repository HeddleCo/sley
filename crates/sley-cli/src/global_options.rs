use sley::{GitError, Result};
use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Accumulated sq-quoted fragment of command-line `-c` / `--config-env`
/// parameters, in left-to-right order. Stands in for git's mutation of the
/// process `GIT_CONFIG_PARAMETERS` env var (forbidden here, as the workspace bans
/// `unsafe`/`set_var`); appended after any inherited `GIT_CONFIG_PARAMETERS` to
/// form the effective parameter list.
static CMDLINE_CONFIG_PARAMETERS: Mutex<String> = Mutex::new(String::new());

/// Default pathspec magic set by the global `--{glob,noglob,icase,literal}-pathspecs`
/// options (and the corresponding `GIT_*_PATHSPECS` env vars). Mirrors git's
/// `get_default_pathspec_flags()`: `--literal-pathspecs` wins and forces every
/// pathspec to be matched literally; otherwise glob/icase magic is OR'd in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PathspecFlags {
    /// `--literal-pathspecs`: no wildcard interpretation at all.
    pub literal: bool,
    /// `--glob-pathspecs`: `*`/`?` are pathname-aware (`WM_PATHNAME`), `**` spans `/`.
    pub glob: bool,
    /// `--icase-pathspecs`: case-insensitive matching (`WM_CASEFOLD`).
    pub icase: bool,
    /// `--literal-pathspecs`: even `:(...)` magic syntax is treated as bytes.
    pub literal_pathspecs: bool,
}

pub(crate) struct GlobalOptions<'a> {
    pub args: &'a [String],
    pub config: Vec<GlobalConfigOverride>,
    pub git_dir: Option<PathBuf>,
    pub work_tree: Option<PathBuf>,
    pub attr_source: Option<String>,
    pub bare: bool,
    pub replace_objects: bool,
    pub lazy_fetch: bool,
    pub pathspec_flags: PathspecFlags,
}

#[derive(Debug, Clone)]
pub(crate) struct GlobalConfigOverride {
    pub key: String,
    pub value: String,
}

pub(crate) fn apply_global_options(args: &[String]) -> Result<GlobalOptions<'_>> {
    let mut index = 0;
    let mut config = Vec::new();
    let mut git_dir = None;
    let mut work_tree = None;
    let mut attr_source = None;
    let mut bare = false;
    let mut replace_objects = env::var_os("GIT_NO_REPLACE_OBJECTS").is_none();
    let mut lazy_fetch = true;
    let mut pathspec_flags = PathspecFlags::default();
    while let Some(arg) = args.get(index) {
        match arg.as_str() {
            "-C" => {
                let Some(path) = args.get(index + 1) else {
                    eprintln!("no directory given for '-C' option");
                    print_global_usage();
                    return Err(GitError::Exit(129));
                };
                if !path.is_empty()
                    && let Err(err) = env::set_current_dir(path)
                {
                    eprintln!("fatal: cannot change to '{}': {err}", path);
                    return Err(GitError::Exit(128));
                }
                index += 2;
            }
            "-c" => {
                let Some(assignment) = args.get(index + 1) else {
                    eprintln!("-c expects a configuration string");
                    print_global_usage();
                    return Err(GitError::Exit(129));
                };
                if let Some(entry) = push_config_parameter(assignment) {
                    config.push(entry);
                }
                index += 2;
            }
            "--config-env" => {
                let Some(spec) = args.get(index + 1) else {
                    eprintln!("no config key given for --config-env");
                    print_global_usage();
                    return Err(GitError::Exit(129));
                };
                config.push(push_config_env(spec)?);
                index += 2;
            }
            "-p" | "--paginate" | "-P" | "--no-pager" | "--no-optional-locks" | "--no-advice" => {
                index += 1;
            }
            "--no-lazy-fetch" => {
                lazy_fetch = false;
                index += 1;
            }
            "--no-replace-objects" => {
                replace_objects = false;
                index += 1;
            }
            "--literal-pathspecs" => {
                pathspec_flags.literal = true;
                pathspec_flags.literal_pathspecs = true;
                index += 1;
            }
            "--glob-pathspecs" => {
                pathspec_flags.glob = true;
                index += 1;
            }
            "--noglob-pathspecs" => {
                // git treats --noglob-pathspecs as forcing literal `*`/`?`/`[`
                // (PATHSPEC_LITERAL is not set, but glob magic is suppressed and
                // wildcards lose their special meaning). Model it as literal for
                // matching purposes.
                pathspec_flags.literal = true;
                index += 1;
            }
            "--icase-pathspecs" => {
                pathspec_flags.icase = true;
                index += 1;
            }
            "--git-dir" => {
                let Some(path) = args.get(index + 1) else {
                    eprintln!("no directory given for '--git-dir' option");
                    print_global_usage();
                    return Err(GitError::Exit(129));
                };
                git_dir = Some(PathBuf::from(path));
                index += 2;
            }
            "--work-tree" => {
                let Some(path) = args.get(index + 1) else {
                    eprintln!("no directory given for '--work-tree' option");
                    print_global_usage();
                    return Err(GitError::Exit(129));
                };
                work_tree = Some(PathBuf::from(path));
                index += 2;
            }
            "--attr-source" => {
                let Some(source) = args.get(index + 1) else {
                    eprintln!("error: option `attr-source' requires a value");
                    return Err(GitError::Exit(129));
                };
                attr_source = Some(source.clone());
                index += 2;
            }
            value if value.starts_with("--git-dir=") => {
                git_dir = Some(PathBuf::from(&value["--git-dir=".len()..]));
                index += 1;
            }
            value if value.starts_with("--work-tree=") => {
                work_tree = Some(PathBuf::from(&value["--work-tree=".len()..]));
                index += 1;
            }
            value if value.starts_with("--attr-source=") => {
                attr_source = Some(value["--attr-source=".len()..].to_string());
                index += 1;
            }
            value if value.starts_with("--config-env=") => {
                config.push(push_config_env(&value["--config-env=".len()..])?);
                index += 1;
            }
            "--bare" => {
                bare = true;
                index += 1;
            }
            _ => break,
        }
    }
    Ok(GlobalOptions {
        args: &args[index..],
        config,
        git_dir,
        work_tree,
        attr_source,
        bare,
        replace_objects,
        lazy_fetch,
        pathspec_flags,
    })
}

/// Fold a `-c <text>` command-line parameter into the process
/// `GIT_CONFIG_PARAMETERS` env var, exactly as git's `git_config_push_parameter`:
/// split off the value at the first `=` (a missing `=` is a bare boolean), then
/// sq-quote the key and value into the env list. This makes the override visible
/// to every config read (including aliases and any subprocess) through the single
/// `injected_config_parameters()` reader.
///
/// Returns a [`GlobalConfigOverride`] (canonical-ish key + string value) for the
/// legacy `init`/`clone` override list when the key is non-empty; an empty key
/// (`-c ""`) yields `None` here and surfaces as a parse error at read time.
fn push_config_parameter(text: &str) -> Option<GlobalConfigOverride> {
    match text.split_once('=') {
        Some((key, value)) => {
            push_split_parameter(key, Some(value));
            (!key.is_empty()).then(|| GlobalConfigOverride {
                key: key.to_string(),
                value: value.to_string(),
            })
        }
        None => {
            push_split_parameter(text, None);
            (!text.is_empty()).then(|| GlobalConfigOverride {
                key: text.to_string(),
                // A bare `-c key` is boolean-true; represent it as "true" for the
                // legacy list consumers (init reads typed values via parse_config_bool).
                value: "true".to_string(),
            })
        }
    }
}

/// Resolve a `--config-env=<key>=<envvar>` spec and fold it into
/// `GIT_CONFIG_PARAMETERS`, exactly as git's `git_config_push_env`: the spec is
/// split at the *last* `=` into the config key and the environment variable name;
/// the variable is read from the environment and its value sq-quoted into the env
/// list. Errors mirror git's `die()` wording (exit 128).
fn push_config_env(spec: &str) -> Result<GlobalConfigOverride> {
    let Some(eq) = spec.rfind('=') else {
        eprintln!("fatal: invalid config format: {spec}");
        return Err(GitError::Exit(128));
    };
    let key = &spec[..eq];
    let env_name = &spec[eq + 1..];
    if env_name.is_empty() {
        eprintln!("fatal: missing environment variable name for configuration '{key}'");
        return Err(GitError::Exit(128));
    }
    let env_value = match env::var(env_name) {
        Ok(value) => value,
        Err(_) => {
            eprintln!("fatal: missing environment variable '{env_name}' for configuration '{key}'");
            return Err(GitError::Exit(128));
        }
    };
    push_split_parameter(key, Some(&env_value));
    Ok(GlobalConfigOverride {
        key: key.to_string(),
        value: env_value,
    })
}

/// Append a `key[=value]` pair to the command-line config-parameter fragment in
/// sq-quoted new-style (`'key'='value'`) or bare (`'key'`) form, mirroring git's
/// `git_config_push_split_parameter`. git mutates the process `GIT_CONFIG_PARAMETERS`
/// env var; because the workspace forbids `unsafe` (and thus `std::env::set_var`),
/// sley instead accumulates the fragment in a process-global store. The effective
/// `GIT_CONFIG_PARAMETERS` — the pre-existing env value followed by this fragment —
/// is reconstructed by [`effective_config_parameters_env`] for both in-process
/// reads and any shell-alias subprocess, preserving git's left-to-right precedence.
fn push_split_parameter(key: &str, value: Option<&str>) {
    if let Ok(mut fragment) = CMDLINE_CONFIG_PARAMETERS.lock() {
        if !fragment.is_empty() {
            fragment.push(' ');
        }
        fragment.push_str(&crate::sley_config::sq_quote(key));
        fragment.push('=');
        if let Some(value) = value {
            fragment.push_str(&crate::sley_config::sq_quote(value));
        }
    }
}

/// The effective `GIT_CONFIG_PARAMETERS` string: the inherited env value (if any)
/// followed by the command-line `-c`/`--config-env` fragment, space-separated.
/// This is what git's process env would hold after folding in `-c`, and is both
/// parsed for in-process reads and exported to shell-alias subprocesses so they
/// inherit the parent's overrides.
pub(crate) fn effective_config_parameters_env() -> Option<String> {
    let inherited = env::var("GIT_CONFIG_PARAMETERS")
        .ok()
        .filter(|s| !s.is_empty());
    let fragment = CMDLINE_CONFIG_PARAMETERS
        .lock()
        .ok()
        .map(|f| f.clone())
        .filter(|s| !s.is_empty());
    match (inherited, fragment) {
        (Some(inherited), Some(fragment)) => Some(format!("{inherited} {fragment}")),
        (Some(inherited), None) => Some(inherited),
        (None, Some(fragment)) => Some(fragment),
        (None, None) => None,
    }
}

/// Look up the last-set injected override for `key` (canonicalised), across the
/// full injection stream (`GIT_CONFIG_COUNT` + `GIT_CONFIG_PARAMETERS`, the latter
/// holding any `-c`/`--config-env`). Returns the string value (a bare boolean-true
/// entry yields `"true"`). Used by command-side consumers (init, rev-parse's
/// `core.abbrev`, etc.) that need a single injected value before a full config load.
pub(crate) fn global_config_value(key: &str) -> Result<Option<String>> {
    let canonical = match crate::sley_config::canonicalize_config_key(key) {
        Ok(canonical) => canonical,
        // The lookup key is a fixed internal key; if it fails to canonicalise
        // there can be no matching override.
        Err(_) => return Ok(None),
    };
    let parameters = injected_config_parameters()?;
    Ok(parameters
        .iter()
        .rev()
        .find(|param| param.canonical_key.eq_ignore_ascii_case(&canonical))
        .map(|param| match &param.value {
            Some(value) => value.clone(),
            None => "true".to_string(),
        }))
}

/// Parse the full config-injection stream (env-count pairs plus the effective
/// `GIT_CONFIG_PARAMETERS` = inherited env + command-line `-c`/`--config-env`),
/// converting any parse failure into git's `error: <msg>\nfatal: unable to parse
/// command-line config` two-line diagnostic with exit 128.
pub(crate) fn injected_config_parameters() -> Result<Vec<crate::sley_config::ConfigParameter>> {
    let params_env = effective_config_parameters_env();
    crate::sley_config::injected_config_parameters(params_env.as_deref())
        .map_err(report_config_parameter_error)
}

pub(crate) const DEFAULT_BIG_FILE_THRESHOLD: u64 = 512 * 1024 * 1024;

pub(crate) fn core_big_file_threshold(git_dir: Option<&Path>) -> Result<u64> {
    let context = match git_dir {
        Some(git_dir) => crate::sley_config::ConfigIncludeContext::new(
            Some(crate::sley_config::git_dir_for_include_context(git_dir)),
            crate::sley_config::repo_current_branch_name(git_dir),
        ),
        None => crate::sley_config::ConfigIncludeContext::new(None, None),
    };
    let mut config = crate::sley_config::load_pre_dispatch_config(git_dir, &context)
        .map_err(crate::report_config_setup_error)?;
    let parameters = injected_config_parameters()?;
    let base = match env::current_dir() {
        Ok(path) => path,
        Err(_) => PathBuf::from("."),
    };
    crate::sley_config::append_injected_config_sections_with_includes(
        &mut config,
        &parameters,
        &context,
        &base,
    )
    .map_err(crate::report_config_setup_error)?;
    let Some(value) = config.get("core", None, "bigfilethreshold") else {
        return Ok(DEFAULT_BIG_FILE_THRESHOLD);
    };
    match crate::sley_config::parse_config_int(value) {
        Some(value) if value >= 0 => Ok(value as u64),
        _ => {
            eprintln!(
                "fatal: bad numeric config value '{value}' for 'core.bigfilethreshold': invalid unit"
            );
            Err(GitError::Exit(128))
        }
    }
}

/// Print git's exact diagnostic for a config-injection parse failure and return
/// the matching exit status. Git prints the specific `error:` line followed by a
/// generic `fatal: unable to parse command-line config` and exits 128.
fn report_config_parameter_error(err: crate::sley_config::ConfigParameterError) -> GitError {
    eprintln!("error: {}", err.message());
    eprintln!("fatal: unable to parse command-line config");
    GitError::Exit(128)
}

fn print_global_usage() {
    eprintln!(
        "usage: git [-v | --version] [-h | --help] [-C <path>] [-c <name>=<value>]\n           [--exec-path[=<path>]] [--html-path] [--man-path] [--info-path]\n           [-p | --paginate | -P | --no-pager] [--no-replace-objects] [--no-lazy-fetch]\n           [--no-optional-locks] [--no-advice] [--bare] [--git-dir=<path>]\n           [--work-tree=<path>] [--namespace=<name>] [--config-env=<name>=<envvar>]\n           <command> [<args>]"
    );
}

const ARGV_BYTE_SENTINEL_BASE: u32 = 0xE000;

pub fn argv_string_from_os(arg: OsString) -> String {
    match arg.into_string() {
        Ok(value) => value,
        Err(arg) => argv_string_from_non_utf8_os(arg),
    }
}

#[cfg(unix)]
fn argv_string_from_non_utf8_os(arg: OsString) -> String {
    use std::os::unix::ffi::OsStrExt;
    argv_string_from_bytes(arg.as_os_str().as_bytes())
}

#[cfg(not(unix))]
fn argv_string_from_non_utf8_os(arg: OsString) -> String {
    arg.to_string_lossy().into_owned()
}

pub(crate) fn argv_string_from_bytes(bytes: &[u8]) -> String {
    if let Ok(value) = std::str::from_utf8(bytes) {
        return value.to_string();
    }
    let mut out = String::with_capacity(bytes.len());
    for byte in bytes {
        if byte.is_ascii() {
            out.push(*byte as char);
        } else if let Some(ch) = char::from_u32(ARGV_BYTE_SENTINEL_BASE + u32::from(*byte)) {
            out.push(ch);
        }
    }
    out
}

pub(crate) fn argv_bytes_from_string(value: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len());
    for ch in value.chars() {
        let code = ch as u32;
        if (ARGV_BYTE_SENTINEL_BASE..=ARGV_BYTE_SENTINEL_BASE + 0xff).contains(&code) {
            out.push((code - ARGV_BYTE_SENTINEL_BASE) as u8);
        } else {
            let mut buf = [0u8; 4];
            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
        }
    }
    out
}

#[cfg(unix)]
pub(crate) fn argv_bytes_from_os(value: OsString) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    value.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
pub(crate) fn argv_bytes_from_os(value: OsString) -> Vec<u8> {
    value.to_string_lossy().into_owned().into_bytes()
}
