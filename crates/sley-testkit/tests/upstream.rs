//! Integration test that runs UPSTREAM git's own `t/*.sh` suite against the
//! sley binary (the ultimate parity oracle).
//!
//! GATING: this test is `#[ignore]` by default so a plain `cargo test
//! --workspace` never runs it, and it *also* skips cleanly (prints a message
//! and returns `Ok`) when no upstream `t/` directory is configured. So even if
//! someone runs it explicitly in an unconfigured environment, it passes.
//!
//! To actually exercise upstream parity:
//!
//! ```sh
//! # 1. Get a built git source checkout (see scripts/run-upstream-tests.sh):
//! git clone --depth=1 https://github.com/git/git /tmp/git-src
//! cd /tmp/git-src && make GIT-BUILD-OPTIONS t/helper/test-tool && \
//!     ( cd templates && make )
//!
//! # 2. Run this test against it:
//! GIT_SRC_DIR=/tmp/git-src \
//!     cargo test -p sley-testkit --test upstream -- --ignored --nocapture
//!
//! Set `SLEY_UPSTREAM_REQUIRE_PASS=1` to turn the measured parity run into a
//! gating assertion that every selected upstream script passes.
//! ```

use sley_testkit::upstream::{self, UpstreamRunOutcome};

#[test]
#[ignore = "runs upstream git's t/*.sh suite; requires GIT_SRC_DIR or SLEY_UPSTREAM_T pointing at a built git checkout"]
fn upstream_default_subset_runs() {
    // `CARGO_BIN_EXE_sley` is injected by Cargo when this crate has the
    // git-cli binary in scope; fall back to letting the runner resolve it.
    let bin = option_env!("CARGO_BIN_EXE_sley").map(std::path::Path::new);

    let outcome = match upstream::run_upstream_default_subset(bin) {
        Ok(outcome) => outcome,
        Err(err) => panic!("upstream runner failed to launch: {err}"),
    };

    match outcome {
        UpstreamRunOutcome::Skipped(reason) => {
            // Clean skip: nothing configured. Still a pass.
            eprintln!("SKIP upstream parity: {reason}");
        }
        UpstreamRunOutcome::Ran {
            results,
            report_path,
            all_passed,
        } => {
            eprintln!(
                "Upstream parity results (full report: {}):",
                report_path.display()
            );
            for r in &results {
                eprintln!(
                    "  {:<28} {:<8} ok={:<5} not-ok={}",
                    r.script, r.result, r.ok, r.failed
                );
            }
            assert!(
                !results.is_empty(),
                "runner produced no parseable per-script results"
            );

            if all_passed {
                eprintln!("All selected upstream scripts passed.");
            } else if std::env::var_os("SLEY_UPSTREAM_REQUIRE_PASS").is_some() {
                panic!("some selected upstream scripts failed; see the report above");
            } else {
                eprintln!(
                    "Some upstream scripts did not fully pass (expected during \
                     development); see the report above for details."
                );
            }
        }
    }
}

#[cfg(unix)]
mod runner_artifacts {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    static FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        root: PathBuf,
        upstream_t: PathBuf,
        sley: PathBuf,
        oracle: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "sley-upstream-runner-test-{}-{}",
                std::process::id(),
                FIXTURE_ID.fetch_add(1, Ordering::Relaxed)
            ));
            let upstream_t = root.join("t");
            let exec_path = root.join("libexec/git-core");
            let sley_exec_path = root.join("sley-libexec");
            let oracle_bin = root.join("bin");
            fs::create_dir_all(&upstream_t).expect("create fake upstream t dir");
            fs::create_dir_all(&exec_path).expect("create fake oracle exec path");
            fs::create_dir_all(&sley_exec_path).expect("create fake Sley exec path");
            fs::create_dir_all(&oracle_bin).expect("create fake oracle bin dir");
            fs::write(root.join("GIT-BUILD-OPTIONS"), "USE_LIBPCRE2='1'\n")
                .expect("write build options");
            fs::write(root.join("GIT-VERSION-FILE"), "GIT_VERSION=2.55.0\n")
                .expect("write version file");
            fs::write(upstream_t.join("test-lib.sh"), "").expect("write test-lib");

            let oracle = oracle_bin.join("git");
            write_executable(
                &oracle,
                &format!(
                    "#!/bin/sh\ncase \"${{1:-}}\" in\nversion) printf '%s\\n' 'git version 2.55.0' ;;\n--exec-path) printf '%s\\n' '{}' ;;\nesac\nexit 0\n",
                    exec_path.display()
                ),
            );
            for helper in [
                "git-upload-pack",
                "git-receive-pack",
                "git-http-backend",
                "git-remote-http",
                "git-submodule",
            ] {
                write_executable(&exec_path.join(helper), "#!/bin/sh\nexit 0\n");
            }
            for helper in ["git-sh-i18n", "git-sh-setup"] {
                fs::write(exec_path.join(helper), "# fake installed shell library\n")
                    .expect("write fake shell library");
            }
            write_executable(&oracle_bin.join("scalar"), "#!/bin/sh\nexit 0\n");

            fs::write(
                sley_exec_path.join(".sley-helper-provenance"),
                "schema=1\nowner=sley\ncrate=sley-i18n\nversion=fixture\n",
            )
            .expect("write fake Sley helper provenance");
            fs::write(sley_exec_path.join("git-sh-i18n"), "# fake shell library\n")
                .expect("write fake Sley shell library");
            for helper in [
                "git-sh-i18n--envsubst",
                "git-upload-pack",
                "git-receive-pack",
                "git-http-backend",
            ] {
                write_executable(&sley_exec_path.join(helper), "#!/bin/sh\nexit 0\n");
            }

            let sley = root.join("sley");
            write_executable(
                &sley,
                &format!(
                    "#!/bin/sh\nif [ -n \"${{SLEY_LAUNCH_MARKER:-}}\" ]; then printf '%s\\n' \"$0\" >\"$SLEY_LAUNCH_MARKER\"; fi\nif [ \"${{1:-}}\" = --exec-path ]; then printf '%s\\n' '{}'; fi\nexit 0\n",
                    sley_exec_path.display()
                ),
            );

            write_executable(
                &root.join("httpd"),
                "#!/bin/sh\nprintf 'sley=%s\\nargs=%s\\n' \"${SLEY_BIN-}\" \"$*\" >\"$SLEY_HTTPD_PROBE\"\nexit 0\n",
            );

            write_executable(
                &upstream_t.join("t0001-init.sh"),
                r#"#!/bin/sh
if test -n "${SLEY_HTTPD_PROBE-}"
then
	"$LIB_HTTPD_PATH" -v || exit 98
fi
test "${GIT_TEST_EXT_CHAIN_LINT:-}" = 0 || {
    printf '%s\n' 'runner did not disable redundant external chainlint' >&2
    exit 99
}
if [ "${SLEY_TEST_TARGET:-sley}" = oracle ]; then
    printf '%s\n' \
        'ok 1 - ordinary pass' \
        'not ok 2 - upstream breakage # TODO known breakage' \
        'ok 3 - optional prerequisite # SKIP unavailable here' \
        'ok 4 - applicable cell' \
        'ok 5 - another applicable cell' \
        '1..5'
    exit 0
fi
printf '%s\n' \
    'ok 1 - ordinary pass' \
    'not ok 2 - upstream breakage # TODO known breakage' \
    'ok 3 - optional prerequisite # SKIP unavailable here' \
    'ok 4 - applicable cell # SKIP sley skipped it' \
    'not ok 5 - another applicable cell' \
    '1..5'
exit 1
"#,
            );
            write_executable(
                &upstream_t.join("t0002-gitfile.sh"),
                "#!/bin/sh\nprintf '%s\\n' 'ok 1 - began' '1..3'\nexit 1\n",
            );
            write_executable(
                &upstream_t.join("t0003-attributes.sh"),
                "#!/bin/sh\nprintf '%s\\n' 'ok 1 - began'\nexit 124\n",
            );

            Self {
                root,
                upstream_t,
                sley,
                oracle,
            }
        }

        fn path(&self, name: &str) -> PathBuf {
            self.root.join(name)
        }

        fn command(&self, target: &str, stem: &str) -> Command {
            let mut command = Command::new("sh");
            command
                .arg(runner())
                .env("SLEY_UPSTREAM_T", &self.upstream_t)
                .env("SLEY_TEST_TARGET", target)
                .env("SLEY_BIN", &self.sley)
                .env("SLEY_ORACLE_BIN", &self.oracle)
                .env("SLEY_REPORT", self.path(&format!("{stem}-report.txt")))
                .env("SLEY_SUMMARY", self.path(&format!("{stem}-summary.csv")))
                .env("SLEY_HISTORY", self.path(&format!("{stem}-history.csv")))
                .env("SLEY_TIMINGS", self.path(&format!("{stem}-timings.csv")))
                .env("SLEY_CELLS", self.path(&format!("{stem}-cells.csv")))
                .env("SLEY_DETAILS", self.path(&format!("{stem}-details.csv")));
            command
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn runner() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("scripts")
            .join("run-upstream-tests.sh")
    }

    fn waves_runner() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("scripts")
            .join("run-upstream-tests-waves.sh")
    }

    fn write_executable(path: &Path, contents: &str) {
        fs::write(path, contents).expect("write executable fixture");
        let mut permissions = fs::metadata(path).expect("stat fixture").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("chmod fixture");
    }

    fn write_fixture_manifest(fixture: &Fixture, expected_included: usize) -> PathBuf {
        let manifest = fixture.path("upstream-manifest.tsv");
        fs::write(
            &manifest,
            format!(
                "# sley-upstream-manifest-v1\n\
# expected_included={expected_included}\n\
include\tt[0-9][0-9][0-9][0-9]-*.sh\toracle\toracle\teligible\tupstream-declared\tfixture\n"
            ),
        )
        .expect("write fixture manifest");
        manifest
    }

    fn validate_fixture_manifest(fixture: &Fixture, manifest: &Path) -> std::process::Output {
        Command::new("sh")
            .arg(runner())
            .arg("--validate-manifest")
            .env_remove("SLEY_UPSTREAM_T")
            .env("GIT_SRC_DIR", &fixture.root)
            .env("SLEY_UPSTREAM_MANIFEST", manifest)
            .output()
            .expect("validate fixture manifest")
    }

    #[test]
    fn manifest_validation_rejects_only_untracked_test_scripts_in_git_checkouts() {
        let fixture = Fixture::new();
        let manifest = write_fixture_manifest(&fixture, 3);

        let tarball = validate_fixture_manifest(&fixture, &manifest);
        assert!(
            tarball.status.success(),
            "non-Git source was rejected: {}",
            String::from_utf8_lossy(&tarball.stderr)
        );

        let init = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&fixture.root)
            .output()
            .expect("initialize fixture checkout");
        assert!(
            init.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&init.stderr)
        );
        let add = Command::new("git")
            .args([
                "add",
                "t/test-lib.sh",
                "t/t0001-init.sh",
                "t/t0002-gitfile.sh",
                "t/t0003-attributes.sh",
            ])
            .current_dir(&fixture.root)
            .output()
            .expect("track fixture test inventory");
        assert!(
            add.status.success(),
            "git add failed: {}",
            String::from_utf8_lossy(&add.stderr)
        );
        fs::write(fixture.path("unrelated.txt"), "untracked but harmless\n")
            .expect("write unrelated untracked file");

        let clean_inventory = validate_fixture_manifest(&fixture, &manifest);
        assert!(
            clean_inventory.status.success(),
            "tracked scripts or unrelated untracked files were rejected: {}",
            String::from_utf8_lossy(&clean_inventory.stderr)
        );

        let stale_script = fixture.upstream_t.join("t9999-stale-local.sh");
        write_executable(
            &stale_script,
            "#!/bin/sh\nprintf '%s\\n' '1..0 # SKIP stale fixture'\n",
        );
        let stale_inventory = validate_fixture_manifest(&fixture, &manifest);
        assert!(!stale_inventory.status.success());
        let stderr = String::from_utf8_lossy(&stale_inventory.stderr);
        assert!(
            stderr.contains("untracked upstream test script(s)"),
            "{stderr}"
        );
        assert!(stderr.contains("t/t9999-stale-local.sh"), "{stderr}");
        assert!(
            stderr.contains("remove or track these files"),
            "missing remediation: {stderr}"
        );

        let add_stale = Command::new("git")
            .args(["add", "t/t9999-stale-local.sh"])
            .current_dir(&fixture.root)
            .output()
            .expect("track formerly stale fixture script");
        assert!(
            add_stale.status.success(),
            "git add failed: {}",
            String::from_utf8_lossy(&add_stale.stderr)
        );
        write_fixture_manifest(&fixture, 4);
        let tracked_inventory = validate_fixture_manifest(&fixture, &manifest);
        assert!(
            tracked_inventory.status.success(),
            "tracked script was rejected: {}",
            String::from_utf8_lossy(&tracked_inventory.stderr)
        );
        let tracked_stderr = String::from_utf8_lossy(&tracked_inventory.stderr);
        assert!(
            !tracked_stderr.contains("untracked upstream test script(s)"),
            "tracked script was misclassified as untracked: {tracked_stderr}"
        );
    }

    #[test]
    fn classifies_tap_cells_and_compares_oracle_to_sley() {
        let fixture = Fixture::new();
        let oracle = fixture
            .command("oracle", "oracle")
            .arg("t0001-init.sh")
            .output()
            .expect("run oracle fixture");
        assert!(
            oracle.status.success(),
            "oracle fixture failed: {}",
            String::from_utf8_lossy(&oracle.stderr)
        );
        // A reusable certification oracle normally contains the full curated
        // suite. Targeted Sley runs must compare only their selected scripts,
        // not manufacture missing-cell rows for every other oracle script.
        let oracle_cells_path = fixture.path("oracle-cells.csv");
        let mut oracle_cells =
            fs::read_to_string(&oracle_cells_path).expect("read oracle cells fixture");
        oracle_cells.push_str(
            "\"oracle\",\"t0002-gitfile.sh\",\"1\",\"PASS\",\"ok\",\"\",\"unselected oracle cell\"\n",
        );
        fs::write(&oracle_cells_path, oracle_cells).expect("extend oracle cells fixture");
        let oracle_details_path = fixture.path("oracle-details.csv");
        let mut oracle_details =
            fs::read_to_string(&oracle_details_path).expect("read oracle details fixture");
        oracle_details.push_str("oracle,t0002-gitfile.sh,PASS,0,1,0,0,0,1,1,0,0,0,0\n");
        fs::write(&oracle_details_path, oracle_details).expect("extend oracle details fixture");

        let mut sley = fixture.command("sley", "sley");
        sley.arg("t0001-init.sh")
            .env("SLEY_ORACLE_CELLS", fixture.path("oracle-cells.csv"))
            .env("SLEY_ORACLE_DETAILS", fixture.path("oracle-details.csv"))
            .env("SLEY_COMPARISON", fixture.path("comparison.csv"));
        let output = sley.output().expect("run sley fixture");
        assert!(
            !output.status.success(),
            "fixture contains one real failure"
        );

        let cells = fs::read_to_string(fixture.path("sley-cells.csv")).expect("read cells");
        assert!(cells.contains("\"1\",\"PASS\",\"ok\",\"\",\"ordinary pass\""));
        assert!(cells.contains("\"2\",\"TODO\",\"not_ok\",\"TODO\""));
        assert!(cells.contains("\"3\",\"SKIP\",\"ok\",\"SKIP\""));
        assert!(cells.contains("\"5\",\"FAIL\",\"not_ok\",\"\""));

        let details = fs::read_to_string(fixture.path("sley-details.csv")).expect("read details");
        assert!(details.contains("sley,t0001-init.sh,FAIL,1,1,1,1,2,5,5,0,0,0,0"));

        let comparison =
            fs::read_to_string(fixture.path("comparison.csv")).expect("read comparison");
        assert!(!comparison.contains("t0002-gitfile.sh"));
        assert!(comparison.contains("t0001-init.sh,4,PASS,SKIP,UNEXPECTED_SLEY_SKIP"));
        assert!(comparison.contains("t0001-init.sh,5,PASS,FAIL,SLEY_FAILURE"));
        let comparison_summary = fs::read_to_string(fixture.path("comparison-summary.csv"))
            .expect("read comparison summary");
        assert!(!comparison_summary.contains("t0002-gitfile.sh"));
        assert!(
            comparison_summary.contains(
                "t0001-init.sh,PASS,FAIL,5,5,INCOMPARABLE,FAIL,1,0,0,eligible,INCOMPARABLE"
            )
        );

        let legacy =
            fs::read_to_string(fixture.path("sley-summary.csv")).expect("read legacy summary");
        assert!(legacy.contains("t0001-init.sh,init,FAIL,3,2,5,5"));
    }

    #[test]
    fn distinguishes_abort_and_timeout_from_assertion_failure() {
        let fixture = Fixture::new();
        let output = fixture
            .command("sley", "incomplete")
            .args(["t0002-gitfile.sh", "t0003-attributes.sh"])
            .output()
            .expect("run incomplete fixtures");
        assert!(!output.status.success());

        let details = fs::read_to_string(fixture.path("incomplete-details.csv"))
            .expect("read classifications");
        assert!(details.contains("sley,t0002-gitfile.sh,ABORT,1,1,0,0,0,1,3,1,0,2,0"));
        assert!(details.contains("sley,t0003-attributes.sh,TIMEOUT,124,1,0,0,0,1,,0,1,0,0"));
    }

    #[test]
    fn tap_parser_tolerates_non_utf8_diagnostics() {
        let fixture = Fixture::new();
        write_executable(
            &fixture.upstream_t.join("t0004-unicode.sh"),
            "#!/bin/sh\nprintf '\\377invalid diagnostic\\n'\nprintf '%s\\n' 'ok 1 - first cell' 'ok 2 - second cell' '1..2'\n",
        );
        let output = fixture
            .command("sley", "non-utf8")
            .arg("t0004-unicode.sh")
            .output()
            .expect("run non-utf8 fixture");
        assert!(
            output.status.success(),
            "runner rejected non-UTF-8 diagnostics: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let cells =
            fs::read_to_string(fixture.path("non-utf8-cells.csv")).expect("read non-utf8 cells");
        assert!(cells.contains("\"1\",\"PASS\",\"ok\",\"\",\"first cell\""));
        assert!(cells.contains("\"2\",\"PASS\",\"ok\",\"\",\"second cell\""));
        let details = fs::read_to_string(fixture.path("non-utf8-details.csv"))
            .expect("read non-utf8 details");
        assert!(details.contains("sley,t0004-unicode.sh,PASS,0,2,0,0,0,2,2,0,0,0,0"));
    }

    #[test]
    fn serial_runner_creates_fresh_artifact_directories() {
        let fixture = Fixture::new();
        let artifact_root = fixture.path("fresh/serial-artifacts");
        let output = fixture
            .command("oracle", "unused")
            .arg("t0001-init.sh")
            .env("SLEY_REPORT", artifact_root.join("report.txt"))
            .env("SLEY_SUMMARY", artifact_root.join("summary.csv"))
            .env("SLEY_HISTORY", artifact_root.join("history.csv"))
            .env("SLEY_TIMINGS", artifact_root.join("timings.csv"))
            .env("SLEY_CELLS", artifact_root.join("cells.csv"))
            .env("SLEY_DETAILS", artifact_root.join("details.csv"))
            .output()
            .expect("run serial fixture");
        assert!(
            output.status.success(),
            "serial fixture failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(artifact_root.join("report.txt").is_file());
        assert!(artifact_root.join("cells.csv").is_file());
        assert!(artifact_root.join("details.csv").is_file());
    }

    #[test]
    fn sley_runner_uses_a_direct_native_git_launcher() {
        let fixture = Fixture::new();
        let marker = fixture.path("launch-marker.txt");
        let output = fixture
            .command("sley", "direct-launch")
            .arg("t0001-init.sh")
            .env("SLEY_LAUNCH_MARKER", &marker)
            .output()
            .expect("run direct-launch fixture");
        assert!(
            !output.status.success(),
            "fixture contains one real failure"
        );
        let invoked = fs::read_to_string(marker).expect("read launcher marker");
        assert_eq!(
            Path::new(invoked.trim())
                .file_name()
                .and_then(|name| name.to_str()),
            Some("git"),
            "Sley must be invoked directly under the installed git name"
        );
    }

    #[test]
    fn sley_runner_passes_candidate_binary_into_http_cgi_environment() {
        let fixture = Fixture::new();
        let marker = fixture.path("httpd-probe.txt");
        let output = fixture
            .command("sley", "httpd-cgi-env")
            .arg("t0001-init.sh")
            .env("LIB_HTTPD_PATH", fixture.path("httpd"))
            .env("SLEY_HTTPD_PROBE", &marker)
            .output()
            .expect("run HTTP CGI environment fixture");
        assert!(
            !output.status.success(),
            "fixture still contains its intentional parity failure"
        );
        let probe = fs::read_to_string(marker).expect("read HTTP daemon probe");
        assert!(
            probe.contains(&format!("sley={}", fixture.sley.display())),
            "HTTP daemon wrapper must inherit the selected SLEY_BIN: {probe}"
        );
        assert!(
            probe.contains("args=-c PassEnv SLEY_BIN -v"),
            "HTTP daemon wrapper must pass SLEY_BIN into CGI: {probe}"
        );
    }

    #[test]
    fn wave_runner_merges_exact_cell_and_script_artifacts() {
        let fixture = Fixture::new();
        let artifact_root = fixture.path("fresh/wave-artifacts");
        let mut command = Command::new("sh");
        command
            .arg(waves_runner())
            .env("SLEY_UPSTREAM_T", &fixture.upstream_t)
            .env("SLEY_BIN", &fixture.sley)
            .env("SLEY_TEST_TARGET", "sley")
            .env("SLEY_TESTS", "t0001-init.sh t0002-gitfile.sh")
            .env("SLEY_UPSTREAM_WAVES", "2")
            .env("SLEY_REPORT", artifact_root.join("waves-report.txt"))
            .env("SLEY_SUMMARY", artifact_root.join("waves-summary.csv"))
            .env("SLEY_HISTORY", artifact_root.join("waves-history.csv"))
            .env("SLEY_TIMINGS", artifact_root.join("waves-timings.csv"))
            .env("SLEY_CELLS", artifact_root.join("waves-cells.csv"))
            .env("SLEY_DETAILS", artifact_root.join("waves-details.csv"));
        let output = command.output().expect("run wave fixture");
        assert!(!output.status.success());

        let cells = fs::read_to_string(artifact_root.join("waves-cells.csv")).expect("read cells");
        assert_eq!(cells.lines().count(), 7, "header plus six observed cells");
        let details =
            fs::read_to_string(artifact_root.join("waves-details.csv")).expect("read details");
        assert_eq!(details.lines().count(), 3, "header plus two scripts");
        assert!(details.contains("sley,t0001-init.sh,FAIL"));
        assert!(details.contains("sley,t0002-gitfile.sh,ABORT"));
    }
}
