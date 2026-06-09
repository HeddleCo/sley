use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir.join("../..");
    let target_dir = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| workspace_root.join("target"));
    let profile = std::env::var("PROFILE").expect("PROFILE");
    let bin = target_dir.join(&profile).join("sley");

    println!("cargo:rerun-if-changed=../sley-cli/src/main.rs");
    println!("cargo:rerun-if-env-changed=PROFILE");
    println!("cargo:rerun-if-env-changed=CARGO_TARGET_DIR");

    if !bin.exists() {
        eprintln!("sley-bench: building sley-cli for benchmarks");
        let cargo = std::env::var("CARGO").expect("CARGO");
        let status = Command::new(&cargo)
            .args(["build", "-p", "sley-cli", "--bin", "sley"])
            .current_dir(&workspace_root)
            .status()
            .expect("failed to spawn cargo build for sley-cli");
        if !status.success() {
            panic!("failed to build sley-cli binary for benchmarks");
        }
    }

    if !bin.exists() {
        panic!(
            "sley binary not found at {} after building sley-cli",
            bin.display()
        );
    }

    println!("cargo:rustc-env=SLEY_BENCH_BIN={}", bin.display());
}