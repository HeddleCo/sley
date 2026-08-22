//! Config policy for protocol-v2 promisor-remote advertisement and acceptance.

use std::env;
use std::fs;
use std::io::Write;
use std::path::Path;

use sley_config::GitConfig;
use sley_config::raw_edit::{ConfigFileWriteOptions, RawConfigEditor, write_config_file_locked};
use sley_config::remotes::remote_names;
use sley_core::{Capability, GitError, Result};
use sley_protocol::{
    PromisorRemoteAdvertisement, encode_promisor_remote_advertisement,
    encode_promisor_remote_reply, parse_promisor_remote_advertisement,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PromisorAcceptPolicy {
    #[default]
    None,
    KnownUrl,
    KnownName,
    All,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PromisorRemoteDecision {
    /// Accepted remotes in server-advertised order.
    pub accepted: Vec<PromisorRemoteAdvertisement>,
    /// Encoded client capability reply, absent when none were accepted.
    pub reply: Option<String>,
    /// Accepted advertised fields that should be persisted in existing client
    /// remote configuration.
    pub stored_fields: Vec<PromisorRemoteFieldUpdate>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromisorRemoteField {
    PartialCloneFilter,
    Token,
}

impl PromisorRemoteField {
    pub const fn config_key(self) -> &'static str {
        match self {
            Self::PartialCloneFilter => "partialCloneFilter",
            Self::Token => "token",
        }
    }

    pub const fn display_name(self) -> &'static str {
        match self {
            Self::PartialCloneFilter => "filter",
            Self::Token => "token",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromisorRemoteFieldUpdate {
    pub remote_name: String,
    pub field: PromisorRemoteField,
    pub previous: Option<String>,
    pub value: String,
}

/// Build the server's capability from configured promisor remotes.
pub fn promisor_remote_server_capability(config: &GitConfig) -> Result<Option<Capability>> {
    if !config
        .get_bool("promisor", None, "advertise")
        .unwrap_or(false)
    {
        return Ok(None);
    }
    let send_fields = configured_fields(config, "sendFields");
    let remotes = configured_promisor_remotes(config, &send_fields);
    if remotes.is_empty() {
        return Ok(None);
    }
    Ok(Some(Capability {
        name: "promisor-remote".into(),
        value: Some(encode_promisor_remote_advertisement(&remotes)?),
    }))
}

/// Apply `promisor.acceptFromServer` to an advertised capability.
pub fn decide_promisor_remote_reply(
    config: &GitConfig,
    capability: &Capability,
) -> Result<PromisorRemoteDecision> {
    if capability.name != "promisor-remote" {
        return Ok(PromisorRemoteDecision::default());
    }
    let Some(value) = capability.value.as_deref() else {
        return Ok(PromisorRemoteDecision::default());
    };
    let policy = promisor_accept_policy(config);
    if policy == PromisorAcceptPolicy::None {
        return Ok(PromisorRemoteDecision::default());
    }
    let checked_fields = configured_fields(config, "checkFields");
    let configured = configured_promisor_remotes(config, &checked_fields);
    let advertised = parse_promisor_remote_advertisement(value)?;
    let accepted = advertised
        .into_iter()
        .filter(|remote| should_accept(policy, remote, &configured, &checked_fields))
        .collect::<Vec<_>>();
    let names = accepted
        .iter()
        .map(|remote| remote.name.clone())
        .collect::<Vec<_>>();
    let stored_fields = planned_stored_fields(config, &accepted);
    Ok(PromisorRemoteDecision {
        accepted,
        reply: encode_promisor_remote_reply(&names)?,
        stored_fields,
    })
}

/// Persist planned accepted-field updates with git's lockfile config writer.
pub fn apply_promisor_remote_field_updates(
    git_dir: &Path,
    updates: &[PromisorRemoteFieldUpdate],
) -> Result<()> {
    if updates.is_empty() {
        return Ok(());
    }
    let path = git_dir.join("config");
    let mut contents = match fs::read(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(err) => return Err(GitError::Io(err.to_string())),
    };
    for update in updates {
        let mut editor = RawConfigEditor::new(
            contents,
            "remote",
            Some(&update.remote_name),
            update.field.config_key(),
        );
        editor.set_multivar(Some(&update.value), None, None, true);
        contents = editor.into_bytes();
    }
    write_config_file_locked(&path, &contents, ConfigFileWriteOptions::default())
        .map_err(|err| GitError::Io(err.to_string()))
}

/// Construct the concrete partial-clone filter for `--filter=auto` from the
/// filters carried by accepted promisor advertisements.
pub fn promisor_remote_auto_filter(
    decision: &PromisorRemoteDecision,
) -> Option<sley_odb::PackObjectFilter> {
    decision
        .accepted
        .iter()
        .filter_map(|remote| remote.partial_clone_filter.as_deref())
        .filter_map(crate::pack_filter_from_spec)
        .reduce(crate::filter::combine_pack_filters)
}

pub fn promisor_accept_policy(config: &GitConfig) -> PromisorAcceptPolicy {
    match config.get("promisor", None, "acceptFromServer") {
        Some(value) if value.eq_ignore_ascii_case("All") => PromisorAcceptPolicy::All,
        Some(value) if value.eq_ignore_ascii_case("KnownName") => PromisorAcceptPolicy::KnownName,
        Some(value) if value.eq_ignore_ascii_case("KnownUrl") => PromisorAcceptPolicy::KnownUrl,
        _ => PromisorAcceptPolicy::None,
    }
}

/// Whether repository config declares at least one promisor object source.
pub fn config_has_promisor_remote(config: &GitConfig) -> bool {
    config
        .get("extensions", None, "partialclone")
        .is_some_and(|value| !value.is_empty())
        || remote_names(config).into_iter().any(|name| {
            config
                .get_bool("remote", Some(&name), "promisor")
                .unwrap_or(false)
                || config
                    .get("remote", Some(&name), "partialCloneFilter")
                    .is_some_and(|value| !value.is_empty())
        })
}

/// Configured promisor remotes in Git's lazy-fetch order.
///
/// Remotes retain config order, except the legacy/default remote named by
/// `extensions.partialClone`, which Git moves to the tail so more specialized
/// promisors are tried first. If that default is not otherwise configured as a
/// promisor, it is still appended as the final fallback.
pub fn configured_promisor_remote_names(config: &GitConfig) -> Vec<String> {
    let mut config_order = Vec::new();
    for section in &config.sections {
        if section.name.eq_ignore_ascii_case("remote")
            && let Some(name) = section.subsection.as_deref()
            && !config_order.iter().any(|existing| existing == name)
        {
            config_order.push(name.to_string());
        }
    }
    let mut names = config_order
        .into_iter()
        .filter(|name| {
            config
                .get_bool("remote", Some(name), "promisor")
                .unwrap_or(false)
                || config
                    .get("remote", Some(name), "partialCloneFilter")
                    .is_some_and(|value| !value.is_empty())
        })
        .collect::<Vec<_>>();
    if let Some(default) = config
        .get("extensions", None, "partialClone")
        .filter(|value| !value.is_empty())
    {
        if let Some(position) = names.iter().position(|name| name == default) {
            let default = names.remove(position);
            names.push(default);
        } else {
            names.push(default.to_string());
        }
    }
    names
}

/// Emit the native equivalent of Git's lazy-promisor `run_command` trace.
///
/// Sley performs this contact in-process; the trace records the equivalent Git
/// operation for `GIT_TRACE` consumers without spawning Git or a helper.
pub(crate) fn trace_promisor_remote_contact(remote_name: &str) {
    let Some(target) = env::var_os("GIT_TRACE") else {
        return;
    };
    let value = target.to_string_lossy();
    if matches!(value.to_ascii_lowercase().as_str(), "" | "0" | "false") {
        return;
    }
    let line = promisor_remote_contact_trace_line(remote_name);
    if matches!(value.to_ascii_lowercase().as_str(), "1" | "2" | "true") {
        eprintln!("{line}");
    } else if Path::new(target.as_os_str()).is_absolute()
        && let Ok(mut file) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(target)
    {
        let _ = writeln!(file, "{line}");
    }
}

fn promisor_remote_contact_trace_line(remote_name: &str) -> String {
    format!(
        "trace: run_command: git -c fetch.negotiationAlgorithm=noop fetch {} --no-tags --no-write-fetch-head --recurse-submodules=no --filter=blob:none --stdin",
        trace_quote_argument(remote_name)
    )
}

/// Trace-style argument rendering (`sq_quote_buf_pretty`): safe arguments stay
/// bare, everything else gets full sq-quote semantics including the `'\!'`
/// bang escape.
fn trace_quote_argument(value: &str) -> String {
    sley_core::text::sq_quote_pretty(value)
}

fn configured_fields(config: &GitConfig, key: &str) -> Vec<String> {
    config
        .get("promisor", None, key)
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|field| {
            field.eq_ignore_ascii_case("partialCloneFilter") || field.eq_ignore_ascii_case("token")
        })
        .map(str::to_string)
        .collect()
}

fn configured_promisor_remotes(
    config: &GitConfig,
    fields: &[String],
) -> Vec<PromisorRemoteAdvertisement> {
    remote_names(config)
        .into_iter()
        .filter(|name| {
            config
                .get_bool("remote", Some(name), "promisor")
                .unwrap_or(false)
                || config
                    .get("remote", Some(name), "partialCloneFilter")
                    .is_some_and(|value| !value.is_empty())
        })
        .filter_map(|name| {
            let url = config.get("remote", Some(&name), "url")?.to_string();
            if url.is_empty() {
                return None;
            }
            let includes = |field: &str| fields.iter().any(|item| item.eq_ignore_ascii_case(field));
            Some(PromisorRemoteAdvertisement {
                partial_clone_filter: includes("partialCloneFilter")
                    .then(|| {
                        config
                            .get("remote", Some(&name), "partialCloneFilter")
                            .map(str::to_string)
                    })
                    .flatten(),
                token: includes("token")
                    .then(|| {
                        config
                            .get("remote", Some(&name), "token")
                            .map(str::to_string)
                    })
                    .flatten(),
                name,
                url,
            })
        })
        .collect()
}

fn planned_stored_fields(
    config: &GitConfig,
    accepted: &[PromisorRemoteAdvertisement],
) -> Vec<PromisorRemoteFieldUpdate> {
    let fields = configured_fields(config, "storeFields");
    if fields.is_empty() {
        return Vec::new();
    }
    let configured = configured_promisor_remotes(config, &fields);
    let mut updates = Vec::new();
    for advertised in accepted {
        let Some(current) = configured
            .iter()
            .find(|remote| remote.name == advertised.name)
        else {
            continue;
        };
        for field_name in &fields {
            let (field, advertised_value, previous) =
                if field_name.eq_ignore_ascii_case("partialCloneFilter") {
                    (
                        PromisorRemoteField::PartialCloneFilter,
                        advertised.partial_clone_filter.as_deref(),
                        current.partial_clone_filter.as_deref(),
                    )
                } else {
                    (
                        PromisorRemoteField::Token,
                        advertised.token.as_deref(),
                        current.token.as_deref(),
                    )
                };
            let Some(value) = advertised_value else {
                continue;
            };
            let valid = match field {
                PromisorRemoteField::PartialCloneFilter => {
                    crate::pack_filter_from_spec(value).is_some()
                }
                PromisorRemoteField::Token => !value.chars().any(char::is_control),
            };
            if valid && previous != Some(value) {
                updates.push(PromisorRemoteFieldUpdate {
                    remote_name: advertised.name.clone(),
                    field,
                    previous: previous.map(str::to_string),
                    value: value.to_string(),
                });
            }
        }
    }
    updates
}

fn should_accept(
    policy: PromisorAcceptPolicy,
    advertised: &PromisorRemoteAdvertisement,
    configured: &[PromisorRemoteAdvertisement],
    checked_fields: &[String],
) -> bool {
    let named = configured
        .iter()
        .find(|remote| remote.name == advertised.name);
    let candidate = match policy {
        PromisorAcceptPolicy::None => return false,
        PromisorAcceptPolicy::All => None,
        PromisorAcceptPolicy::KnownName => match named {
            Some(remote) => Some(remote),
            None => return false,
        },
        PromisorAcceptPolicy::KnownUrl => match named {
            Some(remote) if remote.url == advertised.url => Some(remote),
            _ => return false,
        },
    };
    checked_fields.iter().all(|field| {
        let advertised_value = field_value(advertised, field);
        match candidate {
            Some(configured) => advertised_value == field_value(configured, field),
            None => advertised_value.is_some_and(|value| {
                configured
                    .iter()
                    .any(|remote| field_value(remote, field) == Some(value))
            }),
        }
    })
}

fn field_value<'a>(remote: &'a PromisorRemoteAdvertisement, field: &str) -> Option<&'a str> {
    if field.eq_ignore_ascii_case("partialCloneFilter") {
        remote.partial_clone_filter.as_deref()
    } else if field.eq_ignore_ascii_case("token") {
        remote.token.as_deref()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertises_promisors_in_config_order_with_selected_fields() {
        let config = GitConfig::parse(
            b"[promisor]\n\tadvertise = true\n\tsendFields = partialCloneFilter, token\n\
              [remote \"first\"]\n\tpromisor = true\n\turl = file:///one\n\tpartialCloneFilter = blob:none\n\
              [remote \"ignored\"]\n\turl = file:///ignored\n\
              [remote \"second\"]\n\tpromisor = true\n\turl = file:///two\n\ttoken = secret\n",
        )
        .expect("config");
        let capability = promisor_remote_server_capability(&config)
            .expect("capability")
            .expect("advertised");
        assert_eq!(
            capability.value.as_deref(),
            Some(
                "name=first,url=file:///one,partialCloneFilter=blob:none;name=second,url=file:///two,token=secret"
            )
        );
    }

    #[test]
    fn acceptance_preserves_server_order_and_honors_known_name() {
        let config = GitConfig::parse(
            b"[promisor]\n\tacceptFromServer = KnownName\n\
              [remote \"second\"]\n\tpromisor = true\n\turl = file:///local-two\n\
              [remote \"first\"]\n\tpromisor = true\n\turl = file:///local-one\n",
        )
        .expect("config");
        let capability = Capability {
            name: "promisor-remote".into(),
            value: Some(
                "name=first,url=file:///server-one;name=second,url=file:///server-two".into(),
            ),
        };
        let decision = decide_promisor_remote_reply(&config, &capability).expect("decision");
        assert_eq!(decision.reply.as_deref(), Some("first;second"));
    }

    #[test]
    fn accepted_fields_plan_storage_and_resolve_auto_filter() {
        let config = GitConfig::parse(
            b"[promisor]\n\tacceptFromServer = All\n\tstoreFields = partialCloneFilter\n\
              [remote \"lop\"]\n\tpromisor = true\n\turl = file:///lop\n\tpartialCloneFilter = blob:none\n",
        )
        .expect("config");
        let capability = Capability {
            name: "promisor-remote".into(),
            value: Some(
                "name=lop,url=file:///lop,partialCloneFilter=blob:limit=9500,token=secret".into(),
            ),
        };
        let decision = decide_promisor_remote_reply(&config, &capability).expect("decision");
        assert_eq!(
            decision.stored_fields,
            vec![PromisorRemoteFieldUpdate {
                remote_name: "lop".into(),
                field: PromisorRemoteField::PartialCloneFilter,
                previous: Some("blob:none".into()),
                value: "blob:limit=9500".into(),
            }]
        );
        assert_eq!(
            promisor_remote_auto_filter(&decision),
            Some(sley_odb::PackObjectFilter::BlobLimit(9500))
        );
    }

    #[test]
    fn lazy_fetch_order_moves_default_partial_clone_remote_to_tail() {
        let config = GitConfig::parse(
            b"[extensions]\n\tpartialClone = origin\n\
              [remote \"origin\"]\n\tpromisor = true\n\turl = file:///origin\n\
              [remote \"lop\"]\n\tpromisor = true\n\turl = file:///lop\n\
              [remote \"archive\"]\n\tpartialCloneFilter = blob:none\n\turl = file:///archive\n",
        )
        .expect("config");
        assert_eq!(
            configured_promisor_remote_names(&config),
            vec!["lop", "archive", "origin"]
        );
    }

    #[test]
    fn native_promisor_contact_trace_matches_git_run_command_shape() {
        assert_eq!(
            promisor_remote_contact_trace_line("lop"),
            "trace: run_command: git -c fetch.negotiationAlgorithm=noop fetch lop --no-tags --no-write-fetch-head --recurse-submodules=no --filter=blob:none --stdin"
        );
        assert!(
            !promisor_remote_contact_trace_line("lop").contains("unused_lop"),
            "the trace names only the remote actually selected"
        );
    }
}
