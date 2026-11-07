//! `POST /v1/direct/runs` with `compute: instances` against the real Step Functions/SSM adapter over recorded exchanges.
//! The start asks SSM for the parameter that holds the Instances runtime's ARN. What SSM
//! answers decides what the user is told:
//!
//! - no such parameter: the stage has no Instances runtime, the documented 422 `compute-unavailable`;
//! - a throttle, a service fault or no answer at all: the read may pass on a retry, a 503 with the retry words;
//! - anything else that SSM refuses (a role without `ssm:GetParameter` on the parameter, a name SSM does not accept): the stage's own setup
//!   is incomplete and the same request fails the same way until an operator finishes it. That is a 422 `compute-not-set-up` that says so,
//!   with no retry words (`ham` retries every 5xx), and the reason goes to the log.
#![allow(clippy::unwrap_used)]

mod chat_support;

use aws_smithy_http_client::test_util::{ReplayEvent, StaticReplayClient};
use aws_smithy_types::body::SdkBody;
use chat_support::{ME, SONNET, World, request};
use control_api::aws::SfnExecution;
use control_api::handler::{Deps, handle_logged};
use serde_json::{Value, json};

const DEPLOYED: &str = r#"{"Parameters": [
    {"Name": "/hamh/direct/deployment-digest", "Type": "String", "Value": "ddd", "Version": 7},
    {"Name": "/hamh/direct/deployed-source-tree", "Type": "String", "Value": "eee", "Version": 7}],
    "InvalidParameters": []}"#;

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

macro_rules! client {
    ($sdk:ident, $replay:expr) => {
        $sdk::Client::from_conf(
            $sdk::Config::builder()
                .behavior_version_latest()
                .region($sdk::config::Region::new("us-west-2"))
                .credentials_provider($sdk::config::Credentials::for_tests())
                .retry_config($sdk::config::retry::RetryConfig::disabled())
                .http_client($replay.clone())
                .build(),
        )
    };
}

fn ssm_error(status: u16, kind: &str) -> (u16, String) {
    (
        status,
        json!({"__type": kind, "message": "SENTINEL-detail-4c2f"}).to_string(),
    )
}

/// One start with `compute: instances`; the deployment read succeeds and the Instances parameter read answers `instances`.
async fn start_instances(instances: (u16, String)) -> (World, u16, Value, Vec<String>) {
    let w = World::new();
    let ssm = replay(vec![(200, DEPLOYED.to_string()), instances]);
    let deps = Deps {
        store: &w,
        blobs: &w,
        execution: SfnExecution {
            client: client!(aws_sdk_sfn, replay(vec![])),
            state_machine_arn: "arn:aws:states:us-west-2:123456789012:stateMachine:hamh-direct-run"
                .into(),
            ssm: client!(aws_sdk_ssm, ssm),
            deployment_prefix: "/hamh/direct".into(),
        },
        account: "123456789012".into(),
        operators: vec![],
        repo_names: Default::default(),
    };
    let body = json!({
        "prompt": "Add percent_change(a, b) with tests",
        "target": {"mode": "s3-only"},
        "model": SONNET,
        "rules": [],
        "compute": {"mode": "instances"}
    });
    let mut r = request(ME, "POST", "/v1/direct/runs", &[], &body.to_string());
    r.headers.insert("idempotency-key".into(), "k-ssm".into());
    let (out, diagnostics) = handle_logged(r, &deps).await;
    let v = serde_json::from_str(&out.body).unwrap_or(Value::Null);
    (w, out.status, v, diagnostics)
}

fn untouched(w: &World) {
    assert!(w.runs.lock().unwrap().is_empty(), "no run was created");
    assert!(w.idem.lock().unwrap().is_empty(), "no claim was written");
    assert!(w.started.lock().unwrap().is_empty(), "no execution started");
    assert_eq!(w.count("blob_put"), 0, "no blob was written");
}

#[tokio::test]
async fn a_stage_without_the_parameter_gets_the_refusal_that_says_what_to_do() {
    let (w, status, v, _) = start_instances(ssm_error(400, "ParameterNotFound")).await;
    assert_eq!(status, 422, "{v}");
    assert_eq!(v["code"], "compute-unavailable", "{v}");
    assert!(
        v["message"].as_str().unwrap().contains("--compute microvm"),
        "{v}"
    );
    untouched(&w);
    // A parameter that holds no runtime ARN is the same stage.
    let value = json!({"Parameter": {"Name": "p", "Type": "String", "Value": "not-an-arn"}});
    let (w, status, v, _) = start_instances((200, value.to_string())).await;
    assert_eq!(
        (status, v["code"].as_str()),
        (422, Some("compute-unavailable"))
    );
    untouched(&w);
}

#[tokio::test]
async fn a_throttle_or_a_service_fault_is_a_503_with_the_retry_words() {
    for (status, kind) in [
        (400, "ThrottlingException"),
        (500, "InternalServerError"),
        (503, "ServiceUnavailable"),
    ] {
        let (w, got, v, diag) = start_instances(ssm_error(status, kind)).await;
        assert_eq!(got, 503, "{kind}: {v}");
        assert_eq!(v["code"], "unavailable", "{kind}: {v}");
        assert!(
            v["message"].as_str().unwrap().contains("retry"),
            "{kind}: {v}"
        );
        assert!(
            diag.iter().any(|l| l.contains(kind)),
            "{kind}: the log names the cause: {diag:?}"
        );
        untouched(&w);
    }
}

#[tokio::test]
async fn a_credential_or_clock_failure_passes_by_itself_so_it_is_a_503_with_the_retry_words() {
    // A token that expired between its issue and the call, a clock that is a few minutes off, a key that is being rotated: none of these
    // says the stage is not set up, and the next attempt gets fresh credentials.
    for (status, kind) in [
        (400, "ExpiredTokenException"),
        (400, "RequestExpired"),
        (403, "InvalidSignatureException"),
        (403, "SignatureDoesNotMatch"),
        (403, "UnrecognizedClientException"),
    ] {
        let (w, got, v, _) = start_instances(ssm_error(status, kind)).await;
        assert_eq!(got, 503, "{kind}: {v}");
        assert_eq!(v["code"], "unavailable", "{kind}: {v}");
        untouched(&w);
    }
}

#[tokio::test]
async fn a_refusal_of_the_stages_own_setup_is_not_a_retryable_answer() {
    // `AccessDeniedException` is what SSM answers a role that lacks the grant, before it looks for the parameter, so a stage without the
    // runtime and without the grant lands here too.
    for (status, kind) in [
        (400, "AccessDeniedException"),
        (400, "ValidationException"),
        (400, "InvalidKeyId"),
        (403, "AccessDeniedException"),
    ] {
        let (w, got, v, diag) = start_instances(ssm_error(status, kind)).await;
        assert_eq!(got, 422, "{kind}: {v}");
        assert_eq!(v["code"], "compute-not-set-up", "{kind}: {v}");
        let message = v["message"].as_str().unwrap();
        assert!(
            message.contains("setup") && message.contains("Instances"),
            "{kind}: says the stage's setup is incomplete: {message}"
        );
        assert!(
            message.contains("--compute microvm") && message.contains("run.compute"),
            "{kind}: and what to do meanwhile: {message}"
        );
        for retry in ["retry", "try again", "Idempotency-Key"] {
            assert!(
                !message.contains(retry),
                "{kind}: no retry words in {message}"
            );
        }
        assert!(
            !v.to_string().contains("SENTINEL"),
            "{kind}: SSM's own words stay in the log: {v}"
        );
        assert!(
            diag.iter().any(|l| l.contains(kind)),
            "{kind}: the log names the cause: {diag:?}"
        );
        untouched(&w);
    }
    // TRIGGER PRESENT: the same exchange used to be a 503.
    println!(
        "SSM-DENIED TRIGGER PRESENT: AccessDeniedException on the Instances parameter answers 422 compute-not-set-up"
    );
}
