// `SubmoduleIgnoreMode`, `DiffStatWidths`, and the dirstat option types moved
// to `sley-diff-merge::porcelain` (the diff render engine owns them); these
// re-exports keep the historical `sley_rev::diff_options` spellings working.
pub use sley_diff_merge::porcelain::{
    DiffStatWidths, DirstatMode, DirstatOptions, SubmoduleIgnoreMode, parse_submodule_ignore_mode,
};
#[derive(Debug, Clone, Default)]
pub struct DiffFilter {
    pub includes: HashSet<char>,
    pub excludes: HashSet<char>,
    pub all_or_none: bool,
}
impl DiffFilter {
    pub fn matches_status(&self, status: char) -> bool {
        (self.includes.is_empty() || self.includes.contains(&status))
            && !self.excludes.contains(&status)
    }
}
pub fn parse_diff_filter(value: &str) -> Result<DiffFilter> {
    let mut f = DiffFilter::default();
    for ch in value.chars() {
        match ch {
            'A' | 'C' | 'D' | 'M' | 'R' | 'T' | 'U' | 'X' | 'B' => {
                f.includes.insert(ch);
            }
            'a' | 'c' | 'd' | 'm' | 'r' | 't' | 'u' | 'x' | 'b' => {
                f.excludes.insert(ch.to_ascii_uppercase());
            }
            '*' => f.all_or_none = true,
            other => {
                sley_core::diagnostic!(
                    Stderr,
                    true,
                    "error: unknown change class '{other}' in --diff-filter={value}"
                );
                return Err(GitError::Rejected(
                    sley_core::RejectionKind::InvalidArguments,
                ));
            }
        }
    }
    Ok(f)
}
pub fn diff_stat_parse_width_option(value: &str, widths: &mut DiffStatWidths) -> Result<bool> {
    fn n(o: &str, v: &str) -> Result<i64> {
        v.parse()
            .map_err(|_| GitError::Command(format!("{o} expects a numerical value")))
    }
    if let Some(s) = value.strip_prefix("--stat=") {
        let mut p = s.split(',');
        if let Some(w) = p.next()
            && !w.is_empty()
        {
            widths.stat_width = n("--stat", w)?;
        }
        if let Some(w) = p.next()
            && !w.is_empty()
        {
            widths.name_width = n("--stat", w)?;
        }
        Ok(true)
    } else if let Some(w) = value.strip_prefix("--stat-width=") {
        widths.stat_width = n("--stat-width", w)?;
        Ok(true)
    } else if let Some(w) = value.strip_prefix("--stat-name-width=") {
        widths.name_width = n("--stat-name-width", w)?;
        Ok(true)
    } else if let Some(w) = value.strip_prefix("--stat-graph-width=") {
        widths.graph_width = n("--stat-graph-width", w)?;
        Ok(true)
    } else {
        Ok(value == "--stat" || value.starts_with("--stat-count="))
    }
}
pub fn diff_stat_count_option(value: &str) -> Result<Option<Option<usize>>> {
    let c = value.strip_prefix("--stat-count=").or_else(|| {
        value
            .strip_prefix("--stat=")
            .and_then(|s| s.split(',').nth(2))
    });
    let Some(c) = c else { return Ok(None) };
    let c = c
        .parse::<usize>()
        .map_err(|_| GitError::Command(format!("invalid stat count {c}")))?;
    Ok(Some((c != 0).then_some(c)))
}
pub fn parse_similarity_threshold(spec: &str) -> u8 {
    let s = spec.strip_suffix('%').unwrap_or(spec);
    match s.parse::<f64>() {
        Ok(v) => {
            let p = if v <= 1.0 && s.contains('.') {
                v * 100.0
            } else {
                v
            };
            p.round().clamp(0.0, 100.0) as u8
        }
        Err(_) => sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
    }
}
pub fn parse_diff_rename_limit(value: &str) -> usize {
    let v = value.trim();
    if v.starts_with('-') {
        return 0;
    }
    let v = v.strip_prefix('+').unwrap_or(v);
    let (d, m) = match v.as_bytes().last().copied() {
        Some(b'k') => (&v[..v.len() - 1], 1024usize),
        Some(b'm') => (&v[..v.len() - 1], 1024 * 1024),
        Some(b'g') => (&v[..v.len() - 1], 1024 * 1024 * 1024),
        _ => (v, 1),
    };
    d.parse()
        .ok()
        .and_then(|l: usize| l.checked_mul(m))
        .unwrap_or(usize::MAX)
}
fn validate_diff_rename_limit(value: &str) -> Result<()> {
    // git's `-l<n>` is OPT_INTEGER: a leading sign is accepted (a negative or
    // zero limit means "unlimited"). Strip an optional sign, then an optional
    // k/m/g magnitude suffix — as two separate steps: a single chained
    // `.strip_suffix(..).unwrap_or(value)` would fall back to the ORIGINAL
    // signed value, re-introducing the sign and failing the digit check
    // (the `-l-1` regression).
    let unsigned = value
        .strip_prefix('+')
        .or_else(|| value.strip_prefix('-'))
        .filter(|rest| !rest.is_empty())
        .unwrap_or(value);
    let digits = unsigned
        .strip_suffix('k')
        .or_else(|| unsigned.strip_suffix('m'))
        .or_else(|| unsigned.strip_suffix('g'))
        .unwrap_or(unsigned);
    if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
        Ok(())
    } else {
        sley_core::diagnostic!(
            Stderr,
            true,
            "error: switch `l' expects an integer value with an optional k/m/g suffix"
        );
        Err(GitError::Rejected(
            sley_core::RejectionKind::InvalidArguments,
        ))
    }
}
fn parse_abbrev(value: &str) -> Result<usize> {
    value
        .parse()
        .map_err(|_| GitError::Command(format!("invalid abbrev length {value}")))
}
fn git_count_value_is_valid(value: &str) -> bool {
    let n = match value.as_bytes().last() {
        Some(b'k' | b'K' | b'm' | b'M' | b'g' | b'G') => &value[..value.len() - 1],
        _ => value,
    };
    let d = match n.as_bytes().first() {
        Some(b'+' | b'-') if n.len() > 1 => &n[1..],
        _ => n,
    };
    !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit())
}
fn commit_validate_unified_context(value: &str, short: bool) -> Result<()> {
    if value.is_empty() {
        sley_core::diagnostic!(
            Stderr,
            true,
            "error: {} expects a numerical value",
            if short {
                "switch `U'"
            } else {
                "option `unified'"
            }
        );
        return Err(GitError::Rejected(
            sley_core::RejectionKind::InvalidArguments,
        ));
    }
    if value.starts_with('-') {
        sley_core::diagnostic!(
            Stderr,
            true,
            "error: --unified expects a non-negative integer"
        );
        return Err(GitError::Rejected(
            sley_core::RejectionKind::InvalidArguments,
        ));
    }
    if git_count_value_is_valid(value) {
        return Ok(());
    }
    sley_core::diagnostic!(
        Stderr,
        true,
        "error: {} expects an integer value with an optional k/m/g suffix",
        if short {
            "switch `U'"
        } else {
            "option `unified'"
        }
    );
    Err(GitError::Rejected(
        sley_core::RejectionKind::InvalidArguments,
    ))
}
fn log_validate_diff_algorithm(value: &str) -> Result<()> {
    match value {
        "myers" | "minimal" | "patience" | "histogram" | "default" => Ok(()),
        _ => {
            sley_core::diagnostic!(
                Stderr,
                true,
                "error: option diff-algorithm accepts \"myers\", \"minimal\", \"patience\" and \"histogram\""
            );
            Err(GitError::Rejected(
                sley_core::RejectionKind::InvalidArguments,
            ))
        }
    }
}
fn log_inter_hunk_context_requires_number_error() -> Result<()> {
    sley_core::diagnostic!(
        Stderr,
        true,
        "error: option `inter-hunk-context' expects a numerical value"
    );
    Err(GitError::Rejected(
        sley_core::RejectionKind::InvalidArguments,
    ))
}
fn log_validate_inter_hunk_context(value: &str) -> Result<()> {
    let n = match value.as_bytes().last() {
        Some(b'k' | b'K' | b'm' | b'M' | b'g' | b'G') => &value[..value.len() - 1],
        _ => value,
    };
    let d = match n.as_bytes().first() {
        Some(b'+') if n.len() > 1 => &n[1..],
        _ => n,
    };
    if !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(());
    }
    sley_core::diagnostic!(
        Stderr,
        true,
        "error: option `inter-hunk-context' expects a non-negative integer value with an optional k/m/g suffix"
    );
    Err(GitError::Rejected(
        sley_core::RejectionKind::InvalidArguments,
    ))
}
fn log_validate_output_indicator(option: &str, value: &str) -> Result<()> {
    // Single-byte indicator always accepted; empty value accepted only by BSD
    // libc (macOS/*BSD), rejected by glibc (Linux/CI). Match the platform's git.
    if value.len() == 1
        || (value.is_empty()
            && cfg!(any(
                target_vendor = "apple",
                target_os = "freebsd",
                target_os = "openbsd",
                target_os = "netbsd",
                target_os = "dragonfly"
            )))
    {
        return Ok(());
    }
    sley_core::diagnostic!(
        Stderr,
        true,
        "error: {option} expects a character, got '{}'",
        if value.is_empty() { "''" } else { value }
    );
    Err(GitError::Rejected(
        sley_core::RejectionKind::InvalidArguments,
    ))
}
fn log_validate_submodule_format(value: &str) -> Result<()> {
    match value {
        "short" | "log" | "diff" => Ok(()),
        _ => {
            sley_core::diagnostic!(
                Stderr,
                true,
                "error: failed to parse --submodule option parameter: '{value}'"
            );
            Err(GitError::Rejected(
                sley_core::RejectionKind::InvalidArguments,
            ))
        }
    }
}
fn log_validate_color_moved(value: &str) -> Result<()> {
    match value {
        "" | "no" | "default" | "blocks" | "zebra" | "dimmed-zebra" | "plain" | "true" | "1"
        | "on" | "yes" | "false" | "0" | "off" => Ok(()),
        _ => {
            sley_core::diagnostic!(
                Stderr,
                true,
                "error: color moved setting must be one of 'no', 'default', 'blocks', 'zebra', 'dimmed-zebra', 'plain'"
            );
            sley_core::diagnostic!(Stderr, true, "error: bad --color-moved argument: {value}");
            Err(GitError::Rejected(
                sley_core::RejectionKind::InvalidArguments,
            ))
        }
    }
}
fn log_validate_color(value: &str) -> Result<()> {
    match value {
        "always" | "auto" | "never" => Ok(()),
        _ => {
            sley_core::diagnostic!(
                Stderr,
                true,
                "error: option `color' expects \"always\", \"auto\", or \"never\""
            );
            Err(GitError::Rejected(
                sley_core::RejectionKind::InvalidArguments,
            ))
        }
    }
}
fn log_validate_color_moved_ws(value: &str) -> Result<()> {
    let mut a = false;
    let mut n = 0;
    for m in value.split(',') {
        n += 1;
        match m {
            "no" | "ignore-space-change" | "ignore-space-at-eol" | "ignore-all-space" => {}
            "allow-indentation-change" => a = true,
            _ => return log_color_moved_ws_invalid_mode(value, m),
        }
    }
    if a && n > 1 {
        sley_core::diagnostic!(
            Stderr,
            true,
            "error: color-moved-ws: allow-indentation-change cannot be combined with other whitespace modes"
        );
        sley_core::diagnostic!(
            Stderr,
            true,
            "error: invalid mode '{value}' in --color-moved-ws"
        );
        return Err(GitError::Rejected(
            sley_core::RejectionKind::InvalidArguments,
        ));
    }
    Ok(())
}
fn log_color_moved_ws_invalid_mode(value: &str, mode: &str) -> Result<()> {
    sley_core::diagnostic!(
        Stderr,
        true,
        "error: unknown color-moved-ws mode '{mode}', possible values are 'ignore-space-change', 'ignore-space-at-eol', 'ignore-all-space', 'allow-indentation-change'"
    );
    sley_core::diagnostic!(
        Stderr,
        true,
        "error: invalid mode '{value}' in --color-moved-ws"
    );
    Err(GitError::Rejected(
        sley_core::RejectionKind::InvalidArguments,
    ))
}
fn log_validate_ws_error_highlight(value: &str) -> Result<()> {
    if value.is_empty() {
        return Ok(());
    }
    let mut p = String::new();
    for m in value.split(',') {
        match m {
            "old" | "new" | "context" | "all" | "none" | "default" => {
                p.push_str(m);
                p.push(',');
            }
            _ => {
                sley_core::diagnostic!(
                    Stderr,
                    true,
                    "error: unknown value after ws-error-highlight={p}"
                );
                return Err(GitError::Rejected(
                    sley_core::RejectionKind::InvalidArguments,
                ));
            }
        }
    }
    Ok(())
}
fn log_validate_similarity_option(value: &str, option: &str) -> Result<()> {
    if value.is_empty() {
        return Ok(());
    }
    let d = value.strip_suffix('%').unwrap_or(value);
    if !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(());
    }
    sley_core::diagnostic!(Stderr, true, "error: invalid argument to {option}");
    Err(GitError::Rejected(
        sley_core::RejectionKind::InvalidArguments,
    ))
}
fn log_valid_break_rewrites_part(value: &str) -> bool {
    if value.is_empty() {
        return true;
    }
    let d = value.strip_suffix('%').unwrap_or(value);
    !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit())
}
fn log_break_rewrites_form_error() -> Result<()> {
    sley_core::diagnostic!(Stderr, true, "error: break-rewrites expects <n>/<m> form");
    Err(GitError::Rejected(
        sley_core::RejectionKind::InvalidArguments,
    ))
}
fn log_validate_break_rewrites_option(value: &str) -> Result<()> {
    if value.is_empty() {
        return Ok(());
    }
    let mut p = value.split('/');
    let f = p.next().unwrap_or_default();
    let s = p.next();
    if p.next().is_some() {
        return log_break_rewrites_form_error();
    }
    if log_valid_break_rewrites_part(f) && s.is_none_or(log_valid_break_rewrites_part) {
        return Ok(());
    }
    log_break_rewrites_form_error()
}
