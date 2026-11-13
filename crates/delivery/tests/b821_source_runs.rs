//! The copy rule reads the stored items of every run whose commits it judges.
//!
//! A follow-up of a run that was not delivered is judged from the base that run started from (run-session picks the base), so the
//! commits of the earlier run are in the judged range. The copy rule compared the delivered files with the items the validated run
//! itself stored. A follow-up has its own run id and inherits no stored item, so a follow-up of a run that was blocked for a
//! third-party copy delivered the copy. run-session now names the runs whose commits are in the judged range (`sourceRunIds`), and the
//! rule reads their items too. A request without the member reads the validated run alone, which is what an earlier release sends.
//!
//! The requests are built as JSON, the way run-session sends them.
#![allow(clippy::unwrap_used)] // test assertions unwrap by design

mod common;
mod third_party_support;

use common::*;
use delivery::lambda::validator_event;
use delivery::validator::{Rule, ValidateOutput, ValidateRequest, Violation};
use harness_protocol::research::s3 as rs3;
use harness_protocol::store::s3::verified_bundle_key;
use serde_json::{Value, json};
use third_party_support::*;

/// The follow-up of `RUN`: the run that is validated.
const FOLLOW_UP: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X5B";
/// Where the first run put its copy of the stored file.
const COPY: &str = "src/vendor/util.js";

/// The request of the follow-up, judged from the base the first run started from, with the members of `extra`.
fn request(w: &World, head: &str, extra: Value) -> ValidateRequest {
    let mut v = json!({
        "runId": FOLLOW_UP, "headSha": head, "baseSha": w.base,
        "allowedGlobs": ["**", "!.git/**"], "engineConfig": "executable-only",
    });
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    serde_json::from_value(v).unwrap()
}

async fn validate(w: &World, head: &str, extra: Value) -> ValidateOutput {
    w.v.validate(&request(w, head, extra)).await.unwrap()
}

/// One more commit on the head the first run left, and the follow-up's own verified bundle in the bucket.
fn follow_up_head(w: &World, files: &[(&str, Option<&str>)]) -> String {
    commit(&w.work, files, "follow-up turn");
    let f = w.root.join("follow-up.bundle");
    let head = verified_bundle(&w.work, FOLLOW_UP, &f);
    w.v.blobs
        .insert(&verified_bundle_key(FOLLOW_UP), std::fs::read(&f).unwrap());
    head
}

/// The first run (`RUN`) stores a repository item and commits a copy of one of its files, and the validator refuses its head. The
/// follow-up adds a commit of its own. Returns the follow-up's head.
async fn follow_up_of_a_run_blocked_for_a_copy(w: &World) -> String {
    let body = text(1200, "util");
    store_repo(&w.v.blobs, "i-0002", &[("lib/util.js", &body)]);
    let refused_head = w.turn(&[(COPY, Some(body.as_str()))]);
    let refused = w.validate(&refused_head).await;
    assert_eq!(
        copies(&refused),
        [(Rule::ThirdPartyCopy, Some(COPY.to_string()))],
        "the first run is blocked for the copy"
    );
    follow_up_head(w, &[("README.md", Some("# calc\n"))])
}

fn copy_violations(list: &[Violation]) -> Vec<&Violation> {
    list.iter()
        .filter(|v| v.rule == Rule::ThirdPartyCopy)
        .collect()
}

#[tokio::test]
async fn b821_a_follow_up_of_a_run_blocked_for_a_third_party_copy_is_refused_for_the_copy() {
    let w = world();
    let head = follow_up_of_a_run_blocked_for_a_copy(&w).await;
    for target in [None, Some("zip")] {
        let mut extra = json!({"sourceRunIds": [RUN]});
        if let Some(t) = target {
            extra["target"] = json!(t);
        }
        let out = validate(&w, &head, extra).await;
        assert!(!out.pass, "{target:?}: the copy does not pass");
        let found = copy_violations(&out.violations);
        assert_eq!(found.len(), 1, "{target:?}: {:?}", out.violations);
        assert_eq!(found[0].path.as_deref(), Some(COPY), "{target:?}");
        // The item is the first run's: the detail says whose it is.
        assert!(
            found[0]
                .detail
                .contains(&format!("(item i-0002 of run {RUN}, licence MIT)")),
            "{target:?}: {}",
            found[0].detail
        );
        assert!(out.findings.is_empty(), "{target:?}: {:?}", out.findings);
    }
}

/// What a caller of an earlier release sends: no member. The rule reads the items of the validated run alone, as it did.
#[tokio::test]
async fn b821_a_request_without_the_member_reads_the_items_of_the_validated_run_alone() {
    let w = world();
    let head = follow_up_of_a_run_blocked_for_a_copy(&w).await;
    let out = validate(&w, &head, json!({})).await;
    assert!(out.pass, "{:?}", out.violations);
    assert!(out.findings.is_empty(), "{:?}", out.findings);
    // An empty list is the default and stays off the wire.
    let sent = serde_json::to_value(request(&w, &head, json!({}))).unwrap();
    assert!(sent.get("sourceRunIds").is_none(), "{sent}");
}

#[tokio::test]
async fn b821_a_follow_up_that_allows_third_party_files_reports_the_copy_of_the_source_run() {
    let w = world();
    let head = follow_up_of_a_run_blocked_for_a_copy(&w).await;
    let out = validate(
        &w,
        &head,
        json!({"sourceRunIds": [RUN], "thirdParty": "allow"}),
    )
    .await;
    assert!(out.pass, "{:?}", out.violations);
    let found = copy_violations(&out.findings);
    assert_eq!(found.len(), 1, "{:?}", out.findings);
    assert_eq!(found[0].path.as_deref(), Some(COPY));
}

/// Item ids count from one in every run, so a follow-up and its source both have an `i-0001`. Each is read from its own run.
#[tokio::test]
async fn b821_items_with_one_id_in_the_follow_up_and_in_its_source_are_both_read() {
    let w = world();
    let first = text(1200, "first");
    let second = text(1400, "second");
    store_repo(&w.v.blobs, "i-0001", &[("lib/first.js", &first)]);
    store_repo_of(
        &w.v.blobs,
        FOLLOW_UP,
        "i-0001",
        &[("lib/second.js", &second)],
    );
    w.turn(&[("src/first.js", Some(first.as_str()))]);
    let head = follow_up_head(&w, &[("src/second.js", Some(second.as_str()))]);
    let out = validate(&w, &head, json!({"sourceRunIds": [RUN]})).await;
    let mut found = copy_violations(&out.violations);
    found.sort_by(|a, b| a.path.cmp(&b.path));
    assert_eq!(
        found.iter().map(|v| v.path.as_deref()).collect::<Vec<_>>(),
        [Some("src/first.js"), Some("src/second.js")],
        "{:?}",
        out.violations
    );
    assert!(
        found[0].detail.contains(&format!(
            "lib/first.js of {ORIGIN} (item i-0001 of run {RUN},"
        )),
        "{}",
        found[0].detail
    );
    // The validated run's own item reads as it always did.
    assert!(
        found[1].detail.contains(&format!(
            "lib/second.js of {ORIGIN} (item i-0001, licence MIT)"
        )),
        "{}",
        found[1].detail
    );
}

/// The validated run named among its sources, or a source named twice, is read once.
#[tokio::test]
async fn b821_a_run_named_twice_is_read_once() {
    let w = world();
    let head = follow_up_of_a_run_blocked_for_a_copy(&w).await;
    let out = validate(&w, &head, json!({"sourceRunIds": [RUN, FOLLOW_UP, RUN]})).await;
    assert_eq!(
        copy_violations(&out.violations).len(),
        1,
        "{:?}",
        out.violations
    );
    let manifest_reads =
        w.v.blobs
            .reads
            .lock()
            .unwrap()
            .iter()
            .filter(|k| **k == rs3::manifest_key(RUN, "i-0002"))
            .count();
    // Once by the first run's own validation, once by the follow-up's.
    assert_eq!(manifest_reads, 2);
}

#[tokio::test]
async fn b821_a_source_item_that_cannot_be_read_blocks_the_follow_up() {
    let w = world();
    let head = follow_up_of_a_run_blocked_for_a_copy(&w).await;
    w.v.blobs.deny(&rs3::manifest_key(RUN, "i-0002"));
    let out = validate(&w, &head, json!({"sourceRunIds": [RUN]})).await;
    assert!(!out.pass);
    let found = copy_violations(&out.violations);
    assert_eq!(found.len(), 1, "{:?}", out.violations);
    assert!(
        found[0].detail.contains("could not be checked")
            && found[0]
                .detail
                .contains(&format!("item i-0002 of run {RUN}")),
        "{}",
        found[0].detail
    );
}

#[tokio::test]
async fn b821_a_listing_of_a_source_run_that_is_refused_is_access_denied_and_never_a_pass() {
    let w = world();
    let head = follow_up_of_a_run_blocked_for_a_copy(&w).await;
    w.v.blobs.deny(&rs3::prefix(RUN));
    let err =
        w.v.validate(&request(&w, &head, json!({"sourceRunIds": [RUN]})))
            .await
            .expect_err("a listing that S3 refuses is not a source run with no stored items");
    assert_eq!(err.error_type(), "AccessDenied");
    let said = err.to_string();
    assert!(
        said.contains("s3 list") && said.contains(&format!("{}items/", rs3::prefix(RUN))),
        "{said}"
    );
}

#[tokio::test]
async fn b821_a_source_run_list_that_is_not_a_short_list_of_run_ids_fails_the_request() {
    let w = world();
    let head = w.turn(&[("docs/own.md", Some("own words\n"))]);
    let many: Vec<String> = (0..33)
        .map(|i| format!("01K5ZQ4T8M3N6P7R9S0V1W2X{i:02}"))
        .collect();
    for bad in [
        json!(["not-a-run"]),
        json!([RUN, "../other"]),
        json!([""]),
        json!(RUN),
        json!([1]),
        json!(null),
        json!({"runId": RUN}),
        json!(many),
    ] {
        let input = json!({
            "runId": RUN, "headSha": head, "baseSha": w.base,
            "allowedGlobs": ["**", "!.git/**"], "sourceRunIds": bad,
        });
        let e = validator_event(&w.v, input).await.unwrap_err();
        assert_eq!(e.error_type(), "Invalid", "{bad}: {e}");
    }
    // The most the walk of run-session can name is accepted.
    let input = json!({
        "runId": RUN, "headSha": head, "baseSha": w.base,
        "allowedGlobs": ["**", "!.git/**"], "sourceRunIds": many[..32],
    });
    let out = validator_event(&w.v, input).await.unwrap();
    assert_eq!(out["pass"], true, "{out}");
}
