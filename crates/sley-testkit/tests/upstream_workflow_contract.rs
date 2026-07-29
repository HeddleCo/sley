//! Contracts for the native binaries used by upstream parity CI.

const WORKFLOW: &str = include_str!("../../../.github/workflows/upstream-parity.yml");
const MATRIX_WORKFLOW: &str = include_str!("../../../.github/workflows/upstream-parity-matrix.yml");

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
fn matrix_propagates_runner_and_correctness_failures() {
    assert!(
        !MATRIX_WORKFLOW.contains("|| true"),
        "matrix failures must not be discarded"
    );
    assert_eq!(
        MATRIX_WORKFLOW
            .matches("- name: Enforce matrix correctness")
            .count(),
        2
    );
    assert!(
        !MATRIX_WORKFLOW.contains("continue-on-error"),
        "the matrix correctness gate must be able to fail each cell"
    );
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
    assert!(MATRIX_WORKFLOW.contains("brew install pcre2 gettext"));
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
