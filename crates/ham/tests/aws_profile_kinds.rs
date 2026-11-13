//! The kinds of AWS profile, read from the AWS files by section and key names only (no value is read). ham decides with these
//! kinds whether a profile signs through the AWS CLI (IAM Identity Center and assume-role profiles) or through the credential
//! providers it has always used (static keys, `credential_process`). setup.sh holds the same definition in its function
//! `profile_kind`; `scripts/tests/test_setup_script.py` runs that function over the same fixture directory, so the two cannot
//! drift apart without a test failing.
//!
//! The fixture is `fixtures/aws-profiles/`: the files `aws-config` and `aws-credentials` (shaped like ~/.aws/config and ~/.aws/credentials; the repository ignores files named credentials*), and `kinds.tsv` (profile name, tab, kind word).
#![allow(clippy::unwrap_used)]

use ham::awsprofile::{ProfileKind, kind_in};
use std::path::PathBuf;

fn fixture(name: &str) -> String {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/aws-profiles");
    std::fs::read_to_string(dir.join(name)).unwrap()
}

#[test]
fn every_profile_of_the_shared_fixture_has_the_kind_the_tsv_names() {
    let config = fixture("aws-config");
    let credentials = fixture("aws-credentials");
    let rows: Vec<(String, String)> = fixture("kinds.tsv")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let (name, kind) = l.split_once('\t').expect("name, tab, kind");
            (name.to_string(), kind.to_string())
        })
        .collect();
    assert!(
        rows.len() >= 20,
        "the fixture should cover every kind and the edge cases: {rows:?}"
    );
    for (name, kind) in &rows {
        let got = kind_in(&[config.as_str(), credentials.as_str()], name);
        assert_eq!(got.word(), kind, "profile {name}");
    }
    // The four kinds that matter and the two that do not are all in the fixture.
    for kind in [
        "credential_process",
        "static",
        "sso",
        "assume",
        "unknown",
        "missing",
    ] {
        assert!(
            rows.iter().any(|(_, k)| k == kind),
            "no profile of kind {kind}"
        );
    }
}

#[test]
fn the_kind_words_are_the_ones_setup_sh_prints() {
    assert_eq!(ProfileKind::Missing.word(), "missing");
    assert_eq!(ProfileKind::CredentialProcess.word(), "credential_process");
    assert_eq!(ProfileKind::StaticKeys.word(), "static");
    assert_eq!(ProfileKind::Sso.word(), "sso");
    assert_eq!(ProfileKind::AssumeRole.word(), "assume");
    assert_eq!(ProfileKind::Unknown.word(), "unknown");
}

#[test]
fn only_sso_and_assume_role_profiles_go_through_the_aws_cli() {
    for (kind, cli) in [
        (ProfileKind::Sso, true),
        (ProfileKind::AssumeRole, true),
        (ProfileKind::CredentialProcess, false),
        (ProfileKind::StaticKeys, false),
        (ProfileKind::Unknown, false),
        (ProfileKind::Missing, false),
    ] {
        assert_eq!(kind.uses_aws_cli(), cli, "{kind:?}");
    }
}

#[test]
fn no_file_text_means_the_profile_is_missing() {
    assert_eq!(kind_in(&[], "anything"), ProfileKind::Missing);
}

#[test]
fn windows_line_ends_and_odd_spacing_do_not_change_the_kind() {
    let crlf = "[profile a]\r\nsso_session = x\r\n\r\n[profile b]\r\nrole_arn = r\r\nsource_profile = a\r\n\r\n[profile c]\r\ncredential_process = p\r\n";
    assert_eq!(kind_in(&[crlf], "a"), ProfileKind::Sso);
    assert_eq!(kind_in(&[crlf], "b"), ProfileKind::AssumeRole);
    assert_eq!(kind_in(&[crlf], "c"), ProfileKind::CredentialProcess);
    let spaced = "  [profile\tt]  \n  sso_account_id   =  1\n";
    assert_eq!(kind_in(&[spaced], "t"), ProfileKind::Sso);
}

#[test]
fn a_section_is_matched_by_its_whole_name() {
    let text = "[profile work-sso]\nsso_session = s\n";
    assert_eq!(kind_in(&[text], "work"), ProfileKind::Missing);
    assert_eq!(kind_in(&[text], "work-sso"), ProfileKind::Sso);
    assert_eq!(kind_in(&[text], "WORK-SSO"), ProfileKind::Missing);
}

#[test]
fn the_keys_of_a_profile_come_from_both_files_and_stop_at_the_next_section() {
    let config = "[profile a]\nregion = us-east-1\n\n[profile b]\nsso_session = s\n";
    let credentials =
        "[a]\naws_access_key_id = AKIDNOTAKEY\naws_secret_access_key = not-a-secret\n";
    assert_eq!(
        kind_in(&[config, credentials], "a"),
        ProfileKind::StaticKeys
    );
    assert_eq!(kind_in(&[config, credentials], "b"), ProfileKind::Sso);
    // `a` has no key of its own in the config file: the SSO keys under [profile b] are b's.
    assert_eq!(kind_in(&[config], "a"), ProfileKind::Unknown);
}
