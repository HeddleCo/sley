use std::path::PathBuf;

// A build script must NOT invoke `cargo` recursively: under `cargo test
// --workspace` (and any build that compiles this crate's bench targets), the
// outer cargo holds the build lock, so a nested `cargo build` blocks on that
// same lock forever — a hard deadlock that hangs the whole workspace test run
// and CI. The binary is not needed to *compile* the benches, only to *run*
// them, so we just compute and emit the expected path here. Benchmarks that
// exec it are responsible for ensuring the `sley` binary exists first, for
// example with `cargo build -p sley-cli --bin sley`.
fn main() {
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir.join("../..");
    let target_dir = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| workspace_root.join("target"));
    let profile = std::env::var("PROFILE").expect("PROFILE");
    let bin = target_dir.join(&profile).join("sley");

    println!("cargo:rerun-if-env-changed=PROFILE");
    println!("cargo:rerun-if-env-changed=CARGO_TARGET_DIR");

    println!("cargo:rustc-env=SLEY_BENCH_BIN={}", bin.display());
}
