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
/// configured, then applying `pushInsteadOf`/`insteadOf` rewriting.
pub fn resolve_remote_push_url(config: &GitConfig, remote: &str) -> String {
    let url = remote_config_values(config, remote, "pushurl")
        .into_iter()
        .next()
        .or_else(|| {
            remote_config_values(config, remote, "url")
                .into_iter()
                .next()
        })
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
