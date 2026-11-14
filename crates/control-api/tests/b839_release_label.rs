//! The version the backend reports can follow a release that deployed no file of the stage.
//!
//! A release whose deployed files are unchanged (no file under the deployed paths differs) makes setup deploy nothing, so the compiled-in `VERSION` of control-api
//! stays the old one. Setup then writes the stage's parameter `/hamh/<stage>/release-version` (`N.NN <source tree>`), and control-api reports that label
//! when it is a valid version, no older than the compiled one and bound to the source tree the stage runs; every other state reports the compiled version.
//! These cases pin the rule, the read through the SSM adapter, the cache, the fallbacks and the header on every answer. No network.
#![allow(clippy::unwrap_used)]

mod chat_support;

use aws_smithy_http_client::test_util::{ReplayEvent, StaticReplayClient};
use aws_smithy_types::body::SdkBody;
use chat_support::{ME, World, deps};
use control_api::aws::SsmReleaseLabel;
use control_api::handler::V1Response;
use control_api::lambda::{serve, to_response, to_response_labelled};
use control_api::version::{
    LEGACY_VERSION_HEADER, LabelParameters, LabelStore, ReleaseLabel, VERSION, VERSION_HEADER,
    reported_version,
};
use serde_json::{Value, json};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const TREE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OTHER_TREE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn label(version: &str, tree: &str) -> String {
    format!("{version} {tree}")
}

#[test]
fn b839_the_rule_reports_the_label_when_it_is_newer_than_the_compiled_version_and_bound_to_the_tree()
 {
    let l = label("5.18", TREE);
    assert_eq!(reported_version(Some(&l), Some(TREE), "5.17"), "5.18");
    // the numbers are compared as numbers, not as text
    assert_eq!(
        reported_version(Some(&label("10.01", TREE)), Some(TREE), "9.99"),
        "10.01"
    );
    // a stage tree that the parameter holds with a line break (an SSM value written by hand) is the same tree
    assert_eq!(
        reported_version(Some(&format!("{l}\n")), Some(&format!("{TREE}\n")), "5.17"),
        "5.18"
    );
}

#[test]
fn b839_the_rule_reports_the_compiled_version_in_every_other_state() {
    let ok = label("5.18", TREE);
    let cases: Vec<(&str, Option<String>, Option<&str>, &str)> = vec![
        ("no label", None, Some(TREE), "5.17"),
        ("no tree on the stage", Some(ok.clone()), None, "5.17"),
        ("another tree", Some(ok.clone()), Some(OTHER_TREE), "5.17"),
        (
            "the same version",
            Some(label("5.17", TREE)),
            Some(TREE),
            "5.17",
        ),
        (
            "an older label",
            Some(label("5.16", TREE)),
            Some(TREE),
            "5.17",
        ),
        (
            "an older major",
            Some(label("4.99", TREE)),
            Some(TREE),
            "5.17",
        ),
        (
            "no tree in the label",
            Some("5.18".into()),
            Some(TREE),
            "5.17",
        ),
        ("an empty label", Some(String::new()), Some(TREE), "5.17"),
        ("a prefix v", Some(label("v5.18", TREE)), Some(TREE), "5.17"),
        (
            "one digit of counter",
            Some(label("5.8", TREE)),
            Some(TREE),
            "5.17",
        ),
        (
            "three digits of counter",
            Some(label("5.018", TREE)),
            Some(TREE),
            "5.17",
        ),
        ("a sign", Some(label("+5.18", TREE)), Some(TREE), "5.17"),
        (
            "a third word",
            Some(format!("{ok} extra")),
            Some(TREE),
            "5.17",
        ),
        (
            "a second line",
            Some(format!("{ok}\n5.19 {TREE}")),
            Some(TREE),
            "5.17",
        ),
        (
            "a tree that is not 64 hex digits",
            Some(label("5.18", "abc")),
            Some("abc"),
            "5.17",
        ),
    ];
    for (what, raw, live, compiled) in cases {
        assert_eq!(
            reported_version(raw.as_deref(), live, compiled),
            compiled,
            "{what}: the compiled version is reported"
        );
    }
}

/// The cases that the deploy script's tests read as well (`fixtures/release-label/cases.json`): the label the deploy writes is the label this crate reads.
#[test]
fn b839_the_cases_the_deploy_script_shares_with_this_crate_are_reported_as_the_file_says() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/release-label/cases.json");
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let cases = doc["cases"].as_array().unwrap();
    assert!(
        cases.len() >= 20,
        "the file holds the cases: {}",
        cases.len()
    );
    for c in cases {
        let (what, label) = (c["what"].as_str().unwrap(), c["label"].as_str().unwrap());
        let live = c["live_tree"].as_str();
        let compiled = c["compiled"].as_str().unwrap();
        assert_eq!(
            reported_version(Some(label), live, compiled),
            c["reports"].as_str().unwrap(),
            "{what}: {label:?} on a stage that runs {live:?}, built with {compiled}"
        );
    }
}

/// A store with a scripted answer, a counter and an optional delay.
struct Scripted {
    reads: AtomicUsize,
    answer: Mutex<Result<LabelParameters, String>>,
    delay: Duration,
}

impl Scripted {
    fn new(answer: Result<LabelParameters, String>, delay: Duration) -> Self {
        Self {
            reads: AtomicUsize::new(0),
            answer: Mutex::new(answer),
            delay,
        }
    }
    fn says(&self, answer: Result<LabelParameters, String>) {
        *self.answer.lock().unwrap() = answer;
    }
    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }
}

impl LabelStore for &Scripted {
    async fn read(&self) -> Result<LabelParameters, String> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        self.answer.lock().unwrap().clone()
    }
}

fn params(version: &str, tree: &str) -> LabelParameters {
    LabelParameters {
        label: Some(label(version, tree)),
        tree: Some(tree.to_string()),
    }
}

/// An older compiled version than the label's, whatever this build's file says: the label is newer than `VERSION` by construction.
fn newer_than_compiled() -> String {
    let (major, minor) = VERSION.split_once('.').unwrap();
    format!("{major}.{:02}", minor.parse::<u32>().unwrap() + 1)
}

#[tokio::test]
async fn b839_the_label_is_read_once_per_interval_and_again_after_it() {
    let newer = newer_than_compiled();
    let store = Scripted::new(Ok(params(&newer, TREE)), Duration::ZERO);
    let cache = ReleaseLabel::with_times(
        &store,
        Duration::from_millis(80),
        Duration::from_millis(20),
        Duration::from_secs(2),
    );
    assert_eq!(cache.current().await, newer);
    assert_eq!(cache.current().await, newer);
    assert_eq!(store.reads(), 1, "the second answer comes from the cache");
    // a new label shows when the interval has passed
    let newest = {
        let (major, minor) = newer.split_once('.').unwrap();
        format!("{major}.{:02}", minor.parse::<u32>().unwrap() + 1)
    };
    store.says(Ok(params(&newest, TREE)));
    assert_eq!(cache.current().await, newer, "still inside the interval");
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(cache.current().await, newest);
    assert_eq!(store.reads(), 2);
}

#[tokio::test]
async fn b839_a_label_that_equals_the_compiled_version_is_kept_for_the_whole_interval() {
    // the steady state: every deploy writes a label equal to the version it was built with, and that answer is not read again before the interval has passed
    let store = Scripted::new(Ok(params(VERSION, TREE)), Duration::ZERO);
    let cache = ReleaseLabel::with_times(
        &store,
        Duration::from_millis(300),
        Duration::from_millis(10),
        Duration::from_secs(2),
    );
    assert_eq!(cache.current().await, VERSION);
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(cache.current().await, VERSION);
    assert_eq!(
        store.reads(),
        1,
        "TRIGGER PRESENT: the fallback interval (10 ms) has passed, the label's (300 ms) has not"
    );
    // no label at all is looked at again after the short interval
    let none = Scripted::new(
        Ok(LabelParameters {
            label: None,
            tree: Some(TREE.into()),
        }),
        Duration::ZERO,
    );
    let cache = ReleaseLabel::with_times(
        &none,
        Duration::from_millis(300),
        Duration::from_millis(10),
        Duration::from_secs(2),
    );
    assert_eq!(cache.current().await, VERSION);
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(cache.current().await, VERSION);
    assert_eq!(none.reads(), 2);
}

#[tokio::test]
async fn b839_a_failed_or_slow_read_reports_the_compiled_version_and_is_tried_again_soon() {
    let newer = newer_than_compiled();
    let store = Scripted::new(Err("AccessDenied".into()), Duration::ZERO);
    let cache = ReleaseLabel::with_times(
        &store,
        Duration::from_secs(60),
        Duration::from_millis(40),
        Duration::from_secs(2),
    );
    assert_eq!(
        cache.current().await,
        VERSION,
        "a refused read: the compiled version"
    );
    assert_eq!(cache.current().await, VERSION);
    assert_eq!(
        store.reads(),
        1,
        "the fallback is cached for a short while too"
    );
    store.says(Ok(params(&newer, TREE)));
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(
        cache.current().await,
        newer,
        "the next read after the short interval finds the label"
    );

    // a read that does not answer in time
    let slow = Scripted::new(Ok(params(&newer, TREE)), Duration::from_secs(5));
    let cache = ReleaseLabel::with_times(
        &slow,
        Duration::from_secs(60),
        Duration::from_millis(40),
        Duration::from_millis(50),
    );
    let started = std::time::Instant::now();
    assert_eq!(cache.current().await, VERSION);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the answer never waits for a slow parameter store: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn b839_a_label_of_another_tree_or_an_older_version_is_not_reported() {
    let newer = newer_than_compiled();
    let other = Scripted::new(
        Ok(LabelParameters {
            label: Some(label(&newer, OTHER_TREE)),
            tree: Some(TREE.to_string()),
        }),
        Duration::ZERO,
    );
    let cache = ReleaseLabel::new(&other);
    assert_eq!(
        cache.current().await,
        VERSION,
        "the label belongs to another deploy"
    );
    let older = Scripted::new(Ok(params("0.01", TREE)), Duration::ZERO);
    let cache = ReleaseLabel::new(&older);
    assert_eq!(
        cache.current().await,
        VERSION,
        "the compiled version is the floor"
    );
}

// ---- the adapter ----------------------------------------------------------------------------------------------------------------------

fn replay(responses: Vec<(u16, String)>) -> StaticReplayClient {
    StaticReplayClient::new(
        responses
            .into_iter()
            .map(|(status, body)| {
                ReplayEvent::new(
                    http::Request::builder()
                        .uri("https://example.invalid/")
                        .body(SdkBody::empty())
                        .unwrap(),
                    http::Response::builder()
                        .status(status)
                        .header("content-type", "application/x-amz-json-1.1")
                        .body(SdkBody::from(body))
                        .unwrap(),
                )
            })
            .collect(),
    )
}

fn ssm(r: &StaticReplayClient) -> aws_sdk_ssm::Client {
    aws_sdk_ssm::Client::from_conf(
        aws_sdk_ssm::Config::builder()
            .behavior_version_latest()
            .region(aws_sdk_ssm::config::Region::new("us-west-2"))
            .credentials_provider(aws_sdk_ssm::config::Credentials::for_tests())
            .retry_config(aws_sdk_ssm::config::retry::RetryConfig::disabled())
            .http_client(r.clone())
            .build(),
    )
}

#[tokio::test]
async fn b839_the_adapter_reads_the_label_and_the_deployed_tree_in_one_call() {
    let both = json!({"Parameters": [
        {"Name": "/hamh/live/release-version", "Type": "String", "Value": label("5.18", TREE), "Version": 3},
        {"Name": "/hamh/live/deployed-source-tree", "Type": "String", "Value": TREE, "Version": 7}],
        "InvalidParameters": []})
    .to_string();
    let only_tree = json!({"Parameters": [
        {"Name": "/hamh/live/deployed-source-tree", "Type": "String", "Value": TREE, "Version": 7}],
        "InvalidParameters": ["/hamh/live/release-version"]})
    .to_string();
    let denied = r#"{"__type": "AccessDeniedException", "message": "not allowed"}"#.to_string();
    let r = replay(vec![(200, both), (200, only_tree), (400, denied)]);
    let store = SsmReleaseLabel {
        client: ssm(&r),
        prefix: "/hamh/live".into(),
    };
    let got = store.read().await.unwrap();
    assert_eq!(got.label.as_deref(), Some(label("5.18", TREE).as_str()));
    assert_eq!(got.tree.as_deref(), Some(TREE));
    let got = store.read().await.unwrap();
    assert_eq!(
        got.label, None,
        "a stage deployed before the label has none"
    );
    assert_eq!(got.tree.as_deref(), Some(TREE));
    let e = store.read().await.unwrap_err();
    assert!(e.contains("AccessDenied"), "{e}");
    let bodies: Vec<Value> = r
        .actual_requests()
        .map(|req| serde_json::from_slice(req.body().bytes().unwrap()).unwrap())
        .collect();
    assert_eq!(bodies.len(), 3);
    assert_eq!(
        bodies[0]["Names"],
        json!([
            "/hamh/live/release-version",
            "/hamh/live/deployed-source-tree"
        ])
    );
}

// ---- the header -----------------------------------------------------------------------------------------------------------------------

fn answer() -> V1Response {
    V1Response {
        status: 200,
        body: "{}".into(),
        retry_after_s: None,
    }
}

#[test]
fn b839_every_answer_carries_the_label_under_both_header_names() {
    let resp = to_response_labelled(answer(), "99.98").unwrap();
    assert_eq!(resp.headers()[VERSION_HEADER], "99.98");
    assert_eq!(resp.headers()[LEGACY_VERSION_HEADER], "99.98");
    // the plain answer still carries the compiled version
    let plain = to_response(answer()).unwrap();
    assert_eq!(plain.headers()[VERSION_HEADER], VERSION);
    assert_eq!(plain.headers()[LEGACY_VERSION_HEADER], VERSION);
}

// ---- the entry point ------------------------------------------------------------------------------------------------------------------

/// The API Gateway event of a request to `path`, with the caller `arn` (none: an event without an identity).
fn event(path: &str, arn: Option<&str>) -> lambda_http::Request {
    let identity = match arn {
        Some(a) => json!({"userArn": a, "sourceIp": "203.0.113.9"}),
        None => json!({"sourceIp": "203.0.113.9"}),
    };
    let doc = json!({
        "resource": path, "path": path, "httpMethod": "GET",
        "headers": {}, "multiValueHeaders": {},
        "queryStringParameters": null, "multiValueQueryStringParameters": null,
        "pathParameters": null, "stageVariables": null, "isBase64Encoded": false, "body": null,
        "requestContext": {
            "accountId": "111122223333", "resourceId": "abc", "stage": "live", "requestId": "req-1",
            "resourcePath": path, "httpMethod": "GET", "apiId": "api123", "path": format!("/live{path}"),
            "identity": identity
        }
    });
    lambda_http::request::from_str(&doc.to_string()).unwrap()
}

/// What the Lambda answers for each of three requests (a 2xx, a refusal for a missing caller and a path that does not exist), as the header pair says.
async fn answered_versions<L: LabelStore>(release: &ReleaseLabel<L>) -> Vec<(u16, String, String)> {
    let w = World::new();
    let deps = deps(&w);
    let mut seen = Vec::new();
    for (path, arn) in [
        ("/v1/whoami", Some(ME)),
        ("/v1/whoami", None),
        ("/v1/no-such-route", Some(ME)),
    ] {
        let resp = serve(&event(path, arn), &deps, release).await.unwrap();
        seen.push((
            resp.status().as_u16(),
            resp.headers()[VERSION_HEADER].to_str().unwrap().to_string(),
            resp.headers()[LEGACY_VERSION_HEADER]
                .to_str()
                .unwrap()
                .to_string(),
        ));
    }
    seen
}

#[tokio::test]
async fn b839_the_lambda_entry_point_answers_with_the_release_label_whatever_the_status() {
    let newer = newer_than_compiled();
    let store = Scripted::new(Ok(params(&newer, TREE)), Duration::ZERO);
    let release = ReleaseLabel::new(&store);
    let seen = answered_versions(&release).await;
    let statuses: Vec<u16> = seen.iter().map(|s| s.0).collect();
    assert!(
        statuses[0] == 200 && statuses[1] >= 400 && statuses[2] >= 400,
        "TRIGGER PRESENT: one answer of each kind (a 2xx and two refusals): {statuses:?}"
    );
    for (status, version, legacy) in &seen {
        assert_eq!(
            version, &newer,
            "the {status} answer carries the label, not the version compiled in"
        );
        assert_eq!(legacy, &newer, "the {status} answer's earlier header too");
    }
    assert_eq!(
        store.reads(),
        1,
        "three answers, one read of the parameters (the cache of the label)"
    );
}

#[tokio::test]
async fn b839_the_lambda_entry_point_answers_with_the_compiled_version_when_the_label_cannot_be_read()
 {
    let store = Scripted::new(Err("AccessDenied".into()), Duration::ZERO);
    let release = ReleaseLabel::new(&store);
    for (status, version, legacy) in answered_versions(&release).await {
        assert_eq!(version, VERSION, "the {status} answer");
        assert_eq!(legacy, VERSION, "the {status} answer's earlier header");
    }
}
