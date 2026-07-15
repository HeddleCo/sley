//! Contracts for the native binaries used by upstream parity CI.

const WORKFLOW: &str = include_str!("../../../.github/workflows/upstream-parity.yml");

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
