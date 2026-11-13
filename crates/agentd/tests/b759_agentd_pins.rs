//! Agentd keeps what S3 answered to each of its own end-of-run writes (the SHA-256 of the bytes it sent and the version id) and sends the
//! records in `turn-complete`. The run's session credentials (the engine holds the same ones) can put a newer version of any of those keys after
//! agentd wrote; the records name agentd's own version, so delivery can read it and nothing else. Against the fake S3 of `direct_support`, which
//! keeps every version of a key and answers with `x-amz-version-id` as a versioned bucket does.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::direct::Deps;
use agentd::direct::hooks::{DirectHook, Hooks};
use agentd::direct::report::{ReportHook, Settings};
use agentd::s3::{PutNew, S3Objects, sha256_base64};
use agentd::time::{format_rfc3339, now_unix};
use common::{creds, scratch};
use direct_support::{BUCKET, Direct, FakeAws, Opts, PutFailure, RUN};
use harness_protocol::api::internal::ProcessCredentials;
use harness_protocol::direct::s3 as keys;
use harness_protocol::output_pins::{OutputPin, sha256_hex};
use harness_protocol::runtime::InvocationResponse;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const KEY: &str = "runs/R1/agents/direct/turns/1/summary.json";
const PAUSE: Duration = Duration::from_millis(10);

fn uri() -> String {
    format!("s3://{BUCKET}/{KEY}")
}

fn c() -> ProcessCredentials {
    creds(&format_rfc3339(now_unix() + 3600))
}

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

// ---- the receipt of one write -------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn b759_a_create_only_put_returns_the_digest_and_the_version_s3_answered_with() {
    let aws = FakeAws::start().await;
    let body = b"{\"first\":true}";
    let (put, pin) = client(&aws)
        .put_file_new_retrying_receipt(&uri(), &c(), &file("b759-new", body), 3, PAUSE)
        .await
        .unwrap();
    assert_eq!(put, PutNew::Stored);
    let pin = pin.expect("a stored object has a record");
    assert_eq!(pin.sha256, sha256_hex(body));
    let versions = aws.versions_of(KEY);
    assert_eq!(versions.len(), 1);
    assert_eq!(pin.version_id.as_deref(), Some(versions[0].0.as_str()));
    // S3 stored the same digest as the object's checksum, so a reader can compare the two without a download.
    assert_eq!(Some(pin.sha256_base64()), aws.checksum_of(KEY));
    assert_eq!(pin.sha256_base64(), sha256_base64(body));
    pin.validate().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn b759_a_bucket_that_answers_no_version_id_gives_a_record_with_the_digest_alone() {
    let aws = FakeAws::start().await;
    aws.without_version_ids();
    let body = b"{\"first\":true}";
    let (put, pin) = client(&aws)
        .put_file_new_retrying_receipt(&uri(), &c(), &file("b759-nover", body), 3, PAUSE)
        .await
        .unwrap();
    assert_eq!(put, PutNew::Stored);
    let pin = pin.unwrap();
    assert_eq!(pin.version_id, None);
    assert_eq!(pin.sha256, sha256_hex(body));
    pin.validate().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn b759_a_taken_key_has_no_record_and_the_replacing_put_returns_its_own_version() {
    let aws = FakeAws::start().await;
    aws.put(KEY, b"{\"forged\":true}".to_vec());
    let s3 = client(&aws);
    let body = b"{\"mine\":true}";
    let (put, pin) = s3
        .put_file_new_retrying_receipt(&uri(), &c(), &file("b759-taken", body), 3, PAUSE)
        .await
        .unwrap();
    assert_eq!(put, PutNew::Exists);
    assert_eq!(
        pin, None,
        "nothing was stored, so there is nothing to record"
    );
    let replaced = s3
        .put_file_retrying_receipt(&uri(), &c(), &file("b759-taken2", body), 3, PAUSE)
        .await
        .unwrap();
    let versions = aws.versions_of(KEY);
    assert_eq!(
        versions.len(),
        2,
        "the foreign object stays as the older version"
    );
    assert_eq!(replaced.version_id.as_deref(), Some(versions[1].0.as_str()));
    assert_ne!(replaced.version_id.as_deref(), Some(versions[0].0.as_str()));
    assert_eq!(replaced.sha256, sha256_hex(body));
    assert_eq!(versions[1].1, body.to_vec());
}

#[tokio::test(flavor = "multi_thread")]
async fn b759_a_lost_answer_settled_by_the_head_records_the_head_version_and_agentds_digest() {
    let aws = FakeAws::start().await;
    aws.fail_puts_after_store(KEY, 1, PutFailure::Status500);
    let body = b"{\"mine\":true}";
    let (put, pin) = client(&aws)
        .put_file_new_retrying_receipt(&uri(), &c(), &file("b759-lost", body), 3, PAUSE)
        .await
        .unwrap();
    assert_eq!(put, PutNew::Stored);
    let pin: OutputPin = pin.unwrap();
    let versions = aws.versions_of(KEY);
    assert_eq!(
        versions.len(),
        1,
        "the first try committed; the retry met a 412"
    );
    assert_eq!(pin.version_id.as_deref(), Some(versions[0].0.as_str()));
    assert_eq!(pin.sha256, sha256_hex(body));
}

// ---- the end sequence --------------------------------------------------------------------------------------------------------

fn pins_of(d: &Direct) -> harness_protocol::output_pins::OutputPins {
    let complete = d.aws.complete();
    assert_eq!(complete.len(), 1);
    complete[0]
        .output_pins
        .clone()
        .expect("turn-complete carries the records of the objects agentd stored")
}

#[tokio::test(flavor = "multi_thread")]
async fn b759_turn_complete_names_the_versions_of_the_bundle_and_the_summary_agentd_wrote() {
    let d = Direct::start(Opts::default()).await;
    let _ = d.invoke().await;
    d.wait_healthy().await;
    let pins = pins_of(&d);
    pins.validate().unwrap();
    let bundle = pins.bundle.expect("the bundle record");
    let summary = pins.summary.expect("the summary record");
    // The default run has the report module too; its record, when there is one, names the version agentd stored.
    if let Some(report) = &pins.test_report {
        let versions = d.aws.versions_of(&keys::test_report_key(RUN));
        assert_eq!(versions.len(), 1);
        assert_eq!(report.version_id.as_deref(), Some(versions[0].0.as_str()));
        assert_eq!(report.sha256, sha256_hex(&versions[0].1));
    }
    let own_bundle = std::fs::read(d.agent_data().join("turns/1/bundle")).unwrap();
    assert_eq!(bundle.sha256, sha256_hex(&own_bundle));
    let versions = d.aws.versions_of(&keys::bundle_key(RUN));
    assert_eq!(versions.len(), 1);
    assert_eq!(bundle.version_id.as_deref(), Some(versions[0].0.as_str()));
    let stored = d.aws.get(&keys::summary_key(RUN)).unwrap();
    assert_eq!(summary.sha256, sha256_hex(&stored));
    let versions = d.aws.versions_of(&keys::summary_key(RUN));
    assert_eq!(versions.len(), 1);
    assert_eq!(summary.version_id.as_deref(), Some(versions[0].0.as_str()));
}

#[tokio::test(flavor = "multi_thread")]
async fn b759_a_version_put_after_agentd_wrote_is_not_the_one_the_records_name() {
    // The engine's credentials put a newer version of the bundle and the summary right after agentd's write; the records stay on agentd's.
    let d = Direct::start(Opts::default()).await;
    let _ = d.invoke().await;
    d.wait_healthy().await;
    let own_bundle = std::fs::read(d.agent_data().join("turns/1/bundle")).unwrap();
    let own_summary = d.aws.get(&keys::summary_key(RUN)).unwrap();
    d.aws
        .put(&keys::bundle_key(RUN), b"planted bundle".to_vec());
    d.aws
        .put(&keys::summary_key(RUN), b"{\"planted\":true}".to_vec());
    let pins = pins_of(&d);
    let bundle = pins.bundle.unwrap();
    let summary = pins.summary.unwrap();
    let versions = d.aws.versions_of(&keys::bundle_key(RUN));
    assert_eq!(versions.len(), 2);
    assert_eq!(bundle.version_id.as_deref(), Some(versions[0].0.as_str()));
    assert_ne!(bundle.version_id.as_deref(), Some(versions[1].0.as_str()));
    assert_eq!(versions[0].1, own_bundle);
    assert_eq!(bundle.sha256, sha256_hex(&own_bundle));
    let versions = d.aws.versions_of(&keys::summary_key(RUN));
    assert_eq!(summary.version_id.as_deref(), Some(versions[0].0.as_str()));
    assert_eq!(versions[0].1, own_summary);
    assert_eq!(summary.sha256, sha256_hex(&own_summary));
    // The current objects are the planted ones, which no record names.
    assert_eq!(
        d.aws.get(&keys::bundle_key(RUN)).unwrap(),
        b"planted bundle".to_vec()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn b759_objects_that_replaced_a_foreign_one_are_recorded_at_the_replacing_version() {
    let d = Direct::start(Opts::default()).await;
    d.aws
        .put(&keys::bundle_key(RUN), b"foreign bundle".to_vec());
    d.aws
        .put(&keys::summary_key(RUN), b"{\"forged\":true}".to_vec());
    let _ = d.invoke().await;
    d.wait_healthy().await;
    let pins = pins_of(&d);
    for (key, pin) in [
        (keys::bundle_key(RUN), pins.bundle.unwrap()),
        (keys::summary_key(RUN), pins.summary.unwrap()),
    ] {
        let versions = d.aws.versions_of(&key);
        assert_eq!(
            versions.len(),
            2,
            "{key}: the foreign object, then agentd's"
        );
        assert_eq!(
            pin.version_id.as_deref(),
            Some(versions[1].0.as_str()),
            "{key}"
        );
        assert_eq!(pin.sha256, sha256_hex(&versions[1].1), "{key}");
        assert_ne!(versions[0].1, versions[1].1, "{key}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn b759_a_bucket_with_no_version_ids_still_gets_digest_only_records() {
    let d = Direct::start(Opts::default()).await;
    d.aws.without_version_ids();
    let _ = d.invoke().await;
    d.wait_healthy().await;
    let pins = pins_of(&d);
    pins.validate().unwrap();
    assert_eq!(pins.bundle.as_ref().unwrap().version_id, None);
    assert_eq!(pins.summary.as_ref().unwrap().version_id, None);
    let stored = d.aws.get(&keys::summary_key(RUN)).unwrap();
    assert_eq!(pins.summary.unwrap().sha256, sha256_hex(&stored));
}

#[tokio::test(flavor = "multi_thread")]
async fn b759_a_summary_that_could_not_be_uploaded_has_no_record() {
    let d = Direct::start(Opts::default()).await;
    d.aws.fail_puts(&keys::summary_key(RUN), u32::MAX);
    let _ = d.invoke().await;
    let pins = pins_of(&d);
    assert!(pins.summary.is_none(), "{pins:?}");
    assert!(pins.bundle.is_some(), "the bundle was stored");
}

// ---- the test report ---------------------------------------------------------------------------------------------------------

fn report_settings() -> Settings {
    Settings {
        enabled: true,
        budget: Duration::from_secs(60),
        step: Duration::from_secs(30),
        tail: harness_protocol::direct::DEFAULT_REPORT_TAIL_BYTES,
    }
}

async fn run_with_report(d: &Direct) -> InvocationResponse {
    let hooks = Hooks::new(vec![
        Arc::new(ReportHook::new(report_settings())) as Arc<dyn DirectHook>
    ]);
    let deps = Deps {
        hooks: Some(hooks),
        ..Deps::default()
    };
    agentd::direct::run_with(&d.app, d.req.clone(), &deps).await
}

#[tokio::test(flavor = "multi_thread")]
async fn b759_the_test_report_is_recorded_at_the_version_agentd_wrote() {
    let d = Direct::start(Opts {
        scenario: "report_green",
        timeout_s: 3600,
        ..Opts::default()
    })
    .await;
    let _ = run_with_report(&d).await;
    let pins = pins_of(&d);
    let report = pins.test_report.expect("the report record");
    let versions = d.aws.versions_of(&keys::test_report_key(RUN));
    assert_eq!(versions.len(), 1);
    assert_eq!(report.version_id.as_deref(), Some(versions[0].0.as_str()));
    assert_eq!(report.sha256, sha256_hex(&versions[0].1));
    // And a newer version put afterwards is not the one it names.
    d.aws
        .put(&keys::test_report_key(RUN), b"{\"planted\":true}".to_vec());
    assert_eq!(d.aws.versions_of(&keys::test_report_key(RUN)).len(), 2);
    assert_eq!(report.version_id.as_deref(), Some(versions[0].0.as_str()));
}

#[tokio::test(flavor = "multi_thread")]
async fn b759_a_test_report_that_replaced_a_foreign_one_is_recorded_at_the_replacing_version() {
    let d = Direct::start(Opts {
        scenario: "report_green",
        timeout_s: 3600,
        ..Opts::default()
    })
    .await;
    d.aws
        .put(&keys::test_report_key(RUN), b"{\"forged\":true}".to_vec());
    let _ = run_with_report(&d).await;
    let report = pins_of(&d).test_report.expect("the report record");
    let versions = d.aws.versions_of(&keys::test_report_key(RUN));
    assert_eq!(versions.len(), 2);
    assert_eq!(report.version_id.as_deref(), Some(versions[1].0.as_str()));
    assert_eq!(report.sha256, sha256_hex(&versions[1].1));
}
