//! Production-binary checks for the stock-git wire fixes: v0 advertisement,
//! stateless upload-pack, `ls-remote --upload-pack`, and `clone -u` on file://.
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

fn unique_temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("sley-{name}-{}-{nanos}", std::process::id()))
}

fn run(cwd: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new(sley_testkit::sley_bin!())
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {args:?}: {err}"));
    assert!(
        output.status.success(),
        "sley {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn pkt_line(payload: &str) -> Vec<u8> {
    let mut out = format!("{:04x}", payload.len() + 4).into_bytes();
    out.extend(payload.as_bytes());
    out
}

#[test]
fn upload_pack_v0_advertises_shallow_and_peeled_tags() {
    let root = unique_temp_dir("upload-pack-v0-adv");
    let repo = root.join("repo");
    fs::create_dir_all(&repo).expect("create repo");
    run(
        &root,
        &[
            "init",
            "-q",
            "-b",
            "main",
            repo.to_str().expect("repo path is utf8"),
        ],
    );
    fs::write(repo.join("a"), b"a\n").expect("write a");
    run(
        &repo,
        &[
            "-c",
            "user.name=Wire User",
            "-c",
            "user.email=wire@example.invalid",
            "add",
            "a",
        ],
    );
    run(
        &repo,
        &[
            "-c",
            "user.name=Wire User",
            "-c",
            "user.email=wire@example.invalid",
            "commit",
            "-qm",
            "a",
        ],
    );
    run(
        &repo,
        &[
            "-c",
            "user.name=Wire User",
            "-c",
            "user.email=wire@example.invalid",
            "tag",
            "-a",
            "v1",
            "-m",
            "v1",
        ],
    );
    run(&repo, &["config", "uploadpack.allowFilter", "true"]);

    let advertised = run(
        &root,
        &[
            "upload-pack",
            "--advertise-refs",
            repo.to_str().expect("utf8"),
        ],
    );
    let text = String::from_utf8_lossy(&advertised);
    for token in [
        "shallow",
        "side-band-64k",
        "ofs-delta",
        "include-tag",
        "filter",
        "symref",
        "agent=",
        "object-format=",
    ] {
        assert!(
            text.contains(token),
            "advertisement missing {token}: {text}"
        );
    }
    assert!(
        text.contains("refs/tags/v1^{}"),
        "advertisement missing peeled tag: {text}"
    );

    let head = String::from_utf8(run(&repo, &["rev-parse", "HEAD"])).expect("utf8 head");
    let head = head.trim();
    let mut request = pkt_line(&format!("want {head}\n"));
    request.extend_from_slice(b"0000");
    request.extend(pkt_line("done\n"));
    request.extend_from_slice(b"0000");
    let mut child = Command::new(sley_testkit::sley_bin!())
        .current_dir(&root)
        .args([
            "upload-pack",
            "--stateless-rpc",
            repo.to_str().expect("utf8"),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn stateless upload-pack");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(&request)
        .expect("write request");
    let output = child
        .wait_with_output()
        .expect("wait stateless upload-pack");
    assert!(
        output.status.success(),
        "stateless upload-pack failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response = String::from_utf8_lossy(&output.stdout);
    assert!(
        !response.contains(" refs/heads/"),
        "stateless upload-pack repeated the ref advertisement: {response}"
    );
    assert!(
        output.stdout.windows(4).any(|window| window == b"PACK"),
        "stateless upload-pack did not return a pack"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn ls_remote_upload_pack_accepts_file_url() {
    let root = unique_temp_dir("ls-remote-upload-pack");
    let repo = root.join("repo");
    fs::create_dir_all(&repo).expect("create repo");
    run(
        &root,
        &[
            "init",
            "-q",
            "-b",
            "main",
            repo.to_str().expect("repo path is utf8"),
        ],
    );
    run(
        &repo,
        &[
            "-c",
            "user.name=Wire User",
            "-c",
            "user.email=wire@example.invalid",
            "commit",
            "--allow-empty",
            "-qm",
            "empty",
        ],
    );
    let log = root.join("invoked");
    let wrapper = root.join("upload-pack-wrapper");
    let sley = sley_testkit::sley_bin!();
    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\necho invoked >> {}\nexec {} upload-pack \"$@\"\n",
            log.display(),
            sley
        ),
    )
    .expect("write wrapper");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut mode = fs::metadata(&wrapper).expect("stat wrapper").permissions();
        mode.set_mode(0o755);
        fs::set_permissions(&wrapper, mode).expect("chmod wrapper");
    }
    let url = format!("file://{}", repo.display());
    let listed = run(
        &root,
        &[
            "ls-remote",
            &format!("--upload-pack={}", wrapper.display()),
            &url,
        ],
    );
    let text = String::from_utf8_lossy(&listed);
    assert!(
        text.contains("refs/heads/main"),
        "ls-remote --upload-pack missed main: {text}"
    );
    let invoked = fs::read_to_string(&log).unwrap_or_default();
    assert!(
        invoked.contains("invoked"),
        "ls-remote --upload-pack did not run the command"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn clone_upload_pack_file_url_invokes_command() {
    let root = unique_temp_dir("clone-upload-pack");
    let repo = root.join("repo");
    let dest = root.join("dest");
    fs::create_dir_all(&repo).expect("create repo");
    run(
        &root,
        &[
            "init",
            "-q",
            "-b",
            "main",
            repo.to_str().expect("repo path is utf8"),
        ],
    );
    run(
        &repo,
        &[
            "-c",
            "user.name=Wire User",
            "-c",
            "user.email=wire@example.invalid",
            "commit",
            "--allow-empty",
            "-qm",
            "empty",
        ],
    );
    let log = root.join("invoked");
    let wrapper = root.join("upload-pack-wrapper");
    let sley = sley_testkit::sley_bin!();
    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\necho invoked >> {}\nexec {} upload-pack \"$@\"\n",
            log.display(),
            sley
        ),
    )
    .expect("write wrapper");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut mode = fs::metadata(&wrapper).expect("stat wrapper").permissions();
        mode.set_mode(0o755);
        fs::set_permissions(&wrapper, mode).expect("chmod wrapper");
    }
    let url = format!("file://{}", repo.display());
    run(
        &root,
        &[
            "clone",
            "-q",
            "-u",
            wrapper.to_str().expect("utf8 wrapper"),
            &url,
            dest.to_str().expect("utf8 dest"),
        ],
    );
    let invoked = fs::read_to_string(&log).unwrap_or_default();
    assert!(
        invoked.contains("invoked"),
        "clone -u did not run the upload-pack command"
    );
    let source = String::from_utf8(run(&repo, &["rev-parse", "HEAD"])).expect("utf8");
    let cloned = String::from_utf8(run(&dest, &["rev-parse", "HEAD"])).expect("utf8");
    assert_eq!(source.trim(), cloned.trim());
    let _ = fs::remove_dir_all(root);
}
