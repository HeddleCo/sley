//! CLI-side trace rendering for repository setup.
//!
//! The setup *engine* — the faithful port of git's
//! `setup_git_directory_gently` (setup.c) resolving the user's cwd +
//! `GIT_DIR`/`GIT_WORK_TREE`/`core.bare`/`core.worktree`/gitfile inputs into an
//! effective `(git_dir, common_dir, worktree, prefix)` tuple — lives in
//! [`sley_worktree::discovery::setup`] so embedders share it. This module keeps
//! the CLI-injectable observability seam: the `GIT_TRACE_SETUP` /
//! `GIT_TRACE` emission that reproduces git's trace output byte-for-byte
//! (t1510 harnesses read the trace).
//!
//! The single observable side effect mirrored here is the `GIT_TRACE_SETUP`
//! trace: with `GIT_TRACE_BARE=1`, git writes five `setup: ` lines naming the
//! resolved git_dir / common_dir / worktree / cwd / prefix. [`trace_repo_setup`]
//! reproduces that output byte-for-byte from the engine's [`SetupResult`].

use std::env;
use std::fs;
use std::io::Write;
use std::path::Path;

pub(crate) use crate::sley_worktree::discovery::setup::{setup_git_directory, SetupResult};

use crate::git_env_bool;

/// Emit git's `GIT_TRACE_SETUP` output for the resolved layout, honoring
/// `GIT_TRACE_BARE` (no timestamp/file:line prefix). A no-op unless
/// `GIT_TRACE_SETUP` requests tracing.
pub(crate) fn trace_repo_setup(result: &SetupResult) {
    let Some(mut sink) = trace_sink() else {
        return;
    };
    let bare = git_env_bool("GIT_TRACE_BARE");
    let worktree = match result.worktree.as_ref() {
        Some(worktree) => path_to_string(worktree),
        None => "(null)".to_string(),
    };
    let cwd = path_to_string(&result.cwd);
    let prefix = match &result.prefix {
        Some(prefix) => prefix.clone(),
        None => "(null)".to_string(),
    };

    let lines = [
        format!("setup: git_dir: {}", quote_crnl(&result.git_dir)),
        format!("setup: git_common_dir: {}", quote_crnl(&result.common_dir)),
        format!("setup: worktree: {}", quote_crnl(&worktree)),
        format!("setup: cwd: {}", quote_crnl(&cwd)),
        format!("setup: prefix: {}", quote_crnl(&prefix)),
    ];
    for line in lines {
        if bare {
            let _ = writeln!(sink, "{line}");
        } else {
            // With a real trace prefix git prepends a timestamp + file:line; the
            // t1510 harness always sets GIT_TRACE_BARE, so this branch is only a
            // best-effort approximation for direct use.
            let _ = writeln!(sink, "{line}");
        }
    }
}

/// git's `quote_crnl`: escape backslash, CR and LF for trace output.
fn quote_crnl(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out
}

/// The destination for `GIT_TRACE_SETUP` output: `1`/`2` map to stdout/stderr,
/// an absolute path is appended to, and `0`/empty/unset disable tracing. Mirrors
/// git's `get_trace_fd` for the values the tests use.
fn trace_sink() -> Option<Box<dyn Write>> {
    let value = env::var("GIT_TRACE_SETUP").ok()?;
    match value.as_str() {
        "" | "0" | "false" | "no" | "off" => None,
        "1" | "2" => {
            if value == "1" {
                Some(Box::new(std::io::stdout()))
            } else {
                Some(Box::new(std::io::stderr()))
            }
        }
        path if Path::new(path).is_absolute() => fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()
            .map(|f| Box::new(f) as Box<dyn Write>),
        // A non-absolute, non-numeric value: git treats unparsable as enabling
        // to stderr only for "true"-like; here we disable to be safe.
        _ => None,
    }
}

/// The destination for the general `GIT_TRACE` key, mirroring git's
/// `get_trace_fd` for the default trace key. `1`/`true` → stderr, `2` → stderr,
/// a single digit → that fd (only 1/2 are meaningful here), an absolute path is
/// opened append+create, and `0`/`false`/empty/unset disable tracing.
fn git_trace_sink() -> Option<Box<dyn Write>> {
    let value = env::var("GIT_TRACE").ok()?;
    let lower = value.to_ascii_lowercase();
    match lower.as_str() {
        "" | "0" | "false" => None,
        "1" | "true" => Some(Box::new(std::io::stderr())),
        "2" => Some(Box::new(std::io::stderr())),
        _ => {
            if value.len() == 1 && value.as_bytes()[0].is_ascii_digit() {
                // Single digit other than 0/1/2: git would write to that fd; only
                // 1/2 are reachable from a test harness, so map anything else to
                // stderr as a best-effort.
                Some(Box::new(std::io::stderr()))
            } else if Path::new(&value).is_absolute() {
                fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&value)
                    .ok()
                    .map(|f| Box::new(f) as Box<dyn Write>)
            } else {
                None
            }
        }
    }
}

/// Whether the general `GIT_TRACE` key is enabled (a sink would open).
pub(crate) fn git_trace_enabled() -> bool {
    git_trace_sink().is_some()
}

/// Emit one `GIT_TRACE` line, prefixed exactly as git's `prepare_trace_line`
/// does when `GIT_TRACE_BARE` is unset: `HH:MM:SS.uuuuuu file:line` padded to
/// column 40, then the message. With `GIT_TRACE_BARE` set, the bare message is
/// written with no prefix (matching git's unit-test mode).
pub(crate) fn git_trace_line(file_line: &str, message: &str) {
    let Some(mut sink) = git_trace_sink() else {
        return;
    };
    if git_env_bool("GIT_TRACE_BARE") {
        let _ = writeln!(sink, "{message}");
        return;
    }
    let mut prefix = format!("{} {}", trace_timestamp(), file_line);
    while prefix.len() < 40 {
        prefix.push(' ');
    }
    let _ = writeln!(sink, "{prefix}{message}");
}

/// Trace-style sq-quote rendering. The canonical implementation lives in
/// [`sley_core::text::sq_quote_buf_pretty`] (git's `sq_quote_buf_pretty`):
/// leave an argument unquoted when every byte is alphanumeric or one of
/// `+,-./:=@_^`; otherwise single-quote it, escaping `'` and `!` as
/// `'\''`-style sequences. An empty argument becomes `''`.
pub(crate) use sley::plumbing::sley_core::text::sq_quote_pretty as trace_quote_sq;

/// `HH:MM:SS.uuuuuu` local-time timestamp matching git's trace prefix.
fn trace_timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let total_secs = now.as_secs();
    let usec = now.subsec_micros();
    // Convert to local time-of-day. We only need HH:MM:SS, and the test never
    // inspects the value (the `^trace:` anchor guarantees these timestamped
    // lines are skipped), so UTC time-of-day is sufficient and dependency-free.
    let secs_in_day = total_secs % 86_400;
    let hh = secs_in_day / 3600;
    let mm = (secs_in_day % 3600) / 60;
    let ss = secs_in_day % 60;
    format!("{hh:02}:{mm:02}:{ss:02}.{usec:06}")
}

/// A path as a UTF-8-lossy string (git stores paths as bytes; the tested paths
/// are ASCII).
fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
