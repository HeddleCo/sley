//! `for-each-ref --sort` key extraction for identity and date fields.

use super::contents::ForEachRefContents;
use super::{
    ForEachRefEmailMode, for_each_ref_identity_email, for_each_ref_identity_name,
    for_each_ref_identity_timestamp,
};

#[derive(Clone, Copy)]
pub struct ForEachRefIdentitySortField {
    pub source: ForEachRefIdentitySource,
    pub role: ForEachRefIdentityRole,
    pub part: ForEachRefIdentityPart,
}

#[derive(Clone, Copy)]
pub enum ForEachRefIdentitySource {
    Direct,
    Peeled,
}

#[derive(Clone, Copy)]
pub enum ForEachRefIdentityRole {
    Author,
    Committer,
    Tagger,
    Creator,
}

#[derive(Clone, Copy)]
pub enum ForEachRefIdentityPart {
    Full,
    Name,
    Email,
}

pub fn parse_for_each_ref_identity_sort(
    value: &str,
) -> Option<(ForEachRefIdentitySortField, bool)> {
    let (value, descending) = value
        .strip_prefix('-')
        .map(|value| (value, true))
        .unwrap_or((value, false));
    let (value, source) = value
        .strip_prefix('*')
        .map(|value| (value, ForEachRefIdentitySource::Peeled))
        .unwrap_or((value, ForEachRefIdentitySource::Direct));
    let (role, part) = match value {
        "author" => (ForEachRefIdentityRole::Author, ForEachRefIdentityPart::Full),
        "authorname" => (ForEachRefIdentityRole::Author, ForEachRefIdentityPart::Name),
        "authoremail" => (
            ForEachRefIdentityRole::Author,
            ForEachRefIdentityPart::Email,
        ),
        "committer" => (
            ForEachRefIdentityRole::Committer,
            ForEachRefIdentityPart::Full,
        ),
        "committername" => (
            ForEachRefIdentityRole::Committer,
            ForEachRefIdentityPart::Name,
        ),
        "committeremail" => (
            ForEachRefIdentityRole::Committer,
            ForEachRefIdentityPart::Email,
        ),
        "tagger" => (ForEachRefIdentityRole::Tagger, ForEachRefIdentityPart::Full),
        "taggername" => (ForEachRefIdentityRole::Tagger, ForEachRefIdentityPart::Name),
        "taggeremail" => (
            ForEachRefIdentityRole::Tagger,
            ForEachRefIdentityPart::Email,
        ),
        "creator" => (
            ForEachRefIdentityRole::Creator,
            ForEachRefIdentityPart::Full,
        ),
        _ => return None,
    };
    Some((
        ForEachRefIdentitySortField { source, role, part },
        descending,
    ))
}

pub fn for_each_ref_sort_identity_key(
    contents: Option<&ForEachRefContents<'_>>,
    field: ForEachRefIdentitySortField,
) -> String {
    let identity = match field.role {
        ForEachRefIdentityRole::Author => contents.and_then(|contents| contents.author.as_deref()),
        ForEachRefIdentityRole::Committer => {
            contents.and_then(|contents| contents.committer.as_deref())
        }
        ForEachRefIdentityRole::Tagger => contents.and_then(|contents| contents.tagger.as_deref()),
        ForEachRefIdentityRole::Creator => {
            contents.and_then(|contents| contents.creator.as_deref())
        }
    };
    let value = match field.part {
        ForEachRefIdentityPart::Full => identity,
        ForEachRefIdentityPart::Name => identity.and_then(for_each_ref_identity_name),
        ForEachRefIdentityPart::Email => identity.and_then(|identity| {
            for_each_ref_identity_email(identity, ForEachRefEmailMode::Bracketed)
        }),
    };
    value
        .map(|value| String::from_utf8_lossy(value).into_owned())
        .unwrap_or_default()
}

#[derive(Clone, Copy)]
pub enum ForEachRefDateSortField {
    Author,
    Committer,
    Tagger,
    Creator,
}

pub fn for_each_ref_sort_date_key(
    contents: Option<ForEachRefContents<'_>>,
    field: ForEachRefDateSortField,
) -> i128 {
    let contents = contents.as_ref();
    let identity = match field {
        ForEachRefDateSortField::Author => contents.and_then(|contents| contents.author.as_deref()),
        ForEachRefDateSortField::Committer => {
            contents.and_then(|contents| contents.committer.as_deref())
        }
        ForEachRefDateSortField::Tagger => contents.and_then(|contents| contents.tagger.as_deref()),
        ForEachRefDateSortField::Creator => {
            contents.and_then(|contents| contents.creator.as_deref())
        }
    };
    identity
        .and_then(for_each_ref_identity_timestamp)
        .map(i128::from)
        .unwrap_or(0)
}
