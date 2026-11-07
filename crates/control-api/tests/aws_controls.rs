//! The DynamoDB and Step Functions adapters behind cancel: the agent read, the versioned run
//! update and StopExecution, against recorded HTTP exchanges (StaticReplayClient). No network.

#![allow(clippy::unwrap_used)]

use aws_smithy_http_client::test_util::{ReplayEvent, StaticReplayClient};
use aws_smithy_types::body::SdkBody;
use control_api::aws::{DynamoStore, SfnExecution, execution_arn};
use control_api::handler::{Execution, Store, StoreError};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use harness_protocol::goal::GoalStatus;
use harness_protocol::store::{GoalItem, RunItem};
use serde_json::{Value, json};

const RUN: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Y";
const OWNER: &str = "arn:aws:sts::800000000022:assumed-role/Admin/testuser";
const MACHINE: &str = "arn:aws:states:us-west-2:800000000022:stateMachine:hamh-live-run";

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

fn run_item(version: u64) -> RunItem {
    serde_json::from_value(json!({
        "runId": RUN, "ownerPrincipalArn": OWNER, "promptSha256": "cd".repeat(32),
        "target": {"mode": "s3-only"}, "status": "paused", "phase": "green",
        "enginePolicyVersion": 1, "createdAt": "2026-09-27T01:00:00Z", "version": version
    }))
    .unwrap()
}

fn goal_item(version: u64) -> GoalItem {
    serde_json::from_value(json!({
        "runId": RUN, "objective": "x", "requirements": [], "specSha256": "", "acceptanceSha256": "",
        "status": "paused", "phase": "green", "version": version
    }))
    .unwrap()
}

fn status_event() -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: "01K5ZQ4T8M3N6P7R9S0V1W2X41".into(),
        run_id: RUN.into(),
        task_id: None,
        agent_id: None,
        seq: 0,
        ts: "2026-09-27T01:05:00Z".into(),
        event_type: EventType::GoalStatus,
        authority: Authority::Harness,
        payload: Payload::Inline {
            value: json!({"status": "paused", "phase": "green"}),
        },
    }
}

#[tokio::test]
async fn a_run_update_sets_the_run_and_goal_fields_at_their_versions_and_the_event_in_one_transaction()
 {
    let r = replay(vec![(200, "{}"), (200, "{}")]);
    let s = ddb(&r);
    s.update_run(&run_item(5), Some(&goal_item(8)), Some(&status_event()))
        .await
        .unwrap();
    s.update_run(&run_item(6), None, None).await.unwrap();
    let b = bodies(&r);
    let items = b[0]["TransactItems"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    // Updates of the fields control-api owns, never a whole-item Put: run-tasks keeps its run
    // machine on the root Goal and dispatch its runtime pins on the Run item (live 2026-09-27: a
    // Put on pause erased `runMachine`, and the run blocked at integrate in "phase none").
    assert!(items[0].get("Put").is_none(), "{}", items[0]);
    assert!(items[1].get("Put").is_none(), "{}", items[1]);
    let run = &items[0]["Update"];
    assert_eq!(
        run["Key"],
        json!({"PK": {"S": format!("RUN#{RUN}")}, "SK": {"S": "META"}})
    );
    assert_eq!(run["UpdateExpression"], "SET #status = :status, #v = :next");
    assert_eq!(run["ConditionExpression"], "#v = :v");
    assert_eq!(
        run["ExpressionAttributeNames"],
        json!({"#v": "version", "#status": "status"})
    );
    assert_eq!(run["ExpressionAttributeValues"][":status"]["S"], "paused");
    assert_eq!(run["ExpressionAttributeValues"][":next"]["N"], "5");
    assert_eq!(run["ExpressionAttributeValues"][":v"]["N"], "4");
    let goal = &items[1]["Update"];
    assert_eq!(
        goal["Key"],
        json!({"PK": {"S": format!("RUN#{RUN}")}, "SK": {"S": "GOAL#root"}})
    );
    assert_eq!(
        goal["UpdateExpression"],
        "SET #status = :status, #v = :next"
    );
    assert_eq!(goal["ConditionExpression"], "#v = :v");
    assert_eq!(goal["ExpressionAttributeValues"][":v"]["N"], "7");
    assert_eq!(goal["ExpressionAttributeValues"][":next"]["N"], "8");
    assert_eq!(goal["ExpressionAttributeValues"][":status"]["S"], "paused");
    let event = &items[2]["Put"];
    assert_eq!(event["Item"]["PK"]["S"], format!("EVT#{RUN}#00"));
    assert_eq!(event["Item"]["envelope"]["M"]["type"]["S"], "goal.status");
    assert_eq!(event["ConditionExpression"], "attribute_not_exists(PK)");
    // Without a goal or an event the transaction holds the Run item alone.
    assert_eq!(b[1]["TransactItems"].as_array().unwrap().len(), 1);
    assert_eq!(GoalStatus::Paused, run_item(5).status);
}

#[tokio::test]
async fn a_lost_version_race_is_a_conflict_and_other_failures_are_unavailable() {
    let cancelled = r#"{"__type":"com.amazonaws.dynamodb.v20120810#TransactionCanceledException",
      "message":"Transaction cancelled [ConditionalCheckFailed, None]",
      "CancellationReasons":[{"Code":"ConditionalCheckFailed"},{"Code":"None"}]}"#;
    let throttled = r#"{"__type":"com.amazonaws.dynamodb.v20120810#ThrottlingException","message":"slow down"}"#;
    let r = replay(vec![(400, cancelled), (400, throttled)]);
    let s = ddb(&r);
    assert_eq!(
        s.update_run(&run_item(5), Some(&goal_item(8)), None)
            .await
            .unwrap_err(),
        StoreError::Conflict
    );
    assert!(matches!(
        s.update_run(&run_item(5), None, None).await,
        Err(StoreError::Unavailable(_))
    ));
}

#[tokio::test]
async fn agents_are_a_consistent_query_of_the_run_partition() {
    let agent = json!({"PK": {"S": format!("RUN#{RUN}")}, "SK": {"S": "AGENT#direct"}, "runId": {"S": RUN},
        "agentId": {"S": "direct"}, "taskId": {"S": "root"}, "attempt": {"N": "1"}, "runtimeArn": {"S": "arn"},
        "runtimeSessionId": {"S": "s"}, "generation": {"N": "1"}, "engine": {"S": "cline"}, "engineVersion": {"S": "1"},
        "transport": {"S": "acp"}, "modelId": {"S": "m"}, "pool": {"S": "p"}, "status": {"S": "running"},
        "version": {"N": "1"}});
    let agents = json!({"Items": [agent]}).to_string();
    let r = replay(vec![(200, &agents)]);
    let s = ddb(&r);
    let a = s.list_agents(RUN).await.unwrap();
    assert_eq!(a[0].agent_id, "direct");
    let b = bodies(&r);
    assert_eq!(
        b[0]["ExpressionAttributeValues"][":sk"],
        json!({"S": "AGENT#"})
    );
    assert_eq!(b[0]["ConsistentRead"], true);
}

#[tokio::test]
async fn cancel_stops_the_named_execution_and_a_missing_one_is_already_stopped() {
    let stopped = r#"{"stopDate": 1790370000}"#;
    let missing = r#"{"__type": "ExecutionDoesNotExist", "message": "Execution Does Not Exist"}"#;
    let denied = r#"{"__type": "AccessDeniedException", "message": "not allowed"}"#;
    let r = replay(vec![(200, stopped), (400, missing), (400, denied)]);
    let x = SfnExecution {
        client: client!(aws_sdk_sfn, r),
        state_machine_arn: MACHINE.into(),
        ssm: client!(aws_sdk_ssm, replay(vec![])),
        deployment_prefix: "/hamh/live".into(),
    };
    x.stop(RUN, &format!("cancelled by {OWNER}")).await.unwrap();
    x.stop(RUN, "c").await.unwrap();
    let e = x.stop(RUN, "c").await.unwrap_err();
    assert!(
        matches!(e, StoreError::Unavailable(ref m) if m.contains("AccessDenied")),
        "{e:?}"
    );
    let b = bodies(&r);
    assert_eq!(
        b[0]["executionArn"],
        format!("arn:aws:states:us-west-2:800000000022:execution:hamh-live-run:{RUN}")
    );
    assert_eq!(b[0]["cause"], format!("cancelled by {OWNER}"));
    assert_eq!(
        execution_arn(MACHINE, "R1"),
        "arn:aws:states:us-west-2:800000000022:execution:hamh-live-run:R1"
    );
}
