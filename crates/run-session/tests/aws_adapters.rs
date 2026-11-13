//! The DynamoDB, KMS, S3, AgentCore, SSM and CodeArtifact adapters of the Lambda against recorded HTTP exchanges (the
//! aws-smithy `StaticReplayClient`; no network), and the wiring that builds them.
//! Each test sends the real request through the
//! real SDK client and reads what the adapter put on the wire: the condition expressions, the keys, the encryption
//! context, the headers; and what it made of each answer, including the errors.
#![allow(clippy::unwrap_used)]

mod support;

use aws_credential_types::Credentials;
use aws_credential_types::provider::error::CredentialsError;
use aws_credential_types::provider::{ProvideCredentials, SharedCredentialsProvider, future};
use aws_smithy_http_client::test_util::{ReplayEvent, StaticReplayClient};
use aws_smithy_types::body::SdkBody;
use dispatcher::endpoints::{EndpointState, Pin};
use harness_protocol::envelope::{Envelope, EventType};
use harness_protocol::goal::{GoalPhase, GoalStatus};
use harness_protocol::orchestration::{ExecutionEnd, RunAbortedPayload};
use harness_protocol::store::{AgentItem, EventItem, GoalItem, RunItem, TurnItem};
use run_session::aws::{
    AgentCoreSessions, CodeArtifactTokens, DynamoStore, KmsTokens, RuntimeParameter, S3Blobs,
    runtime_arn, runtime_error, runtime_id,
};
use run_session::events;
use run_session::lambda::handle_payload;
use run_session::ports::{Blobs, PackageTokens, Sessions, Store, Tokens};
use run_session::wiring::{Packages, wire};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use worker_api::handler::TokenContext;

const RUN: &str = "01M4X3Q7R9S0V1W2X3Y4Z5A6B7";
const TABLE: &str = "hamh-direct-harness";
const ARN: &str = "arn:aws:bedrock-agentcore:us-east-1:700000000011:runtime/hamh_direct_harness_worker_python-AbCdEf";
const CCF: &str = r#"{"__type":"com.amazonaws.dynamodb.v20120810#ConditionalCheckFailedException","message":"The conditional request failed"}"#;
const DDB_500: &str =
    r#"{"__type":"com.amazonaws.dynamodb.v20120810#InternalServerError","message":"boom"}"#;

// ---- replay plumbing --------------------------------------------------------------------------------------------------

fn event(status: u16, content_type: &str, body: &str) -> ReplayEvent {
    ReplayEvent::new(
        http::Request::builder()
            .uri("https://example.invalid/")
            .body(SdkBody::empty())
            .unwrap(),
        http::Response::builder()
            .status(status)
            .header("content-type", content_type)
            .body(SdkBody::from(body.to_string()))
            .unwrap(),
    )
}

fn json10(responses: Vec<(u16, &str)>) -> StaticReplayClient {
    StaticReplayClient::new(
        responses
            .into_iter()
            .map(|(s, b)| event(s, "application/x-amz-json-1.0", b))
            .collect(),
    )
}

fn replay(events: Vec<ReplayEvent>) -> StaticReplayClient {
    StaticReplayClient::new(events)
}

macro_rules! client {
    ($sdk:ident, $replay:expr) => {
        $sdk::Client::from_conf(
            $sdk::Config::builder()
                .behavior_version_latest()
                .region($sdk::config::Region::new("us-east-1"))
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
        table: TABLE.into(),
        research_table: None,
    }
}

fn kms(r: &StaticReplayClient) -> KmsTokens {
    KmsTokens {
        client: client!(aws_sdk_kms, r),
        key_id: "arn:aws:kms:us-east-1:700000000011:key/data".into(),
    }
}

fn s3(r: &StaticReplayClient) -> S3Blobs {
    S3Blobs {
        client: client!(aws_sdk_s3, r),
        bucket: "hamh-direct-harness-700000000011".into(),
    }
}

fn agentcore(r: &StaticReplayClient) -> AgentCoreSessions {
    AgentCoreSessions {
        client: client!(aws_sdk_bedrockagentcore, r),
        control: client!(aws_sdk_bedrockagentcorecontrol, r),
        conflict_base_ms: 0,
        runtime_parameter: None,
        instances_parameter: None,
        capacity_provider_parameter: None,
    }
}

/// The JSON body of every request the client sent, in order.
fn bodies(r: &StaticReplayClient) -> Vec<Value> {
    r.actual_requests()
        .map(|q| {
            serde_json::from_slice(q.body().bytes().unwrap_or_default()).unwrap_or(Value::Null)
        })
        .collect()
}

fn uris(r: &StaticReplayClient) -> Vec<String> {
    r.actual_requests().map(|q| q.uri().to_string()).collect()
}

fn target(r: &StaticReplayClient, i: usize) -> String {
    r.actual_requests()
        .nth(i)
        .unwrap()
        .headers()
        .get("x-amz-target")
        .unwrap()
        .to_string()
}

/// A plain JSON value as a DynamoDB attribute value.
fn av(v: &Value) -> Value {
    match v {
        Value::String(s) => json!({"S": s}),
        Value::Number(n) => json!({"N": n.to_string()}),
        Value::Bool(b) => json!({"BOOL": b}),
        Value::Null => json!({"NULL": true}),
        Value::Array(a) => json!({"L": a.iter().map(av).collect::<Vec<_>>()}),
        Value::Object(o) => {
            json!({"M": o.iter().map(|(k, v)| (k.clone(), av(v))).collect::<Map<_, _>>()})
        }
    }
}

/// A DynamoDB item (`{"Item": ...}`) from the plain JSON of a protocol item and its key.
fn item_json(plain: Value, pk: &str, sk: &str) -> Value {
    let mut m = plain.as_object().cloned().unwrap();
    m.insert("PK".into(), json!(pk));
    m.insert("SK".into(), json!(sk));
    Value::Object(m.iter().map(|(k, v)| (k.clone(), av(v))).collect())
}

fn item_reply(plain: Value, pk: &str, sk: &str) -> String {
    json!({"Item": item_json(plain, pk, sk)}).to_string()
}

fn run_json() -> Value {
    json!({
        "runId": RUN, "ownerPrincipalArn": "arn:aws:sts::700000000011:assumed-role/Admin/testuser",
        "promptSha256": "ab".repeat(32), "target": {"mode": "repo", "repo": "cc:hamh-e2e-python-calc"},
        "status": "active", "phase": "planning", "enginePolicyVersion": 0, "createdAt": "2026-10-13T18:00:00Z", "version": 3
    })
}

fn run_item() -> RunItem {
    serde_json::from_value(run_json()).unwrap()
}

fn goal_json() -> Value {
    json!({"runId": RUN, "objective": "p", "requirements": [], "specSha256": "", "acceptanceSha256": "",
        "status": "active", "phase": "planning", "version": 2})
}

fn agent_json() -> Value {
    json!({"runId": RUN, "agentId": "direct", "taskId": "root", "attempt": 1, "runtimeArn": ARN,
        "runtimeSessionId": format!("{RUN}-direct-g1"), "generation": 1, "engine": "cline", "engineVersion": "3.0.71",
        "transport": "acp", "modelId": "us.anthropic.claude-opus-5-5", "pool": "direct", "status": "running", "version": 1})
}

fn turn_json() -> Value {
    json!({"runId": RUN, "agentId": "direct", "n": 1, "taskTokenCiphertext": "Y3Q=",
        "startedAt": "2026-10-13T18:00:00Z", "rulesDigest": "", "version": 1})
}

fn agent_item() -> AgentItem {
    serde_json::from_value(agent_json()).unwrap()
}

fn turn_item() -> TurnItem {
    serde_json::from_value(turn_json()).unwrap()
}

fn run_pk() -> String {
    format!("RUN#{RUN}")
}

// ---- DynamoDB: reads ----------------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_four_item_reads_are_consistent_gets_of_their_keys_and_a_missing_item_is_none() {
    let r = json10(vec![
        (200, &item_reply(run_json(), &run_pk(), "META")),
        (200, &item_reply(goal_json(), &run_pk(), "GOAL#root")),
        (200, &item_reply(agent_json(), &run_pk(), "AGENT#direct")),
        (
            200,
            &item_reply(turn_json(), &run_pk(), "TURN#direct#000001"),
        ),
        (200, "{}"),
        (200, "{}"),
        (200, "{}"),
        (200, "{}"),
    ]);
    let s = ddb(&r);
    assert_eq!(s.get_run(RUN).await.unwrap().unwrap().version, 3);
    assert_eq!(s.get_root_goal(RUN).await.unwrap().unwrap().version, 2);
    assert_eq!(
        s.get_agent(RUN, "direct").await.unwrap().unwrap().agent_id,
        "direct"
    );
    assert_eq!(s.get_turn(RUN, "direct", 1).await.unwrap().unwrap().n, 1);
    assert!(s.get_run(RUN).await.unwrap().is_none());
    assert!(s.get_root_goal(RUN).await.unwrap().is_none());
    assert!(s.get_agent(RUN, "direct").await.unwrap().is_none());
    assert!(s.get_turn(RUN, "direct", 1).await.unwrap().is_none());
    let b = bodies(&r);
    let sks = ["META", "GOAL#root", "AGENT#direct", "TURN#direct#000001"];
    for (i, sk) in sks.iter().chain(sks.iter()).enumerate() {
        assert_eq!(b[i]["TableName"], TABLE);
        assert_eq!(
            b[i]["Key"],
            json!({"PK": {"S": run_pk()}, "SK": {"S": sk}}),
            "{i}"
        );
        assert_eq!(b[i]["ConsistentRead"], true, "{i}");
        assert_eq!(target(&r, i), "DynamoDB_20120810.GetItem");
    }
}

#[tokio::test]
async fn an_item_that_does_not_read_back_and_a_failing_get_are_unavailable() {
    let r = json10(vec![
        (
            200,
            &json!({"Item": {"PK": {"S": run_pk()}, "SK": {"S": "META"}}}).to_string(),
        ),
        (500, DDB_500),
    ]);
    let s = ddb(&r);
    let e = s.get_run(RUN).await.unwrap_err();
    assert_eq!(e.error_type(), "RunSessionUnavailable");
    let e = s.get_run(RUN).await.unwrap_err();
    assert_eq!(e.error_type(), "RunSessionUnavailable");
    assert!(e.to_string().contains("boom"), "{e}");
}

fn envelope_item(e: &Envelope, shard_pk: &str) -> Value {
    let plain = serde_json::to_value(EventItem {
        envelope: e.clone(),
    })
    .unwrap();
    item_json(plain, shard_pk, &e.event_id)
}

#[tokio::test]
async fn the_request_sha_is_read_from_the_run_created_event_on_the_first_shard() {
    let mut created = events::goal_active(RUN, 1_790_000_000_000);
    created.event_type = EventType::RunCreated;
    created.payload = harness_protocol::envelope::Payload::Inline {
        value: json!({"requestSha256": "ab".repeat(32), "mode": "direct"}),
    };
    let other = events::goal_active(RUN, 1_790_000_000_001);
    let pk = format!("EVT#{RUN}#00");
    let page = json!({"Items": [
        envelope_item(&other, &pk),
        {"PK": {"S": pk}, "SK": {"S": "garbage"}},
        envelope_item(&created, &pk),
    ]});
    let r = json10(vec![(200, &page.to_string())]);
    assert_eq!(
        ddb(&r).run_created_request_sha256(RUN).await.unwrap(),
        Some("ab".repeat(32))
    );
    let b = &bodies(&r)[0];
    assert_eq!(b["KeyConditionExpression"], "PK = :pk");
    assert_eq!(b["ExpressionAttributeValues"][":pk"], json!({"S": pk}));
    assert_eq!(b["ConsistentRead"], true);
    assert_eq!(b["Limit"], 25);
    assert_eq!(target(&r, 0), "DynamoDB_20120810.Query");
}

#[tokio::test]
async fn a_run_without_a_run_created_event_or_without_its_sha_reads_as_none() {
    let pk = format!("EVT#{RUN}#00");
    let mut created = events::goal_active(RUN, 1_790_000_000_000);
    created.event_type = EventType::RunCreated;
    created.payload = harness_protocol::envelope::Payload::Inline {
        value: json!({"mode": "direct"}),
    };
    let only_other =
        json!({"Items": [envelope_item(&events::goal_active(RUN, 1_790_000_000_001), &pk)]});
    let no_sha = json!({"Items": [envelope_item(&created, &pk)]});
    let r = json10(vec![
        (200, &only_other.to_string()),
        (200, "{}"),
        (200, &no_sha.to_string()),
        (500, DDB_500),
    ]);
    let s = ddb(&r);
    assert_eq!(s.run_created_request_sha256(RUN).await.unwrap(), None);
    assert_eq!(s.run_created_request_sha256(RUN).await.unwrap(), None);
    assert_eq!(s.run_created_request_sha256(RUN).await.unwrap(), None);
    assert_eq!(
        s.run_created_request_sha256(RUN)
            .await
            .unwrap_err()
            .error_type(),
        "RunSessionUnavailable"
    );
}

#[tokio::test]
async fn the_link_of_a_runs_pull_request_is_read_from_its_delivery_pr_event_newest_first() {
    let pk = format!("EVT#{RUN}#00");
    let pr = events::delivery_pr(RUN, 1_790_000_000_000, "https://example.invalid/pr/7");
    let ready = events::goal_active(RUN, 1_790_000_000_005);
    let page = json!({"Items": [
        envelope_item(&ready, &pk),
        {"PK": {"S": pk.clone()}, "SK": {"S": "garbage"}},
        envelope_item(&pr, &pk),
    ]});
    let r = json10(vec![(200, &page.to_string())]);
    assert_eq!(
        ddb(&r).delivery_pr_url(RUN).await.unwrap().as_deref(),
        Some("https://example.invalid/pr/7")
    );
    let b = &bodies(&r)[0];
    assert_eq!(b["KeyConditionExpression"], "PK = :pk");
    assert_eq!(b["ExpressionAttributeValues"][":pk"], json!({"S": pk}));
    assert_eq!(b["ConsistentRead"], true);
    assert_eq!(
        b["ScanIndexForward"], false,
        "the newest events first: the link is near the end"
    );
    assert_eq!(target(&r, 0), "DynamoDB_20120810.Query");
}

#[tokio::test]
async fn the_link_search_follows_the_pages_and_reads_none_for_a_run_that_opened_no_pull_request() {
    let pk = format!("EVT#{RUN}#00");
    let other = events::goal_active(RUN, 1_790_000_000_005);
    let pr = events::delivery_pr(RUN, 1_790_000_000_000, "https://example.invalid/pr/9");
    let first = json!({"Items": [envelope_item(&other, &pk)], "LastEvaluatedKey": {"PK": {"S": pk.clone()}, "SK": {"S": other.event_id.clone()}}});
    let second = json!({"Items": [envelope_item(&pr, &pk)]});
    let r = json10(vec![(200, &first.to_string()), (200, &second.to_string())]);
    assert_eq!(
        ddb(&r).delivery_pr_url(RUN).await.unwrap().as_deref(),
        Some("https://example.invalid/pr/9")
    );
    assert_eq!(
        bodies(&r)[1]["ExclusiveStartKey"],
        json!({"PK": {"S": pk}, "SK": {"S": other.event_id}}),
        "the second page starts where the first ended"
    );
    let r = json10(vec![(
        200,
        &json!({"Items": [envelope_item(&other, &format!("EVT#{RUN}#00"))]}).to_string(),
    )]);
    assert_eq!(ddb(&r).delivery_pr_url(RUN).await.unwrap(), None);
    let r = json10(vec![(500, DDB_500)]);
    assert_eq!(
        ddb(&r).delivery_pr_url(RUN).await.unwrap_err().error_type(),
        "RunSessionUnavailable"
    );
}

// ---- DynamoDB: the agent and the turn -------------------------------------------------------------------------------------

#[tokio::test]
async fn the_agent_item_is_created_once_and_an_existing_one_is_left_alone() {
    let r = json10(vec![(200, "{}"), (400, CCF), (500, DDB_500)]);
    let s = ddb(&r);
    s.put_agent(&agent_item()).await.unwrap();
    s.put_agent(&agent_item()).await.unwrap();
    assert_eq!(
        s.put_agent(&agent_item()).await.unwrap_err().error_type(),
        "RunSessionUnavailable"
    );
    let b = bodies(&r);
    assert_eq!(b[0]["ConditionExpression"], "attribute_not_exists(PK)");
    assert_eq!(b[0]["Item"]["PK"], json!({"S": run_pk()}));
    assert_eq!(b[0]["Item"]["SK"], json!({"S": "AGENT#direct"}));
    assert_eq!(b[0]["Item"]["pool"], json!({"S": "direct"}));
}

#[tokio::test]
async fn the_agent_is_ended_stopped_and_revoked_and_a_missing_binding_is_fine() {
    let r = json10(vec![(200, "{}"), (400, CCF), (500, DDB_500)]);
    let s = ddb(&r);
    s.end_agent(RUN, "direct").await.unwrap();
    s.end_agent(RUN, "direct").await.unwrap();
    assert_eq!(
        s.end_agent(RUN, "direct").await.unwrap_err().error_type(),
        "RunSessionUnavailable"
    );
    let b = &bodies(&r)[0];
    assert_eq!(
        b["Key"],
        json!({"PK": {"S": run_pk()}, "SK": {"S": "AGENT#direct"}})
    );
    assert_eq!(
        b["UpdateExpression"],
        "SET #s = :stopped, #c.#r = :true, #v = #v + :one"
    );
    assert_eq!(b["ConditionExpression"], "attribute_exists(#c)");
    assert_eq!(
        b["ExpressionAttributeValues"][":stopped"],
        json!({"S": "stopped"})
    );
    assert_eq!(
        b["ExpressionAttributeValues"][":true"],
        json!({"BOOL": true})
    );
    assert_eq!(b["ExpressionAttributeNames"]["#r"], "revoked");
}

#[tokio::test]
async fn a_turn_is_created_once_its_token_replaced_and_its_invocation_recorded() {
    let r = json10(vec![
        (200, "{}"),
        (400, CCF),
        (200, "{}"),
        (500, DDB_500),
        (200, "{}"),
        (500, DDB_500),
    ]);
    let s = ddb(&r);
    assert!(s.create_turn(&turn_item()).await.unwrap());
    assert!(!s.create_turn(&turn_item()).await.unwrap());
    s.replace_turn_token(RUN, "direct", 1, "bmV3")
        .await
        .unwrap();
    assert_eq!(
        s.replace_turn_token(RUN, "direct", 1, "bmV3")
            .await
            .unwrap_err()
            .error_type(),
        "RunSessionUnavailable"
    );
    s.mark_turn_invoked(RUN, "direct", 1).await.unwrap();
    assert_eq!(
        s.mark_turn_invoked(RUN, "direct", 1)
            .await
            .unwrap_err()
            .error_type(),
        "RunSessionUnavailable"
    );
    let b = bodies(&r);
    assert_eq!(b[0]["ConditionExpression"], "attribute_not_exists(PK)");
    assert_eq!(b[0]["Item"]["SK"], json!({"S": "TURN#direct#000001"}));
    let key = json!({"PK": {"S": run_pk()}, "SK": {"S": "TURN#direct#000001"}});
    assert_eq!(b[2]["Key"], key);
    assert_eq!(b[2]["UpdateExpression"], "SET taskTokenCiphertext = :c");
    assert_eq!(b[2]["ConditionExpression"], "attribute_exists(PK)");
    assert_eq!(
        b[2]["ExpressionAttributeValues"][":c"],
        json!({"S": "bmV3"})
    );
    assert_eq!(b[4]["Key"], key);
    assert_eq!(b[4]["UpdateExpression"], "SET invokedAt = :now");
    assert_eq!(b[4]["ConditionExpression"], "attribute_exists(PK)");
    let now = b[4]["ExpressionAttributeValues"][":now"]["S"]
        .as_str()
        .unwrap();
    assert!(
        now.len() == 20 && now.ends_with('Z') && now.starts_with("20"),
        "an RFC 3339 second: {now}"
    );
}

// ---- DynamoDB: the pin, the phase, the statuses ----------------------------------------------------------------------------

fn pin() -> Pin {
    Pin {
        version: "7".into(),
        endpoint: "v7".into(),
    }
}

#[tokio::test]
async fn the_runtime_pin_is_set_once_and_a_lost_race_returns_the_pin_that_won() {
    let mut pinned = run_json();
    pinned["runtimeVersion"] = json!("8");
    pinned["runtimeEndpoint"] = json!("v8");
    let r = json10(vec![
        (200, "{}"),
        (400, CCF),
        (200, &item_reply(pinned, &run_pk(), "META")),
    ]);
    let s = ddb(&r);
    assert_eq!(s.pin_runtime(RUN, &pin()).await.unwrap(), pin());
    assert_eq!(
        s.pin_runtime(RUN, &pin()).await.unwrap(),
        Pin {
            version: "8".into(),
            endpoint: "v8".into()
        }
    );
    let b = bodies(&r);
    assert_eq!(
        b[0]["UpdateExpression"],
        "SET runtimeVersion = :v, runtimeEndpoint = :e"
    );
    assert_eq!(
        b[0]["ConditionExpression"],
        "attribute_exists(PK) AND attribute_not_exists(runtimeEndpoint)"
    );
    assert_eq!(b[0]["ExpressionAttributeValues"][":v"], json!({"S": "7"}));
    assert_eq!(b[0]["ExpressionAttributeValues"][":e"], json!({"S": "v7"}));
    assert_eq!(b[0]["Key"]["SK"], json!({"S": "META"}));
}

#[tokio::test]
async fn a_pin_that_cannot_be_set_for_want_of_a_run_item_is_invalid_and_other_errors_retry() {
    let r = json10(vec![
        (400, CCF),
        (200, "{}"),
        (400, CCF),
        (200, &item_reply(run_json(), &run_pk(), "META")),
        (500, DDB_500),
    ]);
    let s = ddb(&r);
    // the condition failed and the run is gone
    assert_eq!(
        s.pin_runtime(RUN, &pin()).await.unwrap_err().error_type(),
        "RunSessionInvalid"
    );
    // the condition failed and the run holds no endpoint
    assert_eq!(
        s.pin_runtime(RUN, &pin()).await.unwrap_err().error_type(),
        "RunSessionInvalid"
    );
    assert_eq!(
        s.pin_runtime(RUN, &pin()).await.unwrap_err().error_type(),
        "RunSessionUnavailable"
    );
}

#[tokio::test]
async fn the_phase_is_set_by_name() {
    let r = json10(vec![(200, "{}"), (500, DDB_500)]);
    let s = ddb(&r);
    s.set_run_phase(RUN, GoalPhase::Green).await.unwrap();
    assert_eq!(
        s.set_run_phase(RUN, GoalPhase::Delivering)
            .await
            .unwrap_err()
            .error_type(),
        "RunSessionUnavailable"
    );
    let b = &bodies(&r)[0];
    assert_eq!(b["UpdateExpression"], "SET #phase = :p");
    assert_eq!(b["ConditionExpression"], "attribute_exists(PK)");
    assert_eq!(b["ExpressionAttributeNames"]["#phase"], "phase");
    assert_eq!(b["ExpressionAttributeValues"][":p"], json!({"S": "green"}));
}

fn cancelled(codes: &[&str]) -> String {
    json!({"__type": "com.amazonaws.dynamodb.v20120810#TransactionCanceledException", "message": "cancelled",
        "CancellationReasons": codes.iter().map(|c| json!({"Code": c})).collect::<Vec<_>>()})
    .to_string()
}

#[tokio::test]
async fn the_status_moves_the_run_item_the_goal_and_the_event_in_one_transaction() {
    let e = events::goal_complete(RUN, 1_790_000_000_000);
    let r = json10(vec![
        (200, "{}"),
        (400, &cancelled(&["ConditionalCheckFailed", "None", "None"])),
        (400, &cancelled(&["TransactionConflict", "None", "None"])),
        (500, DDB_500),
    ]);
    let s = ddb(&r);
    let run = run_item();
    assert!(
        s.set_run_status(&run, GoalStatus::Complete, &e)
            .await
            .unwrap()
    );
    assert!(
        !s.set_run_status(&run, GoalStatus::Complete, &e)
            .await
            .unwrap(),
        "a lost race"
    );
    for _ in 0..2 {
        assert_eq!(
            s.set_run_status(&run, GoalStatus::Complete, &e)
                .await
                .unwrap_err()
                .error_type(),
            "RunSessionUnavailable"
        );
    }
    let b = &bodies(&r)[0];
    assert_eq!(target(&r, 0), "DynamoDB_20120810.TransactWriteItems");
    let items = b["TransactItems"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    let meta = &items[0]["Update"];
    assert_eq!(
        meta["Key"],
        json!({"PK": {"S": run_pk()}, "SK": {"S": "META"}})
    );
    assert_eq!(
        meta["UpdateExpression"],
        "SET #status = :to, #v = #v + :one"
    );
    assert_eq!(meta["ConditionExpression"], "#status = :from");
    assert_eq!(
        meta["ExpressionAttributeValues"][":to"],
        json!({"S": "complete"})
    );
    assert_eq!(
        meta["ExpressionAttributeValues"][":from"],
        json!({"S": "active"})
    );
    let goal = &items[1]["Update"];
    assert_eq!(goal["Key"]["SK"], json!({"S": "GOAL#root"}));
    assert_eq!(goal["ConditionExpression"], "attribute_exists(PK)");
    assert!(goal["ExpressionAttributeValues"].get(":from").is_none());
    let put = &items[2]["Put"];
    assert_eq!(put["ConditionExpression"], "attribute_not_exists(PK)");
    assert_eq!(put["Item"]["PK"], json!({"S": format!("EVT#{RUN}#00")}));
    assert_eq!(put["Item"]["SK"], json!({"S": e.event_id}));
}

#[tokio::test]
async fn an_event_is_a_plain_put_of_its_item() {
    let e = events::goal_active(RUN, 1_790_000_000_000);
    let r = json10(vec![(200, "{}"), (500, DDB_500)]);
    let s = ddb(&r);
    s.put_event(&e).await.unwrap();
    assert_eq!(
        s.put_event(&e).await.unwrap_err().error_type(),
        "RunSessionUnavailable"
    );
    let b = &bodies(&r)[0];
    assert!(
        b.get("ConditionExpression").is_none(),
        "the same id again is the same item"
    );
    assert_eq!(b["Item"]["SK"], json!({"S": e.event_id}));
    assert_eq!(b["Item"]["PK"], json!({"S": format!("EVT#{RUN}#00")}));
}

fn cleared_run() -> RunItem {
    let mut run = run_item();
    run.status = GoalStatus::Cleared;
    run.version += 1;
    run.aborted = Some(Box::new(RunAbortedPayload::new(
        ExecutionEnd::Aborted,
        Some("States.Runtime"),
        Some("cause"),
        "2026-10-13T19:00:00Z",
    )));
    run
}

#[tokio::test]
async fn an_aborted_run_is_cleared_with_its_goal_and_events_under_the_versions_it_read() {
    let ev = [events::goal_status(
        RUN,
        1,
        5,
        GoalStatus::Cleared,
        GoalPhase::Green,
    )];
    let goal: GoalItem = serde_json::from_value(goal_json()).unwrap();
    let mut goal_after = goal.clone();
    goal_after.status = GoalStatus::Cleared;
    goal_after.version += 1;
    let r = json10(vec![
        (200, "{}"),
        (400, &cancelled(&["ConditionalCheckFailed", "None", "None"])),
        (200, "{}"),
        (500, DDB_500),
    ]);
    let s = ddb(&r);
    let run = cleared_run();
    assert!(s.abort_run(&run, Some(&goal_after), &ev).await.unwrap());
    assert!(
        !s.abort_run(&run, Some(&goal_after), &ev).await.unwrap(),
        "a writer got in between"
    );
    // no goal, no aborted marker, no events: the run item alone
    let mut bare = cleared_run();
    bare.aborted = None;
    assert!(s.abort_run(&bare, None, &[]).await.unwrap());
    assert_eq!(
        s.abort_run(&run, None, &ev).await.unwrap_err().error_type(),
        "RunSessionUnavailable"
    );
    let b = bodies(&r);
    let items = b[0]["TransactItems"].as_array().unwrap();
    assert_eq!(items.len(), 3, "run, goal, one event");
    let u = &items[0]["Update"];
    assert_eq!(u["Key"]["SK"], json!({"S": "META"}));
    assert_eq!(
        u["UpdateExpression"],
        "SET #status = :status, #v = :next, #aborted = :aborted"
    );
    assert_eq!(u["ConditionExpression"], "#v = :v");
    assert_eq!(
        u["ExpressionAttributeValues"][":status"],
        json!({"S": "cleared"})
    );
    assert_eq!(u["ExpressionAttributeValues"][":next"], json!({"N": "4"}));
    assert_eq!(u["ExpressionAttributeValues"][":v"], json!({"N": "3"}));
    assert_eq!(
        u["ExpressionAttributeValues"][":aborted"]["M"]["status"],
        json!({"S": "ABORTED"})
    );
    assert_eq!(
        u["ExpressionAttributeValues"][":aborted"]["M"]["error"],
        json!({"S": "States.Runtime"})
    );
    let g = &items[1]["Update"];
    assert_eq!(g["Key"]["SK"], json!({"S": "GOAL#root"}));
    assert_eq!(g["UpdateExpression"], "SET #status = :status, #v = :next");
    assert_eq!(g["ExpressionAttributeValues"][":v"], json!({"N": "2"}));
    assert_eq!(g["ExpressionAttributeValues"][":next"], json!({"N": "3"}));
    assert!(items[2]["Put"]["Item"]["SK"]["S"].as_str().is_some());
    let lone = b[2]["TransactItems"].as_array().unwrap();
    assert_eq!(lone.len(), 1);
    assert!(
        lone[0]["Update"]["UpdateExpression"]
            .as_str()
            .unwrap()
            .find("#aborted")
            .is_none()
    );
}

// ---- KMS ----------------------------------------------------------------------------------------------------------------

fn ctx(agent: &str) -> TokenContext {
    TokenContext {
        run_id: RUN.into(),
        agent_id: agent.into(),
        turn: 1,
    }
}

#[tokio::test]
async fn a_token_is_encrypted_under_the_turn_context_and_a_missing_ciphertext_is_unavailable() {
    let r = json10(vec![
        (200, r#"{"CiphertextBlob": "Y2lwaGVy", "KeyId": "k"}"#),
        (200, r#"{"KeyId": "k"}"#),
        (400, r#"{"__type":"DisabledException","message":"off"}"#),
    ]);
    let t = kms(&r);
    assert_eq!(
        t.encrypt("tok-1", &ctx("direct")).await.unwrap(),
        "Y2lwaGVy"
    );
    assert_eq!(
        t.encrypt("tok-1", &ctx("direct"))
            .await
            .unwrap_err()
            .error_type(),
        "RunSessionUnavailable"
    );
    assert_eq!(
        t.encrypt("tok-1", &ctx("direct"))
            .await
            .unwrap_err()
            .error_type(),
        "RunSessionUnavailable"
    );
    let b = &bodies(&r)[0];
    assert_eq!(b["KeyId"], "arn:aws:kms:us-east-1:700000000011:key/data");
    assert_eq!(b["Plaintext"], "dG9rLTE=");
    assert_eq!(
        b["EncryptionContext"],
        json!({"run": RUN, "agent": "direct", "turn": "1"})
    );
}

#[tokio::test]
async fn a_stored_nonce_is_decrypted_under_the_context_it_was_sealed_with() {
    let r = json10(vec![(
        200,
        r#"{"Plaintext": "bm9uY2UtMQ==", "KeyId": "k"}"#,
    )]);
    let nonce = kms(&r)
        .decrypt("Y2lwaGVy", &ctx("direct-nonce"))
        .await
        .unwrap();
    assert_eq!(nonce, "nonce-1");
    let b = &bodies(&r)[0];
    assert_eq!(b["CiphertextBlob"], "Y2lwaGVy");
    assert_eq!(b["KeyId"], "arn:aws:kms:us-east-1:700000000011:key/data");
    assert_eq!(
        b["EncryptionContext"],
        json!({"run": RUN, "agent": "direct-nonce", "turn": "1"})
    );
}

#[tokio::test]
async fn a_ciphertext_nobody_can_read_is_invalid_and_a_kms_fault_is_unavailable() {
    let r = json10(vec![
        (
            400,
            r#"{"__type":"InvalidCiphertextException","message":"bad"}"#,
        ),
        (
            400,
            r#"{"__type":"IncorrectKeyException","message":"wrong key"}"#,
        ),
        (500, r#"{"__type":"KMSInternalException","message":"down"}"#),
        (200, r#"{"KeyId": "k"}"#),
        (200, r#"{"Plaintext": "//4=", "KeyId": "k"}"#),
    ]);
    let t = kms(&r);
    let c = ctx("direct-nonce");
    assert_eq!(
        t.decrypt("Y2lwaGVy", &c).await.unwrap_err().error_type(),
        "RunSessionInvalid"
    );
    assert_eq!(
        t.decrypt("Y2lwaGVy", &c).await.unwrap_err().error_type(),
        "RunSessionInvalid"
    );
    assert_eq!(
        t.decrypt("Y2lwaGVy", &c).await.unwrap_err().error_type(),
        "RunSessionUnavailable"
    );
    assert_eq!(
        t.decrypt("Y2lwaGVy", &c).await.unwrap_err().error_type(),
        "RunSessionUnavailable"
    );
    assert_eq!(
        t.decrypt("Y2lwaGVy", &c).await.unwrap_err().error_type(),
        "RunSessionInvalid",
        "not text"
    );
    // text that is not base64 never leaves
    let sent = r.actual_requests().count();
    assert_eq!(
        t.decrypt("not base64 !", &c)
            .await
            .unwrap_err()
            .error_type(),
        "RunSessionInvalid"
    );
    assert_eq!(r.actual_requests().count(), sent);
}

// ---- S3 -------------------------------------------------------------------------------------------------------------------

const XML: &str = "application/xml";

fn s3_error(status: u16, code: &str) -> ReplayEvent {
    event(
        status,
        XML,
        &format!("<Error><Code>{code}</Code><Message>m</Message></Error>"),
    )
}

#[tokio::test]
async fn an_object_is_read_whole_and_a_missing_key_is_none() {
    let r = replay(vec![
        event(200, "application/json", "{\"a\":1}"),
        s3_error(404, "NoSuchKey"),
        s3_error(500, "InternalError"),
    ]);
    let b = s3(&r);
    assert_eq!(
        b.get("runs/x/y.json").await.unwrap(),
        Some(b"{\"a\":1}".to_vec())
    );
    assert_eq!(b.get("runs/x/y.json").await.unwrap(), None);
    assert_eq!(
        b.get("runs/x/y.json").await.unwrap_err().error_type(),
        "RunSessionUnavailable"
    );
    let u = uris(&r);
    assert!(
        u[0].contains("hamh-direct-harness-700000000011") && u[0].contains("runs/x/y.json"),
        "{u:?}"
    );
}

#[tokio::test]
async fn a_key_exists_when_it_answers_a_head_and_not_when_it_answers_not_found() {
    let r = replay(vec![
        event(200, XML, ""),
        event(404, XML, ""),
        event(403, XML, ""),
    ]);
    let b = s3(&r);
    assert!(b.exists("runs/x/bundle").await.unwrap());
    assert!(!b.exists("runs/x/bundle").await.unwrap());
    assert_eq!(
        b.exists("runs/x/bundle").await.unwrap_err().error_type(),
        "RunSessionUnavailable"
    );
    for q in r.actual_requests() {
        assert_eq!(q.method(), "HEAD");
    }
}

#[tokio::test]
async fn an_object_is_written_only_when_the_key_is_absent() {
    let r = replay(vec![
        event(200, XML, ""),
        s3_error(412, "PreconditionFailed"),
        s3_error(500, "InternalError"),
    ]);
    let b = s3(&r);
    assert!(
        b.put_if_absent("runs/x/evidence/direct.json", b"{}")
            .await
            .unwrap()
    );
    assert!(
        !b.put_if_absent("runs/x/evidence/direct.json", b"{}")
            .await
            .unwrap(),
        "it existed"
    );
    assert_eq!(
        b.put_if_absent("runs/x/evidence/direct.json", b"{}")
            .await
            .unwrap_err()
            .error_type(),
        "RunSessionUnavailable"
    );
    let q = r.actual_requests().next().unwrap();
    assert_eq!(q.method(), "PUT");
    assert_eq!(q.headers().get("if-none-match"), Some("*"));
    assert_eq!(q.headers().get("content-type"), Some("application/json"));
    assert!(q.uri().contains("runs/x/evidence/direct.json"));
}

// ---- AgentCore ------------------------------------------------------------------------------------------------------------

fn endpoint_json(status: &str, live_version: &str) -> String {
    json!({
        "liveVersion": live_version, "agentRuntimeEndpointArn": format!("{ARN}/runtime-endpoint/live"),
        "agentRuntimeArn": ARN, "status": status, "createdAt": "2026-10-13T10:00:00Z",
        "lastUpdatedAt": "2026-10-13T10:00:00Z", "name": "live", "id": "live"
    })
    .to_string()
}

#[test]
fn a_runtime_id_is_the_last_segment_of_a_runtime_arn() {
    assert_eq!(
        runtime_id(ARN),
        Some("hamh_direct_harness_worker_python-AbCdEf")
    );
    assert_eq!(runtime_id("arn:aws:s3:::bucket"), None);
    assert_eq!(
        runtime_id("arn:aws:bedrock-agentcore:us-east-1:1:runtime/"),
        None
    );
    assert_eq!(
        runtime_id("arn:aws:bedrock-agentcore:us-east-1:1:runtime/a/b"),
        None
    );
}

#[tokio::test]
async fn an_endpoint_is_read_from_the_control_plane_and_an_absent_one_is_none() {
    let missing = r#"{"__type": "ResourceNotFoundException", "message": "Agent endpoint with name v8 not found"}"#;
    let r = json10(vec![
        (200, &endpoint_json("READY", "7")),
        (200, &endpoint_json("CREATING", "")),
        (404, missing),
        (
            500,
            r#"{"__type": "InternalServerException", "message": "down"}"#,
        ),
    ]);
    let s = agentcore(&r);
    assert_eq!(
        s.endpoint(ARN, "live").await.unwrap(),
        Some(EndpointState {
            name: "live".into(),
            status: "READY".into(),
            live_version: Some("7".into())
        })
    );
    assert_eq!(
        s.endpoint(ARN, "v7").await.unwrap().unwrap().live_version,
        None,
        "an empty version is none"
    );
    assert_eq!(s.endpoint(ARN, "v8").await.unwrap(), None);
    assert_eq!(
        s.endpoint(ARN, "live").await.unwrap_err().error_type(),
        "RunSessionUnavailable"
    );
    let u = uris(&r);
    assert!(
        u[0].contains("/runtimes/hamh_direct_harness_worker_python-AbCdEf/runtime-endpoints/live"),
        "{u:?}"
    );
    assert!(u[2].ends_with("/runtime-endpoints/v8"), "{u:?}");
    // neither a foreign ARN nor a version number leaves
    let sent = r.actual_requests().count();
    assert_eq!(
        s.endpoint("arn:aws:s3:::b", "live")
            .await
            .unwrap_err()
            .error_type(),
        "RunSessionInvalid"
    );
    assert_eq!(
        s.endpoint(ARN, "7").await.unwrap_err().error_type(),
        "RunSessionInvalid"
    );
    assert_eq!(r.actual_requests().count(), sent);
}

#[tokio::test]
async fn a_session_is_invoked_and_stopped_at_the_runs_endpoint() {
    let r = json10(vec![
        (
            200,
            r#"{"runId": "R", "agentId": "direct", "turn": 1, "accepted": true}"#,
        ),
        (200, "{}"),
        (
            404,
            r#"{"__type": "ResourceNotFoundException", "message": "gone"}"#,
        ),
        (
            409,
            r#"{"__type": "RetryableConflictException", "message": "Session operation in progress"}"#,
        ),
        (200, "{}"),
    ]);
    let s = agentcore(&r);
    let session = format!("{RUN}-direct-g1");
    let body = s
        .invoke(ARN, "v7", &session, b"{\"kind\":\"direct\"}")
        .await
        .unwrap();
    assert!(String::from_utf8(body).unwrap().contains("accepted"));
    assert!(s.stop(ARN, "v7", &session).await.unwrap());
    assert!(
        !s.stop(ARN, "v7", &session).await.unwrap(),
        "an ended session"
    );
    assert!(
        s.stop(ARN, "v7", &session).await.unwrap(),
        "after a conflict the stop is retried"
    );
    let reqs: Vec<_> = r.actual_requests().collect();
    assert_eq!(reqs.len(), 5);
    for q in &reqs {
        assert!(q.uri().contains("qualifier=v7"), "{}", q.uri());
    }
    assert_eq!(reqs[0].body().bytes().unwrap(), b"{\"kind\":\"direct\"}");
    assert_eq!(
        reqs[0]
            .headers()
            .get("x-amzn-bedrock-agentcore-runtime-session-id"),
        Some(session.as_str())
    );
}

#[tokio::test]
async fn a_bare_version_is_invalid_and_never_sent() {
    let r = json10(vec![]);
    let s = agentcore(&r);
    for q in ["7", "", "v-7"] {
        assert_eq!(
            s.invoke(ARN, q, "S", b"{}").await.unwrap_err().error_type(),
            "RunSessionInvalid",
            "{q:?}"
        );
        assert_eq!(
            s.stop(ARN, q, "S").await.unwrap_err().error_type(),
            "RunSessionInvalid",
            "{q:?}"
        );
    }
    assert_eq!(r.actual_requests().count(), 0);
}

#[tokio::test]
async fn a_stop_that_fails_for_any_reason_but_not_found_is_an_error() {
    let r = json10(vec![(
        500,
        r#"{"__type": "InternalServerException", "message": "down"}"#,
    )]);
    let e = agentcore(&r).stop(ARN, "v7", "S").await.unwrap_err();
    assert_eq!(e.error_type(), "RunSessionUnavailable");
}

#[test]
fn what_the_runtime_answered_says_whether_the_invocation_can_be_retried() {
    let msg = |code: u16| {
        format!(
            "InvokeAgentRuntime: service error: Received error ({code}) from runtime. Check the logs"
        )
    };
    // agentd refused the request itself: no retry helps
    assert_eq!(runtime_error(msg(400)).error_type(), "RunSessionInvalid");
    assert_eq!(runtime_error(msg(422)).error_type(), "RunSessionInvalid");
    assert_eq!(runtime_error(msg(499)).error_type(), "RunSessionInvalid");
    // busy (409) is a conflict: the run's own turn is running; 429 and 5xx pass on a retry
    assert_eq!(runtime_error(msg(409)).error_type(), "RunSessionConflict");
    assert_eq!(
        runtime_error(msg(429)).error_type(),
        "RunSessionUnavailable"
    );
    assert_eq!(
        runtime_error(msg(500)).error_type(),
        "RunSessionUnavailable"
    );
    assert_eq!(
        runtime_error(msg(503)).error_type(),
        "RunSessionUnavailable"
    );
    // anything that is not the runtime's own answer
    assert_eq!(
        runtime_error("InvokeAgentRuntime: timed out".into()).error_type(),
        "RunSessionUnavailable"
    );
    assert_eq!(
        runtime_error("Received error (abc) from runtime".into()).error_type(),
        "RunSessionUnavailable"
    );
    assert_eq!(
        runtime_error("Received error (".into()).error_type(),
        "RunSessionUnavailable"
    );
}

#[tokio::test]
async fn a_runtime_that_answers_with_an_error_status_is_classified_by_that_status() {
    let runtime = |code: u16| {
        (
            424,
            json!({"__type": "RuntimeClientError", "message": format!("Received error ({code}) from runtime. Please check your CloudWatch logs")}).to_string(),
        )
    };
    let all: Vec<(u16, String)> = vec![runtime(409), runtime(400), runtime(500)];
    let r = json10(all.iter().map(|(s, b)| (*s, b.as_str())).collect());
    let s = agentcore(&r);
    assert_eq!(
        s.invoke(ARN, "v7", "S", b"{}")
            .await
            .unwrap_err()
            .error_type(),
        "RunSessionConflict"
    );
    assert_eq!(
        s.invoke(ARN, "v7", "S", b"{}")
            .await
            .unwrap_err()
            .error_type(),
        "RunSessionInvalid"
    );
    assert_eq!(
        s.invoke(ARN, "v7", "S", b"{}")
            .await
            .unwrap_err()
            .error_type(),
        "RunSessionUnavailable"
    );
}

// ---- SSM ------------------------------------------------------------------------------------------------------------------

fn parameter(value: Option<&str>) -> String {
    match value {
        Some(v) => json!({"Parameter": {"Name": "/hamh/direct/runtime-arn", "Type": "String", "Value": v, "Version": 1}}).to_string(),
        None => json!({"Parameter": {"Name": "/hamh/direct/runtime-arn", "Type": "String", "Version": 1}}).to_string(),
    }
}

#[tokio::test]
async fn the_runtime_arn_is_read_from_its_parameter_and_must_be_a_runtime_arn() {
    let r = json10(vec![
        (200, &parameter(Some(ARN))),
        (200, &parameter(Some("arn:aws:s3:::bucket"))),
        (200, &parameter(None)),
        (200, "{}"),
        (
            400,
            r#"{"__type": "ParameterNotFound", "message": "missing"}"#,
        ),
    ]);
    let ssm = client!(aws_sdk_ssm, r);
    assert_eq!(
        runtime_arn(&ssm, "/hamh/direct/runtime-arn").await.unwrap(),
        ARN
    );
    for _ in 0..3 {
        let e = runtime_arn(&ssm, "/hamh/direct/runtime-arn")
            .await
            .unwrap_err();
        assert!(e.contains("holds no runtime ARN"), "{e}");
    }
    let e = runtime_arn(&ssm, "/hamh/direct/runtime-arn")
        .await
        .unwrap_err();
    assert!(
        e.contains("ParameterNotFound") || e.contains("missing"),
        "{e}"
    );
    assert_eq!(bodies(&r)[0]["Name"], "/hamh/direct/runtime-arn");
}

fn ssm_source(r: &StaticReplayClient) -> RuntimeParameter {
    RuntimeParameter {
        client: client!(aws_sdk_ssm, r),
        name: "/hamh/direct/runtime-arn/worker-python".into(),
    }
}

#[tokio::test]
async fn the_sessions_adapter_reads_the_runtime_arn_when_asked_and_types_what_went_wrong() {
    let r = json10(vec![
        (200, &parameter(Some(ARN))),
        (
            400,
            r#"{"__type": "ParameterNotFound", "message": "no such parameter"}"#,
        ),
        (200, &parameter(Some("arn:aws:s3:::bucket"))),
        (200, &parameter(None)),
        // An AgentCore ARN of another resource type, and a runtime endpoint ARN, are not runtime ARNs either.
        (
            200,
            &parameter(Some(
                "arn:aws:bedrock-agentcore:us-east-1:700000000011:gateway/hamh-direct-gateway",
            )),
        ),
        (
            200,
            &parameter(Some(
                "arn:aws:bedrock-agentcore:us-east-1:700000000011:runtime/a/runtime-endpoint/live",
            )),
        ),
        (
            500,
            r#"{"__type": "InternalServerError", "message": "boom"}"#,
        ),
        (
            400,
            r#"{"__type": "ThrottlingException", "message": "Rate exceeded"}"#,
        ),
    ]);
    let s = AgentCoreSessions {
        runtime_parameter: Some(ssm_source(&r)),
        ..agentcore(&r)
    };
    assert_eq!(s.runtime_arn().await.unwrap().as_deref(), Some(ARN));
    // The parameter is absent: the stage is half deployed. This run can never start, so it is not retried (Invalid) and the
    // reason names the parameter.
    let e = s.runtime_arn().await.unwrap_err();
    assert_eq!(e.error_type(), "RunSessionInvalid", "{e}");
    assert!(
        e.to_string()
            .contains("/hamh/direct/runtime-arn/worker-python")
            && e.to_string().contains("not deployed"),
        "{e}"
    );
    // A value that is not a runtime ARN is the same: the stage is not ready for this run.
    for _ in 0..4 {
        let e = s.runtime_arn().await.unwrap_err();
        assert_eq!(e.error_type(), "RunSessionInvalid", "{e}");
        assert!(e.to_string().contains("holds no runtime ARN"), "{e}");
    }
    // Anything else may pass on a retry.
    for _ in 0..2 {
        let e = s.runtime_arn().await.unwrap_err();
        assert_eq!(e.error_type(), "RunSessionUnavailable", "{e}");
    }
    // One read per ask: a parameter written later is seen by a function that is already warm.
    assert_eq!(r.actual_requests().count(), 8);
    assert_eq!(
        bodies(&r)[0]["Name"],
        "/hamh/direct/runtime-arn/worker-python"
    );
}

#[tokio::test]
async fn an_adapter_with_no_parameter_has_no_lookup() {
    let r = json10(vec![]);
    assert_eq!(agentcore(&r).runtime_arn().await.unwrap(), None);
    assert_eq!(r.actual_requests().count(), 0);
}

// ---- CodeArtifact -----------------------------------------------------------------------------------------------------------

fn sdk_config(
    creds: Option<SharedCredentialsProvider>,
    http: Option<&StaticReplayClient>,
) -> aws_config::SdkConfig {
    let mut b = aws_config::SdkConfig::builder()
        .behavior_version(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new("us-west-2"))
        .retry_config(aws_config::retry::RetryConfig::disabled());
    if let Some(c) = creds {
        b = b.credentials_provider(c);
    }
    if let Some(h) = http {
        b = b.http_client(h.clone());
    }
    b.build()
}

fn test_creds() -> SharedCredentialsProvider {
    SharedCredentialsProvider::new(Credentials::for_tests())
}

fn lookup(pairs: &[(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
    let m: BTreeMap<&'static str, &'static str> = pairs.iter().copied().collect();
    move |k| m.get(k).map(|v| (*v).to_string())
}

#[test]
fn the_package_index_is_read_from_the_functions_environment() {
    let none = CodeArtifactTokens::from_lookup(&sdk_config(None, None), &lookup(&[]));
    assert!(
        none.is_none(),
        "no credentials provider, nothing to mint with"
    );
    let bare = CodeArtifactTokens::from_lookup(&sdk_config(Some(test_creds()), None), &lookup(&[]))
        .unwrap();
    assert!(
        bare.index.is_none(),
        "no PACKAGES_DOMAIN: a stage without a package index"
    );
    assert_eq!(bare.region, "us-west-2");
    let full = CodeArtifactTokens::from_lookup(
        &sdk_config(Some(test_creds()), None),
        &lookup(&[
            ("PACKAGES_DOMAIN", "hamh-direct"),
            ("PACKAGES_OWNER", "700000000011"),
            ("PACKAGES_REPOSITORY", "pkgs"),
            ("PACKAGES_ENDPOINT", "http://127.0.0.1:9"),
        ]),
    )
    .unwrap();
    let ix = full.index.unwrap();
    assert_eq!(
        (
            ix.domain.as_str(),
            ix.owner.as_str(),
            ix.repository.as_str(),
            ix.api_endpoint.as_deref()
        ),
        (
            "hamh-direct",
            "700000000011",
            "pkgs",
            Some("http://127.0.0.1:9")
        )
    );
    // the process environment of the test holds none of them
    let env = CodeArtifactTokens::from_env(&sdk_config(Some(test_creds()), None)).unwrap();
    assert!(env.index.is_none());
    // a region-less config falls back to the stage's
    let b = aws_config::SdkConfig::builder()
        .behavior_version(aws_config::BehaviorVersion::latest())
        .credentials_provider(test_creds())
        .build();
    let t = CodeArtifactTokens::from_lookup(&b, &lookup(&[])).unwrap();
    assert_eq!(t.region, dispatcher::stage::REGION);
}

/// A one-request HTTP server: answers `status` and `body`, returns the request head it read.
async fn serve_once(
    status: u16,
    body: impl Into<String>,
) -> (String, tokio::task::JoinHandle<String>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let body: String = body.into();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let h = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = sock.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let reply = format!(
            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        sock.write_all(reply.as_bytes()).await.unwrap();
        sock.shutdown().await.ok();
        String::from_utf8_lossy(&buf).to_string()
    });
    (url, h)
}

fn index_at(url: Option<String>) -> Option<agentd::packages::PackagesIndex> {
    Some(agentd::packages::PackagesIndex {
        domain: "hamh-direct".into(),
        owner: "700000000011".into(),
        repository: "packages".into(),
        api_endpoint: url,
    })
}

#[tokio::test]
async fn the_package_token_is_minted_with_the_lambdas_own_credentials() {
    let token = ["tok.en-", "1234567890"].concat();
    let (url, seen) = serve_once(
        200,
        format!(r#"{{"authorizationToken":"{token}","expiration":1790341000}}"#),
    )
    .await;
    let t = CodeArtifactTokens {
        index: index_at(Some(url)),
        region: "us-west-2".into(),
        credentials: test_creds(),
    };
    assert_eq!(t.token(1200).await.unwrap(), Some(token));
    let head = seen.await.unwrap();
    assert!(
        head.starts_with("POST /v1/authorization-token?domain=hamh-direct&domain-owner=700000000011&duration=1200 "),
        "{head}"
    );
    assert!(
        head.to_lowercase()
            .contains("authorization: aws4-hmac-sha256"),
        "signed: {head}"
    );
    assert!(
        head.contains("/us-west-2/codeartifact/aws4_request"),
        "{head}"
    );
}

#[tokio::test]
async fn a_package_token_that_cannot_be_minted_is_a_message_the_op_logs() {
    let (url, seen) = serve_once(403, r#"{"message":"not authorized"}"#).await;
    let t = CodeArtifactTokens {
        index: index_at(Some(url)),
        region: "us-west-2".into(),
        credentials: test_creds(),
    };
    let e = t.token(1200).await.unwrap_err();
    assert!(e.contains("403") && e.contains("not authorized"), "{e}");
    seen.await.unwrap();
    // credentials that will not load
    #[derive(Debug)]
    struct NoCreds;
    impl ProvideCredentials for NoCreds {
        fn provide_credentials<'a>(&'a self) -> future::ProvideCredentials<'a>
        where
            Self: 'a,
        {
            future::ProvideCredentials::ready(Err(CredentialsError::not_loaded("no role")))
        }
    }
    let t = CodeArtifactTokens {
        index: index_at(None),
        region: "us-west-2".into(),
        credentials: SharedCredentialsProvider::new(NoCreds),
    };
    let e = t.token(1200).await.unwrap_err();
    assert!(e.starts_with("run-session credentials:"), "{e}");
}

#[tokio::test]
async fn a_stage_without_a_package_index_mints_nothing_and_never_asks_for_credentials() {
    #[derive(Debug)]
    struct Panics;
    impl ProvideCredentials for Panics {
        fn provide_credentials<'a>(&'a self) -> future::ProvideCredentials<'a>
        where
            Self: 'a,
        {
            panic!("no credentials are needed");
        }
    }
    let t = CodeArtifactTokens {
        index: None,
        region: "us-west-2".into(),
        credentials: SharedCredentialsProvider::new(Panics),
    };
    assert_eq!(t.token(900).await.unwrap(), None);
    assert_eq!(Packages::CodeArtifact(t).token(900).await.unwrap(), None);
    assert_eq!(Packages::Off.token(900).await.unwrap(), None);
}

// ---- the wiring -------------------------------------------------------------------------------------------------------------

fn full_env() -> BTreeMap<&'static str, &'static str> {
    BTreeMap::from([
        ("HAMH_STAGE", "direct"),
        ("TABLE_NAME", TABLE),
        ("BUCKET_NAME", "hamh-direct-harness-700000000011"),
        (
            "DATA_KEY_ARN",
            "arn:aws:kms:us-east-1:700000000011:key/data",
        ),
        (
            "WORKER_SESSION_ROLE_ARN",
            "arn:aws:iam::700000000011:role/hamh-direct-harness-worker-session",
        ),
        (
            "RUNTIME_ARN_PARAMETER",
            "/hamh/direct/runtime-arn/worker-python",
        ),
        (
            "INTERNAL_API_URL",
            "https://abc.execute-api.us-east-1.amazonaws.com/direct",
        ),
        ("TENANT", "testuser"),
        ("WORKER_MAX_LIFETIME_S", "28800"),
        (
            "RUN_STATE_MACHINE_ARN",
            "arn:aws:states:us-east-1:700000000011:stateMachine:hamh-direct-run",
        ),
        ("PACKAGES_DOMAIN", "hamh-direct"),
        ("PACKAGES_OWNER", "700000000011"),
    ])
}

fn env_lookup(env: BTreeMap<&'static str, &'static str>) -> impl Fn(&str) -> Option<String> {
    move |k| env.get(k).map(|v| (*v).to_string())
}

#[tokio::test]
async fn the_lambda_is_wired_from_its_environment_and_makes_no_aws_call_while_it_starts() {
    // Defect F1/2: the SSM read at start made a half-deployed stage kill the function at init (Runtime.ExitError), and with it
    // every op, the Blocked state and the reaper. The runtime ARN is read when `start-session` needs it.
    let r = json10(vec![]);
    let deps = wire(
        &sdk_config(Some(test_creds()), Some(&r)),
        env_lookup(full_env()),
    )
    .await
    .unwrap();
    assert_eq!(deps.cfg.runtime_arn, "", "not read at start");
    assert_eq!(deps.cfg.bucket, "hamh-direct-harness-700000000011");
    assert_eq!(deps.cfg.max_lifetime_s, 28_800);
    assert_eq!(deps.store.table, TABLE);
    assert_eq!(
        deps.tokens.key_id,
        "arn:aws:kms:us-east-1:700000000011:key/data"
    );
    assert_eq!(deps.blobs.bucket, "hamh-direct-harness-700000000011");
    assert_eq!(
        deps.sessions.conflict_base_ms,
        dispatcher::aws::CONFLICT_BASE_MS
    );
    assert_eq!(
        deps.sessions.runtime_parameter.as_ref().unwrap().name,
        "/hamh/direct/runtime-arn/worker-python"
    );
    assert!(matches!(deps.packages, Packages::CodeArtifact(_)));
    assert_eq!(
        r.actual_requests().count(),
        0,
        "wiring makes no AWS call: a missing parameter cannot stop the function"
    );
}

#[tokio::test]
async fn the_environment_the_infra_stack_sets_wires_both_instances_parameters_and_calls_nothing() {
    // The names infra/lib/stacks/orchestration-direct.ts puts on the function for a stage with the Instances part:
    // the function holds the names of the two parameters, reads them when an Instances session starts, and starts without any call.
    let r = json10(vec![]);
    let mut env = full_env();
    env.insert("COMPUTE_POLICY", "instances");
    env.insert("COMPUTE_MAX_ACTIVE", "4");
    env.insert(
        "INSTANCES_RUNTIME_ARN_PARAMETER",
        "/hamh/direct/worker-python-instances-runtime-arn",
    );
    env.insert(
        "CAPACITY_PROVIDER_ID_PARAMETER",
        "/hamh/direct/instances-capacity-provider-id",
    );
    let deps = wire(&sdk_config(Some(test_creds()), Some(&r)), env_lookup(env))
        .await
        .unwrap();
    assert_eq!(
        deps.cfg.compute.policy,
        run_session::compute::ComputePolicy::Instances
    );
    assert_eq!(deps.cfg.compute.max_active, Some(4));
    assert_eq!(
        deps.sessions.instances_parameter.as_ref().unwrap().name,
        "/hamh/direct/worker-python-instances-runtime-arn"
    );
    assert_eq!(
        deps.sessions
            .capacity_provider_parameter
            .as_ref()
            .unwrap()
            .name,
        "/hamh/direct/instances-capacity-provider-id"
    );
    assert_eq!(r.actual_requests().count(), 0, "wiring makes no AWS call");
}

#[tokio::test]
async fn a_missing_variable_stops_the_start_by_name_before_any_call() {
    for name in full_env()
        .keys()
        .copied()
        .filter(|k| !matches!(*k, "PACKAGES_DOMAIN" | "PACKAGES_OWNER"))
    {
        let mut env = full_env();
        env.remove(name);
        let r = json10(vec![(200, &parameter(Some(ARN)))]);
        let e = wire(&sdk_config(Some(test_creds()), Some(&r)), env_lookup(env))
            .await
            .map(|_| ())
            .unwrap_err();
        assert!(e.contains(name), "{name}: {e}");
        assert_eq!(
            r.actual_requests().count(),
            0,
            "{name}: no call before the environment is complete"
        );
    }
    // an empty value is a missing one, and only the direct stage runs it
    let mut env = full_env();
    env.insert("TABLE_NAME", "");
    let e = wire(&sdk_config(Some(test_creds()), None), env_lookup(env))
        .await
        .map(|_| ())
        .unwrap_err();
    assert!(e.contains("TABLE_NAME"), "{e}");
    let mut env = full_env();
    env.insert("HAMH_STAGE", "live");
    let e = wire(&sdk_config(Some(test_creds()), None), env_lookup(env))
        .await
        .map(|_| ())
        .unwrap_err();
    assert!(e.contains("HAMH_STAGE must be direct"), "{e}");
    // a parameter that does not exist, or holds no runtime ARN, does not stop the start (it fails `start-session` alone)
    let r = json10(vec![(200, &parameter(Some("nope")))]);
    let deps = wire(
        &sdk_config(Some(test_creds()), Some(&r)),
        env_lookup(full_env()),
    )
    .await
    .unwrap();
    assert_eq!(deps.cfg.runtime_arn, "");
    assert_eq!(r.actual_requests().count(), 0);
}

#[tokio::test]
async fn an_invocation_answers_its_value_or_the_error_type_and_message_the_machine_matches() {
    let r = json10(vec![]);
    let deps = wire(
        &sdk_config(Some(test_creds()), Some(&r)),
        env_lookup(full_env()),
    )
    .await
    .unwrap();
    // an input that is no op: invalid, with the reason, no retry
    let d = handle_payload(&deps, json!({"op": "nonsense", "runId": RUN}))
        .await
        .unwrap_err();
    assert_eq!(d.error_type, "RunSessionInvalid");
    assert!(d.error_message.starts_with("input:"), "{}", d.error_message);
    // an op the Lambda refuses before any AWS call
    let d = handle_payload(&deps, json!({"op": "prepare", "runId": "not-a-run-id"}))
        .await
        .unwrap_err();
    assert_eq!(d.error_type, "RunSessionInvalid");
    assert!(
        d.error_message.contains("is not a run id"),
        "{}",
        d.error_message
    );
    // an event that is not a run machine's
    let d = handle_payload(&deps, json!({"source": "aws.states", "detail": {}}))
        .await
        .unwrap_err();
    assert_eq!(d.error_type, "RunSessionInvalid");
}

#[tokio::test]
async fn an_op_runs_over_the_wired_clients_and_a_store_fault_is_the_retryable_error_type() {
    // the same replay client behind every SDK client: the Run item, the stored request and the run.created query
    let mem = support::fakes::Mem::default();
    mem.seed(support::fakes::repo_target(), "p", support::fakes::MODEL);
    let run = serde_json::to_value(mem.run()).unwrap();
    let request = mem.blob(&format!("runs/{RUN}/input/request.json")).unwrap();
    let mut created = mem
        .0
        .events
        .lock()
        .unwrap()
        .values()
        .next()
        .cloned()
        .unwrap();
    created.run_id = RUN.into();
    let pk = format!("EVT#{RUN}#00");
    let r = replay(vec![
        event(
            200,
            "application/x-amz-json-1.0",
            &item_reply(run, &run_pk(), "META"),
        ),
        event(
            200,
            "application/octet-stream",
            std::str::from_utf8(&request).unwrap(),
        ),
        event(
            200,
            "application/x-amz-json-1.0",
            &json!({"Items": [envelope_item(&created, &pk)]}).to_string(),
        ),
        event(500, "application/x-amz-json-1.0", DDB_500),
    ]);
    let deps = wire(
        &sdk_config(Some(test_creds()), Some(&r)),
        env_lookup(full_env()),
    )
    .await
    .unwrap();
    let out = handle_payload(&deps, json!({"op": "prepare", "runId": RUN}))
        .await
        .unwrap();
    assert_eq!(out["repo"], "hamh-e2e-python-calc");
    assert_eq!(out["stage"]["operation"], "stage");
    assert_eq!(out["model"]["id"], support::fakes::MODEL);
    let d = handle_payload(&deps, json!({"op": "prepare", "runId": RUN}))
        .await
        .unwrap_err();
    assert_eq!(d.error_type, "RunSessionUnavailable");
    assert!(d.error_message.contains("boom"), "{}", d.error_message);
}

#[tokio::test]
async fn the_wired_lambda_names_runs_only_when_the_deploy_set_a_utility_model() {
    // No UTILITY_MODEL: the helper calls are off, and the op answers without a single request.
    let r = json10(vec![]);
    let off = wire(
        &sdk_config(Some(test_creds()), Some(&r)),
        env_lookup(full_env()),
    )
    .await
    .unwrap();
    assert!(off.cfg.utility.is_none());
    let d = handle_payload(&off, json!({"op": "name-run", "runId": RUN}))
        .await
        .unwrap();
    assert_eq!(d, json!({"named": "off", "reason": "disabled"}));
    assert_eq!(r.actual_requests().count(), 0);

    // The deploy names the row: the function holds it and a client built from its own credentials, and making them calls nothing.
    let mut env = full_env();
    env.insert("AWS_REGION", "us-east-1");
    env.insert("UTILITY_MODEL", "us.anthropic.claude-haiku-5-5");
    let r = json10(vec![(200, "{}")]);
    let on = wire(
        &sdk_config(Some(test_creds()), Some(&r)),
        env_lookup(env.clone()),
    )
    .await
    .unwrap();
    assert_eq!(
        on.cfg.utility.as_ref().map(|m| m.id.as_str()),
        Some("us.anthropic.claude-haiku-5-5")
    );
    assert!(on.namer.client.is_some());
    assert_eq!(r.actual_requests().count(), 0, "wiring makes no call");
    // The op reads the Run item first; a run that does not exist ends it there, with no model call (the one request is that read).
    let d = handle_payload(&on, json!({"op": "name-run", "runId": RUN}))
        .await
        .unwrap();
    assert_eq!(d, json!({"named": "skipped", "reason": "no_run"}));
    assert_eq!(r.actual_requests().count(), 1);

    // A function with no credentials provider has nothing to call the model with.
    let r = json10(vec![]);
    let bare = wire(&sdk_config(None, Some(&r)), env_lookup(env))
        .await
        .unwrap();
    assert!(bare.namer.client.is_none());
}

// ---- /: the slice and stop writes ---------------------------------------------------------------------------

#[tokio::test]
async fn b688_the_agent_item_moves_to_the_next_slice_by_one_conditional_put() {
    let r = json10(vec![(200, "{}"), (400, CCF), (500, DDB_500)]);
    let s = ddb(&r);
    let mut next = agent_item();
    next.runtime_session_id = format!("{RUN}-direct-g2");
    next.version = 2;
    assert!(s.rotate_agent(&next, 1, 1).await.unwrap());
    assert!(
        !s.rotate_agent(&next, 1, 1).await.unwrap(),
        "an item that is not as read is left as it is"
    );
    assert_eq!(
        s.rotate_agent(&next, 1, 1).await.unwrap_err().error_type(),
        "RunSessionUnavailable"
    );
    let b = &bodies(&r)[0];
    assert_eq!(target(&r, 0), "DynamoDB_20120810.PutItem");
    assert_eq!(
        b["ConditionExpression"],
        "attribute_exists(PK) AND #sg = :g AND #v = :v"
    );
    assert_eq!(b["ExpressionAttributeNames"]["#sg"], "sessionGeneration");
    assert_eq!(b["ExpressionAttributeNames"]["#v"], "version");
    assert_eq!(b["ExpressionAttributeValues"][":g"], json!({"N": "1"}));
    assert_eq!(b["ExpressionAttributeValues"][":v"], json!({"N": "1"}));
    assert_eq!(b["Item"]["SK"], json!({"S": "AGENT#direct"}));
    assert_eq!(
        b["Item"]["runtimeSessionId"],
        json!({"S": format!("{RUN}-direct-g2")})
    );
}

#[tokio::test]
async fn b688_the_run_item_keeps_the_highest_slice_and_a_lower_one_is_not_an_error() {
    let r = json10(vec![(200, "{}"), (400, CCF), (500, DDB_500)]);
    let s = ddb(&r);
    s.set_run_slice(RUN, 3).await.unwrap();
    s.set_run_slice(RUN, 2).await.unwrap();
    assert_eq!(
        s.set_run_slice(RUN, 4).await.unwrap_err().error_type(),
        "RunSessionUnavailable"
    );
    let b = &bodies(&r)[0];
    assert_eq!(target(&r, 0), "DynamoDB_20120810.UpdateItem");
    assert_eq!(
        b["Key"],
        json!({"PK": {"S": run_pk()}, "SK": {"S": "META"}})
    );
    assert_eq!(b["UpdateExpression"], "SET #sl = :n");
    assert_eq!(
        b["ConditionExpression"],
        "attribute_exists(PK) AND (attribute_not_exists(#sl) OR #sl < :n)"
    );
    assert_eq!(b["ExpressionAttributeNames"]["#sl"], "slice");
    assert_eq!(b["ExpressionAttributeValues"][":n"], json!({"N": "3"}));
}

#[tokio::test]
async fn b689_a_stop_stores_the_reason_and_the_resume_mode_on_the_run_item_with_the_status() {
    use harness_protocol::goal::ResumeMode;
    let e = events::goal_complete(RUN, 1_790_000_000_000);
    let r = json10(vec![
        (200, "{}"),
        (400, &cancelled(&["ConditionalCheckFailed", "None", "None"])),
        (500, DDB_500),
    ]);
    let s = ddb(&r);
    let run = run_item();
    let reason = "no_progress: three sessions in a row changed nothing";
    assert!(
        s.set_run_stop(&run, GoalStatus::Stalled, &e, reason, ResumeMode::FollowUp)
            .await
            .unwrap()
    );
    assert!(
        !s.set_run_stop(&run, GoalStatus::Stalled, &e, reason, ResumeMode::FollowUp)
            .await
            .unwrap(),
        "a lost race"
    );
    assert_eq!(
        s.set_run_stop(&run, GoalStatus::Paused, &e, reason, ResumeMode::FollowUp)
            .await
            .unwrap_err()
            .error_type(),
        "RunSessionUnavailable"
    );
    let b = &bodies(&r)[0];
    assert_eq!(target(&r, 0), "DynamoDB_20120810.TransactWriteItems");
    let items = b["TransactItems"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    let meta = &items[0]["Update"];
    assert_eq!(
        meta["UpdateExpression"],
        "SET #status = :to, #v = #v + :one, statusReason = :reason, resumeMode = :mode"
    );
    assert_eq!(meta["ConditionExpression"], "#status = :from");
    assert_eq!(
        meta["ExpressionAttributeValues"][":to"],
        json!({"S": "stalled"})
    );
    assert_eq!(
        meta["ExpressionAttributeValues"][":reason"],
        json!({"S": reason})
    );
    assert_eq!(
        meta["ExpressionAttributeValues"][":mode"],
        json!({"S": "follow_up"})
    );
    // the root Goal moves with the status and carries no reason
    let goal = &items[1]["Update"];
    assert_eq!(
        goal["UpdateExpression"],
        "SET #status = :to, #v = #v + :one"
    );
    assert!(goal["ExpressionAttributeValues"].get(":reason").is_none());
    assert_eq!(
        bodies(&r)[2]["TransactItems"][0]["Update"]["ExpressionAttributeValues"][":to"],
        json!({"S": "paused"})
    );
}
