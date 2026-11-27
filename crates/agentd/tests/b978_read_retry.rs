//! `S3Objects::get_to_file_retrying`, the read that asks again as the stores do, against the fake S3 of `direct_support`,
//! and `s3::temporary_answer`, which says whether an answer of the storage service may be another one a moment later. No AWS
//! call is made. The waits between the reads are held from below only; no test holds an upper bound of time.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::s3::{GetDone, S3Objects, temporary_answer};
use agentd::time::{format_rfc3339, now_unix};
use common::{creds, scratch};
use direct_support::{BUCKET, FakeAws, GetFailure};
use harness_protocol::api::internal::ProcessCredentials;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const KEY: &str = "runs/R1/input/request.json";
const BODY: &[u8] = b"{\"prompt\":\"the request of a run\"}";
const PAUSE: Duration = Duration::from_millis(10);
const RETRIES: u32 = 3;

fn uri() -> String {
    format!("s3://{BUCKET}/{KEY}")
}

fn c() -> ProcessCredentials {
    creds(&format_rfc3339(now_unix() + 3600))
}

fn client(aws: &FakeAws) -> S3Objects {
    S3Objects::new("us-west-2".into(), Some(aws.base.clone()))
}

/// Where a test's download goes: a folder of its own that holds nothing yet.
fn dest(label: &str) -> PathBuf {
    let dir = scratch(label).join("download");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("request.json")
}

/// The names in the folder of `file`.
fn beside(file: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(file.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// A bucket that holds the object.
async fn bucket() -> FakeAws {
    let aws = FakeAws::start().await;
    aws.put(KEY, BODY.to_vec());
    aws
}

#[tokio::test(flavor = "multi_thread")]
async fn b978_two_temporary_answers_then_the_object_are_three_reads() {
    let aws = bucket().await;
    aws.fail_gets(KEY, 2);
    let file = dest("b978-two-then-object");
    let done = client(&aws)
        .get_to_file_retrying(&uri(), &c(), &file, RETRIES, PAUSE)
        .await
        .unwrap();
    assert_eq!(done.tries, 3, "{done:?}");
    assert_eq!(std::fs::read(&file).unwrap(), BODY);
    assert_eq!(aws.gets_of(KEY).len(), 3);
    assert_eq!(beside(&file), vec!["request.json".to_string()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn b978_each_kind_of_temporary_answer_is_asked_again() {
    for how in [
        GetFailure::Status500,
        GetFailure::SlowDown,
        GetFailure::ResetBody,
    ] {
        let aws = bucket().await;
        aws.fail_gets_as(KEY, 1, how);
        let file = dest(&format!("b978-once-{how:?}"));
        let done: GetDone = client(&aws)
            .get_to_file_retrying(&uri(), &c(), &file, RETRIES, PAUSE)
            .await
            .unwrap_or_else(|e| panic!("{how:?}: {e}"));
        assert_eq!(done.tries, 2, "{how:?}");
        assert_eq!(std::fs::read(&file).unwrap(), BODY, "{how:?}");
        assert_eq!(aws.gets_of(KEY).len(), 2, "{how:?}");
        assert_eq!(
            beside(&file),
            vec!["request.json".to_string()],
            "{how:?}: a read that failed left nothing behind"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn b978_only_temporary_answers_end_after_the_waits_and_the_sentence_names_the_tries() {
    let aws = bucket().await;
    aws.fail_gets(KEY, u32::MAX);
    let file = dest("b978-every-time");
    let began = Instant::now();
    let err = client(&aws)
        .get_to_file_retrying(&uri(), &c(), &file, RETRIES, PAUSE)
        .await
        .unwrap_err();
    let took = began.elapsed();
    assert_eq!(
        err,
        format!(
            "s3 GET {} returned 500 InternalError: We encountered an internal error. (the read was tried 4 times)",
            uri()
        )
    );
    assert_eq!(aws.gets_of(KEY).len(), 4);
    // 10 ms, 20 ms and 40 ms between the four reads.
    assert!(took >= PAUSE * 7, "{took:?}");
    assert!(beside(&file).is_empty(), "{:?}", beside(&file));
}

#[tokio::test(flavor = "multi_thread")]
async fn b978_an_answer_that_stays_what_it_is_is_one_read_with_the_sentence_of_one_read() {
    // The object is not there.
    let aws = FakeAws::start().await;
    let file = dest("b978-absent");
    let s3 = client(&aws);
    let err = s3
        .get_to_file_retrying(&uri(), &c(), &file, RETRIES, PAUSE)
        .await
        .unwrap_err();
    assert_eq!(aws.gets_of(KEY).len(), 1, "{err}");
    assert!(err.contains("returned 404 NoSuchKey"), "{err}");
    assert_eq!(
        err,
        s3.get_to_file(&uri(), &c(), &file).await.unwrap_err(),
        "the sentence of one read"
    );
    // The access is refused, for an object that is there.
    let aws = bucket().await;
    aws.fail_gets_as(KEY, u32::MAX, GetFailure::Denied);
    let file = dest("b978-refused");
    let s3 = client(&aws);
    let err = s3
        .get_to_file_retrying(&uri(), &c(), &file, RETRIES, PAUSE)
        .await
        .unwrap_err();
    assert_eq!(aws.gets_of(KEY).len(), 1, "{err}");
    assert_eq!(
        err,
        format!("s3 GET {} returned 403 AccessDenied: Access Denied", uri())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b978_no_further_tries_is_the_read_of_before() {
    let aws = bucket().await;
    aws.fail_gets(KEY, u32::MAX);
    let file = dest("b978-no-further-tries");
    let err = client(&aws)
        .get_to_file_retrying(&uri(), &c(), &file, 0, PAUSE)
        .await
        .unwrap_err();
    assert_eq!(
        err,
        format!(
            "s3 GET {} returned 500 InternalError: We encountered an internal error.",
            uri()
        )
    );
    assert_eq!(aws.gets_of(KEY).len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn b978_a_store_that_answers_no_connection_is_asked_again_and_the_sentence_names_the_tries() {
    // A listener that is closed again: its port takes no connection.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let s3 = S3Objects::new("us-west-2".into(), Some(format!("http://127.0.0.1:{port}")));
    let file = dest("b978-no-connection");
    let began = Instant::now();
    let err = s3
        .get_to_file_retrying(&uri(), &c(), &file, RETRIES, PAUSE)
        .await
        .unwrap_err();
    assert!(err.starts_with("transport: "), "{err}");
    assert!(err.ends_with(" (the read was tried 4 times)"), "{err}");
    assert!(began.elapsed() >= PAUSE * 7, "{:?}", began.elapsed());
}

#[test]
fn b978_which_answers_may_be_another_one_a_moment_later() {
    let doc = |code: &str| format!("<Error><Code>{code}</Code><Message>m</Message></Error>");
    let rows: [(u16, &str, bool); 13] = [
        (500, "InternalError", true),
        (502, "", true),
        (503, "SlowDown", true),
        (503, "ServiceUnavailable", true),
        (504, "", true),
        (429, "", true),
        (400, "RequestTimeout", true),
        (400, "Throttling", true),
        (404, "NoSuchKey", false),
        (403, "AccessDenied", false),
        (400, "InvalidRequest", false),
        (301, "PermanentRedirect", false),
        (412, "PreconditionFailed", false),
    ];
    for (status, code, temporary) in rows {
        let body = if code.is_empty() {
            String::new()
        } else {
            doc(code)
        };
        assert_eq!(
            temporary_answer(status, body.as_bytes()),
            temporary,
            "{status} {code}"
        );
    }
}
