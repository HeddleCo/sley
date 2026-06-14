//! Typed `.gitmodules` configuration — a Rust port of git's `submodule-config.c`.
//!
//! Today every consumer in sley re-derives submodule fields by hand-walking a
//! [`GitConfig`] (`section.name == "submodule"`, `find(|e| e.key == "path")`,
//! …), scattered across ~14 call sites. This module centralizes that into one
//! typed parser so the submodule command AND the tree-switch commands share a
//! single source of truth for what a `.gitmodules` entry means.
//!
//! Porting notes (git `submodule-config.c`):
//! - [`parse_config`] is the per-key dispatch; we drive it over a parsed
//!   [`GitConfig`] rather than git's streaming `git_config_from_mem` callback,
//!   but the per-key semantics (last-one-wins vs. first-one-wins, validation,
//!   value normalization) match faithfully.
//! - [`check_submodule_name`] / [`check_submodule_url`] port the security
//!   checks that reject `..`-bearing names and command-line-option-looking
//!   values.
//! - The recurse-mode and update-strategy enums port `submodule.h`.

use sley_config::{GitConfig, parse_config_bool};

/// `enum submodule_recurse_mode` (git `submodule.h`). Discriminants match git's
/// so the numeric values are stable across the wire / config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RecurseMode {
    Only,
    Check,
    Error,
    #[default]
    None,
    OnDemand,
    Off,
    Default,
    On,
}

impl RecurseMode {
    /// Port of git's numeric discriminants for `submodule_recurse_mode`.
    pub fn as_i8(self) -> i8 {
        match self {
            RecurseMode::Only => -5,
            RecurseMode::Check => -4,
            RecurseMode::Error => -3,
            RecurseMode::None => -2,
            RecurseMode::OnDemand => -1,
            RecurseMode::Off => 0,
            RecurseMode::Default => 1,
            RecurseMode::On => 2,
        }
    }
}

/// Port of `parse_fetch_recurse` (git `submodule-config.c`). Returns
/// [`RecurseMode::Error`] on an unrecognized argument (the `die_on_error == 0`
/// branch); callers that want the fatal behavior check for `Error`.
pub fn parse_fetch_recurse(arg: &str) -> RecurseMode {
    match parse_config_bool(arg) {
        Some(true) => RecurseMode::On,
        Some(false) => RecurseMode::Off,
        None => {
            if arg == "on-demand" {
                RecurseMode::OnDemand
            } else {
                RecurseMode::Error
            }
        }
    }
}

/// `enum submodule_update_type` (git `submodule.h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UpdateType {
    #[default]
    Unspecified,
    Checkout,
    Rebase,
    Merge,
    None,
    Command,
}

/// `struct submodule_update_strategy` (git `submodule.h`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UpdateStrategy {
    pub kind: UpdateType,
    /// Only populated when `kind == Command`: the command string (the part
    /// after the leading `!`).
    pub command: Option<String>,
}

/// Port of `parse_submodule_update_type` (git `submodule.c`).
pub fn parse_update_type(value: &str) -> UpdateType {
    match value {
        "none" => UpdateType::None,
        "checkout" => UpdateType::Checkout,
        "rebase" => UpdateType::Rebase,
        "merge" => UpdateType::Merge,
        _ if value.starts_with('!') => UpdateType::Command,
        _ => UpdateType::Unspecified,
    }
}

/// Port of `parse_submodule_update_strategy` (git `submodule.c`). Returns the
/// parsed strategy, or `None` for an unrecognized value (git's `-1`).
pub fn parse_update_strategy(value: &str) -> Option<UpdateStrategy> {
    let kind = parse_update_type(value);
    if kind == UpdateType::Unspecified {
        return None;
    }
    let command = if kind == UpdateType::Command {
        Some(value[1..].to_string())
    } else {
        None
    };
    Some(UpdateStrategy { kind, command })
}

/// Port of `submodule_update_type_to_string` (git `submodule.c`). Returns
/// `None` for the two types that have no string form (`Unspecified`,
/// `Command`), matching git's `BUG()` cases — callers handle those before
/// stringifying.
pub fn update_type_to_string(kind: UpdateType) -> Option<&'static str> {
    match kind {
        UpdateType::Checkout => Some("checkout"),
        UpdateType::Merge => Some("merge"),
        UpdateType::Rebase => Some("rebase"),
        UpdateType::None => Some("none"),
        UpdateType::Unspecified | UpdateType::Command => None,
    }
}

/// A single submodule's typed configuration, the analogue of git's
/// `struct submodule`. Built by [`SubmoduleConfigSet::parse`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Submodule {
    /// The `.gitmodules` subsection (the submodule "name").
    pub name: String,
    pub path: Option<String>,
    pub url: Option<String>,
    pub fetch_recurse: RecurseMode,
    pub ignore: Option<String>,
    pub branch: Option<String>,
    pub update_strategy: UpdateStrategy,
    /// `submodule.<name>.shallow`; `None` means unset (git's `-1` sentinel).
    pub recommend_shallow: Option<bool>,
}

/// A diagnostic emitted while parsing `.gitmodules`, mirroring git's
/// `warning(...)` calls in `parse_config`. Surfacing them lets a consumer
/// reproduce git's stderr without this crate owning an output channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseWarning {
    /// `warning(_("ignoring suspicious submodule name: %s"), ...)`.
    SuspiciousName { name: String },
    /// `warning(_("ignoring '%s' which may be interpreted as a command-line
    /// option: %s"), ...)`.
    CommandLineOption { var: String, value: String },
    /// `warning("...multiple configurations found for 'submodule.%s.%s'...")`.
    MultipleConfig { name: String, option: String },
    /// `warning("Invalid parameter '%s' for config option
    /// 'submodule.%s.ignore'")`.
    InvalidIgnore { name: String, value: String },
}

/// The parsed set of all submodules from one `.gitmodules`, the analogue of the
/// path/name-keyed `submodule_cache`. Lookups are by name or by bound path,
/// matching git's `submodule_from_name` / `submodule_from_path`.
#[derive(Debug, Clone, Default)]
pub struct SubmoduleConfigSet {
    submodules: Vec<Submodule>,
    /// Non-fatal diagnostics produced during parsing, in encounter order.
    pub warnings: Vec<ParseWarning>,
}

impl SubmoduleConfigSet {
    /// Parse a typed submodule set from an already-loaded `.gitmodules`
    /// [`GitConfig`]. This is the moral equivalent of git driving
    /// `parse_config` over every `submodule.*.*` key in the blob.
    ///
    /// Per-key precedence matches git's `parse_config` with `overwrite == 0`:
    /// the FIRST value of a key wins and later duplicates emit a
    /// [`ParseWarning::MultipleConfig`] (git's `warn_multiple_config`).
    pub fn parse(config: &GitConfig) -> Self {
        let mut set = SubmoduleConfigSet::default();
        for section in &config.sections {
            if section.name != "submodule" {
                continue;
            }
            // `name_and_item_from_var`: the subsection IS the submodule name,
            // and suspicious names are dropped wholesale with a warning.
            let Some(name) = section.subsection.as_deref() else {
                continue;
            };
            if !check_submodule_name(name) {
                set.warnings.push(ParseWarning::SuspiciousName {
                    name: name.to_string(),
                });
                continue;
            }
            // Ensure the submodule exists even if it has zero recognized keys
            // (git's lookup_or_create_by_name runs before each key's dispatch).
            set.lookup_or_create_by_name(name);
            for entry in &section.entries {
                // git lowercases the variable name (`key`) before dispatch.
                let item = entry.key.to_ascii_lowercase();
                let value = entry.value.as_deref();
                parse_config(&mut set, name, &item, value);
            }
        }
        set
    }

    fn lookup_or_create_by_name(&mut self, name: &str) -> usize {
        if let Some(index) = self.submodules.iter().position(|sub| sub.name == name) {
            return index;
        }
        self.submodules.push(Submodule {
            name: name.to_string(),
            ..Submodule::default()
        });
        self.submodules.len() - 1
    }

    /// All parsed submodules, in `.gitmodules` declaration order.
    pub fn iter(&self) -> impl Iterator<Item = &Submodule> {
        self.submodules.iter()
    }

    /// `submodule_from_name`: look up by the `.gitmodules` subsection name.
    pub fn from_name(&self, name: &str) -> Option<&Submodule> {
        self.submodules.iter().find(|sub| sub.name == name)
    }

    /// `submodule_from_path`: look up by the path a submodule is bound at.
    pub fn from_path(&self, path: &str) -> Option<&Submodule> {
        self.submodules
            .iter()
            .find(|sub| sub.path.as_deref() == Some(path))
    }

    /// True when no submodules were declared.
    pub fn is_empty(&self) -> bool {
        self.submodules.is_empty()
    }

    /// Number of declared submodules.
    pub fn len(&self) -> usize {
        self.submodules.len()
    }
}

/// Per-key parse dispatch — direct port of git `submodule-config.c`'s
/// `parse_config`. `set` already contains the named submodule (created by the
/// caller); we index it back out so each arm can mutate in place and push
/// warnings onto `set.warnings`.
fn parse_config(set: &mut SubmoduleConfigSet, name: &str, item: &str, value: Option<&str>) {
    let index = set
        .submodules
        .iter()
        .position(|sub| sub.name == name)
        .expect("submodule created before parse_config dispatch");

    match item {
        "path" => {
            let Some(value) = value else { return };
            if looks_like_command_line_option(value) {
                set.warnings.push(ParseWarning::CommandLineOption {
                    var: format!("submodule.{name}.path"),
                    value: value.to_string(),
                });
            } else if set.submodules[index].path.is_some() {
                set.warnings.push(ParseWarning::MultipleConfig {
                    name: name.to_string(),
                    option: "path".to_string(),
                });
            } else {
                set.submodules[index].path = Some(value.to_string());
            }
        }
        "fetchrecursesubmodules" => {
            if set.submodules[index].fetch_recurse != RecurseMode::None {
                set.warnings.push(ParseWarning::MultipleConfig {
                    name: name.to_string(),
                    option: "fetchrecursesubmodules".to_string(),
                });
            } else if let Some(value) = value {
                set.submodules[index].fetch_recurse = parse_fetch_recurse(value);
            }
        }
        "ignore" => {
            let Some(value) = value else { return };
            if set.submodules[index].ignore.is_some() {
                set.warnings.push(ParseWarning::MultipleConfig {
                    name: name.to_string(),
                    option: "ignore".to_string(),
                });
            } else if !matches!(value, "untracked" | "dirty" | "all" | "none") {
                set.warnings.push(ParseWarning::InvalidIgnore {
                    name: name.to_string(),
                    value: value.to_string(),
                });
            } else {
                set.submodules[index].ignore = Some(value.to_string());
            }
        }
        "url" => {
            let Some(value) = value else { return };
            if looks_like_command_line_option(value) {
                set.warnings.push(ParseWarning::CommandLineOption {
                    var: format!("submodule.{name}.url"),
                    value: value.to_string(),
                });
            } else if set.submodules[index].url.is_some() {
                set.warnings.push(ParseWarning::MultipleConfig {
                    name: name.to_string(),
                    option: "url".to_string(),
                });
            } else {
                set.submodules[index].url = Some(value.to_string());
            }
        }
        "update" => {
            let Some(value) = value else { return };
            if set.submodules[index].update_strategy.kind != UpdateType::Unspecified {
                set.warnings.push(ParseWarning::MultipleConfig {
                    name: name.to_string(),
                    option: "update".to_string(),
                });
            } else if let Some(strategy) = parse_update_strategy(value)
                && strategy.kind != UpdateType::Command
            {
                // git die()s on a bad value or a `!command` from .gitmodules;
                // we keep the unspecified strategy (rejecting the value) rather
                // than aborting the whole parse, since this crate has no fatal
                // channel. The command-form `!cmd` is forbidden from
                // .gitmodules for security and is silently dropped here.
                set.submodules[index].update_strategy = strategy;
            }
        }
        "shallow" => {
            if set.submodules[index].recommend_shallow.is_some() {
                set.warnings.push(ParseWarning::MultipleConfig {
                    name: name.to_string(),
                    option: "shallow".to_string(),
                });
            } else {
                // git_config_bool: a bare key (no value) is true.
                let parsed = value.is_none_or(|v| parse_config_bool(v).unwrap_or(false));
                set.submodules[index].recommend_shallow = Some(parsed);
            }
        }
        "branch" => {
            let Some(value) = value else { return };
            if set.submodules[index].branch.is_some() {
                set.warnings.push(ParseWarning::MultipleConfig {
                    name: name.to_string(),
                    option: "branch".to_string(),
                });
            } else {
                set.submodules[index].branch = Some(value.to_string());
            }
        }
        // git's parse_config silently ignores any other submodule.<name>.<key>.
        _ => {}
    }
}

/// Port of `check_submodule_name` (git `submodule-config.c`). Returns `true`
/// if `name` is syntactically acceptable as a `.gitmodules` subsection, `false`
/// otherwise (git's `0` vs `-1`). Rejects empty names and any `..` path
/// component (using the cross-platform separator set `/` and `\` so the rule is
/// OS-independent).
pub fn check_submodule_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let bytes = name.as_bytes();
    // git starts "inside a component" then re-checks at every separator.
    let mut i = 0;
    let mut at_component_start = true;
    while i <= bytes.len() {
        if at_component_start && is_xplatform_dir_sep_component(bytes, i) {
            return false;
        }
        at_component_start = false;
        if i < bytes.len() && is_xplatform_dir_sep(bytes[i]) {
            at_component_start = true;
        }
        i += 1;
    }
    true
}

/// A `..` component begins at `i` when bytes[i..] is `..` followed by EOS or a
/// separator. Mirrors git's `name[0]=='.' && name[1]=='.' && (!name[2] ||
/// sep(name[2]))` check applied at each component boundary.
fn is_xplatform_dir_sep_component(bytes: &[u8], i: usize) -> bool {
    bytes.get(i) == Some(&b'.')
        && bytes.get(i + 1) == Some(&b'.')
        && match bytes.get(i + 2) {
            None => true,
            Some(&c) => is_xplatform_dir_sep(c),
        }
}

fn is_xplatform_dir_sep(c: u8) -> bool {
    c == b'/' || c == b'\\'
}

/// Port of `check_submodule_url` (git `submodule-config.c`). Returns `true`
/// if the URL is acceptable (per the CVE-2020-11008 / option-injection checks),
/// `false` otherwise (git's `0` vs `-1`). Mirrors the relative-URL and
/// `git://` newline/`../`-escape checks; the http(s) `url_normalize` round-trip
/// is approximated by the same newline check on the decoded form (sley has no
/// `url_normalize` yet — TODO(submodule) below).
pub fn check_submodule_url(url: &str) -> bool {
    if looks_like_command_line_option(url) {
        return false;
    }

    if submodule_url_is_relative(url) || url.starts_with("git://") {
        let decoded = url_decode(url);
        if decoded.contains('\n') {
            return false;
        }
        // URLs that escape their root via "../" can overwrite the host field.
        let (dotdots, next) = count_leading_dotdots(url);
        if dotdots > 0 {
            let first = next.as_bytes().first().copied();
            if first == Some(b':') || first == Some(b'/') {
                return false;
            }
        }
    } else if let Some(curl_url) = url_to_curl_url(url) {
        // TODO(submodule): port url_normalize for the full http(s) check.
        // For now reject only an embedded newline in the decoded form, which is
        // the concrete injection vector check_submodule_url guards.
        let decoded = url_decode(curl_url);
        if decoded.contains('\n') {
            return false;
        }
    }

    true
}

fn submodule_url_is_relative(url: &str) -> bool {
    url.starts_with("./") || url.starts_with("../")
}

/// Port of `count_leading_dotdots` (git `submodule-config.c`): counts leading
/// `../` components (skipping `./`) and returns the remaining suffix.
fn count_leading_dotdots(url: &str) -> (usize, &str) {
    let mut result = 0;
    let mut rest = url;
    loop {
        if let Some(stripped) = rest.strip_prefix("../") {
            result += 1;
            rest = stripped;
        } else if let Some(stripped) = rest.strip_prefix("./") {
            rest = stripped;
        } else {
            return (result, rest);
        }
    }
}

/// Port of `url_to_curl_url` (git `submodule-config.c`): if the transport is one
/// git-remote-curl handles, returns the URL that would be passed to it.
fn url_to_curl_url(url: &str) -> Option<&str> {
    for prefix in ["http::", "https::", "ftp::", "ftps::"] {
        if let Some(stripped) = url.strip_prefix(prefix) {
            return Some(stripped);
        }
    }
    for prefix in ["http://", "https://", "ftp://", "ftps://"] {
        if url.starts_with(prefix) {
            return Some(url);
        }
    }
    None
}

/// Port of git's `looks_like_command_line_option`: a value starting with `-`
/// could be mistaken for a CLI flag when passed to a child git process.
pub fn looks_like_command_line_option(value: &str) -> bool {
    value.starts_with('-')
}

/// Minimal percent-decoder for the `check_submodule_url` newline check. Mirrors
/// git's `url_decode` for the bytes we care about (`%0a` etc.); leaves
/// malformed escapes intact.
fn url_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = hex_val(bytes[i + 1]);
            let lo = hex_val(bytes[i + 2]);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use sley_config::GitConfig;

    fn config_from(text: &str) -> GitConfig {
        GitConfig::parse(text.as_bytes()).expect("valid config")
    }

    #[test]
    fn parses_basic_submodule() {
        let cfg = config_from(
            "[submodule \"lib\"]\n\tpath = lib\n\turl = https://example.com/lib.git\n",
        );
        let set = SubmoduleConfigSet::parse(&cfg);
        assert_eq!(set.len(), 1);
        let sub = set.from_name("lib").expect("lib present");
        assert_eq!(sub.path.as_deref(), Some("lib"));
        assert_eq!(sub.url.as_deref(), Some("https://example.com/lib.git"));
        assert_eq!(set.from_path("lib").map(|s| s.name.as_str()), Some("lib"));
    }

    #[test]
    fn first_value_wins_and_warns_on_duplicate() {
        let cfg = config_from("[submodule \"x\"]\n\tpath = a\n\tpath = b\n");
        let set = SubmoduleConfigSet::parse(&cfg);
        assert_eq!(set.from_name("x").and_then(|s| s.path.as_deref()), Some("a"));
        assert!(set.warnings.iter().any(|w| matches!(
            w,
            ParseWarning::MultipleConfig { option, .. } if option == "path"
        )));
    }

    #[test]
    fn suspicious_name_dropped() {
        let cfg = config_from("[submodule \"../evil\"]\n\tpath = x\n");
        let set = SubmoduleConfigSet::parse(&cfg);
        assert!(set.is_empty());
        assert!(matches!(
            set.warnings.first(),
            Some(ParseWarning::SuspiciousName { .. })
        ));
    }

    #[test]
    fn check_submodule_name_rejects_dotdot() {
        assert!(!check_submodule_name("a/../b"));
        assert!(!check_submodule_name(".."));
        assert!(!check_submodule_name("../x"));
        assert!(!check_submodule_name("a/.."));
        assert!(!check_submodule_name(""));
        assert!(check_submodule_name("normal/name"));
        assert!(check_submodule_name("a..b"));
        assert!(check_submodule_name("..."));
    }

    #[test]
    fn check_submodule_url_rejects_escapes() {
        // Looks like a command-line option.
        assert!(!check_submodule_url("-upload-pack=evil"));
        // Relative URL whose first byte after the leading "../" is ':' / '/',
        // the CVE-2020-11008 host-overwrite vector git guards.
        assert!(!check_submodule_url("../:evil"));
        assert!(!check_submodule_url("..//evil"));
        // A relative URL with a normal first component after the "../" is fine
        // (git only rejects the ':'/'/' first byte).
        assert!(check_submodule_url("../../../host/path"));
        assert!(check_submodule_url("https://example.com/ok.git"));
        assert!(check_submodule_url("./relative"));
        // Embedded newline in a git:// / relative URL is rejected.
        assert!(!check_submodule_url("git://h/%0arepo"));
    }

    #[test]
    fn update_strategy_parses() {
        assert_eq!(parse_update_type("checkout"), UpdateType::Checkout);
        assert_eq!(parse_update_type("none"), UpdateType::None);
        assert_eq!(parse_update_type("!cmd"), UpdateType::Command);
        assert_eq!(parse_update_type("bogus"), UpdateType::Unspecified);
        let strat = parse_update_strategy("!run").expect("command");
        assert_eq!(strat.kind, UpdateType::Command);
        assert_eq!(strat.command.as_deref(), Some("run"));
        assert!(parse_update_strategy("bogus").is_none());
    }

    #[test]
    fn fetch_recurse_parses() {
        assert_eq!(parse_fetch_recurse("true"), RecurseMode::On);
        assert_eq!(parse_fetch_recurse("false"), RecurseMode::Off);
        assert_eq!(parse_fetch_recurse("on-demand"), RecurseMode::OnDemand);
        assert_eq!(parse_fetch_recurse("garbage"), RecurseMode::Error);
    }

    #[test]
    fn shallow_and_branch_parse() {
        let cfg = config_from(
            "[submodule \"s\"]\n\tbranch = main\n\tshallow = true\n",
        );
        let set = SubmoduleConfigSet::parse(&cfg);
        let sub = set.from_name("s").expect("s");
        assert_eq!(sub.branch.as_deref(), Some("main"));
        assert_eq!(sub.recommend_shallow, Some(true));
    }
}
