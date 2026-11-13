//! The DynamoDB calls behind the run name against recorded HTTP exchanges (StaticReplayClient): one consistent read of the
//! stored name, and one conditional `UpdateItem` of the owner-keyed search item that sets the three name attributes only when the
//! item holds none, and the key fields only where the item is new. NAME is a DynamoDB reserved word, so the attribute is `runName`
//! and the expressions never hold a bare `name`.
#![allow(clippy::unwrap_used)]

use aws_smithy_http_client::test_util::{ReplayEvent, StaticReplayClient};
use aws_smithy_types::body::SdkBody;
use ham_core::run_index::{owner_pk, search_sk};
use harness_protocol::store::RunItem;
use run_session::aws::DynamoStore;
use run_session::ports::Store;
use serde_json::{Value, json};

const RUN: &str = "01M4X3Q7R9S0V1W2X3Y4Z5A6B7";
const OWNER: &str = "arn:aws:sts::111122223333:assumed-role/Admin/tester";
const CREATED: &str = "2026-10-13T18:00:00Z";
const NAME: &str = "Exponential backoff retry for S3 uploader";
const ROUTE: &str = "us.anthropic.claude-haiku-5-5";
const CCF: &str = r#"{"__type":"com.amazonaws.dynamodb.v20120810#ConditionalCheckFailedException","message":"The conditional request failed"}"#;
const DDB_500: &str =
    r#"{"__type":"com.amazonaws.dynamodb.v20120810#InternalServerError","message":"boom"}"#;

fn replay(responses: Vec<(u16, &str)>) -> StaticReplayClient {
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
                        .header("content-type", "application/x-amz-json-1.0")
                        .body(SdkBody::from(body.to_string()))
                        .unwrap(),
                )
            })
            .collect(),
    )
}

fn ddb(r: &StaticReplayClient) -> DynamoStore {
    DynamoStore {
        client: aws_sdk_dynamodb::Client::from_conf(
            aws_sdk_dynamodb::Config::builder()
                .behavior_version_latest()
                .region(aws_sdk_dynamodb::config::Region::new("us-east-1"))
                .credentials_provider(aws_sdk_dynamodb::config::Credentials::for_tests())
                .retry_config(aws_sdk_dynamodb::config::retry::RetryConfig::disabled())
                .http_client(r.clone())
                .build(),
        ),
        table: "hamh-direct-harness".into(),
        research_table: None,
    }
}

fn bodies(r: &StaticReplayClient) -> Vec<Value> {
    r.actual_requests()
        .map(|q| serde_json::from_slice(q.body().bytes().unwrap_or_default()).unwrap())
        .collect()
}

fn run() -> RunItem {
    serde_json::from_value(json!({
        "runId": RUN, "ownerPrincipalArn": OWNER, "promptSha256": "cd".repeat(32),
        "target": {"mode": "s3-only"}, "status": "active", "phase": "planning",
        "enginePolicyVersion": 0, "createdAt": CREATED, "version": 1
    }))
    .unwrap()
}

fn key() -> Value {
    json!({"PK": {"S": owner_pk(OWNER)}, "SK": {"S": search_sk(CREATED, RUN)}})
}

/// The words of an expression, split on everything that is not a letter, a digit or an underscore.
fn tokens(expression: &str) -> Vec<String> {
    expression
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

#[tokio::test]
async fn the_name_is_one_conditional_update_of_the_search_item() {
    let r = replay(vec![(200, "{}")]);
    let stored = ddb(&r)
        .put_run_name(&run(), NAME, "ai", ROUTE)
        .await
        .unwrap();
    assert!(stored);
    let all = bodies(&r);
    assert_eq!(all.len(), 1, "one call");
    let b = &all[0];
    assert_eq!(b["TableName"], "hamh-direct-harness");
    assert_eq!(
        b["Key"],
        key(),
        "the owner's partition and the item control-api writes at the start"
    );

    let expr = b["UpdateExpression"].as_str().unwrap();
    assert!(expr.starts_with("SET "), "{expr}");
    // The key fields only where the item is new (the name may land before the start's write), then the three attributes.
    assert!(expr.contains("runId = if_not_exists(runId, :r)"), "{expr}");
    assert!(
        expr.contains("createdAt = if_not_exists(createdAt, :c)"),
        "{expr}"
    );
    assert!(expr.contains("runName = :n"), "{expr}");
    assert!(expr.contains("runNameSource = :s"), "{expr}");
    assert!(expr.contains("runNameModel = :m"), "{expr}");
    assert_eq!(
        expr.matches(" = ").count(),
        5,
        "five assignments and no more: {expr}"
    );
    assert_eq!(b["ConditionExpression"], "attribute_not_exists(runName)");

    // Literal attribute names: no placeholder map, and no token that DynamoDB reads as the reserved word NAME.
    assert!(b.get("ExpressionAttributeNames").is_none(), "{b}");
    for text in [expr, b["ConditionExpression"].as_str().unwrap()] {
        for t in tokens(text) {
            assert!(!t.eq_ignore_ascii_case("name"), "a bare `{t}` in {text}");
        }
    }
    assert_eq!(
        b["ExpressionAttributeValues"],
        json!({
            ":r": {"S": RUN}, ":c": {"S": CREATED},
            ":n": {"S": NAME}, ":s": {"S": "ai"}, ":m": {"S": ROUTE}
        })
    );

    // Another writer's attributes are never named: the end of a run owns the time and the outcome, the start owns the head.
    for other in [
        "lastActivity",
        "outcome",
        "promptHead",
        "repository",
        "target",
        "ownerPrincipalArn",
        "title",
    ] {
        assert!(!b.to_string().contains(other), "{other} in {b}");
    }
}

#[tokio::test]
async fn a_name_already_there_is_not_an_error_and_a_failed_call_is() {
    let r = replay(vec![(400, CCF), (500, DDB_500)]);
    let s = ddb(&r);
    assert!(
        !s.put_run_name(&run(), NAME, "ai", ROUTE).await.unwrap(),
        "the condition failed: the item holds a name"
    );
    let err = s.put_run_name(&run(), NAME, "ai", ROUTE).await.unwrap_err();
    assert_eq!(err.error_type(), "RunSessionUnavailable");
}

#[tokio::test]
async fn the_stored_name_is_read_consistently_with_one_projection() {
    let r = replay(vec![
        (
            200,
            r#"{"Item":{"runName":{"S":"Exponential backoff retry for S3 uploader"}}}"#,
        ),
        (200, "{}"),
        (
            200,
            r#"{"Item":{"runId":{"S":"01M4X3Q7R9S0V1W2X3Y4Z5A6B7"}}}"#,
        ),
        (200, r#"{"Item":{"runName":{"N":"5"}}}"#),
        (500, DDB_500),
    ]);
    let s = ddb(&r);
    assert_eq!(s.run_name(&run()).await.unwrap().as_deref(), Some(NAME));
    assert_eq!(s.run_name(&run()).await.unwrap(), None, "no item");
    assert_eq!(
        s.run_name(&run()).await.unwrap(),
        None,
        "an item without a name"
    );
    assert_eq!(
        s.run_name(&run()).await.unwrap(),
        None,
        "a name of another type is no name"
    );
    assert_eq!(
        s.run_name(&run()).await.unwrap_err().error_type(),
        "RunSessionUnavailable"
    );
    let first = &bodies(&r)[0];
    assert_eq!(first["TableName"], "hamh-direct-harness");
    assert_eq!(first["Key"], key());
    assert_eq!(
        first["ConsistentRead"], true,
        "a retry must see the name the first invocation wrote"
    );
    assert_eq!(
        first["ProjectionExpression"], "runName",
        "only the name is read, never the item's other fields"
    );
    assert!(first.get("ExpressionAttributeNames").is_none(), "{first}");
}
