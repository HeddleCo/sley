use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

fn unique_temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("sley-{name}-{}-{nanos}", std::process::id()))
}

/// Runs `<program> sh-i18n--envsubst <args>` with `stdin_text` on stdin and the
/// substitution variables in the environment, in a hermetic git config setup.
fn run_envsubst(
    program: &str,
    cwd: &Path,
    args: &[&str],
    stdin_text: &str,
    env: &[(&str, &str)],
) -> Output {
    let mut child = Command::new(program)
        .current_dir(cwd)
        .arg("sh-i18n--envsubst")
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .envs(env.to_vec())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| panic!("failed to run {program} sh-i18n--envsubst {args:?}: {err}"));
    child
        .stdin
        .as_mut()
        .expect("piped stdin")
        .write_all(stdin_text.as_bytes())
        .expect("write envsubst stdin");
    child
        .wait_with_output()
        .expect("wait for sh-i18n--envsubst")
}

fn assert_envsubst_matches_oracle(args: &[&str], stdin_text: &str, env: &[(&str, &str)]) {
    let root = unique_temp_dir("sh-i18n-envsubst");
    std::fs::create_dir_all(&root).expect("create temp dir");
    let expected = run_envsubst(sley_testkit::oracle_git(), &root, args, stdin_text, env);
    let actual = run_envsubst(sley_testkit::sley_bin!(), &root, args, stdin_text, env);
    assert_eq!(
        actual.status.code(),
        expected.status.code(),
        "sley exit differed for {args:?}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&actual.stdout),
        String::from_utf8_lossy(&actual.stderr)
    );
    assert_eq!(
        actual.stderr, expected.stderr,
        "sley stderr differed for {args:?}"
    );
    if args.first().is_some_and(|first| *first == "--variables") || args.len() != 1 {
        // Variable listings and argument errors are line-oriented text.
        assert_eq!(
            String::from_utf8_lossy(&actual.stdout),
            String::from_utf8_lossy(&expected.stdout),
            "sley stdout differed for {args:?}"
        );
    } else {
        // Substitution output is byte data (UTF-8 passthrough) — compare bytes.
        assert_eq!(
            actual.stdout, expected.stdout,
            "sley stdout differed for {args:?} with stdin {stdin_text:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// t0202-style: multibyte UTF-8 in the substituted text must survive byte-exactly
/// (regression: non-`$` input bytes were re-encoded as Latin-1 chars).
#[test]
fn sh_i18n_envsubst_multibyte_passthrough_matches_upstream_git() {
    let translated = "ja:日本語 de:éö Börde $WHO 😀 end\n";
    let format = "$WHO";
    assert_envsubst_matches_oracle(&[format], translated, &[("WHO", "wörld"), ("HOME", "/tmp")]);
    // Same template with a variable-free format filter passes everything through.
    assert_envsubst_matches_oracle(
        &["ja:$IGNORED"],
        translated,
        &[("WHO", "x"), ("IGNORED", "y")],
    );
    // Braced form substitutes identically to upstream.
    assert_envsubst_matches_oracle(&[], "日本 ${WHO} é", &[]);
}

#[test]
fn sh_i18n_envsubst_variables_listing_matches_upstream_git() {
    assert_envsubst_matches_oracle(&["--variables"], "a $one ${two_2} 日本 $one $9 $_ok", &[]);
}

/// Upstream's argc arms all end in success; malformed invocations only print an
/// `error:` line (and the two-arg arm still lists variables after complaining).
#[test]
fn sh_i18n_envsubst_argument_errors_match_upstream_git() {
    assert_envsubst_matches_oracle(&[], "ignored $WHO", &[("WHO", "x")]);
    assert_envsubst_matches_oracle(&["oops", "$A $B"], "ignored", &[]);
    assert_envsubst_matches_oracle(&["--variables", "$A", "$B"], "", &[]);
}
