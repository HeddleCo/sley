//! Differential test: sley's ref-name validators against the real
//! `git check-ref-format` binary (HeddleCo/sley#244).
//!
//! Every name in the corpus is fed to the `git` on `PATH` (CI pins it to the
//! oracle version) under each flag combination, and sley's verdict must match.
//! The oracle is consulted live rather than through a checked-in expectation
//! file, so the corpus can grow without a regeneration step.

use std::process::Command;

/// One `git check-ref-format` flag combination.
#[derive(Clone, Copy, Debug)]
struct Mode {
    allow_onelevel: bool,
    refspec_pattern: bool,
}

const MODES: [Mode; 4] = [
    Mode {
        allow_onelevel: false,
        refspec_pattern: false,
    },
    Mode {
        allow_onelevel: true,
        refspec_pattern: false,
    },
    Mode {
        allow_onelevel: false,
        refspec_pattern: true,
    },
    Mode {
        allow_onelevel: true,
        refspec_pattern: true,
    },
];

/// Ask the real git whether `name` is a valid refname under `mode`.
///
/// Returns `None` when git's CLI cannot be asked: a name starting with `-` is
/// parsed as an option by `git check-ref-format`, so it never reaches
/// `check_refname_format`.
fn git_verdict(name: &str, mode: Mode) -> Option<bool> {
    if name.starts_with('-') {
        return None;
    }
    let mut command = Command::new("git");
    command.arg("check-ref-format");
    if mode.allow_onelevel {
        command.arg("--allow-onelevel");
    }
    if mode.refspec_pattern {
        command.arg("--refspec-pattern");
    }
    command.arg(name);
    let status = command.status().unwrap_or_else(|err| {
        panic!("failed to run `git check-ref-format` (is git on PATH?): {err}")
    });
    match status.code() {
        Some(0) => Some(true),
        Some(1) => Some(false),
        other => panic!("`git check-ref-format` {mode:?} {name:?} exited with {other:?}"),
    }
}

/// Unicode whitespace (per `char::is_whitespace`) plus a few look-alikes that
/// are not whitespace but are commonly confused with it. Git treats every
/// non-ASCII byte as an ordinary refname byte.
const UNICODE_SPACES: &[char] = &[
    '\u{0085}', // NEXT LINE
    '\u{00A0}', // NO-BREAK SPACE
    '\u{1680}', // OGHAM SPACE MARK
    '\u{2000}', // EN QUAD
    '\u{2002}', // EN SPACE
    '\u{2003}', // EM SPACE
    '\u{2009}', // THIN SPACE
    '\u{200A}', // HAIR SPACE
    '\u{200B}', // ZERO WIDTH SPACE (not whitespace)
    '\u{2028}', // LINE SEPARATOR
    '\u{2029}', // PARAGRAPH SEPARATOR
    '\u{202F}', // NARROW NO-BREAK SPACE
    '\u{205F}', // MEDIUM MATHEMATICAL SPACE
    '\u{3000}', // IDEOGRAPHIC SPACE
    '\u{FEFF}', // ZERO WIDTH NO-BREAK SPACE (not whitespace)
];

/// ASCII bytes that are forbidden or notable in refnames. NUL is excluded
/// because it cannot be passed through argv (see the unit tests instead).
const ASCII_SPECIALS: &[char] = &[
    ' ', '\t', '\n', '\r', '\u{1}', '\u{1b}', '\u{1f}', '\u{7f}', '~', '^', ':', '?', '*', '[',
    '\\', ']', '{', '}', '!', '"', '#', '$', '%', '&', '\'', '(', ')', '+', ',', ';', '<', '=',
    '>', '|', '`', '@', '-', '_', '.',
];

fn corpus() -> Vec<String> {
    let mut names: Vec<String> = Vec::new();

    // The issue's motivating names.
    names.push("refs/heads/\u{00A0}edge\u{00A0}".into());
    names.push("refs/heads/ edge".into());

    // Unicode whitespace at the edges of the whole name, the edges of a
    // component, in the middle, and as a whole component.
    for &ws in UNICODE_SPACES {
        names.push(format!("refs/heads/{ws}edge"));
        names.push(format!("refs/heads/edge{ws}"));
        names.push(format!("refs/heads/{ws}edge{ws}"));
        names.push(format!("refs/heads/mid{ws}dle"));
        names.push(format!("{ws}refs/heads/main"));
        names.push(format!("refs/{ws}/main"));
        names.push(format!("{ws}"));
    }

    // ASCII specials at the edges and in the middle of a component.
    for &ch in ASCII_SPECIALS {
        names.push(format!("refs/heads/{ch}x"));
        names.push(format!("refs/heads/x{ch}"));
        names.push(format!("refs/heads/a{ch}b"));
        names.push(format!("{ch}"));
    }

    // Structural rules.
    for name in [
        "",
        "a",
        "HEAD",
        "FETCH_HEAD",
        "refs",
        "refs/heads/main",
        "refs/heads/feature/x",
        "@",
        "@@",
        "refs/heads/@",
        "refs/@/x",
        "@/x",
        "x/@",
        "a@b",
        "refs/heads/a@{b",
        "refs/heads/@{",
        "refs/heads/@{-1}",
        "refs/heads/a@{",
        "refs/heads/a{b",
        "refs/heads/a@ {b",
        "..",
        ".",
        "refs/heads/..",
        "refs/heads/.",
        "refs/heads/a..b",
        "refs/heads/a.b",
        "refs/heads/a...b",
        "refs/heads/a./b",
        "refs/heads/a/.b",
        "refs/heads/.a",
        "refs/heads/a.",
        "refs/heads/a/",
        "refs/heads/a/.",
        "refs/heads/a.lock",
        "refs/heads/a.lock/b",
        "refs/heads/a.lockx",
        "refs/heads/.lock",
        "refs/heads/a.LOCK",
        "refs/heads/a.lock.lock",
        "refs/heads/lock",
        "refs/heads/a/b.lock",
        "a.lock",
        "refs/heads//a",
        "refs//heads/a",
        "//refs/heads/a",
        "/refs/heads/a",
        "refs/heads/a//",
        "refs/heads/-",
        "refs/heads/-foo",
        "refs/heads/--foo",
        "-foo",
        "-",
        "refs/heads/foo-",
        "refs/heads/*",
        "refs/*/main",
        "refs/heads/a*",
        "refs/heads/*a*",
        "refs/*/*",
        "*",
        "*/x",
        "refs/heads/a*b",
        "refs/heads/.*",
        "refs/heads/*.lock",
        "refs/heads/HEAD",
        "refs/tags/v1.0",
        "refs/tags/v1.0^{}",
        "refs/remotes/origin/HEAD",
    ] {
        names.push(name.into());
    }

    // Non-ASCII that is not whitespace.
    for name in [
        "refs/heads/é",
        "refs/heads/caf\u{0065}\u{0301}",
        "refs/heads/日本語",
        "refs/heads/🦀",
        "refs/heads/\u{200F}rtl",
        "refs/heads/\u{00AD}soft-hyphen",
        "refs/heads/\u{FF0E}\u{FF0E}fullwidth-dots",
        "refs/heads/\u{2024}one-dot-leader",
        "refs/heads/x\u{2215}division-slash",
        "\u{00E9}",
        "refs/heads/\u{FFFD}",
    ] {
        names.push(name.into());
    }

    // Long names.
    names.push(format!("refs/heads/{}", "a".repeat(1000)));
    names.push(format!("refs/heads/{}", "a/".repeat(200) + "b"));
    names.push(format!("refs/heads/{}\u{00A0}", "n".repeat(4000)));
    names.push(format!("refs/heads/{}.lock", "a".repeat(500)));

    names
}

/// Debug-format `name`, eliding the middle of very long names.
fn abbreviate(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= 80 {
        return format!("{name:?}");
    }
    let head: String = chars[..40].iter().collect();
    let tail: String = chars[chars.len() - 20..].iter().collect();
    format!("{head:?}..{tail:?} ({} chars)", chars.len())
}

/// Run `check` over the corpus under each of `modes`, comparing against git.
fn assert_matches_git(label: &str, modes: &[Mode], check: impl Fn(&str, Mode) -> bool) {
    let names = corpus();
    assert!(names.len() >= 200, "corpus has only {} names", names.len());
    let mut compared = 0usize;
    let mut mismatches = Vec::new();
    for name in &names {
        for &mode in modes {
            let Some(git) = git_verdict(name, mode) else {
                continue;
            };
            compared += 1;
            let sley = check(name, mode);
            if sley != git {
                mismatches.push(format!(
                    "  {} {mode:?}: git={} sley={}",
                    abbreviate(name),
                    if git { "valid" } else { "invalid" },
                    if sley { "valid" } else { "invalid" },
                ));
            }
        }
    }
    assert!(
        mismatches.is_empty(),
        "{label}: {} of {compared} verdicts differ from git check-ref-format:\n{}",
        mismatches.len(),
        mismatches.join("\n"),
    );
    eprintln!(
        "{label}: {compared} verdicts over {} names match git",
        names.len()
    );
}

#[test]
fn sley_core_check_refname_format_matches_git() {
    assert_matches_git("sley_core::check_refname_format", &MODES, |name, mode| {
        let format = sley_core::RefnameFormat {
            allow_onelevel: mode.allow_onelevel,
            refspec_pattern: mode.refspec_pattern,
        };
        sley_core::check_refname_format(name.as_bytes(), format).is_ok()
    });
}

#[test]
fn full_name_matches_git_allow_onelevel() {
    // `FullName` holds `HEAD` and pseudo-refs as well as `refs/...`, so its
    // policy is `check_refname_format(_, REFNAME_ALLOW_ONELEVEL)`.
    assert_matches_git(
        "FullName::new",
        &[Mode {
            allow_onelevel: true,
            refspec_pattern: false,
        }],
        |name, _| sley_core::FullName::new(name).is_ok(),
    );
}

#[test]
fn sley_refs_check_refname_format_matches_git() {
    assert_matches_git(
        "sley_refs::check_refname_format",
        &MODES[..2],
        |name, mode| sley_refs::check_refname_format(name, mode.allow_onelevel).is_ok(),
    );
}

#[test]
fn issue_244_nbsp_edges_are_valid() {
    let name = "refs/heads/\u{00A0}edge\u{00A0}";
    assert_eq!(git_verdict(name, MODES[0]), Some(true));
    assert!(sley_core::FullName::new(name).is_ok());
    assert!(sley_refs::check_refname_format(name, false).is_ok());

    let name = "refs/heads/ edge";
    assert_eq!(git_verdict(name, MODES[0]), Some(false));
    assert!(sley_core::FullName::new(name).is_err());
    assert!(sley_refs::check_refname_format(name, false).is_err());
}
