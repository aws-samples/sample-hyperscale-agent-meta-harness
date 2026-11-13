//! Review fixes of the third-party copy rule: a stored file padded with blanks cannot make a
//! short ordinary file look like a copy, a stored archive that lacks a file its manifest lists, or holds one it does not list, is a stored
//! item that cannot be checked, and the answer of `git cat-file --batch` is read with checked offsets and every incomplete record is an error.
#![allow(clippy::unwrap_used)]

mod common;
mod third_party_support;

use common::*;
use delivery::git::parse_blobs;
use delivery::validator::{Rule, ThirdPartyMode};
use harness_protocol::research::{ItemKind, ManifestFile, s3 as rs3};
use third_party_support::*;

#[tokio::test]
async fn a_stored_file_padded_with_blanks_does_not_block_a_short_ordinary_file() {
    let w = world();
    let mut padded = String::from("x\n");
    padded.push_str(&" ".repeat(1200));
    padded.push_str("\n\n\n");
    store_repo(&w.v.blobs, "i-0002", &[("pad.txt", padded.as_str())]);
    let head = w.turn(&[("src/x.txt", Some("x\n"))]);
    let out = w.validate(&head).await;
    assert!(out.pass, "{:?}", out.violations);
    // The same bytes as the stored file still match exactly.
    let head = w.turn(&[("src/pad.txt", Some(padded.as_str()))]);
    let out = w.validate(&head).await;
    assert_eq!(
        copies(&out),
        [(Rule::ThirdPartyCopy, Some("src/pad.txt".to_string()))]
    );
}

#[tokio::test]
async fn a_stored_archive_that_lacks_a_file_its_manifest_lists_cannot_be_checked() {
    let w = world();
    let a = text(1500, "first file");
    let b = text(1500, "second file");
    let archive = targz(&[("a.js", a.as_bytes())]);
    let mut m = manifest("i-0002", ItemKind::Repo, "body.tar.gz", &archive);
    m.files = [("a.js", &a), ("b.js", &b)]
        .iter()
        .map(|(p, body)| ManifestFile {
            path: (*p).to_string(),
            sha256: sha(body.as_bytes()),
            bytes: body.len() as u64,
        })
        .collect();
    w.v.blobs
        .insert(&rs3::manifest_key(RUN, "i-0002"), m.canonical_bytes());
    w.v.blobs.insert(&m.body.key, archive);
    let head = w.turn(&[("docs/own.md", Some("own words\n"))]);
    let out = w.validate(&head).await;
    assert!(!out.pass);
    let v = out
        .violations
        .iter()
        .find(|v| v.rule == Rule::ThirdPartyCopy)
        .unwrap();
    assert!(
        v.detail.contains("i-0002") && v.detail.contains("could not be checked"),
        "{}",
        v.detail
    );
    // With the user's setting the validation passes and the rule's finding is reported.
    let allowed =
        w.v.validate(&w.req(&head, ThirdPartyMode::Allow))
            .await
            .unwrap();
    assert!(allowed.pass);
    assert_eq!(allowed.findings.len(), 1);
}

#[tokio::test]
async fn a_stored_archive_with_a_file_its_manifest_does_not_list_cannot_be_checked() {
    let w = world();
    let a = text(1500, "first file");
    let extra = text(1500, "unlisted file");
    let archive = targz(&[("a.js", a.as_bytes()), ("extra.js", extra.as_bytes())]);
    let mut m = manifest("i-0002", ItemKind::Repo, "body.tar.gz", &archive);
    m.files = vec![ManifestFile {
        path: "a.js".to_string(),
        sha256: sha(a.as_bytes()),
        bytes: a.len() as u64,
    }];
    w.v.blobs
        .insert(&rs3::manifest_key(RUN, "i-0002"), m.canonical_bytes());
    w.v.blobs.insert(&m.body.key, archive);
    let head = w.turn(&[("docs/own.md", Some("own words\n"))]);
    let out = w.validate(&head).await;
    assert!(!out.pass);
    let v = out
        .violations
        .iter()
        .find(|v| v.rule == Rule::ThirdPartyCopy)
        .unwrap();
    assert!(
        v.detail.contains("i-0002") && v.detail.contains("could not be checked"),
        "{}",
        v.detail
    );
}

#[tokio::test]
async fn a_stored_archive_that_holds_one_file_twice_cannot_be_checked() {
    let w = world();
    let a = text(1500, "first file");
    let archive = targz(&[("a.js", a.as_bytes()), ("a.js", a.as_bytes())]);
    let mut m = manifest("i-0002", ItemKind::Repo, "body.tar.gz", &archive);
    m.files = vec![ManifestFile {
        path: "a.js".to_string(),
        sha256: sha(a.as_bytes()),
        bytes: a.len() as u64,
    }];
    w.v.blobs
        .insert(&rs3::manifest_key(RUN, "i-0002"), m.canonical_bytes());
    w.v.blobs.insert(&m.body.key, archive);
    let head = w.turn(&[("docs/own.md", Some("own words\n"))]);
    let out = w.validate(&head).await;
    assert!(!out.pass);
    assert!(
        out.violations
            .iter()
            .any(|v| v.rule == Rule::ThirdPartyCopy && v.detail.contains("could not be checked"))
    );
}

#[test]
fn the_answer_of_cat_file_batch_is_read_record_by_record() {
    let ok = b"aaaa blob 3\nabc\nbbbb blob 0\n\ncccc blob 5\nhello\n".to_vec();
    assert_eq!(
        parse_blobs(&ok, 3).unwrap(),
        vec![b"abc".to_vec(), Vec::new(), b"hello".to_vec()]
    );
    assert_eq!(parse_blobs(b"", 0).unwrap(), Vec::<Vec<u8>>::new());
}

#[test]
fn every_incomplete_record_of_the_answer_is_an_error() {
    for (what, data, count) in [
        ("a missing object", b"aaaa missing\n".to_vec(), 1),
        ("a size past the end", b"aaaa blob 9\nabc\n".to_vec(), 1),
        (
            "no newline after the content",
            b"aaaa blob 3\nabc".to_vec(),
            1,
        ),
        ("a header with no newline", b"aaaa blob 3".to_vec(), 1),
        (
            "fewer records than asked",
            b"aaaa blob 3\nabc\n".to_vec(),
            2,
        ),
        (
            "a size that is not a number",
            b"aaaa blob x\nabc\n".to_vec(),
            1,
        ),
        ("a type other than blob", b"aaaa tree 3\nabc\n".to_vec(), 1),
        (
            "a size that overflows",
            b"aaaa blob 18446744073709551615\nabc\n".to_vec(),
            1,
        ),
    ] {
        assert!(parse_blobs(&data, count).is_err(), "{what}");
    }
}
