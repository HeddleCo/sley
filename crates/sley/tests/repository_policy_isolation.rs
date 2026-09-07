#![cfg(feature = "remote")]

use std::sync::Barrier;

use sley::{ObjectFormat, RefChange, ReferenceTarget, Repository};
use sley_core::Namespace;
use sley_remote::{RemotePolicy, TransportPolicy};

/// Both physical namespaces exist in each repository. Accidentally selecting
/// the other operation's namespace therefore returns a valid but wrong ref.
#[test]
fn two_repositories_keep_namespace_unicode_and_transport_policy_isolated() {
    let root = std::env::temp_dir().join(format!("sley-policy-isolation-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create fixture");
    let left = Repository::init(root.join("left")).expect("left repository");
    let right = Repository::init(root.join("right")).expect("right repository");
    let nfd = "A\u{0308}.txt";
    for (repo, precompose) in [(&left, true), (&right, false)] {
        std::fs::write(
            repo.git_dir().join("config"),
            format!("[core]\n\tbare = false\n\tprecomposeunicode = {precompose}\n[protocol \"custom\"]\n\tallow = user\n"),
        ).expect("repository config");
        std::fs::write(repo.workdir().expect("worktree").join(nfd), b"content\n")
            .expect("NFD file");
        for namespace in ["left", "right"] {
            let oid = repo
                .write_blob(namespace.as_bytes().to_vec())
                .expect("blob");
            repo.apply_ref_changes(&[RefChange::new(
                format!("refs/namespaces/{namespace}/refs/heads/{namespace}"),
                ReferenceTarget::Direct(oid),
            )
            .expect("ref name")])
                .expect("namespace ref");
        }
    }
    let barrier = Barrier::new(2);
    std::thread::scope(|scope| {
        let run = |repo: &Repository, name: &str, precompose: bool, allowed: &str| {
            let policy = RemotePolicy {
                namespace: Namespace::new(name),
                transport: TransportPolicy {
                    allow_protocols: Some(vec!["file".into(), allowed.into()]),
                    from_user: precompose,
                },
            };
            // Resolve both snapshots before either starts doing repository work.
            let config =
                sley_config::read_repo_config_file_only(repo.git_dir()).expect("config snapshot");
            barrier.wait();
            for _ in 0..32 {
                let refs = sley_remote::local_fetch_advertisements(
                    &policy,
                    repo.git_dir(),
                    ObjectFormat::Sha1,
                )
                .expect("advertisements");
                assert_eq!(
                    refs.iter()
                        .map(|reference| reference.name.as_str())
                        .collect::<Vec<_>>(),
                    [format!("refs/heads/{name}")]
                );
                let status = sley_worktree::collect_short_status_with_options(
                    repo.workdir().expect("worktree"),
                    repo.git_dir(),
                    ObjectFormat::Sha1,
                    sley_worktree::ShortStatusOptions::default(),
                )
                .expect("worktree status");
                assert_eq!(status.len(), 1);
                assert_eq!(
                    status[0].path,
                    if precompose { "Ä.txt" } else { nfd }.as_bytes()
                );
                assert_eq!(
                    config.precompose_unicode().string(nfd),
                    if precompose { "Ä.txt" } else { nfd }
                );
                sley_remote::check_transport_allowed(allowed, Some(&config), &policy.transport)
                    .expect("allowed protocol");
                let denied = if allowed == "https" { "ssh" } else { "https" };
                assert!(
                    sley_remote::check_transport_allowed(denied, Some(&config), &policy.transport)
                        .is_err()
                );
                let user_policy = TransportPolicy {
                    allow_protocols: None,
                    ..policy.transport.clone()
                };
                assert_eq!(
                    sley_remote::check_transport_allowed("custom", Some(&config), &user_policy)
                        .is_ok(),
                    precompose
                );
            }
        };
        let first = scope.spawn(move || run(&left, "left", true, "https"));
        let second = scope.spawn(move || run(&right, "right", false, "ssh"));
        first.join().expect("left operation");
        second.join().expect("right operation");
    });
    std::fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn two_worktree_mutations_preserve_their_callers_directory() {
    let root = std::env::temp_dir().join(format!("sley-cwd-isolation-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let left = Repository::init(root.join("left")).expect("left repository");
    let right = Repository::init(root.join("right")).expect("right repository");
    let left_root = left.workdir().expect("left worktree");
    let right_root = right.workdir().expect("right worktree");
    for worktree in [&left_root, &right_root] {
        std::fs::create_dir(worktree.join("dir")).expect("directory");
        std::fs::write(worktree.join("dir/file"), b"content").expect("file");
    }
    let preserved_left = left_root.join("dir");
    let barrier = Barrier::new(2);
    std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            barrier.wait();
            sley_worktree::remove_worktree_path(Some(&preserved_left), &left_root, b"dir/file")
                .expect("left removal");
        });
        let second = scope.spawn(|| {
            barrier.wait();
            sley_worktree::remove_worktree_path(Some(&right_root), &right_root, b"dir/file")
                .expect("right removal");
        });
        first.join().expect("left operation");
        second.join().expect("right operation");
    });
    assert!(
        preserved_left.is_dir(),
        "left caller's current directory must survive"
    );
    assert!(!left_root.join("dir/file").exists());
    assert!(
        !right_root.join("dir").exists(),
        "right operation prunes its empty directory"
    );
    std::fs::remove_dir_all(root).expect("remove fixture");
}
