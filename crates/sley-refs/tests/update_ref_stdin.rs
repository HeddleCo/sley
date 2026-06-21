use std::borrow::Cow;
use std::io::Cursor;

use sley_core::RecordReader;
use sley_refs::update_ref_stdin::{
    UpdateRefStdinCommand, UpdateRefStdinOid, UpdateRefStdinOption, UpdateRefStdinSymrefOld,
    parse_update_ref_stdin_line, parse_update_ref_stdin_nul,
    update_ref_stdin_nul_additional_records,
};

#[test]
fn parses_newline_update_with_borrowed_arguments() {
    let parsed = parse_update_ref_stdin_line(
        b"update refs/heads/main aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    )
    .expect("test operation should succeed");

    let UpdateRefStdinCommand::Update { refname, new, old } = parsed else {
        panic!("expected update command");
    };
    assert_eq!(refname, "refs/heads/main");
    assert!(matches!(refname, Cow::Borrowed(_)));

    let UpdateRefStdinOid::Value(new) = new else {
        panic!("expected new oid");
    };
    assert_eq!(new, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    assert!(matches!(new, Cow::Borrowed(_)));

    let Some(UpdateRefStdinOid::Value(old)) = old else {
        panic!("expected old oid");
    };
    assert_eq!(old, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
    assert!(matches!(old, Cow::Borrowed(_)));
}

#[test]
fn parses_newline_quoted_arguments_as_owned_values() {
    let parsed = parse_update_ref_stdin_line(
        br#"create "refs/heads/with\040space" aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"#,
    )
    .expect("test operation should succeed");

    let UpdateRefStdinCommand::Create { refname, new } = parsed else {
        panic!("expected create command");
    };
    assert_eq!(refname, "refs/heads/with space");
    assert!(matches!(refname, Cow::Owned(_)));
    assert_eq!(
        new.as_str(),
        Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
    );
}

#[test]
fn parses_nul_update_from_record_reader() {
    let input = b"update refs/heads/main\0aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\0\0commit\0";
    let mut reader = RecordReader::new(Cursor::new(input.as_slice()), b'\0');

    let first = reader
        .read_record()
        .expect("test operation should succeed")
        .expect("first record should exist");
    let additional_count =
        update_ref_stdin_nul_additional_records(&first).expect("test operation should succeed");
    let mut additional = Vec::new();
    for _ in 0..additional_count {
        additional.push(
            reader
                .read_record()
                .expect("test operation should succeed")
                .expect("additional record should exist"),
        );
    }
    let additional_refs: Vec<&[u8]> = additional.iter().map(Vec::as_slice).collect();
    let parsed = parse_update_ref_stdin_nul(&first, &additional_refs)
        .expect("test operation should succeed");

    let UpdateRefStdinCommand::Update { refname, new, old } = parsed else {
        panic!("expected update command");
    };
    assert_eq!(refname, "refs/heads/main");
    assert_eq!(
        new.as_str(),
        Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
    );
    assert_eq!(old, None);

    let commit = reader
        .read_record()
        .expect("test operation should succeed")
        .expect("commit record should exist");
    assert_eq!(
        update_ref_stdin_nul_additional_records(&commit).expect("test operation should succeed"),
        0
    );
    assert_eq!(
        parse_update_ref_stdin_nul(&commit, &[]).expect("test operation should succeed"),
        UpdateRefStdinCommand::Commit
    );
}

#[test]
fn parses_symref_update_old_target() {
    let additional: [&[u8]; 3] = [
        b"refs/heads/main".as_slice(),
        b"ref".as_slice(),
        b"refs/heads/old".as_slice(),
    ];
    let parsed = parse_update_ref_stdin_nul(b"symref-update HEAD", &additional)
        .expect("test operation should succeed");

    let UpdateRefStdinCommand::SymrefUpdate {
        refname,
        target,
        old,
    } = parsed
    else {
        panic!("expected symref-update command");
    };
    assert_eq!(refname, "HEAD");
    assert_eq!(target, "refs/heads/main");
    assert_eq!(
        old,
        Some(UpdateRefStdinSymrefOld::Ref(Cow::Borrowed(
            "refs/heads/old"
        )))
    );
}

#[test]
fn parses_no_deref_option() {
    assert_eq!(
        parse_update_ref_stdin_line(b"option no-deref").expect("test operation should succeed"),
        UpdateRefStdinCommand::Option(UpdateRefStdinOption::NoDeref)
    );
}

#[test]
fn rejects_whitespace_before_command() {
    let err = parse_update_ref_stdin_line(b" update refs/heads/main")
        .expect_err("leading whitespace should be rejected");

    assert_eq!(
        err.message(),
        "whitespace before command:  update refs/heads/main"
    );
}
