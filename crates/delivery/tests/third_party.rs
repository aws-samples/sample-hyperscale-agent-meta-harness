//! The third-party copy rule of the bundle validator: a file the run delivers, or
//! a file anywhere in the history it delivers, that equals a file the run stored through the research route ends the validation with a
//! violation that names the file, the origin and the licence. "Equals" is the same bytes or the same after line endings, trailing blanks
//! and the final newline are made alike. Stored files under 1 KiB, licence texts, ignore files, lockfiles and minified files do not
//! count. A run that stored nothing is not asked anything. A per-run setting lets the files through and reports them as findings.
#![allow(clippy::unwrap_used)]

mod common;
mod third_party_support;

use common::*;
use delivery::validator::{Rule, ThirdPartyMode, ValidateRequest};
use harness_protocol::research::{ItemKind, ResearchProvider, s3 as rs3};
use serde_json::json;
use third_party_support::*;

// ---- the request and the answer ---------------------------------------------------------------------------------------------

#[test]
fn the_setting_is_block_by_default_and_the_default_stays_off_the_wire() {
    let base = json!({"runId": RUN, "headSha": "a".repeat(40), "allowedGlobs": ["**"]});
    let r: ValidateRequest = serde_json::from_value(base.clone()).unwrap();
    assert_eq!(r.third_party, ThirdPartyMode::Block);
    assert!(
        serde_json::to_value(&r)
            .unwrap()
            .get("thirdParty")
            .is_none()
    );
    let mut allow = base.clone();
    allow["thirdParty"] = json!("allow");
    let r: ValidateRequest = serde_json::from_value(allow).unwrap();
    assert_eq!(r.third_party, ThirdPartyMode::Allow);
    assert_eq!(serde_json::to_value(&r).unwrap()["thirdParty"], "allow");
    let mut bad = base;
    bad["thirdParty"] = json!("maybe");
    assert!(serde_json::from_value::<ValidateRequest>(bad).is_err());
}

#[tokio::test]
async fn an_answer_without_findings_does_not_carry_the_member() {
    let w = world();
    let head = w.turn(&[("src/new.py", Some("x = 1\n"))]);
    let out = w.validate(&head).await;
    assert!(out.pass, "{:?}", out.violations);
    assert!(out.findings.is_empty());
    assert!(
        serde_json::to_value(&out)
            .unwrap()
            .get("findings")
            .is_none()
    );
}

// ---- the rule ---------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_delivered_copy_of_a_stored_file_ends_the_validation_and_names_the_file_the_origin_and_the_licence()
 {
    let w = world();
    let body = text(1200, "util");
    store_repo(&w.v.blobs, "i-0002", &[("lib/util.js", &body)]);
    let head = w.turn(&[("src/vendor/renamed.js", Some(body.as_str()))]);
    let out = w.validate(&head).await;
    assert!(!out.pass);
    assert_eq!(
        copies(&out),
        [(
            Rule::ThirdPartyCopy,
            Some("src/vendor/renamed.js".to_string())
        )]
    );
    let detail = &out
        .violations
        .iter()
        .find(|v| v.rule == Rule::ThirdPartyCopy)
        .unwrap()
        .detail;
    for part in ["lib/util.js", ORIGIN, "licence MIT", "i-0002"] {
        assert!(detail.contains(part), "{part} missing from {detail}");
    }
}

#[tokio::test]
async fn a_copy_of_a_stored_page_is_blocked_too() {
    let w = world();
    let body = text(1500, "guide page");
    store_page(&w.v.blobs, "i-0001", &body);
    let head = w.turn(&[("docs/guide.md", Some(body.as_str()))]);
    let out = w.validate(&head).await;
    assert_eq!(
        copies(&out),
        [(Rule::ThirdPartyCopy, Some("docs/guide.md".to_string()))]
    );
}

#[tokio::test]
async fn a_stored_file_of_1_kib_counts_and_one_byte_less_does_not() {
    let w = world();
    let edge = text(1024, "edge");
    let below = &edge[..1023];
    // `text` ends every line with a newline, so cut the stored copies to the exact sizes.
    let edge = format!("{}x", &edge[..1023]);
    assert_eq!(edge.len(), 1024);
    store_repo(
        &w.v.blobs,
        "i-0002",
        &[("a.js", edge.as_str()), ("b.js", below)],
    );
    let head = w.turn(&[("src/a.js", Some(edge.as_str())), ("src/b.js", Some(below))]);
    let out = w.validate(&head).await;
    assert_eq!(
        copies(&out),
        [(Rule::ThirdPartyCopy, Some("src/a.js".to_string()))]
    );
}

#[tokio::test]
async fn a_file_of_900_bytes_passes() {
    let w = world();
    let small = text(900, "small");
    store_repo(&w.v.blobs, "i-0002", &[("small.js", &small)]);
    let head = w.turn(&[("src/small.js", Some(small.as_str()))]);
    assert!(w.validate(&head).await.pass);
}

#[tokio::test]
async fn a_crlf_copy_and_a_copy_with_trailing_blanks_are_blocked() {
    let w = world();
    let body = text(1500, "variant");
    store_repo(&w.v.blobs, "i-0002", &[("lib/v.js", &body)]);
    let crlf = body.replace('\n', "\r\n");
    let padded = format!("{body}\n\n   \n");
    let head = w.turn(&[
        ("src/crlf.js", Some(crlf.as_str())),
        ("src/padded.js", Some(padded.as_str())),
    ]);
    let out = w.validate(&head).await;
    let mut got = copies(&out);
    got.sort();
    assert_eq!(
        got.iter()
            .map(|(_, p)| p.clone().unwrap())
            .collect::<Vec<_>>(),
        ["src/crlf.js", "src/padded.js"]
    );
    let detail = &out.violations[0].detail;
    assert!(
        detail.contains("line endings") || detail.contains("trailing blanks"),
        "{detail}"
    );
}

#[tokio::test]
async fn a_changed_copy_passes() {
    let w = world();
    let body = text(1500, "variant");
    store_repo(&w.v.blobs, "i-0002", &[("lib/v.js", &body)]);
    let changed = body.replacen("line 0003", "line 3 changed", 1);
    assert_ne!(changed, body);
    let head = w.turn(&[("src/v.js", Some(changed.as_str()))]);
    assert!(w.validate(&head).await.pass);
}

#[tokio::test]
async fn an_empty_file_a_licence_text_a_lockfile_and_a_minified_file_pass() {
    let w = world();
    let body = text(1500, "shared text");
    store_repo(&w.v.blobs, "i-0002", &[("lib/shared.js", &body)]);
    let head = w.turn(&[
        ("src/empty.txt", Some("")),
        ("LICENSE", Some(body.as_str())),
        ("package-lock.json", Some(body.as_str())),
        ("web/app.min.js", Some(body.as_str())),
        (".gitignore", Some(body.as_str())),
    ]);
    let out = w.validate(&head).await;
    assert!(out.pass, "{:?}", out.violations);
}

#[tokio::test]
async fn stored_files_with_exempt_names_do_not_count() {
    let w = world();
    let licence = text(3000, "licence text");
    let lock = text(5000, "lock");
    store_repo(
        &w.v.blobs,
        "i-0002",
        &[("LICENSE", &licence), ("package-lock.json", &lock)],
    );
    let head = w.turn(&[
        ("docs/licence-copy.txt", Some(licence.as_str())),
        ("docs/deps.txt", Some(lock.as_str())),
    ]);
    assert!(w.validate(&head).await.pass);
}

#[tokio::test]
async fn a_file_that_was_added_and_removed_in_the_history_is_still_blocked() {
    let w = world();
    let body = text(1500, "gone");
    store_repo(&w.v.blobs, "i-0002", &[("lib/gone.js", &body)]);
    commit(&w.work, &[("src/gone.js", Some(body.as_str()))], "add");
    commit(&w.work, &[("src/gone.js", None)], "remove");
    let head = w.stage();
    let out = w.validate(&head).await;
    assert_eq!(
        copies(&out),
        [(Rule::ThirdPartyCopy, Some("src/gone.js".to_string()))]
    );
    assert!(
        out.violations[0].detail.contains("history"),
        "{}",
        out.violations[0].detail
    );
}

#[tokio::test]
async fn a_file_the_base_already_held_is_not_the_runs_copy() {
    let body = text(1500, "preexisting");
    let w = world_with(&[("third_party/lib.js", body.as_str())]);
    store_repo(&w.v.blobs, "i-0002", &[("lib/lib.js", &body)]);
    let head = w.turn(&[("src/new.py", Some("y = 2\n"))]);
    assert!(w.validate(&head).await.pass);
}

#[tokio::test]
async fn a_run_that_stored_nothing_is_not_asked_anything() {
    let w = world();
    let head = w.turn(&[("src/new.py", Some(text(3000, "own code").as_str()))]);
    let out = w.validate(&head).await;
    assert!(out.pass);
    assert_eq!(w.research_reads(), 0);
}

#[tokio::test]
async fn items_of_another_run_are_not_read() {
    let w = world();
    let body = text(1500, "other");
    let other = "01K5ZQ4T8M3N6P7R9S0V1W2X3Z";
    let mut m = manifest("i-0002", ItemKind::Page, "body.txt", body.as_bytes());
    m.run = other.into();
    m.body.key = format!("{}items/i-0002/body.txt", rs3::prefix(other));
    w.v.blobs
        .insert(&rs3::manifest_key(other, "i-0002"), m.canonical_bytes());
    w.v.blobs.insert(&m.body.key, body.as_bytes().to_vec());
    let head = w.turn(&[("docs/guide.md", Some(body.as_str()))]);
    assert!(w.validate(&head).await.pass);
}

#[tokio::test]
async fn an_attachment_is_the_users_own_file_and_a_delivered_copy_of_it_passes() {
    // `ham --attach` files are the user's own material (provider `user`), not something the research route fetched from a third
    // party. Even if a manifest of that kind sits in the run's store, the rule indexes no file of it, so delivering the same bytes is fine.
    let w = world();
    let body = text(1500, "mine");
    let mut m = manifest("i-0003", ItemKind::Attachment, "body.bin", body.as_bytes());
    m.provider = ResearchProvider::User;
    m.origin = "attached by you: notes.md".into();
    w.v.blobs
        .insert(&rs3::manifest_key(RUN, "i-0003"), m.canonical_bytes());
    w.v.blobs.insert(&m.body.key, body.as_bytes().to_vec());
    let head = w.turn(&[("docs/notes.md", Some(body.as_str()))]);
    let out = w.validate(&head).await;
    assert!(out.pass, "{:?}", out.violations);
    assert!(out.findings.is_empty(), "{:?}", out.findings);
    // The twin: the same bytes stored as a page of a third party are blocked, so the pass above is the kind, not the bytes.
    let other = world();
    store_page(&other.v.blobs, "i-0002", &body);
    let head = other.turn(&[("docs/notes.md", Some(body.as_str()))]);
    assert!(!other.validate(&head).await.pass);
}

#[tokio::test]
async fn a_stored_body_that_is_not_what_its_manifest_says_cannot_be_checked_and_blocks() {
    let w = world();
    let body = text(1500, "page");
    store_page(&w.v.blobs, "i-0001", &body);
    w.v.blobs.insert(
        &format!("{}items/i-0001/body.txt", rs3::prefix(RUN)),
        b"changed".to_vec(),
    );
    let head = w.turn(&[("docs/own.md", Some("own words\n"))]);
    let out = w.validate(&head).await;
    assert!(!out.pass);
    let v = out
        .violations
        .iter()
        .find(|v| v.rule == Rule::ThirdPartyCopy)
        .unwrap();
    assert!(
        v.detail.contains("i-0001") && v.detail.contains("could not be checked"),
        "{}",
        v.detail
    );
}

#[tokio::test]
async fn the_per_run_setting_lets_a_copy_through_and_reports_it_as_a_finding() {
    let w = world();
    let body = text(1200, "util");
    store_repo(&w.v.blobs, "i-0002", &[("lib/util.js", &body)]);
    let head = w.turn(&[("src/vendor/util.js", Some(body.as_str()))]);
    let out =
        w.v.validate(&w.req(&head, ThirdPartyMode::Allow))
            .await
            .unwrap();
    assert!(out.pass, "{:?}", out.violations);
    assert_eq!(out.findings.len(), 1);
    assert_eq!(out.findings[0].rule, Rule::ThirdPartyCopy);
    assert_eq!(out.findings[0].path.as_deref(), Some("src/vendor/util.js"));
    assert_eq!(
        serde_json::to_value(&out).unwrap()["findings"][0]["rule"],
        "third-party-copy"
    );
}

#[tokio::test]
async fn every_other_rule_still_applies_beside_it() {
    let w = world();
    let body = text(1200, "util");
    store_repo(&w.v.blobs, "i-0002", &[("lib/util.js", &body)]);
    let head = w.turn(&[
        ("src/vendor/util.js", Some(body.as_str())),
        (".github/workflows/ci.yml", Some("name: ci\n")),
    ]);
    let out = w.validate(&head).await;
    let rules: Vec<Rule> = out.violations.iter().map(|v| v.rule).collect();
    assert!(rules.contains(&Rule::ThirdPartyCopy));
    assert!(rules.contains(&Rule::WorkflowFile));
}
