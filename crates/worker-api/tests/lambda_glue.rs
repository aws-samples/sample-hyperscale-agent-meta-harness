//! Mapping between the API Gateway REST (v1) proxy event and the handler's request and response.

#![allow(clippy::unwrap_used)] // test assertions unwrap by design

use worker_api::handler::InternalResponse;
use worker_api::lambda::{to_internal, to_response};

const EVENT: &str = r#"{
  "resource": "/internal/creds", "path": "/internal/creds", "httpMethod": "POST",
  "headers": {"content-type": "application/json"}, "multiValueHeaders": {"content-type": ["application/json"]},
  "queryStringParameters": null, "multiValueQueryStringParameters": null, "pathParameters": null,
  "stageVariables": null, "isBase64Encoded": false, "body": "{\"runId\":\"R1\"}",
  "requestContext": {
    "accountId": "800000000022", "resourceId": "abc", "stage": "live", "requestId": "req-1",
    "resourcePath": "/internal/creds", "httpMethod": "POST", "apiId": "api123", "path": "/live/internal/creds",
    "identity": {"userArn": "arn:aws:sts::800000000022:assumed-role/hamh-live-harness-worker-session/R1.a1",
                 "sourceIp": "10.40.0.10", "vpceId": "vpce-0123"}
  }
}"#;

#[test]
fn the_caller_method_path_and_body_come_from_the_event() {
    let req = lambda_http::request::from_str(EVENT).unwrap();
    let internal = to_internal(&req);
    assert_eq!(internal.method, "POST");
    assert!(
        internal.path.ends_with("/internal/creds"),
        "{}",
        internal.path
    );
    assert_eq!(
        internal.caller_arn.as_deref(),
        Some("arn:aws:sts::800000000022:assumed-role/hamh-live-harness-worker-session/R1.a1")
    );
    assert_eq!(internal.body, "{\"runId\":\"R1\"}");
}

#[test]
fn an_event_without_identity_has_no_caller() {
    let event = EVENT.replace(r#""userArn": "arn:aws:sts::800000000022:assumed-role/hamh-live-harness-worker-session/R1.a1","#, "");
    let req = lambda_http::request::from_str(&event).unwrap();
    assert_eq!(to_internal(&req).caller_arn, None);
}

#[test]
fn responses_are_json_with_the_handler_status() {
    let resp = to_response(InternalResponse {
        status: 403,
        body: "{\"code\":\"x\"}".into(),
    })
    .unwrap();
    assert_eq!(resp.status(), 403);
    assert_eq!(resp.headers()["content-type"], "application/json");
}

#[test]
fn the_access_line_names_caller_route_and_status_and_never_the_body() {
    let event = EVENT.replace(
        r#""body": "{\"runId\":\"R1\"}""#,
        r#""body": "{\"nonce\":\"secret-nonce-value\"}""#,
    );
    let req = lambda_http::request::from_str(&event).unwrap();
    let internal = to_internal(&req);
    let line = worker_api::lambda::access_line(&internal, 403);
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(
        v["caller"],
        "arn:aws:sts::800000000022:assumed-role/hamh-live-harness-worker-session/R1.a1"
    );
    assert_eq!(v["method"], "POST");
    assert!(v["path"].as_str().unwrap().ends_with("/internal/creds"));
    assert_eq!(v["status"], 403);
    assert!(!line.contains("secret-nonce-value"));
    assert!(v.get("body").is_none());
}

#[test]
fn log_lines_are_the_access_line_then_one_json_line_per_diagnostic() {
    let req = lambda_http::request::from_str(EVENT).unwrap();
    let internal = to_internal(&req);
    let lines = worker_api::lambda::log_lines(
        &internal,
        200,
        &["controls for R1/a1 not read: AccessDenied".to_string()],
    );
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0], worker_api::lambda::access_line(&internal, 200));
    let d: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();
    assert!(d["path"].as_str().unwrap().ends_with("/internal/creds"));
    assert_eq!(d["diagnostic"], "controls for R1/a1 not read: AccessDenied");
    assert_eq!(worker_api::lambda::log_lines(&internal, 200, &[]).len(), 1);
}
