//! Thin-pack completion.

use super::*;

/// A self-contained pack and the v2 index built for its final bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixThinPackBuild {
    /// The completed pack. This is byte-for-byte identical to the input when
    /// the input was already self-contained.
    pub pack: Vec<u8>,
    /// The v2 index build corresponding to `pack`.
    pub index: PackIndexBuild,
    /// External base object ids appended to the pack, in append order.
    pub appended_bases: Vec<ObjectId>,
}

/// Complete a thin pack by appending every external ref-delta base it needs.
///
/// Required bases are written once each as full, non-delta entries, the object
/// count is patched, and the pack trailer and v2 index are rebuilt. If the
/// input already resolves without `external_base`, its bytes are returned
/// unchanged and the resolver is not called.
///
/// An object id already present in the pack body is never appended again. The
/// completed bytes are indexed again without an external resolver, so unusual
/// forward or cyclic ref-delta arrangements cannot use that de-duplication to
/// produce a pack that only sley's permissive resolver accepts.
pub fn fix_thin_pack<F>(
    pack_bytes: &[u8],
    format: ObjectFormat,
    external_base: F,
) -> Result<FixThinPackBuild>
where
    F: FnMut(&ObjectId) -> Result<Option<EncodedObject>>,
{
    fix_thin_pack_with_limits(pack_bytes, format, external_base, PackReadLimits::default())
}

/// [`fix_thin_pack`] with explicit pack parsing and delta-depth limits.
pub fn fix_thin_pack_with_limits<F>(
    pack_bytes: &[u8],
    format: ObjectFormat,
    mut external_base: F,
    limits: PackReadLimits,
) -> Result<FixThinPackBuild>
where
    F: FnMut(&ObjectId) -> Result<Option<EncodedObject>>,
{
    if let Ok(index) = PackIndex::write_v2_for_pack_with_limits(pack_bytes, format, limits) {
        return Ok(FixThinPackBuild {
            pack: pack_bytes.to_vec(),
            index,
            appended_bases: Vec::new(),
        });
    }

    let mut resolved_external = HashMap::<ObjectId, Option<EncodedObject>>::new();
    let mut external_order = Vec::new();
    let thin_index = PackIndex::write_v2_for_pack_with_base_and_limits(
        pack_bytes,
        format,
        |oid| {
            if let Some(object) = resolved_external.get(oid) {
                return Ok(object.clone());
            }
            let object = external_base(oid)?;
            if let Some(object) = &object {
                let actual = object.object_id(format)?;
                if actual != *oid {
                    return Err(GitError::InvalidObject(format!(
                        "external base {oid} resolved to object {actual}"
                    )));
                }
                external_order.push(*oid);
            }
            resolved_external.insert(*oid, object.clone());
            Ok(object)
        },
        limits,
    )?;

    let body_oids = thin_index
        .entries
        .iter()
        .map(|entry| entry.oid)
        .collect::<HashSet<_>>();
    let appended_bases = external_order
        .into_iter()
        .filter(|oid| !body_oids.contains(oid))
        .collect::<Vec<_>>();

    let trailer_len = format.raw_len();
    let trailer_offset = pack_bytes
        .len()
        .checked_sub(trailer_len)
        .ok_or_else(|| GitError::InvalidFormat("pack file too short".into()))?;
    let old_count = u32_be(&pack_bytes[8..12]);
    let append_count = u32::try_from(appended_bases.len())
        .map_err(|_| GitError::InvalidFormat("too many external pack bases".into()))?;
    let new_count = old_count
        .checked_add(append_count)
        .ok_or_else(|| GitError::InvalidFormat("pack object count overflow".into()))?;

    let mut pack = Vec::with_capacity(pack_bytes.len());
    pack.extend_from_slice(&pack_bytes[..trailer_offset]);
    pack[8..12].copy_from_slice(&new_count.to_be_bytes());
    for oid in &appended_bases {
        let object = resolved_external
            .get(oid)
            .and_then(Option::as_ref)
            .ok_or_else(|| GitError::not_found(format!("external pack base {oid}")))?;
        write_entry_header(&mut pack, object.object_type, object.body.len() as u64);
        write_compressed_payload(&mut pack, &object.body, 6)?;
    }
    let checksum = sley_core::digest_bytes(format, &pack)?;
    pack.extend_from_slice(checksum.as_bytes());

    // This is the interoperability gate: the result must resolve with no
    // cross-pack base lookup, regardless of what the first indexing pass used.
    let index = PackIndex::write_v2_for_pack_with_limits(&pack, format, limits)?;
    Ok(FixThinPackBuild {
        pack,
        index,
        appended_bases,
    })
}
