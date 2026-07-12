use std::process::Command;

#[test]
fn credential_family_short_help_is_byte_identical_to_git() {
    for command in ["credential", "credential-cache", "credential-store"] {
        let expected = Command::new(sley_testkit::oracle_git())
            .args([command, "-h"])
            .output()
            .expect("run oracle help");
        let actual = Command::new(sley_testkit::sley_bin!())
            .args([command, "-h"])
            .output()
            .expect("run sley help");

        assert_eq!(actual.status.code(), expected.status.code(), "{command}");
        assert_eq!(actual.stdout, expected.stdout, "{command} stdout");
        assert_eq!(actual.stderr, expected.stderr, "{command} stderr");
    }
}
