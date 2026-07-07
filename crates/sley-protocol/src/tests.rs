use super::*;
use crate::receive_pack::zero_object_id;
use sley_core::{Capability, ObjectFormat, ObjectId};

#[test]
fn pkt_line_frame_encodes_data_and_control_frames() {
    assert_eq!(PktLine(b"hello\n".to_vec()).encode(), b"000ahello\n");
    assert_eq!(
        PktLineFrame::data(b"hello\n".to_vec())
            .expect("test operation should succeed")
            .encode(),
        b"000ahello\n"
    );
    assert_eq!(PktLineFrame::Flush.encode(), b"0000");
    assert_eq!(PktLineFrame::Delimiter.encode(), b"0001");
    assert_eq!(PktLineFrame::ResponseEnd.encode(), b"0002");
    assert_eq!(
        PktLineFrame::data(b"hello\n".to_vec())
            .expect("test operation should succeed")
            .try_encode()
            .expect("test operation should succeed"),
        b"000ahello\n"
    );
}

#[test]
fn pkt_line_frame_parses_data_and_control_frames() {
    assert_eq!(
        PktLineFrame::parse(b"000ahello\n").expect("test operation should succeed"),
        (PktLineFrame::Data(b"hello\n".to_vec()), 10)
    );
    assert_eq!(
        PktLineFrame::parse(b"0000").expect("test operation should succeed"),
        (PktLineFrame::Flush, 4)
    );
    assert_eq!(
        PktLineFrame::parse(b"0001").expect("test operation should succeed"),
        (PktLineFrame::Delimiter, 4)
    );
    assert_eq!(
        PktLineFrame::parse(b"0002").expect("test operation should succeed"),
        (PktLineFrame::ResponseEnd, 4)
    );
}

#[test]
fn pkt_line_stream_parses_multiple_frames() {
    let frames = parse_pkt_line_stream(b"000eversion 2\n00010009done\n0000")
        .expect("test operation should succeed");
    assert_eq!(
        frames,
        vec![
            PktLineFrame::Data(b"version 2\n".to_vec()),
            PktLineFrame::Delimiter,
            PktLineFrame::Data(b"done\n".to_vec()),
            PktLineFrame::Flush,
        ]
    );
}

#[test]
fn pkt_line_stream_reads_and_writes_incremental_io() {
    let frames = vec![
        PktLineFrame::Data(b"version 2\n".to_vec()),
        PktLineFrame::Delimiter,
        PktLineFrame::Data(b"done\n".to_vec()),
        PktLineFrame::Flush,
    ];
    let mut encoded = Vec::new();
    write_pkt_line_frames(&mut encoded, &frames).expect("test operation should succeed");
    assert_eq!(encoded, b"000eversion 2\n00010009done\n0000");
    assert_eq!(
        read_pkt_line_frames(&mut encoded.as_slice()).expect("test operation should succeed"),
        frames
    );

    let mut empty: &[u8] = b"";
    assert_eq!(
        read_pkt_line_frame(&mut empty).expect("test operation should succeed"),
        None
    );
}

#[test]
fn pkt_line_stream_reads_until_control_packets() {
    let input = b"000eversion 2\n0000trailing";
    let frames =
        read_pkt_line_frames_until_flush(&mut &input[..]).expect("test operation should succeed");
    assert_eq!(
        frames,
        vec![
            PktLineFrame::Data(b"version 2\n".to_vec()),
            PktLineFrame::Flush,
        ]
    );

    let input = b"0009done\n0002next";
    let frames = read_pkt_line_frames_until_response_end(&mut &input[..])
        .expect("test operation should succeed");
    assert_eq!(
        frames,
        vec![
            PktLineFrame::Data(b"done\n".to_vec()),
            PktLineFrame::ResponseEnd,
        ]
    );
    assert!(read_pkt_line_frames_until_flush(&mut &b"0009done\n"[..]).is_err());
}

#[test]
fn pkt_line_rejects_invalid_lengths() {
    assert!(PktLineFrame::parse(b"000").is_err());
    assert!(PktLineFrame::parse(b"0003").is_err());
    assert!(PktLineFrame::parse(b"000ahello").is_err());
    assert!(PktLineFrame::parse(b"zzzz").is_err());
    assert!(read_pkt_line_frame(&mut &b"000"[..]).is_err());
    assert!(read_pkt_line_frame(&mut &b"0003"[..]).is_err());
}

#[test]
fn pkt_line_rejects_oversized_data() {
    let payload = vec![b'x'; PKT_LINE_MAX_PAYLOAD_LEN + 1];
    assert!(PktLineFrame::data(payload.clone()).is_err());
    assert!(PktLine(payload.clone()).try_encode().is_err());
    assert!(PktLineFrame::Data(payload.clone()).try_encode().is_err());
    assert!(write_pkt_line_frame(&mut Vec::new(), &PktLineFrame::Data(payload)).is_err());
    assert!(PktLineFrame::parse(b"fff1").is_err());
}

#[test]
fn protocol_error_lines_parse_encode_and_stream() {
    let error =
        parse_error_line(b"ERR remote rejected request\n").expect("test operation should succeed");
    assert_eq!(
        error,
        ProtocolErrorLine {
            message: "remote rejected request".into(),
        }
    );
    assert_eq!(
        encode_error_line(&error).expect("test operation should succeed"),
        b"ERR remote rejected request\n"
    );
    assert_eq!(
        parse_error_frame(&PktLineFrame::Data(
            b"ERR remote rejected request\n".to_vec()
        ))
        .expect("test operation should succeed"),
        Some(error.clone())
    );
    assert_eq!(
        parse_error_frame(&PktLineFrame::Data(b"NAK\n".to_vec()))
            .expect("test operation should succeed"),
        None
    );

    let mut encoded = Vec::new();
    write_error_line(&mut encoded, &error).expect("test operation should succeed");
    encoded.extend_from_slice(b"tail");
    let mut input = encoded.as_slice();
    assert_eq!(
        read_error_line(&mut input).expect("test operation should succeed"),
        error
    );
    assert_eq!(input, b"tail");
}

#[test]
fn protocol_error_lines_reject_malformed_messages() {
    assert!(parse_error_line(b"ERR\n").is_err());
    assert!(parse_error_line(b"ERR \n").is_err());
    assert!(parse_error_line(b"ERR bad\0message\n").is_err());
    assert!(parse_error_line(b"NAK\n").is_err());
    assert!(encode_error_line(&ProtocolErrorLine {
        message: "bad\nmessage".into(),
    })
    .is_err());
    assert!(read_error_line(&mut &b"0000"[..]).is_err());
}

#[test]
fn refspec_parser_handles_fetch_push_and_negative_forms() {
    assert_eq!(
        parse_refspec("+refs/heads/*:refs/remotes/origin/*")
            .expect("test operation should succeed"),
        RefSpec {
            force: true,
            negative: false,
            src: Some("refs/heads/*".into()),
            dst: Some("refs/remotes/origin/*".into()),
            pattern: true,
        }
    );
    assert_eq!(
        parse_refspec("refs/heads/main").expect("test operation should succeed"),
        RefSpec {
            force: false,
            negative: false,
            src: Some("refs/heads/main".into()),
            dst: None,
            pattern: false,
        }
    );
    assert_eq!(
        parse_refspec(":refs/heads/topic").expect("test operation should succeed"),
        RefSpec {
            force: false,
            negative: false,
            src: None,
            dst: Some("refs/heads/topic".into()),
            pattern: false,
        }
    );
    assert_eq!(
        parse_refspec(":").expect("test operation should succeed"),
        RefSpec {
            force: false,
            negative: false,
            src: None,
            dst: None,
            pattern: false,
        }
    );
    assert_eq!(
        parse_refspec("^refs/tags/private/*").expect("test operation should succeed"),
        RefSpec {
            force: false,
            negative: true,
            src: Some("refs/tags/private/*".into()),
            dst: None,
            pattern: true,
        }
    );
}

#[test]
fn refspec_encode_and_map_sources() {
    let pattern = parse_refspec("+refs/heads/*:refs/remotes/origin/*")
        .expect("test operation should succeed");
    assert_eq!(
        encode_refspec(&pattern).expect("test operation should succeed"),
        "+refs/heads/*:refs/remotes/origin/*"
    );
    assert!(
        refspec_matches_source(&pattern, "refs/heads/main").expect("test operation should succeed")
    );
    assert_eq!(
        refspec_map_source(&pattern, "refs/heads/main").expect("test operation should succeed"),
        Some("refs/remotes/origin/main".into())
    );
    assert_eq!(
        refspec_map_source(&pattern, "refs/tags/v1").expect("test operation should succeed"),
        None
    );

    let direct = parse_refspec("HEAD:refs/heads/main").expect("test operation should succeed");
    assert_eq!(
        encode_refspec(&direct).expect("test operation should succeed"),
        "HEAD:refs/heads/main"
    );
    assert_eq!(
        refspec_map_source(&direct, "HEAD").expect("test operation should succeed"),
        Some("refs/heads/main".into())
    );

    let delete = parse_refspec(":refs/heads/old").expect("test operation should succeed");
    assert_eq!(
        encode_refspec(&delete).expect("test operation should succeed"),
        ":refs/heads/old"
    );
    assert_eq!(
        refspec_map_source(&delete, "HEAD").expect("test operation should succeed"),
        None
    );

    let matching = parse_refspec(":").expect("test operation should succeed");
    assert_eq!(
        encode_refspec(&matching).expect("test operation should succeed"),
        ":"
    );
}

#[test]
fn refspec_parser_rejects_malformed_values() {
    assert!(parse_refspec("").is_err());
    assert!(parse_refspec("+^refs/heads/main").is_err());
    assert!(parse_refspec("^refs/heads/main:refs/remotes/origin/main").is_err());
    assert!(parse_refspec("refs/heads/*:refs/remotes/origin/main").is_err());
    assert!(parse_refspec("refs/heads/**:refs/remotes/origin/*").is_err());
    assert!(parse_refspec("refs/heads/main:refs/remotes/origin/main:extra").is_err());
    assert!(parse_refspec("refs/heads/main\n").is_err());
    assert!(encode_refspec(&RefSpec {
        force: false,
        negative: false,
        src: Some("refs/heads/*".into()),
        dst: Some("refs/remotes/origin/main".into()),
        pattern: true,
    })
    .is_err());
}

#[test]
fn fetch_head_records_parse_encode_and_describe_refs() {
    let first = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");
    let second = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "2222222222222222222222222222222222222222",
    )
    .expect("test operation should succeed");
    let input = b"1111111111111111111111111111111111111111\t\tbranch 'main' of ../bundle.bdl\n2222222222222222222222222222222222222222\tnot-for-merge\ttag 'v1' of ../bundle.bdl\n";
    let records =
        parse_fetch_head(ObjectFormat::Sha1, input).expect("test operation should succeed");
    assert_eq!(
        records,
        vec![
            FetchHeadRecord {
                oid: first,
                not_for_merge: false,
                description: "branch 'main' of ../bundle.bdl".into(),
            },
            FetchHeadRecord {
                oid: second,
                not_for_merge: true,
                description: "tag 'v1' of ../bundle.bdl".into(),
            },
        ]
    );
    assert_eq!(
        encode_fetch_head(&records).expect("test operation should succeed"),
        input
    );
    assert_eq!(
        parse_fetch_head(ObjectFormat::Sha1, b"").expect("test operation should succeed"),
        Vec::<FetchHeadRecord>::new()
    );
    assert_eq!(
        fetch_head_remote_description("refs/heads/main", "../bundle.bdl")
            .expect("test operation should succeed"),
        "branch 'main' of ../bundle.bdl"
    );
    assert_eq!(
        fetch_head_remote_description("refs/tags/v1", "../bundle.bdl")
            .expect("test operation should succeed"),
        "tag 'v1' of ../bundle.bdl"
    );
    // A bare `HEAD` fetch records just the URL — git emits an empty note.
    assert_eq!(
        fetch_head_remote_description("HEAD", "../bundle.bdl")
            .expect("test operation should succeed"),
        "../bundle.bdl"
    );
}

#[test]
fn fetch_head_records_streams_round_trip() {
    let records = vec![FetchHeadRecord {
        oid: ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed"),
        not_for_merge: false,
        description: "branch 'main' of ../bundle.bdl".into(),
    }];
    let mut encoded = Vec::new();
    write_fetch_head(&mut encoded, &records).expect("test operation should succeed");
    let mut input = encoded.as_slice();
    assert_eq!(
        read_fetch_head(ObjectFormat::Sha1, &mut input).expect("test operation should succeed"),
        records
    );
    assert!(input.is_empty());
}

#[test]
fn fetch_head_records_reject_malformed_lines() {
    assert!(parse_fetch_head(
        ObjectFormat::Sha1,
        b"1111111111111111111111111111111111111111\t\tbranch 'main'"
    )
    .is_err());
    assert!(parse_fetch_head(
        ObjectFormat::Sha1,
        b"1111111111111111111111111111111111111111\tfor-merge\tbranch 'main'\n"
    )
    .is_err());
    assert!(parse_fetch_head(ObjectFormat::Sha1, b"not-a-hash\t\tbranch 'main'\n").is_err());
    assert!(encode_fetch_head(&[FetchHeadRecord {
        oid: ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111"
        )
        .expect("test operation should succeed"),
        not_for_merge: false,
        description: "bad\ndescription".into(),
    }])
    .is_err());
}

#[test]
fn fetch_planner_maps_direct_pattern_and_negative_refspecs() {
    let main = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");
    let next = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "2222222222222222222222222222222222222222",
    )
    .expect("test operation should succeed");
    let refs = vec![
        RefAdvertisement {
            oid: main.clone(),
            name: "refs/heads/main".into(),
            capabilities: Vec::new(),
        },
        RefAdvertisement {
            oid: next.clone(),
            name: "refs/heads/tmp".into(),
            capabilities: Vec::new(),
        },
    ];
    let refspecs = vec![
        parse_refspec("+refs/heads/*:refs/remotes/origin/*")
            .expect("test operation should succeed"),
        parse_refspec("^refs/heads/tmp").expect("test operation should succeed"),
    ];
    assert_eq!(
        plan_fetch_ref_updates(&refs, &refspecs, false).expect("test operation should succeed"),
        vec![FetchRefUpdate {
            src: "refs/heads/main".into(),
            dst: Some("refs/remotes/origin/main".into()),
            oid: main,
            not_for_merge: false,
            force: true,
        }]
    );
}

#[test]
fn fetch_planner_autofollows_tags_and_builds_fetch_head_records() {
    let commit = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");
    let refs = vec![
        RefAdvertisement {
            oid: commit.clone(),
            name: "refs/heads/main".into(),
            capabilities: Vec::new(),
        },
        RefAdvertisement {
            oid: commit.clone(),
            name: "refs/tags/v1".into(),
            capabilities: Vec::new(),
        },
    ];
    let refspecs =
        vec![parse_refspec("refs/heads/main:refs/heads/main")
            .expect("test operation should succeed")];
    let updates =
        plan_fetch_ref_updates(&refs, &refspecs, true).expect("test operation should succeed");
    assert_eq!(
        updates,
        vec![
            FetchRefUpdate {
                src: "refs/heads/main".into(),
                dst: Some("refs/heads/main".into()),
                oid: commit.clone(),
                not_for_merge: false,
                force: false,
            },
            FetchRefUpdate {
                src: "refs/tags/v1".into(),
                dst: Some("refs/tags/v1".into()),
                oid: commit.clone(),
                not_for_merge: true,
                force: false,
            },
        ]
    );
    assert_eq!(
        fetch_ref_updates_to_fetch_head(&updates, "../bundle.bdl")
            .expect("test operation should succeed"),
        vec![
            FetchHeadRecord {
                oid: commit.clone(),
                not_for_merge: false,
                description: "branch 'main' of ../bundle.bdl".into(),
            },
            FetchHeadRecord {
                oid: commit,
                not_for_merge: true,
                description: "tag 'v1' of ../bundle.bdl".into(),
            },
        ]
    );
}

#[test]
fn fetch_planner_rejects_missing_or_sourceless_refspecs() {
    let refs = vec![RefAdvertisement {
        oid: ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed"),
        name: "refs/heads/main".into(),
        capabilities: Vec::new(),
    }];
    assert!(plan_fetch_ref_updates(
        &refs,
        &[parse_refspec("refs/heads/missing").expect("test operation should succeed")],
        false
    )
    .is_err());
    assert!(plan_fetch_ref_updates(
        &refs,
        &[parse_refspec(":refs/heads/main").expect("test operation should succeed")],
        false
    )
    .is_err());
}

#[test]
fn fetch_planner_sourceless_positive_refspec_returns_err_not_panic() {
    // Regression guard for the sley#7 conversion at the `let Some(src) = ..`
    // binding: a non-pattern positive refspec with no source must return an
    // error, never panic. Construct the malformed RefSpec directly so the
    // test pins the converted guard rather than parse_refspec's behavior.
    let refs = vec![RefAdvertisement {
        oid: ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed"),
        name: "refs/heads/main".into(),
        capabilities: Vec::new(),
    }];
    let malformed = RefSpec {
        force: false,
        negative: false,
        src: None,
        dst: Some("refs/heads/main".into()),
        pattern: false,
    };
    let result = plan_fetch_ref_updates(&refs, &[malformed], false);
    assert!(
        result.is_err(),
        "sourceless positive refspec must yield Err, got {result:?}"
    );
}

#[test]
fn push_planner_builds_create_update_delete_and_matching_commands() {
    let old = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");
    let new = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "2222222222222222222222222222222222222222",
    )
    .expect("test operation should succeed");
    let zero = zero_object_id(ObjectFormat::Sha1).expect("test operation should succeed");
    let local_refs = vec![
        PushSourceRef {
            oid: new.clone(),
            name: "refs/heads/main".into(),
        },
        PushSourceRef {
            oid: new.clone(),
            name: "refs/heads/new".into(),
        },
    ];
    let remote_refs = vec![
        RefAdvertisement {
            oid: old.clone(),
            name: "refs/heads/main".into(),
            capabilities: Vec::new(),
        },
        RefAdvertisement {
            oid: old.clone(),
            name: "refs/heads/old".into(),
            capabilities: Vec::new(),
        },
    ];

    assert_eq!(
        plan_push_commands(
            ObjectFormat::Sha1,
            &local_refs,
            &remote_refs,
            &[parse_refspec("refs/heads/main").expect("test operation should succeed")],
        )
        .expect("test operation should succeed"),
        vec![ReceivePackCommand {
            old_id: old.clone(),
            new_id: new.clone(),
            name: "refs/heads/main".into(),
        }]
    );
    assert_eq!(
        plan_push_commands(
            ObjectFormat::Sha1,
            &local_refs,
            &remote_refs,
            &[parse_refspec("refs/heads/new:refs/heads/new")
                .expect("test operation should succeed")],
        )
        .expect("test operation should succeed"),
        vec![ReceivePackCommand {
            old_id: zero.clone(),
            new_id: new.clone(),
            name: "refs/heads/new".into(),
        }]
    );
    assert_eq!(
        plan_push_commands(
            ObjectFormat::Sha1,
            &local_refs,
            &remote_refs,
            &[parse_refspec(":refs/heads/old").expect("test operation should succeed")],
        )
        .expect("test operation should succeed"),
        vec![ReceivePackCommand {
            old_id: old.clone(),
            new_id: zero,
            name: "refs/heads/old".into(),
        }]
    );
    assert_eq!(
        plan_push_commands(
            ObjectFormat::Sha1,
            &local_refs,
            &remote_refs,
            &[parse_refspec(":").expect("test operation should succeed")],
        )
        .expect("test operation should succeed"),
        vec![ReceivePackCommand {
            old_id: old,
            new_id: new,
            name: "refs/heads/main".into(),
        }]
    );
}

#[test]
fn push_planner_builds_wildcard_commands_and_rejects_bad_refspecs() {
    let new = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "2222222222222222222222222222222222222222",
    )
    .expect("test operation should succeed");
    let zero = zero_object_id(ObjectFormat::Sha1).expect("test operation should succeed");
    let local_refs = vec![PushSourceRef {
        oid: new.clone(),
        name: "refs/heads/topic".into(),
    }];
    let commands = plan_push_commands(
        ObjectFormat::Sha1,
        &local_refs,
        &[],
        &[parse_refspec("refs/heads/*:refs/heads/review/*")
            .expect("test operation should succeed")],
    )
    .expect("test operation should succeed");
    assert_eq!(
        commands,
        vec![ReceivePackCommand {
            old_id: zero,
            new_id: new,
            name: "refs/heads/review/topic".into(),
        }]
    );
    assert!(plan_push_commands(
        ObjectFormat::Sha1,
        &local_refs,
        &[],
        &[parse_refspec("^refs/heads/topic").expect("test operation should succeed")],
    )
    .is_err());
    assert_eq!(
        plan_push_commands(
            ObjectFormat::Sha1,
            &local_refs,
            &[],
            &[parse_refspec(":refs/heads/missing").expect("test operation should succeed")],
        )
        .expect("missing deletes are sent as zero-to-zero commands"),
        vec![ReceivePackCommand {
            old_id: zero,
            new_id: zero,
            name: "refs/heads/missing".into(),
        }]
    );
}

#[test]
fn receive_pack_push_request_builder_negotiates_capabilities() {
    let old_id = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");
    let new_id = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "2222222222222222222222222222222222222222",
    )
    .expect("test operation should succeed");
    let features = ReceivePackFeatures {
        report_status_v2: true,
        atomic: true,
        ofs_delta: true,
        push_options: true,
        side_band_64k: true,
        quiet: true,
        object_format: Some(ObjectFormat::Sha1),
        ..ReceivePackFeatures::default()
    };
    let request = build_receive_pack_push_request(
        &features,
        vec![ReceivePackCommand {
            old_id,
            new_id,
            name: "refs/heads/main".into(),
        }],
        b"PACKdata".to_vec(),
        ReceivePackPushRequestOptions {
            report_status_v2: true,
            atomic: true,
            ofs_delta: true,
            side_band_64k: true,
            quiet: true,
            agent: Some("sley/0".into()),
            object_format: Some(ObjectFormat::Sha1),
            push_options: vec!["ci.skip".into()],
            ..ReceivePackPushRequestOptions::default()
        },
    )
    .expect("test operation should succeed");
    assert_eq!(
        request.commands.capabilities,
        vec![
            Capability {
                name: "report-status-v2".into(),
                value: None,
            },
            Capability {
                name: "atomic".into(),
                value: None,
            },
            Capability {
                name: "ofs-delta".into(),
                value: None,
            },
            Capability {
                name: "side-band-64k".into(),
                value: None,
            },
            Capability {
                name: "quiet".into(),
                value: None,
            },
            Capability {
                name: "agent".into(),
                value: Some("sley/0".into()),
            },
            Capability {
                name: "object-format".into(),
                value: Some("sha1".into()),
            },
            Capability {
                name: "push-options".into(),
                value: None,
            },
        ]
    );
    assert_eq!(request.push_options, Some(vec!["ci.skip".into()]));
    validate_receive_pack_push_request_features(&features, &request)
        .expect("test operation should succeed");
}

#[test]
fn receive_pack_push_request_builder_handles_delete_only_and_rejects_unadvertised() {
    let old_id = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");
    let zero = zero_object_id(ObjectFormat::Sha1).expect("test operation should succeed");
    let features = ReceivePackFeatures {
        delete_refs: true,
        ..ReceivePackFeatures::default()
    };
    let request = build_receive_pack_push_request(
        &features,
        vec![ReceivePackCommand {
            old_id,
            new_id: zero,
            name: "refs/heads/old".into(),
        }],
        Vec::new(),
        ReceivePackPushRequestOptions::default(),
    )
    .expect("test operation should succeed");
    assert_eq!(
        request.commands.capabilities,
        vec![Capability {
            name: "delete-refs".into(),
            value: None,
        }]
    );
    assert!(request.packfile.is_empty());

    assert!(build_receive_pack_push_request(
        &ReceivePackFeatures::default(),
        request.commands.commands.clone(),
        Vec::new(),
        ReceivePackPushRequestOptions::default(),
    )
    .is_err());
    assert!(build_receive_pack_push_request(
        &features,
        request.commands.commands,
        b"PACK".to_vec(),
        ReceivePackPushRequestOptions::default(),
    )
    .is_err());
    assert!(build_receive_pack_push_request(
        &features,
        Vec::new(),
        Vec::new(),
        ReceivePackPushRequestOptions {
            push_options: vec!["ci.skip".into()],
            ..ReceivePackPushRequestOptions::default()
        },
    )
    .is_err());
}

#[test]
fn smart_http_helpers_build_paths_and_content_types() {
    let sha1 = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");
    let sha256 = ObjectId::from_hex(
        ObjectFormat::Sha256,
        "2222222222222222222222222222222222222222222222222222222222222222",
    )
    .expect("test operation should succeed");
    assert_eq!(
        smart_http_info_refs_path("/repo.git/", GitService::UploadPack)
            .expect("test operation should succeed"),
        "/repo.git/info/refs?service=git-upload-pack"
    );
    assert_eq!(
        dumb_http_info_refs_path("/repo.git/").expect("test operation should succeed"),
        "/repo.git/info/refs"
    );
    assert_eq!(
        dumb_http_alternates_path("/repo.git").expect("test operation should succeed"),
        "/repo.git/objects/info/http-alternates"
    );
    assert_eq!(
        dumb_http_packs_path("/repo.git/").expect("test operation should succeed"),
        "/repo.git/objects/info/packs"
    );
    assert_eq!(
        dumb_http_loose_object_path("/repo.git/", &sha1).expect("test operation should succeed"),
        "/repo.git/objects/11/11111111111111111111111111111111111111"
    );
    assert_eq!(
        dumb_http_loose_object_path("/repo.git/", &sha256).expect("test operation should succeed"),
        "/repo.git/objects/22/22222222222222222222222222222222222222222222222222222222222222"
    );
    assert_eq!(
        dumb_http_pack_file_path("/repo.git/", &sha1).expect("test operation should succeed"),
        "/repo.git/objects/pack/pack-1111111111111111111111111111111111111111.pack"
    );
    assert_eq!(
        dumb_http_pack_index_path("/repo.git/", &sha1).expect("test operation should succeed"),
        "/repo.git/objects/pack/pack-1111111111111111111111111111111111111111.idx"
    );
    assert_eq!(
        smart_http_rpc_path("/repo.git", GitService::ReceivePack)
            .expect("test operation should succeed"),
        "/repo.git/git-receive-pack"
    );
    assert_eq!(
        smart_http_advertisement_content_type(GitService::UploadPack)
            .expect("test operation should succeed"),
        "application/x-git-upload-pack-advertisement"
    );
    assert_eq!(
        smart_http_rpc_request_content_type(GitService::UploadPack)
            .expect("test operation should succeed"),
        "application/x-git-upload-pack-request"
    );
    assert_eq!(
        smart_http_rpc_result_content_type(GitService::ReceivePack)
            .expect("test operation should succeed"),
        "application/x-git-receive-pack-result"
    );
    assert_eq!(
        parse_smart_http_advertisement_content_type("Application/X-Git-Upload-Pack-Advertisement")
            .expect("test operation should succeed"),
        GitService::UploadPack
    );
    assert_eq!(
        parse_smart_http_rpc_request_content_type("application/x-git-receive-pack-request")
            .expect("test operation should succeed"),
        GitService::ReceivePack
    );
    assert_eq!(
        parse_smart_http_rpc_result_content_type("application/x-git-upload-pack-result")
            .expect("test operation should succeed"),
        GitService::UploadPack
    );
}

#[test]
fn smart_http_helpers_reject_invalid_services_paths_and_content_types() {
    let oid = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");
    assert!(smart_http_info_refs_path("repo.git", GitService::UploadPack).is_err());
    assert!(smart_http_rpc_path("/repo.git?x=1", GitService::UploadPack).is_err());
    assert!(dumb_http_info_refs_path("repo.git").is_err());
    assert!(dumb_http_alternates_path("/repo.git#fragment").is_err());
    assert!(dumb_http_packs_path("/repo.git?query").is_err());
    assert!(dumb_http_loose_object_path("repo.git", &oid).is_err());
    assert!(dumb_http_pack_file_path("/repo.git#fragment", &oid).is_err());
    assert!(dumb_http_pack_index_path("/repo.git?query", &oid).is_err());
    assert!(smart_http_info_refs_path("/repo.git", GitService::UploadArchive).is_err());
    assert!(smart_http_advertisement_content_type(GitService::UploadArchive).is_err());
    assert!(parse_smart_http_advertisement_content_type(
        "application/x-git-upload-archive-advertisement"
    )
    .is_err());
    assert!(
        parse_smart_http_rpc_request_content_type("application/x-git-upload-pack-result").is_err()
    );
    assert!(parse_smart_http_rpc_result_content_type(
        "application/x-git-receive-pack-result; charset=utf-8"
    )
    .is_err());
}

#[test]
fn sideband_packets_parse_and_encode_channels() {
    let payloads = vec![
        b"\x01PACK bytes".to_vec(),
        b"\x02counting objects\n".to_vec(),
        b"\x03fatal error\n".to_vec(),
    ];
    let packets = parse_sideband_packets(&payloads).expect("test operation should succeed");
    assert_eq!(
        packets,
        vec![
            SideBandPacket {
                channel: SideBandChannel::Data,
                data: b"PACK bytes".to_vec(),
            },
            SideBandPacket {
                channel: SideBandChannel::Progress,
                data: b"counting objects\n".to_vec(),
            },
            SideBandPacket {
                channel: SideBandChannel::Fatal,
                data: b"fatal error\n".to_vec(),
            },
        ]
    );
    assert_eq!(
        encode_sideband_packets(&packets).expect("test operation should succeed"),
        payloads
    );
}

#[test]
fn sideband_stream_parses_encodes_and_demuxes_packets() {
    let frames = vec![
        PktLineFrame::Data(vec![1, b'P', b'A']),
        PktLineFrame::Data(vec![2, b'c', b'o', b'u', b'n', b't', b'\n']),
        PktLineFrame::Data(vec![1, b'C', b'K']),
        PktLineFrame::Flush,
    ];
    let packets = parse_sideband_stream(&frames).expect("test operation should succeed");
    assert_eq!(
        packets,
        vec![
            SideBandPacket {
                channel: SideBandChannel::Data,
                data: b"PA".to_vec(),
            },
            SideBandPacket {
                channel: SideBandChannel::Progress,
                data: b"count\n".to_vec(),
            },
            SideBandPacket {
                channel: SideBandChannel::Data,
                data: b"CK".to_vec(),
            },
        ]
    );
    assert_eq!(
        encode_sideband_stream(&packets).expect("test operation should succeed"),
        frames
    );
    assert_eq!(
        demux_sideband_stream(&frames).expect("test operation should succeed"),
        SideBandDemux {
            data: b"PACK".to_vec(),
            progress: vec![b"count\n".to_vec()],
        }
    );
}

#[test]
fn sideband_stream_reads_and_writes_until_flush() {
    let packets = vec![
        SideBandPacket {
            channel: SideBandChannel::Data,
            data: b"PACK".to_vec(),
        },
        SideBandPacket {
            channel: SideBandChannel::Progress,
            data: b"done\n".to_vec(),
        },
    ];
    let mut encoded = Vec::new();
    write_sideband_stream(&mut encoded, &packets).expect("test operation should succeed");
    encoded.extend_from_slice(b"tail");

    let mut input = encoded.as_slice();
    assert_eq!(
        read_sideband_stream(&mut input).expect("test operation should succeed"),
        packets
    );
    assert_eq!(input, b"tail");

    let mut input = encoded.as_slice();
    assert_eq!(
        read_and_demux_sideband_stream(&mut input).expect("test operation should succeed"),
        SideBandDemux {
            data: b"PACK".to_vec(),
            progress: vec![b"done\n".to_vec()],
        }
    );
    assert_eq!(input, b"tail");
}

#[test]
fn sideband_packets_demux_data_and_progress() {
    let payloads = vec![
        b"\x01PACK".to_vec(),
        b"\x02counting objects\n".to_vec(),
        b"\x01 bytes".to_vec(),
        b"\x02done\n".to_vec(),
    ];
    assert_eq!(
        parse_and_demux_sideband_packets(&payloads).expect("test operation should succeed"),
        SideBandDemux {
            data: b"PACK bytes".to_vec(),
            progress: vec![b"counting objects\n".to_vec(), b"done\n".to_vec()],
        }
    );
}

#[test]
fn sideband_packets_reject_bad_channels_and_oversize_payloads() {
    assert!(parse_sideband_packet(b"").is_err());
    assert!(parse_sideband_packet(b"\x04bad").is_err());
    assert!(parse_sideband_stream(&[PktLineFrame::Data(vec![1, b'P', b'A', b'C', b'K'])]).is_err());
    assert!(parse_sideband_stream(&[PktLineFrame::Delimiter, PktLineFrame::Flush]).is_err());
    assert!(parse_sideband_stream(&[
        PktLineFrame::Data(vec![1, b'P', b'A']),
        PktLineFrame::Flush,
        PktLineFrame::Data(vec![1, b'C', b'K']),
    ])
    .is_err());
    assert!(parse_sideband_stream(&[
        PktLineFrame::Data(vec![1, b'P', b'A']),
        PktLineFrame::Data(b"\x04bad".to_vec()),
        PktLineFrame::Flush,
    ])
    .is_err());
    assert!(encode_sideband_packet(&SideBandPacket {
        channel: SideBandChannel::Data,
        data: vec![0; PKT_LINE_MAX_PAYLOAD_LEN],
    })
    .is_err());
    assert!(demux_sideband_packets(&[SideBandPacket {
        channel: SideBandChannel::Fatal,
        data: b"remote died\n".to_vec(),
    }])
    .is_err());
}

#[test]
fn upload_archive_request_parses_and_encodes_arguments() {
    let frames = vec![
        PktLineFrame::Data(b"argument --format=tar\n".to_vec()),
        PktLineFrame::Data(b"argument HEAD:dir with spaces\n".to_vec()),
        PktLineFrame::Flush,
    ];
    let request = parse_upload_archive_request(&frames).expect("test operation should succeed");
    assert_eq!(
        request,
        UploadArchiveRequest {
            arguments: vec!["--format=tar".into(), "HEAD:dir with spaces".into()],
        }
    );
    assert_eq!(
        encode_upload_archive_request(&request).expect("test operation should succeed"),
        frames
    );
}

#[test]
fn upload_archive_request_streams_round_trip() {
    let request = UploadArchiveRequest {
        arguments: vec!["--prefix=src/".into(), "main".into()],
    };
    let mut encoded = Vec::new();
    write_upload_archive_request(&mut encoded, &request).expect("test operation should succeed");
    encoded.extend_from_slice(b"tail");

    let mut input = encoded.as_slice();
    assert_eq!(
        read_upload_archive_request(&mut input).expect("test operation should succeed"),
        request
    );
    assert_eq!(input, b"tail");
}

#[test]
fn upload_archive_request_rejects_malformed_streams() {
    assert!(parse_upload_archive_request(&[PktLineFrame::Flush]).is_err());
    assert!(parse_upload_archive_request(&[
        PktLineFrame::Data(b"--format=tar\n".to_vec()),
        PktLineFrame::Flush,
    ])
    .is_err());
    assert!(parse_upload_archive_request(&[
        PktLineFrame::Data(b"argument HEAD\n".to_vec()),
        PktLineFrame::Delimiter,
        PktLineFrame::Flush,
    ])
    .is_err());
    assert!(encode_upload_archive_request(&UploadArchiveRequest {
        arguments: vec!["bad\narg".into()],
    })
    .is_err());
}

#[test]
fn upload_archive_response_parses_ack_sideband_and_nack() {
    let ack_frames = vec![
        PktLineFrame::Data(b"ACK\n".to_vec()),
        PktLineFrame::Data(b"\x01tar bytes".to_vec()),
        PktLineFrame::Data(b"\x02progress\n".to_vec()),
        PktLineFrame::Flush,
    ];
    let response =
        parse_upload_archive_response(&ack_frames).expect("test operation should succeed");
    assert_eq!(
        response,
        UploadArchiveResponse::Ack {
            sideband: vec![
                SideBandPacket {
                    channel: SideBandChannel::Data,
                    data: b"tar bytes".to_vec(),
                },
                SideBandPacket {
                    channel: SideBandChannel::Progress,
                    data: b"progress\n".to_vec(),
                },
            ],
        }
    );
    assert_eq!(
        encode_upload_archive_response(&response).expect("test operation should succeed"),
        ack_frames
    );
    assert_eq!(
        demux_upload_archive_response(&response).expect("test operation should succeed"),
        SideBandDemux {
            data: b"tar bytes".to_vec(),
            progress: vec![b"progress\n".to_vec()],
        }
    );

    let nack = UploadArchiveResponse::Nack {
        message: "unreachable tree".into(),
    };
    let nack_frames = vec![
        PktLineFrame::Data(b"NACK unreachable tree\n".to_vec()),
        PktLineFrame::Flush,
    ];
    assert_eq!(
        parse_upload_archive_response(&nack_frames).expect("test operation should succeed"),
        nack
    );
    assert_eq!(
        encode_upload_archive_response(&nack).expect("test operation should succeed"),
        nack_frames
    );
    assert!(demux_upload_archive_response(&nack).is_err());
}

#[test]
fn upload_archive_response_streams_round_trip() {
    let response = UploadArchiveResponse::Ack {
        sideband: vec![SideBandPacket {
            channel: SideBandChannel::Data,
            data: b"tar bytes".to_vec(),
        }],
    };
    let mut encoded = Vec::new();
    write_upload_archive_response(&mut encoded, &response).expect("test operation should succeed");
    encoded.extend_from_slice(b"tail");

    let mut input = encoded.as_slice();
    assert_eq!(
        read_upload_archive_response(&mut input).expect("test operation should succeed"),
        response
    );
    assert_eq!(input, b"tail");
}

#[test]
fn upload_archive_response_rejects_malformed_streams() {
    assert!(parse_upload_archive_response(&[]).is_err());
    assert!(parse_upload_archive_response(&[
        PktLineFrame::Data(b"ACK\n".to_vec()),
        PktLineFrame::Flush,
        PktLineFrame::Data(b"\x01tail".to_vec()),
    ])
    .is_err());
    assert!(parse_upload_archive_response(&[
        PktLineFrame::Data(b"NACK\n".to_vec()),
        PktLineFrame::Flush,
    ])
    .is_err());
    assert!(parse_upload_archive_response(&[
        PktLineFrame::Data(b"NACK denied\n".to_vec()),
        PktLineFrame::Data(b"\x02extra\n".to_vec()),
        PktLineFrame::Flush,
    ])
    .is_err());
    assert!(
        encode_upload_archive_response(&UploadArchiveResponse::Nack {
            message: "bad\nmessage".into(),
        })
        .is_err()
    );
}

#[test]
fn capabilities_parse_and_encode_tokens() {
    let capabilities =
        parse_capabilities(b"multi_ack thin-pack agent=git/2.54.0 symref=HEAD:refs/heads/main\n")
            .expect("test operation should succeed");
    assert_eq!(
        capabilities,
        vec![
            Capability {
                name: "multi_ack".into(),
                value: None,
            },
            Capability {
                name: "thin-pack".into(),
                value: None,
            },
            Capability {
                name: "agent".into(),
                value: Some("git/2.54.0".into()),
            },
            Capability {
                name: "symref".into(),
                value: Some("HEAD:refs/heads/main".into()),
            },
        ]
    );
    assert_eq!(
        encode_capabilities(&capabilities).expect("test operation should succeed"),
        b"multi_ack thin-pack agent=git/2.54.0 symref=HEAD:refs/heads/main"
    );
}

#[test]
fn capabilities_reject_empty_or_delimited_fields() {
    assert!(parse_capabilities(b"multi_ack  thin-pack").is_err());
    assert!(parse_capabilities(b"agent=").is_err());
    assert!(parse_capabilities(b"symref=HEAD:refs/heads/main\nbad").is_err());
    assert!(encode_capabilities(&[Capability {
        name: "bad name".into(),
        value: None,
    }])
    .is_err());
}

#[test]
fn protocol_v2_object_format_uses_capability_or_defaults_to_sha1() {
    assert_eq!(
        protocol_v2_object_format(&[]).expect("test operation should succeed"),
        ObjectFormat::Sha1
    );
    assert_eq!(
        protocol_v2_object_format(&[Capability {
            name: "object-format".into(),
            value: Some("sha256".into()),
        }])
        .expect("test operation should succeed"),
        ObjectFormat::Sha256
    );
    assert!(protocol_v2_object_format(&[Capability {
        name: "object-format".into(),
        value: None,
    }])
    .is_err());
    assert!(protocol_v2_object_format(&[
        Capability {
            name: "object-format".into(),
            value: Some("sha1".into()),
        },
        Capability {
            name: "object-format".into(),
            value: Some("sha256".into()),
        },
    ])
    .is_err());
    assert!(protocol_v2_object_format(&[Capability {
        name: "object-format".into(),
        value: Some("unknown".into()),
    }])
    .is_err());
}

#[test]
fn protocol_v2_command_request_capabilities_validate_against_handshake() {
    let handshake = TransportHandshake {
        protocol: ProtocolVersion::V2,
        capabilities: vec![
            Capability {
                name: "fetch".into(),
                value: Some("shallow filter".into()),
            },
            Capability {
                name: "agent".into(),
                value: Some("sley/0".into()),
            },
            Capability {
                name: "object-format".into(),
                value: Some("sha1".into()),
            },
        ],
    };
    validate_protocol_v2_command_request_capabilities(
        &handshake,
        &ProtocolV2CommandRequest {
            command: "fetch".into(),
            capabilities: vec![
                Capability {
                    name: "agent".into(),
                    value: Some("client/1".into()),
                },
                Capability {
                    name: "object-format".into(),
                    value: Some("sha1".into()),
                },
            ],
            arguments: Vec::new(),
        },
    )
    .expect("test operation should succeed");
    assert!(validate_protocol_v2_command_request_capabilities(
        &handshake,
        &ProtocolV2CommandRequest {
            command: "ls-refs".into(),
            capabilities: Vec::new(),
            arguments: Vec::new(),
        },
    )
    .is_err());
    assert!(validate_protocol_v2_command_request_capabilities(
        &handshake,
        &ProtocolV2CommandRequest {
            command: "fetch".into(),
            capabilities: vec![Capability {
                name: "server-option".into(),
                value: None,
            }],
            arguments: Vec::new(),
        },
    )
    .is_err());
    assert!(validate_protocol_v2_command_request_capabilities(
        &handshake,
        &ProtocolV2CommandRequest {
            command: "fetch".into(),
            capabilities: vec![Capability {
                name: "object-format".into(),
                value: Some("sha256".into()),
            }],
            arguments: Vec::new(),
        },
    )
    .is_err());
    assert!(validate_protocol_v2_command_request_capabilities(
        &handshake,
        &ProtocolV2CommandRequest {
            command: "fetch".into(),
            capabilities: vec![Capability {
                name: "agent".into(),
                value: None,
            }],
            arguments: Vec::new(),
        },
    )
    .is_err());
}

#[test]
fn protocol_v2_command_options_parse_and_encode_known_capabilities() {
    let capabilities = vec![
        Capability {
            name: "agent".into(),
            value: Some("sley/0".into()),
        },
        Capability {
            name: "object-format".into(),
            value: Some("sha256".into()),
        },
        Capability {
            name: "server-option".into(),
            value: Some("trace=true".into()),
        },
        Capability {
            name: "server-option".into(),
            value: Some("region=west".into()),
        },
        Capability {
            name: "session-id".into(),
            value: Some("abc123".into()),
        },
    ];
    let options =
        parse_protocol_v2_command_options(&capabilities).expect("test operation should succeed");
    assert_eq!(
        options,
        ProtocolV2CommandOptions {
            agent: Some("sley/0".into()),
            object_format: Some(ObjectFormat::Sha256),
            server_options: vec!["trace=true".into(), "region=west".into()],
            extra: vec![Capability {
                name: "session-id".into(),
                value: Some("abc123".into()),
            }],
        }
    );
    assert_eq!(
        encode_protocol_v2_command_options(&options).expect("test operation should succeed"),
        capabilities
    );
}

#[test]
fn protocol_v2_command_options_reject_malformed_known_capabilities() {
    assert!(parse_protocol_v2_command_options(&[
        Capability {
            name: "agent".into(),
            value: Some("sley/0".into()),
        },
        Capability {
            name: "agent".into(),
            value: Some("sley/1".into()),
        },
    ])
    .is_err());
    assert!(parse_protocol_v2_command_options(&[Capability {
        name: "object-format".into(),
        value: Some("sha512".into()),
    }])
    .is_err());
    assert!(parse_protocol_v2_command_options(&[Capability {
        name: "server-option".into(),
        value: None,
    }])
    .is_err());
    assert!(
        encode_protocol_v2_command_options(&ProtocolV2CommandOptions {
            extra: vec![Capability {
                name: "server-option".into(),
                value: Some("trace=true".into()),
            }],
            ..ProtocolV2CommandOptions::default()
        })
        .is_err()
    );
}

#[test]
fn protocol_v2_ls_refs_features_parse_and_encode_advertisement() {
    let capabilities = vec![Capability {
        name: "ls-refs".into(),
        value: Some("unborn custom".into()),
    }];
    let features = parse_protocol_v2_ls_refs_features(&capabilities)
        .expect("test operation should succeed")
        .expect("test operation should succeed");
    assert_eq!(
        features,
        ProtocolV2LsRefsFeatures {
            unborn: true,
            unknown: vec!["custom".into()],
        }
    );
    assert_eq!(
        encode_protocol_v2_ls_refs_capability(&features).expect("test operation should succeed"),
        capabilities[0]
    );
    assert_eq!(
        parse_protocol_v2_ls_refs_features(&[Capability {
            name: "ls-refs".into(),
            value: None,
        }])
        .expect("test operation should succeed")
        .expect("test operation should succeed"),
        ProtocolV2LsRefsFeatures::default()
    );
    assert!(parse_protocol_v2_ls_refs_features(&[Capability {
        name: "fetch".into(),
        value: Some("filter".into()),
    }])
    .expect("test operation should succeed")
    .is_none());
}

#[test]
fn protocol_v2_ls_refs_features_reject_malformed_advertisements() {
    assert!(parse_protocol_v2_ls_refs_features(&[
        Capability {
            name: "ls-refs".into(),
            value: None,
        },
        Capability {
            name: "ls-refs".into(),
            value: None,
        },
    ])
    .is_err());
    assert!(parse_protocol_v2_ls_refs_features(&[Capability {
        name: "ls-refs".into(),
        value: Some("unborn  custom".into()),
    }])
    .is_err());
    assert!(
        encode_protocol_v2_ls_refs_capability(&ProtocolV2LsRefsFeatures {
            unknown: vec!["unborn".into()],
            ..ProtocolV2LsRefsFeatures::default()
        })
        .is_err()
    );
}

#[test]
fn protocol_v2_ls_refs_command_request_validates_unborn_feature() {
    let handshake = TransportHandshake {
        protocol: ProtocolVersion::V2,
        capabilities: vec![Capability {
            name: "ls-refs".into(),
            value: Some("unborn".into()),
        }],
    };
    let request = ProtocolV2CommandRequest {
        command: "ls-refs".into(),
        capabilities: Vec::new(),
        arguments: vec![b"unborn".to_vec(), b"ref-prefix HEAD".to_vec()],
    };
    let parsed = validate_protocol_v2_ls_refs_command_request(&handshake, &request)
        .expect("test operation should succeed");
    assert!(parsed.unborn);
    assert_eq!(parsed.ref_prefixes, vec!["HEAD"]);

    let blocked = TransportHandshake {
        protocol: ProtocolVersion::V2,
        capabilities: vec![Capability {
            name: "ls-refs".into(),
            value: None,
        }],
    };
    assert!(validate_protocol_v2_ls_refs_command_request(&blocked, &request).is_err());
}

#[test]
fn protocol_v2_fetch_features_parse_and_encode_advertisement() {
    let capabilities = vec![Capability {
        name: "fetch".into(),
        value: Some(
            "shallow wait-for-done filter ref-in-want sideband-all packfile-uris custom".into(),
        ),
    }];
    let features = parse_protocol_v2_fetch_features(&capabilities)
        .expect("test operation should succeed")
        .expect("test operation should succeed");
    assert_eq!(
        features,
        ProtocolV2FetchFeatures {
            shallow: true,
            wait_for_done: true,
            filter: true,
            ref_in_want: true,
            sideband_all: true,
            packfile_uris: true,
            unknown: vec!["custom".into()],
        }
    );
    assert_eq!(
        encode_protocol_v2_fetch_capability(&features).expect("test operation should succeed"),
        capabilities[0]
    );
    assert_eq!(
        parse_protocol_v2_fetch_features(&[Capability {
            name: "fetch".into(),
            value: None,
        }])
        .expect("test operation should succeed")
        .expect("test operation should succeed"),
        ProtocolV2FetchFeatures::default()
    );
    assert!(parse_protocol_v2_fetch_features(&[])
        .expect("test operation should succeed")
        .is_none());
}

#[test]
fn protocol_v2_fetch_features_reject_malformed_advertisements() {
    assert!(parse_protocol_v2_fetch_features(&[
        Capability {
            name: "fetch".into(),
            value: None,
        },
        Capability {
            name: "fetch".into(),
            value: None,
        },
    ])
    .is_err());
    assert!(parse_protocol_v2_fetch_features(&[Capability {
        name: "fetch".into(),
        value: Some("filter  shallow".into()),
    }])
    .is_err());
    assert!(
        encode_protocol_v2_fetch_capability(&ProtocolV2FetchFeatures {
            unknown: vec!["filter".into()],
            ..ProtocolV2FetchFeatures::default()
        })
        .is_err()
    );
}

#[test]
fn protocol_v2_fetch_request_features_validate_feature_gated_arguments() {
    let features = ProtocolV2FetchFeatures {
        shallow: true,
        wait_for_done: true,
        filter: true,
        ref_in_want: true,
        sideband_all: true,
        packfile_uris: true,
        unknown: Vec::new(),
    };
    validate_protocol_v2_fetch_request_features(
        &features,
        &ProtocolV2FetchRequest {
            want_refs: vec!["refs/heads/main".into()],
            shallow: vec![ObjectId::from_hex(
                ObjectFormat::Sha1,
                "1111111111111111111111111111111111111111",
            )
            .expect("test operation should succeed")],
            deepen: Some(1),
            filter: Some("blob:none".into()),
            packfile_uris: Some("https".into()),
            sideband_all: true,
            wait_for_done: true,
            ..ProtocolV2FetchRequest::default()
        },
    )
    .expect("test operation should succeed");

    let request = ProtocolV2FetchRequest {
        want_refs: vec!["refs/heads/main".into()],
        filter: Some("blob:none".into()),
        sideband_all: true,
        ..ProtocolV2FetchRequest::default()
    };
    assert!(validate_protocol_v2_fetch_request_features(
        &ProtocolV2FetchFeatures::default(),
        &request,
    )
    .is_err());
    assert!(validate_protocol_v2_fetch_request_features(
        &ProtocolV2FetchFeatures {
            ref_in_want: true,
            filter: true,
            ..ProtocolV2FetchFeatures::default()
        },
        &request,
    )
    .is_err());
}

#[test]
fn protocol_v2_fetch_command_request_validates_against_handshake_features() {
    let handshake = TransportHandshake {
        protocol: ProtocolVersion::V2,
        capabilities: vec![
            Capability {
                name: "fetch".into(),
                value: Some("filter ref-in-want".into()),
            },
            Capability {
                name: "agent".into(),
                value: Some("sley/0".into()),
            },
        ],
    };
    let request = ProtocolV2CommandRequest {
        command: "fetch".into(),
        capabilities: vec![Capability {
            name: "agent".into(),
            value: Some("client/1".into()),
        }],
        arguments: vec![
            b"want-ref refs/heads/main".to_vec(),
            b"filter blob:none".to_vec(),
        ],
    };
    let fetch =
        validate_protocol_v2_fetch_command_request(&handshake, ObjectFormat::Sha1, &request)
            .expect("test operation should succeed");
    assert_eq!(fetch.want_refs, vec!["refs/heads/main"]);
    assert_eq!(fetch.filter.as_deref(), Some("blob:none"));

    let mut bad = request.clone();
    bad.arguments.push(b"sideband-all".to_vec());
    assert!(
        validate_protocol_v2_fetch_command_request(&handshake, ObjectFormat::Sha1, &bad).is_err()
    );
}

#[test]
fn protocol_v2_object_info_request_parses_encodes_and_validates() {
    let oid = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");
    let request = ProtocolV2CommandRequest {
        command: "object-info".into(),
        capabilities: Vec::new(),
        arguments: vec![
            b"size".to_vec(),
            b"oid 1111111111111111111111111111111111111111".to_vec(),
        ],
    };
    let parsed = ProtocolV2ObjectInfoRequest::from_command_request(ObjectFormat::Sha1, &request)
        .expect("test operation should succeed");
    assert_eq!(
        parsed,
        ProtocolV2ObjectInfoRequest {
            size: true,
            oids: vec![oid],
        }
    );
    assert_eq!(
        parsed
            .to_command_request()
            .expect("test operation should succeed"),
        request
    );

    let handshake = TransportHandshake {
        protocol: ProtocolVersion::V2,
        capabilities: vec![Capability {
            name: "object-info".into(),
            value: None,
        }],
    };
    assert_eq!(
        validate_protocol_v2_object_info_command_request(&handshake, ObjectFormat::Sha1, &request,)
            .expect("test operation should succeed"),
        parsed
    );
}

#[test]
fn protocol_v2_object_info_request_streams_round_trip() {
    let request = ProtocolV2ObjectInfoRequest {
        size: true,
        oids: vec![ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed")],
    };
    let mut encoded = Vec::new();
    write_protocol_v2_object_info_request(&mut encoded, &request)
        .expect("test operation should succeed");
    encoded.extend_from_slice(b"tail");

    let mut input = encoded.as_slice();
    assert_eq!(
        read_protocol_v2_object_info_request(ObjectFormat::Sha1, &mut input)
            .expect("test operation should succeed"),
        request
    );
    assert_eq!(input, b"tail");
}

#[test]
fn protocol_v2_object_info_request_rejects_malformed_arguments() {
    assert!(ProtocolV2ObjectInfoRequest::from_command_request(
        ObjectFormat::Sha1,
        &ProtocolV2CommandRequest {
            command: "object-info".into(),
            capabilities: Vec::new(),
            arguments: vec![b"oid 1111111111111111111111111111111111111111".to_vec()],
        },
    )
    .is_err());
    assert!(ProtocolV2ObjectInfoRequest::from_command_request(
        ObjectFormat::Sha1,
        &ProtocolV2CommandRequest {
            command: "object-info".into(),
            capabilities: Vec::new(),
            arguments: vec![b"size".to_vec(), b"size".to_vec()],
        },
    )
    .is_err());
    assert!(ProtocolV2ObjectInfoRequest::from_command_request(
        ObjectFormat::Sha1,
        &ProtocolV2CommandRequest {
            command: "object-info".into(),
            capabilities: Vec::new(),
            arguments: vec![b"size".to_vec()],
        },
    )
    .is_err());
    assert!(ProtocolV2ObjectInfoRequest::from_command_request(
        ObjectFormat::Sha1,
        &ProtocolV2CommandRequest {
            command: "object-info".into(),
            capabilities: Vec::new(),
            arguments: vec![b"size".to_vec(), b"oid not-an-oid".to_vec()],
        },
    )
    .is_err());
    assert!(validate_protocol_v2_object_info_command_request(
        &TransportHandshake {
            protocol: ProtocolVersion::V2,
            capabilities: Vec::new(),
        },
        ObjectFormat::Sha1,
        &ProtocolV2CommandRequest {
            command: "object-info".into(),
            capabilities: Vec::new(),
            arguments: vec![
                b"size".to_vec(),
                b"oid 1111111111111111111111111111111111111111".to_vec(),
            ],
        },
    )
    .is_err());
}

#[test]
fn protocol_v2_command_request_classifies_known_and_unknown_commands() {
    let handshake = TransportHandshake {
        protocol: ProtocolVersion::V2,
        capabilities: vec![
            Capability {
                name: "ls-refs".into(),
                value: Some("unborn".into()),
            },
            Capability {
                name: "fetch".into(),
                value: Some("filter ref-in-want".into()),
            },
            Capability {
                name: "object-info".into(),
                value: None,
            },
            Capability {
                name: "server-option".into(),
                value: None,
            },
            Capability {
                name: "server-info".into(),
                value: Some("custom".into()),
            },
        ],
    };
    assert_eq!(
        classify_protocol_v2_command_request(
            &handshake,
            ObjectFormat::Sha1,
            &ProtocolV2CommandRequest {
                command: "ls-refs".into(),
                capabilities: Vec::new(),
                arguments: vec![b"unborn".to_vec()],
            },
        )
        .expect("test operation should succeed"),
        ProtocolV2Command::LsRefs(ProtocolV2LsRefsRequest {
            unborn: true,
            ..ProtocolV2LsRefsRequest::default()
        })
    );
    assert_eq!(
        classify_protocol_v2_command_request(
            &handshake,
            ObjectFormat::Sha1,
            &ProtocolV2CommandRequest {
                command: "fetch".into(),
                capabilities: Vec::new(),
                arguments: vec![
                    b"want-ref refs/heads/main".to_vec(),
                    b"filter blob:none".to_vec(),
                ],
            },
        )
        .expect("test operation should succeed"),
        ProtocolV2Command::Fetch(ProtocolV2FetchRequest {
            want_refs: vec!["refs/heads/main".into()],
            filter: Some("blob:none".into()),
            ..ProtocolV2FetchRequest::default()
        })
    );
    assert_eq!(
        classify_protocol_v2_command_request(
            &handshake,
            ObjectFormat::Sha1,
            &ProtocolV2CommandRequest {
                command: "object-info".into(),
                capabilities: Vec::new(),
                arguments: vec![
                    b"size".to_vec(),
                    b"oid 1111111111111111111111111111111111111111".to_vec(),
                ],
            },
        )
        .expect("test operation should succeed"),
        ProtocolV2Command::ObjectInfo(ProtocolV2ObjectInfoRequest {
            size: true,
            oids: vec![ObjectId::from_hex(
                ObjectFormat::Sha1,
                "1111111111111111111111111111111111111111",
            )
            .expect("test operation should succeed")],
        })
    );

    let unknown = ProtocolV2CommandRequest {
        command: "server-info".into(),
        capabilities: vec![Capability {
            name: "server-option".into(),
            value: Some("trace=true".into()),
        }],
        arguments: Vec::new(),
    };
    assert_eq!(
        classify_protocol_v2_command_request(&handshake, ObjectFormat::Sha1, &unknown)
            .expect("test operation should succeed"),
        ProtocolV2Command::Unknown(unknown)
    );
    assert!(classify_protocol_v2_command_request(
        &handshake,
        ObjectFormat::Sha1,
        &ProtocolV2CommandRequest {
            command: "not-advertised".into(),
            capabilities: Vec::new(),
            arguments: Vec::new(),
        },
    )
    .is_err());
}

#[test]
fn protocol_v2_session_request_classifies_streamed_command_and_done() {
    let handshake = TransportHandshake {
        protocol: ProtocolVersion::V2,
        capabilities: vec![
            Capability {
                name: "ls-refs".into(),
                value: Some("unborn".into()),
            },
            Capability {
                name: "fetch".into(),
                value: Some("filter ref-in-want".into()),
            },
        ],
    };
    let command = ProtocolV2Request::Command(ProtocolV2CommandRequest {
        command: "ls-refs".into(),
        capabilities: Vec::new(),
        arguments: vec![b"unborn".to_vec()],
    });
    assert_eq!(
        classify_protocol_v2_request(&handshake, ObjectFormat::Sha1, &command)
            .expect("test operation should succeed"),
        ProtocolV2SessionRequest::Command(ProtocolV2Command::LsRefs(ProtocolV2LsRefsRequest {
            unborn: true,
            ..ProtocolV2LsRefsRequest::default()
        }))
    );
    assert_eq!(
        classify_protocol_v2_request(&handshake, ObjectFormat::Sha1, &ProtocolV2Request::Done)
            .expect("test operation should succeed"),
        ProtocolV2SessionRequest::Done
    );

    let mut encoded = Vec::new();
    write_protocol_v2_request(&mut encoded, &command).expect("test operation should succeed");
    write_protocol_v2_request(&mut encoded, &ProtocolV2Request::Done)
        .expect("test operation should succeed");
    encoded.extend_from_slice(b"tail");

    let mut input = encoded.as_slice();
    assert_eq!(
        read_protocol_v2_session_request(&handshake, ObjectFormat::Sha1, &mut input)
            .expect("test operation should succeed"),
        ProtocolV2SessionRequest::Command(ProtocolV2Command::LsRefs(ProtocolV2LsRefsRequest {
            unborn: true,
            ..ProtocolV2LsRefsRequest::default()
        }))
    );
    assert_eq!(
        read_protocol_v2_session_request(&handshake, ObjectFormat::Sha1, &mut input)
            .expect("test operation should succeed"),
        ProtocolV2SessionRequest::Done
    );
    assert_eq!(input, b"tail");
}

#[test]
fn advertised_ref_parses_first_v0_capability_line() {
    let payload =
        b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391 HEAD\0multi_ack symref=HEAD:refs/heads/main\n";
    let advertisement = parse_ref_advertisement(ObjectFormat::Sha1, payload)
        .expect("test operation should succeed");
    assert_eq!(
        advertisement.oid,
        ObjectId::from_hex(
            ObjectFormat::Sha1,
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        )
        .expect("test operation should succeed")
    );
    assert_eq!(advertisement.name, "HEAD");
    assert_eq!(
        advertisement.capabilities,
        vec![
            Capability {
                name: "multi_ack".into(),
                value: None,
            },
            Capability {
                name: "symref".into(),
                value: Some("HEAD:refs/heads/main".into()),
            },
        ]
    );
}

#[test]
fn advertised_ref_parses_lines_without_capabilities() {
    let advertisement = parse_ref_advertisement(
        ObjectFormat::Sha1,
        b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391 refs/heads/main\n",
    )
    .expect("test operation should succeed");
    assert_eq!(advertisement.name, "refs/heads/main");
    assert!(advertisement.capabilities.is_empty());
}

#[test]
fn advertised_ref_rejects_malformed_payloads() {
    assert!(parse_ref_advertisement(ObjectFormat::Sha1, b"not-an-oid refs/heads/main\n").is_err());
    assert!(parse_ref_advertisement(
        ObjectFormat::Sha1,
        b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391\n"
    )
    .is_err());
}

#[test]
fn advertised_refs_parse_and_encode_stream() {
    let main = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");
    let feature = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "2222222222222222222222222222222222222222",
    )
    .expect("test operation should succeed");
    let frames = vec![
            PktLineFrame::Data(
                b"1111111111111111111111111111111111111111 HEAD\0multi_ack thin-pack agent=git/2.54.0\n"
                    .to_vec(),
            ),
            PktLineFrame::Data(
                b"2222222222222222222222222222222222222222 refs/heads/feature\n".to_vec(),
            ),
            PktLineFrame::Flush,
        ];
    let advertisements = parse_ref_advertisements(ObjectFormat::Sha1, &frames)
        .expect("test operation should succeed");
    assert_eq!(
        advertisements,
        vec![
            RefAdvertisement {
                oid: main,
                name: "HEAD".into(),
                capabilities: vec![
                    Capability {
                        name: "multi_ack".into(),
                        value: None,
                    },
                    Capability {
                        name: "thin-pack".into(),
                        value: None,
                    },
                    Capability {
                        name: "agent".into(),
                        value: Some("git/2.54.0".into()),
                    },
                ],
            },
            RefAdvertisement {
                oid: feature,
                name: "refs/heads/feature".into(),
                capabilities: Vec::new(),
            },
        ]
    );
    assert_eq!(
        encode_ref_advertisements(&advertisements).expect("test operation should succeed"),
        frames
    );
    assert_eq!(
        parse_ref_advertisements(ObjectFormat::Sha1, &[PktLineFrame::Flush])
            .expect("test operation should succeed"),
        Vec::<RefAdvertisement>::new()
    );
}

#[test]
fn advertised_ref_set_parses_v1_version_refs_and_shallow() {
    let main = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");
    let feature = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "2222222222222222222222222222222222222222",
    )
    .expect("test operation should succeed");
    let shallow = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "3333333333333333333333333333333333333333",
    )
    .expect("test operation should succeed");
    let frames = vec![
            PktLineFrame::Data(b"version 1\n".to_vec()),
            PktLineFrame::Data(
                b"1111111111111111111111111111111111111111 HEAD\0multi_ack symref=HEAD:refs/heads/main\n"
                    .to_vec(),
            ),
            PktLineFrame::Data(
                b"2222222222222222222222222222222222222222 refs/heads/feature\n".to_vec(),
            ),
            PktLineFrame::Data(b"shallow 3333333333333333333333333333333333333333\n".to_vec()),
            PktLineFrame::Flush,
        ];

    let set = parse_ref_advertisement_set(ObjectFormat::Sha1, &frames)
        .expect("test operation should succeed");
    assert_eq!(set.protocol, ProtocolVersion::V1);
    assert_eq!(set.shallow, vec![shallow]);
    assert_eq!(
        set.refs,
        vec![
            RefAdvertisement {
                oid: main,
                name: "HEAD".into(),
                capabilities: vec![
                    Capability {
                        name: "multi_ack".into(),
                        value: None,
                    },
                    Capability {
                        name: "symref".into(),
                        value: Some("HEAD:refs/heads/main".into()),
                    },
                ],
            },
            RefAdvertisement {
                oid: feature,
                name: "refs/heads/feature".into(),
                capabilities: Vec::new(),
            },
        ]
    );
    assert_eq!(
        parse_ref_advertisements(ObjectFormat::Sha1, &frames)
            .expect("test operation should succeed"),
        set.refs
    );
    assert_eq!(
        encode_ref_advertisement_set(&set).expect("test operation should succeed"),
        frames
    );
}

#[test]
fn advertised_refs_streams_round_trip() {
    let advertisements = vec![RefAdvertisement {
        oid: ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed"),
        name: "HEAD".into(),
        capabilities: vec![Capability {
            name: "symref".into(),
            value: Some("HEAD:refs/heads/main".into()),
        }],
    }];
    let mut encoded = Vec::new();
    write_ref_advertisements(&mut encoded, &advertisements).expect("test operation should succeed");
    encoded.extend_from_slice(b"tail");

    let mut input = encoded.as_slice();
    assert_eq!(
        read_ref_advertisements(ObjectFormat::Sha1, &mut input)
            .expect("test operation should succeed"),
        advertisements
    );
    assert_eq!(input, b"tail");
}

#[test]
fn advertised_ref_set_streams_round_trip() {
    let set = RefAdvertisementSet {
        protocol: ProtocolVersion::V1,
        refs: vec![RefAdvertisement {
            oid: ObjectId::from_hex(
                ObjectFormat::Sha1,
                "1111111111111111111111111111111111111111",
            )
            .expect("test operation should succeed"),
            name: "HEAD".into(),
            capabilities: vec![Capability {
                name: "symref".into(),
                value: Some("HEAD:refs/heads/main".into()),
            }],
        }],
        shallow: vec![ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .expect("test operation should succeed")],
    };
    let mut encoded = Vec::new();
    write_ref_advertisement_set(&mut encoded, &set).expect("test operation should succeed");
    encoded.extend_from_slice(b"tail");

    let mut input = encoded.as_slice();
    assert_eq!(
        read_ref_advertisement_set(ObjectFormat::Sha1, &mut input)
            .expect("test operation should succeed"),
        set
    );
    assert_eq!(input, b"tail");
}

#[test]
fn advertised_refs_reject_malformed_streams() {
    assert!(parse_ref_advertisements(
        ObjectFormat::Sha1,
        &[PktLineFrame::Data(
            b"1111111111111111111111111111111111111111 HEAD\n".to_vec(),
        )],
    )
    .is_err());
    assert!(parse_ref_advertisements(
        ObjectFormat::Sha1,
        &[PktLineFrame::Delimiter, PktLineFrame::Flush],
    )
    .is_err());
    assert!(parse_ref_advertisements(
        ObjectFormat::Sha1,
        &[
            PktLineFrame::Data(b"1111111111111111111111111111111111111111 HEAD\n".to_vec(),),
            PktLineFrame::Data(
                b"2222222222222222222222222222222222222222 refs/heads/main\0thin-pack\n".to_vec(),
            ),
            PktLineFrame::Flush,
        ],
    )
    .is_err());
    assert!(parse_ref_advertisement_set(
        ObjectFormat::Sha1,
        &[
            PktLineFrame::Data(b"1111111111111111111111111111111111111111 HEAD\n".to_vec(),),
            PktLineFrame::Data(b"version 1\n".to_vec()),
            PktLineFrame::Flush,
        ],
    )
    .is_err());
    assert!(parse_ref_advertisement_set(
        ObjectFormat::Sha1,
        &[
            PktLineFrame::Data(b"version 2\n".to_vec()),
            PktLineFrame::Flush,
        ],
    )
    .is_err());
    assert!(parse_ref_advertisement_set(
        ObjectFormat::Sha1,
        &[
            PktLineFrame::Data(b"shallow 1111111111111111111111111111111111111111\n".to_vec()),
            PktLineFrame::Flush,
        ],
    )
    .is_err());
    assert!(parse_ref_advertisement_set(
        ObjectFormat::Sha1,
        &[
            PktLineFrame::Data(b"1111111111111111111111111111111111111111 HEAD\n".to_vec(),),
            PktLineFrame::Data(b"shallow not-an-oid\n".to_vec()),
            PktLineFrame::Flush,
        ],
    )
    .is_err());
    assert!(parse_ref_advertisement_set(
        ObjectFormat::Sha1,
        &[
            PktLineFrame::Data(b"1111111111111111111111111111111111111111 HEAD\n".to_vec(),),
            PktLineFrame::Data(b"shallow 2222222222222222222222222222222222222222\n".to_vec()),
            PktLineFrame::Data(
                b"3333333333333333333333333333333333333333 refs/heads/main\n".to_vec(),
            ),
            PktLineFrame::Flush,
        ],
    )
    .is_err());
    assert!(encode_ref_advertisements(&[
        RefAdvertisement {
            oid: ObjectId::from_hex(
                ObjectFormat::Sha1,
                "1111111111111111111111111111111111111111",
            )
            .expect("test operation should succeed"),
            name: "HEAD".into(),
            capabilities: Vec::new(),
        },
        RefAdvertisement {
            oid: ObjectId::from_hex(
                ObjectFormat::Sha1,
                "2222222222222222222222222222222222222222",
            )
            .expect("test operation should succeed"),
            name: "refs/heads/main".into(),
            capabilities: vec![Capability {
                name: "thin-pack".into(),
                value: None,
            }],
        },
    ])
    .is_err());
    assert!(encode_ref_advertisement(&RefAdvertisement {
        oid: ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed"),
        name: "bad ref".into(),
        capabilities: Vec::new(),
    })
    .is_err());
    assert!(encode_ref_advertisement_set(&RefAdvertisementSet {
        protocol: ProtocolVersion::V2,
        refs: Vec::new(),
        shallow: Vec::new(),
    })
    .is_err());
    assert!(encode_ref_advertisement_set(&RefAdvertisementSet {
        protocol: ProtocolVersion::V0,
        refs: Vec::new(),
        shallow: vec![ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed")],
    })
    .is_err());
}

#[test]
fn dumb_http_info_refs_parse_and_encode_records() {
    let main = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");
    let tag = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "2222222222222222222222222222222222222222",
    )
    .expect("test operation should succeed");
    let peeled = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "3333333333333333333333333333333333333333",
    )
    .expect("test operation should succeed");
    let input = b"1111111111111111111111111111111111111111\trefs/heads/main\n2222222222222222222222222222222222222222\trefs/tags/v1.0\n3333333333333333333333333333333333333333\trefs/tags/v1.0^{}\n";

    let records = parse_dumb_http_info_refs(ObjectFormat::Sha1, input)
        .expect("test operation should succeed");
    assert_eq!(
        records,
        vec![
            DumbHttpRefRecord {
                oid: main,
                name: "refs/heads/main".into(),
                peeled: false,
            },
            DumbHttpRefRecord {
                oid: tag,
                name: "refs/tags/v1.0".into(),
                peeled: false,
            },
            DumbHttpRefRecord {
                oid: peeled,
                name: "refs/tags/v1.0".into(),
                peeled: true,
            },
        ]
    );
    assert_eq!(
        encode_dumb_http_info_refs(&records).expect("test operation should succeed"),
        input
    );
    assert_eq!(
        parse_dumb_http_info_refs(ObjectFormat::Sha1, b"").expect("test operation should succeed"),
        Vec::<DumbHttpRefRecord>::new()
    );
}

#[test]
fn dumb_http_info_refs_streams_round_trip() {
    let records = vec![DumbHttpRefRecord {
        oid: ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed"),
        name: "refs/heads/main".into(),
        peeled: false,
    }];
    let mut encoded = Vec::new();
    write_dumb_http_info_refs(&mut encoded, &records).expect("test operation should succeed");
    let mut input = encoded.as_slice();
    assert_eq!(
        read_dumb_http_info_refs(ObjectFormat::Sha1, &mut input)
            .expect("test operation should succeed"),
        records
    );
    assert!(input.is_empty());
}

#[test]
fn dumb_http_info_refs_reject_malformed_records() {
    assert!(parse_dumb_http_info_refs(
        ObjectFormat::Sha1,
        b"1111111111111111111111111111111111111111 refs/heads/main\n",
    )
    .is_err());
    assert!(parse_dumb_http_info_refs(
        ObjectFormat::Sha1,
        b"1111111111111111111111111111111111111111\trefs/heads/main",
    )
    .is_err());
    assert!(
        parse_dumb_http_info_refs(ObjectFormat::Sha1, b"not-an-oid\trefs/heads/main\n").is_err()
    );
    assert!(parse_dumb_http_info_refs(
        ObjectFormat::Sha1,
        b"1111111111111111111111111111111111111111\tbad ref\n",
    )
    .is_err());
    assert!(encode_dumb_http_info_refs(&[DumbHttpRefRecord {
        oid: ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed"),
        name: "refs/tags/v1.0^{}".into(),
        peeled: false,
    }])
    .is_err());
}

#[test]
fn dumb_http_alternates_parse_and_encode_locations() {
    let input = b"https://example.com/base.git/objects/\n../other.git/objects/\n";
    let alternates = parse_dumb_http_alternates(input).expect("test operation should succeed");
    assert_eq!(
        alternates,
        vec![
            "https://example.com/base.git/objects/".to_string(),
            "../other.git/objects/".to_string(),
        ]
    );
    assert_eq!(
        encode_dumb_http_alternates(&alternates).expect("test operation should succeed"),
        input
    );
    assert_eq!(
        parse_dumb_http_alternates(b"").expect("test operation should succeed"),
        Vec::<String>::new()
    );
}

#[test]
fn dumb_http_alternates_streams_round_trip() {
    let alternates = vec!["https://example.com/base.git/objects/".to_string()];
    let mut encoded = Vec::new();
    write_dumb_http_alternates(&mut encoded, &alternates).expect("test operation should succeed");
    let mut input = encoded.as_slice();
    assert_eq!(
        read_dumb_http_alternates(&mut input).expect("test operation should succeed"),
        alternates
    );
    assert!(input.is_empty());
}

#[test]
fn dumb_http_alternates_reject_malformed_lines() {
    assert!(parse_dumb_http_alternates(b"https://example.com/base.git/objects/").is_err());
    assert!(parse_dumb_http_alternates(b"\n").is_err());
    assert!(parse_dumb_http_alternates(b"https://example.com/base.git/objects/\r\n").is_err());
    assert!(encode_dumb_http_alternates(&["bad\nalternate".to_string()]).is_err());
}

#[test]
fn dumb_http_packs_parse_and_encode_pack_records() {
    let first = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");
    let second = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "2222222222222222222222222222222222222222",
    )
    .expect("test operation should succeed");
    let input = b"P pack-1111111111111111111111111111111111111111.pack\nP pack-2222222222222222222222222222222222222222.pack\n";
    let records =
        parse_dumb_http_packs(ObjectFormat::Sha1, input).expect("test operation should succeed");
    assert_eq!(
        records,
        vec![
            DumbHttpPackRecord { hash: first },
            DumbHttpPackRecord { hash: second },
        ]
    );
    assert_eq!(
        encode_dumb_http_packs(&records).expect("test operation should succeed"),
        input
    );
    assert_eq!(
        parse_dumb_http_packs(ObjectFormat::Sha1, b"").expect("test operation should succeed"),
        Vec::<DumbHttpPackRecord>::new()
    );
}

#[test]
fn dumb_http_packs_streams_round_trip() {
    let records = vec![DumbHttpPackRecord {
        hash: ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed"),
    }];
    let mut encoded = Vec::new();
    write_dumb_http_packs(&mut encoded, &records).expect("test operation should succeed");
    let mut input = encoded.as_slice();
    assert_eq!(
        read_dumb_http_packs(ObjectFormat::Sha1, &mut input)
            .expect("test operation should succeed"),
        records
    );
    assert!(input.is_empty());
}

#[test]
fn dumb_http_packs_reject_malformed_records() {
    assert!(parse_dumb_http_packs(
        ObjectFormat::Sha1,
        b"P pack-1111111111111111111111111111111111111111.pack",
    )
    .is_err());
    assert!(parse_dumb_http_packs(
        ObjectFormat::Sha1,
        b"pack-1111111111111111111111111111111111111111.pack\n",
    )
    .is_err());
    assert!(parse_dumb_http_packs(ObjectFormat::Sha1, b"P pack-not-a-hash.pack\n",).is_err());
    assert!(parse_dumb_http_packs(
        ObjectFormat::Sha1,
        b"P pack-1111111111111111111111111111111111111111.idx\n",
    )
    .is_err());
}

#[test]
fn upload_pack_features_parse_encode_and_validate_request() {
    let capabilities = vec![
        Capability {
            name: "multi_ack".into(),
            value: None,
        },
        Capability {
            name: "multi_ack_detailed".into(),
            value: None,
        },
        Capability {
            name: "no-done".into(),
            value: None,
        },
        Capability {
            name: "thin-pack".into(),
            value: None,
        },
        Capability {
            name: "side-band-64k".into(),
            value: None,
        },
        Capability {
            name: "ofs-delta".into(),
            value: None,
        },
        Capability {
            name: "shallow".into(),
            value: None,
        },
        Capability {
            name: "deepen-since".into(),
            value: None,
        },
        Capability {
            name: "deepen-not".into(),
            value: None,
        },
        Capability {
            name: "include-tag".into(),
            value: None,
        },
        Capability {
            name: "no-progress".into(),
            value: None,
        },
        Capability {
            name: "filter".into(),
            value: None,
        },
        Capability {
            name: "agent".into(),
            value: Some("git/2.54.0".into()),
        },
        Capability {
            name: "object-format".into(),
            value: Some("sha256".into()),
        },
        Capability {
            name: "symref".into(),
            value: Some("HEAD:refs/heads/main".into()),
        },
        Capability {
            name: "custom".into(),
            value: Some("value".into()),
        },
    ];
    let features =
        parse_upload_pack_features(&capabilities).expect("test operation should succeed");
    assert_eq!(
        features,
        UploadPackFeatures {
            multi_ack: true,
            multi_ack_detailed: true,
            no_done: true,
            thin_pack: true,
            side_band_64k: true,
            ofs_delta: true,
            shallow: true,
            deepen_since: true,
            deepen_not: true,
            include_tag: true,
            no_progress: true,
            filter: true,
            agent: Some("git/2.54.0".into()),
            object_format: Some(ObjectFormat::Sha256),
            symrefs: vec!["HEAD:refs/heads/main".into()],
            unknown: vec![Capability {
                name: "custom".into(),
                value: Some("value".into()),
            }],
            ..UploadPackFeatures::default()
        }
    );
    assert_eq!(
        encode_upload_pack_features(&features).expect("test operation should succeed"),
        capabilities
    );

    let request = UploadPackRequest {
        wants: vec![ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed")],
        capabilities: vec![
            Capability {
                name: "multi_ack_detailed".into(),
                value: None,
            },
            Capability {
                name: "thin-pack".into(),
                value: None,
            },
            Capability {
                name: "side-band-64k".into(),
                value: None,
            },
            Capability {
                name: "ofs-delta".into(),
                value: None,
            },
            Capability {
                name: "include-tag".into(),
                value: None,
            },
            Capability {
                name: "agent".into(),
                value: Some("sley".into()),
            },
        ],
        shallow: vec![ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .expect("test operation should succeed")],
        deepen: Some(5),
        deepen_since: Some(1_710_000_000),
        deepen_not: vec!["refs/tags/base".into()],
        filter: Some("blob:none".into()),
    };
    validate_upload_pack_request_features(&features, &request)
        .expect("test operation should succeed");
}

#[test]
fn upload_pack_features_reject_invalid_requests() {
    let want = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");
    let features = UploadPackFeatures {
        thin_pack: true,
        side_band: true,
        ..UploadPackFeatures::default()
    };

    assert!(validate_upload_pack_request_features(
        &features,
        &UploadPackRequest {
            wants: vec![want],
            capabilities: vec![Capability {
                name: "ofs-delta".into(),
                value: None,
            }],
            ..UploadPackRequest::default()
        },
    )
    .is_err());
    assert!(validate_upload_pack_request_features(
        &features,
        &UploadPackRequest {
            wants: vec![want],
            shallow: vec![want],
            ..UploadPackRequest::default()
        },
    )
    .is_err());
    assert!(validate_upload_pack_request_features(
        &features,
        &UploadPackRequest {
            wants: vec![want],
            filter: Some("blob:none".into()),
            ..UploadPackRequest::default()
        },
    )
    .is_err());
    assert!(validate_upload_pack_request_features(
        &UploadPackFeatures {
            side_band: true,
            side_band_64k: true,
            ..UploadPackFeatures::default()
        },
        &UploadPackRequest {
            wants: vec![want],
            capabilities: vec![
                Capability {
                    name: "side-band".into(),
                    value: None,
                },
                Capability {
                    name: "side-band-64k".into(),
                    value: None,
                },
            ],
            ..UploadPackRequest::default()
        },
    )
    .is_err());

    assert!(parse_upload_pack_features(&[
        Capability {
            name: "thin-pack".into(),
            value: None,
        },
        Capability {
            name: "thin-pack".into(),
            value: None,
        },
    ])
    .is_err());
    assert!(encode_upload_pack_features(&UploadPackFeatures {
        unknown: vec![Capability {
            name: "filter".into(),
            value: None,
        }],
        ..UploadPackFeatures::default()
    })
    .is_err());
}

#[test]
fn upload_pack_raw_response_builder_filters_unknown_haves_and_builds_pack() {
    let want = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");
    let known_have = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "2222222222222222222222222222222222222222",
    )
    .expect("test operation should succeed");
    let unknown_have = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "3333333333333333333333333333333333333333",
    )
    .expect("test operation should succeed");
    let existing = std::collections::HashSet::from([want, known_have]);

    let response = build_upload_pack_raw_packfile_response(
        &UploadPackFeatures::default(),
        UploadPackRequest {
            wants: vec![want],
            ..UploadPackRequest::default()
        },
        [known_have, unknown_have],
        |oid| Ok(existing.contains(oid)),
        |wants, haves| {
            assert_eq!(wants, vec![want]);
            assert_eq!(haves, vec![known_have]);
            Ok(Some(b"PACKmock".to_vec()))
        },
    )
    .expect("test operation should succeed");

    assert_eq!(
        response.acknowledgments,
        vec![UploadPackAcknowledgment::Nak]
    );
    assert_eq!(response.packfile, b"PACKmock");
}

#[test]
fn upload_pack_raw_response_builder_rejects_missing_want_and_empty_pack() {
    let want = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");

    assert!(build_upload_pack_raw_packfile_response(
        &UploadPackFeatures::default(),
        UploadPackRequest {
            wants: vec![want],
            ..UploadPackRequest::default()
        },
        Vec::<ObjectId>::new(),
        |_| Ok(false),
        |_, _| Ok(Some(b"PACKmock".to_vec())),
    )
    .is_err());

    assert!(build_upload_pack_raw_packfile_response(
        &UploadPackFeatures::default(),
        UploadPackRequest {
            wants: vec![want],
            ..UploadPackRequest::default()
        },
        Vec::<ObjectId>::new(),
        |_| Ok(true),
        |_, _| Ok(None),
    )
    .is_err());
}

#[test]
fn upload_pack_request_parses_and_encodes_initial_fetch_request() {
    let want = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");
    let second_want = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "2222222222222222222222222222222222222222",
    )
    .expect("test operation should succeed");
    let shallow = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "3333333333333333333333333333333333333333",
    )
    .expect("test operation should succeed");
    let frames = vec![
        PktLineFrame::Data(
            b"want 1111111111111111111111111111111111111111 multi_ack thin-pack agent=git/2.54.0\n"
                .to_vec(),
        ),
        PktLineFrame::Data(b"want 2222222222222222222222222222222222222222\n".to_vec()),
        PktLineFrame::Data(b"shallow 3333333333333333333333333333333333333333\n".to_vec()),
        PktLineFrame::Data(b"deepen-since 1710000000\n".to_vec()),
        PktLineFrame::Data(b"deepen-not refs/tags/base\n".to_vec()),
        PktLineFrame::Data(b"filter blob:none\n".to_vec()),
        PktLineFrame::Flush,
    ];
    let request = parse_upload_pack_request(ObjectFormat::Sha1, &frames)
        .expect("test operation should succeed")
        .expect("test operation should succeed");
    assert_eq!(
        request,
        UploadPackRequest {
            wants: vec![want, second_want],
            capabilities: vec![
                Capability {
                    name: "multi_ack".into(),
                    value: None,
                },
                Capability {
                    name: "thin-pack".into(),
                    value: None,
                },
                Capability {
                    name: "agent".into(),
                    value: Some("git/2.54.0".into()),
                },
            ],
            shallow: vec![shallow],
            deepen: None,
            deepen_since: Some(1_710_000_000),
            deepen_not: vec!["refs/tags/base".into()],
            filter: Some("blob:none".into()),
        }
    );
    assert_eq!(
        encode_upload_pack_request(Some(&request)).expect("test operation should succeed"),
        frames
    );
    assert_eq!(
        parse_upload_pack_request(ObjectFormat::Sha1, &[PktLineFrame::Flush])
            .expect("test operation should succeed"),
        None
    );
    assert_eq!(
        encode_upload_pack_request(None).expect("test operation should succeed"),
        vec![PktLineFrame::Flush]
    );
}

#[test]
fn upload_pack_request_streams_round_trip() {
    let request = UploadPackRequest {
        wants: vec![ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed")],
        capabilities: vec![Capability {
            name: "ofs-delta".into(),
            value: None,
        }],
        deepen: Some(10),
        ..UploadPackRequest::default()
    };
    let mut encoded = Vec::new();
    write_upload_pack_request(&mut encoded, Some(&request)).expect("test operation should succeed");
    encoded.extend_from_slice(b"tail");

    let mut input = encoded.as_slice();
    assert_eq!(
        read_upload_pack_request(ObjectFormat::Sha1, &mut input)
            .expect("test operation should succeed"),
        Some(request)
    );
    assert_eq!(input, b"tail");
}

#[test]
fn upload_pack_request_rejects_malformed_requests() {
    assert!(parse_upload_pack_request(
        ObjectFormat::Sha1,
        &[PktLineFrame::Data(
            b"want 1111111111111111111111111111111111111111\n".to_vec(),
        )],
    )
    .is_err());
    assert!(parse_upload_pack_request(
        ObjectFormat::Sha1,
        &[
            PktLineFrame::Data(b"shallow 1111111111111111111111111111111111111111\n".to_vec(),),
            PktLineFrame::Flush,
        ],
    )
    .is_err());
    assert!(parse_upload_pack_request(
        ObjectFormat::Sha1,
        &[
            PktLineFrame::Data(
                b"want 1111111111111111111111111111111111111111 thin-pack\n".to_vec(),
            ),
            PktLineFrame::Data(
                b"want 2222222222222222222222222222222222222222 ofs-delta\n".to_vec(),
            ),
            PktLineFrame::Flush,
        ],
    )
    .is_err());
    assert!(parse_upload_pack_request(
        ObjectFormat::Sha1,
        &[
            PktLineFrame::Data(b"want 1111111111111111111111111111111111111111\n".to_vec(),),
            PktLineFrame::Data(b"deepen 1\n".to_vec()),
            PktLineFrame::Data(b"want 2222222222222222222222222222222222222222\n".to_vec()),
            PktLineFrame::Flush,
        ],
    )
    .is_err());
    assert!(parse_upload_pack_request(
        ObjectFormat::Sha1,
        &[
            PktLineFrame::Data(b"want 1111111111111111111111111111111111111111\n".to_vec(),),
            PktLineFrame::Data(b"filter blob:none\n".to_vec()),
            PktLineFrame::Data(b"filter tree:0\n".to_vec()),
            PktLineFrame::Flush,
        ],
    )
    .is_err());
    assert!(encode_upload_pack_request(Some(&UploadPackRequest::default())).is_err());
    assert!(encode_upload_pack_request(Some(&UploadPackRequest {
        wants: vec![ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed")],
        deepen: Some(0),
        ..UploadPackRequest::default()
    }))
    .is_err());
}

#[test]
fn upload_pack_shallow_update_parses_and_encodes_records() {
    let shallow = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");
    let unshallow = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "2222222222222222222222222222222222222222",
    )
    .expect("test operation should succeed");
    let frames = vec![
        PktLineFrame::Data(b"shallow 1111111111111111111111111111111111111111\n".to_vec()),
        PktLineFrame::Data(b"unshallow 2222222222222222222222222222222222222222\n".to_vec()),
        PktLineFrame::Flush,
    ];
    let entries = parse_upload_pack_shallow_update(ObjectFormat::Sha1, &frames)
        .expect("test operation should succeed");
    assert_eq!(
        entries,
        vec![
            ProtocolV2FetchShallowInfo::Shallow(shallow),
            ProtocolV2FetchShallowInfo::Unshallow(unshallow),
        ]
    );
    assert_eq!(
        encode_upload_pack_shallow_update(&entries).expect("test operation should succeed"),
        frames
    );
    assert_eq!(
        parse_upload_pack_shallow_update(ObjectFormat::Sha1, &[PktLineFrame::Flush])
            .expect("test operation should succeed"),
        Vec::<ProtocolV2FetchShallowInfo>::new()
    );
}

#[test]
fn upload_pack_shallow_update_streams_round_trip() {
    let entries = vec![ProtocolV2FetchShallowInfo::Shallow(
        ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed"),
    )];
    let mut encoded = Vec::new();
    write_upload_pack_shallow_update(&mut encoded, &entries)
        .expect("test operation should succeed");
    encoded.extend_from_slice(b"tail");

    let mut input = encoded.as_slice();
    assert_eq!(
        read_upload_pack_shallow_update(ObjectFormat::Sha1, &mut input)
            .expect("test operation should succeed"),
        entries
    );
    assert_eq!(input, b"tail");
}

#[test]
fn upload_pack_shallow_update_rejects_malformed_records() {
    assert!(parse_upload_pack_shallow_update(
        ObjectFormat::Sha1,
        &[PktLineFrame::Data(
            b"shallow 1111111111111111111111111111111111111111\n".to_vec(),
        )],
    )
    .is_err());
    assert!(parse_upload_pack_shallow_update(
        ObjectFormat::Sha1,
        &[PktLineFrame::Delimiter, PktLineFrame::Flush],
    )
    .is_err());
    assert!(parse_upload_pack_shallow_update(
        ObjectFormat::Sha1,
        &[
            PktLineFrame::Data(b"shallow 1111111111111111111111111111111111111111\n".to_vec(),),
            PktLineFrame::Flush,
            PktLineFrame::Data(b"unshallow 2222222222222222222222222222222222222222\n".to_vec(),),
        ],
    )
    .is_err());
    assert!(parse_upload_pack_shallow_update(
        ObjectFormat::Sha1,
        &[
            PktLineFrame::Data(b"unsupported 1111111111111111111111111111111111111111\n".to_vec(),),
            PktLineFrame::Flush,
        ],
    )
    .is_err());
}

#[test]
fn upload_pack_negotiation_request_parses_flush_and_done_rounds() {
    let have = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");
    let second_have = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "2222222222222222222222222222222222222222",
    )
    .expect("test operation should succeed");
    let flush_round = vec![
        PktLineFrame::Data(b"have 1111111111111111111111111111111111111111\n".to_vec()),
        PktLineFrame::Data(b"have 2222222222222222222222222222222222222222\n".to_vec()),
        PktLineFrame::Flush,
    ];
    let request = parse_upload_pack_negotiation_request(ObjectFormat::Sha1, &flush_round)
        .expect("test operation should succeed");
    assert_eq!(
        request,
        UploadPackNegotiationRequest {
            haves: vec![have, second_have],
            done: false,
        }
    );
    assert_eq!(
        encode_upload_pack_negotiation_request(&request).expect("test operation should succeed"),
        flush_round
    );

    let done_round = vec![
        PktLineFrame::Data(b"have 1111111111111111111111111111111111111111\n".to_vec()),
        PktLineFrame::Data(b"done\n".to_vec()),
    ];
    let request = parse_upload_pack_negotiation_request(ObjectFormat::Sha1, &done_round)
        .expect("test operation should succeed");
    assert_eq!(
        request,
        UploadPackNegotiationRequest {
            haves: vec![have],
            done: true,
        }
    );
    assert_eq!(
        encode_upload_pack_negotiation_request(&request).expect("test operation should succeed"),
        done_round
    );
}

#[test]
fn upload_pack_negotiation_request_streams_round_trip() {
    let first = UploadPackNegotiationRequest {
        haves: vec![ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed")],
        done: false,
    };
    let second = UploadPackNegotiationRequest {
        haves: Vec::new(),
        done: true,
    };
    let mut encoded = Vec::new();
    write_upload_pack_negotiation_request(&mut encoded, &first)
        .expect("test operation should succeed");
    write_upload_pack_negotiation_request(&mut encoded, &second)
        .expect("test operation should succeed");
    encoded.extend_from_slice(b"tail");

    let mut input = encoded.as_slice();
    assert_eq!(
        read_upload_pack_negotiation_request(ObjectFormat::Sha1, &mut input)
            .expect("test operation should succeed"),
        first
    );
    assert_eq!(
        read_upload_pack_negotiation_request(ObjectFormat::Sha1, &mut input)
            .expect("test operation should succeed"),
        second
    );
    assert_eq!(input, b"tail");
}

#[test]
fn upload_pack_negotiation_request_rejects_malformed_rounds() {
    assert!(parse_upload_pack_negotiation_request(
        ObjectFormat::Sha1,
        &[PktLineFrame::Data(
            b"have 1111111111111111111111111111111111111111\n".to_vec(),
        )],
    )
    .is_err());
    assert!(parse_upload_pack_negotiation_request(
        ObjectFormat::Sha1,
        &[PktLineFrame::Data(
            b"want 1111111111111111111111111111111111111111\n".to_vec(),
        )],
    )
    .is_err());
    assert!(parse_upload_pack_negotiation_request(
        ObjectFormat::Sha1,
        &[
            PktLineFrame::Data(b"done\n".to_vec()),
            PktLineFrame::Data(b"have 1111111111111111111111111111111111111111\n".to_vec(),),
        ],
    )
    .is_err());
    assert!(parse_upload_pack_negotiation_request(
        ObjectFormat::Sha1,
        &[PktLineFrame::Delimiter, PktLineFrame::Flush],
    )
    .is_err());
}

#[test]
fn upload_pack_acknowledgments_parse_and_encode_statuses() {
    let oid = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");
    assert_eq!(
        parse_upload_pack_acknowledgment(ObjectFormat::Sha1, b"NAK\n")
            .expect("test operation should succeed"),
        UploadPackAcknowledgment::Nak
    );
    for (payload, status) in [
        (
            b"ACK 1111111111111111111111111111111111111111\n".as_slice(),
            None,
        ),
        (
            b"ACK 1111111111111111111111111111111111111111 continue\n".as_slice(),
            Some(UploadPackAckStatus::Continue),
        ),
        (
            b"ACK 1111111111111111111111111111111111111111 common\n".as_slice(),
            Some(UploadPackAckStatus::Common),
        ),
        (
            b"ACK 1111111111111111111111111111111111111111 ready\n".as_slice(),
            Some(UploadPackAckStatus::Ready),
        ),
    ] {
        let acknowledgment = parse_upload_pack_acknowledgment(ObjectFormat::Sha1, payload)
            .expect("test operation should succeed");
        assert_eq!(
            acknowledgment,
            UploadPackAcknowledgment::Ack { oid, status }
        );
        assert_eq!(
            encode_upload_pack_acknowledgment(&acknowledgment)
                .expect("test operation should succeed"),
            payload
        );
    }
}

#[test]
fn upload_pack_acknowledgments_stream_round_trip_and_reject_bad_lines() {
    let acknowledgment = UploadPackAcknowledgment::Ack {
        oid: ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed"),
        status: Some(UploadPackAckStatus::Ready),
    };
    let mut encoded = Vec::new();
    write_upload_pack_acknowledgment(&mut encoded, &acknowledgment)
        .expect("test operation should succeed");
    encoded.extend_from_slice(b"tail");

    let mut input = encoded.as_slice();
    assert_eq!(
        read_upload_pack_acknowledgment(ObjectFormat::Sha1, &mut input)
            .expect("test operation should succeed"),
        acknowledgment
    );
    assert_eq!(input, b"tail");
    assert!(parse_upload_pack_acknowledgment(ObjectFormat::Sha1, b"ACK not-an-oid\n").is_err());
    assert!(parse_upload_pack_acknowledgment(
        ObjectFormat::Sha1,
        b"ACK 1111111111111111111111111111111111111111 unknown\n",
    )
    .is_err());
    assert!(parse_upload_pack_acknowledgment(
        ObjectFormat::Sha1,
        b"ACK 1111111111111111111111111111111111111111 ready extra\n",
    )
    .is_err());
    assert!(parse_upload_pack_acknowledgment(ObjectFormat::Sha1, b"ERR remote died\n").is_err());
    assert!(read_upload_pack_acknowledgment(ObjectFormat::Sha1, &mut &b"0000"[..]).is_err());
}

#[test]
fn upload_pack_packfile_response_parses_acknowledgments_and_sideband() {
    let oid = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");
    let frames = vec![
        PktLineFrame::Data(b"ACK 1111111111111111111111111111111111111111 common\n".to_vec()),
        PktLineFrame::Data(b"NAK\n".to_vec()),
        PktLineFrame::Data(b"\x01PACK".to_vec()),
        PktLineFrame::Data(b"\x02counting objects\n".to_vec()),
        PktLineFrame::Data(b"\x01 bytes".to_vec()),
        PktLineFrame::Flush,
    ];
    let response = parse_upload_pack_packfile_response(ObjectFormat::Sha1, &frames)
        .expect("test operation should succeed");
    assert_eq!(
        response,
        UploadPackPackfileResponse {
            acknowledgments: vec![
                UploadPackAcknowledgment::Ack {
                    oid,
                    status: Some(UploadPackAckStatus::Common),
                },
                UploadPackAcknowledgment::Nak,
            ],
            sideband: vec![
                SideBandPacket {
                    channel: SideBandChannel::Data,
                    data: b"PACK".to_vec(),
                },
                SideBandPacket {
                    channel: SideBandChannel::Progress,
                    data: b"counting objects\n".to_vec(),
                },
                SideBandPacket {
                    channel: SideBandChannel::Data,
                    data: b" bytes".to_vec(),
                },
            ],
        }
    );
    assert_eq!(
        demux_upload_pack_packfile_response(&response).expect("test operation should succeed"),
        SideBandDemux {
            data: b"PACK bytes".to_vec(),
            progress: vec![b"counting objects\n".to_vec()],
        }
    );
    assert_eq!(
        encode_upload_pack_packfile_response(&response).expect("test operation should succeed"),
        frames
    );
}

#[test]
fn upload_pack_packfile_response_streams_round_trip() {
    let response = UploadPackPackfileResponse {
        acknowledgments: vec![UploadPackAcknowledgment::Nak],
        sideband: vec![SideBandPacket {
            channel: SideBandChannel::Data,
            data: b"PACK".to_vec(),
        }],
    };
    let mut encoded = Vec::new();
    write_upload_pack_packfile_response(&mut encoded, &response)
        .expect("test operation should succeed");
    encoded.extend_from_slice(b"tail");

    let mut input = encoded.as_slice();
    assert_eq!(
        read_upload_pack_packfile_response(ObjectFormat::Sha1, &mut input)
            .expect("test operation should succeed"),
        response
    );
    assert_eq!(input, b"tail");
}

#[test]
fn upload_pack_packfile_response_rejects_malformed_streams() {
    assert!(parse_upload_pack_packfile_response(
        ObjectFormat::Sha1,
        &[PktLineFrame::Data(b"NAK\n".to_vec())],
    )
    .is_err());
    assert!(parse_upload_pack_packfile_response(
        ObjectFormat::Sha1,
        &[PktLineFrame::Delimiter, PktLineFrame::Flush],
    )
    .is_err());
    assert!(parse_upload_pack_packfile_response(
        ObjectFormat::Sha1,
        &[
            PktLineFrame::Data(b"\x01PACK".to_vec()),
            PktLineFrame::Data(b"ACK 1111111111111111111111111111111111111111 common\n".to_vec()),
            PktLineFrame::Flush,
        ],
    )
    .is_err());
    assert!(parse_upload_pack_packfile_response(
        ObjectFormat::Sha1,
        &[
            PktLineFrame::Data(b"NAK\n".to_vec()),
            PktLineFrame::Flush,
            PktLineFrame::Data(b"\x01PACK".to_vec()),
        ],
    )
    .is_err());
    assert!(parse_upload_pack_packfile_response(
        ObjectFormat::Sha1,
        &[
            PktLineFrame::Data(b"NAK\n".to_vec()),
            PktLineFrame::Data(b"\x04bad".to_vec()),
            PktLineFrame::Flush,
        ],
    )
    .is_err());
}

#[test]
fn upload_pack_raw_packfile_response_parses_acknowledgments_and_raw_pack() {
    let oid = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");
    let response = UploadPackRawPackfileResponse {
        acknowledgments: vec![
            UploadPackAcknowledgment::Ack {
                oid,
                status: Some(UploadPackAckStatus::Common),
            },
            UploadPackAcknowledgment::Nak,
        ],
        packfile: b"PACK\x00\x00\x00\x02raw-bytes".to_vec(),
    };
    let encoded =
        encode_upload_pack_raw_packfile_response(&response).expect("test operation should succeed");
    assert_eq!(
        parse_upload_pack_raw_packfile_response(ObjectFormat::Sha1, &encoded)
            .expect("test operation should succeed"),
        response
    );
}

#[test]
fn upload_pack_raw_packfile_response_streams_round_trip() {
    let response = UploadPackRawPackfileResponse {
        acknowledgments: vec![UploadPackAcknowledgment::Nak],
        packfile: b"PACK\x00\x00\x00\x02raw-bytes".to_vec(),
    };
    let mut encoded = Vec::new();
    write_upload_pack_raw_packfile_response(&mut encoded, &response)
        .expect("test operation should succeed");
    assert_eq!(
        encoded,
        encode_upload_pack_raw_packfile_response(&response).expect("test operation should succeed")
    );

    let mut input = encoded.as_slice();
    assert_eq!(
        read_upload_pack_raw_packfile_response(ObjectFormat::Sha1, &mut input)
            .expect("test operation should succeed"),
        response
    );
    assert!(input.is_empty());
}

#[test]
fn upload_pack_raw_packfile_response_rejects_malformed_streams() {
    let ack = PktLineFrame::data(b"NAK\n".to_vec())
        .expect("test operation should succeed")
        .try_encode()
        .expect("test operation should succeed");
    let bad_ack = PktLineFrame::data(b"ACK not-an-oid\n".to_vec())
        .expect("test operation should succeed")
        .try_encode()
        .expect("test operation should succeed");
    let non_ack = PktLineFrame::data(b"have 1111111111111111111111111111111111111111\n".to_vec())
        .expect("test operation should succeed")
        .try_encode()
        .expect("test operation should succeed");
    let mut garbage_after_ack = ack.clone();
    garbage_after_ack.extend_from_slice(b"garbage");

    assert!(parse_upload_pack_raw_packfile_response(ObjectFormat::Sha1, b"").is_err());
    assert!(parse_upload_pack_raw_packfile_response(ObjectFormat::Sha1, &ack).is_err());
    assert!(parse_upload_pack_raw_packfile_response(ObjectFormat::Sha1, &bad_ack).is_err());
    assert!(parse_upload_pack_raw_packfile_response(ObjectFormat::Sha1, b"0000PACK").is_err());
    assert!(parse_upload_pack_raw_packfile_response(ObjectFormat::Sha1, &non_ack).is_err());
    assert!(
        parse_upload_pack_raw_packfile_response(ObjectFormat::Sha1, &garbage_after_ack).is_err()
    );
    assert!(
        encode_upload_pack_raw_packfile_response(&UploadPackRawPackfileResponse {
            acknowledgments: vec![UploadPackAcknowledgment::Nak],
            packfile: Vec::new(),
        })
        .is_err()
    );
    assert!(
        encode_upload_pack_raw_packfile_response(&UploadPackRawPackfileResponse {
            acknowledgments: Vec::new(),
            packfile: b"not-a-pack".to_vec(),
        })
        .is_err()
    );
}

#[test]
fn upload_pack_request_encodes_deepen_request() {
    // A `--depth 1` clone over smart-HTTP v1: the `want` line carries the
    // capabilities, the client's existing shallow boundary is replayed as a
    // `shallow` line, and `deepen 1` requests the truncation. Built as raw
    // pkt-line bytes so the 4-hex length prefixes are exercised.
    let want = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");
    let boundary = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "2222222222222222222222222222222222222222",
    )
    .expect("test operation should succeed");
    let request = UploadPackRequest {
        wants: vec![want],
        capabilities: vec![Capability {
            name: "shallow".into(),
            value: None,
        }],
        shallow: vec![boundary],
        deepen: Some(1),
        ..UploadPackRequest::default()
    };
    let mut encoded = Vec::new();
    write_upload_pack_request(&mut encoded, Some(&request)).expect("test operation should succeed");
    let mut expected = Vec::new();
    expected.extend_from_slice(b"003awant 1111111111111111111111111111111111111111 shallow\n");
    expected.extend_from_slice(b"0035shallow 2222222222222222222222222222222222222222\n");
    expected.extend_from_slice(b"000ddeepen 1\n");
    expected.extend_from_slice(b"0000");
    assert_eq!(encoded, expected);
}

#[test]
fn upload_pack_shallow_info_response_parses_shallow_unshallow_and_pack() {
    // The smart-HTTP v1 deepen response: a shallow-info section (one
    // `shallow` and one `unshallow` line) terminated by a flush, then the
    // NAK and the raw packfile. Hand-built pkt-lines (mind the lengths).
    let shallow = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");
    let unshallow = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "2222222222222222222222222222222222222222",
    )
    .expect("test operation should succeed");
    let mut input = Vec::new();
    input.extend_from_slice(b"0035shallow 1111111111111111111111111111111111111111\n");
    input.extend_from_slice(b"0037unshallow 2222222222222222222222222222222222222222\n");
    input.extend_from_slice(b"0000"); // shallow-info terminator
    input.extend_from_slice(b"0008NAK\n");
    input.extend_from_slice(b"PACK\x00\x00\x00\x02raw-bytes");

    let (entries, response) =
        parse_upload_pack_shallow_info_and_raw_packfile_response(ObjectFormat::Sha1, &input)
            .expect("test operation should succeed");
    assert_eq!(
        entries,
        vec![
            ProtocolV2FetchShallowInfo::Shallow(shallow),
            ProtocolV2FetchShallowInfo::Unshallow(unshallow),
        ]
    );
    assert_eq!(
        response,
        UploadPackRawPackfileResponse {
            acknowledgments: vec![UploadPackAcknowledgment::Nak],
            packfile: b"PACK\x00\x00\x00\x02raw-bytes".to_vec(),
        }
    );

    // The reader entry point yields the same result over a stream.
    let mut stream = input.as_slice();
    let (read_entries, read_response) =
        read_upload_pack_shallow_info_and_raw_packfile_response(ObjectFormat::Sha1, &mut stream)
            .expect("test operation should succeed");
    assert_eq!(read_entries, entries);
    assert_eq!(read_response, response);
}

#[test]
fn upload_pack_shallow_info_response_handles_empty_shallow_section() {
    // A deepen request that creates no boundary change still gets an empty
    // shallow-info section (a bare flush) before the NAK + pack.
    let mut input = Vec::new();
    input.extend_from_slice(b"0000"); // empty shallow-info
    input.extend_from_slice(b"0008NAK\n");
    input.extend_from_slice(b"PACK\x00\x00\x00\x02raw-bytes");

    let (entries, response) =
        parse_upload_pack_shallow_info_and_raw_packfile_response(ObjectFormat::Sha1, &input)
            .expect("test operation should succeed");
    assert!(entries.is_empty());
    assert_eq!(
        response.acknowledgments,
        vec![UploadPackAcknowledgment::Nak]
    );
    assert!(response.packfile.starts_with(b"PACK"));
}

#[test]
fn upload_pack_shallow_info_response_rejects_malformed_sections() {
    // Truncated section (no terminating flush before EOF).
    let truncated = b"0035shallow 1111111111111111111111111111111111111111\n".to_vec();
    assert!(parse_upload_pack_shallow_info_and_raw_packfile_response(
        ObjectFormat::Sha1,
        &truncated
    )
    .is_err());
    // A non-flush control packet inside the shallow-info section.
    let mut delimiter_section = Vec::new();
    delimiter_section.extend_from_slice(b"0001"); // delimiter, not a flush
    assert!(
        parse_upload_pack_shallow_info_section(ObjectFormat::Sha1, &delimiter_section).is_err()
    );
    // A non-shallow data line inside the section.
    let mut bad_line = Vec::new();
    bad_line.extend_from_slice(b"0008NAK\n");
    assert!(parse_upload_pack_shallow_info_section(ObjectFormat::Sha1, &bad_line).is_err());
    // Valid shallow-info but a missing packfile afterwards.
    let mut no_pack = Vec::new();
    no_pack.extend_from_slice(b"0000"); // empty shallow-info
    no_pack.extend_from_slice(b"0008NAK\n");
    assert!(
        parse_upload_pack_shallow_info_and_raw_packfile_response(ObjectFormat::Sha1, &no_pack)
            .is_err()
    );
}

#[test]
fn receive_pack_request_parses_and_encodes_commands() {
    let old_id = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");
    let new_id = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "2222222222222222222222222222222222222222",
    )
    .expect("test operation should succeed");
    let delete_old_id = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "3333333333333333333333333333333333333333",
    )
    .expect("test operation should succeed");
    let zero = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "0000000000000000000000000000000000000000",
    )
    .expect("test operation should succeed");
    let shallow = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "4444444444444444444444444444444444444444",
    )
    .expect("test operation should succeed");
    let frames = vec![
            PktLineFrame::Data(b"shallow 4444444444444444444444444444444444444444\n".to_vec()),
            PktLineFrame::Data(
                b"1111111111111111111111111111111111111111 2222222222222222222222222222222222222222 refs/heads/main\0report-status side-band-64k agent=git/2.54.0\n"
                    .to_vec(),
            ),
            PktLineFrame::Data(
                b"3333333333333333333333333333333333333333 0000000000000000000000000000000000000000 refs/heads/old\n"
                    .to_vec(),
            ),
            PktLineFrame::Flush,
        ];
    let request = parse_receive_pack_request(ObjectFormat::Sha1, &frames)
        .expect("test operation should succeed");
    assert_eq!(
        request,
        ReceivePackRequest {
            shallow: vec![shallow],
            commands: vec![
                ReceivePackCommand {
                    old_id,
                    new_id,
                    name: "refs/heads/main".into(),
                },
                ReceivePackCommand {
                    old_id: delete_old_id,
                    new_id: zero,
                    name: "refs/heads/old".into(),
                },
            ],
            capabilities: vec![
                Capability {
                    name: "report-status".into(),
                    value: None,
                },
                Capability {
                    name: "side-band-64k".into(),
                    value: None,
                },
                Capability {
                    name: "agent".into(),
                    value: Some("git/2.54.0".into()),
                },
            ],
        }
    );
    assert_eq!(
        encode_receive_pack_request(&request).expect("test operation should succeed"),
        frames
    );
    assert_eq!(
        parse_receive_pack_request(ObjectFormat::Sha1, &[PktLineFrame::Flush])
            .expect("test operation should succeed"),
        ReceivePackRequest::default()
    );
}

#[test]
fn receive_pack_request_streams_round_trip() {
    let request = ReceivePackRequest {
        commands: vec![ReceivePackCommand {
            old_id: ObjectId::from_hex(
                ObjectFormat::Sha1,
                "0000000000000000000000000000000000000000",
            )
            .expect("test operation should succeed"),
            new_id: ObjectId::from_hex(
                ObjectFormat::Sha1,
                "1111111111111111111111111111111111111111",
            )
            .expect("test operation should succeed"),
            name: "refs/heads/main".into(),
        }],
        capabilities: vec![Capability {
            name: "report-status".into(),
            value: None,
        }],
        ..ReceivePackRequest::default()
    };
    let mut encoded = Vec::new();
    write_receive_pack_request(&mut encoded, &request).expect("test operation should succeed");
    encoded.extend_from_slice(b"PACK");

    let mut input = encoded.as_slice();
    assert_eq!(
        read_receive_pack_request(ObjectFormat::Sha1, &mut input)
            .expect("test operation should succeed"),
        request
    );
    assert_eq!(input, b"PACK");
}

#[test]
fn receive_pack_request_rejects_malformed_commands() {
    assert!(
            parse_receive_pack_request(
                ObjectFormat::Sha1,
                &[PktLineFrame::Data(
                    b"1111111111111111111111111111111111111111 2222222222222222222222222222222222222222 refs/heads/main\n"
                        .to_vec(),
                )],
            )
            .is_err()
        );
    assert!(
            parse_receive_pack_request(
                ObjectFormat::Sha1,
                &[
                    PktLineFrame::Data(
                        b"1111111111111111111111111111111111111111 2222222222222222222222222222222222222222 refs/heads/main\n"
                            .to_vec(),
                    ),
                    PktLineFrame::Data(
                        b"shallow 3333333333333333333333333333333333333333\n".to_vec(),
                    ),
                    PktLineFrame::Flush,
                ],
            )
            .is_err()
        );
    assert!(
            parse_receive_pack_request(
                ObjectFormat::Sha1,
                &[
                    PktLineFrame::Data(
                        b"1111111111111111111111111111111111111111 2222222222222222222222222222222222222222 refs/heads/main\0report-status\n"
                            .to_vec(),
                    ),
                    PktLineFrame::Data(
                        b"3333333333333333333333333333333333333333 4444444444444444444444444444444444444444 refs/heads/next\0side-band-64k\n"
                            .to_vec(),
                    ),
                    PktLineFrame::Flush,
                ],
            )
            .is_err()
        );
    assert!(parse_receive_pack_request(
        ObjectFormat::Sha1,
        &[
            PktLineFrame::Data(
                b"1111111111111111111111111111111111111111 refs/heads/main\n".to_vec(),
            ),
            PktLineFrame::Flush,
        ],
    )
    .is_err());
    assert!(encode_receive_pack_request(&ReceivePackRequest {
        shallow: vec![ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed")],
        ..ReceivePackRequest::default()
    })
    .is_err());
    assert!(encode_receive_pack_request(&ReceivePackRequest {
        commands: vec![ReceivePackCommand {
            old_id: ObjectId::from_hex(
                ObjectFormat::Sha1,
                "1111111111111111111111111111111111111111",
            )
            .expect("test operation should succeed"),
            new_id: ObjectId::from_hex(
                ObjectFormat::Sha1,
                "2222222222222222222222222222222222222222",
            )
            .expect("test operation should succeed"),
            name: "bad ref".into(),
        }],
        ..ReceivePackRequest::default()
    })
    .is_err());
}

#[test]
fn receive_pack_features_parse_encode_and_validate_push_request() {
    let capabilities = vec![
        Capability {
            name: "report-status".into(),
            value: None,
        },
        Capability {
            name: "report-status-v2".into(),
            value: None,
        },
        Capability {
            name: "delete-refs".into(),
            value: None,
        },
        Capability {
            name: "ofs-delta".into(),
            value: None,
        },
        Capability {
            name: "atomic".into(),
            value: None,
        },
        Capability {
            name: "push-options".into(),
            value: None,
        },
        Capability {
            name: "side-band-64k".into(),
            value: None,
        },
        Capability {
            name: "quiet".into(),
            value: None,
        },
        Capability {
            name: "no-thin".into(),
            value: None,
        },
        Capability {
            name: "agent".into(),
            value: Some("git/2.54.0".into()),
        },
        Capability {
            name: "object-format".into(),
            value: Some("sha256".into()),
        },
        Capability {
            name: "custom".into(),
            value: Some("value".into()),
        },
    ];
    let features =
        parse_receive_pack_features(&capabilities).expect("test operation should succeed");
    assert_eq!(
        features,
        ReceivePackFeatures {
            report_status: true,
            report_status_v2: true,
            delete_refs: true,
            ofs_delta: true,
            atomic: true,
            push_options: true,
            side_band_64k: true,
            quiet: true,
            no_thin: true,
            agent: Some("git/2.54.0".into()),
            object_format: Some(ObjectFormat::Sha256),
            unknown: vec![Capability {
                name: "custom".into(),
                value: Some("value".into()),
            }],
        }
    );
    assert_eq!(
        encode_receive_pack_features(&features).expect("test operation should succeed"),
        capabilities
    );

    let request = ReceivePackPushRequest {
        commands: ReceivePackRequest {
            commands: vec![ReceivePackCommand {
                old_id: ObjectId::from_hex(
                    ObjectFormat::Sha1,
                    "1111111111111111111111111111111111111111",
                )
                .expect("test operation should succeed"),
                new_id: ObjectId::from_hex(
                    ObjectFormat::Sha1,
                    "2222222222222222222222222222222222222222",
                )
                .expect("test operation should succeed"),
                name: "refs/heads/main".into(),
            }],
            capabilities: vec![
                Capability {
                    name: "report-status".into(),
                    value: None,
                },
                Capability {
                    name: "ofs-delta".into(),
                    value: None,
                },
                Capability {
                    name: "push-options".into(),
                    value: None,
                },
                Capability {
                    name: "side-band-64k".into(),
                    value: None,
                },
                Capability {
                    name: "agent".into(),
                    value: Some("sley".into()),
                },
            ],
            ..ReceivePackRequest::default()
        },
        push_options: Some(vec!["ci.skip".into()]),
        packfile: b"PACKpayload".to_vec(),
    };
    validate_receive_pack_push_request_features(&features, &request)
        .expect("test operation should succeed");
}

#[test]
fn receive_pack_features_reject_invalid_push_requests() {
    let old_id = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");
    let new_id = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "2222222222222222222222222222222222222222",
    )
    .expect("test operation should succeed");
    let zero = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "0000000000000000000000000000000000000000",
    )
    .expect("test operation should succeed");
    let features = ReceivePackFeatures {
        report_status: true,
        push_options: true,
        ..ReceivePackFeatures::default()
    };
    let update = ReceivePackCommand {
        old_id: old_id.clone(),
        new_id: new_id.clone(),
        name: "refs/heads/main".into(),
    };

    assert!(validate_receive_pack_push_request_features(
        &features,
        &ReceivePackPushRequest {
            commands: ReceivePackRequest {
                commands: vec![update.clone()],
                capabilities: vec![Capability {
                    name: "push-options".into(),
                    value: None,
                }],
                ..ReceivePackRequest::default()
            },
            push_options: None,
            packfile: b"PACKpayload".to_vec(),
        },
    )
    .is_err());
    assert!(validate_receive_pack_push_request_features(
        &features,
        &ReceivePackPushRequest {
            commands: ReceivePackRequest {
                commands: vec![update.clone()],
                ..ReceivePackRequest::default()
            },
            push_options: Some(Vec::new()),
            packfile: b"PACKpayload".to_vec(),
        },
    )
    .is_err());
    assert!(validate_receive_pack_push_request_features(
        &features,
        &ReceivePackPushRequest {
            commands: ReceivePackRequest {
                commands: vec![ReceivePackCommand {
                    old_id: old_id.clone(),
                    new_id: zero.clone(),
                    name: "refs/heads/main".into(),
                }],
                ..ReceivePackRequest::default()
            },
            push_options: None,
            packfile: Vec::new(),
        },
    )
    .is_err());
    validate_receive_pack_push_request_features(
        &features,
        &ReceivePackPushRequest {
            commands: ReceivePackRequest {
                commands: vec![update.clone()],
                ..ReceivePackRequest::default()
            },
            push_options: None,
            packfile: Vec::new(),
        },
    )
    .expect("updates to already-present objects may omit a packfile");
    assert!(validate_receive_pack_push_request_features(
        &ReceivePackFeatures {
            delete_refs: true,
            ..ReceivePackFeatures::default()
        },
        &ReceivePackPushRequest {
            commands: ReceivePackRequest {
                commands: vec![ReceivePackCommand {
                    old_id,
                    new_id: zero,
                    name: "refs/heads/main".into(),
                }],
                ..ReceivePackRequest::default()
            },
            push_options: None,
            packfile: b"PACKpayload".to_vec(),
        },
    )
    .is_err());
    assert!(validate_receive_pack_push_request_features(
        &features,
        &ReceivePackPushRequest {
            commands: ReceivePackRequest {
                commands: vec![update],
                capabilities: vec![Capability {
                    name: "atomic".into(),
                    value: None,
                }],
                ..ReceivePackRequest::default()
            },
            push_options: None,
            packfile: b"PACKpayload".to_vec(),
        },
    )
    .is_err());

    assert!(parse_receive_pack_features(&[
        Capability {
            name: "push-options".into(),
            value: None,
        },
        Capability {
            name: "push-options".into(),
            value: None,
        },
    ])
    .is_err());
    assert!(encode_receive_pack_features(&ReceivePackFeatures {
        unknown: vec![Capability {
            name: "atomic".into(),
            value: None,
        }],
        ..ReceivePackFeatures::default()
    })
    .is_err());
}

#[test]
fn receive_pack_apply_helper_installs_pack_verifies_objects_and_reports_ok() {
    let old_id = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");
    let new_id = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "2222222222222222222222222222222222222222",
    )
    .expect("test operation should succeed");
    let request = ReceivePackPushRequest {
        commands: ReceivePackRequest {
            commands: vec![ReceivePackCommand {
                old_id: old_id.clone(),
                new_id: new_id.clone(),
                name: "refs/heads/main".into(),
            }],
            ..ReceivePackRequest::default()
        },
        packfile: b"PACKpayload".to_vec(),
        ..ReceivePackPushRequest::default()
    };
    let installed = std::cell::Cell::new(false);
    let applied = std::cell::RefCell::new(Vec::new());

    let report = apply_receive_pack_push_request(
        &ReceivePackFeatures::default(),
        &request,
        |_| unreachable!("update stale-old checks belong to the ref transaction callback"),
        |packfile| {
            assert_eq!(packfile, b"PACKpayload");
            installed.set(true);
            Ok(())
        },
        |oid| Ok(oid == &new_id),
        |commands| {
            applied.borrow_mut().extend_from_slice(commands);
            Ok(())
        },
        |_| unreachable!("no delete command should be applied"),
    )
    .expect("test operation should succeed");

    assert!(installed.get());
    assert_eq!(applied.into_inner(), request.commands.commands);
    assert_eq!(report.unpack, ReceivePackUnpackStatus::Ok);
    assert_eq!(
        report.commands,
        vec![ReceivePackCommandStatus::Ok {
            name: "refs/heads/main".into(),
        }]
    );
}

#[test]
fn receive_pack_apply_helper_allows_update_without_pack_when_object_exists() {
    let old_id = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");
    let new_id = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "2222222222222222222222222222222222222222",
    )
    .expect("test operation should succeed");
    let request = ReceivePackPushRequest {
        commands: ReceivePackRequest {
            commands: vec![ReceivePackCommand {
                old_id: old_id.clone(),
                new_id: new_id.clone(),
                name: "refs/heads/main".into(),
            }],
            ..ReceivePackRequest::default()
        },
        ..ReceivePackPushRequest::default()
    };
    let installed = std::cell::Cell::new(false);
    let applied = std::cell::RefCell::new(Vec::new());

    let report = apply_receive_pack_push_request(
        &ReceivePackFeatures::default(),
        &request,
        |_| unreachable!("update stale-old checks belong to the ref transaction callback"),
        |_| {
            installed.set(true);
            Ok(())
        },
        |oid| Ok(oid == &new_id),
        |commands| {
            applied.borrow_mut().extend_from_slice(commands);
            Ok(())
        },
        |_| unreachable!("no delete command should be applied"),
    )
    .expect("test operation should succeed");

    assert!(!installed.get());
    assert_eq!(applied.into_inner(), request.commands.commands);
    assert_eq!(report.unpack, ReceivePackUnpackStatus::Ok);
}

#[test]
fn receive_pack_apply_helper_preserves_delete_only_and_stale_delete_rules() {
    let old_id = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");
    let other_id = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "2222222222222222222222222222222222222222",
    )
    .expect("test operation should succeed");
    let zero = zero_object_id(ObjectFormat::Sha1).expect("test operation should succeed");
    let request = ReceivePackPushRequest {
        commands: ReceivePackRequest {
            commands: vec![ReceivePackCommand {
                old_id: old_id.clone(),
                new_id: zero,
                name: "refs/heads/main".into(),
            }],
            ..ReceivePackRequest::default()
        },
        ..ReceivePackPushRequest::default()
    };
    let features = ReceivePackFeatures {
        delete_refs: true,
        ..ReceivePackFeatures::default()
    };
    let installed = std::cell::Cell::new(false);
    let deleted = std::cell::RefCell::new(Vec::new());

    let report = apply_receive_pack_push_request(
        &features,
        &request,
        |_| Ok(Some(old_id.clone())),
        |_| {
            installed.set(true);
            Ok(())
        },
        |_| Ok(false),
        |_| unreachable!("delete-only request should not apply updates"),
        |command| {
            deleted.borrow_mut().push(command.name.clone());
            Ok(())
        },
    )
    .expect("test operation should succeed");

    assert!(!installed.get());
    assert_eq!(deleted.into_inner(), vec!["refs/heads/main"]);
    assert_eq!(report.unpack, ReceivePackUnpackStatus::Ok);
    assert!(apply_receive_pack_push_request(
        &features,
        &request,
        |_| Ok(Some(other_id.clone())),
        |_| Ok(()),
        |_| Ok(false),
        |_| Ok(()),
        |_| Ok(()),
    )
    .is_err());
}

#[test]
fn receive_pack_push_request_parses_commands_options_and_packfile() {
    let command = ReceivePackCommand {
        old_id: ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed"),
        new_id: ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .expect("test operation should succeed"),
        name: "refs/heads/main".into(),
    };
    let expected = ReceivePackPushRequest {
        commands: ReceivePackRequest {
            commands: vec![command],
            capabilities: vec![
                Capability {
                    name: "report-status".into(),
                    value: None,
                },
                Capability {
                    name: "push-options".into(),
                    value: None,
                },
            ],
            ..ReceivePackRequest::default()
        },
        push_options: Some(vec!["ci.skip".into(), "deploy=staging".into()]),
        packfile: b"PACK\x00\x00\x00\x02payload".to_vec(),
    };
    let encoded =
        encode_receive_pack_push_request(&expected).expect("test operation should succeed");

    assert_eq!(
        parse_receive_pack_push_request(ObjectFormat::Sha1, &encoded, true)
            .expect("test operation should succeed"),
        expected
    );
}

#[test]
fn receive_pack_push_request_preserves_packfile_without_push_options() {
    let request = ReceivePackPushRequest {
        commands: ReceivePackRequest {
            commands: vec![ReceivePackCommand {
                old_id: ObjectId::from_hex(
                    ObjectFormat::Sha1,
                    "1111111111111111111111111111111111111111",
                )
                .expect("test operation should succeed"),
                new_id: ObjectId::from_hex(
                    ObjectFormat::Sha1,
                    "2222222222222222222222222222222222222222",
                )
                .expect("test operation should succeed"),
                name: "refs/heads/main".into(),
            }],
            ..ReceivePackRequest::default()
        },
        push_options: None,
        packfile: b"0000PACK-like bytes stay raw".to_vec(),
    };
    let encoded =
        encode_receive_pack_push_request(&request).expect("test operation should succeed");

    assert_eq!(
        parse_receive_pack_push_request(ObjectFormat::Sha1, &encoded, false)
            .expect("test operation should succeed"),
        request
    );
}

#[test]
fn receive_pack_push_request_streams_round_trip() {
    let request = ReceivePackPushRequest {
        commands: ReceivePackRequest {
            commands: vec![ReceivePackCommand {
                old_id: ObjectId::from_hex(
                    ObjectFormat::Sha1,
                    "1111111111111111111111111111111111111111",
                )
                .expect("test operation should succeed"),
                new_id: ObjectId::from_hex(
                    ObjectFormat::Sha1,
                    "2222222222222222222222222222222222222222",
                )
                .expect("test operation should succeed"),
                name: "refs/heads/main".into(),
            }],
            capabilities: vec![Capability {
                name: "push-options".into(),
                value: None,
            }],
            ..ReceivePackRequest::default()
        },
        push_options: Some(Vec::new()),
        packfile: b"PACKpayload".to_vec(),
    };
    let mut encoded = Vec::new();
    write_receive_pack_push_request(&mut encoded, &request).expect("test operation should succeed");

    assert_eq!(
        read_receive_pack_push_request(ObjectFormat::Sha1, &mut encoded.as_slice(), true)
            .expect("test operation should succeed"),
        request
    );
}

#[test]
fn receive_pack_push_request_rejects_malformed_sections() {
    assert!(parse_receive_pack_push_request(
        ObjectFormat::Sha1,
        b"0014not-a-command\n0000PACK",
        false,
    )
    .is_err());

    let request = ReceivePackPushRequest {
        commands: ReceivePackRequest {
            commands: vec![ReceivePackCommand {
                old_id: ObjectId::from_hex(
                    ObjectFormat::Sha1,
                    "1111111111111111111111111111111111111111",
                )
                .expect("test operation should succeed"),
                new_id: ObjectId::from_hex(
                    ObjectFormat::Sha1,
                    "2222222222222222222222222222222222222222",
                )
                .expect("test operation should succeed"),
                name: "refs/heads/main".into(),
            }],
            ..ReceivePackRequest::default()
        },
        push_options: None,
        packfile: b"PACKpayload".to_vec(),
    };
    let encoded =
        encode_receive_pack_push_request(&request).expect("test operation should succeed");
    assert!(parse_receive_pack_push_request(ObjectFormat::Sha1, &encoded, true).is_err());

    assert!(encode_receive_pack_push_request(&ReceivePackPushRequest {
        commands: ReceivePackRequest {
            shallow: vec![ObjectId::from_hex(
                ObjectFormat::Sha1,
                "1111111111111111111111111111111111111111",
            )
            .expect("test operation should succeed")],
            ..ReceivePackRequest::default()
        },
        push_options: None,
        packfile: Vec::new(),
    })
    .is_err());
}

#[test]
fn receive_pack_report_status_parses_and_encodes_status_lines() {
    let frames = vec![
        PktLineFrame::Data(b"unpack ok\n".to_vec()),
        PktLineFrame::Data(b"ok refs/heads/main\n".to_vec()),
        PktLineFrame::Data(b"ng refs/heads/old non-fast-forward\n".to_vec()),
        PktLineFrame::Flush,
    ];
    let report = parse_receive_pack_report_status(&frames).expect("test operation should succeed");
    assert_eq!(
        report,
        ReceivePackReportStatus {
            unpack: ReceivePackUnpackStatus::Ok,
            commands: vec![
                ReceivePackCommandStatus::Ok {
                    name: "refs/heads/main".into(),
                },
                ReceivePackCommandStatus::Ng {
                    name: "refs/heads/old".into(),
                    message: "non-fast-forward".into(),
                },
            ],
        }
    );
    assert_eq!(
        encode_receive_pack_report_status(&report).expect("test operation should succeed"),
        frames
    );

    let frames = vec![
        PktLineFrame::Data(b"unpack pack exceeds maximum size\n".to_vec()),
        PktLineFrame::Flush,
    ];
    assert_eq!(
        parse_receive_pack_report_status(&frames).expect("test operation should succeed"),
        ReceivePackReportStatus {
            unpack: ReceivePackUnpackStatus::Error("pack exceeds maximum size".into()),
            commands: Vec::new(),
        }
    );
}

#[test]
fn receive_pack_report_status_streams_round_trip() {
    let report = ReceivePackReportStatus {
        unpack: ReceivePackUnpackStatus::Ok,
        commands: vec![ReceivePackCommandStatus::Ok {
            name: "refs/heads/main".into(),
        }],
    };
    let mut encoded = Vec::new();
    write_receive_pack_report_status(&mut encoded, &report).expect("test operation should succeed");
    encoded.extend_from_slice(b"tail");

    let mut input = encoded.as_slice();
    assert_eq!(
        read_receive_pack_report_status(&mut input).expect("test operation should succeed"),
        report
    );
    assert_eq!(input, b"tail");
}

#[test]
fn receive_pack_report_status_rejects_malformed_status_lines() {
    assert!(parse_receive_pack_report_status(&[]).is_err());
    assert!(parse_receive_pack_report_status(&[
        PktLineFrame::Data(b"unpack ok\n".to_vec()),
        PktLineFrame::Data(b"ok refs/heads/main\n".to_vec()),
    ])
    .is_err());
    assert!(parse_receive_pack_report_status(&[
        PktLineFrame::Flush,
        PktLineFrame::Data(b"ok refs/heads/main\n".to_vec()),
    ])
    .is_err());
    assert!(parse_receive_pack_report_status(&[
        PktLineFrame::Data(b"unpack ok\n".to_vec()),
        PktLineFrame::Data(b"bad refs/heads/main\n".to_vec()),
        PktLineFrame::Flush,
    ])
    .is_err());
    assert!(parse_receive_pack_report_status(&[
        PktLineFrame::Data(b"unpack ok\n".to_vec()),
        PktLineFrame::Data(b"ng refs/heads/main\n".to_vec()),
        PktLineFrame::Flush,
    ])
    .is_err());
    assert!(encode_receive_pack_report_status(&ReceivePackReportStatus {
        unpack: ReceivePackUnpackStatus::Error("".into()),
        commands: Vec::new(),
    })
    .is_err());
    assert!(encode_receive_pack_report_status(&ReceivePackReportStatus {
        unpack: ReceivePackUnpackStatus::Ok,
        commands: vec![ReceivePackCommandStatus::Ok {
            name: "bad ref".into(),
        }],
    })
    .is_err());
}

#[test]
fn receive_pack_report_status_v2_parses_and_encodes_options() {
    let old_oid = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");
    let new_oid = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "2222222222222222222222222222222222222222",
    )
    .expect("test operation should succeed");
    let frames = vec![
        PktLineFrame::Data(b"unpack ok\n".to_vec()),
        PktLineFrame::Data(b"ok refs/for/main\n".to_vec()),
        PktLineFrame::Data(b"option refname refs/heads/main\n".to_vec()),
        PktLineFrame::Data(b"option old-oid 1111111111111111111111111111111111111111\n".to_vec()),
        PktLineFrame::Data(b"option new-oid 2222222222222222222222222222222222222222\n".to_vec()),
        PktLineFrame::Data(b"option forced-update\n".to_vec()),
        PktLineFrame::Data(b"ng refs/heads/old rejected by hook\n".to_vec()),
        PktLineFrame::Flush,
    ];
    let report = parse_receive_pack_report_status_v2(ObjectFormat::Sha1, &frames)
        .expect("test operation should succeed");
    assert_eq!(
        report,
        ReceivePackReportStatusV2 {
            unpack: ReceivePackUnpackStatus::Ok,
            commands: vec![
                ReceivePackCommandStatusV2::Ok {
                    name: "refs/for/main".into(),
                    options: ReceivePackCommandStatusV2Options {
                        refname: Some("refs/heads/main".into()),
                        old_oid: Some(old_oid),
                        new_oid: Some(new_oid),
                        forced_update: true,
                    },
                },
                ReceivePackCommandStatusV2::Ng {
                    name: "refs/heads/old".into(),
                    message: "rejected by hook".into(),
                },
            ],
        }
    );
    assert_eq!(
        encode_receive_pack_report_status_v2(&report).expect("test operation should succeed"),
        frames
    );
}

#[test]
fn receive_pack_report_status_v2_streams_round_trip() {
    let report = ReceivePackReportStatusV2 {
        unpack: ReceivePackUnpackStatus::Ok,
        commands: vec![ReceivePackCommandStatusV2::Ok {
            name: "refs/for/main".into(),
            options: ReceivePackCommandStatusV2Options {
                refname: Some("refs/heads/main".into()),
                old_oid: None,
                new_oid: None,
                forced_update: false,
            },
        }],
    };
    let mut encoded = Vec::new();
    write_receive_pack_report_status_v2(&mut encoded, &report)
        .expect("test operation should succeed");
    encoded.extend_from_slice(b"tail");

    let mut input = encoded.as_slice();
    assert_eq!(
        read_receive_pack_report_status_v2(ObjectFormat::Sha1, &mut input)
            .expect("test operation should succeed"),
        report
    );
    assert_eq!(input, b"tail");
}

#[test]
fn receive_pack_report_status_v2_rejects_malformed_options() {
    assert!(parse_receive_pack_report_status_v2(ObjectFormat::Sha1, &[]).is_err());
    assert!(parse_receive_pack_report_status_v2(
        ObjectFormat::Sha1,
        &[
            PktLineFrame::Data(b"unpack ok\n".to_vec()),
            PktLineFrame::Data(b"option refname refs/heads/main\n".to_vec()),
            PktLineFrame::Flush,
        ],
    )
    .is_err());
    assert!(parse_receive_pack_report_status_v2(
        ObjectFormat::Sha1,
        &[
            PktLineFrame::Data(b"unpack ok\n".to_vec()),
            PktLineFrame::Data(b"ng refs/heads/main rejected\n".to_vec()),
            PktLineFrame::Data(b"option refname refs/heads/main\n".to_vec()),
            PktLineFrame::Flush,
        ],
    )
    .is_err());
    assert!(parse_receive_pack_report_status_v2(
        ObjectFormat::Sha1,
        &[
            PktLineFrame::Data(b"unpack ok\n".to_vec()),
            PktLineFrame::Data(b"ok refs/for/main\n".to_vec()),
            PktLineFrame::Data(b"option refname refs/heads/main\n".to_vec()),
            PktLineFrame::Data(b"option refname refs/heads/next\n".to_vec()),
            PktLineFrame::Flush,
        ],
    )
    .is_err());
    assert!(parse_receive_pack_report_status_v2(
        ObjectFormat::Sha1,
        &[
            PktLineFrame::Data(b"unpack ok\n".to_vec()),
            PktLineFrame::Data(b"ok refs/for/main\n".to_vec()),
            PktLineFrame::Data(b"option old-oid not-an-oid\n".to_vec()),
            PktLineFrame::Flush,
        ],
    )
    .is_err());
    assert!(
        encode_receive_pack_report_status_v2(&ReceivePackReportStatusV2 {
            unpack: ReceivePackUnpackStatus::Ok,
            commands: vec![ReceivePackCommandStatusV2::Ok {
                name: "refs/for/main".into(),
                options: ReceivePackCommandStatusV2Options {
                    refname: Some("bad ref".into()),
                    ..ReceivePackCommandStatusV2Options::default()
                },
            }],
        })
        .is_err()
    );
}

#[test]
fn receive_pack_push_options_parse_and_encode_options() {
    let frames = vec![
        PktLineFrame::Data(b"ci.skip\n".to_vec()),
        PktLineFrame::Data(b"deploy target=staging\n".to_vec()),
        PktLineFrame::Data(b"\n".to_vec()),
        PktLineFrame::Flush,
    ];
    let options = parse_receive_pack_push_options(&frames).expect("test operation should succeed");
    assert_eq!(
        options,
        vec![
            "ci.skip".to_string(),
            "deploy target=staging".to_string(),
            String::new(),
        ]
    );
    assert_eq!(
        encode_receive_pack_push_options(&options).expect("test operation should succeed"),
        frames
    );
    assert_eq!(
        parse_receive_pack_push_options(&[PktLineFrame::Flush])
            .expect("test operation should succeed"),
        Vec::<String>::new()
    );
}

#[test]
fn receive_pack_push_options_streams_round_trip() {
    let options = vec!["ci.skip".to_string(), "reviewer=alice".to_string()];
    let mut encoded = Vec::new();
    write_receive_pack_push_options(&mut encoded, &options).expect("test operation should succeed");
    encoded.extend_from_slice(b"PACK");

    let mut input = encoded.as_slice();
    assert_eq!(
        read_receive_pack_push_options(&mut input).expect("test operation should succeed"),
        options
    );
    assert_eq!(input, b"PACK");
}

#[test]
fn receive_pack_push_options_reject_malformed_streams() {
    assert!(parse_receive_pack_push_options(&[PktLineFrame::Data(b"ci.skip\n".to_vec())]).is_err());
    assert!(
        parse_receive_pack_push_options(&[PktLineFrame::Delimiter, PktLineFrame::Flush]).is_err()
    );
    assert!(parse_receive_pack_push_options(&[
        PktLineFrame::Data(b"ci.skip\n".to_vec()),
        PktLineFrame::Flush,
        PktLineFrame::Data(b"after\n".to_vec()),
    ])
    .is_err());
    assert!(parse_receive_pack_push_options(&[
        PktLineFrame::Data(b"bad\0option\n".to_vec()),
        PktLineFrame::Flush,
    ])
    .is_err());
    assert!(encode_receive_pack_push_options(&["bad\noption".to_string()]).is_err());
}

#[test]
fn protocol_v2_advertisement_parses_version_and_capabilities() {
    let frames = parse_pkt_line_stream(
            b"000eversion 2\n0015agent=git/2.54.0\n0013ls-refs=unborn\n0027fetch=shallow wait-for-done filter\n0012server-option\n0000",
        )
        .expect("test operation should succeed");
    let handshake =
        parse_protocol_v2_advertisement(&frames).expect("test operation should succeed");
    assert_eq!(handshake.protocol, ProtocolVersion::V2);
    assert_eq!(
        handshake.capabilities,
        vec![
            Capability {
                name: "agent".into(),
                value: Some("git/2.54.0".into()),
            },
            Capability {
                name: "ls-refs".into(),
                value: Some("unborn".into()),
            },
            Capability {
                name: "fetch".into(),
                value: Some("shallow wait-for-done filter".into()),
            },
            Capability {
                name: "server-option".into(),
                value: None,
            },
        ]
    );
    assert_eq!(
        encode_protocol_v2_advertisement(&handshake).expect("test operation should succeed"),
        frames
    );
}

#[test]
fn protocol_v2_advertisement_reads_until_flush() {
    let mut input = b"000eversion 2\n0013ls-refs=unborn\n0000next-session".as_slice();
    let handshake =
        read_protocol_v2_advertisement(&mut input).expect("test operation should succeed");
    assert_eq!(handshake.protocol, ProtocolVersion::V2);
    assert_eq!(
        handshake.capabilities,
        vec![Capability {
            name: "ls-refs".into(),
            value: Some("unborn".into()),
        }]
    );
    assert_eq!(input, b"next-session");
}

#[test]
fn protocol_v2_advertisement_writes_stream() {
    let handshake = TransportHandshake {
        protocol: ProtocolVersion::V2,
        capabilities: vec![
            Capability {
                name: "agent".into(),
                value: Some("sley/0".into()),
            },
            Capability {
                name: "fetch".into(),
                value: Some("shallow filter".into()),
            },
        ],
    };
    let mut encoded = Vec::new();
    write_protocol_v2_advertisement(&mut encoded, &handshake)
        .expect("test operation should succeed");
    let mut input = encoded.as_slice();
    assert_eq!(
        read_protocol_v2_advertisement(&mut input).expect("test operation should succeed"),
        handshake
    );
    assert!(input.is_empty());
    assert!(encode_protocol_v2_advertisement(&TransportHandshake {
        protocol: ProtocolVersion::V1,
        capabilities: Vec::new(),
    })
    .is_err());
}

#[test]
fn protocol_v2_advertisement_rejects_malformed_sequences() {
    assert!(parse_protocol_v2_advertisement(&[]).is_err());
    assert!(parse_protocol_v2_advertisement(&[
        PktLineFrame::Data(b"version 1\n".to_vec()),
        PktLineFrame::Flush,
    ])
    .is_err());
    assert!(
        parse_protocol_v2_advertisement(&[PktLineFrame::Data(b"version 2\n".to_vec())]).is_err()
    );
    assert!(parse_protocol_v2_advertisement(&[
        PktLineFrame::Data(b"version 2\n".to_vec()),
        PktLineFrame::Delimiter,
    ])
    .is_err());
    assert!(parse_protocol_v2_advertisement(&[
        PktLineFrame::Data(b"version 2\n".to_vec()),
        PktLineFrame::Data(b"fetch=\n".to_vec()),
        PktLineFrame::Flush,
    ])
    .is_err());
}

#[test]
fn protocol_v2_command_request_parses_and_encodes_sections() {
    let frames = parse_pkt_line_stream(
            b"0014command=ls-refs\n0011agent=sley/0\n0017object-format=sha1\n00010009peel\n000csymrefs\n001bref-prefix refs/heads/\n0000",
        )
        .expect("test operation should succeed");
    let request =
        parse_protocol_v2_command_request(&frames).expect("test operation should succeed");
    assert_eq!(
        request,
        ProtocolV2CommandRequest {
            command: "ls-refs".into(),
            capabilities: vec![
                Capability {
                    name: "agent".into(),
                    value: Some("sley/0".into()),
                },
                Capability {
                    name: "object-format".into(),
                    value: Some("sha1".into()),
                },
            ],
            arguments: vec![
                b"peel".to_vec(),
                b"symrefs".to_vec(),
                b"ref-prefix refs/heads/".to_vec(),
            ],
        }
    );
    assert_eq!(
        encode_protocol_v2_command_request(&request).expect("test operation should succeed"),
        frames
    );
}

#[test]
fn protocol_v2_command_request_allows_no_argument_section() {
    let frames =
        parse_pkt_line_stream(b"0012command=fetch\n0000").expect("test operation should succeed");
    let request =
        parse_protocol_v2_command_request(&frames).expect("test operation should succeed");
    assert_eq!(
        request,
        ProtocolV2CommandRequest {
            command: "fetch".into(),
            capabilities: Vec::new(),
            arguments: Vec::new(),
        }
    );
    assert_eq!(
        encode_protocol_v2_command_request(&request).expect("test operation should succeed"),
        frames
    );
}

#[test]
fn protocol_v2_request_parses_commands_and_empty_done() {
    let frames =
        parse_pkt_line_stream(b"0012command=fetch\n0000").expect("test operation should succeed");
    let command = ProtocolV2CommandRequest {
        command: "fetch".into(),
        capabilities: Vec::new(),
        arguments: Vec::new(),
    };
    assert_eq!(
        parse_protocol_v2_request(&frames).expect("test operation should succeed"),
        ProtocolV2Request::Command(command.clone())
    );
    assert_eq!(
        encode_protocol_v2_request(&ProtocolV2Request::Command(command))
            .expect("test operation should succeed"),
        frames
    );

    assert_eq!(
        parse_protocol_v2_request(&[PktLineFrame::Flush]).expect("test operation should succeed"),
        ProtocolV2Request::Done
    );
    assert_eq!(
        encode_protocol_v2_request(&ProtocolV2Request::Done)
            .expect("test operation should succeed"),
        vec![PktLineFrame::Flush]
    );
}

#[test]
fn protocol_v2_request_streams_empty_done() {
    let mut encoded = Vec::new();
    write_protocol_v2_request(&mut encoded, &ProtocolV2Request::Done)
        .expect("test operation should succeed");
    encoded.extend_from_slice(b"tail");

    let mut input = encoded.as_slice();
    assert_eq!(
        read_protocol_v2_request(&mut input).expect("test operation should succeed"),
        ProtocolV2Request::Done
    );
    assert_eq!(input, b"tail");
    let mut command_input = encoded.as_slice();
    assert!(read_protocol_v2_command_request(&mut command_input).is_err());
}

#[test]
fn protocol_v2_command_request_streams_round_trip() {
    let request = ProtocolV2CommandRequest {
        command: "ls-refs".into(),
        capabilities: vec![Capability {
            name: "agent".into(),
            value: Some("sley/0".into()),
        }],
        arguments: vec![b"peel".to_vec(), b"symrefs".to_vec()],
    };
    let mut encoded = Vec::new();
    write_protocol_v2_command_request(&mut encoded, &request)
        .expect("test operation should succeed");
    encoded.extend_from_slice(b"tail");

    let mut input = encoded.as_slice();
    assert_eq!(
        read_protocol_v2_command_request(&mut input).expect("test operation should succeed"),
        request
    );
    assert_eq!(input, b"tail");
}

#[test]
fn protocol_v2_command_request_rejects_malformed_sequences() {
    assert!(parse_protocol_v2_command_request(&[]).is_err());
    assert!(parse_protocol_v2_command_request(&[
        PktLineFrame::Data(b"agent=sley/0\n".to_vec()),
        PktLineFrame::Flush,
    ])
    .is_err());
    assert!(parse_protocol_v2_command_request(&[
        PktLineFrame::Data(b"command=ls-refs\n".to_vec()),
        PktLineFrame::Delimiter,
        PktLineFrame::Delimiter,
        PktLineFrame::Flush,
    ])
    .is_err());
    assert!(parse_protocol_v2_command_request(&[
        PktLineFrame::Data(b"command=ls-refs\n".to_vec()),
        PktLineFrame::Delimiter,
        PktLineFrame::Data(b"\n".to_vec()),
        PktLineFrame::Flush,
    ])
    .is_err());
    assert!(
        encode_protocol_v2_command_request(&ProtocolV2CommandRequest {
            command: "bad command".into(),
            capabilities: Vec::new(),
            arguments: Vec::new(),
        })
        .is_err()
    );
}

#[test]
fn protocol_v2_ls_refs_request_parses_and_encodes_arguments() {
    let command = ProtocolV2CommandRequest {
        command: "ls-refs".into(),
        capabilities: Vec::new(),
        arguments: vec![
            b"peel".to_vec(),
            b"symrefs".to_vec(),
            b"unborn".to_vec(),
            b"ref-prefix HEAD".to_vec(),
            b"ref-prefix refs/heads/".to_vec(),
        ],
    };
    let request = ProtocolV2LsRefsRequest::from_command_request(&command)
        .expect("test operation should succeed");
    assert_eq!(
        request,
        ProtocolV2LsRefsRequest {
            peel: true,
            symrefs: true,
            unborn: true,
            ref_prefixes: vec!["HEAD".into(), "refs/heads/".into()],
        }
    );
    assert_eq!(
        request
            .to_command_request()
            .expect("test operation should succeed"),
        command
    );
    assert!(
        ProtocolV2LsRefsRequest::from_command_request(&ProtocolV2CommandRequest {
            command: "fetch".into(),
            capabilities: Vec::new(),
            arguments: Vec::new(),
        })
        .is_err()
    );
    assert!(
        ProtocolV2LsRefsRequest::from_command_request(&ProtocolV2CommandRequest {
            command: "ls-refs".into(),
            capabilities: Vec::new(),
            arguments: vec![b"ref-prefix ".to_vec()],
        })
        .is_err()
    );
}

#[test]
fn protocol_v2_ls_refs_request_streams_round_trip() {
    let request = ProtocolV2LsRefsRequest {
        peel: true,
        symrefs: true,
        unborn: false,
        ref_prefixes: vec!["HEAD".into(), "refs/tags/".into()],
    };
    let mut encoded = Vec::new();
    write_protocol_v2_ls_refs_request(&mut encoded, &request)
        .expect("test operation should succeed");
    encoded.extend_from_slice(b"tail");

    let mut input = encoded.as_slice();
    assert_eq!(
        read_protocol_v2_ls_refs_request(&mut input).expect("test operation should succeed"),
        request
    );
    assert_eq!(input, b"tail");
}

#[test]
fn protocol_v2_ls_refs_response_parses_and_encodes_records() {
    let oid = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");
    let peeled = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "2222222222222222222222222222222222222222",
    )
    .expect("test operation should succeed");
    let frames = vec![
            PktLineFrame::Data(
                b"1111111111111111111111111111111111111111 refs/tags/v1 peeled:2222222222222222222222222222222222222222 symref-target:refs/heads/main custom\n"
                    .to_vec(),
            ),
            PktLineFrame::Data(b"unborn HEAD symref-target:refs/heads/main\n".to_vec()),
            PktLineFrame::Flush,
        ];
    let records = parse_protocol_v2_ls_refs_response(ObjectFormat::Sha1, &frames)
        .expect("test operation should succeed");
    assert_eq!(
        records,
        vec![
            ProtocolV2LsRefsRecord::Ref(ProtocolV2LsRefsRef {
                oid,
                name: "refs/tags/v1".into(),
                peeled: Some(peeled),
                symref_target: Some("refs/heads/main".into()),
                attributes: vec!["custom".into()],
            }),
            ProtocolV2LsRefsRecord::Unborn {
                name: "HEAD".into(),
                symref_target: Some("refs/heads/main".into()),
                attributes: Vec::new(),
            },
        ]
    );
    assert_eq!(
        encode_protocol_v2_ls_refs_response(&records).expect("test operation should succeed"),
        frames
    );
}

#[test]
fn protocol_v2_ls_refs_response_streams_round_trip() {
    let oid = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");
    let records = vec![ProtocolV2LsRefsRecord::Ref(ProtocolV2LsRefsRef {
        oid,
        name: "refs/heads/main".into(),
        peeled: None,
        symref_target: Some("refs/heads/trunk".into()),
        attributes: vec!["custom".into()],
    })];
    let mut encoded = Vec::new();
    write_protocol_v2_ls_refs_response(&mut encoded, &records)
        .expect("test operation should succeed");
    encoded.extend_from_slice(b"tail");

    let mut input = encoded.as_slice();
    assert_eq!(
        read_protocol_v2_ls_refs_response(ObjectFormat::Sha1, &mut input)
            .expect("test operation should succeed"),
        records
    );
    assert_eq!(input, b"tail");
}

#[test]
fn protocol_v2_ls_refs_response_reads_stateless_response_end() {
    let oid = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");
    let records = vec![ProtocolV2LsRefsRecord::Ref(ProtocolV2LsRefsRef {
        oid,
        name: "refs/heads/main".into(),
        peeled: None,
        symref_target: None,
        attributes: Vec::new(),
    })];
    let mut encoded = Vec::new();
    write_protocol_v2_ls_refs_response_with_response_end(&mut encoded, &records)
        .expect("test operation should succeed");
    encoded.extend_from_slice(b"tail");

    let mut input = encoded.as_slice();
    assert_eq!(
        read_protocol_v2_ls_refs_response_until_response_end(ObjectFormat::Sha1, &mut input)
            .expect("test operation should succeed"),
        records
    );
    assert_eq!(input, b"tail");
    assert!(parse_protocol_v2_ls_refs_response(
        ObjectFormat::Sha1,
        &[
            PktLineFrame::Data(
                b"1111111111111111111111111111111111111111 refs/heads/main\n".to_vec()
            ),
            PktLineFrame::ResponseEnd
        ],
    )
    .is_err());
}

#[test]
fn protocol_v2_ls_refs_exchange_writes_request_and_reads_response() {
    let oid = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");
    let request = ProtocolV2LsRefsRequest {
        peel: true,
        symrefs: true,
        unborn: false,
        ref_prefixes: vec!["refs/heads/".into()],
    };
    let records = vec![ProtocolV2LsRefsRecord::Ref(ProtocolV2LsRefsRef {
        oid,
        name: "refs/heads/main".into(),
        peeled: None,
        symref_target: None,
        attributes: Vec::new(),
    })];
    let mut response = Vec::new();
    write_protocol_v2_ls_refs_response(&mut response, &records)
        .expect("test operation should succeed");

    let mut input = response.as_slice();
    let mut output = Vec::new();
    assert_eq!(
        exchange_protocol_v2_ls_refs(ObjectFormat::Sha1, &mut input, &mut output, &request)
            .expect("test operation should succeed"),
        records
    );
    assert!(input.is_empty());
    let mut output_read = output.as_slice();
    assert_eq!(
        read_protocol_v2_ls_refs_request(&mut output_read).expect("test operation should succeed"),
        request
    );
}

#[test]
fn protocol_v2_ls_refs_response_rejects_malformed_records() {
    assert!(parse_protocol_v2_ls_refs_response(
        ObjectFormat::Sha1,
        &[PktLineFrame::Data(
            b"1111111111111111111111111111111111111111 refs/heads/main\n".to_vec()
        )],
    )
    .is_err());
    assert!(
            parse_protocol_v2_ls_refs_response(
                ObjectFormat::Sha1,
                &[
                    PktLineFrame::Data(
                        b"1111111111111111111111111111111111111111 refs/heads/main peeled:2222222222222222222222222222222222222222 peeled:3333333333333333333333333333333333333333\n"
                            .to_vec()
                    ),
                    PktLineFrame::Flush,
                ],
            )
            .is_err()
        );
    assert!(parse_protocol_v2_ls_refs_response(
        ObjectFormat::Sha1,
        &[
            PktLineFrame::Data(
                b"unborn HEAD peeled:2222222222222222222222222222222222222222\n".to_vec()
            ),
            PktLineFrame::Flush,
        ],
    )
    .is_err());
    assert!(
        encode_protocol_v2_ls_refs_response(&[ProtocolV2LsRefsRecord::Ref(ProtocolV2LsRefsRef {
            oid: ObjectId::from_hex(
                ObjectFormat::Sha1,
                "1111111111111111111111111111111111111111",
            )
            .expect("test operation should succeed"),
            name: "refs/heads/main".into(),
            peeled: None,
            symref_target: None,
            attributes: vec!["peeled:2222222222222222222222222222222222222222".into()],
        })])
        .is_err()
    );
}

#[test]
fn protocol_v2_fetch_request_parses_and_encodes_arguments() {
    let want = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");
    let have = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "2222222222222222222222222222222222222222",
    )
    .expect("test operation should succeed");
    let shallow = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "3333333333333333333333333333333333333333",
    )
    .expect("test operation should succeed");
    let command = ProtocolV2CommandRequest {
        command: "fetch".into(),
        capabilities: Vec::new(),
        arguments: vec![
            b"want 1111111111111111111111111111111111111111".to_vec(),
            b"want-ref refs/heads/main".to_vec(),
            b"have 2222222222222222222222222222222222222222".to_vec(),
            b"shallow 3333333333333333333333333333333333333333".to_vec(),
            b"deepen 10".to_vec(),
            b"deepen-since 123456789".to_vec(),
            b"deepen-not refs/tags/v1".to_vec(),
            b"deepen-relative".to_vec(),
            b"filter blob:none".to_vec(),
            b"packfile-uris http,https".to_vec(),
            b"thin-pack".to_vec(),
            b"no-progress".to_vec(),
            b"include-tag".to_vec(),
            b"ofs-delta".to_vec(),
            b"sideband-all".to_vec(),
            b"wait-for-done".to_vec(),
            b"done".to_vec(),
        ],
    };
    let request = ProtocolV2FetchRequest::from_command_request(ObjectFormat::Sha1, &command)
        .expect("test operation should succeed");
    assert_eq!(
        request,
        ProtocolV2FetchRequest {
            wants: vec![want],
            want_refs: vec!["refs/heads/main".into()],
            haves: vec![have],
            shallow: vec![shallow],
            deepen: Some(10),
            deepen_since: Some(123456789),
            deepen_not: vec!["refs/tags/v1".into()],
            deepen_relative: true,
            filter: Some("blob:none".into()),
            packfile_uris: Some("http,https".into()),
            thin_pack: true,
            no_progress: true,
            include_tag: true,
            ofs_delta: true,
            sideband_all: true,
            wait_for_done: true,
            done: true,
        }
    );
    assert_eq!(
        request
            .to_command_request()
            .expect("test operation should succeed"),
        command
    );
}

#[test]
fn protocol_v2_fetch_request_rejects_malformed_arguments() {
    assert!(ProtocolV2FetchRequest::from_command_request(
        ObjectFormat::Sha1,
        &ProtocolV2CommandRequest {
            command: "ls-refs".into(),
            capabilities: Vec::new(),
            arguments: Vec::new(),
        },
    )
    .is_err());
    assert!(ProtocolV2FetchRequest::from_command_request(
        ObjectFormat::Sha1,
        &ProtocolV2CommandRequest {
            command: "fetch".into(),
            capabilities: Vec::new(),
            arguments: vec![b"want not-an-oid".to_vec()],
        },
    )
    .is_err());
    assert!(ProtocolV2FetchRequest::from_command_request(
        ObjectFormat::Sha1,
        &ProtocolV2CommandRequest {
            command: "fetch".into(),
            capabilities: Vec::new(),
            arguments: vec![b"deepen 0".to_vec()],
        },
    )
    .is_err());
    assert!(ProtocolV2FetchRequest::from_command_request(
        ObjectFormat::Sha1,
        &ProtocolV2CommandRequest {
            command: "fetch".into(),
            capabilities: Vec::new(),
            arguments: vec![b"filter blob:none".to_vec(), b"filter tree:0".to_vec()],
        },
    )
    .is_err());
    assert!(ProtocolV2FetchRequest {
        deepen: Some(0),
        ..ProtocolV2FetchRequest::default()
    }
    .to_command_request()
    .is_err());
}

#[test]
fn protocol_v2_fetch_request_streams_round_trip() {
    let want = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");
    let have = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "2222222222222222222222222222222222222222",
    )
    .expect("test operation should succeed");
    let request = ProtocolV2FetchRequest {
        wants: vec![want],
        haves: vec![have],
        deepen: Some(5),
        filter: Some("blob:none".into()),
        thin_pack: true,
        done: true,
        ..ProtocolV2FetchRequest::default()
    };
    let mut encoded = Vec::new();
    write_protocol_v2_fetch_request(&mut encoded, &request).expect("test operation should succeed");
    encoded.extend_from_slice(b"tail");

    let mut input = encoded.as_slice();
    assert_eq!(
        read_protocol_v2_fetch_request(ObjectFormat::Sha1, &mut input)
            .expect("test operation should succeed"),
        request
    );
    assert_eq!(input, b"tail");
}

#[test]
fn protocol_v2_fetch_response_parses_and_encodes_sections() {
    let ack = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");
    let shallow = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "2222222222222222222222222222222222222222",
    )
    .expect("test operation should succeed");
    let wanted = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "3333333333333333333333333333333333333333",
    )
    .expect("test operation should succeed");
    let pack_hash = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "4444444444444444444444444444444444444444",
    )
    .expect("test operation should succeed");
    let frames = vec![
        PktLineFrame::Data(b"acknowledgments\n".to_vec()),
        PktLineFrame::Data(b"ACK 1111111111111111111111111111111111111111\n".to_vec()),
        PktLineFrame::Data(b"ready\n".to_vec()),
        PktLineFrame::Delimiter,
        PktLineFrame::Data(b"shallow-info\n".to_vec()),
        PktLineFrame::Data(b"shallow 2222222222222222222222222222222222222222\n".to_vec()),
        PktLineFrame::Delimiter,
        PktLineFrame::Data(b"wanted-refs\n".to_vec()),
        PktLineFrame::Data(b"3333333333333333333333333333333333333333 refs/heads/main\n".to_vec()),
        PktLineFrame::Delimiter,
        PktLineFrame::Data(b"packfile-uris\n".to_vec()),
        PktLineFrame::Data(
            b"4444444444444444444444444444444444444444 https://example.invalid/pack-a.pack\n"
                .to_vec(),
        ),
        PktLineFrame::Delimiter,
        PktLineFrame::Data(b"packfile\n".to_vec()),
        PktLineFrame::Data(b"\x01PACK bytes".to_vec()),
        PktLineFrame::Flush,
    ];
    let sections = parse_protocol_v2_fetch_response(ObjectFormat::Sha1, &frames)
        .expect("test operation should succeed");
    assert_eq!(
        sections,
        vec![
            ProtocolV2FetchResponseSection::Acknowledgments(vec![
                ProtocolV2FetchAcknowledgment::Ack(ack),
                ProtocolV2FetchAcknowledgment::Ready,
            ]),
            ProtocolV2FetchResponseSection::ShallowInfo(vec![ProtocolV2FetchShallowInfo::Shallow(
                shallow
            )]),
            ProtocolV2FetchResponseSection::WantedRefs(vec![ProtocolV2FetchWantedRef {
                oid: wanted,
                name: "refs/heads/main".into(),
            }]),
            ProtocolV2FetchResponseSection::PackfileUris(vec![ProtocolV2FetchPackfileUri {
                pack_hash,
                uri: "https://example.invalid/pack-a.pack".into(),
            }]),
            ProtocolV2FetchResponseSection::Packfile(vec![b"\x01PACK bytes".to_vec()]),
        ]
    );
    assert_eq!(
        encode_protocol_v2_fetch_response(&sections).expect("test operation should succeed"),
        frames
    );
}

#[test]
fn protocol_v2_fetch_response_preserves_unknown_sections() {
    let frames = vec![
        PktLineFrame::Data(b"server-feature\n".to_vec()),
        PktLineFrame::Data(b"opaque line\n".to_vec()),
        PktLineFrame::Flush,
    ];
    let sections = parse_protocol_v2_fetch_response(ObjectFormat::Sha1, &frames)
        .expect("test operation should succeed");
    assert_eq!(
        sections,
        vec![ProtocolV2FetchResponseSection::Unknown {
            name: "server-feature".into(),
            lines: vec![b"opaque line\n".to_vec()],
        }]
    );
    assert_eq!(
        encode_protocol_v2_fetch_response(&sections).expect("test operation should succeed"),
        frames
    );
}

#[test]
fn protocol_v2_fetch_response_streams_round_trip() {
    let ack = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");
    let sections = vec![
        ProtocolV2FetchResponseSection::Acknowledgments(vec![
            ProtocolV2FetchAcknowledgment::Ack(ack),
            ProtocolV2FetchAcknowledgment::Ready,
        ]),
        ProtocolV2FetchResponseSection::Packfile(vec![b"\x01PACK bytes".to_vec()]),
    ];
    let mut encoded = Vec::new();
    write_protocol_v2_fetch_response(&mut encoded, &sections)
        .expect("test operation should succeed");
    encoded.extend_from_slice(b"tail");

    let mut input = encoded.as_slice();
    assert_eq!(
        read_protocol_v2_fetch_response(ObjectFormat::Sha1, &mut input)
            .expect("test operation should succeed"),
        sections
    );
    assert_eq!(input, b"tail");
}

fn sideband_data_frame(data: &[u8]) -> PktLineFrame {
    PktLineFrame::Data(
        encode_sideband_packet(&SideBandPacket {
            channel: SideBandChannel::Data,
            data: data.to_vec(),
        })
        .expect("test operation should succeed"),
    )
}

fn sideband_progress_frame(data: &[u8]) -> PktLineFrame {
    PktLineFrame::Data(
        encode_sideband_packet(&SideBandPacket {
            channel: SideBandChannel::Progress,
            data: data.to_vec(),
        })
        .expect("test operation should succeed"),
    )
}

#[test]
fn fetch_response_header_reads_sideband_wrapped_section_headers() {
    // Under sideband-all, every fetch-response pkt — including section
    // headers such as `acknowledgments` — arrives sideband-wrapped on
    // channel 1, and a leading channel-2 progress frame may precede the
    // first section header. The header reader must demux both.
    let frames = vec![
        sideband_progress_frame(b"Enumerating objects: 3, done.\n"),
        sideband_data_frame(b"acknowledgments\n"),
        sideband_data_frame(b"NAK\n"),
        PktLineFrame::Delimiter,
        sideband_data_frame(b"packfile\n"),
    ];
    let mut encoded = Vec::new();
    write_pkt_line_frames(&mut encoded, &frames).expect("test operation should succeed");

    let mut input = encoded.as_slice();
    let header = read_protocol_v2_fetch_response_header(ObjectFormat::Sha1, &mut input, true)
        .expect("test operation should succeed");
    assert_eq!(
        header,
        ProtocolV2FetchResponseHeader {
            sections: vec![ProtocolV2FetchResponseSection::Acknowledgments(vec![
                ProtocolV2FetchAcknowledgment::Nak
            ])],
            has_packfile: true,
        }
    );
}

#[test]
fn fetch_response_header_skips_leading_advertisement_before_sideband_response() {
    // On stateless transports the server may prepend an (unwrapped) v2
    // capability advertisement before the sideband-wrapped fetch response.
    // The skip path must consume the raw advertisement and then demux the
    // wrapped section headers that follow.
    let frames = vec![
        PktLineFrame::Data(b"version 2\n".to_vec()),
        PktLineFrame::Data(b"agent=git/2.55\n".to_vec()),
        PktLineFrame::Data(b"fetch=sideband-all\n".to_vec()),
        PktLineFrame::Flush,
        sideband_progress_frame(b"remote: working\n"),
        sideband_data_frame(b"acknowledgments\n"),
        sideband_data_frame(b"NAK\n"),
        PktLineFrame::Delimiter,
        sideband_data_frame(b"packfile\n"),
    ];
    let mut encoded = Vec::new();
    write_pkt_line_frames(&mut encoded, &frames).expect("test operation should succeed");

    let mut input = encoded.as_slice();
    let header = read_protocol_v2_fetch_response_header(ObjectFormat::Sha1, &mut input, true)
        .expect("test operation should succeed");
    assert_eq!(
        header,
        ProtocolV2FetchResponseHeader {
            sections: vec![ProtocolV2FetchResponseSection::Acknowledgments(vec![
                ProtocolV2FetchAcknowledgment::Nak
            ])],
            has_packfile: true,
        }
    );
}

#[test]
fn protocol_v2_fetch_sideband_all_response_parses_sections_and_progress() {
    let frames = vec![
        PktLineFrame::Data(
            encode_sideband_packet(&SideBandPacket {
                channel: SideBandChannel::Data,
                data: b"acknowledgments\n".to_vec(),
            })
            .expect("test operation should succeed"),
        ),
        PktLineFrame::Data(
            encode_sideband_packet(&SideBandPacket {
                channel: SideBandChannel::Data,
                data: b"NAK\n".to_vec(),
            })
            .expect("test operation should succeed"),
        ),
        PktLineFrame::Data(
            encode_sideband_packet(&SideBandPacket {
                channel: SideBandChannel::Progress,
                data: b"keepalive\n".to_vec(),
            })
            .expect("test operation should succeed"),
        ),
        PktLineFrame::Delimiter,
        PktLineFrame::Data(
            encode_sideband_packet(&SideBandPacket {
                channel: SideBandChannel::Data,
                data: b"packfile\n".to_vec(),
            })
            .expect("test operation should succeed"),
        ),
        PktLineFrame::Data(b"\x01PACK".to_vec()),
        PktLineFrame::Data(b"\x02counting objects\n".to_vec()),
        PktLineFrame::Flush,
    ];

    let response = parse_protocol_v2_fetch_sideband_all_response(ObjectFormat::Sha1, &frames)
        .expect("test operation should succeed");
    assert_eq!(
        response,
        ProtocolV2FetchSidebandAllResponse {
            sections: vec![
                ProtocolV2FetchResponseSection::Acknowledgments(vec![
                    ProtocolV2FetchAcknowledgment::Nak
                ]),
                ProtocolV2FetchResponseSection::Packfile(vec![
                    b"\x01PACK".to_vec(),
                    b"\x02counting objects\n".to_vec(),
                ]),
            ],
            progress: vec![b"keepalive\n".to_vec()],
        }
    );
    assert_eq!(
        demux_protocol_v2_fetch_packfile(&response.sections)
            .expect("test operation should succeed"),
        Some(SideBandDemux {
            data: b"PACK".to_vec(),
            progress: vec![b"counting objects\n".to_vec()],
        })
    );
}

#[test]
fn protocol_v2_fetch_sideband_all_response_streams_round_trip() {
    let sections = vec![
        ProtocolV2FetchResponseSection::Acknowledgments(vec![ProtocolV2FetchAcknowledgment::Nak]),
        ProtocolV2FetchResponseSection::Packfile(vec![b"\x01PACK bytes".to_vec()]),
    ];
    let mut encoded = Vec::new();
    write_protocol_v2_fetch_sideband_all_response(&mut encoded, &sections)
        .expect("test operation should succeed");
    encoded.extend_from_slice(b"tail");

    let mut input = encoded.as_slice();
    assert_eq!(
        read_protocol_v2_fetch_sideband_all_response(ObjectFormat::Sha1, &mut input)
            .expect("test operation should succeed"),
        ProtocolV2FetchSidebandAllResponse {
            sections: sections.clone(),
            progress: Vec::new(),
        }
    );
    assert_eq!(input, b"tail");

    let mut encoded = Vec::new();
    write_protocol_v2_fetch_sideband_all_response_with_response_end(&mut encoded, &sections)
        .expect("test operation should succeed");
    encoded.extend_from_slice(b"tail");

    let mut input = encoded.as_slice();
    assert_eq!(
        read_protocol_v2_fetch_sideband_all_response_until_response_end(
            ObjectFormat::Sha1,
            &mut input,
        )
        .expect("test operation should succeed")
        .sections,
        sections
    );
    assert_eq!(input, b"tail");
}

#[test]
fn protocol_v2_fetch_sideband_all_response_rejects_malformed_sideband() {
    assert!(parse_protocol_v2_fetch_sideband_all_response(
        ObjectFormat::Sha1,
        &[
            PktLineFrame::Data(b"acknowledgments\n".to_vec()),
            PktLineFrame::Flush,
        ],
    )
    .is_err());
    assert!(parse_protocol_v2_fetch_sideband_all_response(
        ObjectFormat::Sha1,
        &[
            PktLineFrame::Data(
                encode_sideband_packet(&SideBandPacket {
                    channel: SideBandChannel::Fatal,
                    data: b"remote died\n".to_vec(),
                })
                .expect("test operation should succeed"),
            ),
            PktLineFrame::Flush,
        ],
    )
    .is_err());
}

#[test]
fn protocol_v2_object_info_response_parses_and_encodes_size_records() {
    let oid = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");
    let frames = vec![
        PktLineFrame::Data(b"size\n".to_vec()),
        PktLineFrame::Data(b"1111111111111111111111111111111111111111 12345\n".to_vec()),
        PktLineFrame::Flush,
    ];
    let response = parse_protocol_v2_object_info_response(ObjectFormat::Sha1, &frames)
        .expect("test operation should succeed");
    assert_eq!(
        response,
        ProtocolV2ObjectInfoResponse {
            size: true,
            records: vec![ProtocolV2ObjectInfoRecord { oid, size: 12345 }],
        }
    );
    assert_eq!(
        encode_protocol_v2_object_info_response(&response).expect("test operation should succeed"),
        frames
    );
}

#[test]
fn protocol_v2_object_info_response_streams_and_exchanges() {
    let request = ProtocolV2ObjectInfoRequest {
        size: true,
        oids: vec![ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed")],
    };
    let response = ProtocolV2ObjectInfoResponse {
        size: true,
        records: vec![ProtocolV2ObjectInfoRecord {
            oid: request.oids[0].clone(),
            size: 7,
        }],
    };

    let mut encoded = Vec::new();
    write_protocol_v2_object_info_response(&mut encoded, &response)
        .expect("test operation should succeed");
    encoded.extend_from_slice(b"tail");
    let mut input = encoded.as_slice();
    assert_eq!(
        read_protocol_v2_object_info_response(ObjectFormat::Sha1, &mut input)
            .expect("test operation should succeed"),
        response
    );
    assert_eq!(input, b"tail");

    let mut response_bytes = Vec::new();
    write_protocol_v2_object_info_response(&mut response_bytes, &response)
        .expect("test operation should succeed");
    let mut input = response_bytes.as_slice();
    let mut output = Vec::new();
    assert_eq!(
        exchange_protocol_v2_object_info(ObjectFormat::Sha1, &mut input, &mut output, &request,)
            .expect("test operation should succeed"),
        response
    );
    assert!(input.is_empty());
    let mut output_read = output.as_slice();
    assert_eq!(
        read_protocol_v2_object_info_request(ObjectFormat::Sha1, &mut output_read)
            .expect("test operation should succeed"),
        request
    );
}

#[test]
fn protocol_v2_object_info_response_rejects_malformed_records() {
    assert!(parse_protocol_v2_object_info_response(ObjectFormat::Sha1, &[]).is_err());
    assert!(parse_protocol_v2_object_info_response(
        ObjectFormat::Sha1,
        &[PktLineFrame::Data(b"size\n".to_vec())],
    )
    .is_err());
    assert!(parse_protocol_v2_object_info_response(
        ObjectFormat::Sha1,
        &[PktLineFrame::Data(b"type\n".to_vec()), PktLineFrame::Flush,],
    )
    .is_err());
    assert!(parse_protocol_v2_object_info_response(
        ObjectFormat::Sha1,
        &[
            PktLineFrame::Data(b"size\n".to_vec()),
            PktLineFrame::Data(b"1111111111111111111111111111111111111111 not-a-size\n".to_vec()),
            PktLineFrame::Flush,
        ],
    )
    .is_err());
    assert!(parse_protocol_v2_object_info_response(
        ObjectFormat::Sha1,
        &[
            PktLineFrame::Data(b"size\n".to_vec()),
            PktLineFrame::Delimiter,
            PktLineFrame::Flush,
        ],
    )
    .is_err());
    assert!(
        encode_protocol_v2_object_info_response(&ProtocolV2ObjectInfoResponse {
            size: false,
            records: Vec::new(),
        })
        .is_err()
    );
}

#[test]
fn protocol_v2_fetch_response_reads_stateless_response_end() {
    let sections = vec![ProtocolV2FetchResponseSection::Acknowledgments(vec![
        ProtocolV2FetchAcknowledgment::Nak,
    ])];
    let mut encoded = Vec::new();
    write_protocol_v2_fetch_response_with_response_end(&mut encoded, &sections)
        .expect("test operation should succeed");
    encoded.extend_from_slice(b"tail");

    let mut input = encoded.as_slice();
    assert_eq!(
        read_protocol_v2_fetch_response_until_response_end(ObjectFormat::Sha1, &mut input)
            .expect("test operation should succeed"),
        sections
    );
    assert_eq!(input, b"tail");
    assert!(parse_protocol_v2_fetch_response(
        ObjectFormat::Sha1,
        &[
            PktLineFrame::Data(b"acknowledgments\n".to_vec()),
            PktLineFrame::ResponseEnd,
        ],
    )
    .is_err());
}

#[test]
fn protocol_v2_fetch_exchange_writes_request_and_reads_response() {
    let want = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");
    let request = ProtocolV2FetchRequest {
        wants: vec![want],
        thin_pack: true,
        done: true,
        ..ProtocolV2FetchRequest::default()
    };
    let sections = vec![ProtocolV2FetchResponseSection::Acknowledgments(vec![
        ProtocolV2FetchAcknowledgment::Nak,
    ])];
    let mut response = Vec::new();
    write_protocol_v2_fetch_response(&mut response, &sections)
        .expect("test operation should succeed");

    let mut input = response.as_slice();
    let mut output = Vec::new();
    assert_eq!(
        exchange_protocol_v2_fetch(ObjectFormat::Sha1, &mut input, &mut output, &request)
            .expect("test operation should succeed"),
        sections
    );
    assert!(input.is_empty());
    let mut output_read = output.as_slice();
    assert_eq!(
        read_protocol_v2_fetch_request(ObjectFormat::Sha1, &mut output_read)
            .expect("test operation should succeed"),
        request
    );
}

#[test]
fn protocol_v2_fetch_packfile_demuxes_sideband_section() {
    let sections = vec![
        ProtocolV2FetchResponseSection::Acknowledgments(vec![ProtocolV2FetchAcknowledgment::Nak]),
        ProtocolV2FetchResponseSection::Packfile(vec![
            b"\x01PACK".to_vec(),
            b"\x02counting objects\n".to_vec(),
            b"\x01 bytes".to_vec(),
            b"\x02done\n".to_vec(),
        ]),
    ];

    assert_eq!(
        demux_protocol_v2_fetch_packfile(&sections).expect("test operation should succeed"),
        Some(SideBandDemux {
            data: b"PACK bytes".to_vec(),
            progress: vec![b"counting objects\n".to_vec(), b"done\n".to_vec()],
        })
    );
    assert_eq!(
        demux_protocol_v2_fetch_packfile(&[ProtocolV2FetchResponseSection::Acknowledgments(vec![
            ProtocolV2FetchAcknowledgment::Nak
        ],)])
        .expect("test operation should succeed"),
        None
    );
}

#[test]
fn protocol_v2_fetch_packfile_demux_rejects_duplicate_or_bad_sideband() {
    assert!(demux_protocol_v2_fetch_packfile(&[
        ProtocolV2FetchResponseSection::Packfile(vec![b"\x01PACK".to_vec()]),
        ProtocolV2FetchResponseSection::Packfile(vec![b"\x01more".to_vec()]),
    ])
    .is_err());
    assert!(
        demux_protocol_v2_fetch_packfile(&[ProtocolV2FetchResponseSection::Packfile(vec![
            b"\x03remote died\n".to_vec()
        ])])
        .is_err()
    );
    assert!(
        demux_protocol_v2_fetch_packfile(&[ProtocolV2FetchResponseSection::Packfile(vec![
            b"\x04bad".to_vec()
        ])])
        .is_err()
    );
}

#[test]
fn protocol_v2_fetch_response_rejects_malformed_sections() {
    assert!(parse_protocol_v2_fetch_response(
        ObjectFormat::Sha1,
        &[PktLineFrame::Data(b"acknowledgments\n".to_vec())],
    )
    .is_err());
    assert!(parse_protocol_v2_fetch_response(
        ObjectFormat::Sha1,
        &[PktLineFrame::Delimiter, PktLineFrame::Flush],
    )
    .is_err());
    assert!(parse_protocol_v2_fetch_response(
        ObjectFormat::Sha1,
        &[
            PktLineFrame::Data(b"acknowledgments\n".to_vec()),
            PktLineFrame::Data(b"ACK not-an-oid\n".to_vec()),
            PktLineFrame::Flush,
        ],
    )
    .is_err());
    assert!(parse_protocol_v2_fetch_response(
        ObjectFormat::Sha1,
        &[
            PktLineFrame::Data(b"packfile-uris\n".to_vec()),
            PktLineFrame::Data(b"https://example.invalid/pack-a.pack\n".to_vec()),
            PktLineFrame::Flush,
        ],
    )
    .is_err());
    assert!(parse_protocol_v2_fetch_response(
        ObjectFormat::Sha1,
        &[
            PktLineFrame::Data(b"packfile-uris\n".to_vec()),
            PktLineFrame::Data(b"not-a-hash https://example.invalid/pack-a.pack\n".to_vec()),
            PktLineFrame::Flush,
        ],
    )
    .is_err());
    assert!(
        encode_protocol_v2_fetch_response(&[ProtocolV2FetchResponseSection::WantedRefs(vec![
            ProtocolV2FetchWantedRef {
                oid: ObjectId::from_hex(
                    ObjectFormat::Sha1,
                    "1111111111111111111111111111111111111111",
                )
                .expect("test operation should succeed"),
                name: "bad ref".into(),
            }
        ])])
        .is_err()
    );
}

#[test]
fn protocol_v2_ls_refs_response_bridges_into_ref_advertisement_set() {
    let head = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "1111111111111111111111111111111111111111",
    )
    .expect("test operation should succeed");
    let tag = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "2222222222222222222222222222222222222222",
    )
    .expect("test operation should succeed");
    let tag_peeled = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "3333333333333333333333333333333333333333",
    )
    .expect("test operation should succeed");
    let frames = vec![
            PktLineFrame::Data(
                b"1111111111111111111111111111111111111111 HEAD symref-target:refs/heads/main\n"
                    .to_vec(),
            ),
            PktLineFrame::Data(
                b"1111111111111111111111111111111111111111 refs/heads/main\n".to_vec(),
            ),
            PktLineFrame::Data(
                b"2222222222222222222222222222222222222222 refs/tags/v1 peeled:3333333333333333333333333333333333333333\n"
                    .to_vec(),
            ),
            PktLineFrame::Flush,
        ];

    let set =
        parse_protocol_v2_ls_refs_response_as_ref_advertisement_set(ObjectFormat::Sha1, &frames)
            .expect("test operation should succeed");
    assert_eq!(
        set,
        RefAdvertisementSet {
            protocol: ProtocolVersion::V2,
            refs: vec![
                RefAdvertisement {
                    oid: head.clone(),
                    name: "HEAD".into(),
                    capabilities: vec![Capability {
                        name: "symref".into(),
                        value: Some("HEAD:refs/heads/main".into()),
                    }],
                },
                RefAdvertisement {
                    oid: head,
                    name: "refs/heads/main".into(),
                    capabilities: Vec::new(),
                },
                RefAdvertisement {
                    oid: tag,
                    name: "refs/tags/v1".into(),
                    capabilities: Vec::new(),
                },
                RefAdvertisement {
                    oid: tag_peeled,
                    name: "refs/tags/v1^{}".into(),
                    capabilities: Vec::new(),
                },
            ],
            shallow: Vec::new(),
        }
    );

    // The streaming reader path produces the same bridged set.
    let mut encoded = Vec::new();
    write_pkt_line_frames(&mut encoded, &frames).expect("test operation should succeed");
    encoded.extend_from_slice(b"tail");
    let mut input = encoded.as_slice();
    assert_eq!(
        read_protocol_v2_ls_refs_response_as_ref_advertisement_set(ObjectFormat::Sha1, &mut input,)
            .expect("test operation should succeed"),
        set,
    );
    assert_eq!(input, b"tail");
}

#[test]
fn protocol_v2_ls_refs_records_bridge_unborn_head_symref_and_empty() {
    // An unborn HEAD pointing at an as-yet-uncreated branch carries only a
    // symref capability and has no concrete ref to attach it to.
    let records = vec![ProtocolV2LsRefsRecord::Unborn {
        name: "HEAD".into(),
        symref_target: Some("refs/heads/main".into()),
        attributes: Vec::new(),
    }];
    assert!(protocol_v2_ls_refs_records_to_ref_advertisement_set(&records).is_err());

    // An empty ls-refs response bridges to an empty v2 set.
    assert_eq!(
        protocol_v2_ls_refs_records_to_ref_advertisement_set(&[])
            .expect("test operation should succeed"),
        RefAdvertisementSet {
            protocol: ProtocolVersion::V2,
            refs: Vec::new(),
            shallow: Vec::new(),
        }
    );

    // An unborn HEAD alongside a concrete ref attaches the symref to the
    // first ref, matching the v0/v1 advertisement convention.
    let main = ObjectId::from_hex(
        ObjectFormat::Sha1,
        "4444444444444444444444444444444444444444",
    )
    .expect("test operation should succeed");
    let records = vec![
        ProtocolV2LsRefsRecord::Unborn {
            name: "HEAD".into(),
            symref_target: Some("refs/heads/main".into()),
            attributes: Vec::new(),
        },
        ProtocolV2LsRefsRecord::Ref(ProtocolV2LsRefsRef {
            oid: main.clone(),
            name: "refs/heads/main".into(),
            peeled: None,
            symref_target: None,
            attributes: Vec::new(),
        }),
    ];
    let set = protocol_v2_ls_refs_records_to_ref_advertisement_set(&records)
        .expect("test operation should succeed");
    assert_eq!(
        set,
        RefAdvertisementSet {
            protocol: ProtocolVersion::V2,
            refs: vec![RefAdvertisement {
                oid: main,
                name: "refs/heads/main".into(),
                capabilities: vec![Capability {
                    name: "symref".into(),
                    value: Some("HEAD:refs/heads/main".into()),
                }],
            }],
            shallow: Vec::new(),
        }
    );
}
