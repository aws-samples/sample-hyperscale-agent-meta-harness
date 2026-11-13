//! The edges of the third-party copy rule: every way a stored item can fail to be read says so in plain words and blocks
//! the run, many copies are reported up to a limit and counted beyond it, and the git helpers the rule uses report a repository that
//! cannot be read. Ordinary inputs only: a missing object, a text that is not JSON, a bundle with nothing in it.
#![allow(clippy::unwrap_used)]

mod common;
mod third_party_support;

use common::*;
use delivery::git::Git;
use delivery::validator::Rule;
use harness_protocol::research::{ItemKind, s3 as rs3};
use third_party_support::*;

fn cannot_be_checked(out: &delivery::validator::ValidateOutput, what: &str) {
    assert!(!out.pass);
    let v = out
        .violations
        .iter()
        .find(|v| v.rule == Rule::ThirdPartyCopy)
        .unwrap();
    assert!(
        v.detail.contains("could not be checked") && v.detail.contains(what),
        "{}",
        v.detail
    );
}

#[tokio::test]
async fn a_stored_item_whose_body_is_gone_or_whose_manifest_is_not_a_manifest_blocks_the_run() {
    let w = world();
    let body = text(1500, "page");
    // The body is not in the bucket.
    let m = manifest("i-0001", ItemKind::Page, "body.txt", body.as_bytes());
    w.v.blobs
        .insert(&rs3::manifest_key(RUN, "i-0001"), m.canonical_bytes());
    let head = w.turn(&[("docs/own.md", Some("own words\n"))]);
    cannot_be_checked(&w.validate(&head).await, "its body is gone");

    // A manifest that is text and not a manifest.
    let w = world();
    w.v.blobs.insert(
        &rs3::manifest_key(RUN, "i-0001"),
        b"not a manifest".to_vec(),
    );
    let head = w.turn(&[("docs/own.md", Some("own words\n"))]);
    cannot_be_checked(&w.validate(&head).await, "its manifest is not a manifest");

    // A manifest that belongs to another item: it is not the one its key says.
    let w = world();
    let other = manifest("i-0002", ItemKind::Page, "body.txt", body.as_bytes());
    w.v.blobs
        .insert(&rs3::manifest_key(RUN, "i-0001"), other.canonical_bytes());
    let head = w.turn(&[("docs/own.md", Some("own words\n"))]);
    cannot_be_checked(&w.validate(&head).await, "breaks the contract");
}

#[tokio::test]
async fn many_copies_are_reported_up_to_a_limit_and_the_rest_are_counted() {
    let w = world();
    let originals: Vec<(String, String)> = (0..27)
        .map(|i| {
            (
                format!("lib/f{i:02}.js"),
                text(1100, &format!("file {i:02}")),
            )
        })
        .collect();
    let files: Vec<(&str, &str)> = originals
        .iter()
        .map(|(p, b)| (p.as_str(), b.as_str()))
        .collect();
    store_repo(&w.v.blobs, "i-0002", &files);
    let delivered: Vec<(String, String)> = originals
        .iter()
        .map(|(p, b)| (format!("src/{}", p.trim_start_matches("lib/")), b.clone()))
        .collect();
    let turn: Vec<(&str, Option<&str>)> = delivered
        .iter()
        .map(|(p, b)| (p.as_str(), Some(b.as_str())))
        .collect();
    let head = w.turn(&turn);
    let out = w.validate(&head).await;
    assert!(!out.pass);
    let copies: Vec<_> = out
        .violations
        .iter()
        .filter(|v| v.rule == Rule::ThirdPartyCopy)
        .collect();
    assert_eq!(copies.len(), 26, "25 named and one that counts the rest");
    assert!(
        copies[25].detail.contains("2 more file(s)"),
        "{}",
        copies[25].detail
    );
}

#[tokio::test]
async fn the_git_helpers_report_a_repository_that_cannot_be_read() {
    let tmp = tmp();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let git = Git::new("git", &home);
    // A directory that is not a repository.
    let empty = tmp.path().join("nothing-here");
    std::fs::create_dir_all(&empty).unwrap();
    let head = "a".repeat(40);
    assert!(git.added_blobs(&empty, None, &head).await.is_err());
    assert!(git.blobs(&empty, &[head.as_str()]).await.is_err());
    // No ids is nothing to read; an id that is not a full hexadecimal id is refused before git runs.
    assert!(git.blobs(&empty, &[]).await.unwrap().is_empty());
    assert!(git.blobs(&empty, &["HEAD"]).await.is_err());
}
