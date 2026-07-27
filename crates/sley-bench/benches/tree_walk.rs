use criterion::{Criterion, criterion_group, criterion_main};
use sley_bench::{CommitBenchFixture, create_commit_fixture};
use sley_core::{GitError, Result};
use std::hint::black_box;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

fn run_sley(cwd: &Path, args: &[&str], stdin: &[u8]) -> Result<Vec<u8>> {
    let mut child = Command::new(env!("SLEY_BENCH_BIN"))
        .current_dir(cwd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| GitError::Command(err.to_string()))?;
    if !stdin.is_empty() {
        let stdin_handle = child
            .stdin
            .as_mut()
            .ok_or_else(|| GitError::Command("missing sley stdin".into()))?;
        stdin_handle
            .write_all(stdin)
            .map_err(|err| GitError::Io(err.to_string()))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|err| GitError::Command(err.to_string()))?;
    if !output.status.success() {
        return Err(GitError::Command(format!(
            "sley {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(output.stdout)
}

fn fixture() -> &'static CommitBenchFixture {
    static FIXTURE: OnceLock<CommitBenchFixture> = OnceLock::new();
    FIXTURE.get_or_init(|| match create_commit_fixture() {
        Ok(fixture) => fixture,
        Err(err) => panic!("benchmark fixture setup failed: {err}"),
    })
}

fn ls_tree_recursive_head(c: &mut Criterion) {
    let fixture = fixture();
    let mut group = c.benchmark_group("ls_tree_recursive_head");
    group.bench_function("sley_cli", |b| {
        b.iter(|| {
            let output = run_sley(&fixture.repo_root, &["ls-tree", "-r", "HEAD"], &[]);
            match output {
                Ok(body) => black_box(body),
                Err(err) => panic!("sley ls-tree -r HEAD failed: {err}"),
            }
        });
    });
    group.finish();
}

criterion_group!(benches, ls_tree_recursive_head);
criterion_main!(benches);
