//! An event that was not made by API Gateway has no caller. A principal that may invoke the function directly can send any JSON, so the
//! function turns the identity of an event into a caller only when the event is whole, comes from this function's own account and carries nothing an
//! SDK invoke adds. This is validation, not authentication: a faithful copy of a real event is indistinguishable, and who may invoke the function is
//! settled by IAM (SECURITY.md). The operator's key-state question needs no caller and keeps working.
#![allow(clippy::unwrap_used)]

mod support;

use harness_protocol::research::route;
use lambda_http::RequestExt;
use lambda_runtime::Context;
use research_api::handler::{KEY_STATE_PATH, handle};
use research_api::lambda::to_request;
use serde_json::{Value, json};
use support::*;

const FUNCTION_ARN: &str =
    "arn:aws:lambda:us-west-2:111122223333:function:hamh-direct-research-api";

fn event(path: &str, body: &str, context: Value) -> String {
    json!({
        "resource": path, "path": format!("/direct{path}"), "httpMethod": "POST",
        "headers": {"content-type": "application/json"}, "multiValueHeaders": {"content-type": ["application/json"]},
        "queryStringParameters": null, "multiValueQueryStringParameters": null, "pathParameters": null,
        "stageVariables": null, "isBase64Encoded": false, "body": body,
        "requestContext": context
    })
    .to_string()
}

/// What API Gateway puts in the context of an IAM-authenticated request from a worker session.
fn real_context() -> Value {
    json!({
        "accountId": ACCOUNT, "resourceId": "abc", "stage": "direct", "requestId": "req-1",
        "resourcePath": "/internal/research/limits", "httpMethod": "POST", "apiId": "api123",
        "identity": {"userArn": caller_arn(RUN), "accessKey": session_key(), "sourceIp": "10.50.0.10", "vpceId": "vpce-0123"}
    })
}

fn invoked(raw: &str, function_arn: &str) -> lambda_http::Request {
    let mut context = Context::default();
    context.invoked_function_arn = function_arn.to_string();
    lambda_http::request::from_str(raw)
        .unwrap()
        .with_lambda_context(context)
}

/// The thinnest context that still parses as an API Gateway one: the account and the ARN a direct invoker wants to be taken for.
fn thin_claim() -> Value {
    json!({
        "accountId": ACCOUNT, "httpMethod": "POST", "requestTimeEpoch": 0,
        "identity": {"userArn": caller_arn(RUN)}
    })
}

/// Key-shaped stand-ins are made at run time from pieces, so no file holds a literal.
fn session_key() -> String {
    ["ASIA", "EXAMPLESESSIONKEY"].concat()
}

fn long_term_key() -> String {
    ["AKIA", "EXAMPLELONGTERMKEY"].concat()
}

const FIVE: [&str; 5] = [
    route::SEARCH,
    route::FETCH,
    route::REPO,
    route::JOB,
    route::LIMITS,
];

#[tokio::test]
async fn b639_a_hand_made_event_that_only_claims_a_worker_arn_is_refused_on_all_five_routes() {
    let d = deps(fixture_provider());
    // The claim a direct invoker writes: the worker's ARN and its account, nothing else an API Gateway event carries.
    let claim = thin_claim();
    for path in FIVE {
        let req = to_request(&invoked(&event(path, "{}", claim.clone()), FUNCTION_ARN));
        assert_eq!(req.caller_arn, None, "{path}: the claim is not a caller");
        let resp = handle(&d, &req).await;
        let v: Value = serde_json::from_str(&resp.body).unwrap();
        assert_eq!(v["code"], "disabled", "{path}: {}", resp.body);
        assert_eq!(resp.log.run, None, "{path}: no run was named");
    }
    assert!(
        d.blobs.keys().is_empty(),
        "a refused claim leaves no audit object: {:?}",
        d.blobs.keys()
    );
}

#[tokio::test]
async fn b639_a_claim_with_a_whole_context_from_another_account_has_no_caller() {
    let mut ctx = real_context();
    ctx["accountId"] = json!("999988887777");
    ctx["identity"]["userArn"] = json!(caller_arn(RUN).replace(ACCOUNT, "999988887777"));
    let raw = event(route::LIMITS, "{}", ctx);
    // The function runs in 111122223333: both fields of the event agree with each other and still do not agree with the function.
    let req = to_request(&invoked(&raw, FUNCTION_ARN));
    assert_eq!(req.caller_arn, None);
}

#[tokio::test]
async fn b639_an_event_without_the_function_context_or_with_an_sdk_client_context_has_no_caller() {
    let raw = event(route::LIMITS, "{}", real_context());
    // No context at all: fail closed.
    let bare = lambda_http::request::from_str(&raw).unwrap();
    assert_eq!(to_request(&bare).caller_arn, None);
    // A function ARN that names no account.
    assert_eq!(to_request(&invoked(&raw, "not-an-arn")).caller_arn, None);
    // The caller supplied a client context (an SDK invoke can; API Gateway never does).
    let mut context = Context::default();
    context.invoked_function_arn = FUNCTION_ARN.to_string();
    context.client_context = Some(serde_json::from_value(json!({})).unwrap());
    let with_client = lambda_http::request::from_str(&raw)
        .unwrap()
        .with_lambda_context(context);
    assert_eq!(to_request(&with_client).caller_arn, None);
}

#[tokio::test]
async fn b639_a_context_missing_a_field_that_api_gateway_always_sends_has_no_caller() {
    for (what, path) in [
        ("apiId", "/apiId"),
        ("stage", "/stage"),
        ("requestId", "/requestId"),
        ("accessKey", "/identity/accessKey"),
        ("sourceIp", "/identity/sourceIp"),
    ] {
        let mut ctx = real_context();
        let (parent, key) = path.rsplit_once('/').unwrap();
        let holder = if parent.is_empty() {
            &mut ctx
        } else {
            ctx.pointer_mut(parent).unwrap()
        };
        holder.as_object_mut().unwrap().remove(key);
        let req = to_request(&invoked(&event(route::LIMITS, "{}", ctx), FUNCTION_ARN));
        assert_eq!(req.caller_arn, None, "without {what}");
    }
    // An access key that is not a temporary session key (ASIA...) is no worker session either.
    let mut ctx = real_context();
    ctx["identity"]["accessKey"] = json!(long_term_key());
    let req = to_request(&invoked(&event(route::LIMITS, "{}", ctx), FUNCTION_ARN));
    assert_eq!(req.caller_arn, None);
}

#[tokio::test]
async fn b639_a_real_shaped_event_keeps_its_caller_and_the_route_answers() {
    let d = deps(fixture_provider());
    let req = to_request(&invoked(
        &event(route::LIMITS, "{}", real_context()),
        FUNCTION_ARN,
    ));
    assert_eq!(req.caller_arn.as_deref(), Some(caller_arn(RUN).as_str()));
    assert_eq!(req.account_id.as_deref(), Some(ACCOUNT));
    let resp = handle(&d, &req).await;
    assert_eq!(resp.status, 200, "{}", resp.body);
    assert_eq!(resp.log.run.as_deref(), Some(RUN));
}

#[tokio::test]
async fn b639_the_operator_key_state_event_has_no_caller_and_is_answered() {
    let d = deps(fixture_provider());
    let raw = std::fs::read_to_string(format!(
        "{}/../../fixtures/research/key-state-event.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let req = to_request(&invoked(&raw, FUNCTION_ARN));
    assert_eq!(req.path, KEY_STATE_PATH);
    assert_eq!(req.caller_arn, None);
    let resp = handle(&d, &req).await;
    assert_eq!(resp.status, 200, "{}", resp.body);
}

#[tokio::test]
async fn b639_a_key_state_event_that_claims_a_whole_worker_identity_is_refused_as_an_unknown_route()
{
    let d = deps(fixture_provider());
    let req = to_request(&invoked(
        &event(KEY_STATE_PATH, "{}", real_context()),
        FUNCTION_ARN,
    ));
    assert!(req.caller_arn.is_some(), "a whole identity is a caller");
    assert_eq!(handle(&d, &req).await.status, 404);
}

#[tokio::test]
async fn b639_a_key_state_event_with_a_partial_identity_is_no_worker_and_the_operator_path_answers()
{
    let d = deps(fixture_provider());
    let claim = thin_claim();
    let req = to_request(&invoked(&event(KEY_STATE_PATH, "{}", claim), FUNCTION_ARN));
    assert_eq!(req.caller_arn, None);
    assert_eq!(handle(&d, &req).await.status, 200);
}
