//! Parsed object contents shared by the for-each-ref atom model.

use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_object::{Commit, EncodedObject, ObjectType, Tag};
use std::borrow::Cow;
use std::io::Write;

#[derive(Clone)]
pub struct ForEachRefContents<'a> {
    pub message: Cow<'a, [u8]>,
    pub tree: Option<ObjectId>,
    pub parents: Vec<ObjectId>,
    pub tag: Option<Cow<'a, [u8]>>,
    pub tag_object_type: Option<ObjectType>,
    pub tag_object: Option<ObjectId>,
    pub author: Option<Cow<'a, [u8]>>,
    pub committer: Option<Cow<'a, [u8]>>,
    pub tagger: Option<Cow<'a, [u8]>>,
    pub creator: Option<Cow<'a, [u8]>>,
}

impl ForEachRefContents<'_> {
    pub fn into_owned(self) -> ForEachRefContents<'static> {
        ForEachRefContents {
            message: Cow::Owned(self.message.into_owned()),
            tree: self.tree,
            parents: self.parents,
            tag: self.tag.map(|tag| Cow::Owned(tag.into_owned())),
            tag_object_type: self.tag_object_type,
            tag_object: self.tag_object,
            author: self.author.map(|author| Cow::Owned(author.into_owned())),
            committer: self
                .committer
                .map(|committer| Cow::Owned(committer.into_owned())),
            tagger: self.tagger.map(|tagger| Cow::Owned(tagger.into_owned())),
            creator: self.creator.map(|creator| Cow::Owned(creator.into_owned())),
        }
    }
}

pub fn for_each_ref_contents<'a>(
    format: ObjectFormat,
    object: &'a EncodedObject,
) -> Result<Option<ForEachRefContents<'a>>> {
    let contents = match object.object_type {
        ObjectType::Commit => {
            let commit = Commit::parse_ref(format, &object.body)?;
            ForEachRefContents {
                message: Cow::Borrowed(commit.message),
                tree: Some(commit.tree),
                parents: commit.parents,
                tag: None,
                tag_object_type: None,
                tag_object: None,
                author: Some(Cow::Borrowed(commit.author)),
                committer: Some(Cow::Borrowed(commit.committer)),
                tagger: None,
                creator: Some(Cow::Borrowed(commit.committer)),
            }
        }
        ObjectType::Tag => {
            let tag = Tag::parse_ref(format, &object.body)?;
            ForEachRefContents {
                message: Cow::Borrowed(tag.message),
                tree: None,
                parents: Vec::new(),
                tag: Some(Cow::Borrowed(tag.name)),
                tag_object_type: Some(tag.object_type),
                tag_object: Some(tag.object),
                author: None,
                committer: None,
                tagger: tag.tagger.map(Cow::Borrowed),
                creator: tag.tagger.map(Cow::Borrowed),
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(contents))
}

pub fn for_each_ref_validate_tag_pointer(
    tag_oid: &ObjectId,
    contents: &ForEachRefContents<'_>,
    target_oid: &ObjectId,
    target: &EncodedObject,
) -> Result<()> {
    if contents
        .tag_object_type
        .is_some_and(|object_type| object_type != target.object_type)
    {
        eprintln!("error: bad tag pointer to {target_oid} in {tag_oid}");
        return Err(GitError::Exit(128));
    }
    Ok(())
}

pub struct ForEachRefPeeledObject<'a> {
    pub oid: ObjectId,
    pub object_type: ObjectType,
    pub object_body: Cow<'a, [u8]>,
    pub object_size: usize,
    pub object_disk_size: Option<u64>,
    pub tree: Option<ObjectId>,
    pub parents: Vec<ObjectId>,
    pub message: Option<Cow<'a, [u8]>>,
    pub author: Option<Cow<'a, [u8]>>,
    pub committer: Option<Cow<'a, [u8]>>,
    pub creator: Option<Cow<'a, [u8]>>,
}

pub fn write_for_each_ref_contents_lines(
    stdout: &mut impl Write,
    message: &[u8],
    count: usize,
) -> Result<()> {
    let mut lines = message.split(|byte| *byte == b'\n').collect::<Vec<_>>();
    if lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    for (idx, line) in lines.into_iter().take(count).enumerate() {
        if idx > 0 {
            stdout.write_all(b"\n    ")?;
        }
        stdout.write_all(line)?;
    }
    Ok(())
}
