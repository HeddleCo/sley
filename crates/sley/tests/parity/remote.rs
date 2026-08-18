use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use sley::plumbing::sley_core::Capability;
use sley::plumbing::sley_odb::repository_object_ids;
use sley::plumbing::sley_pack::PackFile;
use sley::plumbing::sley_protocol::{
    PktLineFrame, ProtocolV2FetchPackfileUri, ProtocolV2FetchResponseSection,
    ProtocolV2LsRefsRecord, ProtocolV2LsRefsRef, ProtocolVersion, TransportHandshake,
    read_protocol_v2_command_request, write_pkt_line_frame, write_pkt_line_payload,
    write_protocol_v2_advertisement, write_protocol_v2_fetch_response,
    write_protocol_v2_ls_refs_response,
};
use sley::remote::{FetchOptions, HttpClient, HttpResponse, NoCredentials, SilentProgress};
use sley::{ObjectFormat, ObjectId, Repository};
use sley_testkit::engine_parity::{EngineOutput, EngineParityCase};

#[derive(Default)]
struct ParityResponses {
    discovery: Vec<u8>,
    ls_refs: Vec<u8>,
    fetch: Vec<u8>,
    uri_pack: Vec<u8>,
}

struct ParityHttpClient {
    responses: Arc<Mutex<ParityResponses>>,
}

impl HttpClient for ParityHttpClient {
    fn get(&self, url: &str, _headers: &[(&str, &str)]) -> sley::Result<HttpResponse> {
        let responses = self.responses.lock().expect("parity HTTP responses");
        let (content_type, body) = if url.contains("/info/refs?") {
            (
                Some("application/x-git-upload-pack-advertisement".into()),
                responses.discovery.clone(),
            )
        } else {
            (
                Some("application/x-git-packed-objects".into()),
                responses.uri_pack.clone(),
            )
        };
        Ok(HttpResponse {
            status: 200,
            content_type,
            content_length: Some(body.len() as u64),
            content_range: None,
            body: Box::new(std::io::Cursor::new(body)),
        })
    }

    fn post(
        &self,
        _url: &str,
        _content_type: &str,
        _headers: &[(&str, &str)],
        body: &[u8],
    ) -> sley::Result<HttpResponse> {
        let mut input = body;
        let request = read_protocol_v2_command_request(&mut input)?;
        let responses = self.responses.lock().expect("parity HTTP responses");
        let body = match request.command.as_str() {
            "ls-refs" => responses.ls_refs.clone(),
            "fetch" => {
                assert!(
                    request
                        .arguments
                        .iter()
                        .any(|argument| argument == b"packfile-uris http,https"),
                    "Sley fetch did not negotiate packfile-uris"
                );
                responses.fetch.clone()
            }
            other => panic!("unexpected protocol-v2 command {other}"),
        };
        Ok(HttpResponse {
            status: 200,
            content_type: Some("application/x-git-upload-pack-result".into()),
            content_length: Some(body.len() as u64),
            content_range: None,
            body: Box::new(std::io::Cursor::new(body)),
        })
    }
}

struct PackHttpServer {
    address: std::net::SocketAddr,
    body: Arc<Mutex<Vec<u8>>>,
    stop: Arc<AtomicBool>,
    requests: Arc<std::sync::atomic::AtomicUsize>,
    worker: Option<JoinHandle<()>>,
}

impl PackHttpServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind packfile URI server");
        listener
            .set_nonblocking(true)
            .expect("set packfile URI server nonblocking");
        let address = listener.local_addr().expect("packfile URI server address");
        let body = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server_body = Arc::clone(&body);
        let server_stop = Arc::clone(&stop);
        let server_requests = Arc::clone(&requests);
        let worker = std::thread::spawn(move || {
            while !server_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        server_requests.fetch_add(1, Ordering::Relaxed);
                        serve_pack(stream, &server_body);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept packfile URI request: {error}"),
                }
            }
        });
        Self {
            address,
            body,
            stop,
            requests,
            worker: Some(worker),
        }
    }

    fn url(&self) -> String {
        format!("http://{}/cdn.pack", self.address)
    }
}

impl Drop for PackHttpServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.address);
        if let Some(worker) = self.worker.take() {
            worker.join().expect("packfile URI server");
        }
    }
}

fn serve_pack(mut stream: TcpStream, body: &Mutex<Vec<u8>>) {
    let mut request = [0_u8; 4096];
    let _ = stream
        .read(&mut request)
        .expect("read packfile URI request");
    let body = body.lock().expect("packfile URI body");
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/x-git-packed-objects\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .expect("write packfile URI headers");
    stream.write_all(&body).expect("write packfile URI body");
}

fn fetch_options() -> FetchOptions {
    FetchOptions {
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

fn repository_state(path: &Path) -> EngineOutput {
    let git_dir = path.join(".git");
    let repository = Repository::discover(path).expect("fetched repository");
    let mut stdout = b"objects\n".to_vec();
    for oid in repository_object_ids(&git_dir, ObjectFormat::Sha1).expect("repository objects") {
        stdout.extend_from_slice(oid.to_hex().as_bytes());
        stdout.push(b'\n');
        let framed = repository
            .read_object(&oid)
            .expect("fetched object")
            .framed_bytes();
        stdout.extend_from_slice(framed.len().to_string().as_bytes());
        stdout.push(b'\n');
        stdout.extend_from_slice(&framed);
        stdout.push(b'\n');
    }
    stdout.extend_from_slice(b"ref\n");
    stdout.extend_from_slice(
        &fs::read(git_dir.join("refs/remotes/origin/main")).expect("remote-tracking ref"),
    );
    EngineOutput::stdout(stdout)
}

#[test]
fn packfile_uris_fetch_object_set_and_ref_match_git_2_55() {
    let pack_server = PackHttpServer::start();
    let pack_url = pack_server.url();
    let responses = Arc::new(Mutex::new(ParityResponses::default()));
    let setup_responses = Arc::clone(&responses);
    let server_body = Arc::clone(&pack_server.body);

    EngineParityCase::new("packfile-uris-fetch").run(
        move |fixture| {
            let source = fixture.mkdir("source");
            fixture.oracle_ok_in(&source, &["init", "-q", "-b", "main"]);
            fs::write(source.join("inline.txt"), b"inline object\n").expect("inline file");
            fs::write(source.join("cdn.txt"), b"cdn object\n").expect("CDN file");
            fixture.oracle_ok_in(&source, &["add", "inline.txt", "cdn.txt"]);
            fixture.oracle_ok_in(
                &source,
                &[
                    "-c",
                    "user.name=Sley Test",
                    "-c",
                    "user.email=sley@example.invalid",
                    "commit",
                    "-q",
                    "-m",
                    "packfile URI fixture",
                ],
            );
            let tip = String::from_utf8(fixture.oracle_ok_in(&source, &["rev-parse", "HEAD"]))
                .expect("tip text");
            let tip = ObjectId::from_hex(ObjectFormat::Sha1, tip.trim()).expect("tip oid");
            let source_repo = Repository::discover(&source).expect("source repository");
            let commit = source_repo.read_commit(&tip).expect("source commit");
            let tree = source_repo.read_tree(&commit.tree).expect("source tree");
            let cdn_oid = tree
                .entries
                .iter()
                .find(|entry| entry.name.as_bytes() == b"cdn.txt")
                .expect("CDN tree entry")
                .oid;
            let inline_oid = tree
                .entries
                .iter()
                .find(|entry| entry.name.as_bytes() == b"inline.txt")
                .expect("inline tree entry")
                .oid;
            let uri_pack = PackFile::write_undeltified(
                &[(*source_repo.read_object(&cdn_oid).expect("CDN object")).clone()],
                ObjectFormat::Sha1,
            )
            .expect("URI pack");
            let inline_pack = PackFile::write_undeltified(
                &[
                    (*source_repo.read_object(&tip).expect("commit object")).clone(),
                    (*source_repo.read_object(&commit.tree).expect("tree object")).clone(),
                    (*source_repo.read_object(&inline_oid).expect("inline object")).clone(),
                ],
                ObjectFormat::Sha1,
            )
            .expect("inline pack");
            *server_body.lock().expect("server pack body") = uri_pack.pack.clone();
            fixture.oracle_ok_in(
                &source,
                &[
                    "config",
                    "--add",
                    "uploadpack.blobPackfileUri",
                    &format!("{cdn_oid} {} {pack_url}", uri_pack.checksum),
                ],
            );
            fixture.oracle_ok_in(&source, &["config", "uploadpack.allowSidebandAll", "true"]);

            for client in ["sley-client", "git-client"] {
                fixture.oracle_ok_in(fixture.path(), &["init", "-q", "-b", "main", client]);
                fixture.oracle_ok_in(
                    &fixture.path().join(client),
                    &["config", "fetch.uriProtocols", "http,https"],
                );
            }

            let handshake = TransportHandshake {
                protocol: ProtocolVersion::V2,
                capabilities: vec![
                    Capability {
                        name: "ls-refs".into(),
                        value: None,
                    },
                    Capability {
                        name: "fetch".into(),
                        value: Some("packfile-uris".into()),
                    },
                    Capability {
                        name: "object-format".into(),
                        value: Some("sha1".into()),
                    },
                ],
            };
            let mut discovery = Vec::new();
            write_pkt_line_payload(&mut discovery, b"# service=git-upload-pack\n")
                .expect("service announcement");
            write_pkt_line_frame(&mut discovery, &PktLineFrame::Flush).expect("announcement flush");
            write_protocol_v2_advertisement(&mut discovery, &handshake).expect("v2 advertisement");
            let mut ls_refs = Vec::new();
            write_protocol_v2_ls_refs_response(
                &mut ls_refs,
                &[
                    ProtocolV2LsRefsRecord::Ref(ProtocolV2LsRefsRef {
                        oid: tip,
                        name: "HEAD".into(),
                        peeled: None,
                        symref_target: Some("refs/heads/main".into()),
                        attributes: Vec::new(),
                    }),
                    ProtocolV2LsRefsRecord::Ref(ProtocolV2LsRefsRef {
                        oid: tip,
                        name: "refs/heads/main".into(),
                        peeled: None,
                        symref_target: None,
                        attributes: Vec::new(),
                    }),
                ],
            )
            .expect("ls-refs response");
            let mut inline_sideband = vec![1];
            inline_sideband.extend_from_slice(&inline_pack.pack);
            let mut fetch = Vec::new();
            write_protocol_v2_fetch_response(
                &mut fetch,
                &[
                    ProtocolV2FetchResponseSection::PackfileUris(vec![
                        ProtocolV2FetchPackfileUri {
                            pack_hash: uri_pack.checksum,
                            uri: pack_url.clone(),
                        },
                    ]),
                    ProtocolV2FetchResponseSection::Packfile(vec![inline_sideband]),
                ],
            )
            .expect("fetch response");
            *setup_responses.lock().expect("parity responses") = ParityResponses {
                discovery,
                ls_refs,
                fetch,
                uri_pack: uri_pack.pack,
            };
        },
        |fixture| {
            let client_path = fixture.path().join("sley-client");
            let repository = Repository::discover(&client_path).expect("Sley client repository");
            let client = ParityHttpClient {
                responses: Arc::clone(&responses),
            };
            repository
                .fetch_with_http_client(
                    "http://fixture.invalid/repo.git",
                    &["refs/heads/main:refs/remotes/origin/main".into()],
                    fetch_options(),
                    &mut NoCredentials,
                    &mut SilentProgress,
                    Some(&client),
                )
                .expect("Sley packfile-URI fetch");
            repository_state(&client_path)
        },
        |fixture| {
            let client_path = fixture.path().join("git-client");
            let source_url = format!("file://{}", fixture.path().join("source").display());
            fixture.oracle_ok_in(
                &client_path,
                &[
                    "-c",
                    "protocol.version=2",
                    "-c",
                    "fetch.uriProtocols=http,https",
                    "fetch",
                    &source_url,
                    "refs/heads/main:refs/remotes/origin/main",
                ],
            );
            repository_state(&client_path)
        },
    );
    assert!(
        pack_server.requests.load(Ordering::Relaxed) >= 1,
        "Git 2.55 oracle did not download the advertised packfile URI"
    );
}
