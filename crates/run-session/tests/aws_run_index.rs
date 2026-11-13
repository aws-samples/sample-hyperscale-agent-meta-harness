//! The DynamoDB update behind `touch_run_index` against recorded HTTP exchanges (StaticReplayClient): it addresses the
//! owner-keyed search item and sets only the outcome word and the time (and the key fields where the item is new), and a later
//! time is never overwritten by an earlier one.
#![allow(clippy::unwrap_used)]

use aws_smithy_http_client::test_util::{ReplayEvent, StaticReplayClient};
use aws_smithy_types::body::SdkBody;
use ham_core::run_index::{owner_pk, search_sk};
use harness_protocol::store::RunItem;
use run_session::aws::DynamoStore;
use run_session::ports::Store;
use serde_json::{Value, json};

const RUN: &str = "01M4X3Q7R9S0V1W2X3Y4Z5A6B7";
const OWNER: &str = "arn:aws:sts::700000000011:assumed-role/Admin/testuser";
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
        "target": {"mode": "s3-only"}, "status": "complete", "phase": "green",
        "enginePolicyVersion": 0, "createdAt": "2026-10-13T18:00:00Z", "version": 4
    }))
    .unwrap()
}

#[tokio::test]
async fn the_touch_sets_the_outcome_and_the_time_and_the_key_fields_of_a_new_item() {
    let r = replay(vec![(200, "{}")]);
    ddb(&r)
        .touch_run_index(&run(), Some("Cline reported done"), "2026-10-13T19:05:00Z")
        .await
        .unwrap();
    let b = &bodies(&r)[0];
    assert_eq!(b["TableName"], "hamh-direct-harness");
    assert_eq!(
        b["Key"],
        json!({"PK": {"S": owner_pk(OWNER)}, "SK": {"S": search_sk("2026-10-13T18:00:00Z", RUN)}})
    );
    let expr = b["UpdateExpression"].as_str().unwrap();
    assert!(expr.starts_with("SET "), "{expr}");
    // The key fields only where the item is new; the end fields always; the start's fields never.
    assert!(expr.contains("runId = if_not_exists(runId, :r)"), "{expr}");
    assert!(
        expr.contains("createdAt = if_not_exists(createdAt, :c)"),
        "{expr}"
    );
    assert!(
        expr.contains("lastActivity = :t") && expr.contains("outcome = :o"),
        "{expr}"
    );
    assert!(
        !expr.contains("promptHead") && !expr.contains("model") && !expr.contains("repository"),
        "{expr}"
    );
    assert_eq!(
        b["ExpressionAttributeValues"][":t"],
        json!({"S": "2026-10-13T19:05:00Z"})
    );
    assert_eq!(
        b["ExpressionAttributeValues"][":o"],
        json!({"S": "Cline reported done"})
    );
    assert_eq!(b["ExpressionAttributeValues"][":r"], json!({"S": RUN}));
    assert_eq!(
        b["ExpressionAttributeValues"][":c"],
        json!({"S": "2026-10-13T18:00:00Z"})
    );
    // An earlier time never replaces a later one (a delayed retry of a first attempt).
    // On equal times the write goes through only when it cannot replace a different outcome: the start and the end of a run may share
    // a second, and two attempts of the same end carry the same word.
    assert_eq!(
        b["ConditionExpression"],
        "attribute_not_exists(lastActivity) OR lastActivity < :t OR (lastActivity = :t AND (attribute_not_exists(outcome) OR outcome = :o))"
    );
    assert!(!b.to_string().contains("ownerPrincipalArn"), "{b}");
}

#[tokio::test]
async fn without_an_outcome_only_the_time_is_set() {
    let r = replay(vec![(200, "{}")]);
    ddb(&r)
        .touch_run_index(&run(), None, "2026-10-13T19:05:00Z")
        .await
        .unwrap();
    let b = &bodies(&r)[0];
    let expr = b["UpdateExpression"].as_str().unwrap();
    assert!(
        expr.contains("lastActivity = :t") && !expr.contains("outcome"),
        "{expr}"
    );
    assert!(b["ExpressionAttributeValues"].get(":o").is_none());
    // No outcome to compare: the placeholder is not in the condition either (DynamoDB refuses an unused or missing value).
    assert_eq!(
        b["ConditionExpression"],
        "attribute_not_exists(lastActivity) OR lastActivity < :t OR (lastActivity = :t AND attribute_not_exists(outcome))"
    );
}

#[tokio::test]
async fn a_later_time_already_there_is_not_an_error_and_a_failed_call_is() {
    let r = replay(vec![(400, CCF), (500, DDB_500)]);
    let s = ddb(&r);
    s.touch_run_index(&run(), None, "2026-10-13T19:05:00Z")
        .await
        .unwrap();
    assert!(
        s.touch_run_index(&run(), None, "2026-10-13T19:05:00Z")
            .await
            .is_err()
    );
}

/// The end of a run and the name of a run have separate writers: the touch never names an attribute the name op owns, so neither
/// can undo the other whichever comes first.
#[tokio::test]
async fn the_touch_never_names_the_name_attributes() {
    let r = replay(vec![(200, "{}"), (200, "{}")]);
    let s = ddb(&r);
    s.touch_run_index(&run(), Some("Cline reported done"), "2026-10-13T19:05:00Z")
        .await
        .unwrap();
    s.touch_run_index(&run(), None, "2026-10-13T19:05:00Z")
        .await
        .unwrap();
    for b in bodies(&r) {
        let text = b.to_string();
        for name_attribute in ["runName", "runNameSource", "runNameModel"] {
            assert!(!text.contains(name_attribute), "{name_attribute} in {text}");
        }
    }
}
