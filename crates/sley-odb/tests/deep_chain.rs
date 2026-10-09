//! Delta chains far deeper than any stack-bound reader could follow, read on
//! a 256 KiB thread stack through the pack API and the object store.
//!
//! Each case runs in a child process so a stack overflow, which aborts the
//! whole process, is reported as an ordinary test failure.

#[path = "support/deep_chain.rs"]
mod deep_chain;

use deep_chain::{
    ChainPack, assert_deep_object, cross_pack_ref_chain, cross_pack_ref_cycle, deepest_oid,
    mixed_cross_pack_chain, ofs_chain,
};
use sley_core::ObjectFormat;
use sley_odb::{FileObjectDatabase, ObjectReader};
use std::path::PathBuf;
use std::{fs, process::Command, thread};

const SMALL_STACK: usize = 256 * 1024;
const CHILD_ENV: &str = "SLEY_DEEP_CHAIN_CHILD";

/// In the parent, re-run this test alone in a child process and require it
/// to exit cleanly. In the child, run `read` on a thread with a 256 KiB stack.
fn run_on_small_stack(name: &str, read: impl FnOnce() + Send + 'static) {
    if std::env::var(CHILD_ENV).as_deref() == Ok(name) {
        thread::Builder::new()
            .stack_size(SMALL_STACK)
            .spawn(read)
            .expect("spawn small-stack reader")
            .join()
            .expect("small-stack reader succeeds");
        return;
    }
    let output = Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, name)
        .output()
        .expect("run isolated reader");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "small-stack child failed: {}\n{stdout}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A throwaway object store holding `packs`, removed on drop.
struct Store {
    root: PathBuf,
    db: FileObjectDatabase,
}

impl Store {
    fn new(name: &str, packs: &[ChainPack]) -> Self {
        let root = std::env::temp_dir().join(format!("sley-{name}-{}", std::process::id()));
        let pack_dir = root.join("objects/pack");
        fs::create_dir_all(&pack_dir).expect("create object directory");
        for (number, pack) in packs.iter().enumerate() {
            fs::write(pack_dir.join(format!("pack-{number}.pack")), &pack.bytes)
                .expect("write pack");
            fs::write(pack_dir.join(format!("pack-{number}.idx")), pack.index())
                .expect("write index");
        }
        let db = FileObjectDatabase::from_git_dir(&root, ObjectFormat::Sha1);
        Self { root, db }
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn pack_api_ofs_chain_10000_small_stack() {
    run_on_small_stack("pack_api_ofs_chain_10000_small_stack", || {
        let pack = ofs_chain();
        let offset = pack.entries.last().expect("deep entry").offset;
        let object =
            sley_pack::read_object_at_arc(&pack.bytes, offset, ObjectFormat::Sha1, |_| Ok(None))
                .expect("read deep OFS chain through pack API");
        assert_deep_object(&object);
    });
}

#[test]
fn odb_ofs_chain_10000_small_stack() {
    run_on_small_stack("odb_ofs_chain_10000_small_stack", || {
        let store = Store::new("deep-ofs", &[ofs_chain()]);
        let object = store
            .db
            .read_object(&deepest_oid())
            .expect("read deep OFS chain through ODB");
        assert_deep_object(&object);
    });
}

#[test]
fn pack_api_cross_pack_ref_chain_10000_small_stack() {
    run_on_small_stack("pack_api_cross_pack_ref_chain_10000_small_stack", || {
        let packs = cross_pack_ref_chain();
        let offset = packs[0].entries.last().expect("deep entry").offset;
        let store = Store::new("deep-ref-pack", &packs);
        let object =
            sley_pack::read_object_at_arc(&packs[0].bytes, offset, ObjectFormat::Sha1, |oid| {
                store.db.read_object(oid).map(Some)
            })
            .expect("read cross-pack REF chain through pack API");
        assert_deep_object(&object);
    });
}

#[test]
fn odb_cross_pack_ref_chain_10000_small_stack() {
    run_on_small_stack("odb_cross_pack_ref_chain_10000_small_stack", || {
        let store = Store::new("deep-ref-odb", &cross_pack_ref_chain());
        let object = store
            .db
            .read_object(&deepest_oid())
            .expect("read cross-pack REF chain through ODB");
        assert_deep_object(&object);
    });
}

#[test]
fn odb_mixed_cross_pack_chain_10000_small_stack() {
    run_on_small_stack("odb_mixed_cross_pack_chain_10000_small_stack", || {
        let store = Store::new("deep-mixed-odb", &mixed_cross_pack_chain());
        let object = store
            .db
            .read_object(&deepest_oid())
            .expect("read mixed OFS/REF chain through ODB");
        assert_deep_object(&object);
    });
}

#[test]
fn odb_cross_pack_ref_cycle_is_an_error_small_stack() {
    run_on_small_stack("odb_cross_pack_ref_cycle_is_an_error_small_stack", || {
        let (packs, oid) = cross_pack_ref_cycle();
        let store = Store::new("ref-cycle-odb", &packs);
        let error = store
            .db
            .read_object(&oid)
            .expect_err("a REF cycle cannot resolve");
        assert!(
            error.to_string().contains("cycle"),
            "unexpected error: {error}"
        );
    });
}
