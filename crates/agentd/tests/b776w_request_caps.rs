//! Wave 2, row C28: agentd holds no size or count of its own for the rule files of a request. The bounds are the protocol's one definition
//! (the one `ham` and control-api use), a rule file has no size of its own, and the whole request is bounded once, by its body.
#![allow(clippy::unwrap_used)]

use agentd::direct::request::{MAX_RULE_FILES, MAX_RULE_NAME_BYTES, check_rules, sha256_hex};
use harness_protocol::api::v1::RuleFile;

fn rule(name: &str, content: &str) -> RuleFile {
    RuleFile {
        path: name.to_string(),
        sha256: sha256_hex(content.as_bytes()),
        content: content.to_string(),
    }
}

#[test]
fn b776w_c28_the_bounds_are_the_protocols_own_definition() {
    assert_eq!(MAX_RULE_FILES, harness_protocol::direct::MAX_RULE_FILES);
    assert_eq!(
        MAX_RULE_NAME_BYTES,
        harness_protocol::direct::MAX_RULE_NAME_BYTES
    );
    assert_eq!(MAX_RULE_FILES, 100);
}

#[test]
fn b776w_c28_a_rule_file_of_three_mib_and_a_hundred_files_pass() {
    let big = "x".repeat(3 * 1024 * 1024);
    let mut rules = vec![rule("big.md", &big)];
    for i in 1..MAX_RULE_FILES {
        rules.push(rule(&format!("r{i}.md"), "a rule"));
    }
    assert_eq!(rules.len(), 100);
    assert_eq!(check_rules(&rules), Ok(()));
}

#[test]
fn b776w_c28_a_hundred_and_first_file_is_refused_with_the_count_in_the_words() {
    let rules: Vec<RuleFile> = (0..=MAX_RULE_FILES)
        .map(|i| rule(&format!("r{i}.md"), "a rule"))
        .collect();
    let e = check_rules(&rules).unwrap_err();
    assert!(e.contains("101 files") && e.contains("limit is 100"), "{e}");
}

#[test]
fn b776w_c28_a_name_of_two_hundred_and_fifty_five_bytes_passes_and_one_more_does_not() {
    let ok = format!("{}.md", "a".repeat(MAX_RULE_NAME_BYTES - 3));
    assert_eq!(ok.len(), MAX_RULE_NAME_BYTES);
    assert_eq!(check_rules(&[rule(&ok, "a rule")]), Ok(()));
    let over = format!("{}.md", "a".repeat(MAX_RULE_NAME_BYTES - 2));
    assert!(check_rules(&[rule(&over, "a rule")]).is_err());
}
