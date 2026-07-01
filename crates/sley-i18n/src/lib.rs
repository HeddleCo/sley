use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;

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
    Ok(dir)
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
