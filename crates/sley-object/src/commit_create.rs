//! Commit object authoring: the [`CommitCreate`] request shape and its
//! serialization.
//!
//! Sunk out of sley-sequencer so commit body building (parent-format
//! validation, header serialization, folded `gpgsig` headers) lives next to
//! [`Commit`] itself. The odb write seam stays on sequencer because
//! `ObjectWriter` is an sley-odb trait and sley-odb depends on this crate.

use sley_core::{GitError, ObjectFormat, ObjectId, Result};

use crate::{Commit, EncodedObject, ObjectType};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitCreate {
    pub tree: ObjectId,
    pub parents: Vec<ObjectId>,
    pub author: Vec<u8>,
    pub committer: Vec<u8>,
    pub message: Vec<u8>,
    /// `encoding` header value (`i18n.commitEncoding`); `None`/UTF-8 omits it.
    pub encoding: Option<Vec<u8>>,
    pub signature: Option<Vec<u8>>,
}

/// Validate and serialize a commit request into an encodable object: parent /
/// tree formats must agree, the canonical commit body is written, and an
/// optional detached signature is folded under git's `gpgsig`
/// (`gpgsig-sha256`) continuation-header convention.
pub fn encode_commit_object(commit: CommitCreate) -> Result<EncodedObject> {
    let format = commit.tree.format();
    for parent in &commit.parents {
        if parent.format() != format {
            return Err(GitError::InvalidObjectId(format!(
                "parent {parent} uses {}, tree uses {}",
                parent.format().name(),
                format.name()
            )));
        }
    }
    let signature = commit.signature;
    let commit = Commit {
        tree: commit.tree,
        parents: commit.parents,
        author: commit.author,
        committer: commit.committer,
        encoding: commit.encoding,
        message: commit.message,
    };
    let mut body = commit.write();
    if let Some(signature) = signature {
        body = commit_body_with_signature(format, &body, &signature);
    }
    Ok(EncodedObject::new(ObjectType::Commit, body))
}

fn commit_body_with_signature(format: ObjectFormat, body: &[u8], signature: &[u8]) -> Vec<u8> {
    let Some(split) = body.windows(2).position(|window| window == b"\n\n") else {
        return body.to_vec();
    };
    let mut out = Vec::with_capacity(body.len() + signature.len() + signature.len() / 70 + 16);
    out.extend_from_slice(&body[..split]);
    out.push(b'\n');
    out.extend_from_slice(match format {
        ObjectFormat::Sha1 => b"gpgsig ",
        ObjectFormat::Sha256 => b"gpgsig-sha256 ",
    });
    append_folded_signature(&mut out, signature);
    out.extend_from_slice(&body[split + 1..]);
    out
}

fn append_folded_signature(out: &mut Vec<u8>, signature: &[u8]) {
    let mut first = true;
    let mut lines = signature.split(|byte| *byte == b'\n').peekable();
    while let Some(line) = lines.next() {
        if line.is_empty() && lines.peek().is_none() && signature.ends_with(b"\n") {
            continue;
        }
        if !first {
            out.push(b' ');
        }
        out.extend_from_slice(line);
        out.push(b'\n');
        first = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format_commit_identity;

    fn sha1(hex: &str) -> ObjectId {
        ObjectId::from_hex(ObjectFormat::Sha1, hex).expect("test operation should succeed")
    }

    fn sample(signature: Option<Vec<u8>>) -> CommitCreate {
        let identity =
            format_commit_identity("Example User", "example@example.invalid", "@0 +0000")
                .expect("test operation should succeed");
        CommitCreate {
            tree: sha1("4b825dc642cb6eb9a060e54bf8d69288fbee4904"),
            parents: Vec::new(),
            author: identity.clone(),
            committer: identity,
            message: b"initial subject\n".to_vec(),
            encoding: None,
            signature,
        }
    }

    #[test]
    fn unsigned_encoding_matches_the_known_commit_bytes() {
        let object = encode_commit_object(sample(None)).expect("test operation should succeed");
        assert_eq!(object.object_type, ObjectType::Commit);
        let text = String::from_utf8_lossy(&object.body);
        assert_eq!(
            text,
            "tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
             author Example User <example@example.invalid> 0 +0000\n\
             committer Example User <example@example.invalid> 0 +0000\n\
             \n\
             initial subject\n"
        );
    }

    #[test]
    fn signatures_fold_into_continuation_header_lines() {
        let signature = b"-----BEGIN PGP SIGNATURE-----\nfirst line\nsecond line\n-----END PGP SIGNATURE-----\n".to_vec();
        let object =
            encode_commit_object(sample(Some(signature))).expect("test operation should succeed");
        let text = String::from_utf8_lossy(&object.body);
        assert!(text.contains(
            "gpgsig -----BEGIN PGP SIGNATURE-----\n \
             first line\n \
             second line\n \
             -----END PGP SIGNATURE-----\n"
        ));
    }

    #[test]
    fn parent_format_mismatch_is_rejected_before_writing() {
        let mut commit = sample(None);
        commit.parents.push(ObjectId::null(ObjectFormat::Sha256));
        let err = match encode_commit_object(commit) {
            Err(err) => err,
            Ok(_) => panic!("expected parent format mismatch to be rejected"),
        };
        assert!(err.to_string().contains("uses sha256, tree uses sha1"));
    }
}
