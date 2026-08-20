//! Create benchmark fixtures and print shell `export` statements for `scripts/bench-vs-git.sh`.
//!
//! ```text
//! cargo run -p sley-bench --example setup_fixtures 2>/dev/null
//! eval "$(cargo run -p sley-bench --example setup_fixtures 2>/dev/null)"
//! ```

use sley_bench::{BenchFixture, FIXTURE_OBJECT_COUNT, create_commit_fixture, create_fixture};
use std::io::Write;
use std::path::PathBuf;

fn write_batch_file(fixture: &BenchFixture) -> std::io::Result<PathBuf> {
    let batch_file = fixture.repo_root.join("batch-oids.txt");
    let mut file = std::fs::File::create(&batch_file)?;
    for oid in fixture.object_ids.iter().take(FIXTURE_OBJECT_COUNT) {
        writeln!(file, "{}", oid.to_hex())?;
    }
    Ok(batch_file)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn export(name: &str, value: &str) {
    println!("export {name}={}", shell_quote(value));
}

fn main() {
    let pack = create_fixture().expect("pack fixture");
    let commit = create_commit_fixture().expect("commit fixture");
    let batch_file = write_batch_file(&pack).expect("batch oid file");

    export(
        "SLEY_BENCH_PACK_REPO",
        &pack.repo_root.display().to_string(),
    );
    export("SLEY_BENCH_PACK_SAMPLE_OID", &pack.sample_oid.to_hex());
    export(
        "SLEY_BENCH_PACK_BATCH_FILE",
        &batch_file.display().to_string(),
    );
    export(
        "SLEY_BENCH_COMMIT_REPO",
        &commit.repo_root.display().to_string(),
    );
    export("SLEY_BENCH_COMMIT_COUNT", &commit.commit_count.to_string());
}
