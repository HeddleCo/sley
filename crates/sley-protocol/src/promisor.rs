use sley_core::{GitError, Result};

/// One remote described by the protocol-v2 `promisor-remote` capability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromisorRemoteAdvertisement {
    pub name: String,
    pub url: String,
    pub partial_clone_filter: Option<String>,
    pub token: Option<String>,
}

/// Encode the server capability value, preserving remote order and the
/// mandatory `name,url` field order.
pub fn encode_promisor_remote_advertisement(
    remotes: &[PromisorRemoteAdvertisement],
) -> Result<String> {
    if remotes.is_empty() {
        return Err(GitError::InvalidFormat(
            "promisor-remote advertisement is empty".into(),
        ));
    }
    remotes
        .iter()
        .map(|remote| {
            if remote.name.is_empty() || remote.url.is_empty() {
                return Err(GitError::InvalidFormat(
                    "promisor remote requires a name and URL".into(),
                ));
            }
            let mut fields = vec![
                format!("name={}", percent_encode(&remote.name)),
                format!("url={}", percent_encode(&remote.url)),
            ];
            if let Some(filter) = &remote.partial_clone_filter {
                fields.push(format!("partialCloneFilter={}", percent_encode(filter)));
            }
            if let Some(token) = &remote.token {
                fields.push(format!("token={}", percent_encode(token)));
            }
            Ok(fields.join(","))
        })
        .collect::<Result<Vec<_>>>()
        .map(|entries| entries.join(";"))
}

/// Parse a server `promisor-remote=<value>` capability. Unknown optional fields
/// are ignored for forward compatibility; duplicate known fields are rejected.
pub fn parse_promisor_remote_advertisement(
    value: &str,
) -> Result<Vec<PromisorRemoteAdvertisement>> {
    if value.is_empty() {
        return Err(GitError::InvalidFormat(
            "promisor-remote advertisement is empty".into(),
        ));
    }
    value
        .split(';')
        .map(|entry| {
            let mut name = None;
            let mut url = None;
            let mut partial_clone_filter = None;
            let mut token = None;
            for field in entry.split(',') {
                let Some((key, encoded)) = field.split_once('=') else {
                    return Err(GitError::InvalidFormat(format!(
                        "invalid promisor remote field {field}"
                    )));
                };
                let decoded = percent_decode(encoded)?;
                let slot = match key {
                    "name" => &mut name,
                    "url" => &mut url,
                    "partialCloneFilter" => &mut partial_clone_filter,
                    "token" => &mut token,
                    _ => continue,
                };
                if slot.replace(decoded).is_some() {
                    return Err(GitError::InvalidFormat(format!(
                        "duplicate promisor remote field {key}"
                    )));
                }
            }
            let name = name.filter(|value| !value.is_empty()).ok_or_else(|| {
                GitError::InvalidFormat("promisor remote is missing its name".into())
            })?;
            let url = url.filter(|value| !value.is_empty()).ok_or_else(|| {
                GitError::InvalidFormat("promisor remote is missing its URL".into())
            })?;
            Ok(PromisorRemoteAdvertisement {
                name,
                url,
                partial_clone_filter,
                token,
            })
        })
        .collect()
}

/// Encode the client reply value from accepted remote names.
pub fn encode_promisor_remote_reply(names: &[String]) -> Result<Option<String>> {
    if names.is_empty() {
        return Ok(None);
    }
    if names.iter().any(|name| name.is_empty()) {
        return Err(GitError::InvalidFormat(
            "accepted promisor remote name is empty".into(),
        ));
    }
    Ok(Some(
        names
            .iter()
            .map(|name| percent_encode(name))
            .collect::<Vec<_>>()
            .join(";"),
    ))
}

pub fn parse_promisor_remote_reply(value: &str) -> Result<Vec<String>> {
    if value.is_empty() {
        return Err(GitError::InvalidFormat(
            "promisor-remote reply is empty".into(),
        ));
    }
    value.split(';').map(percent_decode).collect()
}

fn percent_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"_.~/: -".contains(&byte) && byte != b' ' {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn percent_decode(value: &str) -> Result<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            out.push(bytes[index]);
            index += 1;
            continue;
        }
        if index + 2 >= bytes.len() {
            return Err(GitError::InvalidFormat(
                "truncated percent escape in promisor remote".into(),
            ));
        }
        let hex = std::str::from_utf8(&bytes[index + 1..index + 3])
            .map_err(|err| GitError::InvalidFormat(err.to_string()))?;
        let byte = u8::from_str_radix(hex, 16).map_err(|_| {
            GitError::InvalidFormat("invalid percent escape in promisor remote".into())
        })?;
        out.push(byte);
        index += 3;
    }
    String::from_utf8(out).map_err(|err| GitError::InvalidFormat(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn promisor_advertisement_round_trips_fields_and_escapes() {
        let remotes = vec![PromisorRemoteAdvertisement {
            name: "large,objects".into(),
            url: "file:///tmp/space here;objects".into(),
            partial_clone_filter: Some("blob:limit=5k".into()),
            token: Some("a%b".into()),
        }];
        let encoded = encode_promisor_remote_advertisement(&remotes).expect("encode");
        assert_eq!(
            encoded,
            "name=large%2Cobjects,url=file:///tmp/space%20here%3Bobjects,partialCloneFilter=blob:limit%3D5k,token=a%25b"
        );
        assert_eq!(
            parse_promisor_remote_advertisement(&encoded).expect("parse"),
            remotes
        );
    }

    #[test]
    fn promisor_reply_preserves_accepted_order() {
        let names = vec!["second".into(), "first;fallback".into()];
        let encoded = encode_promisor_remote_reply(&names)
            .expect("encode")
            .expect("non-empty reply");
        assert_eq!(encoded, "second;first%3Bfallback");
        assert_eq!(parse_promisor_remote_reply(&encoded).expect("parse"), names);
    }
}
