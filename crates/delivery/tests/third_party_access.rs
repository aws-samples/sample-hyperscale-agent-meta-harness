//! A research item the validator may not list or read is an error the run shows, never a rule that quietly did not apply.
//! On a stage whose bundle-validator role lacked `s3:ListBucket` and `s3:GetObject` under the research items, S3
//! answered AccessDenied to the listing of every run; the answer must reach the run machine as the error `AccessDenied` with the
//! operation and the key in its message, and an item whose manifest or body cannot be read must block the delivery (or be a finding
//! when the request allows third-party files), not pass.
#![allow(clippy::unwrap_used)]

mod common;
mod third_party_support;

use common::*;
use delivery::validator::{Rule, ThirdPartyMode, ValidateOutput, Violation};
use harness_protocol::research::s3 as rs3;
use third_party_support::*;

fn unreadable(violations: &[Violation], what: &str) {
    let v = violations
        .iter()
        .find(|v| v.rule == Rule::ThirdPartyCopy)
        .expect("the unreadable item is reported");
    assert!(
        v.detail.contains("could not be checked") && v.detail.contains(what),
        "{}",
        v.detail
    );
}

#[tokio::test]
async fn a_listing_the_role_may_not_make_is_access_denied_naming_the_listing_and_never_a_pass() {
    let w = world();
    w.v.blobs.deny(&rs3::prefix(RUN));
    let head = w.turn(&[("docs/own.md", Some("own words\n"))]);
    let err =
        w.v.validate(&w.req(&head, ThirdPartyMode::Block))
            .await
            .expect_err("a listing that S3 refuses is not a run with no stored items");
    assert_eq!(err.error_type(), "AccessDenied");
    let said = err.to_string();
    assert!(
        said.contains("s3 list") && said.contains(&format!("{}items/", rs3::prefix(RUN))),
        "{said}"
    );
}

#[tokio::test]
async fn the_listing_is_made_for_a_run_that_stored_nothing_too() {
    // The listing comes first, whatever the run stored: a missing grant blocks every delivery, so it must show at once.
    let w = world();
    w.v.blobs.deny(&rs3::prefix(RUN));
    let head = w.turn(&[("docs/own.md", Some("own words\n"))]);
    assert!(w.v.blobs.keys().iter().all(|k| !k.contains("/research/")));
    let err =
        w.v.validate(&w.req(&head, ThirdPartyMode::Allow))
            .await
            .expect_err("the listing is refused");
    assert_eq!(err.error_type(), "AccessDenied");
}

#[tokio::test]
async fn an_item_whose_manifest_may_not_be_read_blocks_the_run_and_says_so() {
    let w = world();
    store_page(&w.v.blobs, "i-0001", &text(1500, "page"));
    w.v.blobs.deny(&rs3::manifest_key(RUN, "i-0001"));
    let head = w.turn(&[("docs/own.md", Some("own words\n"))]);
    let out: ValidateOutput = w.validate(&head).await;
    assert!(!out.pass);
    unreadable(&out.violations, "its manifest could not be read");
}

#[tokio::test]
async fn an_item_whose_body_may_not_be_read_blocks_the_run_and_says_so() {
    let w = world();
    store_page(&w.v.blobs, "i-0001", &text(1500, "page"));
    w.v.blobs
        .deny(&format!("{}items/i-0001/body", rs3::prefix(RUN)));
    let head = w.turn(&[("docs/own.md", Some("own words\n"))]);
    let out = w.validate(&head).await;
    assert!(!out.pass);
    unreadable(&out.violations, "its body could not be read");
}

#[tokio::test]
async fn an_unreadable_item_is_a_finding_when_third_party_files_are_allowed_never_dropped() {
    let w = world();
    store_page(&w.v.blobs, "i-0001", &text(1500, "page"));
    w.v.blobs.deny(&rs3::manifest_key(RUN, "i-0001"));
    let head = w.turn(&[("docs/own.md", Some("own words\n"))]);
    let out =
        w.v.validate(&w.req(&head, ThirdPartyMode::Allow))
            .await
            .unwrap();
    assert!(out.pass, "allowed third-party files do not block");
    unreadable(&out.findings, "its manifest could not be read");
}
