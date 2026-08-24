//! Per-ref rendering context shared by the for-each-ref format renderer.

use super::contents::{ForEachRefContents, ForEachRefPeeledObject};
use super::tracking::{ForEachRefPush, ForEachRefUpstream};
use super::{ForEachRefQuoteMode, ForEachRefTrack, shorten_unambiguous_ref};
use sley_core::{ObjectFormat, ObjectId};
use sley_object::ObjectType;
use sley_odb::FileObjectDatabase;
use std::collections::HashSet;
use std::path::Path;

/// The signature facts the `%(signature[:opt])` atom family renders, mirroring
/// git's `grab_signature` field access. The concrete verifier stays a CLI
/// adapter (GPG/SSH subprocess plumbing); this trait is the sink boundary.
pub trait ForEachRefSignatureVerification {
    /// gpg's human-readable verification output (the bare `%(signature)` atom).
    fn bare_output(&self) -> &[u8];
    /// 'G'/'U'/'B'/'E'/'N' — git downgrades good-but-untrusted to 'U'.
    fn grade_byte(&self) -> u8;
    fn key(&self) -> &str;
    fn signer(&self) -> &str;
    fn fingerprint(&self) -> &str;
    fn primary_fingerprint(&self) -> &str;
    fn trust(&self) -> &str;
}

/// Identity rewriting for the `mailmap` atom options. The mailmap parser
/// remains a CLI adapter; ref-filter only needs the rewrite.
pub trait ForEachRefMailmapRewrite {
    fn rewrite_identity(&self, identity: &[u8]) -> (Vec<u8>, Vec<u8>);
}

pub struct ForEachRefFormatContext<'a> {
    pub git_dir: &'a Path,
    pub db: &'a FileObjectDatabase,
    pub format: ObjectFormat,
    pub refname: &'a str,
    pub oid: &'a ObjectId,
    pub deltabase: &'a ObjectId,
    pub object_type: ObjectType,
    pub object_body: &'a [u8],
    pub object_size: usize,
    pub object_disk_size: Option<u64>,
    pub color: bool,
    pub quote: ForEachRefQuoteMode,
    pub objectname_abbrev: Option<usize>,
    pub objectname_candidates: &'a [ObjectId],
    pub worktree_path: Option<&'a str>,
    pub is_head: bool,
    pub symref: Option<&'a str>,
    pub upstream: Option<ForEachRefUpstream>,
    pub push: Option<ForEachRefPush>,
    pub upstream_track: Option<ForEachRefTrack>,
    pub push_track: Option<ForEachRefTrack>,
    pub contents: Option<ForEachRefContents<'a>>,
    pub peeled_object: Option<ForEachRefPeeledObject<'a>>,
    // %(signature*) verification of the ref object and its peeled tag target.
    pub signature: Option<&'a dyn ForEachRefSignatureVerification>,
    pub peeled_signature: Option<&'a dyn ForEachRefSignatureVerification>,
    pub mailmap: &'a dyn ForEachRefMailmapRewrite,
    // All ref names in the store + `core.warnambiguousrefs`, for the
    // `:short` atoms' shorten_unambiguous_ref resolution.
    pub ref_names: &'a HashSet<String>,
    pub warn_ambiguous_refs: bool,
}

impl ForEachRefFormatContext<'_> {
    /// Shorten a fully-qualified refname to its unambiguous abbreviation, the
    /// way git's `%(refname:short)` / `%(symref:short)` / `%(upstream:short)` do.
    pub fn shorten_ref(&self, refname: &str) -> String {
        shorten_unambiguous_ref(refname, self.warn_ambiguous_refs, |candidate| {
            self.ref_names.contains(candidate)
        })
    }
}
