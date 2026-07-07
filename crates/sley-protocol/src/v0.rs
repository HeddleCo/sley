use sley_core::{Capability, GitError, ObjectFormat, ObjectId, Result};
use std::io::{Read, Write};

use crate::pktline::{
    FetchHeadRecord, FetchRefUpdate, PktLineFrame, ProtocolVersion, RefSpec,
    line_from_str, parse_oid_argument, parse_protocol_v2_line_text,
    read_pkt_line_frames_until_flush, refspec_map_source, refspec_matches_source, trim_trailing_lf,
    validate_capability_field, validate_fetch_head_description_field,
    validate_fetch_head_line, validate_protocol_v2_token, validate_refspec_shape,
    write_pkt_line_payload,
};

use crate::v1::{encode_v1_version_frame, is_v1_version_payload, write_v1_version_line};

pub fn fetch_head_ref_description(refname: &str) -> Result<String> {
    validate_fetch_head_description_field(refname)?;
    // Mirror git's `kind`/`what` split in builtin/fetch.c: `HEAD` yields an empty
    // note (no `'…' of` prefix at all), the standard ref namespaces get their
    // kind word, and any other ref name is quoted bare.
    if refname == "HEAD" {
        Ok(String::new())
    } else if let Some(branch) = refname.strip_prefix("refs/heads/") {
        Ok(format!("branch '{branch}'"))
    } else if let Some(tag) = refname.strip_prefix("refs/tags/") {
        Ok(format!("tag '{tag}'"))
    } else if let Some(rest) = refname.strip_prefix("refs/remotes/") {
        Ok(format!("remote-tracking branch '{rest}'"))
    } else {
        Ok(format!("'{refname}'"))
    }
}

pub fn fetch_head_remote_description(refname: &str, remote: &str) -> Result<String> {
    validate_fetch_head_description_field(remote)?;
    // git only appends `of <url>` when the note (`what`) is non-empty; a bare
    // `HEAD` fetch records just the URL with an empty description.
    let what = fetch_head_ref_description(refname)?;
    if what.is_empty() {
        Ok(remote.to_string())
    } else {
        Ok(format!("{what} of {remote}"))
    }
}

pub fn parse_fetch_head(format: ObjectFormat, input: &[u8]) -> Result<Vec<FetchHeadRecord>> {
    if input.is_empty() {
        return Ok(Vec::new());
    }
    input
        .split_inclusive(|byte| *byte == b'\n')
        .map(|line| parse_fetch_head_record(format, line))
        .collect()
}

pub fn encode_fetch_head(records: &[FetchHeadRecord]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for record in records {
        validate_fetch_head_description_field(&record.description)?;
        out.extend_from_slice(record.oid.to_string().as_bytes());
        out.push(b'\t');
        if record.not_for_merge {
            out.extend_from_slice(b"not-for-merge");
        }
        out.push(b'\t');
        out.extend_from_slice(record.description.as_bytes());
        out.push(b'\n');
    }
    Ok(out)
}

pub fn read_fetch_head(
    format: ObjectFormat,
    reader: &mut impl Read,
) -> Result<Vec<FetchHeadRecord>> {
    let mut input = Vec::new();
    reader.read_to_end(&mut input)?;
    parse_fetch_head(format, &input)
}

pub fn write_fetch_head(writer: &mut impl Write, records: &[FetchHeadRecord]) -> Result<()> {
    for record in records {
        validate_fetch_head_description_field(&record.description)?;
        writer.write_all(record.oid.to_string().as_bytes())?;
        writer.write_all(b"\t")?;
        if record.not_for_merge {
            writer.write_all(b"not-for-merge")?;
        }
        writer.write_all(b"\t")?;
        writer.write_all(record.description.as_bytes())?;
        writer.write_all(b"\n")?;
    }
    Ok(())
}

/// Match an abbreviated refspec source against the advertised refs the way
/// upstream's `find_ref_by_name_abbrev` (remote.c) does: score each
/// advertisement with `refname_match`'s `ref_rev_parse_rules` (exact name
/// first, then `refs/<name>`, `refs/tags/<name>`, `refs/heads/<name>`,
/// `refs/remotes/<name>`, `refs/remotes/<name>/HEAD`) and keep the best.
fn find_advertised_ref_by_name_abbrev<'a>(
    refs: &'a [RefAdvertisement],
    name: &str,
) -> Option<&'a RefAdvertisement> {
    let mut best: Option<(&RefAdvertisement, usize)> = None;
    for reference in refs {
        let score = fetch_refname_match_score(name, &reference.name);
        if score > best.map(|(_, score)| score).unwrap_or(0) {
            best = Some((reference, score));
        }
    }
    best.map(|(reference, _)| reference)
}

/// `refname_match` (refs.c): non-zero when `abbrev` can mean `full`, with the
/// magnitude giving disambiguation precedence (earlier rules win).
fn fetch_refname_match_score(abbrev: &str, full: &str) -> usize {
    let expansions = [
        abbrev.to_string(),
        format!("refs/{abbrev}"),
        format!("refs/tags/{abbrev}"),
        format!("refs/heads/{abbrev}"),
        format!("refs/remotes/{abbrev}"),
        format!("refs/remotes/{abbrev}/HEAD"),
    ];
    for (index, candidate) in expansions.iter().enumerate() {
        if candidate == full {
            return expansions.len() - index;
        }
    }
    0
}

/// Whether `abbrev` (a possibly-abbreviated ref like `three` or `refs/heads/main`)
/// matches the full ref `full` under git's `ref_rev_parse_rules` expansion, the
/// way `refname_match`/`branch_merge_matches` (remote.c) compare a configured
/// `branch.<name>.merge` value against an advertised ref name.
pub fn refname_matches(abbrev: &str, full: &str) -> bool {
    fetch_refname_match_score(abbrev, full) > 0
}

/// Qualify a fetch refspec destination the way upstream's `get_local_ref`
/// (remote.c) does: `refs/...` stays as-is, `heads/`, `tags/` and `remotes/`
/// gain a `refs/` prefix, and anything else lands under `refs/heads/`.
fn fetch_local_ref_name(name: &str) -> String {
    if name.starts_with("refs/") {
        name.to_string()
    } else if name.starts_with("heads/")
        || name.starts_with("tags/")
        || name.starts_with("remotes/")
    {
        format!("refs/{name}")
    } else {
        format!("refs/heads/{name}")
    }
}

pub fn plan_fetch_ref_updates(
    refs: &[RefAdvertisement],
    refspecs: &[RefSpec],
    auto_follow_tags: bool,
) -> Result<Vec<FetchRefUpdate>> {
    let negative = refspecs
        .iter()
        .filter(|refspec| refspec.negative)
        .collect::<Vec<_>>();
    let mut updates = Vec::new();
    for refspec in refspecs.iter().filter(|refspec| !refspec.negative) {
        validate_refspec_shape(refspec)?;
        let Some(src) = refspec.src.as_deref() else {
            return Err(GitError::InvalidFormat(
                "fetch refspec is missing a source".into(),
            ));
        };
        if refspec.pattern {
            for reference in refs {
                if refspec_is_excluded(&negative, &reference.name)? {
                    continue;
                }
                if let Some(dst) = refspec_map_source(refspec, &reference.name)? {
                    updates.push(FetchRefUpdate {
                        src: reference.name.clone(),
                        dst: Some(dst),
                        oid: reference.oid,
                        not_for_merge: false,
                        force: refspec.force,
                    });
                }
            }
            continue;
        }
        if refspec_is_excluded(&negative, src)? {
            continue;
        }
        let Some(reference) = find_advertised_ref_by_name_abbrev(refs, src) else {
            return Err(GitError::reference_not_found(format!("remote ref {src}")));
        };
        updates.push(FetchRefUpdate {
            src: reference.name.clone(),
            dst: refspec.dst.as_deref().map(fetch_local_ref_name),
            oid: reference.oid,
            not_for_merge: false,
            force: refspec.force,
        });
    }
    if auto_follow_tags && updates.iter().any(|update| update.dst.is_some()) {
        let fetched_oids = updates.iter().map(|update| update.oid).collect::<Vec<_>>();
        let fetched_srcs = updates
            .iter()
            .map(|update| update.src.clone())
            .collect::<Vec<_>>();
        for reference in refs {
            if reference.name.starts_with("refs/tags/")
                && fetched_oids.iter().any(|oid| oid == &reference.oid)
                && !fetched_srcs.contains(&reference.name)
                && !refspec_is_excluded(&negative, &reference.name)?
            {
                updates.push(FetchRefUpdate {
                    src: reference.name.clone(),
                    dst: Some(reference.name.clone()),
                    oid: reference.oid,
                    not_for_merge: true,
                    force: false,
                });
            }
        }
    }
    Ok(updates)
}

pub fn fetch_ref_updates_to_fetch_head(
    updates: &[FetchRefUpdate],
    remote: &str,
) -> Result<Vec<FetchHeadRecord>> {
    updates
        .iter()
        .map(|update| {
            Ok(FetchHeadRecord {
                oid: update.oid,
                not_for_merge: update.not_for_merge,
                description: fetch_head_remote_description(&update.src, remote)?,
            })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportHandshake {
    pub protocol: ProtocolVersion,
    pub capabilities: Vec<Capability>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefAdvertisement {
    pub oid: ObjectId,
    pub name: String,
    pub capabilities: Vec<Capability>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DumbHttpRefRecord {
    pub oid: ObjectId,
    pub name: String,
    pub peeled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DumbHttpPackRecord {
    pub hash: ObjectId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefAdvertisementSet {
    pub protocol: ProtocolVersion,
    pub refs: Vec<RefAdvertisement>,
    pub shallow: Vec<ObjectId>,
}
pub fn parse_capabilities(input: &[u8]) -> Result<Vec<Capability>> {
    let input = trim_trailing_lf(input);
    if input.is_empty() {
        return Ok(Vec::new());
    }
    let text =
        std::str::from_utf8(input).map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    text.split(' ')
        .map(parse_capability_token)
        .collect::<Result<Vec<_>>>()
}

pub fn encode_capabilities(capabilities: &[Capability]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for (idx, capability) in capabilities.iter().enumerate() {
        validate_capability_field("capability name", &capability.name)?;
        if idx != 0 {
            out.push(b' ');
        }
        out.extend_from_slice(capability.name.as_bytes());
        if let Some(value) = &capability.value {
            validate_capability_field("capability value", value)?;
            out.push(b'=');
            out.extend_from_slice(value.as_bytes());
        }
    }
    Ok(out)
}

pub fn parse_ref_advertisement(format: ObjectFormat, payload: &[u8]) -> Result<RefAdvertisement> {
    let payload = trim_trailing_lf(payload);
    let (reference, capabilities) = match payload.iter().position(|byte| *byte == 0) {
        Some(idx) => (&payload[..idx], parse_capabilities(&payload[idx + 1..])?),
        None => (payload, Vec::new()),
    };
    let text =
        std::str::from_utf8(reference).map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    let (oid, name) = text
        .split_once(' ')
        .ok_or_else(|| GitError::InvalidFormat("advertised ref is missing name".into()))?;
    if name.is_empty() {
        return Err(GitError::InvalidFormat(
            "advertised ref name is empty".into(),
        ));
    }
    Ok(RefAdvertisement {
        oid: ObjectId::from_hex(format, oid)?,
        name: name.to_string(),
        capabilities,
    })
}

pub fn encode_ref_advertisement(advertisement: &RefAdvertisement) -> Result<Vec<u8>> {
    validate_protocol_v2_token("advertised ref name", &advertisement.name)?;
    let mut out = advertisement.oid.to_string().into_bytes();
    out.push(b' ');
    out.extend_from_slice(advertisement.name.as_bytes());
    if !advertisement.capabilities.is_empty() {
        out.push(0);
        out.extend_from_slice(&encode_capabilities(&advertisement.capabilities)?);
    }
    out.push(b'\n');
    Ok(out)
}

pub fn parse_ref_advertisements(
    format: ObjectFormat,
    frames: &[PktLineFrame],
) -> Result<Vec<RefAdvertisement>> {
    Ok(parse_ref_advertisement_set(format, frames)?.refs)
}

pub fn parse_ref_advertisement_set(
    format: ObjectFormat,
    frames: &[PktLineFrame],
) -> Result<RefAdvertisementSet> {
    let mut set = RefAdvertisementSet {
        protocol: ProtocolVersion::V0,
        refs: Vec::new(),
        shallow: Vec::new(),
    };
    let mut saw_flush = false;
    let mut in_shallow = false;
    for (idx, frame) in frames.iter().enumerate() {
        match frame {
            PktLineFrame::Data(payload) if !saw_flush => {
                let trimmed = trim_trailing_lf(payload);
                if is_v1_version_payload(payload) {
                    if idx != 0 {
                        return Err(GitError::InvalidFormat(
                            "advertised ref protocol version must be the first line".into(),
                        ));
                    }
                    set.protocol = ProtocolVersion::V1;
                    continue;
                }
                if trimmed.starts_with(b"version ") {
                    return Err(GitError::InvalidFormat(
                        "unsupported advertised ref protocol version".into(),
                    ));
                }
                if trimmed.starts_with(b"shallow ") {
                    if set.refs.is_empty() {
                        return Err(GitError::InvalidFormat(
                            "advertised shallow refs must follow advertised refs".into(),
                        ));
                    }
                    let text = parse_protocol_v2_line_text("advertised shallow ref", payload)?;
                    set.shallow.push(parse_oid_argument(
                        format,
                        "advertised shallow ref",
                        text,
                        "shallow ",
                    )?);
                    in_shallow = true;
                    continue;
                }
                if in_shallow {
                    return Err(GitError::InvalidFormat(
                        "advertised refs must not follow shallow refs".into(),
                    ));
                }
                let advertisement = parse_ref_advertisement(format, payload)?;
                if !set.refs.is_empty() && !advertisement.capabilities.is_empty() {
                    return Err(GitError::InvalidFormat(
                        "advertised ref capabilities must appear on the first ref".into(),
                    ));
                }
                set.refs.push(advertisement);
            }
            PktLineFrame::Data(_) => {
                return Err(GitError::InvalidFormat(
                    "advertised ref stream has data after flush".into(),
                ));
            }
            PktLineFrame::Flush => {
                saw_flush = true;
                if idx + 1 != frames.len() {
                    return Err(GitError::InvalidFormat(
                        "advertised ref stream has frames after flush".into(),
                    ));
                }
            }
            PktLineFrame::Delimiter | PktLineFrame::ResponseEnd => {
                return Err(GitError::InvalidFormat(
                    "advertised ref stream contains a non-flush control packet".into(),
                ));
            }
        }
    }
    if !saw_flush {
        return Err(GitError::InvalidFormat(
            "advertised ref stream missing flush".into(),
        ));
    }
    Ok(set)
}

pub fn encode_ref_advertisements(advertisements: &[RefAdvertisement]) -> Result<Vec<PktLineFrame>> {
    encode_ref_advertisement_set(&RefAdvertisementSet {
        protocol: ProtocolVersion::V0,
        refs: advertisements.to_vec(),
        shallow: Vec::new(),
    })
}

pub fn encode_ref_advertisement_set(set: &RefAdvertisementSet) -> Result<Vec<PktLineFrame>> {
    let mut frames = Vec::new();
    match set.protocol {
        ProtocolVersion::V0 => {}
        ProtocolVersion::V1 => frames.push(encode_v1_version_frame()?),
        ProtocolVersion::V2 => {
            return Err(GitError::InvalidFormat(
                "protocol v2 does not use v0/v1 advertised-ref streams".into(),
            ));
        }
    }
    if set.refs.is_empty() && !set.shallow.is_empty() {
        return Err(GitError::InvalidFormat(
            "advertised shallow refs require advertised refs".into(),
        ));
    }
    for (idx, advertisement) in set.refs.iter().enumerate() {
        if idx != 0 && !advertisement.capabilities.is_empty() {
            return Err(GitError::InvalidFormat(
                "advertised ref capabilities must appear on the first ref".into(),
            ));
        }
        frames.push(PktLineFrame::data(encode_ref_advertisement(
            advertisement,
        )?)?);
    }
    for oid in &set.shallow {
        frames.push(PktLineFrame::data(line_from_str(&format!(
            "shallow {oid}"
        )))?);
    }
    frames.push(PktLineFrame::Flush);
    Ok(frames)
}

pub fn read_ref_advertisements(
    format: ObjectFormat,
    reader: &mut impl Read,
) -> Result<Vec<RefAdvertisement>> {
    let frames = read_pkt_line_frames_until_flush(reader)?;
    parse_ref_advertisements(format, &frames)
}

pub fn read_ref_advertisement_set(
    format: ObjectFormat,
    reader: &mut impl Read,
) -> Result<RefAdvertisementSet> {
    let frames = read_pkt_line_frames_until_flush(reader)?;
    parse_ref_advertisement_set(format, &frames)
}

pub fn write_ref_advertisements(
    writer: &mut impl Write,
    advertisements: &[RefAdvertisement],
) -> Result<()> {
    write_ref_advertisement_stream(writer, ProtocolVersion::V0, advertisements, &[])
}

pub fn write_ref_advertisement_set(
    writer: &mut impl Write,
    set: &RefAdvertisementSet,
) -> Result<()> {
    write_ref_advertisement_stream(writer, set.protocol, &set.refs, &set.shallow)
}

fn write_ref_advertisement_stream(
    writer: &mut impl Write,
    protocol: ProtocolVersion,
    refs: &[RefAdvertisement],
    shallow: &[ObjectId],
) -> Result<()> {
    match protocol {
        ProtocolVersion::V0 => {}
        ProtocolVersion::V1 => write_v1_version_line(writer)?,
        ProtocolVersion::V2 => {
            return Err(GitError::InvalidFormat(
                "protocol v2 does not use v0/v1 advertised-ref streams".into(),
            ));
        }
    }
    if refs.is_empty() && !shallow.is_empty() {
        return Err(GitError::InvalidFormat(
            "advertised shallow refs require advertised refs".into(),
        ));
    }
    for (idx, advertisement) in refs.iter().enumerate() {
        if idx != 0 && !advertisement.capabilities.is_empty() {
            return Err(GitError::InvalidFormat(
                "advertised ref capabilities must appear on the first ref".into(),
            ));
        }
        write_pkt_line_payload(writer, &encode_ref_advertisement(advertisement)?)?;
    }
    for oid in shallow {
        write_pkt_line_payload(writer, &line_from_str(&format!("shallow {oid}")))?;
    }
    writer.write_all(b"0000")?;
    Ok(())
}

pub fn parse_dumb_http_info_refs(
    format: ObjectFormat,
    input: &[u8],
) -> Result<Vec<DumbHttpRefRecord>> {
    if input.is_empty() {
        return Ok(Vec::new());
    }
    input
        .split_inclusive(|byte| *byte == b'\n')
        .map(|line| parse_dumb_http_info_ref_record(format, line))
        .collect()
}

pub fn encode_dumb_http_info_refs(records: &[DumbHttpRefRecord]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for record in records {
        validate_dumb_http_ref_name(&record.name)?;
        out.extend_from_slice(record.oid.to_string().as_bytes());
        out.push(b'\t');
        out.extend_from_slice(record.name.as_bytes());
        if record.peeled {
            out.extend_from_slice(b"^{}");
        }
        out.push(b'\n');
    }
    Ok(out)
}

pub fn read_dumb_http_info_refs(
    format: ObjectFormat,
    reader: &mut impl Read,
) -> Result<Vec<DumbHttpRefRecord>> {
    let mut input = Vec::new();
    reader.read_to_end(&mut input)?;
    parse_dumb_http_info_refs(format, &input)
}

pub fn write_dumb_http_info_refs(
    writer: &mut impl Write,
    records: &[DumbHttpRefRecord],
) -> Result<()> {
    for record in records {
        validate_dumb_http_ref_name(&record.name)?;
        writer.write_all(record.oid.to_string().as_bytes())?;
        writer.write_all(b"\t")?;
        writer.write_all(record.name.as_bytes())?;
        if record.peeled {
            writer.write_all(b"^{}")?;
        }
        writer.write_all(b"\n")?;
    }
    Ok(())
}

pub fn parse_dumb_http_alternates(input: &[u8]) -> Result<Vec<String>> {
    if input.is_empty() {
        return Ok(Vec::new());
    }
    input
        .split_inclusive(|byte| *byte == b'\n')
        .map(parse_dumb_http_alternate)
        .collect()
}

pub fn encode_dumb_http_alternates(alternates: &[String]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for alternate in alternates {
        validate_dumb_http_alternate(alternate)?;
        out.extend_from_slice(alternate.as_bytes());
        out.push(b'\n');
    }
    Ok(out)
}

pub fn read_dumb_http_alternates(reader: &mut impl Read) -> Result<Vec<String>> {
    let mut input = Vec::new();
    reader.read_to_end(&mut input)?;
    parse_dumb_http_alternates(&input)
}

pub fn write_dumb_http_alternates(writer: &mut impl Write, alternates: &[String]) -> Result<()> {
    for alternate in alternates {
        validate_dumb_http_alternate(alternate)?;
        writer.write_all(alternate.as_bytes())?;
        writer.write_all(b"\n")?;
    }
    Ok(())
}

pub fn parse_dumb_http_packs(
    format: ObjectFormat,
    input: &[u8],
) -> Result<Vec<DumbHttpPackRecord>> {
    if input.is_empty() {
        return Ok(Vec::new());
    }
    input
        .split_inclusive(|byte| *byte == b'\n')
        .map(|line| parse_dumb_http_pack_record(format, line))
        .collect()
}

pub fn encode_dumb_http_packs(records: &[DumbHttpPackRecord]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for record in records {
        out.extend_from_slice(format!("P pack-{}.pack\n", record.hash).as_bytes());
    }
    Ok(out)
}

pub fn read_dumb_http_packs(
    format: ObjectFormat,
    reader: &mut impl Read,
) -> Result<Vec<DumbHttpPackRecord>> {
    let mut input = Vec::new();
    reader.read_to_end(&mut input)?;
    parse_dumb_http_packs(format, &input)
}

pub fn write_dumb_http_packs(
    writer: &mut impl Write,
    records: &[DumbHttpPackRecord],
) -> Result<()> {
    for record in records {
        writer.write_all(format!("P pack-{}.pack\n", record.hash).as_bytes())?;
    }
    Ok(())
}
fn parse_capability_token(token: &str) -> Result<Capability> {
    if token.is_empty() {
        return Err(GitError::InvalidFormat("empty capability token".into()));
    }
    let (name, value) = token
        .split_once('=')
        .map_or((token, None), |(name, value)| (name, Some(value)));
    validate_capability_field("capability name", name)?;
    if let Some(value) = value {
        validate_capability_field("capability value", value)?;
    }
    Ok(Capability {
        name: name.to_string(),
        value: value.map(str::to_string),
    })
}

fn parse_fetch_head_record(format: ObjectFormat, line: &[u8]) -> Result<FetchHeadRecord> {
    validate_fetch_head_line(line)?;
    let line = trim_trailing_lf(line);
    let mut fields = line.splitn(3, |byte| *byte == b'\t');
    let oid = fields
        .next()
        .ok_or_else(|| GitError::InvalidFormat("FETCH_HEAD record is missing oid".into()))?;
    let merge_marker = fields.next().ok_or_else(|| {
        GitError::InvalidFormat("FETCH_HEAD record is missing merge marker".into())
    })?;
    let description = fields.next().ok_or_else(|| {
        GitError::InvalidFormat("FETCH_HEAD record is missing description".into())
    })?;
    let oid = std::str::from_utf8(oid).map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    validate_protocol_v2_token("FETCH_HEAD oid", oid)?;
    let not_for_merge = match merge_marker {
        b"" => false,
        b"not-for-merge" => true,
        _ => {
            return Err(GitError::InvalidFormat(
                "FETCH_HEAD record has invalid merge marker".into(),
            ));
        }
    };
    let description =
        std::str::from_utf8(description).map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    validate_fetch_head_description_field(description)?;
    Ok(FetchHeadRecord {
        oid: ObjectId::from_hex(format, oid)?,
        not_for_merge,
        description: description.to_string(),
    })
}

fn refspec_is_excluded(negative: &[&RefSpec], source: &str) -> Result<bool> {
    for refspec in negative {
        if refspec_matches_source(refspec, source)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn parse_dumb_http_info_ref_record(format: ObjectFormat, line: &[u8]) -> Result<DumbHttpRefRecord> {
    validate_dumb_http_info_ref_line(line)?;
    let line = trim_trailing_lf(line);
    let tab = line
        .iter()
        .position(|byte| *byte == b'\t')
        .ok_or_else(|| GitError::InvalidFormat("dumb HTTP ref record is missing name".into()))?;
    let (oid, name) = (&line[..tab], &line[tab + 1..]);
    let oid = std::str::from_utf8(oid).map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    validate_protocol_v2_token("dumb HTTP ref oid", oid)?;
    let name = std::str::from_utf8(name).map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    let (name, peeled) = name
        .strip_suffix("^{}")
        .map_or((name, false), |name| (name, true));
    validate_dumb_http_ref_name(name)?;
    Ok(DumbHttpRefRecord {
        oid: ObjectId::from_hex(format, oid)?,
        name: name.to_string(),
        peeled,
    })
}

fn parse_dumb_http_alternate(line: &[u8]) -> Result<String> {
    validate_dumb_http_alternate_line(line)?;
    let line = trim_trailing_lf(line);
    let alternate =
        std::str::from_utf8(line).map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    validate_dumb_http_alternate(alternate)?;
    Ok(alternate.to_string())
}

fn parse_dumb_http_pack_record(format: ObjectFormat, line: &[u8]) -> Result<DumbHttpPackRecord> {
    validate_dumb_http_info_ref_line(line)?;
    let line = parse_protocol_v2_line_text("dumb HTTP pack record", line)?;
    let pack_name = line
        .strip_prefix("P ")
        .ok_or_else(|| GitError::InvalidFormat("dumb HTTP pack record must start with P".into()))?;
    let hash = pack_name
        .strip_prefix("pack-")
        .and_then(|value| value.strip_suffix(".pack"))
        .ok_or_else(|| GitError::InvalidFormat("invalid dumb HTTP pack name".into()))?;
    validate_protocol_v2_token("dumb HTTP pack hash", hash)?;
    Ok(DumbHttpPackRecord {
        hash: ObjectId::from_hex(format, hash)?,
    })
}

fn validate_dumb_http_info_ref_line(value: &[u8]) -> Result<()> {
    if value.is_empty() {
        return Err(GitError::InvalidFormat(
            "dumb HTTP ref record is empty".into(),
        ));
    }
    if !value.ends_with(b"\n") {
        return Err(GitError::InvalidFormat(
            "dumb HTTP ref record missing LF".into(),
        ));
    }
    if value.iter().any(|byte| matches!(*byte, b'\r' | 0)) {
        return Err(GitError::InvalidFormat(
            "dumb HTTP ref record contains a delimiter byte".into(),
        ));
    }
    Ok(())
}

fn validate_dumb_http_ref_name(value: &str) -> Result<()> {
    validate_protocol_v2_token("dumb HTTP ref name", value)?;
    if value.ends_with("^{}") {
        return Err(GitError::InvalidFormat(
            "dumb HTTP ref name must not include peeled suffix".into(),
        ));
    }
    Ok(())
}

fn validate_dumb_http_alternate_line(value: &[u8]) -> Result<()> {
    if value.is_empty() {
        return Err(GitError::InvalidFormat(
            "dumb HTTP alternate is empty".into(),
        ));
    }
    if !value.ends_with(b"\n") {
        return Err(GitError::InvalidFormat(
            "dumb HTTP alternate missing LF".into(),
        ));
    }
    if value.iter().any(|byte| matches!(*byte, b'\r' | 0)) {
        return Err(GitError::InvalidFormat(
            "dumb HTTP alternate contains a delimiter byte".into(),
        ));
    }
    Ok(())
}

fn validate_dumb_http_alternate(value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(GitError::InvalidFormat(
            "dumb HTTP alternate is empty".into(),
        ));
    }
    if value.bytes().any(|byte| matches!(byte, b'\n' | b'\r' | 0)) {
        return Err(GitError::InvalidFormat(
            "dumb HTTP alternate contains a delimiter byte".into(),
        ));
    }
    Ok(())
}

