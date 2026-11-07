//! Start-request members of protocol 1.38.0 added by package REQ1: `goal`, `thirdParty`
//! and `attachments`. Each is additive and left off the wire at its default, so a request that keeps the defaults is byte for
//! byte the request of an older client; the request type refuses unknown members, so a member must exist to be accepted at all.

#![allow(clippy::unwrap_used)]

use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::direct::{
    Attachment, AttachmentProblem, GoalOptions, MAX_ATTACHMENT_NAME_BYTES, check_attachments,
};
use harness_protocol::research::ThirdPartyMode;
use serde_json::{Value, json};

fn base() -> Value {
    json!({
        "prompt": "add a function", "target": {"mode": "s3-only"}, "model": "us.openai.gpt-6.1-sol", "rules": []
    })
}

fn parse(extra: Value) -> Result<StartDirectRunRequest, serde_json::Error> {
    let mut v = base();
    for (k, val) in extra.as_object().unwrap() {
        v[k] = val.clone();
    }
    serde_json::from_value(v)
}

fn problem(items: &[Attachment]) -> AttachmentProblem {
    check_attachments(items).unwrap_err().problem
}

fn text_of(bytes: usize) -> String {
    "a".repeat(bytes)
}

// ---- goal ----------------------------------------------------------------------------------------------------------------------

#[test]
fn the_goal_member_is_optional_and_left_off_the_wire_when_absent() {
    let plain = parse(json!({})).unwrap();
    assert_eq!(plain.goal, None);
    assert!(serde_json::to_value(&plain).unwrap().get("goal").is_none());
}

#[test]
fn goal_off_round_trips_as_an_enabled_switch() {
    let off = parse(json!({"goal": {"enabled": false}})).unwrap();
    assert_eq!(off.goal, Some(GoalOptions::switch(false)));
    assert_eq!(
        serde_json::to_value(&off).unwrap()["goal"],
        json!({"enabled": false})
    );
}

#[test]
fn a_goal_member_with_an_unknown_key_or_a_non_boolean_is_refused() {
    assert!(parse(json!({"goal": {"enabled": false, "rounds": 3}})).is_err());
    assert!(parse(json!({"goal": {"enabled": "off"}})).is_err());
    assert!(parse(json!({"goal": "off"})).is_err());
    assert!(parse(json!({"goal": {}})).is_err());
}

// ---- thirdParty ------------------------------------------------------------------------------------------------------------------

#[test]
fn the_third_party_member_is_block_or_allow_and_absent_means_block() {
    let plain = parse(json!({})).unwrap();
    assert_eq!(plain.third_party, None);
    assert!(
        serde_json::to_value(&plain)
            .unwrap()
            .get("thirdParty")
            .is_none()
    );
    let allow = parse(json!({"thirdParty": "allow"})).unwrap();
    assert_eq!(allow.third_party, Some(ThirdPartyMode::Allow));
    assert_eq!(
        serde_json::to_value(&allow).unwrap()["thirdParty"],
        json!("allow")
    );
    let block = parse(json!({"thirdParty": "block"})).unwrap();
    assert_eq!(block.third_party, Some(ThirdPartyMode::Block));
    assert!(ThirdPartyMode::default().is_block());
    assert!(!ThirdPartyMode::Allow.is_block());
}

#[test]
fn any_other_third_party_word_is_refused() {
    for bad in [
        json!("maybe"),
        json!("Allow"),
        json!(true),
        json!({"mode": "allow"}),
    ] {
        assert!(parse(json!({"thirdParty": bad})).is_err());
    }
}

// ---- attachments -----------------------------------------------------------------------------------------------------------------

#[test]
fn the_attachments_member_is_left_off_the_wire_when_empty() {
    let plain = parse(json!({})).unwrap();
    assert!(plain.attachments.is_empty());
    assert!(
        serde_json::to_value(&plain)
            .unwrap()
            .get("attachments")
            .is_none()
    );
}

#[test]
fn an_attachment_round_trips_with_its_size_and_sha256() {
    let a = Attachment::new("notes.md", "the API takes a page token\n");
    assert_eq!(a.size, 27);
    let wire = serde_json::to_value(&a).unwrap();
    assert_eq!(wire["name"], "notes.md");
    assert_eq!(wire["size"], 27);
    assert_eq!(wire["sha256"].as_str().unwrap().len(), 64);
    let req = parse(json!({"attachments": [wire.clone()]})).unwrap();
    assert_eq!(req.attachments, vec![a]);
    assert_eq!(
        serde_json::to_value(&req).unwrap()["attachments"],
        json!([wire])
    );
}

#[test]
fn an_attachment_with_an_unknown_key_is_refused() {
    let mut wire = serde_json::to_value(Attachment::new("a.txt", "x")).unwrap();
    wire["path"] = json!("../../etc/passwd");
    assert!(parse(json!({"attachments": [wire]})).is_err());
}

#[test]
fn the_name_limit_is_what_a_file_system_takes() {
    assert_eq!(MAX_ATTACHMENT_NAME_BYTES, 255);
}

#[test]
fn good_attachments_pass_and_so_does_the_empty_set() {
    assert!(check_attachments(&[]).is_ok());
    let items = vec![
        Attachment::new("api-notes.md", "# Notes\nline\n"),
        Attachment::new("DATA_v2.json", "{\"a\": 1}"),
        Attachment::new("table.csv", "a,b\n1,2\n"),
    ];
    assert!(check_attachments(&items).is_ok());
}

#[test]
fn the_validator_judges_no_size_and_no_count() {
    // The old limits were 256 KiB an item, 2 MiB in all and 32 items; the request as a whole is held to its own bound instead.
    assert!(check_attachments(&[Attachment::new("big.txt", &text_of(256 * 1024 + 1))]).is_ok());
    let many: Vec<Attachment> = (0..200)
        .map(|i| Attachment::new(&format!("f{i}.txt"), &text_of(64 * 1024)))
        .collect();
    assert!(check_attachments(&many).is_ok());
}

#[test]
fn a_name_must_be_a_plain_file_name() {
    let long = format!("{}.md", "n".repeat(MAX_ATTACHMENT_NAME_BYTES));
    for bad in [
        "",
        ".",
        "..",
        ".hidden",
        "a/b.md",
        "..\\x.md",
        "a\\b",
        "my notes.md",
        "tab\there.md",
        "caf\u{e9}.md",
        "nul\0.md",
        "-",
        &long,
        "a:b",
        "con*",
    ] {
        assert_eq!(
            problem(&[Attachment::new(bad, "x")]),
            AttachmentProblem::BadName,
            "name {bad:?}"
        );
    }
    // The longest allowed name is exactly the limit.
    let edge = "n".repeat(MAX_ATTACHMENT_NAME_BYTES);
    assert!(check_attachments(&[Attachment::new(&edge, "x")]).is_ok());
}

#[test]
fn two_names_that_differ_only_in_case_are_one_name() {
    assert_eq!(
        problem(&[
            Attachment::new("Notes.md", "a"),
            Attachment::new("notes.MD", "b")
        ]),
        AttachmentProblem::DuplicateName
    );
}

#[test]
fn the_names_of_the_index_files_are_reserved() {
    // agentd writes INDEX.md and INDEX.json beside the files; an attachment of that name would collide with them (names compare without case).
    for name in ["INDEX.md", "INDEX.json", "index.MD", "Index.Json"] {
        assert_eq!(
            problem(&[Attachment::new(name, "x")]),
            AttachmentProblem::ReservedName,
            "{name}"
        );
    }
    assert!(
        check_attachments(&[
            Attachment::new("INDEX.txt", "x"),
            Attachment::new("my-index.md", "y"),
            Attachment::new("INDEX", "z"),
        ])
        .is_ok()
    );
}

#[test]
fn the_size_and_the_sha256_must_describe_the_content() {
    let mut a = Attachment::new("a.txt", "hello");
    a.size = 4;
    assert_eq!(problem(&[a]), AttachmentProblem::SizeMismatch);
    let mut b = Attachment::new("b.txt", "hello");
    b.sha256 = "0".repeat(64);
    assert_eq!(problem(&[b]), AttachmentProblem::ShaMismatch);
    let mut c = Attachment::new("c.txt", "hello");
    c.sha256 = c.sha256.to_uppercase();
    assert_eq!(problem(&[c]), AttachmentProblem::ShaMismatch);
}

#[test]
fn a_nul_byte_and_an_empty_file_are_refused() {
    assert_eq!(
        problem(&[Attachment::new("n.txt", "a\0b")]),
        AttachmentProblem::Nul
    );
    assert_eq!(
        problem(&[Attachment::new("e.txt", "")]),
        AttachmentProblem::Empty
    );
}

#[test]
fn the_error_names_the_item_and_never_holds_its_content() {
    let secret_looking = "plain words in a document body";
    let mut a = Attachment::new("a.txt", secret_looking);
    a.size = 1;
    let e = check_attachments(&[a]).unwrap_err();
    let text = e.to_string();
    assert!(text.contains("a.txt"), "{text}");
    assert!(!text.contains(secret_looking), "{text}");
    // A hostile name is cut and made printable.
    let e = check_attachments(&[Attachment::new("\u{1b}[31mred\u{7}.md", "x")]).unwrap_err();
    assert!(e.to_string().chars().all(|c| !c.is_control()), "{e}");
}

#[test]
fn every_problem_has_a_stable_code() {
    let codes: Vec<&str> = [
        AttachmentProblem::BadName,
        AttachmentProblem::ReservedName,
        AttachmentProblem::DuplicateName,
        AttachmentProblem::Empty,
        AttachmentProblem::TooLarge { bytes: 1, max: 1 },
        AttachmentProblem::TooLargeTotal { bytes: 1, max: 1 },
        AttachmentProblem::SizeMismatch,
        AttachmentProblem::ShaMismatch,
        AttachmentProblem::Nul,
    ]
    .iter()
    .map(AttachmentProblem::code)
    .collect();
    let mut sorted = codes.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), codes.len(), "codes repeat: {codes:?}");
}
