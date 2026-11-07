//! Create-only puts of the end-of-run objects against the fake S3 of `direct_support`, and the checksum check that tells agentd's own
//! object from a foreign one when a retry meets S3's 412. The fake keeps what real S3 keeps: the `x-amz-checksum-sha256` value it
//! validated against the body, refused when it does not match, and refuses an `x-amz-*` header the signature does not cover.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::s3::{PutNew, S3Objects, base64_encode, sha256_base64};
use agentd::time::{format_rfc3339, now_unix};
use common::{creds, scratch};
use direct_support::{BUCKET, FakeAws, PutFailure, sha256_b64};
use harness_protocol::api::internal::ProcessCredentials;
use std::path::PathBuf;
use std::time::Duration;

const KEY: &str = "runs/R1/agents/direct/turns/1/summary.json";
const PAUSE: Duration = Duration::from_millis(10);

fn uri() -> String {
    format!("s3://{BUCKET}/{KEY}")
}

fn c() -> ProcessCredentials {
    creds(&format_rfc3339(now_unix() + 3600))
}

/// A client whose request limit is short, so a put whose answer never comes ends within the test.
fn client(aws: &FakeAws) -> S3Objects {
    S3Objects::new("us-west-2".into(), Some(aws.base.clone()))
        .with_request_timeout(Duration::from_millis(400))
}

fn file(label: &str, body: &[u8]) -> PathBuf {
    let f = scratch(label).join("object");
    std::fs::create_dir_all(f.parent().unwrap()).unwrap();
    std::fs::write(&f, body).unwrap();
    f
}

#[test]
fn the_encoder_matches_known_answers() {
    // RFC 4648 section 10, all three padding forms.
    assert_eq!(base64_encode(b"f"), "Zg==");
    assert_eq!(base64_encode(b"fo"), "Zm8=");
    assert_eq!(base64_encode(b"foo"), "Zm9v");
    assert_eq!(base64_encode(b""), "");
    // SHA-256 of the empty input and of "abc" (FIPS 180-4 examples), base64 of the 32-byte digest.
    assert_eq!(
        sha256_base64(b""),
        "47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU="
    );
    assert_eq!(
        sha256_base64(b"abc"),
        "ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0="
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_create_only_put_sends_the_signed_checksum_of_the_body() {
    let aws = FakeAws::start().await;
    let body = b"{\"first\":true}";
    let put = client(&aws)
        .put_file_new(&uri(), &c(), &file("s3-co-sum", body))
        .await
        .unwrap();
    assert_eq!(put, PutNew::Stored);
    // The fake refuses a checksum that does not match the body and a header that the signature does not cover: the stored value is the
    // proof that the header was sent, signed and right.
    assert_eq!(aws.checksum_of(KEY), Some(sha256_b64(body)));
    assert_eq!(aws.get(KEY), Some(body.to_vec()));
}

#[tokio::test(flavor = "multi_thread")]
async fn head_reads_the_stored_checksum_and_tells_a_missing_key_a_refusal_and_no_checksum_apart() {
    let aws = FakeAws::start().await;
    let s3 = client(&aws);
    let body = b"{\"first\":true}";
    s3.put_file_new(&uri(), &c(), &file("s3-co-head", body))
        .await
        .unwrap();
    let head = s3.head_checksum(&uri(), &c()).await.unwrap();
    assert_eq!(head.sha256, Some(sha256_b64(body)));
    // An object stored without a SHA-256 checksum is found, with no value to compare.
    aws.put("runs/R1/agents/direct/turns/1/other", b"x".to_vec());
    let other = format!("s3://{BUCKET}/runs/R1/agents/direct/turns/1/other");
    assert_eq!(s3.head_checksum(&other, &c()).await.unwrap().sha256, None);
    // A missing key and a refused HEAD are errors: they say nothing about who wrote the object.
    let missing = format!("s3://{BUCKET}/runs/R1/agents/direct/turns/1/none");
    let e = s3.head_checksum(&missing, &c()).await.unwrap_err();
    assert!(e.contains("404"), "{e}");
    aws.heads_answer(403);
    let e = s3.head_checksum(&uri(), &c()).await.unwrap_err();
    assert!(e.contains("403"), "{e}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_taken_key_on_the_first_try_is_exists_after_one_put_and_no_head() {
    let aws = FakeAws::start().await;
    aws.put(KEY, b"{\"forged\":true}".to_vec());
    let put = client(&aws)
        .put_file_new_retrying(&uri(), &c(), &file("s3-co-first", b"mine"), 3, PAUSE)
        .await
        .unwrap();
    assert_eq!(put, PutNew::Exists);
    assert_eq!(aws.put_attempts(KEY), 1);
    assert_eq!(aws.head_attempts(KEY), 0);
    assert_eq!(aws.get(KEY), Some(b"{\"forged\":true}".to_vec()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_put_whose_answer_never_comes_is_found_again_by_its_checksum() {
    let aws = FakeAws::start().await;
    aws.fail_puts_after_store(KEY, 1, PutFailure::Hang);
    let put = client(&aws)
        .put_file_new_retrying(&uri(), &c(), &file("s3-co-hang", b"mine"), 3, PAUSE)
        .await
        .unwrap();
    assert_eq!(put, PutNew::Stored);
    assert_eq!(aws.put_attempts(KEY), 2, "the second put met the 412");
    assert_eq!(aws.head_attempts(KEY), 1);
    assert_eq!(aws.get(KEY), Some(b"mine".to_vec()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_put_whose_answer_was_lost_is_found_again_for_a_5xx_and_a_broken_connection() {
    for how in [PutFailure::Status500, PutFailure::ResetBody] {
        let aws = FakeAws::start().await;
        aws.fail_puts_after_store(KEY, 1, how);
        let put = client(&aws)
            .put_file_new_retrying(&uri(), &c(), &file("s3-co-lost", b"mine"), 3, PAUSE)
            .await
            .unwrap();
        assert_eq!(put, PutNew::Stored, "{how:?}");
        assert_eq!(aws.put_attempts(KEY), 2, "{how:?}");
        assert_eq!(aws.head_attempts(KEY), 1, "{how:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lost_answer_followed_by_a_foreign_overwrite_is_exists() {
    let aws = FakeAws::start().await;
    aws.fail_put_after_store_then_overwrite(KEY, PutFailure::Status500, b"{\"forged\":true}");
    let put = client(&aws)
        .put_file_new_retrying(&uri(), &c(), &file("s3-co-over", b"mine"), 3, PAUSE)
        .await
        .unwrap();
    assert_eq!(put, PutNew::Exists);
    assert_eq!(aws.head_attempts(KEY), 1);
    assert_eq!(aws.get(KEY), Some(b"{\"forged\":true}".to_vec()));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_object_the_head_cannot_vouch_for_is_an_error_after_the_tries_not_exists() {
    let aws = FakeAws::start().await;
    aws.fail_puts_after_store(KEY, 1, PutFailure::Status500);
    aws.heads_answer(403);
    let e = client(&aws)
        .put_file_new_retrying(&uri(), &c(), &file("s3-co-403", b"mine"), 2, PAUSE)
        .await
        .unwrap_err();
    assert!(e.contains("403"), "{e}");
    assert_eq!(aws.put_attempts(KEY), 3);
    assert_eq!(aws.head_attempts(KEY), 2, "one HEAD after each 412");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_that_cannot_be_read_fails_before_any_request_and_never_looks_at_the_object() {
    let aws = FakeAws::start().await;
    aws.put(KEY, b"{\"forged\":true}".to_vec());
    let missing = scratch("s3-co-nofile").join("not-there");
    let e = client(&aws)
        .put_file_new_retrying(&uri(), &c(), &missing, 2, PAUSE)
        .await
        .unwrap_err();
    assert!(e.contains("not-there"), "{e}");
    assert_eq!(aws.put_attempts(KEY), 0);
    assert_eq!(aws.head_attempts(KEY), 0);
}
