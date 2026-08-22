//! Local-path clone into a bare repository: object transfer, ref copy, and
//! source-HEAD mirroring, without any transport helper.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use sley::plumbing::sley_object::{Commit, EncodedObject};
use sley::{
    EntryKind, FullName, GitObjectType, LocalCloneOptions, ObjectId, ReferenceTarget, Repository,
    TreeEditor,
};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "sley-local-clone-{label}-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Write one commit (with a single blob) on top of `parent` and return its id.
fn write_commit(repo: &Repository, parent: Option<&ObjectId>, name: &str) -> ObjectId {
    let blob = repo
        .write_blob(format!("{name} contents\n").into_bytes())
        .expect("blob");
    let mut builder = TreeEditor::new();
    builder.upsert(format!("{name}.txt").as_str(), EntryKind::Blob, blob);
    let tree = repo.write_tree(builder).expect("tree");
    let commit = Commit {
        tree,
        parents: parent.into_iter().cloned().collect(),
        author: b"T <t@example.invalid> 1 +0000".to_vec(),
        committer: b"T <t@example.invalid> 1 +0000".to_vec(),
        encoding: None,
        message: format!("commit {name}\n").into_bytes(),
    };
    repo.write_object(EncodedObject::new(GitObjectType::Commit, commit.write()))
        .expect("commit")
}

fn branch_oid(repo: &Repository, branch: &str) -> Option<ObjectId> {
    repo.find_reference(&format!("refs/heads/{branch}"))
        .expect("ref lookup")
        .and_then(|reference| reference.direct_target().ok())
}

#[test]
fn clone_copies_refs_objects_and_mirrors_the_source_head() {
    let source_dir = TempDir::new("source");
    let dest_dir = TempDir::new("dest");
    let source = Repository::init(&source_dir.path).expect("init checkout");
    let trunk_base = write_commit(&source, None, "trunk-base");
    let trunk_tip = write_commit(&source, Some(&trunk_base), "trunk-tip");
    let main_tip = write_commit(&source, Some(&trunk_base), "main-tip");
    let tag_target = write_commit(&source, Some(&trunk_base), "tagged");
    source
        .apply_ref_changes(&[
            sley::RefChange::new(
                "refs/heads/trunk",
                ReferenceTarget::Direct(trunk_tip.clone()),
            )
            .expect("ref name"),
            sley::RefChange::new("refs/heads/main", ReferenceTarget::Direct(main_tip))
                .expect("ref name"),
            sley::RefChange::new("refs/tags/v1", ReferenceTarget::Direct(tag_target))
                .expect("ref name"),
        ])
        .expect("seed refs");
    // The source is checked out on `trunk`, not `main`.
    source
        .set_head_symref("refs/heads/trunk", sley::HeadUpdateOptions::new())
        .expect("attach HEAD to trunk");

    let summary =
        sley::clone_local_to_bare(&source_dir.path, &dest_dir.path, &LocalCloneOptions::new())
            .expect("local clone");

    assert_eq!(summary.refs_copied, 3);
    assert_eq!(summary.head_branch.as_deref(), Some("trunk"));

    let dest = Repository::open_exact_bare(&dest_dir.path).expect("bare destination");
    assert!(dest.workdir().is_none(), "destination must be bare");
    assert_eq!(branch_oid(&dest, "trunk"), Some(trunk_tip));
    assert_eq!(
        branch_oid(&dest, "main"),
        Some(main_tip),
        "`main` must exist but must NOT steal HEAD"
    );
    let tag = dest
        .find_reference("refs/tags/v1")
        .expect("tag lookup")
        .expect("tag copied");
    assert_eq!(tag.target, ReferenceTarget::Direct(tag_target));

    // Objects are readable in the destination, not just refs pointing at ids.
    let head = dest.head().expect("head");
    assert_eq!(
        head.symbolic_target.as_ref().map(FullName::as_str),
        Some("refs/heads/trunk")
    );
    let commit = dest
        .read_commit(head.oid.as_ref().expect("attached head oid"))
        .expect("commit readable in destination");
    assert_eq!(commit.message, b"commit trunk-tip\n".to_vec());
}

#[test]
fn uncopied_source_head_falls_back_to_main_then_first_branch() {
    let first = TempDir::new("fallback-main");
    let dest_a = TempDir::new("fallback-a");
    let source = Repository::init(&first.path).expect("init");
    let base = write_commit(&source, None, "base");
    let beta = write_commit(&source, Some(&base), "beta");
    let main = write_commit(&source, Some(&base), "main");
    source
        .apply_ref_changes(&[
            sley::RefChange::new("refs/heads/beta", ReferenceTarget::Direct(beta))
                .expect("ref name"),
            sley::RefChange::new("refs/heads/main", ReferenceTarget::Direct(main))
                .expect("ref name"),
        ])
        .expect("seed branches");
    // HEAD points at a branch that will never be copied.
    source
        .set_head_symref("refs/heads/gone", sley::HeadUpdateOptions::new())
        .expect("dangling HEAD");

    let summary = sley::clone_local_to_bare(&first.path, &dest_a.path, &LocalCloneOptions::new())
        .expect("clone with dangling source HEAD");
    assert_eq!(summary.head_branch.as_deref(), Some("main"));
}

#[test]
fn alphabetical_fallback_picks_the_first_copied_branch() {
    let source_dir = TempDir::new("alpha-source");
    let dest_dir = TempDir::new("alpha-dest");
    let source = Repository::init(&source_dir.path).expect("init");
    let base = write_commit(&source, None, "base");
    for branch in ["zeta", "alpha", "mid"] {
        let tip = write_commit(&source, Some(&base), branch);
        source
            .apply_ref_changes(&[sley::RefChange::new(
                format!("refs/heads/{branch}"),
                ReferenceTarget::Direct(tip),
            )
            .expect("ref name")])
            .expect("seed branch");
    }
    source
        .set_head_symref("refs/heads/gone", sley::HeadUpdateOptions::new())
        .expect("dangling HEAD");

    let summary =
        sley::clone_local_to_bare(&source_dir.path, &dest_dir.path, &LocalCloneOptions::new())
            .expect("clone");
    assert_eq!(summary.head_branch.as_deref(), Some("alpha"));
    let dest = Repository::open_exact_bare(&dest_dir.path).expect("bare destination");
    assert_eq!(
        dest.head()
            .expect("head")
            .symbolic_target
            .as_ref()
            .map(FullName::as_str),
        Some("refs/heads/alpha")
    );
}

#[test]
fn recopy_into_an_existing_bare_destination_merges_refs() {
    let source_dir = TempDir::new("merge-source");
    let dest_dir = TempDir::new("merge-dest");
    let source = Repository::init(&source_dir.path).expect("init");
    let base = write_commit(&source, None, "base");
    source
        .apply_ref_changes(&[sley::RefChange::new(
            "refs/heads/main",
            ReferenceTarget::Direct(base.clone()),
        )
        .expect("ref name")])
        .expect("seed main");

    sley::clone_local_to_bare(&source_dir.path, &dest_dir.path, &LocalCloneOptions::new())
        .expect("first clone");

    // New work lands upstream; the re-copy must update `main` in place.
    let newer = write_commit(&source, Some(&base), "newer");
    source
        .apply_ref_changes(&[sley::RefChange::new(
            "refs/heads/main",
            ReferenceTarget::Direct(newer.clone()),
        )
        .expect("ref name")])
        .expect("advance main");
    let summary =
        sley::clone_local_to_bare(&source_dir.path, &dest_dir.path, &LocalCloneOptions::new())
            .expect("re-copy");

    assert_eq!(summary.refs_copied, 1);
    assert_eq!(summary.head_branch.as_deref(), Some("main"));
    let dest = Repository::open_exact_bare(&dest_dir.path).expect("bare destination");
    assert_eq!(branch_oid(&dest, "main"), Some(newer));

    // The update reads as an update in the reflog: old side = previous tip.
    let log = dest
        .references()
        .read_reflog("refs/heads/main")
        .expect("read reflog");
    let last = log.last().expect("reflog entry");
    assert_eq!(last.old_oid, base);
    assert_eq!(last.new_oid, newer);
}

#[test]
fn reflog_message_is_recorded_on_copied_branches() {
    let source_dir = TempDir::new("reflog-source");
    let dest_dir = TempDir::new("reflog-dest");
    let source = Repository::init(&source_dir.path).expect("init");
    let base = write_commit(&source, None, "base");
    source
        .apply_ref_changes(&[sley::RefChange::new(
            "refs/heads/trunk",
            ReferenceTarget::Direct(base),
        )
        .expect("ref name")])
        .expect("seed trunk");

    let options = LocalCloneOptions::new().reflog_message("heddle: clone from fixture");
    sley::clone_local_to_bare(&source_dir.path, &dest_dir.path, &options).expect("clone");

    let dest = Repository::open_exact_bare(&dest_dir.path).expect("bare destination");
    let log = dest
        .references()
        .read_reflog("refs/heads/trunk")
        .expect("read reflog");
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].message, b"heddle: clone from fixture".to_vec());
}

#[test]
fn empty_source_initializes_an_empty_bare_destination() {
    let source_dir = TempDir::new("empty-source");
    let dest_dir = TempDir::new("empty-dest");
    let _source = Repository::init(&source_dir.path).expect("init");
    let summary =
        sley::clone_local_to_bare(&source_dir.path, &dest_dir.path, &LocalCloneOptions::new())
            .expect("clone of an unborn repository");
    assert_eq!(summary.refs_copied, 0);
    assert_eq!(summary.head_branch, None);
    Repository::open_exact_bare(&dest_dir.path).expect("destination still becomes bare");
}

#[test]
fn object_format_mismatch_is_rejected_before_refs_move() {
    let source_dir = TempDir::new("sha256-source");
    let dest_dir = TempDir::new("sha1-dest");
    let source = Repository::init_with_format(&source_dir.path, sley::ObjectFormat::Sha256, false)
        .expect("init sha256");
    let tip = write_commit(&source, None, "tip");
    source
        .apply_ref_changes(&[
            sley::RefChange::new("refs/heads/main", ReferenceTarget::Direct(tip))
                .expect("ref name"),
        ])
        .expect("seed main");
    let dest = Repository::init_bare(&dest_dir.path).expect("init sha1 bare");

    let error =
        sley::clone_local_to_bare(&source_dir.path, &dest_dir.path, &LocalCloneOptions::new())
            .expect_err("format mismatch must fail");
    assert!(
        error.to_string().contains("object format mismatch"),
        "unexpected error: {error}"
    );
    assert!(
        dest.find_reference("refs/heads/main")
            .expect("lookup")
            .is_none(),
        "no ref may land when the object transfer failed"
    );
}
