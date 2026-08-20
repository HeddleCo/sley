//! Contracts for the native binaries used by upstream parity CI.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const WORKFLOW: &str = include_str!("../../../.github/workflows/upstream-parity.yml");
const MATRIX_WORKFLOW: &str = include_str!("../../../.github/workflows/upstream-parity-matrix.yml");
const PR_SCRIPTS: &str = include_str!("../../../.github/workflows/parity-pr-scripts.txt");

#[test]
fn builds_all_native_cli_binaries() {
    let build = WORKFLOW
        .lines()
        .find(|line| line.contains("cargo build -p sley-cli"))
        .expect("upstream parity must build sley-cli");

    assert!(build.contains("--bins"), "build every native CLI binary");
    assert!(
        !build.contains("--bin sley"),
        "a sley-only build omits the required scalar helper"
    );
    assert!(build.contains("--features git-compat-i18n"));
}

#[test]
fn verifies_both_binaries_before_launching_sley_waves() {
    let verification = WORKFLOW
        .find("- name: Verify native Sley binaries")
        .expect("workflow must verify native binaries");
    let launch = WORKFLOW
        .find("- name: Run Sley on curated manifest")
        .expect("workflow must launch Sley waves");
    assert!(verification < launch);

    let section = &WORKFLOW[verification..launch];
    assert!(section.contains("test -x target/release/sley"));
    assert!(section.contains("test -x target/release/scalar"));
}

#[test]
fn passes_verified_scalar_path_to_runner() {
    assert!(WORKFLOW.contains("SLEY_SCALAR_BIN: ${{ github.workspace }}/target/release/scalar"));
}

#[test]
fn pull_requests_run_a_required_fast_floor_surface() {
    assert!(WORKFLOW.contains("  pull_request:"));
    assert!(WORKFLOW.contains("- name: Select parity surface"));
    assert!(WORKFLOW.contains(".github/workflows/parity-pr-scripts.txt"));
    assert!(WORKFLOW.contains("\"$SLEY_PARITY_REQUIRED\""));
    assert!(
        PR_SCRIPTS
            .lines()
            .filter(|line| line.ends_with(".sh"))
            .count()
            >= 10
    );
}

#[test]
fn matrix_propagates_oracle_failures_and_gates_sley_on_floors() {
    assert_eq!(
        MATRIX_WORKFLOW
            .matches("run-upstream-tests-waves.sh || true")
            .count(),
        2
    );
    assert_eq!(
        MATRIX_WORKFLOW
            .matches("- name: Check parity floors")
            .count(),
        2
    );
    assert!(!MATRIX_WORKFLOW.contains("- name: Enforce matrix correctness"));
    assert_eq!(
        MATRIX_WORKFLOW
            .matches("- name: Enforce release correctness")
            .count(),
        2
    );
    assert!(
        !MATRIX_WORKFLOW.contains("continue-on-error"),
        "the floor gate must be able to fail each cell"
    );
}

#[test]
fn floor_gate_refuses_an_unmeasured_windows_profile() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before epoch")
        .as_nanos();
    let temp = std::env::temp_dir().join(format!(
        "sley-windows-floor-contract-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&temp).expect("create Windows floor test directory");
    let summary = temp.join("summary.csv");
    fs::write(
        &summary,
        "script,command,result,ok,notok,total,plan_total\n",
    )
    .expect("write Windows floor summary");

    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let checker = repo.join(".github/workflows/scripts/check-parity-floors.sh");
    let output = Command::new("bash")
        .current_dir(&repo)
        .env("SLEY_PARITY_PLATFORM", "windows")
        .arg(checker)
        .arg(summary)
        .output()
        .expect("run floor checker");

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("Windows parity floors have not been measured")
    );

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn partial_floor_gate_demonstrates_regression_and_zero_script_failures() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before epoch")
        .as_nanos();
    let temp = std::env::temp_dir().join(format!(
        "sley-parity-floor-contract-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&temp).expect("create floor test directory");
    let required = temp.join("required.txt");
    let summary = temp.join("summary.csv");
    fs::write(&required, "t0001-init.sh\n").expect("write required scripts");

    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let checker = repo.join(".github/workflows/scripts/check-parity-floors.sh");
    let run = || {
        Command::new("bash")
            .current_dir(&repo)
            .env("SLEY_PARITY_PLATFORM", "macos")
            .arg(&checker)
            .arg(&summary)
            .arg(&required)
            .output()
            .expect("run floor checker")
    };

    fs::write(
        &summary,
        "script,command,result,ok,notok,total,plan_total\n\
         t0001-init.sh,t0001-init.sh,PASS,102,0,102,102\n",
    )
    .expect("write passing summary");
    assert!(run().status.success(), "recorded floor must pass");

    fs::write(
        &summary,
        "script,command,result,ok,notok,total,plan_total\n\
         t0001-init.sh,t0001-init.sh,FAIL,101,1,102,102\n",
    )
    .expect("write regressed summary");
    let regressed = run();
    assert!(
        !regressed.status.success(),
        "a deliberate floor drop must fail"
    );
    assert!(String::from_utf8_lossy(&regressed.stderr).contains("dropped below floor"));

    fs::write(
        &summary,
        "script,command,result,ok,notok,total,plan_total\n",
    )
    .expect("write zero-script summary");
    let empty = run();
    assert!(!empty.status.success(), "a zero-script result must fail");
    assert!(String::from_utf8_lossy(&empty.stderr).contains("absent from summary"));

    let _ = fs::remove_dir_all(temp);
}

#[test]
fn matrix_builds_and_passes_the_native_scalar_binary() {
    assert_eq!(
        MATRIX_WORKFLOW
            .matches("cargo build --locked -p sley-cli --bins")
            .count(),
        2
    );
    assert!(MATRIX_WORKFLOW.contains("test -x target/release/scalar"));
    assert!(MATRIX_WORKFLOW.contains("test -x target/release/scalar.exe"));
    assert!(
        MATRIX_WORKFLOW.contains("SLEY_SCALAR_BIN=\"$GITHUB_WORKSPACE/target/release/scalar\"")
    );
    assert!(MATRIX_WORKFLOW.contains("SLEY_SCALAR_BIN=\"$(pwd)/target/release/scalar.exe\""));
}

#[test]
fn matrix_exposes_platform_build_dependencies() {
    assert!(MATRIX_WORKFLOW.contains("brew install pcre2 gettext bash"));
    assert!(MATRIX_WORKFLOW.contains("pcre2_prefix=\"$(brew --prefix pcre2)\""));
    assert!(MATRIX_WORKFLOW.contains("export CPPFLAGS=\"-I$pcre2_prefix/include"));
    assert!(
        MATRIX_WORKFLOW.contains("export PATH=\"$(cygpath -u \"$USERPROFILE\")/.cargo/bin:$PATH\"")
    );
    assert!(MATRIX_WORKFLOW.contains("command -v cargo"));
    assert!(MATRIX_WORKFLOW.contains("targets: x86_64-pc-windows-gnu"));
    assert!(MATRIX_WORKFLOW.contains("CARGO_BUILD_TARGET=x86_64-pc-windows-gnu"));
    assert!(MATRIX_WORKFLOW.contains("RUST_TARGET_DIR=target/x86_64-pc-windows-gnu/release"));
}
