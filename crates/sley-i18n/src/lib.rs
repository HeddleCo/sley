use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

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
elif type gettext.sh >/dev/null 2>&1
then
	GIT_INTERNAL_GETTEXT_SH_SCHEME=gnu
elif test "$(gettext -h 2>&1)" = "-h"
then
	GIT_INTERNAL_GETTEXT_SH_SCHEME=gettext_without_eval_gettext
fi
export GIT_INTERNAL_GETTEXT_SH_SCHEME

case "$GIT_INTERNAL_GETTEXT_SH_SCHEME" in
gnu)
	. gettext.sh
	;;
gettext_without_eval_gettext)
	eval_gettext () {
		if test -z "${SLEY_BIN-}"
		then
			echo "fatal: SLEY_BIN is required by Sley's shell i18n helper" >&2
			return 127
		fi
		gettext "$1" | (
			export PATH $("$SLEY_BIN" sh-i18n--envsubst --variables "$1")
			"$SLEY_BIN" sh-i18n--envsubst "$1"
		)
	}
	;;
*)
	gettext () {
		printf "%s" "$1"
	}

	eval_gettext () {
		if test -z "${SLEY_BIN-}"
		then
			echo "fatal: SLEY_BIN is required by Sley's shell i18n helper" >&2
			return 127
		fi
		printf "%s" "$1" | (
			export PATH $("$SLEY_BIN" sh-i18n--envsubst --variables "$1")
			"$SLEY_BIN" sh-i18n--envsubst "$1"
		)
	}
	;;
esac

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
if test -z "${SLEY_BIN-}"
then
	echo "fatal: SLEY_BIN is required by Sley's shell i18n helper" >&2
	exit 127
fi
exec "$SLEY_BIN" sh-i18n--envsubst "$@"
"#;

const GIT_UPLOAD_PACK: &str = r#"#!/bin/sh
if test -z "${SLEY_BIN-}"
then
	echo "fatal: SLEY_BIN is required by Sley's upload-pack adapter" >&2
	exit 127
fi
exec "$SLEY_BIN" upload-pack "$@"
"#;

const GIT_RECEIVE_PACK: &str = r#"#!/bin/sh
if test -z "${SLEY_BIN-}"
then
	echo "fatal: SLEY_BIN is required by Sley's receive-pack adapter" >&2
	exit 127
fi
exec "$SLEY_BIN" receive-pack "$@"
"#;

// Keep this byte-for-byte invariant across launcher paths. Upstream waves may
// invoke hardlinks to the same Sley binary concurrently, so embedding
// `current_exe` would make one shared helper generation have multiple owners.
const GIT_HTTP_BACKEND: &str = r#"#!/bin/sh
if test -z "${SLEY_BIN-}"
then
	echo "fatal: SLEY_BIN is required by Sley's http-backend adapter" >&2
	exit 127
fi
exec "$SLEY_BIN" http-backend "$@"
"#;

/// The manifest stored next to Sley's Git-compatible shell helpers.
pub const HELPER_PROVENANCE_FILE: &str = ".sley-helper-provenance";

const HELPER_PROVENANCE: &str = concat!(
    "schema=1\n",
    "owner=sley\n",
    "crate=sley-i18n\n",
    "version=",
    env!("CARGO_PKG_VERSION"),
    "\n",
    "git-sh-i18n\tshell-library\n",
    "git-sh-i18n--envsubst\tsley-cli-adapter\n",
    "git-upload-pack\tsley-cli-adapter\n",
    "git-receive-pack\tsley-cli-adapter\n",
    "git-http-backend\tsley-cli-adapter\n",
);

/// Executables formerly borrowed from an upstream build or system Git.
///
/// These names are removed if found in the native-only helper directory. They
/// are intentionally not materialized until Sley has implementations it owns.
pub const UNIMPLEMENTED_NATIVE_HELPERS: &[&str] = &[
    "git-http-fetch",
    "git-http-push",
    "git-remote-ftp",
    "git-remote-ftps",
    "git-remote-http",
    "git-remote-https",
    "git-upload-archive",
];

/// How a materialized helper is implemented by Sley.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HelperImplementation {
    /// A shell library sourced by Git's shell scripts.
    ShellLibrary,
    /// A shell adapter that dispatches to Sley's native CLI command.
    SleyCliAdapter,
}

/// Provenance for one file Sley owns in its compatibility exec directory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HelperProvenance {
    pub name: &'static str,
    pub implementation: HelperImplementation,
    pub executable: bool,
}

const MATERIALIZED_HELPER_PROVENANCE: &[HelperProvenance] = &[
    HelperProvenance {
        name: "git-sh-i18n",
        implementation: HelperImplementation::ShellLibrary,
        executable: false,
    },
    HelperProvenance {
        name: "git-sh-i18n--envsubst",
        implementation: HelperImplementation::SleyCliAdapter,
        executable: true,
    },
    HelperProvenance {
        name: "git-upload-pack",
        implementation: HelperImplementation::SleyCliAdapter,
        executable: true,
    },
    HelperProvenance {
        name: "git-receive-pack",
        implementation: HelperImplementation::SleyCliAdapter,
        executable: true,
    },
    HelperProvenance {
        name: "git-http-backend",
        implementation: HelperImplementation::SleyCliAdapter,
        executable: true,
    },
];

/// Returns the complete set of helpers Sley materializes under `GIT_EXEC_PATH`.
///
/// Each entry is implemented by Sley. In particular, transport and server
/// helpers that are not native yet are absent rather than borrowed from Git.
pub const fn materialized_helper_provenance() -> &'static [HelperProvenance] {
    MATERIALIZED_HELPER_PROVENANCE
}

/// Materializes and verifies Sley's native compatibility helpers.
///
/// The returned path is stable for this crate version. Materialization is
/// process-safe, including when multiple installed-Git launchers initialize
/// the shared directory concurrently.
pub fn materialize_git_i18n_helpers() -> io::Result<PathBuf> {
    let dir = env::temp_dir().join(format!(
        "sley-git-compat-i18n-native-v1-{}",
        env!("CARGO_PKG_VERSION")
    ));
    materialize_git_i18n_helpers_in(&dir)?;
    Ok(dir)
}

fn materialize_git_i18n_helpers_in(dir: &Path) -> io::Result<()> {
    let parent = dir.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let lock_path = env::temp_dir().join(format!(
        ".sley-git-compat-i18n-native-v1-{}.materialize.lock",
        env!("CARGO_PKG_VERSION")
    ));
    // An OS lock is released even if a materializing process exits abruptly.
    // Its persistent, empty lock file lives beside (not inside) GIT_EXEC_PATH,
    // so exact helper provenance and the stable exec path remain unchanged.
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_path)?;
    lock.lock()?;

    let result = (|| {
        fs::create_dir_all(dir)?;
        write_helper(dir.join("git-sh-i18n"), GIT_SH_I18N, 0o644)?;
        write_helper(
            dir.join("git-sh-i18n--envsubst"),
            GIT_SH_I18N_ENVSUBST,
            0o755,
        )?;
        write_helper(dir.join("git-upload-pack"), GIT_UPLOAD_PACK, 0o755)?;
        write_helper(dir.join("git-receive-pack"), GIT_RECEIVE_PACK, 0o755)?;
        write_helper(dir.join("git-http-backend"), GIT_HTTP_BACKEND, 0o755)?;
        for name in UNIMPLEMENTED_NATIVE_HELPERS {
            remove_legacy_helper(&dir.join(name))?;
        }
        write_helper(dir.join(HELPER_PROVENANCE_FILE), HELPER_PROVENANCE, 0o644)?;
        verify_materialized_git_i18n_helpers(dir)
    })();
    let unlock = lock.unlock();
    result.and(unlock)
}

fn remove_legacy_helper(path: &Path) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    if metadata.is_file() || metadata.file_type().is_symlink() {
        return fs::remove_file(path);
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!("legacy helper path is not a file: {}", path.display()),
    ))
}

/// Verifies that `dir` contains exactly the helpers declared as Sley-owned.
///
/// Verification rejects symlinks, modified helper contents, an invalid
/// provenance manifest, and undeclared files. It is suitable for harnesses
/// enforcing that `GIT_EXEC_PATH` contains no borrowed Git executables.
pub fn verify_materialized_git_i18n_helpers(dir: &Path) -> io::Result<()> {
    verify_regular_file_contents(&dir.join("git-sh-i18n"), GIT_SH_I18N.as_bytes())?;
    verify_regular_file_contents(
        &dir.join("git-sh-i18n--envsubst"),
        GIT_SH_I18N_ENVSUBST.as_bytes(),
    )?;
    verify_regular_file_contents(&dir.join("git-upload-pack"), GIT_UPLOAD_PACK.as_bytes())?;
    verify_regular_file_contents(&dir.join("git-receive-pack"), GIT_RECEIVE_PACK.as_bytes())?;
    verify_regular_file_contents(&dir.join("git-http-backend"), GIT_HTTP_BACKEND.as_bytes())?;
    verify_regular_file_contents(
        &dir.join(HELPER_PROVENANCE_FILE),
        HELPER_PROVENANCE.as_bytes(),
    )?;

    let expected: BTreeSet<OsString> = [
        OsString::from("git-sh-i18n"),
        OsString::from("git-sh-i18n--envsubst"),
        OsString::from("git-upload-pack"),
        OsString::from("git-receive-pack"),
        OsString::from("git-http-backend"),
        OsString::from(HELPER_PROVENANCE_FILE),
    ]
    .into_iter()
    .collect();
    let mut actual = BTreeSet::new();
    for entry in fs::read_dir(dir)? {
        actual.insert(entry?.file_name());
    }
    if actual != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("helper directory contains undeclared entries: {actual:?}"),
        ));
    }
    Ok(())
}

fn verify_regular_file_contents(path: &Path, expected: &[u8]) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("helper is not an owned regular file: {}", path.display()),
        ));
    }
    if fs::read(path)? != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "helper contents do not match Sley's copy: {}",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn write_helper(path: PathBuf, contents: &str, mode: u32) -> io::Result<()> {
    let is_symlink = fs::symlink_metadata(&path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false);
    let needs_write = match fs::read_to_string(&path) {
        Ok(existing) => is_symlink || existing != contents,
        Err(err) if err.kind() == io::ErrorKind::NotFound => true,
        Err(err) => return Err(err),
    };
    if needs_write {
        write_helper_atomically(&path, contents, mode)?;
    }
    set_mode(&path, mode)
}

fn write_helper_atomically(path: &Path, contents: &str, mode: u32) -> io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let parent = path
        .parent()
        .and_then(Path::parent)
        .unwrap_or_else(|| Path::new("."));
    let temp = parent.join(format!(
        ".sley-helper-write-{}-{sequence}",
        std::process::id()
    ));
    fs::write(&temp, contents)?;
    set_mode(&temp, mode)?;
    if let Err(err) = fs::rename(&temp, path) {
        // Windows does not replace an existing destination. Another Sley
        // process may have won the race with the same immutable contents. The
        // materialization lock makes replacement of an older generation safe.
        if fs::read(path).ok().as_deref() == Some(contents.as_bytes()) {
            let _ = fs::remove_file(&temp);
        } else {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(remove_err) if remove_err.kind() == io::ErrorKind::NotFound => {}
                Err(_) => {
                    let _ = fs::remove_file(&temp);
                    return Err(err);
                }
            }
            if let Err(replace_err) = fs::rename(&temp, path) {
                let _ = fs::remove_file(&temp);
                return Err(replace_err);
            }
        }
    }
    Ok(())
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

// ---------------------------------------------------------------------------
// gettext message catalogs (GNU MO) + locale re-encoding
// ---------------------------------------------------------------------------

const GIT_TEXTDOMAIN: &str = "git";
const GIT_TEXTDOMAINDIR_ENV: &str = "GIT_TEXTDOMAINDIR";

struct MessageCatalog {
    /// UTF-8 msgid → UTF-8 msgstr (as stored in git's *.mo files).
    messages: BTreeMap<String, String>,
}

static CATALOG: OnceLock<Option<MessageCatalog>> = OnceLock::new();

/// Translate `msgid` and re-encode to the process locale codeset.
///
/// Mirrors git's `_()` + `bind_textdomain_codeset`: catalogs are UTF-8; output
/// is converted to the codeset of `LC_ALL`/`LC_CTYPE`/`LANG` (e.g. ISO-8859-1).
/// When no catalog is available the original English `msgid` is returned
/// (re-encoded if the locale is not UTF-8).
pub fn gettext(msgid: &str) -> Vec<u8> {
    let translated = lookup_msgid(msgid).unwrap_or(msgid);
    reencode_for_locale(translated)
}

/// `gettext` with sequential `%s` substitution (git's init-style formats).
pub fn gettext_printf(msgid: &str, args: &[&str]) -> Vec<u8> {
    let translated = lookup_msgid(msgid).unwrap_or(msgid);
    let filled = substitute_percent_s(translated, args);
    reencode_for_locale(&filled)
}

fn lookup_msgid(msgid: &str) -> Option<&'static str> {
    let catalog = CATALOG.get_or_init(load_catalog);
    catalog
        .as_ref()
        .and_then(|cat| cat.messages.get(msgid).map(String::as_str))
}

fn load_catalog() -> Option<MessageCatalog> {
    let textdomaindir = env::var_os(GIT_TEXTDOMAINDIR_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)?;
    if !textdomaindir.is_dir() {
        return None;
    }
    for lang in preferred_languages() {
        let mo_path = textdomaindir
            .join(&lang)
            .join("LC_MESSAGES")
            .join(format!("{GIT_TEXTDOMAIN}.mo"));
        if let Ok(bytes) = fs::read(&mo_path)
            && let Some(messages) = parse_mo(&bytes)
        {
            return Some(MessageCatalog { messages });
        }
        // LANGUAGE=is → also try is_IS when only a full locale dir exists.
        if !lang.contains('_') {
            // Prefer any `is_*` directory that has a catalog.
            if let Ok(entries) = fs::read_dir(&textdomaindir) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if name.starts_with(&format!("{lang}_")) || name == lang {
                        let mo_path = entry
                            .path()
                            .join("LC_MESSAGES")
                            .join(format!("{GIT_TEXTDOMAIN}.mo"));
                        if let Ok(bytes) = fs::read(&mo_path)
                            && let Some(messages) = parse_mo(&bytes)
                        {
                            return Some(MessageCatalog { messages });
                        }
                    }
                }
            }
        }
    }
    None
}

/// Preferred language tags from `LANGUAGE`, then `LC_ALL`/`LC_MESSAGES`/`LANG`.
fn preferred_languages() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(language) = env::var("LANGUAGE") {
        for part in language.split(':') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            push_language_variants(part, &mut out);
        }
    }
    for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(value) = env::var(key)
            && !value.is_empty()
            && !value.eq_ignore_ascii_case("C")
            && !value.eq_ignore_ascii_case("POSIX")
        {
            push_language_variants(&value, &mut out);
            break;
        }
    }
    out
}

fn push_language_variants(raw: &str, out: &mut Vec<String>) {
    // Strip encoding: `is_IS.UTF-8` → `is_IS`
    let base = raw.split('.').next().unwrap_or(raw);
    let base = base.split('@').next().unwrap_or(base);
    if base.is_empty()
        || base.eq_ignore_ascii_case("C")
        || base.eq_ignore_ascii_case("POSIX")
    {
        return;
    }
    if !out.iter().any(|existing| existing == base) {
        out.push(base.to_string());
    }
    if let Some((lang, _)) = base.split_once('_')
        && !lang.is_empty()
        && !out.iter().any(|existing| existing == lang)
    {
        out.push(lang.to_string());
    }
}

fn parse_mo(bytes: &[u8]) -> Option<BTreeMap<String, String>> {
    if bytes.len() < 28 {
        return None;
    }
    let magic = u32::from_le_bytes(bytes[0..4].try_into().ok()?);
    let read_u32: fn(&[u8], usize) -> Option<u32> = if magic == 0x9504_12de {
        read_u32_le
    } else if magic == 0xde12_0495 {
        read_u32_be
    } else {
        return None;
    };
    let _revision = read_u32(bytes, 4)?;
    let n = read_u32(bytes, 8)? as usize;
    let o_orig = read_u32(bytes, 12)? as usize;
    let o_trans = read_u32(bytes, 16)? as usize;
    let mut messages = BTreeMap::new();
    for i in 0..n {
        let orig_len = read_u32(bytes, o_orig + i * 8)? as usize;
        let orig_off = read_u32(bytes, o_orig + i * 8 + 4)? as usize;
        let trans_len = read_u32(bytes, o_trans + i * 8)? as usize;
        let trans_off = read_u32(bytes, o_trans + i * 8 + 4)? as usize;
        if orig_off + orig_len > bytes.len() || trans_off + trans_len > bytes.len() {
            continue;
        }
        let msgid = match std::str::from_utf8(&bytes[orig_off..orig_off + orig_len]) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let msgstr = match std::str::from_utf8(&bytes[trans_off..trans_off + trans_len]) {
            Ok(s) => s,
            Err(_) => continue,
        };
        // Skip header (empty msgid).
        if msgid.is_empty() {
            continue;
        }
        // Plural forms store `\0`-separated variants; take the singular.
        let msgid = msgid.split('\0').next().unwrap_or(msgid);
        let msgstr = msgstr.split('\0').next().unwrap_or(msgstr);
        messages.insert(msgid.to_string(), msgstr.to_string());
    }
    Some(messages)
}

fn read_u32_le(bytes: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(off..off + 4)?.try_into().ok()?))
}

fn read_u32_be(bytes: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(off..off + 4)?.try_into().ok()?))
}

fn substitute_percent_s(template: &str, args: &[&str]) -> String {
    let mut out = String::with_capacity(template.len() + args.iter().map(|a| a.len()).sum::<usize>());
    let mut arg_idx = 0;
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b's' => {
                    if let Some(arg) = args.get(arg_idx) {
                        out.push_str(arg);
                        arg_idx += 1;
                    }
                    i += 2;
                    continue;
                }
                b'%' => {
                    out.push('%');
                    i += 2;
                    continue;
                }
                _ => {}
            }
        }
        // Preserve UTF-8 by copying the char, not a single byte.
        let ch = template[i..].chars().next().unwrap_or('\u{fffd}');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn reencode_for_locale(utf8_text: &str) -> Vec<u8> {
    let codeset = locale_codeset();
    if codeset_is_utf8(&codeset) {
        return utf8_text.as_bytes().to_vec();
    }
    let encoding = encoding_for_codeset(&codeset).unwrap_or(encoding_rs::UTF_8);
    if encoding == encoding_rs::UTF_8 {
        return utf8_text.as_bytes().to_vec();
    }
    let (encoded, _, _) = encoding.encode(utf8_text);
    encoded.into_owned()
}

fn locale_codeset() -> String {
    for key in ["LC_ALL", "LC_CTYPE", "LANG"] {
        if let Ok(value) = env::var(key)
            && !value.is_empty()
        {
            if let Some((_, codeset)) = value.split_once('.') {
                let codeset = codeset.split('@').next().unwrap_or(codeset);
                if !codeset.is_empty() {
                    return codeset.to_string();
                }
            }
            // Locale without explicit codeset (e.g. `is_IS`): treat as UTF-8
            // when the name does not hint otherwise.
            return "UTF-8".to_string();
        }
    }
    "UTF-8".to_string()
}

fn codeset_is_utf8(codeset: &str) -> bool {
    let c = codeset.to_ascii_lowercase();
    c == "utf-8" || c == "utf8"
}

fn encoding_for_codeset(codeset: &str) -> Option<&'static encoding_rs::Encoding> {
    let compact: String = codeset
        .bytes()
        .filter(|b| !matches!(*b, b'-' | b'_' | b' '))
        .map(|b| b.to_ascii_uppercase() as char)
        .collect();
    match compact.as_str() {
        "UTF8" => Some(encoding_rs::UTF_8),
        "ISO88591" | "LATIN1" | "88591" => Some(encoding_rs::WINDOWS_1252),
        "ISO885915" | "LATIN9" => encoding_rs::Encoding::for_label(b"iso-8859-15"),
        _ => encoding_rs::Encoding::for_label(codeset.as_bytes()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);
    // The parents of these tests both launch the current libtest executable.
    // Serialize only those parents; child-marker branches return before taking
    // this lock, so a parent can still wait for all of its children.
    static SUBPROCESS_TEST_LOCK: Mutex<()> = Mutex::new(());

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> io::Result<Self> {
            let sequence = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!(
                "sley-i18n-test-{label}-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&path)?;
            Ok(Self(path))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn variables_are_unique_in_order() {
        assert_eq!(
            envsubst_variables("a $one ${two} $one ${bad:-no} $9 $_ok"),
            vec!["one", "two", "_ok"]
        );
    }

    #[test]
    fn substitute_percent_s_fills_in_order() {
        assert_eq!(
            substitute_percent_s("in %s%s\n", &["/tmp/repo/.git", "/"]),
            "in /tmp/repo/.git/\n"
        );
        assert_eq!(substitute_percent_s("100%% done", &[]), "100% done");
    }

    #[test]
    fn parse_mo_reads_utf8_msgstr() {
        // Minimal little-endian MO with one entry: "hi" → "halló"
        // Header (7 u32) + 2 string descriptor pairs + string table.
        let msgid = b"hi";
        let msgstr = "halló".as_bytes();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x9504_12deu32.to_le_bytes()); // magic
        bytes.extend_from_slice(&0u32.to_le_bytes()); // revision
        bytes.extend_from_slice(&1u32.to_le_bytes()); // nstrings
        // orig table offset = 28, trans table offset = 36
        bytes.extend_from_slice(&28u32.to_le_bytes());
        bytes.extend_from_slice(&36u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes()); // hash size
        bytes.extend_from_slice(&0u32.to_le_bytes()); // hash offset
        // orig desc at 28: len, off
        let strings_off = 44u32;
        bytes.extend_from_slice(&(msgid.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&strings_off.to_le_bytes());
        // trans desc at 36
        bytes.extend_from_slice(&(msgstr.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(strings_off + msgid.len() as u32).to_le_bytes());
        bytes.extend_from_slice(msgid);
        bytes.extend_from_slice(msgstr);
        let map = parse_mo(&bytes).expect("parse mo");
        assert_eq!(map.get("hi").map(String::as_str), Some("halló"));
    }

    #[test]
    fn substitutes_only_format_variables() {
        let out = envsubst("a $one ${two} $three", "x $one ${two}", |name| {
            Some(format!("<{name}>"))
        });
        assert_eq!(out, "a <one> <two> $three");
    }

    #[test]
    fn materializes_only_declared_sley_owned_helpers() -> io::Result<()> {
        let temp = TestDir::new("owned-only")?;
        for name in UNIMPLEMENTED_NATIVE_HELPERS {
            fs::write(temp.path().join(name), b"borrowed git executable")?;
        }

        materialize_git_i18n_helpers_in(temp.path())?;
        verify_materialized_git_i18n_helpers(temp.path())?;
        let shell_library = fs::read_to_string(temp.path().join("git-sh-i18n"))?;
        let envsubst_adapter = fs::read_to_string(temp.path().join("git-sh-i18n--envsubst"))?;
        let upload_pack_adapter = fs::read_to_string(temp.path().join("git-upload-pack"))?;
        let receive_pack_adapter = fs::read_to_string(temp.path().join("git-receive-pack"))?;
        let http_backend_adapter = fs::read_to_string(temp.path().join("git-http-backend"))?;
        assert!(!shell_library.contains("$(git "));
        assert!(!shell_library.contains("\n\tgit "));
        assert!(!envsubst_adapter.contains("exec git "));
        assert!(shell_library.contains("$SLEY_BIN"));
        assert!(envsubst_adapter.contains("$SLEY_BIN"));
        assert!(upload_pack_adapter.contains("exec \"$SLEY_BIN\" upload-pack"));
        assert!(receive_pack_adapter.contains("exec \"$SLEY_BIN\" receive-pack"));
        assert!(http_backend_adapter.contains("exec \"$SLEY_BIN\" http-backend"));
        assert!(!http_backend_adapter.contains(env::current_exe()?.to_string_lossy().as_ref()));

        assert_eq!(
            materialized_helper_provenance(),
            &[
                HelperProvenance {
                    name: "git-sh-i18n",
                    implementation: HelperImplementation::ShellLibrary,
                    executable: false,
                },
                HelperProvenance {
                    name: "git-sh-i18n--envsubst",
                    implementation: HelperImplementation::SleyCliAdapter,
                    executable: true,
                },
                HelperProvenance {
                    name: "git-upload-pack",
                    implementation: HelperImplementation::SleyCliAdapter,
                    executable: true,
                },
                HelperProvenance {
                    name: "git-receive-pack",
                    implementation: HelperImplementation::SleyCliAdapter,
                    executable: true,
                },
                HelperProvenance {
                    name: "git-http-backend",
                    implementation: HelperImplementation::SleyCliAdapter,
                    executable: true,
                },
            ]
        );
        for name in UNIMPLEMENTED_NATIVE_HELPERS {
            assert!(!temp.path().join(name).exists(), "{name} must stay absent");
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn shell_helper_probes_gettext_and_preserves_forced_fallback() -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let temp = TestDir::new("gettext-probe")?;
        let bin = temp.path().join("bin");
        let helpers = temp.path().join("helpers");
        fs::create_dir_all(&bin)?;
        fs::create_dir_all(&helpers)?;
        let gettext_sh = bin.join("gettext.sh");
        fs::write(
            &gettext_sh,
            b"gettext () { printf 'gnu:%s' \"$1\"; }\neval_gettext () { gettext \"$1\"; }\n",
        )?;
        let mut permissions = fs::metadata(&gettext_sh)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&gettext_sh, permissions)?;
        materialize_git_i18n_helpers_in(&helpers)?;
        let helper = helpers.join("git-sh-i18n");
        let path = env::join_paths([bin.as_path(), Path::new("/usr/bin"), Path::new("/bin")])
            .map_err(io::Error::other)?;

        let probed = std::process::Command::new("sh")
            .arg("-c")
            .arg(". \"$1\"; printf '%s\\n' \"$GIT_INTERNAL_GETTEXT_SH_SCHEME\"; gettext value")
            .arg("sh")
            .arg(&helper)
            .env("PATH", &path)
            .output()?;
        assert!(probed.status.success());
        assert_eq!(probed.stdout, b"gnu\ngnu:value");

        let fallback = std::process::Command::new("sh")
            .arg("-c")
            .arg("GIT_INTERNAL_GETTEXT_TEST_FALLBACKS=1; export GIT_INTERNAL_GETTEXT_TEST_FALLBACKS; . \"$1\"; printf '%s' \"$GIT_INTERNAL_GETTEXT_SH_SCHEME\"")
            .arg("sh")
            .arg(&helper)
            .env("PATH", &path)
            .output()?;
        assert!(fallback.status.success());
        assert_eq!(fallback.stdout, b"fallthrough");
        Ok(())
    }

    #[test]
    fn provenance_verification_rejects_undeclared_helpers() -> io::Result<()> {
        let temp = TestDir::new("provenance")?;
        materialize_git_i18n_helpers_in(temp.path())?;
        fs::write(temp.path().join("git-http-fetch"), b"system git")?;
        let error = verify_materialized_git_i18n_helpers(temp.path())
            .expect_err("an undeclared executable must invalidate provenance");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn materialization_replaces_linked_helpers_with_owned_files() -> io::Result<()> {
        use std::os::unix::fs::symlink;

        let temp = TestDir::new("linked-helper")?;
        let borrowed = temp.path().join("borrowed-git-sh-i18n");
        let helper_dir = temp.path().join("exec");
        fs::create_dir_all(&helper_dir)?;
        fs::write(&borrowed, b"system Git helper")?;
        symlink(&borrowed, helper_dir.join("git-sh-i18n"))?;
        symlink(&borrowed, helper_dir.join("git-http-backend"))?;

        materialize_git_i18n_helpers_in(&helper_dir)?;
        verify_materialized_git_i18n_helpers(&helper_dir)?;
        assert!(
            !fs::symlink_metadata(helper_dir.join("git-sh-i18n"))?
                .file_type()
                .is_symlink()
        );
        assert!(
            !fs::symlink_metadata(helper_dir.join("git-http-backend"))?
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_to_string(helper_dir.join("git-http-backend"))?,
            GIT_HTTP_BACKEND
        );
        Ok(())
    }

    #[test]
    fn concurrent_processes_materialize_one_invariant_helper_generation() -> io::Result<()> {
        // Keep multiple executable identities while avoiding eight concurrent
        // copies of the libtest harness. The workers still place eight actual
        // materialization calls behind one cross-process start barrier.
        const PROCESS_COUNT: usize = 2;
        const WORKERS_PER_PROCESS: usize = 4;
        const MATERIALIZER_COUNT: usize = PROCESS_COUNT * WORKERS_PER_PROCESS;
        const CHILD_ENV: &str = "SLEY_I18N_CONCURRENT_CHILD";
        const HELPER_DIR_ENV: &str = "SLEY_I18N_CONCURRENT_HELPER_DIR";
        const START_ENV: &str = "SLEY_I18N_CONCURRENT_START";
        const IDENTITY_DIR_ENV: &str = "SLEY_I18N_CONCURRENT_IDENTITY_DIR";

        if let Some(index) = env::var_os(CHILD_ENV) {
            let index = index.to_string_lossy().into_owned();
            let helper_dir = PathBuf::from(env::var_os(HELPER_DIR_ENV).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "missing concurrent helper dir")
            })?);
            let start = PathBuf::from(env::var_os(START_ENV).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "missing concurrent start marker",
                )
            })?);
            let identity_dir = PathBuf::from(env::var_os(IDENTITY_DIR_ENV).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "missing identity dir")
            })?);
            fs::write(
                identity_dir.join(&index),
                env::current_exe()?.to_string_lossy().as_bytes(),
            )?;
            let mut workers = Vec::new();
            for worker in 0..WORKERS_PER_PROCESS {
                let helper_dir = helper_dir.clone();
                let start = start.clone();
                let ready = identity_dir.join(format!("ready-{index}-{worker}"));
                workers.push(std::thread::spawn(move || -> io::Result<()> {
                    fs::write(ready, b"ready\n")?;
                    for _ in 0..500 {
                        if start.exists() {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    if !start.exists() {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "concurrent materialization start marker was not published",
                        ));
                    }
                    for _ in 0..8 {
                        materialize_git_i18n_helpers_in(&helper_dir)?;
                        verify_materialized_git_i18n_helpers(&helper_dir)?;
                    }
                    Ok(())
                }));
            }
            for worker in workers {
                worker
                    .join()
                    .map_err(|_| io::Error::other("materializer worker panicked"))??;
            }
            return Ok(());
        }

        let _subprocess_guard = SUBPROCESS_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let temp = TestDir::new("concurrent-processes")?;
        let helper_dir = temp.path().join("helpers");
        let launcher_dir = temp.path().join("launchers");
        let identity_dir = temp.path().join("identities");
        let start = temp.path().join("start");
        fs::create_dir_all(&launcher_dir)?;
        fs::create_dir_all(&identity_dir)?;

        let current_exe = env::current_exe()?;
        let mut children = Vec::new();
        for index in 0..PROCESS_COUNT {
            let mut launcher = launcher_dir.join(format!("materializer-{index}"));
            if let Some(extension) = current_exe.extension() {
                launcher.set_extension(extension);
            }
            if fs::hard_link(&current_exe, &launcher).is_err() {
                fs::copy(&current_exe, &launcher)?;
                set_mode(&launcher, 0o755)?;
            }
            let child = std::process::Command::new(&launcher)
                .arg("--exact")
                .arg("tests::concurrent_processes_materialize_one_invariant_helper_generation")
                .arg("--nocapture")
                .arg("--test-threads=1")
                .env(CHILD_ENV, index.to_string())
                .env(HELPER_DIR_ENV, &helper_dir)
                .env(START_ENV, &start)
                .env(IDENTITY_DIR_ENV, &identity_dir)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()?;
            children.push((index, child));
        }
        let mut ready_count = 0;
        for _ in 0..500 {
            ready_count = fs::read_dir(&identity_dir)?
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().starts_with("ready-"))
                .count();
            if ready_count == MATERIALIZER_COUNT {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        fs::write(&start, b"go\n")?;

        let mut failures = Vec::new();
        for (index, child) in children {
            let output = child.wait_with_output()?;
            if !output.status.success() {
                failures.push(format!(
                    "materializer {index} failed with {}\nstdout:\n{}\nstderr:\n{}",
                    output.status,
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
        }
        if !failures.is_empty() {
            return Err(io::Error::other(failures.join("\n")));
        }
        assert_eq!(
            ready_count, MATERIALIZER_COUNT,
            "every materializer must reach the cross-process barrier"
        );

        let identities = (0..PROCESS_COUNT)
            .map(|index| fs::read_to_string(identity_dir.join(index.to_string())))
            .collect::<io::Result<BTreeSet<_>>>()?;
        assert_eq!(
            identities.len(),
            PROCESS_COUNT,
            "the regression must exercise distinct executable identities"
        );
        verify_materialized_git_i18n_helpers(&helper_dir)?;
        let http_backend = fs::read_to_string(helper_dir.join("git-http-backend"))?;
        assert_eq!(http_backend, GIT_HTTP_BACKEND);
        assert!(
            identities
                .iter()
                .all(|identity| !http_backend.contains(identity)),
            "the shared helper generation must not capture a launcher identity"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn materialization_ignores_hostile_git_environments() -> io::Result<()> {
        const CHILD_ENV: &str = "SLEY_I18N_HOSTILE_ENV_CHILD";
        const MARKER_ENV: &str = "SLEY_I18N_HOSTILE_GIT_MARKER";

        if env::var_os(CHILD_ENV).is_some() {
            let dir = materialize_git_i18n_helpers()?;
            verify_materialized_git_i18n_helpers(&dir)?;
            for name in UNIMPLEMENTED_NATIVE_HELPERS {
                assert!(!dir.join(name).exists(), "{name} must not be borrowed");
            }
            return Ok(());
        }

        let _subprocess_guard = SUBPROCESS_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let temp = TestDir::new("hostile-env")?;
        let bin_dir = temp.path().join("bin");
        let build_dir = temp.path().join("git-build");
        let source_dir = temp.path().join("git-source");
        let exec_dir = temp.path().join("git-exec");
        let child_tmp = temp.path().join("tmp");
        for dir in [&bin_dir, &build_dir, &source_dir, &exec_dir, &child_tmp] {
            fs::create_dir_all(dir)?;
        }

        let marker = temp.path().join("git-was-probed");
        let hostile_git = bin_dir.join("git");
        fs::write(
            &hostile_git,
            format!("#!/bin/sh\nprintf probed > \"${{{MARKER_ENV}}}\"\nexit 97\n"),
        )?;
        set_mode(&hostile_git, 0o755)?;
        for dir in [&build_dir, &source_dir, &exec_dir] {
            for name in UNIMPLEMENTED_NATIVE_HELPERS {
                let helper = dir.join(name);
                fs::write(&helper, format!("hostile source: {}\n", helper.display()))?;
                set_mode(&helper, 0o755)?;
            }
        }

        let output = std::process::Command::new(env::current_exe()?)
            .arg("--exact")
            .arg("tests::materialization_ignores_hostile_git_environments")
            .arg("--nocapture")
            .env(CHILD_ENV, "1")
            .env(MARKER_ENV, &marker)
            .env("PATH", &bin_dir)
            .env("GIT_BUILD_DIR", &build_dir)
            .env("GIT_SRC_DIR", &source_dir)
            .env("GIT_TEST_EXEC_PATH", &exec_dir)
            .env("SLEY_TEST_GIT", &hostile_git)
            .env("GIT_TEST_GIT", &hostile_git)
            .env("TMPDIR", &child_tmp)
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "hostile-environment child failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        assert!(!marker.exists(), "the hostile Git executable was probed");
        Ok(())
    }
}
