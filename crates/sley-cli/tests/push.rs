use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use sley_core::{Capability, ObjectFormat, ObjectId};
use sley_fetch::install_upload_pack_raw_response;
use sley_odb::FileObjectDatabase;
use sley_protocol::{
    ReceivePackCommand, ReceivePackCommandStatus, ReceivePackFeatures,
    ReceivePackPushRequestOptions, ReceivePackUnpackStatus, UploadPackAcknowledgment,
    UploadPackNegotiationRequest, UploadPackRequest, build_receive_pack_push_request,
    demux_upload_pack_packfile_response, read_receive_pack_report_status,
    read_ref_advertisement_set, read_upload_pack_packfile_response,
    read_upload_pack_raw_packfile_response, write_upload_pack_negotiation_request,
    write_upload_pack_request,
};

fn unique_temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("sley-{name}-{}-{nanos}", std::process::id()))
}

fn run(program: &str, cwd: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn run_with_env(program: &str, cwd: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .envs(envs.iter().copied())
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn run_success(program: &str, cwd: &Path, args: &[&str]) -> Vec<u8> {
    let output = run(program, cwd, args);
    assert!(
        output.status.success(),
        "{program} {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn assert_same_output(actual: Output, expected: Output, args: &[&str]) {
    assert_eq!(
        actual.status.code(),
        expected.status.code(),
        "status differed for {args:?}"
    );
    assert_eq!(
        actual.stdout, expected.stdout,
        "stdout differed for {args:?}"
    );
    assert_eq!(
        actual.stderr, expected.stderr,
        "stderr differed for {args:?}"
    );
}

fn run_with_stdin(program: &str, cwd: &Path, args: &[&str], input: &[u8]) -> Output {
    let mut child = Command::new(program)
        .current_dir(cwd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"));
    sley_testkit::write_stdin_tolerating_early_exit(
        child.stdin.as_mut().expect("child stdin"),
        input,
    );
    child
        .wait_with_output()
        .unwrap_or_else(|err| panic!("failed to wait for {program} {args:?}: {err}"))
}

fn parse_oid(format: ObjectFormat, output: &[u8]) -> ObjectId {
    let text = std::str::from_utf8(output)
        .expect("oid output is utf8")
        .trim();
    ObjectId::from_hex(format, text).expect("parse object id")
}

fn pack_objects(repo: &Path, revs: &str) -> Vec<u8> {
    let output = run_with_stdin(
        sley_testkit::oracle_git(),
        repo,
        &["pack-objects", "--stdout", "--revs"],
        revs.as_bytes(),
    );
    assert!(
        output.status.success(),
        "git pack-objects failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn create_work_repo(root: &Path, object_format: Option<&str>) {
    let mut init_args = vec!["init", "-q", "-b", "main"];
    if let Some(format) = object_format {
        init_args.push("--object-format");
        init_args.push(format);
    }
    run_success(sley_testkit::oracle_git(), root, &init_args);
    fs::write(root.join("payload.txt"), b"push payload\n").expect("write payload");
    run_success(sley_testkit::oracle_git(), root, &["add", "payload.txt"]);
    run_success(
        sley_testkit::oracle_git(),
        root,
        &[
            "-c",
            "user.name=Example User",
            "-c",
            "user.email=example@example.invalid",
            "commit",
            "-m",
            "initial",
            "-q",
        ],
    );
}

fn create_bare_repo(root: &Path, object_format: Option<&str>) {
    let mut init_args = vec!["init", "-q", "--bare", "-b", "main"];
    if let Some(format) = object_format {
        init_args.push("--object-format");
        init_args.push(format);
    }
    run_success(sley_testkit::oracle_git(), root, &init_args);
}

fn percent_encoded_file_url(path: &Path) -> String {
    format!("file://{}", path.to_string_lossy().replace(' ', "%20"))
}

fn ssh_url(path: &Path) -> String {
    format!("ssh://fake-host{}", path.to_string_lossy())
}

fn percent_encoded_ssh_url(path: &Path) -> String {
    format!(
        "ssh://fake-host{}",
        path.to_string_lossy().replace(' ', "%20")
    )
}

fn fake_ssh_script(root: &Path) -> PathBuf {
    let script = root.join("fake-ssh.sh");
    fs::write(
        &script,
        b"#!/bin/sh\nlast=''\nfor arg in \"$@\"; do last=$arg; done\neval \"exec $last\"\n",
    )
    .expect("write fake ssh script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&script).expect("stat fake ssh").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).expect("chmod fake ssh");
    }
    script
}

fn make_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path).expect("stat executable").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("chmod executable");
    }
}

fn loose_object_path(git_dir: &Path, oid: &str) -> PathBuf {
    git_dir.join("objects").join(&oid[..2]).join(&oid[2..])
}

fn repository_pack_indexes(git_dir: &Path) -> Vec<PathBuf> {
    let pack_dir = git_dir.join("objects").join("pack");
    fs::read_dir(&pack_dir)
        .expect("read pack dir")
        .map(|entry| entry.expect("read pack entry").path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("idx"))
        .collect()
}

fn repository_pack_pair(git_dir: &Path) -> (PathBuf, PathBuf) {
    let pack_dir = git_dir.join("objects").join("pack");
    let mut packs = Vec::new();
    let mut indexes = Vec::new();
    for entry in fs::read_dir(&pack_dir).expect("read pack dir") {
        let path = entry.expect("read pack entry").path();
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("pack") => packs.push(path),
            Some("idx") => indexes.push(path),
            _ => {}
        }
    }
    assert_eq!(
        packs.len(),
        1,
        "expected one pack in {}",
        pack_dir.display()
    );
    assert_eq!(
        indexes.len(),
        1,
        "expected one pack index in {}",
        pack_dir.display()
    );
    assert_eq!(
        packs[0].file_stem(),
        indexes[0].file_stem(),
        "pack and index stem should match"
    );
    (packs.remove(0), indexes.remove(0))
}

fn assert_remote_stored_pushed_objects_in_pack(remote: &Path) {
    let head = String::from_utf8(run_success(
        sley_testkit::oracle_git(),
        remote,
        &["rev-parse", "refs/heads/main"],
    ))
    .expect("head oid is utf8")
    .trim()
    .to_string();
    let tree = String::from_utf8(run_success(
        sley_testkit::oracle_git(),
        remote,
        &["rev-parse", "refs/heads/main^{tree}"],
    ))
    .expect("tree oid is utf8")
    .trim()
    .to_string();
    let blob = String::from_utf8(run_success(
        sley_testkit::oracle_git(),
        remote,
        &["rev-parse", "refs/heads/main:payload.txt"],
    ))
    .expect("blob oid is utf8")
    .trim()
    .to_string();
    let (_pack_path, index_path) = repository_pack_pair(remote);
    let index_arg = index_path.to_string_lossy();
    run_success(sley_testkit::oracle_git(), remote, &["verify-pack", "-v", &index_arg]);
    for oid in [&head, &tree, &blob] {
        assert!(
            !loose_object_path(remote, oid).exists(),
            "pushed object {oid} should be stored in pack, not as loose object"
        );
    }
}

fn assert_ref_missing(repo: &Path, name: &str) {
    let output = run(sley_testkit::oracle_git(), repo, &["show-ref", "--verify", name]);
    assert!(
        !output.status.success(),
        "{name} should be missing\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn receive_pack_service_updates_bare_repo_with_raw_pack() {
    let root = unique_temp_dir("receive-pack-service");
    fs::create_dir_all(&root).expect("create temp root");
    let work = root.join("work");
    let remote = root.join("remote.git");
    fs::create_dir_all(&work).expect("create work");
    fs::create_dir_all(&remote).expect("create remote");
    create_work_repo(&work, None);
    create_bare_repo(&remote, None);

    let remote_arg = remote.to_string_lossy();
    run_success(sley_testkit::oracle_git(), &work, &["push", "-q", &remote_arg, "main"]);
    fs::write(work.join("payload.txt"), b"receive-pack update\n").expect("write update");
    run_success(sley_testkit::oracle_git(), &work, &["add", "payload.txt"]);
    run_success(
        sley_testkit::oracle_git(),
        &work,
        &[
            "-c",
            "user.name=Example User",
            "-c",
            "user.email=example@example.invalid",
            "commit",
            "-m",
            "update",
            "-q",
        ],
    );

    let old_id = parse_oid(
        ObjectFormat::Sha1,
        &run_success(sley_testkit::oracle_git(), &remote, &["rev-parse", "refs/heads/main"]),
    );
    let new_id = parse_oid(
        ObjectFormat::Sha1,
        &run_success(sley_testkit::oracle_git(), &work, &["rev-parse", "refs/heads/main"]),
    );
    let packfile = pack_objects(&work, &format!("{new_id}\n"));
    let features = ReceivePackFeatures {
        report_status: true,
        delete_refs: true,
        ofs_delta: true,
        quiet: true,
        object_format: Some(ObjectFormat::Sha1),
        ..ReceivePackFeatures::default()
    };
    let request = build_receive_pack_push_request(
        &features,
        vec![ReceivePackCommand {
            old_id,
            new_id: new_id.clone(),
            name: "refs/heads/main".into(),
        }],
        packfile,
        ReceivePackPushRequestOptions {
            report_status: true,
            ofs_delta: true,
            object_format: Some(ObjectFormat::Sha1),
            ..ReceivePackPushRequestOptions::default()
        },
    )
    .expect("build receive-pack request");
    let mut encoded_request = Vec::new();
    sley_protocol::write_receive_pack_push_request(&mut encoded_request, &request)
        .expect("encode receive-pack request");

    let output = run_with_stdin(
        env!("CARGO_BIN_EXE_sley"),
        &root,
        &["receive-pack", &remote_arg],
        &encoded_request,
    );
    assert!(
        output.status.success(),
        "receive-pack failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let mut stdout = output.stdout.as_slice();
    let advertisements = read_ref_advertisement_set(ObjectFormat::Sha1, &mut stdout)
        .expect("parse receive-pack advertisements");
    assert!(
        advertisements.refs[0]
            .capabilities
            .iter()
            .any(|capability| capability.name == "report-status")
    );
    assert!(advertisements.refs[0].capabilities.contains(&Capability {
        name: "object-format".into(),
        value: Some("sha1".into()),
    }));
    let report = read_receive_pack_report_status(&mut stdout).expect("parse report status");
    assert_eq!(report.unpack, ReceivePackUnpackStatus::Ok);
    assert_eq!(
        report.commands,
        vec![ReceivePackCommandStatus::Ok {
            name: "refs/heads/main".into(),
        }]
    );

    let remote_head = parse_oid(
        ObjectFormat::Sha1,
        &run_success(sley_testkit::oracle_git(), &remote, &["rev-parse", "refs/heads/main"]),
    );
    assert_eq!(remote_head, new_id);
    run_success(
        sley_testkit::oracle_git(),
        &remote,
        &["cat-file", "-p", "refs/heads/main:payload.txt"],
    );
    assert_remote_stored_pushed_objects_in_pack(&remote);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn receive_pack_service_accepts_push_options() {
    let root = unique_temp_dir("receive-pack-push-options-service");
    fs::create_dir_all(&root).expect("create temp root");
    let remote = root.join("remote.git");
    let work = root.join("work");
    let remote_arg = remote.to_string_lossy().to_string();
    fs::create_dir_all(&remote).expect("create remote");
    fs::create_dir_all(&work).expect("create work");
    create_bare_repo(&remote, None);
    create_work_repo(&work, None);
    run_success(sley_testkit::oracle_git(), &work, &["remote", "add", "origin", &remote_arg]);
    run_success(sley_testkit::oracle_git(), &work, &["push", "-q", "origin", "main"]);

    fs::write(work.join("payload.txt"), b"receive-pack push option\n").expect("write update");
    run_success(sley_testkit::oracle_git(), &work, &["add", "payload.txt"]);
    run_success(
        sley_testkit::oracle_git(),
        &work,
        &[
            "-c",
            "user.name=Example User",
            "-c",
            "user.email=example@example.invalid",
            "commit",
            "-m",
            "push option",
            "-q",
        ],
    );

    let old_id = parse_oid(
        ObjectFormat::Sha1,
        &run_success(sley_testkit::oracle_git(), &remote, &["rev-parse", "refs/heads/main"]),
    );
    let new_id = parse_oid(
        ObjectFormat::Sha1,
        &run_success(sley_testkit::oracle_git(), &work, &["rev-parse", "refs/heads/main"]),
    );
    let packfile = pack_objects(&work, &format!("{new_id}\n"));
    let features = ReceivePackFeatures {
        report_status: true,
        delete_refs: true,
        ofs_delta: true,
        push_options: true,
        quiet: true,
        object_format: Some(ObjectFormat::Sha1),
        ..ReceivePackFeatures::default()
    };
    let request = build_receive_pack_push_request(
        &features,
        vec![ReceivePackCommand {
            old_id,
            new_id: new_id.clone(),
            name: "refs/heads/main".into(),
        }],
        packfile,
        ReceivePackPushRequestOptions {
            report_status: true,
            ofs_delta: true,
            object_format: Some(ObjectFormat::Sha1),
            push_options: vec!["ci.skip".into()],
            ..ReceivePackPushRequestOptions::default()
        },
    )
    .expect("build receive-pack request with push-options");
    let mut encoded_request = Vec::new();
    sley_protocol::write_receive_pack_push_request(&mut encoded_request, &request)
        .expect("encode receive-pack request");

    let output = run_with_stdin(
        env!("CARGO_BIN_EXE_sley"),
        &root,
        &["receive-pack", &remote_arg],
        &encoded_request,
    );
    assert!(
        output.status.success(),
        "receive-pack failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let mut stdout = output.stdout.as_slice();
    let advertisements = read_ref_advertisement_set(ObjectFormat::Sha1, &mut stdout)
        .expect("parse receive-pack advertisements");
    assert!(advertisements.refs[0].capabilities.contains(&Capability {
        name: "push-options".into(),
        value: None,
    }));
    let report = read_receive_pack_report_status(&mut stdout).expect("parse report status");
    assert_eq!(report.unpack, ReceivePackUnpackStatus::Ok);
    assert_eq!(
        report.commands,
        vec![ReceivePackCommandStatus::Ok {
            name: "refs/heads/main".into(),
        }]
    );

    let remote_head = parse_oid(
        ObjectFormat::Sha1,
        &run_success(sley_testkit::oracle_git(), &remote, &["rev-parse", "refs/heads/main"]),
    );
    assert_eq!(remote_head, new_id);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn receive_pack_service_accepts_empty_push_options_section() {
    let root = unique_temp_dir("receive-pack-empty-push-options-service");
    fs::create_dir_all(&root).expect("create temp root");
    let remote = root.join("remote.git");
    let work = root.join("work");
    let remote_arg = remote.to_string_lossy().to_string();
    fs::create_dir_all(&remote).expect("create remote");
    fs::create_dir_all(&work).expect("create work");
    create_bare_repo(&remote, None);
    create_work_repo(&work, None);
    run_success(sley_testkit::oracle_git(), &work, &["remote", "add", "origin", &remote_arg]);
    run_success(sley_testkit::oracle_git(), &work, &["push", "-q", "origin", "main"]);

    fs::write(work.join("payload.txt"), b"empty push options section\n").expect("write update");
    run_success(sley_testkit::oracle_git(), &work, &["add", "payload.txt"]);
    run_success(
        sley_testkit::oracle_git(),
        &work,
        &[
            "-c",
            "user.name=Example User",
            "-c",
            "user.email=example@example.invalid",
            "commit",
            "-m",
            "empty push options",
            "-q",
        ],
    );

    let old_id = parse_oid(
        ObjectFormat::Sha1,
        &run_success(sley_testkit::oracle_git(), &remote, &["rev-parse", "refs/heads/main"]),
    );
    let new_id = parse_oid(
        ObjectFormat::Sha1,
        &run_success(sley_testkit::oracle_git(), &work, &["rev-parse", "refs/heads/main"]),
    );
    let packfile = pack_objects(&work, &format!("{new_id}\n"));
    let request = sley_protocol::ReceivePackPushRequest {
        commands: sley_protocol::ReceivePackRequest {
            shallow: Vec::new(),
            commands: vec![ReceivePackCommand {
                old_id,
                new_id: new_id.clone(),
                name: "refs/heads/main".into(),
            }],
            capabilities: vec![
                Capability {
                    name: "report-status".into(),
                    value: None,
                },
                Capability {
                    name: "ofs-delta".into(),
                    value: None,
                },
                Capability {
                    name: "push-options".into(),
                    value: None,
                },
                Capability {
                    name: "object-format".into(),
                    value: Some("sha1".into()),
                },
            ],
        },
        push_options: Some(Vec::new()),
        packfile,
    };
    let mut encoded_request = Vec::new();
    sley_protocol::write_receive_pack_push_request(&mut encoded_request, &request)
        .expect("encode receive-pack request");

    let output = run_with_stdin(
        env!("CARGO_BIN_EXE_sley"),
        &root,
        &["receive-pack", &remote_arg],
        &encoded_request,
    );
    assert!(
        output.status.success(),
        "receive-pack failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let mut stdout = output.stdout.as_slice();
    let advertisements = read_ref_advertisement_set(ObjectFormat::Sha1, &mut stdout)
        .expect("parse receive-pack advertisements");
    assert!(advertisements.refs[0].capabilities.contains(&Capability {
        name: "push-options".into(),
        value: None,
    }));
    let report = read_receive_pack_report_status(&mut stdout).expect("parse report status");
    assert_eq!(report.unpack, ReceivePackUnpackStatus::Ok);
    assert_eq!(
        report.commands,
        vec![ReceivePackCommandStatus::Ok {
            name: "refs/heads/main".into(),
        }]
    );

    let remote_head = parse_oid(
        ObjectFormat::Sha1,
        &run_success(sley_testkit::oracle_git(), &remote, &["rev-parse", "refs/heads/main"]),
    );
    assert_eq!(remote_head, new_id);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn upload_pack_service_serves_raw_pack() {
    let root = unique_temp_dir("upload-pack-service");
    fs::create_dir_all(&root).expect("create temp root");
    let source = root.join("source");
    let receiver = root.join("receiver.git");
    fs::create_dir_all(&source).expect("create source");
    fs::create_dir_all(&receiver).expect("create receiver");
    create_work_repo(&source, None);
    create_bare_repo(&receiver, None);

    let head = parse_oid(
        ObjectFormat::Sha1,
        &run_success(sley_testkit::oracle_git(), &source, &["rev-parse", "HEAD"]),
    );
    let request = UploadPackRequest {
        wants: vec![head.clone()],
        capabilities: vec![Capability {
            name: "object-format".into(),
            value: Some("sha1".into()),
        }],
        ..UploadPackRequest::default()
    };
    let mut encoded_request = Vec::new();
    write_upload_pack_request(&mut encoded_request, Some(&request))
        .expect("encode upload-pack request");
    write_upload_pack_negotiation_request(
        &mut encoded_request,
        &UploadPackNegotiationRequest {
            haves: vec![
                ObjectId::from_hex(
                    ObjectFormat::Sha1,
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                )
                .expect("parse unknown have"),
            ],
            done: true,
        },
    )
    .expect("encode upload-pack done");

    let source_arg = source.to_string_lossy();
    let output = run_with_stdin(
        env!("CARGO_BIN_EXE_sley"),
        &root,
        &["upload-pack", &source_arg],
        &encoded_request,
    );
    assert!(
        output.status.success(),
        "upload-pack failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let mut stdout = output.stdout.as_slice();
    let advertisements = read_ref_advertisement_set(ObjectFormat::Sha1, &mut stdout)
        .expect("parse upload-pack advertisements");
    assert!(
        advertisements
            .refs
            .iter()
            .any(|advertisement| advertisement.name == "HEAD" && advertisement.oid == head)
    );
    assert!(advertisements.refs[0].capabilities.contains(&Capability {
        name: "object-format".into(),
        value: Some("sha1".into()),
    }));
    let response = read_upload_pack_raw_packfile_response(ObjectFormat::Sha1, &mut stdout)
        .expect("parse upload-pack response");
    assert_eq!(
        response.acknowledgments,
        vec![UploadPackAcknowledgment::Nak]
    );

    let receiver_db = FileObjectDatabase::from_git_dir(&receiver, ObjectFormat::Sha1);
    install_upload_pack_raw_response(&response, &receiver_db)
        .expect("install upload-pack response pack");
    assert!(
        receiver_db
            .contains(&head)
            .expect("read receiver object db")
    );
    assert!(
        !loose_object_path(&receiver, &head.to_string()).exists(),
        "upload-pack response should install as pack, not loose object"
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn upload_pack_service_serves_sideband_64k_pack() {
    let root = unique_temp_dir("upload-pack-sideband-service");
    fs::create_dir_all(&root).expect("create temp root");
    let source = root.join("source");
    let receiver = root.join("receiver.git");
    fs::create_dir_all(&source).expect("create source");
    fs::create_dir_all(&receiver).expect("create receiver");
    create_work_repo(&source, None);
    create_bare_repo(&receiver, None);

    let head = parse_oid(
        ObjectFormat::Sha1,
        &run_success(sley_testkit::oracle_git(), &source, &["rev-parse", "HEAD"]),
    );
    let request = UploadPackRequest {
        wants: vec![head.clone()],
        capabilities: vec![
            Capability {
                name: "side-band-64k".into(),
                value: None,
            },
            Capability {
                name: "object-format".into(),
                value: Some("sha1".into()),
            },
        ],
        ..UploadPackRequest::default()
    };
    let mut encoded_request = Vec::new();
    write_upload_pack_request(&mut encoded_request, Some(&request))
        .expect("encode upload-pack request");
    write_upload_pack_negotiation_request(
        &mut encoded_request,
        &UploadPackNegotiationRequest {
            haves: Vec::new(),
            done: true,
        },
    )
    .expect("encode upload-pack done");

    let source_arg = source.to_string_lossy();
    let output = run_with_stdin(
        env!("CARGO_BIN_EXE_sley"),
        &root,
        &["upload-pack", &source_arg],
        &encoded_request,
    );
    assert!(
        output.status.success(),
        "upload-pack failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let mut stdout = output.stdout.as_slice();
    let advertisements = read_ref_advertisement_set(ObjectFormat::Sha1, &mut stdout)
        .expect("parse upload-pack advertisements");
    assert!(advertisements.refs[0].capabilities.contains(&Capability {
        name: "side-band-64k".into(),
        value: None,
    }));

    let response = read_upload_pack_packfile_response(ObjectFormat::Sha1, &mut stdout)
        .expect("parse upload-pack sideband response");
    assert_eq!(
        response.acknowledgments,
        vec![UploadPackAcknowledgment::Nak]
    );
    let demuxed = demux_upload_pack_packfile_response(&response).expect("demux sideband response");
    assert!(demuxed.data.starts_with(b"PACK"));

    let receiver_db = FileObjectDatabase::from_git_dir(&receiver, ObjectFormat::Sha1);
    receiver_db
        .install_raw_pack(&demuxed.data)
        .expect("install sideband pack");
    assert!(
        receiver_db
            .contains(&head)
            .expect("read receiver object db")
    );
    assert!(
        !loose_object_path(&receiver, &head.to_string()).exists(),
        "upload-pack sideband response should install as pack, not loose object"
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn push_local_branch_to_bare_repo_matches_upstream_ref_and_objects() {
    let root = unique_temp_dir("push-local-branch");
    fs::create_dir_all(&root).expect("create temp root");
    let work = root.join("work");
    let remote = root.join("remote.git");
    fs::create_dir_all(&work).expect("create work");
    fs::create_dir_all(&remote).expect("create remote");
    create_work_repo(&work, None);
    create_bare_repo(&remote, None);

    let remote_arg = remote.to_string_lossy();
    run_success(
        env!("CARGO_BIN_EXE_sley"),
        &work,
        &["push", "-q", &remote_arg, "main"],
    );

    let local_head = run_success(sley_testkit::oracle_git(), &work, &["rev-parse", "refs/heads/main"]);
    let remote_head = run_success(sley_testkit::oracle_git(), &remote, &["rev-parse", "refs/heads/main"]);
    assert_eq!(remote_head, local_head);
    run_success(
        sley_testkit::oracle_git(),
        &remote,
        &["cat-file", "-e", "refs/heads/main^{tree}"],
    );
    assert_remote_stored_pushed_objects_in_pack(&remote);
}

#[test]
fn failing_pre_push_hook_leaves_remote_ref_unchanged_and_runs_at_worktree_root() {
    let root = unique_temp_dir("push-pre-push-fails-before-ref-update");
    fs::create_dir_all(&root).expect("create temp root");
    let work = root.join("work");
    let remote = root.join("remote.git");
    fs::create_dir_all(&work).expect("create work");
    fs::create_dir_all(&remote).expect("create remote");
    create_work_repo(&work, None);
    create_bare_repo(&remote, None);

    let remote_arg = remote.to_string_lossy();
    run_success(env!("CARGO_BIN_EXE_sley"), &work, &["push", "-q", &remote_arg, "main"]);
    let old_remote_head = run_success(
        sley_testkit::oracle_git(),
        &remote,
        &["rev-parse", "refs/heads/main"],
    );

    fs::write(work.join("payload.txt"), b"blocked by pre-push\n").expect("write update");
    run_success(sley_testkit::oracle_git(), &work, &["add", "payload.txt"]);
    run_success(
        sley_testkit::oracle_git(),
        &work,
        &[
            "-c",
            "user.name=Example User",
            "-c",
            "user.email=example@example.invalid",
            "commit",
            "-m",
            "blocked update",
            "-q",
        ],
    );
    let local_head = run_success(
        sley_testkit::oracle_git(),
        &work,
        &["rev-parse", "refs/heads/main"],
    );
    assert_ne!(local_head, old_remote_head);

    let hooks_dir = work.join(".git").join("hooks");
    fs::create_dir_all(&hooks_dir).expect("create hooks dir");
    let hook = hooks_dir.join("pre-push");
    fs::write(&hook, b"#!/bin/sh\npwd >hook.cwd\nexit 12\n").expect("write pre-push");
    make_executable(&hook);

    let subdir = work.join("subdir");
    fs::create_dir_all(&subdir).expect("create subdir");
    let output = run(
        env!("CARGO_BIN_EXE_sley"),
        &subdir,
        &["push", "-q", &remote_arg, "main"],
    );
    assert!(
        !output.status.success(),
        "failing pre-push should abort push\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "hook failures should be normalized to exit 1"
    );
    assert_eq!(
        run_success(
            sley_testkit::oracle_git(),
            &remote,
            &["rev-parse", "refs/heads/main"]
        ),
        old_remote_head,
        "remote ref must be unchanged when pre-push fails"
    );
    let hook_cwd = fs::read_to_string(work.join("hook.cwd")).expect("read hook cwd");
    assert_eq!(
        fs::canonicalize(hook_cwd.trim()).expect("canonical hook cwd"),
        fs::canonicalize(&work).expect("canonical worktree root"),
        "pre-push should run from the worktree root"
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn push_ssh_branch_to_bare_repo_matches_upstream_git_protocol_v0() {
    let root = unique_temp_dir("push-ssh-branch");
    fs::create_dir_all(&root).expect("create temp root");
    let expected_work = root.join("expected-work");
    let actual_work = root.join("actual-work");
    let expected_remote = root.join("expected.git");
    let actual_remote = root.join("actual.git");
    fs::create_dir_all(&expected_work).expect("create expected work");
    fs::create_dir_all(&actual_work).expect("create actual work");
    fs::create_dir_all(&expected_remote).expect("create expected remote");
    fs::create_dir_all(&actual_remote).expect("create actual remote");
    {
        create_work_repo(&expected_work, None);
        create_work_repo(&actual_work, None);
        create_bare_repo(&expected_remote, None);
        create_bare_repo(&actual_remote, None);
        let fake_ssh = fake_ssh_script(&root);
        let fake_ssh = fake_ssh.to_str().expect("fake ssh path is utf8");
        let expected_url = ssh_url(&expected_remote);
        let actual_url = ssh_url(&actual_remote);
        let expected_args = [
            "-c",
            "protocol.version=0",
            "push",
            "-q",
            expected_url.as_str(),
            "main",
        ];
        let actual_args = ["push", "-q", actual_url.as_str(), "main"];

        let expected = run_with_env(
            sley_testkit::oracle_git(),
            &expected_work,
            &expected_args,
            &[("GIT_SSH", fake_ssh)],
        );
        let actual = run_with_env(
            env!("CARGO_BIN_EXE_sley"),
            &actual_work,
            &actual_args,
            &[("GIT_SSH", fake_ssh)],
        );
        assert_same_output(actual, expected, &actual_args);

        let expected_head = run_success(sley_testkit::oracle_git(), &expected_work, &["rev-parse", "refs/heads/main"]);
        assert_eq!(
            run_success(sley_testkit::oracle_git(), &expected_remote, &["rev-parse", "refs/heads/main"]),
            expected_head
        );
        let actual_work_head = run_success(sley_testkit::oracle_git(), &actual_work, &["rev-parse", "refs/heads/main"]);
        let actual_head = run_success(sley_testkit::oracle_git(), &actual_remote, &["rev-parse", "refs/heads/main"]);
        assert_eq!(actual_head, actual_work_head);
        run_success(
            sley_testkit::oracle_git(),
            &actual_remote,
            &["cat-file", "-p", "refs/heads/main:payload.txt"],
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn push_configured_percent_encoded_ssh_remote_matches_upstream_git_protocol_v0() {
    let root = unique_temp_dir("push-ssh-configured-percent");
    fs::create_dir_all(&root).expect("create temp root");
    let expected_work = root.join("expected-work");
    let actual_work = root.join("actual-work");
    let expected_remote = root.join("expected remote.git");
    let actual_remote = root.join("actual remote.git");
    fs::create_dir_all(&expected_work).expect("create expected work");
    fs::create_dir_all(&actual_work).expect("create actual work");
    fs::create_dir_all(&expected_remote).expect("create expected remote");
    fs::create_dir_all(&actual_remote).expect("create actual remote");
    {
        create_work_repo(&expected_work, None);
        create_work_repo(&actual_work, None);
        create_bare_repo(&expected_remote, None);
        create_bare_repo(&actual_remote, None);
        let fake_ssh = fake_ssh_script(&root);
        let fake_ssh = fake_ssh.to_str().expect("fake ssh path is utf8");
        let expected_url = percent_encoded_ssh_url(&expected_remote);
        let actual_url = percent_encoded_ssh_url(&actual_remote);
        run_success(
            sley_testkit::oracle_git(),
            &expected_work,
            &["remote", "add", "origin", expected_url.as_str()],
        );
        run_success(
            sley_testkit::oracle_git(),
            &actual_work,
            &["remote", "add", "origin", actual_url.as_str()],
        );
        let expected_args = ["-c", "protocol.version=0", "push", "-q", "origin", "main"];
        let actual_args = ["push", "-q", "origin", "main"];

        let expected = run_with_env(
            sley_testkit::oracle_git(),
            &expected_work,
            &expected_args,
            &[("GIT_SSH", fake_ssh)],
        );
        let actual = run_with_env(
            env!("CARGO_BIN_EXE_sley"),
            &actual_work,
            &actual_args,
            &[("GIT_SSH", fake_ssh)],
        );
        assert_same_output(actual, expected, &actual_args);
        let expected_head = run_success(sley_testkit::oracle_git(), &expected_work, &["rev-parse", "refs/heads/main"]);
        assert_eq!(
            run_success(sley_testkit::oracle_git(), &expected_remote, &["rev-parse", "refs/heads/main"]),
            expected_head
        );
        let actual_work_head = run_success(sley_testkit::oracle_git(), &actual_work, &["rev-parse", "refs/heads/main"]);
        let actual_head = run_success(sley_testkit::oracle_git(), &actual_remote, &["rev-parse", "refs/heads/main"]);
        assert_eq!(actual_head, actual_work_head);
        run_success(
            sley_testkit::oracle_git(),
            &actual_remote,
            &["cat-file", "-p", "refs/heads/main:payload.txt"],
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn push_configured_percent_encoded_file_remote_updates_bare_repo() {
    let root = unique_temp_dir("push-configured-percent-file");
    fs::create_dir_all(&root).expect("create temp root");
    let work = root.join("work");
    let remote = root.join("remote repo.git");
    fs::create_dir_all(&work).expect("create work");
    fs::create_dir_all(&remote).expect("create remote");
    {
        create_work_repo(&work, None);
        create_bare_repo(&remote, None);
        let remote_url = percent_encoded_file_url(&remote);
        run_success(
            sley_testkit::oracle_git(),
            &work,
            &["remote", "add", "origin", remote_url.as_str()],
        );

        let actual = run(
            env!("CARGO_BIN_EXE_sley"),
            &work,
            &["push", "-q", "origin", "main"],
        );
        assert!(
            actual.status.success(),
            "sley push failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
            actual.status.code(),
            String::from_utf8_lossy(&actual.stdout),
            String::from_utf8_lossy(&actual.stderr)
        );
        let remote_head = run_success(sley_testkit::oracle_git(), &remote, &["rev-parse", "refs/heads/main"]);
        let local_head = run_success(sley_testkit::oracle_git(), &work, &["rev-parse", "refs/heads/main"]);
        assert_eq!(remote_head, local_head);
        assert_remote_stored_pushed_objects_in_pack(&remote);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn push_rejects_non_fast_forward_without_force() {
    let root = unique_temp_dir("push-reject-non-fast-forward");
    fs::create_dir_all(&root).expect("create temp root");
    let work = root.join("work");
    let other = root.join("other");
    let remote = root.join("remote.git");
    fs::create_dir_all(&work).expect("create work");
    fs::create_dir_all(&remote).expect("create remote");
    {
        create_work_repo(&work, None);
        create_bare_repo(&remote, None);
        let remote_arg = remote.to_string_lossy();
        run_success(sley_testkit::oracle_git(), &work, &["push", "-q", &remote_arg, "main"]);
        run_success(sley_testkit::oracle_git(), &root, &["clone", "-q", &remote_arg, "other"]);

        fs::write(other.join("payload.txt"), b"remote side\n").expect("write remote side");
        run_success(sley_testkit::oracle_git(), &other, &["add", "payload.txt"]);
        run_success(
            sley_testkit::oracle_git(),
            &other,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "remote side",
                "-q",
            ],
        );
        run_success(sley_testkit::oracle_git(), &other, &["push", "-q", "origin", "main"]);
        let remote_head = run_success(sley_testkit::oracle_git(), &remote, &["rev-parse", "refs/heads/main"]);

        fs::write(work.join("payload.txt"), b"local side\n").expect("write local side");
        run_success(sley_testkit::oracle_git(), &work, &["add", "payload.txt"]);
        run_success(
            sley_testkit::oracle_git(),
            &work,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "local side",
                "-q",
            ],
        );
        let local_head = run_success(sley_testkit::oracle_git(), &work, &["rev-parse", "refs/heads/main"]);

        let actual = run(
            env!("CARGO_BIN_EXE_sley"),
            &work,
            &["push", "-q", &remote_arg, "main"],
        );
        assert!(
            !actual.status.success(),
            "non-fast-forward push should fail\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&actual.stdout),
            String::from_utf8_lossy(&actual.stderr)
        );
        assert!(
            String::from_utf8_lossy(&actual.stderr).contains("non-fast-forward"),
            "stderr should mention non-fast-forward, got {}",
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(
            run_success(sley_testkit::oracle_git(), &remote, &["rev-parse", "refs/heads/main"]),
            remote_head
        );
        assert_ne!(remote_head, local_head);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn push_force_refspec_updates_non_fast_forward_branch() {
    let root = unique_temp_dir("push-force-non-fast-forward");
    fs::create_dir_all(&root).expect("create temp root");
    let work = root.join("work");
    let other = root.join("other");
    let remote = root.join("remote.git");
    fs::create_dir_all(&work).expect("create work");
    fs::create_dir_all(&remote).expect("create remote");
    {
        create_work_repo(&work, None);
        create_bare_repo(&remote, None);
        let remote_arg = remote.to_string_lossy();
        run_success(sley_testkit::oracle_git(), &work, &["push", "-q", &remote_arg, "main"]);
        run_success(sley_testkit::oracle_git(), &root, &["clone", "-q", &remote_arg, "other"]);

        fs::write(other.join("payload.txt"), b"remote side\n").expect("write remote side");
        run_success(sley_testkit::oracle_git(), &other, &["add", "payload.txt"]);
        run_success(
            sley_testkit::oracle_git(),
            &other,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "remote side",
                "-q",
            ],
        );
        run_success(sley_testkit::oracle_git(), &other, &["push", "-q", "origin", "main"]);

        fs::write(work.join("payload.txt"), b"local side\n").expect("write local side");
        run_success(sley_testkit::oracle_git(), &work, &["add", "payload.txt"]);
        run_success(
            sley_testkit::oracle_git(),
            &work,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "local side",
                "-q",
            ],
        );
        let local_head = run_success(sley_testkit::oracle_git(), &work, &["rev-parse", "refs/heads/main"]);

        run_success(
            env!("CARGO_BIN_EXE_sley"),
            &work,
            &["push", "-q", &remote_arg, "+main"],
        );
        assert_eq!(
            run_success(sley_testkit::oracle_git(), &remote, &["rev-parse", "refs/heads/main"]),
            local_head
        );
        assert_remote_stored_pushed_objects_in_pack(&remote);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn push_update_pack_excludes_objects_reachable_from_remote_refs() {
    let root = unique_temp_dir("push-update-excludes-remote");
    fs::create_dir_all(&root).expect("create temp root");
    let work = root.join("work");
    let remote = root.join("remote.git");
    fs::create_dir_all(&work).expect("create work");
    fs::create_dir_all(&remote).expect("create remote");
    create_work_repo(&work, None);
    create_bare_repo(&remote, None);

    let remote_arg = remote.to_string_lossy();
    run_success(
        env!("CARGO_BIN_EXE_sley"),
        &work,
        &["push", "-q", &remote_arg, "main"],
    );
    let old_head = String::from_utf8(run_success(
        sley_testkit::oracle_git(),
        &remote,
        &["rev-parse", "refs/heads/main"],
    ))
    .expect("old head is utf8")
    .trim()
    .to_string();
    let old_tree = String::from_utf8(run_success(
        sley_testkit::oracle_git(),
        &remote,
        &["rev-parse", "refs/heads/main^{tree}"],
    ))
    .expect("old tree is utf8")
    .trim()
    .to_string();
    let old_blob = String::from_utf8(run_success(
        sley_testkit::oracle_git(),
        &remote,
        &["rev-parse", "refs/heads/main:payload.txt"],
    ))
    .expect("old blob is utf8")
    .trim()
    .to_string();
    let before_indexes = repository_pack_indexes(&remote)
        .into_iter()
        .collect::<HashSet<_>>();

    run_success(
        sley_testkit::oracle_git(),
        &work,
        &[
            "-c",
            "user.name=Example User",
            "-c",
            "user.email=example@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "metadata only",
            "-q",
        ],
    );
    let new_head = String::from_utf8(run_success(sley_testkit::oracle_git(), &work, &["rev-parse", "refs/heads/main"]))
        .expect("new head is utf8")
        .trim()
        .to_string();
    run_success(
        env!("CARGO_BIN_EXE_sley"),
        &work,
        &["push", "-q", &remote_arg, "main"],
    );

    let after_indexes = repository_pack_indexes(&remote)
        .into_iter()
        .collect::<HashSet<_>>();
    let new_indexes = after_indexes
        .difference(&before_indexes)
        .collect::<Vec<_>>();
    assert_eq!(new_indexes.len(), 1, "expected one new push pack index");
    let index_arg = new_indexes[0].to_string_lossy();
    let verify = String::from_utf8(run_success(
        sley_testkit::oracle_git(),
        &remote,
        &["verify-pack", "-v", &index_arg],
    ))
    .expect("verify-pack output is utf8");
    assert!(
        verify.contains(&new_head),
        "new push pack should contain new commit {new_head}\n{verify}"
    );
    for reused in [&old_head, &old_tree, &old_blob] {
        assert!(
            !verify.contains(reused),
            "new push pack should exclude already-remote object {reused}\n{verify}"
        );
    }
}

#[test]
fn push_delete_refspec_removes_remote_branch() {
    let root = unique_temp_dir("push-delete-refspec");
    fs::create_dir_all(&root).expect("create temp root");
    let work = root.join("work");
    let remote = root.join("remote.git");
    fs::create_dir_all(&work).expect("create work");
    fs::create_dir_all(&remote).expect("create remote");
    create_work_repo(&work, None);
    create_bare_repo(&remote, None);

    let remote_arg = remote.to_string_lossy();
    run_success(
        env!("CARGO_BIN_EXE_sley"),
        &work,
        &["push", "-q", &remote_arg, "main"],
    );
    run_success(sley_testkit::oracle_git(), &remote, &["branch", "old", "main"]);

    run_success(
        env!("CARGO_BIN_EXE_sley"),
        &work,
        &["push", "-q", &remote_arg, ":old"],
    );

    assert_ref_missing(&remote, "refs/heads/old");
}

#[test]
fn push_delete_option_removes_remote_branch() {
    let root = unique_temp_dir("push-delete-option");
    fs::create_dir_all(&root).expect("create temp root");
    let work = root.join("work");
    let remote = root.join("remote.git");
    fs::create_dir_all(&work).expect("create work");
    fs::create_dir_all(&remote).expect("create remote");
    create_work_repo(&work, None);
    create_bare_repo(&remote, None);

    let remote_arg = remote.to_string_lossy();
    run_success(
        env!("CARGO_BIN_EXE_sley"),
        &work,
        &["push", "-q", &remote_arg, "main"],
    );
    run_success(sley_testkit::oracle_git(), &remote, &["branch", "old", "main"]);

    run_success(
        env!("CARGO_BIN_EXE_sley"),
        &work,
        &["push", "-q", "--delete", &remote_arg, "old"],
    );

    assert_ref_missing(&remote, "refs/heads/old");
}

#[test]
fn push_delete_missing_remote_branch_fails() {
    let root = unique_temp_dir("push-delete-missing");
    fs::create_dir_all(&root).expect("create temp root");
    let work = root.join("work");
    let remote = root.join("remote.git");
    fs::create_dir_all(&work).expect("create work");
    fs::create_dir_all(&remote).expect("create remote");
    create_work_repo(&work, None);
    create_bare_repo(&remote, None);

    let remote_arg = remote.to_string_lossy();
    let output = run(
        env!("CARGO_BIN_EXE_sley"),
        &work,
        &["push", "-q", &remote_arg, ":missing"],
    );

    assert!(
        !output.status.success(),
        "delete of missing remote branch should fail"
    );
}

#[test]
fn push_sha256_branch_to_file_remote_sets_upstream() {
    let root = unique_temp_dir("push-sha256-file");
    fs::create_dir_all(&root).expect("create temp root");
    let work = root.join("work");
    let remote = root.join("remote.git");
    fs::create_dir_all(&work).expect("create work");
    fs::create_dir_all(&remote).expect("create remote");
    create_work_repo(&work, Some("sha256"));
    create_bare_repo(&remote, Some("sha256"));

    let remote_url = format!("file://{}", remote.display());
    run_success(
        env!("CARGO_BIN_EXE_sley"),
        &work,
        &["push", "-q", "-u", &remote_url, "main"],
    );

    let local_head = run_success(sley_testkit::oracle_git(), &work, &["rev-parse", "refs/heads/main"]);
    let remote_head = run_success(sley_testkit::oracle_git(), &remote, &["rev-parse", "refs/heads/main"]);
    assert_eq!(remote_head, local_head);
    assert_eq!(
        run_success(sley_testkit::oracle_git(), &work, &["config", "--get", "branch.main.remote"]),
        remote_url
            .as_bytes()
            .iter()
            .copied()
            .chain(std::iter::once(b'\n'))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        run_success(sley_testkit::oracle_git(), &work, &["config", "--get", "branch.main.merge"]),
        b"refs/heads/main\n"
    );
    assert_remote_stored_pushed_objects_in_pack(&remote);
}

#[test]
fn push_delete_sha256_remote_branch() {
    let root = unique_temp_dir("push-delete-sha256");
    fs::create_dir_all(&root).expect("create temp root");
    let work = root.join("work");
    let remote = root.join("remote.git");
    fs::create_dir_all(&work).expect("create work");
    fs::create_dir_all(&remote).expect("create remote");
    create_work_repo(&work, Some("sha256"));
    create_bare_repo(&remote, Some("sha256"));

    let remote_url = format!("file://{}", remote.display());
    run_success(
        env!("CARGO_BIN_EXE_sley"),
        &work,
        &["push", "-q", &remote_url, "main"],
    );
    run_success(sley_testkit::oracle_git(), &remote, &["branch", "old", "main"]);

    run_success(
        env!("CARGO_BIN_EXE_sley"),
        &work,
        &["push", "-q", "--delete", &remote_url, "old"],
    );

    assert_ref_missing(&remote, "refs/heads/old");
}
