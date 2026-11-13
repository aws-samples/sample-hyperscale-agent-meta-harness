//! The request the direct driver fetches: `runs/<run>/input/request.json` is checked against the
//! SHA-256 of the invocation, parsed with unknown fields refused, and every rule file is checked again without `ham-core`.
//! A failure ends the run `stopped_setup_failed` before any model call.
#![allow(clippy::unwrap_used)]

use agentd::direct::request::{
    check_rules, parse_request, rule_refs, rules_digest, rules_manifest,
};
use harness_protocol::api::v1::{RuleFile, StartDirectRunRequest};
use harness_protocol::runtime::{RuleDelivery, RuleLayer};
use sha2::{Digest, Sha256};

fn sha(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

fn rule(name: &str, content: &str) -> RuleFile {
    RuleFile {
        path: name.into(),
        sha256: sha(content.as_bytes()),
        content: content.into(),
    }
}

fn request_json(rules: &[RuleFile]) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "prompt": "Add a percent_change(a, b) function with tests",
        "target": {"mode": "repo", "repo": "cc:hamh-e2e-python-calc"},
        "model": "us.anthropic.claude-opus-5-5",
        "rules": rules,
    }))
    .unwrap()
}

#[test]
fn a_request_with_the_right_hash_parses() {
    let bytes = request_json(&[rule("00-my-rules.md", "Always write the test first.\n")]);
    let req: StartDirectRunRequest = parse_request(&bytes, &sha(&bytes)).unwrap();
    assert_eq!(req.prompt, "Add a percent_change(a, b) function with tests");
    assert_eq!(req.rules.len(), 1);
}

#[test]
fn a_request_whose_bytes_hash_differently_is_refused() {
    let bytes = request_json(&[]);
    let err = parse_request(&bytes, &sha(b"something else")).unwrap_err();
    assert_eq!(err, "request.json sha256 differs");
}

#[test]
fn an_uppercase_expected_hash_is_still_a_mismatch_the_hash_is_lowercase_hex() {
    let bytes = request_json(&[]);
    assert!(parse_request(&bytes, &sha(&bytes).to_uppercase()).is_err());
}

#[test]
fn an_unknown_member_is_refused_and_the_error_quotes_no_input() {
    let bytes = serde_json::to_vec(&serde_json::json!({
        "prompt": "SECRET-PROMPT-TEXT",
        "target": {"mode": "s3-only"},
        "model": "m",
        "surprise": 1,
    }))
    .unwrap();
    let err = parse_request(&bytes, &sha(&bytes)).unwrap_err();
    assert!(
        err.starts_with("request.json is not a StartDirectRunRequest"),
        "{err}"
    );
    assert!(!err.contains("SECRET-PROMPT-TEXT"), "{err}");
}

#[test]
fn bytes_that_are_not_json_are_refused() {
    let bytes = b"not json at all".to_vec();
    assert!(parse_request(&bytes, &sha(&bytes)).is_err());
}

#[test]
fn a_rule_whose_hash_is_not_its_contents_is_refused() {
    let mut r = rule("00-my-rules.md", "text\n");
    r.sha256 = sha(b"other");
    let err = check_rules(&[r]).unwrap_err();
    assert!(
        err.contains("00-my-rules.md") && err.contains("sha256"),
        "{err}"
    );
}

#[test]
fn flat_ascii_markdown_names_are_accepted() {
    let rules: Vec<RuleFile> = ["00-my-rules.md", "a.md", "A_b-c.1.md", "x.md.md"]
        .iter()
        .map(|n| rule(n, "x\n"))
        .collect();
    check_rules(&rules).unwrap();
}

#[test]
fn bad_names_are_refused() {
    for name in [
        "../escape.md",
        "dir/file.md",
        "a b.md",
        ".hidden.md",
        "-dash.md",
        "notes.txt",
        "noext",
        ".md.txt",
        "\u{e9}.md",
        &format!("{}.md", "a".repeat(253)),
        "",
        "AGENTS.md",
        "agents.md",
        "Agents.MD",
        "back\\slash.md",
    ] {
        assert!(
            check_rules(&[rule(name, "x\n")]).is_err(),
            "{name:?} was accepted"
        );
    }
}

#[test]
fn the_longest_name_is_255_bytes() {
    let ok = format!("{}.md", "a".repeat(252));
    assert_eq!(ok.len(), 255);
    check_rules(&[rule(&ok, "x\n")]).unwrap();
    let too_long = format!("{}.md", "a".repeat(253));
    assert!(check_rules(&[rule(&too_long, "x\n")]).is_err());
}

#[test]
fn at_most_100_files() {
    let ok: Vec<RuleFile> = (0..100)
        .map(|i| rule(&format!("r{i:03}.md"), "x\n"))
        .collect();
    check_rules(&ok).unwrap();
    let too_many: Vec<RuleFile> = (0..101)
        .map(|i| rule(&format!("r{i:03}.md"), "x\n"))
        .collect();
    let err = check_rules(&too_many).unwrap_err();
    assert!(err.contains("100"), "{err}");
}

#[test]
fn a_file_has_no_size_of_its_own() {
    // The old limit was 64 KiB a file; the request as a whole was held to its bound when it was stored.
    let over = "a".repeat(64 * 1024 + 1);
    check_rules(&[rule("big.md", &over)]).unwrap();
    check_rules(&[rule("bigger.md", &"a".repeat(3 * 1024 * 1024))]).unwrap();
    let mut wrong = rule("big.md", &over);
    wrong.sha256 = "0".repeat(64);
    assert!(check_rules(&[wrong]).unwrap_err().contains("big.md"));
}

#[test]
fn two_names_that_differ_only_in_case_are_refused_the_mac_folds_them() {
    assert!(check_rules(&[rule("A.md", "x\n"), rule("a.md", "x\n")]).is_err());
    assert!(check_rules(&[rule("a.md", "x\n"), rule("a.md", "y\n")]).is_err());
}

#[test]
fn no_rules_is_fine() {
    check_rules(&[]).unwrap();
}

#[test]
fn the_manifest_lists_each_rule_as_a_tenant_native_rule_and_the_digest_is_over_its_json() {
    let rules = vec![rule("00-my-rules.md", "one\n"), rule("10-more.md", "two\n")];
    let m = rules_manifest(&rules);
    assert_eq!(m.entries.len(), 2);
    for (e, r) in m.entries.iter().zip(&rules) {
        assert_eq!(e.layer, RuleLayer::Tenant);
        assert_eq!(e.delivery, RuleDelivery::Native);
        assert_eq!(e.source, r.path);
        assert_eq!(e.sha256, r.sha256);
    }
    assert_eq!(m.compiled_sha256, None);
    assert_eq!(rules_digest(&m), sha(&serde_json::to_vec(&m).unwrap()));
}

#[test]
fn the_summary_refs_carry_name_hash_and_byte_count() {
    let refs = rule_refs(&[rule("00-my-rules.md", "héllo\n")]);
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].name, "00-my-rules.md");
    assert_eq!(refs[0].sha256, sha("héllo\n".as_bytes()));
    assert_eq!(refs[0].bytes, Some("héllo\n".len() as u64));
}
