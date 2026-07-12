//! Credential URL parsing, matching, and config collection.

use std::collections::BTreeMap;

use sley_config::{ConfigStack, GitConfig};
use sley_core::{GitError, Result};

use super::{GitCredential, TIME_MAX};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct UrlMatchScore {
    host_len: usize,
    path_len: usize,
    user_matched: bool,
}

pub(crate) fn credential_match(
    want: &GitCredential,
    have: &GitCredential,
    match_password: bool,
) -> bool {
    field_matches(want.protocol.as_deref(), have.protocol.as_deref())
        && field_matches(want.host.as_deref(), have.host.as_deref())
        && field_matches(want.path.as_deref(), have.path.as_deref())
        && field_matches(want.username.as_deref(), have.username.as_deref())
        && (!match_password
            || (field_matches(want.password.as_deref(), have.password.as_deref())
                && field_matches(want.credential.as_deref(), have.credential.as_deref())))
}

fn field_matches(want: Option<&str>, have: Option<&str>) -> bool {
    want.is_none() || have.is_some_and(|value| want == Some(value))
}

pub(crate) fn credential_describe(credential: &GitCredential) -> String {
    let Some(protocol) = credential.protocol.as_deref() else {
        return String::new();
    };
    let mut out = format!("{protocol}://");
    if let Some(username) = credential.username.as_deref()
        && !username.is_empty()
    {
        out.push_str(username);
        out.push('@');
    }
    if let Some(host) = credential.host.as_deref() {
        out.push_str(host);
    }
    if let Some(path) = credential.path.as_deref() {
        out.push('/');
        out.push_str(path);
    }
    out
}

pub(crate) fn credential_format(credential: &GitCredential) -> String {
    let Some(protocol) = credential.protocol.as_deref() else {
        return String::new();
    };
    let mut out = format!("{protocol}://");
    if let Some(username) = credential.username.as_deref()
        && !username.is_empty()
    {
        percent_encode(username, EncodeMode::Slash, &mut out);
        out.push('@');
    }
    if let Some(host) = credential.host.as_deref() {
        percent_encode(host, EncodeMode::HostAndPort, &mut out);
    }
    if let Some(path) = credential.path.as_deref() {
        out.push('/');
        percent_encode(path, EncodeMode::Path, &mut out);
    }
    out
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum EncodeMode {
    Slash,
    HostAndPort,
    Path,
    Unreserved,
    StorePath,
}

pub(crate) fn percent_encode(input: &str, mode: EncodeMode, out: &mut String) {
    for ch in input.chars() {
        let encode = match mode {
            EncodeMode::Slash => ch == '/' || !is_rfc3986_reserved_or_unreserved(ch),
            EncodeMode::HostAndPort => {
                matches!(ch, '/' | '?' | '#' | '[' | ']' | '@')
                    || !is_rfc3986_reserved_or_unreserved(ch)
            }
            EncodeMode::Path | EncodeMode::StorePath => false,
            EncodeMode::Unreserved => !is_rfc3986_unreserved(ch),
        };
        if encode
            || matches!(mode, EncodeMode::Unreserved)
            || (matches!(mode, EncodeMode::Path | EncodeMode::StorePath)
                && !is_rfc3986_reserved_or_unreserved(ch))
        {
            for byte in ch.to_string().as_bytes() {
                out.push('%');
                out.push(hex_digit(byte >> 4));
                out.push(hex_digit(byte & 0xf));
            }
        } else {
            out.push(ch);
        }
    }
}

fn hex_digit(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        10..=15 => (b'A' + nibble - 10) as char,
        _ => unreachable!(),
    }
}

fn is_rfc3986_unreserved(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '-' | '.' | '_' | '~')
}

pub(crate) fn is_rfc3986_reserved_or_unreserved(ch: char) -> bool {
    is_rfc3986_unreserved(ch)
        || matches!(
            ch,
            '!' | '*'
                | '\''
                | '('
                | ')'
                | ';'
                | ':'
                | '@'
                | '&'
                | '='
                | '+'
                | '$'
                | ','
                | '/'
                | '?'
                | '#'
                | '['
                | ']'
        )
}

fn url_decode_mem(input: &str, len: usize) -> String {
    url_decode(&input[..len.min(input.len())])
}

fn url_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

pub(crate) fn credential_from_url_gently(
    url: &str,
    allow_partial_url: bool,
    quiet: bool,
) -> Result<GitCredential> {
    let proto_end = url.find("://");
    if !allow_partial_url && (proto_end.is_none() || proto_end == Some(0)) {
        if !quiet {
            eprintln!("warning: url has no scheme: {url}");
        }
        return Err(GitError::InvalidFormat("url has no scheme".into()));
    }
    let cp = proto_end.map(|idx| idx + 3).unwrap_or(0);
    let at = url[cp..].find('@').map(|idx| cp + idx);
    let colon = url[cp..].find(':').map(|idx| cp + idx);
    let slash = cp + url[cp..].find(['/', '?', '#']).unwrap_or(url[cp..].len());

    let (username, password, host_start) = if let Some(at) = at
        && slash > at
    {
        if let Some(colon) = colon
            && colon < at
        {
            (
                Some(url_decode_mem(&url[cp..], colon - cp)),
                Some(url_decode_mem(&url[colon + 1..], at - (colon + 1))),
                at + 1,
            )
        } else {
            (Some(url_decode_mem(&url[cp..], at - cp)), None, at + 1)
        }
    } else {
        (None, None, cp)
    };

    let mut credential = GitCredential::default();
    if let Some(proto_end) = proto_end
        && proto_end > 0
    {
        credential.protocol = Some(url[..proto_end].to_string());
    }
    if !allow_partial_url || slash > host_start {
        credential.host = Some(url_decode_mem(&url[host_start..], slash - host_start));
    }
    let mut path_start = slash;
    while path_start < url.len() && url.as_bytes()[path_start] == b'/' {
        path_start += 1;
    }
    if path_start < url.len() {
        let mut path = url_decode(&url[path_start..]);
        while path.ends_with('/') && path.len() > 1 {
            path.pop();
        }
        credential.path = Some(path);
    }
    if let Some(username) = username.filter(|value| !value.is_empty()) {
        credential.username = Some(username);
        credential.username_from_proto = true;
    }
    credential.password = password.filter(|value| !value.is_empty());
    for (name, value) in [
        ("username", credential.username.as_deref()),
        ("password", credential.password.as_deref()),
        ("protocol", credential.protocol.as_deref()),
        ("host", credential.host.as_deref()),
        ("path", credential.path.as_deref()),
    ] {
        if value.is_some_and(|value| value.contains('\n')) {
            if !quiet {
                eprintln!("warning: url contains a newline in its {name} component: {url}");
            }
            return Err(GitError::InvalidFormat(format!(
                "url contains a newline in its {name} component"
            )));
        }
    }
    Ok(credential)
}

pub(crate) fn credential_from_potentially_partial_url(url: &str) -> Result<GitCredential> {
    credential_from_url_gently(url, true, false)
}

pub(crate) fn credential_from_url(credential: &mut GitCredential, url: &str) -> Result<()> {
    match credential_from_url_gently(url, false, false) {
        Ok(parsed) => {
            *credential = parsed;
            Ok(())
        }
        Err(_) => Err(GitError::InvalidFormat(format!(
            "fatal: credential url cannot be parsed: {url}"
        ))),
    }
}

pub(crate) fn collect_credential_config_stack(
    stack: &ConfigStack,
    credential: &mut GitCredential,
) -> Result<()> {
    let url = credential_format(credential);
    if url.is_empty() {
        return Ok(());
    }
    let mut best_helpers: BTreeMap<String, (UrlMatchScore, String)> = BTreeMap::new();
    for entry in &stack.entries {
        if !entry.section.eq_ignore_ascii_case("credential") {
            continue;
        }
        let Some(partial) = entry.subsection.as_deref() else {
            apply_global_credential_stack_entry(credential, entry)?;
            continue;
        };
        let matched = urlmatch_score(partial, &url).is_some()
            || credential_from_potentially_partial_url(partial)
                .ok()
                .is_some_and(|want| credential_match(&want, credential, false));
        if !matched {
            if credential_from_potentially_partial_url(partial).is_err() {
                eprintln!("warning: skipping credential lookup for key: credential.{partial}");
            }
            continue;
        }
        let score = urlmatch_score(partial, &url).unwrap_or_default();
        if entry.key.eq_ignore_ascii_case("helper") {
            let value = entry.value.as_deref().unwrap_or("");
            let slot = best_helpers
                .entry(partial.to_string())
                .or_insert((score, String::new()));
            if score > slot.0 {
                *slot = (score, String::new());
            }
            if value.is_empty() {
                slot.1.clear();
            } else if !slot.1.is_empty() {
                slot.1.push(' ');
            }
            slot.1.push_str(value);
        } else {
            apply_subsection_credential_stack_entry(credential, entry)?;
        }
    }
    apply_sorted_helpers(&mut best_helpers, credential);
    Ok(())
}

pub(crate) fn collect_credential_config(
    config: &GitConfig,
    credential: &mut GitCredential,
) -> Result<()> {
    let url = credential_format(credential);
    if url.is_empty() {
        return Ok(());
    }
    let mut best_helpers: BTreeMap<String, (UrlMatchScore, String)> = BTreeMap::new();
    for section in &config.sections {
        if !section.name.eq_ignore_ascii_case("credential") {
            continue;
        }
        let Some(partial) = section.subsection.as_deref() else {
            for entry in &section.entries {
                apply_global_credential_entry(credential, entry)?;
            }
            continue;
        };
        let matched = urlmatch_score(partial, &url).is_some()
            || credential_from_potentially_partial_url(partial)
                .ok()
                .is_some_and(|want| credential_match(&want, credential, false));
        if !matched {
            if credential_from_potentially_partial_url(partial).is_err() {
                eprintln!("warning: skipping credential lookup for key: credential.{partial}");
            }
            continue;
        }
        let score = urlmatch_score(partial, &url).unwrap_or_default();
        for entry in &section.entries {
            if entry.key.eq_ignore_ascii_case("helper") {
                let value = entry.value.as_deref().unwrap_or("");
                let slot = best_helpers
                    .entry(partial.to_string())
                    .or_insert((score, String::new()));
                if score > slot.0 {
                    *slot = (score, String::new());
                }
                if value.is_empty() {
                    slot.1.clear();
                } else if !slot.1.is_empty() {
                    slot.1.push(' ');
                }
                slot.1.push_str(value);
            } else {
                apply_subsection_credential_entry(credential, entry)?;
            }
        }
    }
    apply_sorted_helpers(&mut best_helpers, credential);
    Ok(())
}

fn apply_sorted_helpers(
    best_helpers: &mut BTreeMap<String, (UrlMatchScore, String)>,
    credential: &mut GitCredential,
) {
    let mut helpers: Vec<(UrlMatchScore, String, String)> = std::mem::take(best_helpers)
        .into_iter()
        .map(|(key, (score, value))| (score, key, value))
        .collect();
    helpers.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    for (_, _, helper) in helpers {
        if helper.is_empty() {
            credential.helpers.clear();
        } else {
            credential.helpers.push(helper);
        }
    }
}

fn apply_global_credential_entry(
    credential: &mut GitCredential,
    entry: &sley_config::ConfigEntry,
) -> Result<()> {
    if entry.key.eq_ignore_ascii_case("helper") {
        match entry.value.as_deref() {
            Some("") | None => credential.helpers.clear(),
            Some(value) => credential.helpers.push(value.to_string()),
        }
    } else if entry.key.eq_ignore_ascii_case("username") {
        if !credential.username_from_proto {
            credential.username = entry.value.clone();
        }
    } else if entry.key.eq_ignore_ascii_case("usehttppath") {
        credential.use_http_path = parse_bool(entry.value.as_deref());
    } else if entry.key.eq_ignore_ascii_case("sanitizeprompt") {
        credential.sanitize_prompt = parse_bool(entry.value.as_deref());
    } else if entry.key.eq_ignore_ascii_case("protectprotocol") {
        credential.protect_protocol = parse_bool(entry.value.as_deref());
    }
    Ok(())
}

fn apply_subsection_credential_entry(
    credential: &mut GitCredential,
    entry: &sley_config::ConfigEntry,
) -> Result<()> {
    if entry.key.eq_ignore_ascii_case("username") && !credential.username_from_proto {
        credential.username = entry.value.clone();
    } else if entry.key.eq_ignore_ascii_case("usehttppath") {
        credential.use_http_path = parse_bool(entry.value.as_deref());
    }
    Ok(())
}

fn apply_global_credential_stack_entry(
    credential: &mut GitCredential,
    entry: &sley_config::ConfigStackEntry,
) -> Result<()> {
    if entry.key.eq_ignore_ascii_case("helper") {
        match entry.value.as_deref() {
            Some("") | None => credential.helpers.clear(),
            Some(value) => credential.helpers.push(value.to_string()),
        }
    } else if entry.key.eq_ignore_ascii_case("username") {
        if !credential.username_from_proto {
            credential.username = entry.value.clone();
        }
    } else if entry.key.eq_ignore_ascii_case("usehttppath") {
        credential.use_http_path = parse_bool(entry.value.as_deref());
    } else if entry.key.eq_ignore_ascii_case("sanitizeprompt") {
        credential.sanitize_prompt = parse_bool(entry.value.as_deref());
    } else if entry.key.eq_ignore_ascii_case("protectprotocol") {
        credential.protect_protocol = parse_bool(entry.value.as_deref());
    }
    Ok(())
}

fn apply_subsection_credential_stack_entry(
    credential: &mut GitCredential,
    entry: &sley_config::ConfigStackEntry,
) -> Result<()> {
    if entry.key.eq_ignore_ascii_case("username") && !credential.username_from_proto {
        credential.username = entry.value.clone();
    } else if entry.key.eq_ignore_ascii_case("usehttppath") {
        credential.use_http_path = parse_bool(entry.value.as_deref());
    }
    Ok(())
}

fn parse_bool(value: Option<&str>) -> bool {
    match value {
        None => true,
        Some(value) => sley_config::parse_config_bool(value).unwrap_or(!value.is_empty()),
    }
}

fn urlmatch_score(partial: &str, url: &str) -> Option<UrlMatchScore> {
    if !partial.contains("://") {
        return url.starts_with(partial).then_some(UrlMatchScore {
            host_len: 0,
            path_len: partial.len(),
            user_matched: false,
        });
    }
    let base = credential_from_potentially_partial_url(partial).ok()?;
    let target = credential_from_potentially_partial_url(url).ok()?;
    if base.protocol != target.protocol {
        return None;
    }
    if let (Some(base_host), Some(target_host)) = (base.host.as_deref(), target.host.as_deref()) {
        if !host_matches(target_host, base_host) {
            return None;
        }
    } else if base.host.is_some() {
        return None;
    }
    let path_len = match (base.path.as_deref(), target.path.as_deref()) {
        (None | Some(""), _) => 1,
        (Some(base_path), Some(target_path)) if path_prefix_matches(base_path, target_path) => {
            base_path.len()
        }
        _ => return None,
    };
    let user_matched = match base.username.as_deref() {
        Some(base_user) if target.username.as_deref() != Some(base_user) => return None,
        Some(_) => true,
        None => false,
    };
    Some(UrlMatchScore {
        host_len: base.host.as_deref().map(str::len).unwrap_or(0),
        path_len,
        user_matched,
    })
}

fn path_prefix_matches(base: &str, target: &str) -> bool {
    target == base
        || target
            .strip_prefix(base)
            .is_some_and(|remainder| remainder.starts_with('/'))
}

fn host_matches(url_host: &str, pattern_host: &str) -> bool {
    let mut url_parts = url_host.split('.');
    let mut pattern_parts = pattern_host.split('.');
    loop {
        match (url_parts.next(), pattern_parts.next()) {
            (None, None) => return true,
            (Some(_), None) | (None, Some(_)) => return false,
            (Some(_), Some("*")) => {}
            (Some(url_part), Some(pattern_part)) if url_part == pattern_part => {}
            _ => return false,
        }
    }
}

pub(crate) fn proto_is_http(protocol: Option<&str>) -> bool {
    matches!(protocol, Some("http") | Some("https"))
}

pub(crate) fn credential_apply_config(
    config: Option<&GitConfig>,
    stack: Option<&ConfigStack>,
    credential: &mut GitCredential,
) -> Result<()> {
    if credential.host.is_none() {
        return Err(GitError::InvalidFormat(
            "fatal: refusing to work with credential missing host field".into(),
        ));
    }
    if credential.protocol.is_none() {
        return Err(GitError::InvalidFormat(
            "fatal: refusing to work with credential missing protocol field".into(),
        ));
    }
    if credential.configured {
        return Ok(());
    }
    if let Some(stack) = stack {
        collect_credential_config_stack(stack, credential)?;
    } else if let Some(config) = config {
        collect_credential_config(config, credential)?;
    }
    if !credential.use_http_path && proto_is_http(credential.protocol.as_deref()) {
        credential.path = None;
    }
    credential.configured = true;
    Ok(())
}

pub(crate) fn parse_timestamp(value: &str) -> i64 {
    value.parse::<i64>().unwrap_or(TIME_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoded_newline_makes_a_full_url_invalid() {
        let error = credential_from_url_gently(
            "https://one.example.com?%0ahost=two.example.com/",
            false,
            true,
        )
        .expect_err("decoded newline must invalidate a credential URL");
        assert_eq!(
            error,
            GitError::InvalidFormat("url contains a newline in its path component".into())
        );
    }

    #[test]
    fn carriage_return_survives_url_parsing_for_protocol_protection() {
        let credential = credential_from_url_gently("https://example%0d.com/", false, true)
            .expect("carriage return is checked by credential protocol protection");
        assert_eq!(credential.host.as_deref(), Some("example\r.com"));
    }

    #[test]
    fn urlmatch_path_prefix_stops_at_component_boundary() {
        let target = "https://example.com/org/repo.git";
        assert!(urlmatch_score("https://example.com/org", target).is_some());
        assert!(urlmatch_score("https://example.com/org/", target).is_some());
        assert!(urlmatch_score("https://example.com/o", target).is_none());
    }

    #[test]
    fn credential_format_sanitizes_username_control_bytes_and_spaces() {
        let credential = GitCredential {
            protocol: Some("https".into()),
            host: Some("example.org".into()),
            username: Some("\u{7}latrix Lestrange".into()),
            ..GitCredential::default()
        };
        assert_eq!(
            credential_format(&credential),
            "https://%07latrix%20Lestrange@example.org"
        );
    }
}
