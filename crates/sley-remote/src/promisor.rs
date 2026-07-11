//! Config policy for protocol-v2 promisor-remote advertisement and acceptance.

use sley_config::GitConfig;
use sley_config::remotes::remote_names;
use sley_core::{Capability, Result};
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
    Ok(PromisorRemoteDecision {
        accepted,
        reply: encode_promisor_remote_reply(&names)?,
    })
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
        })
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
}
