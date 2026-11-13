//! The CodeCommit git-over-HTTPS credential: `repo-gateway` signs git with its
//! Lambda role credentials in the scheme of the AWS CLI credential helper and
//! `git-remote-codecommit`; no stored secret exists.
//!
//! The scheme (git-remote-codecommit 1.17 `sign()`): the
//! canonical request is `GIT\n<path>\n\nhost:<host>\n\nhost\n` (method `GIT`, empty query, one
//! signed header, empty payload hash); the string to sign, signing key and signature are SigV4
//! for service `codecommit`; the password is `<timestamp>Z<signature>` and the username is the
//! access key id, plus `%<session token>` for temporary credentials.
//!
//! Known vector: computed with botocore 1.43.102
//! (`SigV4Auth.string_to_sign` and `.signature` over git-remote-codecommit's canonical request,
//! `request.context['timestamp'] = "20260925T120000"`), and again with Python `hmac` and `hashlib`
//! alone; both gave the password below. The key values are test strings, not credentials.
#![allow(clippy::unwrap_used)] // test assertions unwrap by design

use delivery::credential::{
    AwsKeys, canonical_request, codecommit_host, git_credential, repo_path, repo_url,
    string_to_sign, timestamp, yyyymmdd,
};

const ACCESS_KEY: &str = "AKIDEXAMPLE";
const SECRET: &str = "test-secret-for-the-codecommit-git-vector";
const TOKEN: &str = "test-session-token";
const REPO: &str = "hamh-e2e-python-calc-20260925";
/// 2026-09-25T12:00:00Z.
const AT: u64 = 1_790_337_600;
const PASSWORD: &str =
    "20260925T120000Z21a37a18a32361cd4287d1c2804cbc80faca3de60d1d7248eeb25f18a53ab0fe";

fn keys(token: Option<&str>) -> AwsKeys {
    AwsKeys {
        access_key_id: ACCESS_KEY.into(),
        secret_access_key: SECRET.into(),
        session_token: token.map(str::to_string),
    }
}

#[test]
fn the_timestamp_is_the_sigv4_basic_format_in_utc() {
    assert_eq!(timestamp(AT), "20260925T120000");
    assert_eq!(timestamp(0), "19700101T000000");
    assert_eq!(timestamp(951_782_400), "20000229T000000");
    assert_eq!(yyyymmdd(AT), "20260925");
}

#[test]
fn the_host_path_and_url_are_the_codecommit_git_endpoint() {
    assert_eq!(
        codecommit_host("us-west-2"),
        "git-codecommit.us-west-2.amazonaws.com"
    );
    assert_eq!(repo_path(REPO), format!("/v1/repos/{REPO}"));
    assert_eq!(
        repo_url("us-west-2", REPO),
        format!("https://git-codecommit.us-west-2.amazonaws.com/v1/repos/{REPO}")
    );
}

#[test]
fn the_canonical_request_and_string_to_sign_match_git_remote_codecommit() {
    let host = codecommit_host("us-west-2");
    let path = repo_path(REPO);
    assert_eq!(
        canonical_request(&host, &path),
        format!("GIT\n{path}\n\nhost:{host}\n\nhost\n")
    );
    assert_eq!(
        string_to_sign(
            "20260925T120000",
            "us-west-2",
            &canonical_request(&host, &path)
        ),
        "AWS4-HMAC-SHA256\n20260925T120000\n20260925/us-west-2/codecommit/aws4_request\n\
         1c3529df60d7071a586fcd259864adf73220aaae8881b01bbdc3a6ed33adfa85"
    );
}

#[test]
fn the_password_matches_the_botocore_vector() {
    let c = git_credential(&keys(Some(TOKEN)), "us-west-2", REPO, AT);
    assert_eq!(c.password, PASSWORD);
    assert_eq!(c.username, format!("{ACCESS_KEY}%{TOKEN}"));
}

#[test]
fn long_lived_keys_sign_the_same_and_the_username_has_no_token() {
    // The session token is not part of the signature, only of the username.
    let c = git_credential(&keys(None), "us-west-2", REPO, AT);
    assert_eq!(c.username, ACCESS_KEY);
    assert_eq!(c.password, PASSWORD);
}

#[test]
fn the_signature_changes_with_the_repository_region_and_time() {
    let base = git_credential(&keys(Some(TOKEN)), "us-west-2", REPO, AT).password;
    assert_ne!(
        git_credential(&keys(Some(TOKEN)), "us-west-2", "hamh-e2e-other", AT).password,
        base
    );
    assert_ne!(
        git_credential(&keys(Some(TOKEN)), "us-east-1", REPO, AT).password,
        base
    );
    let later = git_credential(&keys(Some(TOKEN)), "us-west-2", REPO, AT + 1).password;
    assert!(later.starts_with("20260925T120001Z"));
    assert_ne!(later, base);
}

#[test]
fn debug_output_never_shows_the_secret_token_or_password() {
    let k = keys(Some(TOKEN));
    let c = git_credential(&k, "us-west-2", REPO, AT);
    for shown in [format!("{k:?}"), format!("{c:?}")] {
        assert!(!shown.contains(SECRET), "{shown}");
        assert!(!shown.contains(TOKEN), "{shown}");
        assert!(!shown.contains(&PASSWORD[17..]), "{shown}");
    }
}
