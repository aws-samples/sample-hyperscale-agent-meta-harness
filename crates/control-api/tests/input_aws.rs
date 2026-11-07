//! The DynamoDB reads and writes behind `POST /v1/runs/{runId}/input`: one event by id, one control by id, the re-arm of a
//! delivered control, and the newest events of a shard, against recorded HTTP exchanges (StaticReplayClient). No network.

#![allow(clippy::unwrap_used)]

use aws_smithy_http_client::test_util::{ReplayEvent, StaticReplayClient};
use aws_smithy_types::body::SdkBody;
use control_api::aws::DynamoStore;
use control_api::handler::{Store, StoreError};
use harness_protocol::api::internal::PendingControl;
use harness_protocol::envelope::Authority;
use harness_protocol::store::event_shard;
use serde_json::{Value, json};

const RUN: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Y";
const REQ: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1N";
const ACK: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT2A";
const CCF: &str = r#"{"__type":"com.amazonaws.dynamodb.v20120810#ConditionalCheckFailedException","message":"The conditional request failed"}"#;
const THROTTLED: &str =
    r#"{"__type":"com.amazonaws.dynamodb.v20120810#ThrottlingException","message":"slow down"}"#;

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
                        .header("content-type", "application/x-amz-json-1.0")
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

fn ddb(r: &StaticReplayClient) -> DynamoStore {
    DynamoStore {
        client: client!(aws_sdk_dynamodb, r),
        table: "hamh-live-harness".into(),
    }
}

fn bodies(r: &StaticReplayClient) -> Vec<Value> {
    r.actual_requests()
        .map(|q| serde_json::from_slice(q.body().bytes().unwrap_or_default()).unwrap())
        .collect()
}

fn shard() -> u8 {
    event_shard(Authority::Agent, Some("direct"))
}

/// A stored `needs-input` or acknowledgement event in DynamoDB's JSON.
fn event_item(event_id: &str, kind: &str) -> Value {
    json!({
        "PK": {"S": format!("EVT#{RUN}#{:02}", shard())}, "SK": {"S": event_id},
        "envelope": {"M": {
            "schemaVersion": {"N": "1"}, "eventId": {"S": event_id}, "runId": {"S": RUN}, "agentId": {"S": "direct"},
            "seq": {"N": "7"}, "ts": {"S": "2026-10-15T13:41:07Z"}, "type": {"S": "turn.progress"}, "authority": {"S": "agent"},
            "payload": {"M": {"kind": {"S": "inline"}, "value": {"M": {
                "turn": {"N": "1"}, "kind": {"S": kind}, "requestId": {"S": REQ}, "blocking": {"BOOL": true}
            }}}}
        }}
    })
}

fn control_item(delivered: bool) -> Value {
    let mut item = json!({
        "PK": {"S": format!("RUN#{RUN}")}, "SK": {"S": format!("CTRL#direct#{REQ}")},
        "runId": {"S": RUN}, "agentId": {"S": "direct"}, "controlId": {"S": REQ},
        "control": {"M": {"kind": {"S": "instruct"}, "requestId": {"S": REQ}, "action": {"S": "continue_with_prompt"},
                          "text": {"S": "poll the log"}, "origin": {"S": "user"}}},
        "createdAt": {"S": "2026-10-15T13:42:38Z"}
    });
    if delivered {
        item["deliveredAt"] = json!({"S": "2026-10-15T13:42:40Z"});
    }
    item
}

#[tokio::test]
async fn one_event_is_a_consistent_get_of_its_shard_partition_by_event_id() {
    let present = json!({"Item": event_item(REQ, "needs-input")}).to_string();
    let r = replay(vec![
        (200, present),
        (200, "{}".into()),
        (
            200,
            json!({"Item": {"PK": {"S": "x"}, "SK": {"S": REQ}, "envelope": {"S": "garbage"}}})
                .to_string(),
        ),
    ]);
    let s = ddb(&r);
    let e = s.get_event(RUN, shard(), REQ).await.unwrap().unwrap();
    assert_eq!(e.event_id, REQ);
    assert_eq!(e.agent_id.as_deref(), Some("direct"));
    assert_eq!(s.get_event(RUN, shard(), REQ).await.unwrap(), None);
    // An item that does not parse reads as absent, like the event cursor skips it.
    assert_eq!(s.get_event(RUN, shard(), REQ).await.unwrap(), None);
    let b = bodies(&r);
    assert_eq!(b[0]["TableName"], "hamh-live-harness");
    assert_eq!(
        b[0]["Key"],
        json!({"PK": {"S": format!("EVT#{RUN}#{:02}", shard())}, "SK": {"S": REQ}})
    );
    assert_eq!(b[0]["ConsistentRead"], true);
}

#[tokio::test]
async fn a_failed_event_read_is_unavailable() {
    let r = replay(vec![(400, THROTTLED.into())]);
    assert!(matches!(
        ddb(&r).get_event(RUN, shard(), REQ).await,
        Err(StoreError::Unavailable(_))
    ));
}

#[tokio::test]
async fn one_control_is_a_consistent_get_of_the_run_partition_by_agent_and_control_id() {
    let r = replay(vec![
        (200, json!({"Item": control_item(true)}).to_string()),
        (200, "{}".into()),
    ]);
    let s = ddb(&r);
    let c = s.get_control(RUN, "direct", REQ).await.unwrap().unwrap();
    assert_eq!(c.control_id, REQ);
    assert_eq!(c.delivered_at.as_deref(), Some("2026-10-15T13:42:40Z"));
    assert!(
        matches!(c.control, PendingControl::Instruct { .. }),
        "{:?}",
        c.control
    );
    assert_eq!(s.get_control(RUN, "direct", REQ).await.unwrap(), None);
    let b = bodies(&r);
    assert_eq!(
        b[0]["Key"],
        json!({"PK": {"S": format!("RUN#{RUN}")}, "SK": {"S": format!("CTRL#direct#{REQ}")}})
    );
    assert_eq!(b[0]["ConsistentRead"], true);
}

#[tokio::test]
async fn a_re_arm_removes_delivered_at_only_when_it_is_there() {
    let r = replay(vec![
        (200, "{}".into()),
        (400, CCF.into()),
        (400, THROTTLED.into()),
    ]);
    let s = ddb(&r);
    assert!(s.rearm_control(RUN, "direct", REQ).await.unwrap());
    assert!(
        !s.rearm_control(RUN, "direct", REQ).await.unwrap(),
        "already queued"
    );
    assert!(matches!(
        s.rearm_control(RUN, "direct", REQ).await,
        Err(StoreError::Unavailable(_))
    ));
    let b = bodies(&r);
    assert_eq!(
        b[0]["Key"],
        json!({"PK": {"S": format!("RUN#{RUN}")}, "SK": {"S": format!("CTRL#direct#{REQ}")}})
    );
    assert_eq!(b[0]["UpdateExpression"], "REMOVE deliveredAt");
    assert_eq!(b[0]["ConditionExpression"], "attribute_exists(deliveredAt)");
}

#[tokio::test]
async fn the_newest_events_are_read_newest_first_and_returned_oldest_first() {
    let page =
        json!({"Items": [event_item(ACK, "input-received"), event_item(REQ, "needs-input")]})
            .to_string();
    let r = replay(vec![(200, page)]);
    let s = ddb(&r);
    let events = s.latest_events(RUN, shard(), 200).await.unwrap();
    let ids: Vec<&str> = events.iter().map(|e| e.event_id.as_str()).collect();
    assert_eq!(ids, vec![REQ, ACK], "oldest first, like `events`");
    let b = bodies(&r);
    assert_eq!(b[0]["ScanIndexForward"], false);
    assert_eq!(b[0]["Limit"], 200);
    assert_eq!(b[0]["ConsistentRead"], true);
    assert_eq!(b[0]["KeyConditionExpression"], "PK = :pk");
    assert_eq!(
        b[0]["ExpressionAttributeValues"][":pk"],
        json!({"S": format!("EVT#{RUN}#{:02}", shard())})
    );
}

#[tokio::test]
async fn a_malformed_item_in_the_tail_is_skipped_and_a_failed_read_is_unavailable() {
    let page = json!({"Items": [
        {"PK": {"S": "x"}, "SK": {"S": ACK}, "envelope": {"S": "garbage"}},
        event_item(REQ, "needs-input")
    ]})
    .to_string();
    let r = replay(vec![(200, page), (400, THROTTLED.into())]);
    let s = ddb(&r);
    let events = s.latest_events(RUN, shard(), 10).await.unwrap();
    assert_eq!(events.len(), 1);
    assert!(matches!(
        s.latest_events(RUN, shard(), 10).await,
        Err(StoreError::Unavailable(_))
    ));
}
