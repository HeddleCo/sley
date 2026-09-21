//! Public embedding contract. Run with --no-default-features --features http.
#![cfg(feature = "http")]

use std::cell::{Cell, RefCell};
use std::io::{self, Cursor, Read};
use std::path::Path;
use std::sync::Arc;

use sley_config::GitConfig;
use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_formats::RepositoryLayout;
use sley_object::{Commit, EncodedObject, ObjectType, Tree};
use sley_odb::{FileObjectDatabase, ObjectReader};
use sley_pack::PackFile;
use sley_protocol::{GitService, ProtocolVersion, RefAdvertisement, RefAdvertisementSet};
use sley_refs::{FileRefStore, RefTarget};
use sley_remote::{
    AtomicCancel, CancelFlag, FetchOptions, FetchOutcome, FetchRequest, FetchServices, FetchSource,
    HttpClient, HttpResponse, NoCredentials, SilentProgress, fetch_with_http_client,
};
use sley_transport::{
    ServiceAnnouncement, ServiceDiscoveryPayload, ServiceDiscoveryResponse, parse_remote_url,
    write_service_discovery_response,
};

const FORMAT: ObjectFormat = ObjectFormat::Sha1;
const TRACKING: &str = "refs/remotes/origin/main";

struct ChunkedBody {
    inner: Cursor<Vec<u8>>,
    cancel: Option<Arc<AtomicCancel>>,
}

impl Read for ChunkedBody {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let len = out.len().min(7);
        let read = self.inner.read(&mut out[..len])?;
        // Stop after entering the pack, so cancellation has to propagate from
        // the response reader through pack installation, not just at entry.
        if self.inner.position() > 24
            && let Some(cancel) = &self.cancel
        {
            cancel.cancel();
        }
        Ok(read)
    }
}

struct Adapter {
    tip: ObjectId,
    advertisement: Vec<u8>,
    pack_response: Vec<u8>,
    gets: Cell<usize>,
    requests: RefCell<Vec<Vec<u8>>>,
    cancel: Option<Arc<AtomicCancel>>,
}

impl Adapter {
    fn new() -> Self {
        let tree = EncodedObject::new(ObjectType::Tree, Tree { entries: vec![] }.write());
        let identity = b"Importer <import@example.invalid> 1 +0000".to_vec();
        let commit = EncodedObject::new(
            ObjectType::Commit,
            Commit {
                tree: tree.object_id(FORMAT).expect("tree id"),
                parents: vec![],
                author: identity.clone(),
                committer: identity,
                encoding: None,
                message: b"hosted import\n".to_vec(),
            }
            .write(),
        );
        let tip = commit.object_id(FORMAT).expect("commit id");
        let pack = PackFile::write_undeltified(&[tree, commit], FORMAT).expect("pack");
        let mut pack_response = b"0008NAK\n".to_vec();
        sley_protocol::write_sideband_packet(
            &mut pack_response,
            &sley_protocol::SideBandPacket {
                channel: sley_protocol::SideBandChannel::Data,
                data: pack.pack,
            },
        )
        .expect("sideband pack");
        pack_response.extend_from_slice(b"0000");
        let mut advertisement = Vec::new();
        write_service_discovery_response(
            &mut advertisement,
            &ServiceDiscoveryResponse {
                announcement: ServiceAnnouncement {
                    service: GitService::UploadPack,
                },
                payload: ServiceDiscoveryPayload::AdvertisedRefs(RefAdvertisementSet {
                    protocol: ProtocolVersion::V0,
                    refs: vec![RefAdvertisement {
                        oid: tip,
                        name: "refs/heads/main".into(),
                        capabilities: vec![sley_core::Capability {
                            name: "side-band-64k".into(),
                            value: None,
                        }],
                    }],
                    shallow: vec![],
                }),
            },
        )
        .expect("advertisement");
        Self {
            tip,
            advertisement,
            pack_response,
            gets: Cell::new(0),
            requests: RefCell::new(vec![]),
            cancel: None,
        }
    }

    fn response(&self, bytes: Vec<u8>, content_type: &str, pack: bool) -> HttpResponse {
        HttpResponse {
            status: 200,
            content_type: Some(content_type.into()),
            content_length: None,
            content_range: None,
            body: Box::new(ChunkedBody {
                inner: Cursor::new(bytes),
                cancel: if pack { self.cancel.clone() } else { None },
            }),
        }
    }
}

impl HttpClient for Adapter {
    fn get(&self, url: &str, _headers: &[(&str, &str)]) -> Result<HttpResponse> {
        assert_eq!(
            url,
            "https://example.invalid/repo.git/info/refs?service=git-upload-pack"
        );
        self.gets.set(self.gets.get() + 1);
        Ok(self.response(
            self.advertisement.clone(),
            "application/x-git-upload-pack-advertisement",
            false,
        ))
    }

    fn post(
        &self,
        _url: &str,
        _content_type: &str,
        _headers: &[(&str, &str)],
        _body: &[u8],
    ) -> Result<HttpResponse> {
        panic!("http.postBuffer=1 must select the adapter's streaming POST");
    }

    fn post_reader(
        &self,
        url: &str,
        content_type: &str,
        _headers: &[(&str, &str)],
        body: &mut dyn Read,
    ) -> Result<HttpResponse> {
        assert_eq!(url, "https://example.invalid/repo.git/git-upload-pack");
        assert_eq!(content_type, "application/x-git-upload-pack-request");
        let mut request = Vec::new();
        body.read_to_end(&mut request)?;
        self.requests.borrow_mut().push(request);
        Ok(self.response(
            self.pack_response.clone(),
            "application/x-git-upload-pack-result",
            true,
        ))
    }
}

fn default_options() -> FetchOptions {
    FetchOptions {
        policy: Default::default(),
        quiet: true,
        progress: None,
        auto_follow_tags: false,
        fetch_all_tags: false,
        prune: false,
        prune_tags: false,
        dry_run: false,
        force: false,
        append: false,
        write_fetch_head: true,
        tag_option_explicit: true,
        prune_option_explicit: true,
        prune_tags_option_explicit: true,
        refmap: None,
        depth: None,
        merge_srcs: Vec::new(),
        filter: None,
        filter_auto: false,
        refetch: false,
        cloning: false,
        record_promisor_refs: true,
        update_shallow: false,
        reject_shallow: false,
        deepen_relative: false,
        update_head_ok: false,
        deepen_since: None,
        deepen_not: Vec::new(),
        ssh_options: None,
        upload_pack_command: None,
        atomic: false,
        negotiation_restrict: None,
        negotiation_include: None,
        negotiate_only: false,
    }
}

fn config() -> GitConfig {
    GitConfig::parse(b"[http]\npostBuffer = 1\n[protocol]\nversion = 0\n").expect("config")
}

fn import(
    git_dir: &Path,
    client: Option<&dyn HttpClient>,
    options: &FetchOptions,
    config: &GitConfig,
    cancel: CancelFlag<'_>,
) -> Result<FetchOutcome> {
    fetch_with_http_client(
        FetchRequest {
            git_dir,
            format: FORMAT,
            config,
            remote_name: "origin",
            source: &FetchSource::Http(
                parse_remote_url("https://example.invalid/repo.git").expect("URL"),
            ),
            refspecs: &[format!("refs/heads/main:{TRACKING}")],
            options,
            validation: None,
        },
        FetchServices {
            credentials: &mut NoCredentials,
            progress: &mut SilentProgress,
            ref_hook: None,
            cancel,
        },
        client,
    )
}

#[test]
fn imports_pack_through_streaming_adapter() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = RepositoryLayout::init_at(temp.path(), FORMAT, true).expect("bare repository");
    let client = Adapter::new();
    let result = import(
        &repo.git_dir,
        Some(&client),
        &default_options(),
        &config(),
        CancelFlag::never(),
    )
    .expect("import");
    assert_eq!(result.ref_updates.len(), 1);
    assert_eq!(client.gets.get(), 1);
    let requests = client.requests.borrow();
    assert_eq!(requests.len(), 1);
    assert!(String::from_utf8_lossy(&requests[0]).contains(&format!("want {}", client.tip)));
    assert_eq!(
        FileRefStore::new(&repo.git_dir, FORMAT)
            .read_ref(TRACKING)
            .expect("ref"),
        Some(RefTarget::Direct(client.tip))
    );
    assert_eq!(
        FileObjectDatabase::from_git_dir(&repo.git_dir, FORMAT)
            .read_object(&client.tip)
            .expect("installed commit")
            .object_type,
        ObjectType::Commit
    );
}

#[test]
fn cancellation_during_stream_does_not_publish_refs() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = RepositoryLayout::init_at(temp.path(), FORMAT, true).expect("bare repository");
    let cancel = Arc::new(AtomicCancel::new());
    let mut client = Adapter::new();
    client.cancel = Some(cancel.clone());
    let error = import(
        &repo.git_dir,
        Some(&client),
        &default_options(),
        &config(),
        CancelFlag::new(&cancel),
    )
    .expect_err("cancelled import");
    assert!(matches!(error, GitError::Cancelled), "{error:?}");
    assert_eq!(client.requests.borrow().len(), 1);
    assert_eq!(
        FileRefStore::new(&repo.git_dir, FORMAT)
            .read_ref(TRACKING)
            .expect("ref"),
        None
    );
    assert!(
        FileObjectDatabase::from_git_dir(&repo.git_dir, FORMAT)
            .read_object(&client.tip)
            .is_err()
    );
}

#[test]
fn policy_and_config_denials_precede_adapter_calls() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = RepositoryLayout::init_at(temp.path(), FORMAT, true).expect("bare repository");
    let client = Adapter::new();
    let mut options = default_options();
    options.policy.transport.allow_protocols = Some(vec![]);
    let error = import(
        &repo.git_dir,
        Some(&client),
        &options,
        &config(),
        CancelFlag::never(),
    )
    .expect_err("policy denied");
    assert!(error.to_string().contains("not allowed"), "{error}");
    let denied = GitConfig::parse(b"[protocol \"https\"]\nallow = never\n").expect("config");
    let error = import(
        &repo.git_dir,
        Some(&client),
        &default_options(),
        &denied,
        CancelFlag::never(),
    )
    .expect_err("config denied");
    assert!(error.to_string().contains("not allowed"), "{error}");
    assert_eq!(client.gets.get(), 0);
    assert!(client.requests.borrow().is_empty());
}

#[cfg(not(feature = "default-http-client"))]
#[test]
fn omitted_client_returns_an_actionable_error() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = RepositoryLayout::init_at(temp.path(), FORMAT, true).expect("bare repository");
    let error = import(
        &repo.git_dir,
        None,
        &default_options(),
        &config(),
        CancelFlag::never(),
    )
    .expect_err("no backend");
    assert!(
        matches!(&error, GitError::Unsupported(message) if message.contains("supply an HttpClient")),
        "{error:?}"
    );
}

#[cfg(not(feature = "worktree"))]
#[test]
fn clone_without_checkout_imports_and_checkout_rejects_before_creating_destination() {
    use sley_remote::{
        CloneOptions, CloneRequest, CloneServices, CloneSource, clone_with_http_client,
    };
    let temp = tempfile::tempdir().expect("tempdir");
    for checkout in [false, true] {
        let destination = temp
            .path()
            .join(if checkout { "checkout" } else { "import" });
        let client = Adapter::new();
        let options = CloneOptions {
            policy: Default::default(),
            origin: "origin",
            checkout_branch: "main",
            remote_head_branch: "main",
            single_branch: true,
            progress: false,
            depth: None,
            deepen_since: None,
            deepen_not: vec![],
            committer: b"Importer <import@example.invalid> 1 +0000".to_vec(),
            detached_head: None,
            checkout,
            sparse: false,
            filter: None,
            filter_auto: false,
            branch_explicit: true,
            ref_storage: sley_formats::RefStorageFormat::Files,
            ssh_options: None,
            upload_pack_command: None,
            reject_shallow: false,
        };
        let clone_config = GitConfig::parse(b"[http]\npostBuffer = 1\n[protocol]\nversion = 0\n[remote \"origin\"]\nfetch = +refs/heads/*:refs/remotes/origin/*\n").expect("clone config");
        let mut configure = |_: &Path| Ok(clone_config.clone());
        let mut configure_branch = |_: &Path, _: &str| Ok(clone_config.clone());
        let result = clone_with_http_client(
            None,
            CloneRequest {
                destination: &destination,
                git_dir_override: None,
                core_worktree: None,
                format: FORMAT,
                source: &CloneSource::Http(
                    parse_remote_url("https://example.invalid/repo.git").expect("URL"),
                ),
                options: &options,
            },
            CloneServices {
                configure: &mut configure,
                configure_branch: &mut configure_branch,
                credentials: &mut NoCredentials,
                progress: &mut SilentProgress,
                cancel: CancelFlag::never(),
            },
            Some(&client),
        );
        if checkout {
            assert!(matches!(result, Err(GitError::Unsupported(_))));
            assert!(!destination.exists());
            assert_eq!(client.gets.get(), 0);
        } else {
            let cloned = result.expect("clone without worktree feature");
            assert_eq!(cloned.branch_oid, Some(client.tip));
            assert_eq!(
                FileRefStore::new(&cloned.git_dir, FORMAT)
                    .read_ref("refs/heads/main")
                    .expect("branch"),
                Some(RefTarget::Direct(client.tip))
            );
            assert!(!cloned.git_dir.join("index").exists());
        }
    }
}
