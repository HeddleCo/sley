#[path = "../../sley-pack/tests/support/deep_chain.rs"]
mod fixture;

use fixture::{ChainPack, assert_deep_object, cross_pack_ref_chain, ofs_chain};
use sley_core::ObjectFormat;
use sley_odb::{FileObjectDatabase, ObjectReader};
use std::{fs, process::Command, thread};

fn run_on_small_stack(name: &str, run: impl FnOnce() + Send + 'static) {
    if std::env::var("SLEY_DEEP_CHAIN_CHILD").as_deref() == Ok(name) {
        thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(run)
            .expect("spawn small-stack reader")
            .join()
            .expect("small-stack reader succeeds");
        return;
    }
    let output = Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", name, "--nocapture"])
        .env("SLEY_DEEP_CHAIN_CHILD", name)
        .output()
        .expect("run isolated reader");
    assert!(
        output.status.success(),
        "small-stack subprocess failed: {}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn install(packs: &[ChainPack], name: &str) -> (std::path::PathBuf, FileObjectDatabase) {
    let root = std::env::temp_dir().join(format!("sley-{name}-{}", std::process::id()));
    let pack_dir = root.join("objects/pack");
    fs::create_dir_all(&pack_dir).expect("create object directory");
    for (number, pack) in packs.iter().enumerate() {
        fs::write(pack_dir.join(format!("pack-{number}.pack")), &pack.bytes).expect("write pack");
        fs::write(pack_dir.join(format!("pack-{number}.idx")), pack.index()).expect("write index");
    }
    let db = FileObjectDatabase::from_git_dir(&root, ObjectFormat::Sha1);
    (root, db)
}

#[test]
fn pack_api_ofs_chain_10000_small_stack() {
    let pack = ofs_chain();
    let offset = pack.entries.last().expect("deep entry").offset;
    run_on_small_stack("pack_api_ofs_chain_10000_small_stack", move || {
        let object =
            sley_pack::read_object_at_arc(&pack.bytes, offset, ObjectFormat::Sha1, |_| Ok(None))
                .expect("read deep OFS chain through pack API");
        assert_deep_object(&object);
    });
}

#[test]
fn odb_ofs_chain_10000_small_stack() {
    let pack = ofs_chain();
    let oid = pack.entries.last().expect("deep entry").oid;
    let (root, db) = install(&[pack], "deep-ofs");
    run_on_small_stack("odb_ofs_chain_10000_small_stack", move || {
        assert_deep_object(
            &db.read_object(&oid)
                .expect("read deep OFS chain through ODB"),
        );
    });
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn pack_api_cross_pack_ref_chain_10000_small_stack() {
    let packs = cross_pack_ref_chain();
    let offset = packs[0].entries.last().expect("deep entry").offset;
    let (root, db) = install(&packs, "deep-ref-pack");
    run_on_small_stack(
        "pack_api_cross_pack_ref_chain_10000_small_stack",
        move || {
            let object =
                sley_pack::read_object_at_arc(&packs[0].bytes, offset, ObjectFormat::Sha1, |oid| {
                    db.read_object(oid).map(Some)
                })
                .expect("read cross-pack REF chain through pack API");
            assert_deep_object(&object);
        },
    );
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn odb_cross_pack_ref_chain_10000_small_stack() {
    let packs = cross_pack_ref_chain();
    let oid = packs[0].entries.last().expect("deep entry").oid;
    let (root, db) = install(&packs, "deep-ref-odb");
    run_on_small_stack("odb_cross_pack_ref_chain_10000_small_stack", move || {
        assert_deep_object(
            &db.read_object(&oid)
                .expect("read cross-pack REF chain through ODB"),
        );
    });
    fs::remove_dir_all(root).expect("remove fixture");
}
