use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

const GIT_SH_I18N: &str = r#"# Git-compatible shell i18n fallback helpers for Sley.
TEXTDOMAIN=git
export TEXTDOMAIN
if test -z "$GIT_TEXTDOMAINDIR"
then
	TEXTDOMAINDIR=
else
	TEXTDOMAINDIR="$GIT_TEXTDOMAINDIR"
fi
export TEXTDOMAINDIR

GIT_INTERNAL_GETTEXT_SH_SCHEME=fallthrough
if test -n "$GIT_INTERNAL_GETTEXT_TEST_FALLBACKS"
then
	: no probing necessary
fi
export GIT_INTERNAL_GETTEXT_SH_SCHEME

gettext () {
	printf "%s" "$1"
}

eval_gettext () {
	printf "%s" "$1" | (
		export PATH $(git sh-i18n--envsubst --variables "$1")
		git sh-i18n--envsubst "$1"
	)
}

gettextln () {
	gettext "$1"
	echo
}

eval_gettextln () {
	eval_gettext "$1"
	echo
}
"#;

const GIT_SH_I18N_ENVSUBST: &str = r#"#!/bin/sh
exec git sh-i18n--envsubst "$@"
"#;

/// Git CGI/remote helpers that upstream expects under `GIT_EXEC_PATH` but that
/// sley does not implement itself. Symlinked from the oracle/system git
/// install when available so Apache `git-http-backend` and bundle-uri downloads
/// via `git-remote-https` work under `GIT_TEST_INSTALLED`.
const SYSTEM_GIT_EXEC_HELPERS: &[&str] = &[
    "git-http-backend",
    "git-remote-http",
    "git-remote-https",
];

/// Prefer the upstream git build for upload-pack when present: it advertises
/// protocol v2 features (bundle-uri, fetch filter) that a stock install may omit.
const PREFERRED_BUILD_EXEC_HELPERS: &[&str] = &["git-upload-pack"];

pub fn materialize_git_i18n_helpers() -> io::Result<PathBuf> {
    let dir = env::temp_dir().join(format!(
        "sley-git-compat-i18n-{}",
        env!("CARGO_PKG_VERSION")
    ));
    fs::create_dir_all(&dir)?;
    write_helper(dir.join("git-sh-i18n"), GIT_SH_I18N, 0o644)?;
    write_helper(
        dir.join("git-sh-i18n--envsubst"),
        GIT_SH_I18N_ENVSUBST,
        0o755,
    )?;
    link_system_git_exec_helpers(&dir)?;
    Ok(dir)
}

fn link_system_git_exec_helpers(dir: &Path) -> io::Result<()> {
    let system_exec_path = discover_system_git_exec_path();
    let build_exec_path = discover_build_git_exec_path();
    for name in PREFERRED_BUILD_EXEC_HELPERS {
        let Some(source) = build_exec_path
            .as_ref()
            .map(|path| path.join(name))
            .filter(|path| path.is_file())
            .or_else(|| {
                system_exec_path
                    .as_ref()
                    .map(|path| path.join(name))
                    .filter(|path| path.is_file())
            })
        else {
            continue;
        };
        link_exec_helper(dir, name, &source)?;
    }
    let Some(system_exec_path) = system_exec_path else {
        return Ok(());
    };
    for name in SYSTEM_GIT_EXEC_HELPERS {
        let source = system_exec_path.join(name);
        if !source.is_file() {
            continue;
        }
        link_exec_helper(dir, name, &source)?;
    }
    Ok(())
}

#[cfg(unix)]
fn link_exec_helper(dir: &Path, name: &str, source: &Path) -> io::Result<()> {
    use std::os::unix::fs::symlink;
    // Apache's CGI handler rejects a symlinked ScriptAlias target (AH00037
    // "Symbolic link not allowed"), and upstream lib-httpd points /smart/ at
    // ${GIT_EXEC_PATH}/git-http-backend — that helper must be a real file.
    if name == "git-http-backend" {
        return copy_exec_helper(dir, name, source);
    }
    let dest = dir.join(name);
    if dest.exists() {
        if dest
            .read_link()
            .ok()
            .filter(|target| target == source)
            .is_some()
        {
            return Ok(());
        }
        fs::remove_file(&dest)?;
    }
    if let Err(err) = symlink(source, &dest) {
        if err.kind() != io::ErrorKind::AlreadyExists {
            return Err(err);
        }
    }
    Ok(())
}

/// Materialize `name` as a regular-file copy, replacing a stale symlink from
/// an earlier shim build. Size equality is the freshness check so repeated
/// sley invocations don't recopy on every run.
fn copy_exec_helper(dir: &Path, name: &str, source: &Path) -> io::Result<()> {
    let dest = dir.join(name);
    if let Ok(metadata) = dest.symlink_metadata() {
        if metadata.is_file()
            && source
                .metadata()
                .map(|source_metadata| source_metadata.len() == metadata.len())
                .unwrap_or(false)
        {
            return Ok(());
        }
        fs::remove_file(&dest)?;
    }
    fs::copy(source, &dest)?;
    Ok(())
}

#[cfg(not(unix))]
fn link_exec_helper(dir: &Path, name: &str, source: &Path) -> io::Result<()> {
    let dest = dir.join(name);
    if dest.exists() {
        if fs::canonicalize(&dest).ok().as_deref() == fs::canonicalize(source).ok().as_deref() {
            return Ok(());
        }
        fs::remove_file(&dest)?;
    }
    fs::copy(source, &dest)?;
    Ok(())
}

fn discover_build_git_exec_path() -> Option<PathBuf> {
    for var in ["GIT_BUILD_DIR", "GIT_SRC_DIR"] {
        if let Ok(path) = env::var(var)
            && !path.is_empty()
        {
            let candidate = PathBuf::from(path);
            if candidate.join("git-upload-pack").is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn discover_system_git_exec_path() -> Option<PathBuf> {
    if let Ok(path) = env::var("GIT_TEST_EXEC_PATH")
        && !path.is_empty()
        && git_exec_helper_dir(Path::new(&path)).is_some()
    {
        return Some(PathBuf::from(path));
    }
    if let Ok(path) = env::var("SLEY_TEST_GIT")
        && !path.is_empty()
        && let Some(exec_path) = git_exec_path_from_program(&path)
    {
        return Some(exec_path);
    }
    if let Ok(path) = env::var("GIT_TEST_GIT")
        && !path.is_empty()
        && let Some(exec_path) = git_exec_path_from_program(&path)
    {
        return Some(exec_path);
    }
    let current_exe = env::current_exe().ok();
    if let Some(path_var) = env::var_os("PATH") {
        for dir in env::split_paths(&path_var) {
            let candidate = dir.join("git");
            if !is_executable_file(&candidate) {
                continue;
            }
            if current_exe.as_ref().is_some_and(|exe| exe == &candidate) {
                continue;
            }
            if let Some(exec_path) = git_exec_path_from_program(candidate.to_string_lossy().as_ref())
            {
                return Some(exec_path);
            }
        }
    }
    for candidate in [
        "/opt/homebrew/opt/git/libexec/git-core",
        "/usr/local/libexec/git-core",
        "/usr/lib/git-core",
        "/usr/libexec/git-core",
    ] {
        let path = PathBuf::from(candidate);
        if git_exec_helper_dir(&path).is_some() {
            return Some(path);
        }
    }
    None
}

fn git_exec_helper_dir(path: &Path) -> Option<()> {
    path.join("git-http-backend").is_file().then_some(())
}

fn git_exec_path_from_program(program: &str) -> Option<PathBuf> {
    let output = Command::new(program).arg("--exec-path").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed))
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path).map(|meta| meta.is_file()).unwrap_or(false)
}

fn write_helper(path: PathBuf, contents: &str, mode: u32) -> io::Result<()> {
    let needs_write = match fs::read_to_string(&path) {
        Ok(existing) => existing != contents,
        Err(err) if err.kind() == io::ErrorKind::NotFound => true,
        Err(err) => return Err(err),
    };
    if needs_write {
        fs::write(&path, contents)?;
    }
    set_mode(&path, mode)
}

#[cfg(unix)]
fn set_mode(path: &std::path::Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(mode);
    fs::set_permissions(path, permissions)
}

#[cfg(not(unix))]
fn set_mode(_path: &std::path::Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

pub fn envsubst_variables(format: &str) -> Vec<String> {
    let mut variables = Vec::new();
    let mut seen = BTreeSet::new();
    scan_variables(format, |name| {
        if seen.insert(name.to_string()) {
            variables.push(name.to_string());
        }
    });
    variables
}

pub fn envsubst(
    input: &str,
    format: &str,
    mut lookup: impl FnMut(&str) -> Option<String>,
) -> String {
    let allowed: BTreeSet<String> = envsubst_variables(format).into_iter().collect();
    let mut output = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'$' {
            output.push(bytes[index] as char);
            index += 1;
            continue;
        }
        if let Some((name, end)) = parse_braced_variable(bytes, index) {
            if allowed.contains(name) {
                output.push_str(&lookup(name).unwrap_or_default());
            } else {
                output.push_str(&input[index..end]);
            }
            index = end;
            continue;
        }
        if let Some((name, end)) = parse_plain_variable(bytes, index) {
            if allowed.contains(name) {
                output.push_str(&lookup(name).unwrap_or_default());
            } else {
                output.push_str(&input[index..end]);
            }
            index = end;
            continue;
        }
        output.push('$');
        index += 1;
    }
    output
}

fn scan_variables(format: &str, mut visit: impl FnMut(&str)) {
    let bytes = format.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'$' {
            index += 1;
            continue;
        }
        if let Some((name, end)) = parse_braced_variable(bytes, index) {
            visit(name);
            index = end;
            continue;
        }
        if let Some((name, end)) = parse_plain_variable(bytes, index) {
            visit(name);
            index = end;
            continue;
        }
        index += 1;
    }
}

fn parse_braced_variable(bytes: &[u8], dollar: usize) -> Option<(&str, usize)> {
    if bytes.get(dollar + 1) != Some(&b'{') {
        return None;
    }
    let start = dollar + 2;
    let first = *bytes.get(start)?;
    if !is_variable_start(first) {
        return None;
    }
    let mut end = start + 1;
    while bytes
        .get(end)
        .is_some_and(|byte| is_variable_continue(*byte))
    {
        end += 1;
    }
    if bytes.get(end) != Some(&b'}') {
        return None;
    }
    std::str::from_utf8(&bytes[start..end])
        .ok()
        .map(|name| (name, end + 1))
}

fn parse_plain_variable(bytes: &[u8], dollar: usize) -> Option<(&str, usize)> {
    let start = dollar + 1;
    let first = *bytes.get(start)?;
    if !is_variable_start(first) {
        return None;
    }
    let mut end = start + 1;
    while bytes
        .get(end)
        .is_some_and(|byte| is_variable_continue(*byte))
    {
        end += 1;
    }
    std::str::from_utf8(&bytes[start..end])
        .ok()
        .map(|name| (name, end))
}

fn is_variable_start(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphabetic()
}

fn is_variable_continue(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphanumeric()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variables_are_unique_in_order() {
        assert_eq!(
            envsubst_variables("a $one ${two} $one ${bad:-no} $9 $_ok"),
            vec!["one", "two", "_ok"]
        );
    }

    #[test]
    fn substitutes_only_format_variables() {
        let out = envsubst("a $one ${two} $three", "x $one ${two}", |name| {
            Some(format!("<{name}>"))
        });
        assert_eq!(out, "a <one> <two> $three");
    }
}
