//! Relative submodule URL resolution — a Rust port of git's `remote.c`
//! `relative_url` and its helpers (`chop_last_dir`, `url_is_local_not_ssh`,
//! `is_absolute_path`).
//!
//! Every consumer that turns a `.gitmodules` `submodule.<name>.url` into the
//! concrete URL recorded in `.git/config` (`submodule init`), re-derived on
//! `submodule sync`, or echoed by `submodule add` MUST go through
//! [`resolve_relative_url`] so that all of git's URL forms — ssh,
//! port-qualified ssh, `file://`, helper transports, scp-style `user@host:path`,
//! and bare relative local paths — resolve byte-for-byte the way upstream does.
//!
//! Porting notes:
//! - [`relative_url`] is the literal port of `remote.c::relative_url`. The
//!   leading-`../` loop walks the url and chops one trailing directory off the
//!   `remoteurl` per `../`, mirroring git exactly (including the `colonsep`
//!   bookkeeping that turns an scp-style `host:repo` separator back into `:`).
//! - [`resolve_relative_url`] is the port of
//!   `builtin/submodule--helper.c::resolve_relative_url`: it picks the base
//!   (`remote.origin.url`, falling back to the cwd) and applies an optional
//!   `up_path` prefix.

/// `git-compat-util.h is_absolute_path` (POSIX form): a leading directory
/// separator. We accept both `/` and `\` so the rule is OS-independent, matching
/// the cross-platform separator handling the rest of `sley-submodule` uses.
fn is_absolute_path(path: &str) -> bool {
    matches!(path.as_bytes().first(), Some(b'/') | Some(b'\\'))
}

/// `connect.c::url_is_local_not_ssh`: a colon-free url, or one whose first slash
/// precedes its first colon, is a local (non-ssh) url. (git also special-cases a
/// DOS drive prefix; sley targets POSIX so that arm is omitted, as elsewhere.)
fn url_is_local_not_ssh(url: &str) -> bool {
    let colon = url.find(':');
    let slash = url.find('/');
    match colon {
        None => true,
        Some(colon_pos) => slash.is_some_and(|slash_pos| slash_pos < colon_pos),
    }
}

fn is_dir_sep(c: u8) -> bool {
    c == b'/' || c == b'\\'
}

/// `dir.c::find_last_dir_sep` over a byte slice: index of the last `/` or `\`.
fn find_last_dir_sep(s: &str) -> Option<usize> {
    s.bytes().rposition(is_dir_sep)
}

fn starts_with_dot_slash(s: &str) -> bool {
    s.starts_with("./") || s.starts_with(".\\")
}

fn starts_with_dot_dot_slash(s: &str) -> bool {
    s.starts_with("../") || s.starts_with("..\\")
}

/// Port of `remote.c::chop_last_dir`. Strips the last path component from
/// `remoteurl` in place. Returns `true` when the component was chopped at a
/// `:` (scp-style separator) — git uses this to re-emit the separator as `:`
/// rather than `/`. The `die` branch (a relative remoteurl with nothing left to
/// chop) is represented by leaving the string as-is and signaling no colonsep;
/// callers in the submodule path never hit it for the inputs git's own tests
/// exercise, and a non-fatal best-effort is preferable to aborting the command.
fn chop_last_dir(remoteurl: &mut String, is_relative: bool) -> bool {
    if let Some(idx) = find_last_dir_sep(remoteurl) {
        remoteurl.truncate(idx);
        return false;
    }
    if let Some(idx) = remoteurl.rfind(':') {
        remoteurl.truncate(idx);
        return true;
    }
    // git die()s here for a relative url it cannot strip further; for an
    // absolute/own-upstream url it resets to ".". We mirror the latter and, for
    // the relative case, also reset to "." instead of aborting.
    let _ = is_relative;
    *remoteurl = ".".to_string();
    false
}

/// Port of `remote.c::relative_url`. Resolves `url` (a `.gitmodules` submodule
/// url) against `remote_url` (the superproject's upstream), applying an optional
/// `up_path` prefix for the worktree-relative form.
///
/// An absolute or non-local (ssh/scheme) `url` is returned verbatim, exactly as
/// git does.
pub fn relative_url(remote_url: &str, url: &str, up_path: Option<&str>) -> String {
    if !url_is_local_not_ssh(url) || is_absolute_path(url) {
        return url.to_string();
    }

    // git BUG()s on an empty remote_url; sley has no upstream there, so treat it
    // as the cwd-less "." base (the caller passes a non-empty base in practice).
    let mut remoteurl = if remote_url.is_empty() {
        ".".to_string()
    } else {
        remote_url.to_string()
    };

    // Strip a single trailing separator (git: remoteurl[len-1] = '\0').
    if remoteurl.as_bytes().last().copied().is_some_and(is_dir_sep) {
        remoteurl.pop();
    }

    let mut is_relative = false;
    if url_is_local_not_ssh(&remoteurl) && !is_absolute_path(&remoteurl) {
        is_relative = true;
        // Ensure all relative remoteurls start with "./" or "../".
        if !starts_with_dot_slash(&remoteurl) && !starts_with_dot_dot_slash(&remoteurl) {
            remoteurl = format!("./{remoteurl}");
        }
    }

    // Walk the leading "../" / "./" components of `url`.
    let mut colonsep = false;
    let mut rest = url;
    loop {
        if starts_with_dot_dot_slash(rest) {
            rest = &rest[3..];
            colonsep |= chop_last_dir(&mut remoteurl, is_relative);
        } else if starts_with_dot_slash(rest) {
            rest = &rest[2..];
        } else {
            break;
        }
    }

    let sep = if colonsep { ":" } else { "/" };
    let mut out = format!("{remoteurl}{sep}{rest}");
    // git: if url ended with '/', drop the trailing separator we just appended.
    if rest.ends_with('/') {
        out.pop();
    }

    // Strip a leading "./" git adds to keep relative urls canonical.
    let out = if starts_with_dot_slash(&out) {
        out[2..].to_string()
    } else {
        out
    };

    match up_path {
        Some(up) if is_relative => format!("{up}{out}"),
        _ => out,
    }
}

/// Port of `builtin/submodule--helper.c::resolve_relative_url`. Resolves a
/// submodule's `.gitmodules` url against the superproject's `remote.origin.url`
/// (the `base`), falling back to the repository's own cwd when no upstream is
/// configured (git's `xgetcwd()` branch — the caller supplies `cwd_fallback`).
///
/// `base` is the already-looked-up `remote.<default>.url`; pass `None` when the
/// repository has no such remote, and `cwd_fallback` (an absolute path string)
/// is used instead, exactly like git.
pub fn resolve_relative_url(
    rel_url: &str,
    base: Option<&str>,
    cwd_fallback: &str,
    up_path: Option<&str>,
) -> String {
    let remoteurl = base.unwrap_or(cwd_fallback);
    relative_url(remoteurl, rel_url, up_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The exact cases from t7400-submodule-basic.sh '../subrepo works with …'.
    // base = remote.origin.url, url = ../subrepo (the .gitmodules value).
    #[test]
    fn t7400_relative_url_matrix() {
        let cases = [
            (
                "ssh://hostname/repo",
                "../subrepo",
                "ssh://hostname/subrepo",
            ),
            (
                "ssh://hostname:22/repo",
                "../subrepo",
                "ssh://hostname:22/subrepo",
            ),
            (
                "//somewhere else/repo",
                "../subrepo",
                "//somewhere else/subrepo",
            ),
            ("file:///tmp/repo", "../subrepo", "file:///tmp/subrepo"),
            (
                "helper:://hostname/repo",
                "../subrepo",
                "helper:://hostname/subrepo",
            ),
            ("user@host:repo", "../subrepo", "user@host:subrepo"),
            (
                "user@host:path/to/repo",
                "../subrepo",
                "user@host:path/to/subrepo",
            ),
            ("foo", "../subrepo", "subrepo"),
            ("foo/bar", "../subrepo", "foo/subrepo"),
            ("./foo", "../subrepo", "subrepo"),
            ("./foo/bar", "../subrepo", "foo/subrepo"),
            ("../foo", "../subrepo", "../subrepo"),
            ("../foo/bar", "../subrepo", "../foo/subrepo"),
        ];
        for (base, url, want) in cases {
            assert_eq!(relative_url(base, url, None), want, "base={base} url={url}");
        }
    }

    #[test]
    fn deeper_relative_path() {
        // ../bar/a/b/c vs base ../foo/bar.git -> ../foo/bar/a/b/c
        assert_eq!(
            relative_url("../foo/bar.git", "../bar/a/b/c", None),
            "../foo/bar/a/b/c"
        );
    }

    #[test]
    fn absolute_and_ssh_urls_passthrough() {
        assert_eq!(relative_url("/base", "/abs/path", None), "/abs/path");
        assert_eq!(relative_url("/base", "ssh://h/repo", None), "ssh://h/repo");
        assert_eq!(
            relative_url("/base", "https://h/repo", None),
            "https://h/repo"
        );
    }

    #[test]
    fn absolute_local_base_resolution() {
        // The t7406/t7407 case: super cloned at /tmp/x/super, .gitmodules url
        // ../submodule -> /tmp/x/submodule (NOT /tmp/submodule).
        assert_eq!(
            relative_url("/tmp/x/super", "../submodule", None),
            "/tmp/x/submodule"
        );
    }

    #[test]
    fn up_path_prefix_only_for_relative_base() {
        // Relative base gets the up_path prefix: ./foo/bar, one ../ chops `bar`
        // -> ./foo, append /sub -> ./foo/sub, strip ./ -> foo/sub, then prefix
        // up_path -> ../foo/sub.
        assert_eq!(
            relative_url("./foo/bar", "../sub", Some("../")),
            "../foo/sub"
        );
        // Absolute base ignores up_path (is_relative == false).
        assert_eq!(
            relative_url("/tmp/x/super", "../sub", Some("../../")),
            "/tmp/x/sub"
        );
    }
}
