//! Structured editing of `[remote "<name>"]` configuration.
//!
//! These helpers operate on an in-memory [`GitConfig`] document and implement
//! the document-mutation half of `git remote add` / `git remote remove` /
//! `git remote set-url`: they decide which `[remote "<name>"]` sections and
//! `[branch]` back-references to add, drop, or rewrite. They deliberately do
//! *not* read or write files, run argument parsing, or print anything — callers
//! (e.g. the CLI) own I/O, argument handling, and the exact user-facing
//! diagnostics, translating the structured [`RemoteEditError`] /
//! [`SetUrlError`] outcomes returned here into their own messages and exit
//! codes.
//!
//! Callers should persist edits with [`GitConfig::to_preserved_bytes`] when the
//! config was loaded from a user file (comments and blank lines are kept).
//! [`GitConfig::to_canonical_bytes`] remains for tests and green-field writes.

use std::collections::BTreeSet;
use std::path::Path;

use crate::{ConfigEntry, ConfigSection, GitConfig};

/// The default fetch refspec git writes for a freshly added remote:
/// `+refs/heads/*:refs/remotes/<name>/*`.
pub fn default_fetch_refspec(name: &str) -> String {
    format!("+refs/heads/*:refs/remotes/{name}/*")
}

/// The configured remote names — the subsection of every `[remote "<name>"]`
/// section — sorted alphabetically with duplicates collapsed.
///
/// This mirrors the order `git remote` lists remotes in (and the order the CLI
/// has always used): names are de-duplicated and sorted, not returned in raw
/// file order.
pub fn remote_names(config: &GitConfig) -> Vec<String> {
    config
        .sections
        .iter()
        .filter(|section| section.name == "remote")
        .filter_map(|section| section.subsection.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// All `remote.<name>.<key>` values, in config order.
pub fn remote_config_values(config: &GitConfig, name: &str, key: &str) -> Vec<String> {
    config
        .sections
        .iter()
        .filter(|section| section.name == "remote" && section.subsection.as_deref() == Some(name))
        .flat_map(|section| {
            section
                .entries
                .iter()
                .filter(move |entry| entry.key.eq_ignore_ascii_case(key))
                .filter_map(|entry| entry.value.clone())
        })
        .collect()
}

/// Rewrite `url` per the longest matching `url.<base>.insteadOf` (or
/// `pushInsteadOf` when `push`) prefix, mirroring git's `insteadOf` resolution.
pub fn rewrite_url_with_config(config: &GitConfig, url: &str, push: bool) -> String {
    let mut best: Option<(&str, &str, u8)> = None;
    for section in &config.sections {
        if section.name != "url" {
            continue;
        }
        let Some(base) = section.subsection.as_deref() else {
            continue;
        };
        for entry in &section.entries {
            let priority = if push && entry.key.eq_ignore_ascii_case("pushInsteadOf") {
                2
            } else if entry.key.eq_ignore_ascii_case("insteadOf") {
                1
            } else {
                continue;
            };
            let Some(prefix) = entry.value.as_deref() else {
                continue;
            };
            if !url.starts_with(prefix) {
                continue;
            }
            let replace = match best {
                None => true,
                Some((_, best_prefix, best_priority)) => {
                    priority > best_priority
                        || (priority == best_priority && prefix.len() > best_prefix.len())
                }
            };
            if replace {
                best = Some((base, prefix, priority));
            }
        }
    }
    if let Some((base, prefix, _)) = best {
        format!("{base}{}", &url[prefix.len()..])
    } else {
        url.to_string()
    }
}

/// Resolve a fetch URL for `remote`, which may be a configured remote name or a
/// literal URL/path. Uses `remote.<name>.url` when configured, then applies
/// `url.*.insteadOf` rewriting from `config`.
pub fn resolve_remote_fetch_url(config: &GitConfig, remote: &str) -> String {
    let url = remote_config_values(config, remote, "url")
        .into_iter()
        .next()
        .unwrap_or_else(|| remote.to_string());
    rewrite_url_with_config(config, &url, false)
}

/// Resolve a push URL for `remote`, preferring `pushurl` over `url` when
/// configured. An explicit `pushurl` is not rewritten by `pushInsteadOf`
/// (matching git's `remote.c`); it still gets regular `insteadOf` rewriting.
/// When falling back to `url` or a literal remote argument, `pushInsteadOf`
/// participates in the rewrite.
pub fn resolve_remote_push_url(config: &GitConfig, remote: &str) -> String {
    if let Some(url) = remote_config_values(config, remote, "pushurl")
        .into_iter()
        .next()
    {
        return rewrite_url_with_config(config, &url, false);
    }
    let url = remote_config_values(config, remote, "url")
        .into_iter()
        .next()
        .unwrap_or_else(|| remote.to_string());
    rewrite_url_with_config(config, &url, true)
}

/// Whether a `[remote "<name>"]` section exists (subsection matched
/// case-sensitively, as git treats subsection names).
pub fn remote_exists(config: &GitConfig, name: &str) -> bool {
    config
        .sections
        .iter()
        .any(|section| section.name == "remote" && section.subsection.as_deref() == Some(name))
}

/// Whether `name` is a valid remote nickname, mirroring git's
/// `valid_remote_nick`: non-empty, not `.`/`..`, and containing no path
/// separators. Only such names trigger the legacy `remotes/`/`branches/` file
/// lookup.
fn valid_remote_nick(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/') && !name.contains('\\')
}

/// The repository default branch name used to fill the missing `#frag` of a
/// `branches/<name>` file, mirroring git's `repo_default_branch_name`:
/// `GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME` env, then `init.defaultBranch`, then
/// the compiled-in default (`master`).
fn default_branch_name(config: &GitConfig) -> String {
    if let Ok(name) = std::env::var("GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME")
        && !name.is_empty()
    {
        return name;
    }
    config
        .get("init", None, "defaultBranch")
        .filter(|name| !name.is_empty())
        .unwrap_or("master")
        .to_string()
}

/// Synthesize `[remote "<name>"]` sections from git's legacy file-based remote
/// definitions in `$GIT_DIR/remotes/<name>` and `$GIT_DIR/branches/<name>`,
/// appending them to `config`.
///
/// This mirrors git's `remote.c` lazy lookup order: a `remotes/`/`branches/`
/// file is only consulted for a remote nickname that is *not* already a valid
/// config remote (one with a `url`). For each such file, the parsed URL and
/// refspecs are turned into the equivalent config entries so every downstream
/// consumer (`resolve_remote_fetch_url`, the configured-fetch refspecs, push)
/// sees a uniform `[remote "<name>"]` view.
///
/// `remotes/<name>` format (`read_remotes_file`):
/// * `URL: <url>` → `url`
/// * `Pull: <spec>` → `fetch`
/// * `Push: <spec>` → `push`
///
/// `branches/<name>` format (`read_branches_file`): a single line `<url>` or
/// `<url>#<branch>`. The fetch refspec is `refs/heads/<branch>:refs/heads/<name>`
/// (the local branch matching the remote name), the push is `HEAD:refs/heads/
/// <branch>`, and tags are always auto-followed.
pub fn augment_with_legacy_remote_files(config: &mut GitConfig, git_dir: &Path) {
    let mut new_sections = Vec::new();
    augment_from_remotes_dir(config, git_dir, &mut new_sections);
    augment_from_branches_dir(config, git_dir, &mut new_sections);
    config.sections.extend(new_sections);
}

fn augment_from_remotes_dir(config: &GitConfig, git_dir: &Path, out: &mut Vec<ConfigSection>) {
    let dir = git_dir.join("remotes");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !valid_remote_nick(&name) || remote_exists(config, &name) {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let mut section_entries = Vec::new();
        for line in contents.lines() {
            let line = line.trim_end();
            if let Some(url) = line.strip_prefix("URL:") {
                section_entries.push(ConfigEntry::new("url", Some(url.trim_start().to_string())));
            } else if let Some(spec) = line.strip_prefix("Pull:") {
                section_entries.push(ConfigEntry::new(
                    "fetch",
                    Some(spec.trim_start().to_string()),
                ));
            } else if let Some(spec) = line.strip_prefix("Push:") {
                section_entries.push(ConfigEntry::new(
                    "push",
                    Some(spec.trim_start().to_string()),
                ));
            }
        }
        if section_entries.iter().any(|e| e.key == "url") {
            out.push(ConfigSection::new("remote", Some(name), section_entries));
        }
    }
}

fn augment_from_branches_dir(config: &GitConfig, git_dir: &Path, out: &mut Vec<ConfigSection>) {
    let dir = git_dir.join("branches");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let default_branch = default_branch_name(config);
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !valid_remote_nick(&name)
            || remote_exists(config, &name)
            || out
                .iter()
                .any(|s| s.subsection.as_deref() == Some(name.as_str()))
        {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let first = contents.lines().next().unwrap_or("").trim();
        if first.is_empty() {
            continue;
        }
        let (url, frag) = match first.split_once('#') {
            Some((url, frag)) => (url, frag.to_string()),
            None => (first, default_branch.clone()),
        };
        let section_entries = vec![
            ConfigEntry::new("url", Some(url.to_string())),
            ConfigEntry::new(
                "fetch",
                Some(format!("refs/heads/{frag}:refs/heads/{name}")),
            ),
            ConfigEntry::new("push", Some(format!("HEAD:refs/heads/{frag}"))),
            // git sets remote->fetch_tags = 1 (always auto-follow tags). The
            // tagopt config equivalent is `--tags`.
            ConfigEntry::new("tagopt", Some("--tags".to_string())),
        ];
        out.push(ConfigSection::new("remote", Some(name), section_entries));
    }
}

/// Failure modes shared by [`add_remote`] and [`remove_remote`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteEditError {
    /// `add_remote` was asked to create a remote that already exists.
    AlreadyExists,
    /// `remove_remote` was asked to drop a remote that does not exist.
    NotFound,
}

/// Append a `[remote "<name>"]` section built from `entries`, failing with
/// [`RemoteEditError::AlreadyExists`] if the remote is already configured.
///
/// `entries` is the fully-formed body of the section (e.g. a `url` entry plus
/// one or more `fetch` entries); building it from command-line options
/// (`--mirror`, `--track`, `--tags`, …) is the caller's responsibility. For the
/// common case use [`add_remote_with_fetch`].
pub fn add_remote(
    config: &mut GitConfig,
    name: &str,
    entries: Vec<ConfigEntry>,
) -> Result<(), RemoteEditError> {
    if remote_exists(config, name) {
        return Err(RemoteEditError::AlreadyExists);
    }
    config.sections.push(ConfigSection::new(
        "remote",
        Some(name.to_string()),
        entries,
    ));
    Ok(())
}

/// Append a `[remote "<name>"]` section with a `url` entry and the given fetch
/// refspecs, failing with [`RemoteEditError::AlreadyExists`] if the remote is
/// already configured.
///
/// When `fetch_refspecs` is empty the standard default
/// (`+refs/heads/*:refs/remotes/<name>/*`, see [`default_fetch_refspec`]) is
/// written, matching `git remote add <name> <url>`.
pub fn add_remote_with_fetch(
    config: &mut GitConfig,
    name: &str,
    url: &str,
    fetch_refspecs: &[String],
) -> Result<(), RemoteEditError> {
    let mut entries = vec![ConfigEntry::new("url", Some(url.to_string()))];
    if fetch_refspecs.is_empty() {
        entries.push(ConfigEntry::new("fetch", Some(default_fetch_refspec(name))));
    } else {
        for refspec in fetch_refspecs {
            entries.push(ConfigEntry::new("fetch", Some(refspec.clone())));
        }
    }
    add_remote(config, name, entries)
}

/// Remove the `[remote "<name>"]` section and every back-reference to it, the
/// way `git remote remove` does.
///
/// In addition to dropping the remote's own section this clears dependent
/// configuration:
/// * `[branch].remote = <name>` and the matching `[branch].merge` for branches
///   that track this remote;
/// * any `[branch].pushRemote = <name>`;
/// * `[remote].pushDefault = <name>`.
///
/// `[branch]` and bare `[remote]` sections left empty by those removals are
/// dropped. Returns [`RemoteEditError::NotFound`] (leaving `config` unchanged)
/// when no `[remote "<name>"]` section exists.
pub fn remove_remote(config: &mut GitConfig, name: &str) -> Result<(), RemoteEditError> {
    let before = config.sections.len();
    config.sections.retain(|section| {
        !(section.name == "remote" && section.subsection.as_deref() == Some(name))
    });
    if config.sections.len() == before {
        return Err(RemoteEditError::NotFound);
    }
    remove_remote_dependent_config(config, name);
    Ok(())
}

/// Strip configuration that references `remote` once its own section is gone:
/// tracking branches (`branch.<x>.remote`/`merge`), push overrides
/// (`branch.<x>.pushRemote`), and `remote.pushDefault`. Sections emptied by the
/// removal are dropped.
fn remove_remote_dependent_config(config: &mut GitConfig, remote: &str) {
    for section in &mut config.sections {
        if section.name == "branch" {
            let remote_matches = section.entries.iter().any(|entry| {
                entry.key.eq_ignore_ascii_case("remote") && entry.value.as_deref() == Some(remote)
            });
            section.entries.retain(|entry| {
                let key = entry.key.to_ascii_lowercase();
                if remote_matches && (key == "remote" || key == "merge") {
                    return false;
                }
                !(key == "pushremote" && entry.value.as_deref() == Some(remote))
            });
        } else if section.name == "remote" && section.subsection.is_none() {
            section.entries.retain(|entry| {
                !(entry.key.eq_ignore_ascii_case("pushDefault")
                    && entry.value.as_deref() == Some(remote))
            });
        }
    }
    config.sections.retain(|section| {
        !((section.name == "branch" || (section.name == "remote" && section.subsection.is_none()))
            && section.entries.is_empty())
    });
}

/// Which URL list a [`set_url`] call edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetUrlKind {
    /// Edit the fetch URLs (`remote.<name>.url`).
    Fetch,
    /// Edit the push URLs (`remote.<name>.pushurl`).
    Push,
}

impl SetUrlKind {
    /// The configuration key this kind edits (`url` or `pushurl`).
    pub fn key(self) -> &'static str {
        match self {
            SetUrlKind::Fetch => "url",
            SetUrlKind::Push => "pushurl",
        }
    }
}

/// The mutation [`set_url`] performs on the selected URL list.
///
/// `Delete` and `Replace` carry the URL matcher git applies (the CLI builds a
/// `value-pattern` matcher; here it is any predicate over the stored URL
/// string) so this crate stays free of a regex implementation.
pub enum SetUrlOp<'a> {
    /// `--add`: append `url` to the list.
    Add { url: &'a str },
    /// `--delete`: remove every URL matching the predicate.
    Delete { matches: &'a dyn Fn(&str) -> bool },
    /// `set-url <name> <newurl> <oldurl>`: replace the single URL matching the
    /// predicate with `url`.
    Replace {
        url: &'a str,
        matches: &'a dyn Fn(&str) -> bool,
    },
    /// `set-url <name> <newurl>`: set the sole URL (or insert one if none
    /// exists) to `url`.
    Set { url: &'a str },
}

/// Why a [`set_url`] call could not be applied.
///
/// Each variant corresponds to a distinct `git remote set-url` failure; the
/// caller maps them to git's exact diagnostics and exit status. `config` is
/// left unchanged when any of these is returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetUrlError {
    /// No `[remote "<name>"]` section exists.
    RemoteNotFound,
    /// `Replace` found no URL matching the old-URL pattern.
    NoMatch,
    /// `Delete` found no URL matching the pattern.
    DeleteNoMatch,
    /// `Delete` (on fetch URLs) would remove every non-push URL.
    DeleteAllFetchUrls,
    /// `Set`/`Replace` found multiple values where a single one was required.
    MultipleValues,
}

/// Apply a URL edit to `remote.<name>`'s fetch or push URL list, mirroring
/// `git remote set-url` (including its `--push`, `--add`, and `--delete`
/// variants).
///
/// On success `config` is mutated in place and `Ok(())` is returned; on any
/// [`SetUrlError`] the document is left untouched. The target section is the
/// last `[remote "<name>"]` section in the document (git edits the
/// highest-precedence one).
pub fn set_url(
    config: &mut GitConfig,
    name: &str,
    kind: SetUrlKind,
    op: SetUrlOp<'_>,
) -> Result<(), SetUrlError> {
    let key = kind.key();
    let Some(section) =
        config.sections.iter_mut().rev().find(|section| {
            section.name == "remote" && section.subsection.as_deref() == Some(name)
        })
    else {
        return Err(SetUrlError::RemoteNotFound);
    };
    match op {
        SetUrlOp::Add { url } => {
            section
                .entries
                .push(ConfigEntry::new(key, Some(url.to_string())));
            Ok(())
        }
        SetUrlOp::Delete { matches } => set_url_delete(section, kind, key, matches),
        SetUrlOp::Replace { url, matches } => set_url_replace(section, key, url, matches),
        SetUrlOp::Set { url } => set_url_set(section, key, url),
    }
}

fn set_url_delete(
    section: &mut ConfigSection,
    kind: SetUrlKind,
    key: &str,
    matches: &dyn Fn(&str) -> bool,
) -> Result<(), SetUrlError> {
    let matched = section
        .entries
        .iter()
        .filter(|entry| entry.key.eq_ignore_ascii_case(key))
        .filter(|entry| entry.value.as_deref().is_some_and(matches))
        .count();
    if matched == 0 {
        return Err(SetUrlError::DeleteNoMatch);
    }
    if kind == SetUrlKind::Fetch {
        let remaining = section
            .entries
            .iter()
            .filter(|entry| entry.key.eq_ignore_ascii_case(key))
            .filter(|entry| entry.value.as_deref().is_none_or(|value| !matches(value)))
            .count();
        if remaining == 0 {
            return Err(SetUrlError::DeleteAllFetchUrls);
        }
    }
    section.entries.retain(|entry| {
        !(entry.key.eq_ignore_ascii_case(key) && entry.value.as_deref().is_some_and(matches))
    });
    Ok(())
}

fn set_url_replace(
    section: &mut ConfigSection,
    key: &str,
    url: &str,
    matches: &dyn Fn(&str) -> bool,
) -> Result<(), SetUrlError> {
    let indices = section
        .entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.key.eq_ignore_ascii_case(key))
        .filter(|(_, entry)| entry.value.as_deref().is_some_and(matches))
        .map(|(idx, _)| idx)
        .collect::<Vec<_>>();
    match indices.as_slice() {
        [idx] => {
            section.entries[*idx].value = Some(url.to_string());
            Ok(())
        }
        [] => Err(SetUrlError::NoMatch),
        _ => Err(SetUrlError::MultipleValues),
    }
}

fn set_url_set(section: &mut ConfigSection, key: &str, url: &str) -> Result<(), SetUrlError> {
    let indices = section
        .entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.key.eq_ignore_ascii_case(key))
        .map(|(idx, _)| idx)
        .collect::<Vec<_>>();
    if indices.len() > 1 {
        return Err(SetUrlError::MultipleValues);
    }
    if let Some(idx) = indices.first().copied() {
        section.entries[idx].value = Some(url.to_string());
    } else {
        section
            .entries
            .insert(0, ConfigEntry::new(key, Some(url.to_string())));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_from(text: &str) -> GitConfig {
        GitConfig::parse(text.as_bytes()).expect("parse config")
    }

    fn render(config: &GitConfig) -> String {
        String::from_utf8(config.to_canonical_bytes()).expect("utf8 config")
    }

    #[test]
    fn remote_names_are_sorted_and_deduped() {
        let config = config_from(
            "[remote \"origin\"]\n\turl = a\n\
             [remote \"upstream\"]\n\turl = b\n\
             [remote \"origin\"]\n\tpushurl = c\n",
        );
        assert_eq!(remote_names(&config), vec!["origin", "upstream"]);
    }

    #[test]
    fn remote_config_values_preserve_config_order() {
        let config = config_from(
            "[remote \"origin\"]\n\turl = first\n\turl = second\n\
             [remote \"origin\"]\n\turl = third\n\tpushurl = push\n",
        );
        assert_eq!(
            remote_config_values(&config, "origin", "url"),
            vec!["first", "second", "third"]
        );
        assert_eq!(
            remote_config_values(&config, "origin", "pushurl"),
            vec!["push"]
        );
    }

    #[test]
    fn rewrite_url_uses_longest_insteadof_match() {
        let config = config_from(
            "[url \"ssh://example.com/\"]\n\tinsteadOf = ex:\n\
             [url \"ssh://example.com/specific/\"]\n\tinsteadOf = ex:specific/\n",
        );
        assert_eq!(
            rewrite_url_with_config(&config, "ex:specific/repo.git", false),
            "ssh://example.com/specific/repo.git"
        );
    }

    #[test]
    fn rewrite_url_prefers_pushinsteadof_for_pushes() {
        let config = config_from(
            "[url \"https://example.com/\"]\n\tinsteadOf = ex:\n\
             [url \"ssh://push.example.com/\"]\n\tpushInsteadOf = ex:\n",
        );
        assert_eq!(
            rewrite_url_with_config(&config, "ex:repo.git", false),
            "https://example.com/repo.git"
        );
        assert_eq!(
            rewrite_url_with_config(&config, "ex:repo.git", true),
            "ssh://push.example.com/repo.git"
        );
    }

    #[test]
    fn resolve_remote_fetch_and_push_urls_apply_precedence_and_rewrites() {
        let config = config_from(
            "[url \"https://fetch.example/\"]\n\tinsteadOf = short:\n\
             [url \"ssh://push.example/\"]\n\tpushInsteadOf = short:\n\
             [remote \"origin\"]\n\turl = short:repo.git\n\tpushurl = short:push.git\n",
        );
        assert_eq!(
            resolve_remote_fetch_url(&config, "origin"),
            "https://fetch.example/repo.git"
        );
        assert_eq!(
            resolve_remote_push_url(&config, "origin"),
            "https://fetch.example/push.git"
        );
    }

    #[test]
    fn resolve_remote_push_url_falls_back_to_fetch_url() {
        let config = config_from(
            "[url \"ssh://push.example/\"]\n\tpushInsteadOf = short:\n\
             [remote \"origin\"]\n\turl = short:repo.git\n",
        );
        assert_eq!(
            resolve_remote_push_url(&config, "origin"),
            "ssh://push.example/repo.git"
        );
    }

    #[test]
    fn resolve_literal_remote_url_is_rewritten_when_no_remote_exists() {
        let config = config_from("[url \"ssh://example/\"]\n\tinsteadOf = ex:\n");
        assert_eq!(
            resolve_remote_fetch_url(&config, "ex:repo.git"),
            "ssh://example/repo.git"
        );
    }

    #[test]
    fn add_remote_writes_url_and_default_fetch() {
        let mut config = GitConfig::default();
        add_remote_with_fetch(&mut config, "origin", "https://example/x.git", &[])
            .expect("add remote");
        assert_eq!(
            render(&config),
            "[remote \"origin\"]\n\turl = https://example/x.git\n\
             \tfetch = +refs/heads/*:refs/remotes/origin/*\n"
        );
    }

    #[test]
    fn add_remote_uses_supplied_fetch_refspecs() {
        let mut config = GitConfig::default();
        let specs = vec!["+refs/heads/main:refs/remotes/origin/main".to_string()];
        add_remote_with_fetch(&mut config, "origin", "url", &specs).expect("add remote");
        assert_eq!(
            config.get_all("remote", Some("origin"), "fetch"),
            vec![Some("+refs/heads/main:refs/remotes/origin/main")]
        );
    }

    #[test]
    fn add_remote_accepts_mirror_style_entries() {
        let mut config = GitConfig::default();
        add_remote(
            &mut config,
            "origin",
            vec![
                ConfigEntry::new("fetch", Some("+refs/*:refs/*".into())),
                ConfigEntry::new("mirror", Some("true".into())),
            ],
        )
        .expect("add mirror remote");
        assert_eq!(
            render(&config),
            "[remote \"origin\"]\n\tfetch = +refs/*:refs/*\n\tmirror = true\n"
        );
    }

    #[test]
    fn add_remote_rejects_duplicate() {
        let mut config = config_from("[remote \"origin\"]\n\turl = a\n");
        let err = add_remote_with_fetch(&mut config, "origin", "b", &[]).expect_err("duplicate");
        assert_eq!(err, RemoteEditError::AlreadyExists);
    }

    #[test]
    fn remove_remote_drops_section_and_back_references() {
        let mut config = config_from(
            "[remote \"origin\"]\n\turl = a\n\
             [branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n\
             [remote]\n\tpushDefault = origin\n",
        );
        remove_remote(&mut config, "origin").expect("remove");
        assert_eq!(render(&config), "");
    }

    #[test]
    fn remove_remote_drops_pushremote_references_but_keeps_other_remotes() {
        let mut config = config_from(
            "[remote \"origin\"]\n\turl = a\n\
             [remote \"backup\"]\n\turl = b\n\
             [branch \"main\"]\n\tremote = backup\n\tmerge = refs/heads/main\n\tpushRemote = origin\n\
             [branch \"topic\"]\n\tpushRemote = backup\n\
             [remote]\n\tpushDefault = backup\n",
        );
        remove_remote(&mut config, "origin").expect("remove");
        assert_eq!(
            render(&config),
            "[remote \"backup\"]\n\turl = b\n\
             [branch \"main\"]\n\tremote = backup\n\tmerge = refs/heads/main\n\
             [branch \"topic\"]\n\tpushRemote = backup\n\
             [remote]\n\tpushDefault = backup\n"
        );
    }

    #[test]
    fn remove_remote_keeps_unrelated_branch_keys() {
        let mut config = config_from(
            "[remote \"origin\"]\n\turl = a\n\
             [branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n\trebase = true\n",
        );
        remove_remote(&mut config, "origin").expect("remove");
        // The tracking keys go; the unrelated `rebase` key (and its section) stay.
        assert_eq!(render(&config), "[branch \"main\"]\n\trebase = true\n");
    }

    #[test]
    fn remove_remote_reports_missing() {
        let mut config = config_from("[remote \"origin\"]\n\turl = a\n");
        let err = remove_remote(&mut config, "missing").expect_err("missing");
        assert_eq!(err, RemoteEditError::NotFound);
    }

    #[test]
    fn set_url_replaces_sole_url() {
        let mut config = config_from("[remote \"origin\"]\n\turl = old\n");
        set_url(
            &mut config,
            "origin",
            SetUrlKind::Fetch,
            SetUrlOp::Set { url: "new" },
        )
        .expect("set");
        assert_eq!(config.get("remote", Some("origin"), "url"), Some("new"));
    }

    #[test]
    fn set_url_edits_highest_precedence_remote_section() {
        let mut config = config_from(
            "[remote \"origin\"]\n\turl = old-low\n\
             [remote \"origin\"]\n\turl = old-high\n",
        );
        set_url(
            &mut config,
            "origin",
            SetUrlKind::Fetch,
            SetUrlOp::Set { url: "new-high" },
        )
        .expect("set");
        assert_eq!(
            remote_config_values(&config, "origin", "url"),
            vec!["old-low", "new-high"]
        );
    }

    #[test]
    fn set_url_inserts_when_absent() {
        let mut config = config_from("[remote \"origin\"]\n\tfetch = spec\n");
        set_url(
            &mut config,
            "origin",
            SetUrlKind::Fetch,
            SetUrlOp::Set { url: "new" },
        )
        .expect("set");
        // url is inserted before the existing fetch entry.
        assert_eq!(
            render(&config),
            "[remote \"origin\"]\n\turl = new\n\tfetch = spec\n"
        );
    }

    #[test]
    fn set_url_add_appends_pushurl() {
        let mut config = config_from("[remote \"origin\"]\n\turl = a\n");
        set_url(
            &mut config,
            "origin",
            SetUrlKind::Push,
            SetUrlOp::Add { url: "p" },
        )
        .expect("add");
        assert_eq!(config.get("remote", Some("origin"), "pushurl"), Some("p"));
    }

    #[test]
    fn set_url_delete_refuses_to_empty_fetch_urls() {
        let mut config = config_from("[remote \"origin\"]\n\turl = only\n");
        let err = set_url(
            &mut config,
            "origin",
            SetUrlKind::Fetch,
            SetUrlOp::Delete {
                matches: &|value| value == "only",
            },
        )
        .expect_err("delete all");
        assert_eq!(err, SetUrlError::DeleteAllFetchUrls);
        // Document untouched on error.
        assert_eq!(config.get("remote", Some("origin"), "url"), Some("only"));
    }

    #[test]
    fn set_url_delete_removes_matching_push_urls() {
        let mut config =
            config_from("[remote \"origin\"]\n\turl = u\n\tpushurl = keep\n\tpushurl = drop\n");
        set_url(
            &mut config,
            "origin",
            SetUrlKind::Push,
            SetUrlOp::Delete {
                matches: &|value| value == "drop",
            },
        )
        .expect("delete");
        assert_eq!(
            config.get_all("remote", Some("origin"), "pushurl"),
            vec![Some("keep")]
        );
    }

    #[test]
    fn set_url_replace_requires_unique_match() {
        let mut config = config_from("[remote \"origin\"]\n\turl = same\n\turl = same\n");
        let err = set_url(
            &mut config,
            "origin",
            SetUrlKind::Fetch,
            SetUrlOp::Replace {
                url: "new",
                matches: &|value| value == "same",
            },
        )
        .expect_err("ambiguous");
        assert_eq!(err, SetUrlError::MultipleValues);
    }

    #[test]
    fn set_url_replace_reports_no_match() {
        let mut config = config_from("[remote \"origin\"]\n\turl = a\n");
        let err = set_url(
            &mut config,
            "origin",
            SetUrlKind::Fetch,
            SetUrlOp::Replace {
                url: "new",
                matches: &|value| value == "absent",
            },
        )
        .expect_err("no match");
        assert_eq!(err, SetUrlError::NoMatch);
    }

    #[test]
    fn set_url_on_missing_remote_errors() {
        let mut config = GitConfig::default();
        let err = set_url(
            &mut config,
            "origin",
            SetUrlKind::Fetch,
            SetUrlOp::Set { url: "x" },
        )
        .expect_err("missing remote");
        assert_eq!(err, SetUrlError::RemoteNotFound);
    }
}
