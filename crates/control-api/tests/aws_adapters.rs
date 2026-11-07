//! DynamoDB, S3 and Step Functions adapters against recorded HTTP exchanges (aws-smithy
//! StaticReplayClient, as in crates/worker-api/tests/aws_adapters.rs): the exact requests they send
//! and how they read the responses. No network.

#![allow(clippy::unwrap_used)] // test assertions unwrap by design

use aws_smithy_http_client::test_util::{ReplayEvent, StaticReplayClient};
use aws_smithy_types::body::SdkBody;
use control_api::aws::{DynamoStore, PageSkips, S3Blobs, SfnExecution, skip_line};
use control_api::handler::{Blobs, DeployedAs, Execution, Store, StoreError, TagRead};
use ham_core::run_index::{RunIndexItem, owner_pk, search_sk};
use ham_core::runtag::dynamo::{edit_update, expr_rejects};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload, SCHEMA_VERSION};
use harness_protocol::goal::{GoalPhase, GoalStatus};
use harness_protocol::store::{ArtifactKind, GoalItem, IdempotencyItem, RunItem};
use harness_protocol::target::RunTarget;
use serde_json::{Value, json};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const RUN: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Y";
const OWNER: &str = "arn:aws:sts::800000000022:assumed-role/Admin/testuser";
const CCF: &str = r#"{"__type":"com.amazonaws.dynamodb.v20120810#ConditionalCheckFailedException","message":"The conditional request failed"}"#;

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

fn claim(run_id: &str) -> IdempotencyItem {
    IdempotencyItem {
        principal_arn: OWNER.into(),
        idempotency_key: "k-1".into(),
        run_id: run_id.into(),
        request_sha256: "ab".repeat(32),
        claimed_at: "2026-09-25T20:00:00Z".into(),
    }
}

fn run_item() -> RunItem {
    RunItem {
        run_id: RUN.into(),
        owner_principal_arn: OWNER.into(),
        prompt_sha256: "cd".repeat(32),
        target: RunTarget::S3Only,
        base_ref: None,
        status: GoalStatus::Active,
        phase: GoalPhase::Planning,
        budget: None,
        deployment_digest: None,
        source_tree: None,
        engine_policy_version: 1,
        created_at: "2026-09-25T20:00:00Z".into(),
        version: 1,
        runtime_version: None,
        runtime_endpoint: None,
        aborted: None,
        continue_from: None,
        status_reason: None,
        resume_mode: None,
        slice: None,
        max_run_s: None,
    }
}

fn goal_item() -> GoalItem {
    serde_json::from_value(json!({
        "runId": RUN, "objective": "x", "requirements": [], "specSha256": "", "acceptanceSha256": "",
        "status": "active", "phase": "planning", "version": 1
    }))
    .unwrap()
}

fn created() -> Envelope {
    Envelope {
        schema_version: SCHEMA_VERSION,
        event_id: "01K5ZQ4T8M3N6P7R9S0V1W2X40".into(),
        run_id: RUN.into(),
        task_id: None,
        agent_id: None,
        seq: 0,
        ts: "2026-09-25T20:00:00Z".into(),
        event_type: EventType::RunCreated,
        authority: Authority::Harness,
        payload: Payload::Inline {
            value: json!({"target": {"mode": "s3-only"}}),
        },
    }
}

#[tokio::test]
async fn a_claim_is_a_conditional_put_of_the_idempotency_item() {
    let r = replay(vec![(200, "{}")]);
    let got = ddb(&r).claim_idempotency(&claim(RUN)).await.unwrap();
    assert_eq!(got, claim(RUN));
    let b = &bodies(&r)[0];
    assert_eq!(b["TableName"], "hamh-live-harness");
    assert_eq!(b["ConditionExpression"], "attribute_not_exists(PK)");
    assert_eq!(b["Item"]["PK"]["S"], format!("IDEM#{OWNER}#k-1"));
    assert_eq!(b["Item"]["SK"]["S"], "META");
    assert_eq!(b["Item"]["runId"]["S"], RUN);
    assert_eq!(b["Item"]["requestSha256"]["S"], "ab".repeat(32));
}

#[tokio::test]
async fn a_lost_claim_returns_the_stored_claim_read_consistently() {
    let stored = format!(
        r#"{{"Item": {{"PK": {{"S": "IDEM#{OWNER}#k-1"}}, "SK": {{"S": "META"}}, "principalArn": {{"S": "{OWNER}"}},
        "idempotencyKey": {{"S": "k-1"}}, "runId": {{"S": "01K5ZQ4T8M3N6P7R9S0V1W2X00"}},
        "requestSha256": {{"S": "{}"}}, "claimedAt": {{"S": "2026-09-25T19:59:00Z"}}}}}}"#,
        "ab".repeat(32)
    );
    let r = replay(vec![(400, CCF), (200, &stored)]);
    let got = ddb(&r).claim_idempotency(&claim(RUN)).await.unwrap();
    assert_eq!(got.run_id, "01K5ZQ4T8M3N6P7R9S0V1W2X00");
    let b = &bodies(&r)[1];
    assert_eq!(b["ConsistentRead"], true);
    assert_eq!(
        b["Key"],
        json!({"PK": {"S": format!("IDEM#{OWNER}#k-1")}, "SK": {"S": "META"}})
    );
}

#[tokio::test]
async fn create_run_writes_the_run_goal_and_event_in_one_transaction() {
    let r = replay(vec![(200, "{}")]);
    ddb(&r)
        .create_run(&run_item(), &goal_item(), &created())
        .await
        .unwrap();
    let b = &bodies(&r)[0];
    let items = b["TransactItems"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    let run = &items[0]["Put"];
    assert_eq!(run["TableName"], "hamh-live-harness");
    assert_eq!(run["Item"]["PK"]["S"], format!("RUN#{RUN}"));
    assert_eq!(run["Item"]["SK"]["S"], "META");
    assert_eq!(run["ConditionExpression"], "attribute_not_exists(PK)");
    // GSI1 keys for GET /v1/runs.
    assert_eq!(run["Item"]["ownerPrincipalArn"]["S"], OWNER);
    assert_eq!(run["Item"]["createdAt"]["S"], "2026-09-25T20:00:00Z");
    let goal = &items[1]["Put"];
    assert_eq!(goal["Item"]["SK"]["S"], "GOAL#root");
    assert_eq!(goal["ConditionExpression"], "attribute_not_exists(PK)");
    let event = &items[2]["Put"];
    assert_eq!(event["Item"]["PK"]["S"], format!("EVT#{RUN}#00"));
    assert_eq!(event["Item"]["SK"]["S"], "01K5ZQ4T8M3N6P7R9S0V1W2X40");
    assert_eq!(event["Item"]["envelope"]["M"]["type"]["S"], "run.created");
}

#[tokio::test]
async fn a_transaction_cancelled_by_a_condition_is_a_conflict_and_other_failures_are_unavailable() {
    let cancelled = r#"{"__type":"com.amazonaws.dynamodb.v20120810#TransactionCanceledException",
      "message":"Transaction cancelled, please refer cancellation reasons for specific reasons [ConditionalCheckFailed, None, None]",
      "CancellationReasons":[{"Code":"ConditionalCheckFailed","Message":"The conditional request failed"},{"Code":"None"},{"Code":"None"}]}"#;
    let r = replay(vec![(400, cancelled)]);
    let e = ddb(&r)
        .create_run(&run_item(), &goal_item(), &created())
        .await
        .unwrap_err();
    assert_eq!(e, StoreError::Conflict);

    let throttled = r#"{"__type":"com.amazonaws.dynamodb.v20120810#TransactionCanceledException",
      "message":"Transaction cancelled [ThrottlingError, None, None]",
      "CancellationReasons":[{"Code":"ThrottlingError"},{"Code":"None"},{"Code":"None"}]}"#;
    let r = replay(vec![(400, throttled)]);
    let e = ddb(&r)
        .create_run(&run_item(), &goal_item(), &created())
        .await
        .unwrap_err();
    assert!(matches!(e, StoreError::Unavailable(_)), "{e:?}");
}

#[tokio::test]
async fn runs_and_root_goals_are_consistent_gets_and_missing_items_are_none() {
    let run_json = format!(
        r#"{{"Item": {{"PK": {{"S": "RUN#{RUN}"}}, "SK": {{"S": "META"}}, "runId": {{"S": "{RUN}"}},
        "ownerPrincipalArn": {{"S": "{OWNER}"}}, "promptSha256": {{"S": "x"}}, "target": {{"M": {{"mode": {{"S": "s3-only"}}}}}},
        "status": {{"S": "active"}}, "phase": {{"S": "planning"}}, "enginePolicyVersion": {{"N": "1"}},
        "createdAt": {{"S": "2026-09-25T20:00:00Z"}}, "version": {{"N": "1"}}}}}}"#
    );
    let r = replay(vec![(200, &run_json), (200, "{}"), (200, "{}")]);
    let s = ddb(&r);
    assert_eq!(
        s.get_run(RUN).await.unwrap().unwrap().owner_principal_arn,
        OWNER
    );
    assert_eq!(s.get_run(RUN).await.unwrap(), None);
    assert_eq!(s.get_root_goal(RUN).await.unwrap(), None);
    let b = bodies(&r);
    assert_eq!(b[0]["ConsistentRead"], true);
    assert_eq!(
        b[0]["Key"],
        json!({"PK": {"S": format!("RUN#{RUN}")}, "SK": {"S": "META"}})
    );
    assert_eq!(
        b[2]["Key"],
        json!({"PK": {"S": format!("RUN#{RUN}")}, "SK": {"S": "GOAL#root"}})
    );
}

#[tokio::test]
async fn list_runs_queries_gsi1_newest_first_and_round_trips_the_page_token() {
    let page1 = format!(
        r#"{{"Items": [{{"PK": {{"S": "RUN#{RUN}"}}, "SK": {{"S": "META"}}, "runId": {{"S": "{RUN}"}},
        "ownerPrincipalArn": {{"S": "{OWNER}"}}, "promptSha256": {{"S": "x"}}, "target": {{"M": {{"mode": {{"S": "s3-only"}}}}}},
        "status": {{"S": "active"}}, "phase": {{"S": "planning"}}, "enginePolicyVersion": {{"N": "1"}},
        "createdAt": {{"S": "2026-09-25T20:00:00Z"}}, "version": {{"N": "1"}}}}],
        "LastEvaluatedKey": {{"PK": {{"S": "RUN#{RUN}"}}, "SK": {{"S": "META"}}, "ownerPrincipalArn": {{"S": "{OWNER}"}},
        "createdAt": {{"S": "2026-09-25T20:00:00Z"}}}}}}"#
    );
    let r = replay(vec![(200, &page1), (200, r#"{"Items": []}"#)]);
    let s = ddb(&r);
    let (runs, next) = s.list_runs(OWNER, None).await.unwrap();
    assert_eq!(runs.len(), 1);
    let token = next.unwrap();
    assert!(!token.contains('"'), "opaque token: {token}");
    let (runs, next) = s.list_runs(OWNER, Some(&token)).await.unwrap();
    assert!(runs.is_empty());
    assert_eq!(next, None);
    let b = bodies(&r);
    assert_eq!(b[0]["IndexName"], "GSI1");
    assert_eq!(b[0]["KeyConditionExpression"], "ownerPrincipalArn = :o");
    assert_eq!(b[0]["ExpressionAttributeValues"][":o"]["S"], OWNER);
    assert_eq!(b[0]["ScanIndexForward"], false);
    assert!(b[0].get("ExclusiveStartKey").is_none());
    assert_eq!(
        b[1]["ExclusiveStartKey"],
        json!({"PK": {"S": format!("RUN#{RUN}")}, "SK": {"S": "META"}, "ownerPrincipalArn": {"S": OWNER},
               "createdAt": {"S": "2026-09-25T20:00:00Z"}})
    );

    // A token the caller made up is refused before any call; so is another caller's token.
    let r = replay(vec![]);
    for bad in ["not base64 !!", "e30", "eyJQSyI6eyJTIjoiUlVOI3gifX0"] {
        assert_eq!(
            ddb(&r).list_runs(OWNER, Some(bad)).await.unwrap_err(),
            StoreError::BadCursor,
            "{bad}"
        );
    }
    let (_, Some(mine)) = ddb(&replay(vec![(200, &page1)]))
        .list_runs(OWNER, None)
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        ddb(&r)
            .list_runs(
                "arn:aws:sts::800000000022:assumed-role/Admin/someone",
                Some(&mine)
            )
            .await
            .unwrap_err(),
        StoreError::BadCursor
    );
    assert_eq!(r.actual_requests().count(), 0);
}

#[tokio::test]
async fn events_query_one_shard_after_the_cursor_consistently() {
    let item = format!(
        r#"{{"Items": [{{"PK": {{"S": "EVT#{RUN}#00"}}, "SK": {{"S": "01K5ZQ4T8M3N6P7R9S0V1W2X40"}}, "envelope": {{"M": {{
        "schemaVersion": {{"N": "1"}}, "eventId": {{"S": "01K5ZQ4T8M3N6P7R9S0V1W2X40"}}, "runId": {{"S": "{RUN}"}},
        "seq": {{"N": "0"}}, "ts": {{"S": "2026-09-25T20:00:00Z"}}, "type": {{"S": "run.created"}},
        "authority": {{"S": "harness"}}, "payload": {{"M": {{"kind": {{"S": "inline"}}, "value": {{"M": {{}}}}}}}}}}}}}}]}}"#
    );
    let r = replay(vec![(200, &item), (200, r#"{"Items": []}"#)]);
    let s = ddb(&r);
    let got = s.events(RUN, 0, None, 500).await.unwrap();
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].event_type, EventType::RunCreated);
    assert!(
        s.events(RUN, 7, Some("01K5ZQ4T8M3N6P7R9S0V1W2X40"), 500)
            .await
            .unwrap()
            .is_empty()
    );
    let b = bodies(&r);
    assert_eq!(b[0]["KeyConditionExpression"], "PK = :pk");
    assert_eq!(
        b[0]["ExpressionAttributeValues"][":pk"]["S"],
        format!("EVT#{RUN}#00")
    );
    assert_eq!(b[0]["ConsistentRead"], true);
    assert_eq!(b[0]["Limit"], 500);
    assert_eq!(b[1]["KeyConditionExpression"], "PK = :pk AND SK > :after");
    assert_eq!(
        b[1]["ExpressionAttributeValues"][":pk"]["S"],
        format!("EVT#{RUN}#07")
    );
    assert_eq!(
        b[1]["ExpressionAttributeValues"][":after"]["S"],
        "01K5ZQ4T8M3N6P7R9S0V1W2X40"
    );
}

#[tokio::test]
async fn the_request_blob_is_put_as_json_and_named_by_its_s3_uri() {
    let r = replay(vec![(200, "")]);
    let b = S3Blobs {
        client: client!(aws_sdk_s3, r),
        bucket: "hamh-live-harness-800000000022".into(),
        credentials: None,
    };
    let uri = b
        .put("runs/R1/input/request.json", b"{\"prompt\":\"x\"}")
        .await
        .unwrap();
    assert_eq!(
        uri,
        "s3://hamh-live-harness-800000000022/runs/R1/input/request.json"
    );
    let q = r.actual_requests().next().unwrap();
    assert_eq!(q.method(), "PUT");
    assert!(
        q.uri().contains("hamh-live-harness-800000000022"),
        "{}",
        q.uri()
    );
    assert!(
        q.uri().contains("runs/R1/input/request.json"),
        "{}",
        q.uri()
    );
    assert_eq!(q.headers().get("content-type"), Some("application/json"));
    assert_eq!(q.body().bytes().unwrap(), b"{\"prompt\":\"x\"}");
}

#[tokio::test]
async fn the_run_execution_is_named_after_the_run_and_a_repeat_start_is_not_an_error() {
    let started = r#"{"executionArn": "arn:aws:states:us-west-2:800000000022:execution:hamh-live-run:R1", "startDate": 1790370000}"#;
    let exists =
        r#"{"__type": "ExecutionAlreadyExists", "message": "Execution Already Exists: 'R1'"}"#;
    let denied = r#"{"__type": "AccessDeniedException", "message": "not allowed"}"#;
    let r = replay(vec![(200, started), (400, exists), (400, denied)]);
    let x = SfnExecution {
        client: client!(aws_sdk_sfn, r),
        state_machine_arn: "arn:aws:states:us-west-2:800000000022:stateMachine:hamh-live-run"
            .into(),
        ssm: client!(aws_sdk_ssm, replay(vec![])),
        deployment_prefix: "/hamh/live".into(),
    };
    x.start("R1", &json!({"runId": "R1"})).await.unwrap();
    x.start("R1", &json!({"runId": "R1"})).await.unwrap();
    let e = x.start("R1", &json!({"runId": "R1"})).await.unwrap_err();
    assert!(
        matches!(e, StoreError::Unavailable(ref m) if m.contains("AccessDenied")),
        "{e:?}"
    );
    let b = bodies(&r);
    assert_eq!(
        b[0]["stateMachineArn"],
        "arn:aws:states:us-west-2:800000000022:stateMachine:hamh-live-run"
    );
    assert_eq!(b[0]["name"], "R1");
    assert_eq!(
        serde_json::from_str::<Value>(b[0]["input"].as_str().unwrap()).unwrap(),
        json!({"runId": "R1"})
    );
}

fn artifact_json(id: &str, kind: &str, key: &str) -> String {
    format!(
        r#"{{"PK": {{"S": "RUN#{RUN}"}}, "SK": {{"S": "ART#{id}"}}, "runId": {{"S": "{RUN}"}},
        "artifactId": {{"S": "{id}"}}, "kind": {{"S": "{kind}"}},
        "s3Uri": {{"S": "s3://hamh-live-harness-800000000022/{key}"}},
        "sha256": {{"S": "{}"}}, "sizeBytes": {{"N": "2048"}}}}"#,
        "ab".repeat(32)
    )
}

#[tokio::test]
async fn artifacts_are_a_consistent_query_of_the_runs_art_items_across_pages() {
    let a = artifact_json(
        "01K5ZQ4T8M3N6P7R9S0V1W2X42",
        "zip",
        "runs/R/deliverable/x.zip",
    );
    let b = artifact_json(
        "01K5ZQ4T8M3N6P7R9S0V1W2X41",
        "evidence",
        "runs/R/evidence/e.md",
    );
    let page1 = format!(
        r#"{{"Items": [{a}], "LastEvaluatedKey": {{"PK": {{"S": "RUN#{RUN}"}}, "SK": {{"S": "ART#01K5ZQ4T8M3N6P7R9S0V1W2X42"}}}}}}"#
    );
    let page2 = format!(r#"{{"Items": [{b}]}}"#);
    let r = replay(vec![(200, &page1), (200, &page2)]);
    let got = ddb(&r).list_artifacts(RUN).await.unwrap();
    let ids: Vec<&str> = got.iter().map(|x| x.artifact_id.as_str()).collect();
    assert_eq!(
        ids,
        ["01K5ZQ4T8M3N6P7R9S0V1W2X41", "01K5ZQ4T8M3N6P7R9S0V1W2X42"],
        "sorted by artifactId"
    );
    assert_eq!(got[1].kind, ArtifactKind::Zip);
    assert_eq!(got[1].size_bytes, 2048);
    assert_eq!(
        got[1].s3_uri,
        "s3://hamh-live-harness-800000000022/runs/R/deliverable/x.zip"
    );
    let q = bodies(&r);
    assert_eq!(q.len(), 2);
    assert_eq!(q[0]["TableName"], "hamh-live-harness");
    assert_eq!(
        q[0]["KeyConditionExpression"],
        "PK = :pk AND begins_with(SK, :art)"
    );
    assert_eq!(
        q[0]["ExpressionAttributeValues"][":pk"]["S"],
        format!("RUN#{RUN}")
    );
    assert_eq!(q[0]["ExpressionAttributeValues"][":art"]["S"], "ART#");
    assert_eq!(q[0]["ConsistentRead"], true);
    assert!(q[0].get("ExclusiveStartKey").is_none());
    assert_eq!(
        q[1]["ExclusiveStartKey"]["SK"]["S"],
        "ART#01K5ZQ4T8M3N6P7R9S0V1W2X42"
    );
}

#[tokio::test]
async fn one_artifact_is_a_consistent_get_and_a_missing_one_is_none() {
    let item = format!(
        r#"{{"Item": {}}}"#,
        artifact_json(
            "01K5ZQ4T8M3N6P7R9S0V1W2X42",
            "zip",
            "runs/R/deliverable/x.zip"
        )
    );
    let denied =
        r#"{"__type": "com.amazonaws.dynamodb.v20120810#AccessDeniedException", "message": "no"}"#;
    let r = replay(vec![(200, &item), (200, "{}"), (400, denied)]);
    let s = ddb(&r);
    let got = s
        .get_artifact(RUN, "01K5ZQ4T8M3N6P7R9S0V1W2X42")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.run_id, RUN);
    assert_eq!(got.kind, ArtifactKind::Zip);
    assert_eq!(
        s.get_artifact(RUN, "01K5ZQ4T8M3N6P7R9S0V1W2X49")
            .await
            .unwrap(),
        None
    );
    assert!(matches!(
        s.get_artifact(RUN, "x").await.unwrap_err(),
        StoreError::Unavailable(_)
    ));
    let q = bodies(&r);
    assert_eq!(q[0]["Key"]["PK"]["S"], format!("RUN#{RUN}"));
    assert_eq!(q[0]["Key"]["SK"]["S"], "ART#01K5ZQ4T8M3N6P7R9S0V1W2X42");
    assert_eq!(q[0]["ConsistentRead"], true);
}

/// Unix seconds of a SigV4 `X-Amz-Date` (`YYYYMMDDTHHMMSSZ`), by days-from-civil.
fn amz_date_secs(d: &str) -> u64 {
    let n = |a: usize, b: usize| d[a..b].parse::<i64>().unwrap();
    let (y, m, day) = (n(0, 4), n(4, 6), n(6, 8));
    let (hh, mm, ss) = (n(9, 11), n(11, 13), n(13, 15));
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    (days * 86_400 + hh * 3600 + mm * 60 + ss) as u64
}

fn query_param(url: &str, name: &str) -> Option<String> {
    url.split_once('?')?
        .1
        .split('&')
        .find_map(|kv| kv.strip_prefix(&format!("{name}=")).map(str::to_string))
}

fn s3_with(creds: aws_sdk_s3::config::Credentials) -> S3Blobs {
    S3Blobs {
        client: aws_sdk_s3::Client::from_conf(
            aws_sdk_s3::Config::builder()
                .behavior_version_latest()
                .region(aws_sdk_s3::config::Region::new("us-west-2"))
                .credentials_provider(creds.clone())
                .build(),
        ),
        bucket: "hamh-live-harness-800000000022".into(),
        credentials: Some(aws_sdk_s3::config::SharedCredentialsProvider::new(creds)),
    }
}

#[tokio::test]
async fn a_presigned_get_names_the_bucket_key_and_lifetime_and_its_expiry_follows_the_signature() {
    let b = s3_with(aws_sdk_s3::config::Credentials::new(
        "AKIDTESTONLY",
        "test-secret-not-a-key",
        None,
        None,
        "test",
    ));
    assert_eq!(b.bucket(), "hamh-live-harness-800000000022");
    let key = "runs/R1/deliverable/python-calc-R1-abc1234.zip";
    let out = b.presign_get(key, Duration::from_secs(900)).await.unwrap();
    assert!(
        out.url
            .starts_with("https://hamh-live-harness-800000000022.s3.us-west-2.amazonaws.com/"),
        "{}",
        out.url
    );
    assert!(out.url.contains(key), "{}", out.url);
    assert_eq!(
        query_param(&out.url, "X-Amz-Expires").as_deref(),
        Some("900")
    );
    assert!(
        query_param(&out.url, "X-Amz-Credential")
            .unwrap()
            .contains("us-west-2%2Fs3%2Faws4_request")
    );
    assert!(query_param(&out.url, "X-Amz-Signature").is_some());
    let signed = amz_date_secs(&query_param(&out.url, "X-Amz-Date").unwrap());
    assert_eq!(out.expires_at, control_api::ids::rfc3339(signed + 900));
}

#[tokio::test]
async fn a_presigned_get_never_claims_to_outlive_its_session_credentials() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let lapse = UNIX_EPOCH + Duration::from_secs(now + 300);
    let b = s3_with(aws_sdk_s3::config::Credentials::new(
        "AKIDTESTSESSION",
        "test-secret-not-a-key",
        Some("test-session-token".into()),
        Some(lapse),
        "test",
    ));
    let out = b
        .presign_get("runs/R1/evidence/EVIDENCE.md", Duration::from_secs(900))
        .await
        .unwrap();
    assert!(query_param(&out.url, "X-Amz-Security-Token").is_some());
    assert_eq!(out.expires_at, control_api::ids::rfc3339(now + 300));
}

#[tokio::test]
async fn a_run_item_that_does_not_parse_is_skipped_not_fatal() {
    // Live 2026-09-25: k-suite Run items written before the phases (phase "working") made
    // GET /v1/runs answer 503 for their owner.
    let page = format!(
        r#"{{"Items": [
        {{"PK": {{"S": "RUN#{RUN}"}}, "SK": {{"S": "META"}}, "runId": {{"S": "{RUN}"}},
          "ownerPrincipalArn": {{"S": "{OWNER}"}}, "promptSha256": {{"S": "x"}}, "target": {{"M": {{"mode": {{"S": "s3-only"}}}}}},
          "status": {{"S": "active"}}, "phase": {{"S": "working"}}, "enginePolicyVersion": {{"N": "0"}},
          "createdAt": {{"S": "2026-09-25T19:00:00Z"}}, "version": {{"N": "1"}}}},
        {{"PK": {{"S": "RUN#{RUN}"}}, "SK": {{"S": "META"}}, "runId": {{"S": "{RUN}"}},
          "ownerPrincipalArn": {{"S": "{OWNER}"}}, "promptSha256": {{"S": "x"}}, "target": {{"M": {{"mode": {{"S": "s3-only"}}}}}},
          "status": {{"S": "active"}}, "phase": {{"S": "red"}}, "enginePolicyVersion": {{"N": "0"}},
          "createdAt": {{"S": "2026-09-25T20:00:00Z"}}, "version": {{"N": "1"}}}}
        ]}}"#
    );
    let r = replay(vec![(200, &page)]);
    let (runs, next) = ddb(&r).list_runs(OWNER, None).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].created_at, "2026-09-25T20:00:00Z");
    assert_eq!(next, None);
}

#[tokio::test]
async fn the_deployment_is_read_from_the_stages_two_ssm_parameters_each_read_fresh() {
    let both = r#"{"Parameters": [
        {"Name": "/hamh/live/deployment-digest", "Type": "String", "Value": "ddd", "Version": 7},
        {"Name": "/hamh/live/deployed-source-tree", "Type": "String", "Value": "eee", "Version": 7}],
        "InvalidParameters": []}"#;
    // Before the stage's first deploy completes, neither parameter exists yet.
    let none = r#"{"Parameters": [], "InvalidParameters": ["/hamh/live/deployment-digest", "/hamh/live/deployed-source-tree"]}"#;
    let denied = r#"{"__type": "AccessDeniedException", "message": "not allowed"}"#;
    let r = replay(vec![(200, both), (200, none), (400, denied)]);
    let x = SfnExecution {
        client: client!(aws_sdk_sfn, replay(vec![])),
        state_machine_arn: "arn:aws:states:us-west-2:800000000022:stateMachine:hamh-live-run"
            .into(),
        ssm: client!(aws_sdk_ssm, r),
        deployment_prefix: "/hamh/live".into(),
    };
    assert_eq!(
        x.deployment().await.unwrap(),
        DeployedAs {
            digest: Some("ddd".into()),
            source_tree: Some("eee".into())
        }
    );
    assert_eq!(x.deployment().await.unwrap(), DeployedAs::default());
    let e = x.deployment().await.unwrap_err();
    assert!(
        matches!(e, StoreError::Unavailable(ref m) if m.contains("AccessDenied")),
        "{e:?}"
    );
    let b = bodies(&r);
    assert_eq!(b.len(), 3);
    assert_eq!(
        b[0]["Names"],
        json!([
            "/hamh/live/deployment-digest",
            "/hamh/live/deployed-source-tree"
        ])
    );
}

#[tokio::test]
async fn the_instances_runtime_parameter_is_read_and_each_failure_has_its_own_error() {
    let param = |value: &str| {
        json!({"Parameter": {"Name": "/hamh/live/worker-python-instances-runtime-arn", "Type": "String", "Value": value}}).to_string()
    };
    let arn = param(
        "arn:aws:bedrock-agentcore:us-east-1:123456789012:runtime/hamh_direct_harness_worker_python_instances-XyZaBc",
    );
    let not_an_arn = param("not-an-arn");
    let missing = r#"{"__type": "ParameterNotFound", "message": "no such parameter"}"#;
    let throttled = r#"{"__type": "ThrottlingException", "message": "slow down"}"#;
    let fault = r#"{"__type": "InternalServerError", "message": "boom"}"#;
    let denied = r#"{"__type": "AccessDeniedException", "message": "not allowed"}"#;
    let invalid = r#"{"__type": "ValidationException", "message": "bad name"}"#;
    let r = replay(vec![
        (200, &arn),
        (200, &not_an_arn),
        (400, missing),
        (400, throttled),
        (500, fault),
        (400, denied),
        (400, invalid),
    ]);
    let x = SfnExecution {
        client: client!(aws_sdk_sfn, replay(vec![])),
        state_machine_arn: "arn:aws:states:us-west-2:123456789012:stateMachine:hamh-live-run"
            .into(),
        ssm: client!(aws_sdk_ssm, r),
        deployment_prefix: "/hamh/live".into(),
    };
    assert!(x.instances_available().await.unwrap(), "a runtime ARN");
    assert!(
        !x.instances_available().await.unwrap(),
        "a value that is no ARN"
    );
    assert!(!x.instances_available().await.unwrap(), "no such parameter");
    for want in ["throttle", "service fault"] {
        let e = x.instances_available().await.unwrap_err();
        assert!(matches!(e, StoreError::Unavailable(_)), "{want}: {e:?}");
    }
    for want in ["AccessDenied", "Validation"] {
        let e = x.instances_available().await.unwrap_err();
        assert!(
            matches!(e, StoreError::NotSetUp(ref m) if m.contains(want)),
            "{want}: {e:?}"
        );
    }
    let b = bodies(&r);
    assert_eq!(b.len(), 7);
    assert_eq!(
        b[0]["Name"], "/hamh/live/worker-python-instances-runtime-arn",
        "the singular GetParameter, on the one parameter"
    );
}

#[tokio::test]
async fn a_control_is_a_conditional_put() {
    use harness_protocol::api::internal::PendingControl;
    use harness_protocol::store::ControlItem;
    let r = replay(vec![(200, "{}"), (400, CCF)]);
    let s = ddb(&r);
    let c = ControlItem {
        run_id: RUN.into(),
        agent_id: "direct".into(),
        control_id: "01K5ZQ4T8M3N6P7R9S0V1W2X70".into(),
        control: PendingControl::Cancel,
        created_at: "2026-09-25T20:01:00Z".into(),
        delivered_at: None,
    };
    s.put_control(&c).await.unwrap();
    assert_eq!(s.put_control(&c).await.unwrap_err(), StoreError::Conflict);
    let b = bodies(&r);
    assert_eq!(b[0]["ConditionExpression"], "attribute_not_exists(PK)");
    assert_eq!(b[0]["Item"]["PK"]["S"], format!("RUN#{RUN}"));
    assert_eq!(
        b[0]["Item"]["SK"]["S"],
        "CTRL#direct#01K5ZQ4T8M3N6P7R9S0V1W2X70"
    );
    assert_eq!(b[0]["Item"]["control"]["M"]["kind"]["S"], "cancel");
}

// ---- the repository allowlist -----------------------------------------------------

fn allow(t: &str) -> harness_protocol::store::RepoAllowItem {
    harness_protocol::store::RepoAllowItem {
        target: t.parse().unwrap(),
        added_by: OWNER.into(),
        added_at: "2026-09-27T03:00:00Z".into(),
    }
}

#[tokio::test]
async fn an_allow_writes_the_entry_and_adds_it_to_the_listing_in_one_transaction() {
    let r = replay(vec![(200, "{}"), (200, "{}")]);
    let d = ddb(&r);
    d.repo_allow_put(&allow("cc:hamh-e2e-foo")).await.unwrap();
    d.repo_allow_delete(&"cc:hamh-e2e-foo".parse().unwrap())
        .await
        .unwrap();
    let b = bodies(&r);
    let put = &b[0]["TransactItems"];
    assert_eq!(
        put[0]["Put"]["Item"]["PK"]["S"],
        format!(
            "REPOALLOW#cc:{}:hamh-e2e-foo",
            harness_protocol::target::E2E_REGION
        )
    );
    assert_eq!(put[0]["Put"]["Item"]["target"]["S"], "cc:hamh-e2e-foo");
    assert_eq!(put[1]["Update"]["Key"]["PK"]["S"], "REPOALLOW#INDEX");
    assert_eq!(put[1]["Update"]["UpdateExpression"], "ADD targets :t");
    assert_eq!(
        put[1]["Update"]["ExpressionAttributeValues"][":t"]["SS"],
        json!(["cc:hamh-e2e-foo"])
    );
    let del = &b[1]["TransactItems"];
    assert_eq!(
        del[0]["Delete"]["Key"]["PK"]["S"],
        format!(
            "REPOALLOW#cc:{}:hamh-e2e-foo",
            harness_protocol::target::E2E_REGION
        )
    );
    assert_eq!(del[1]["Update"]["UpdateExpression"], "DELETE targets :t");
}

#[tokio::test]
async fn the_listing_reads_the_index_then_each_entry() {
    let index = json!({"Item": {"PK": {"S": "REPOALLOW#INDEX"}, "SK": {"S": "META"},
        "targets": {"SS": ["gh:ex-sample1/app", "cc:hamh-e2e-foo"]}}})
    .to_string();
    let entry = |t: &str, pk: &str| {
        json!({"PK": {"S": pk}, "SK": {"S": "META"}, "target": {"S": t},
        "addedBy": {"S": OWNER}, "addedAt": {"S": "2026-09-27T03:00:00Z"}})
    };
    let batch = json!({"Responses": {"hamh-live-harness": [
        entry("gh:ex-sample1/app", "REPOALLOW#ex-sample1/app"),
        entry(
            "cc:hamh-e2e-foo",
            &format!("REPOALLOW#cc:{}:hamh-e2e-foo", harness_protocol::target::E2E_REGION)
        )
    ]}, "UnprocessedKeys": {}})
    .to_string();
    let r = replay(vec![(200, &index), (200, &batch)]);
    let d = ddb(&r);
    let got = d.repo_allow_list().await.unwrap();
    let targets: Vec<String> = got.iter().map(|i| i.target.to_string()).collect();
    assert_eq!(targets, ["cc:hamh-e2e-foo", "gh:ex-sample1/app"]);
    let b = bodies(&r);
    assert_eq!(b[0]["Key"]["PK"]["S"], "REPOALLOW#INDEX");
    assert_eq!(b[0]["ConsistentRead"], true);
    let keys = b[1]["RequestItems"]["hamh-live-harness"]["Keys"]
        .as_array()
        .unwrap();
    assert_eq!(keys.len(), 2);
    assert_eq!(
        b[1]["RequestItems"]["hamh-live-harness"]["ConsistentRead"],
        true
    );
    // No index item: an empty list and no batch read.
    let empty = replay(vec![(200, "{}")]);
    assert!(ddb(&empty).repo_allow_list().await.unwrap().is_empty());
    assert_eq!(bodies(&empty).len(), 1);
}

#[tokio::test]
async fn run_usage_is_a_consistent_query_of_the_runs_agent_and_turn_items_across_pages() {
    let agent = json!({"PK": {"S": format!("RUN#{RUN}")}, "SK": {"S": "AGENT#t1-a1"}, "runId": {"S": RUN},
        "agentId": {"S": "t1-a1"}, "taskId": {"S": "t1"}, "attempt": {"N": "1"}, "runtimeArn": {"S": "arn"},
        "runtimeSessionId": {"S": "s"}, "generation": {"N": "1"}, "engine": {"S": "opencode"}, "engineVersion": {"S": "1"},
        "transport": {"S": "acp"}, "modelId": {"S": "us.anthropic.claude-sonnet-5"}, "pool": {"S": "p"},
        "status": {"S": "running"}, "version": {"N": "1"}});
    let turn = |n: u32| {
        json!({"PK": {"S": format!("RUN#{RUN}")}, "SK": {"S": format!("TURN#t1-a1#{n:06}")}, "runId": {"S": RUN},
        "agentId": {"S": "t1-a1"}, "n": {"N": n.to_string()}, "taskTokenCiphertext": {"S": "c"},
        "startedAt": {"S": "2026-09-27T00:00:00Z"}, "rulesDigest": {"S": "d"}, "version": {"N": "1"},
        "usage": {"M": {"inputTokens": {"N": "10"}, "outputTokens": {"N": "2"}, "cacheReadInputTokens": {"N": "0"},
                        "cacheWriteInputTokens": {"N": "0"}}}})
    };
    let page1 = json!({"Items": [agent]}).to_string();
    let turns1 = json!({"Items": [turn(1)], "LastEvaluatedKey": {"PK": {"S": format!("RUN#{RUN}")}, "SK": {"S": "TURN#t1-a1#000001"}}}).to_string();
    let turns2 = json!({"Items": [turn(2)]}).to_string();
    let r = replay(vec![
        (200, &page1),
        (200, &turns1),
        (200, &turns2),
        (400, CCF),
    ]);
    let store = ddb(&r);
    let u = store.run_usage(RUN).await.unwrap();
    assert_eq!(u.agents.len(), 1);
    assert_eq!(u.turns.iter().map(|t| t.n).collect::<Vec<_>>(), [1, 2]);
    assert_eq!(u.turns[0].usage.unwrap().input_tokens, 10);
    let b = bodies(&r);
    assert_eq!(
        b[0]["KeyConditionExpression"],
        "PK = :pk AND begins_with(SK, :sk)"
    );
    assert_eq!(
        b[0]["ExpressionAttributeValues"][":sk"],
        json!({"S": "AGENT#"})
    );
    assert_eq!(
        b[1]["ExpressionAttributeValues"][":sk"],
        json!({"S": "TURN#"})
    );
    assert_eq!(b[0]["ConsistentRead"], true);
    assert_eq!(
        b[2]["ExclusiveStartKey"]["SK"],
        json!({"S": "TURN#t1-a1#000001"})
    );
    assert!(matches!(
        store.run_usage(RUN).await,
        Err(StoreError::Unavailable(_))
    ));
}

// ---- the run index and run names -----------------------------------------------------------------------------------------

const RUN_NAME: &str = "Make uploads idempotent across retries now";

#[tokio::test]
async fn put_run_index_never_names_the_name_attributes() {
    // The start and the backfill write the search item with `if_not_exists`. Only `name-run` (run-session) writes the name, so a retry of
    // the start can neither set nor erase it. Even an item that carries a name must not make this adapter write one.
    let item = RunIndexItem {
        run_id: RUN.into(),
        created_at: "2026-09-25T20:00:00Z".into(),
        prompt_head: "Add percent_change".into(),
        prompt_truncated: false,
        model: Some("global.anthropic.claude-sonnet-5-5".into()),
        repository: "s3-only".into(),
        outcome: None,
        last_activity: "2026-09-25T20:00:00Z".into(),
        run_name: Some(RUN_NAME.into()),
        run_name_source: Some("ai".into()),
        run_name_model: Some("us.anthropic.claude-haiku-5-5".into()),
        ..Default::default()
    };
    let r = replay(vec![(200, "{}")]);
    ddb(&r).put_run_index(OWNER, &item).await.unwrap();
    let b = &bodies(&r)[0];
    let text = b.to_string();
    for needle in [
        "runName",
        "runNameSource",
        "runNameModel",
        "idempotent",
        "haiku",
    ] {
        assert!(!text.contains(needle), "{needle} was written: {text}");
    }
    // No bare `name` either: it is a DynamoDB reserved word and not the attribute's name.
    let expr = b["UpdateExpression"].as_str().unwrap();
    assert!(
        !expr
            .split(|c: char| !c.is_alphanumeric())
            .any(|w| w.eq_ignore_ascii_case("name")),
        "{expr}"
    );
    assert!(expr.starts_with("SET "), "{expr}");
}

#[tokio::test]
async fn the_name_attributes_decode_from_the_stored_item() {
    // The item `name-run` creates when it runs before the start: the key, the run id, the creation time and the three attributes.
    let created = "2026-09-25T20:00:00Z";
    let stored = json!({"Items": [{
        "PK": {"S": owner_pk(OWNER)}, "SK": {"S": search_sk(created, RUN)},
        "runId": {"S": RUN}, "createdAt": {"S": created},
        "runName": {"S": RUN_NAME}, "runNameSource": {"S": "ai"},
        "runNameModel": {"S": "us.anthropic.claude-haiku-5-5"}
    }]});
    let r = replay(vec![(200, &stored.to_string())]);
    let got = ddb(&r)
        .run_index_between(OWNER, created, created)
        .await
        .unwrap();
    assert_eq!(got.len(), 1, "an item with only a name still decodes");
    let it = &got[0];
    assert_eq!(it.run_name.as_deref(), Some(RUN_NAME));
    assert_eq!(it.run_name_source.as_deref(), Some("ai"));
    assert_eq!(
        it.run_name_model.as_deref(),
        Some("us.anthropic.claude-haiku-5-5")
    );
    assert!(
        it.is_partial(),
        "no prompt head yet: the backfill completes it"
    );
}

// ---- run tags on the search item ---------------------------------------------------------------------------
// The tag writers own `runTags`, `runTagsUser`, `runTagsOff`, `runTagsAt`, `runTagsModel`, `runTagsVer`, `runTagsInh` and `runTagsRev`.
// This adapter only reads them: the start and the backfill never name them, and a tag attribute of a wrong type costs the row nothing but
// its tags. A page that dropped anything logs one line of counts.

const TAG_ATTRIBUTES: [&str; 8] = [
    "runTags",
    "runTagsUser",
    "runTagsOff",
    "runTagsAt",
    "runTagsModel",
    "runTagsVer",
    "runTagsInh",
    "runTagsRev",
];
const CREATED: &str = "2026-09-25T20:00:00Z";
const SONNET: &str = "global.anthropic.claude-sonnet-5-5";

/// A stored search item (DynamoDB JSON) with a prompt head, a model and a name, plus the attributes in `extra`.
fn stored_item(run: &str, extra: Value) -> Value {
    let mut item = json!({
        "PK": {"S": owner_pk(OWNER)}, "SK": {"S": search_sk(CREATED, run)},
        "runId": {"S": run}, "createdAt": {"S": CREATED}, "promptHead": {"S": "Add percent_change"},
        "promptTruncated": {"BOOL": false}, "repository": {"S": "s3-only"}, "lastActivity": {"S": CREATED},
        "model": {"S": SONNET}, "runName": {"S": RUN_NAME}, "runNameSource": {"S": "ai"}
    });
    for (k, v) in extra.as_object().cloned().unwrap_or_default() {
        item[k] = v;
    }
    item
}

/// One window read of `items` with the log lines it wrote.
async fn window(items: Vec<Value>) -> (Vec<RunIndexItem>, Vec<String>) {
    let r = replay(vec![(200, &json!({ "Items": items }).to_string())]);
    let mut lines: Vec<String> = Vec::new();
    let got = ddb(&r)
        .run_index_window(OWNER, CREATED, CREATED, &mut |l: &str| {
            lines.push(l.to_string())
        })
        .await
        .unwrap();
    (got, lines)
}

fn counts(line: &str) -> (u64, u64, u64) {
    let v: Value = serde_json::from_str(line).unwrap();
    assert_eq!(v["event"], "run-index-skip", "{line}");
    (
        v["count"].as_u64().unwrap(),
        v["tagAttributes"].as_u64().unwrap(),
        v["offAttributes"].as_u64().unwrap(),
    )
}

#[tokio::test]
async fn put_run_index_never_names_the_tag_attributes() {
    // The start and the backfill write the search item with `if_not_exists`. Only the tag writers name the tag attributes, so a retry of
    // the start can neither set nor erase them. Even an item that carries tags must not make this adapter write any.
    let item = RunIndexItem {
        run_id: RUN.into(),
        created_at: CREATED.into(),
        prompt_head: "Add percent_change".into(),
        model: Some(SONNET.into()),
        repository: "s3-only".into(),
        last_activity: CREATED.into(),
        run_tags: Some(vec!["topic:canary-auto".into()]),
        run_tags_user: Some(vec!["topic:canary-user".into()]),
        run_tags_off: Some(vec!["topic:canary-off".into()]),
        run_tags_at: Some("2026-10-21T10:00:00Z".into()),
        run_tags_model: Some("us.anthropic.claude-haiku-5-5".into()),
        run_tags_ver: Some("rt-1".into()),
        run_tags_inh: Some("canary-parent".into()),
        run_tags_rev: Some(4),
        ..RunIndexItem::default()
    };
    let r = replay(vec![(200, "{}")]);
    ddb(&r).put_run_index(OWNER, &item).await.unwrap();
    let b = &bodies(&r)[0];
    let text = b.to_string();
    for needle in TAG_ATTRIBUTES
        .iter()
        .copied()
        .chain(["canary", "haiku", "rt-1"])
    {
        assert!(!text.contains(needle), "{needle} was written: {text}");
    }
    let expr = b["UpdateExpression"].as_str().unwrap();
    assert!(expr.starts_with("SET "), "{expr}");
}

#[tokio::test]
async fn the_tag_attributes_decode_from_the_stored_item() {
    // What the tag writers store: lists of text (an empty one is a list), text, and the revision as a number.
    let (got, lines) = window(vec![stored_item(
        RUN,
        json!({
            "runTags": {"L": [{"S": "topic:csv-export"}, {"S": "tech:python"}]},
            "runTagsUser": {"L": []},
            "runTagsOff": {"L": [{"S": "tech:python"}]},
            "runTagsAt": {"S": "2026-10-21T10:00:00Z"},
            "runTagsModel": {"S": "us.anthropic.claude-haiku-5-5"},
            "runTagsVer": {"S": "rt-1"},
            "runTagsInh": {"S": "01K5ZQ4T8M3N6P7R9S0V1W2X3Z"},
            "runTagsRev": {"N": "7"}
        }),
    )])
    .await;
    assert!(lines.is_empty(), "{lines:?}");
    let it = &got[0];
    assert_eq!(
        it.run_tags,
        Some(vec!["topic:csv-export".into(), "tech:python".into()])
    );
    assert_eq!(it.run_tags_user, Some(vec![]));
    assert_eq!(it.run_tags_off, Some(vec!["tech:python".into()]));
    assert_eq!(it.run_tags_at.as_deref(), Some("2026-10-21T10:00:00Z"));
    assert_eq!(
        it.run_tags_model.as_deref(),
        Some("us.anthropic.claude-haiku-5-5")
    );
    assert_eq!(it.run_tags_ver.as_deref(), Some("rt-1"));
    assert_eq!(
        it.run_tags_inh.as_deref(),
        Some("01K5ZQ4T8M3N6P7R9S0V1W2X3Z")
    );
    assert_eq!(it.run_tags_rev, Some(7));
    // The rest of the item is as it was.
    assert_eq!(it.prompt_head, "Add percent_change");
    assert_eq!(it.run_name.as_deref(), Some(RUN_NAME));
    // A string SET is read as its strings.
    let (got, lines) = window(vec![stored_item(
        RUN,
        json!({"runTags": {"SS": ["topic:a", "topic:b"]}}),
    )])
    .await;
    assert!(lines.is_empty(), "{lines:?}");
    let mut set = got[0].run_tags.clone().unwrap();
    set.sort();
    assert_eq!(set, ["topic:a", "topic:b"]);
}

#[tokio::test]
async fn a_wrong_typed_tag_attribute_keeps_the_title_model_and_name() {
    // Each of these dropped the whole item before (the old `.ok()`): the row lost its title, model and name along with its tags.
    let wrong = [
        ("runTags", json!({"N": "5"})),
        ("runTags", json!({"M": {"a": {"S": "b"}}})),
        ("runTags", json!({"BOOL": true})),
        ("runTags", json!({"S": "topic:a"})),
        ("runTags", json!({"L": [{"N": "1"}]})),
        ("runTags", json!({"L": [{"S": "topic:a"}, {"N": "1"}]})),
        ("runTags", json!({"NS": ["1", "2"]})),
        ("runTags", json!({"B": "AAEC"})),
        ("runTagsUser", json!({"N": "5"})),
        ("runTagsAt", json!({"N": "5"})),
        ("runTagsModel", json!({"L": []})),
        ("runTagsVer", json!({"M": {}})),
        ("runTagsInh", json!({"BOOL": false})),
        ("runTagsRev", json!({"S": "two"})),
        ("runTagsRev", json!({"N": "99999999999"})),
        ("runTagsRev", json!({"N": "-1"})),
        ("runTagsRev", json!({"N": "1.5"})),
    ];
    for (name, value) in wrong {
        let (got, lines) = window(vec![stored_item(RUN, json!({ name: value.clone() }))]).await;
        assert_eq!(got.len(), 1, "{name}={value}: the item must still decode");
        let it = &got[0];
        assert_eq!(it.prompt_head, "Add percent_change", "{name}={value}");
        assert_eq!(it.model.as_deref(), Some(SONNET), "{name}={value}");
        assert_eq!(it.run_name.as_deref(), Some(RUN_NAME), "{name}={value}");
        assert!(
            it.run_tags.is_none()
                && it.run_tags_user.is_none()
                && it.run_tags_at.is_none()
                && it.run_tags_model.is_none()
                && it.run_tags_ver.is_none()
                && it.run_tags_inh.is_none()
                && it.run_tags_rev.is_none(),
            "{name}={value}: {it:?}"
        );
        assert_eq!(
            lines.len(),
            1,
            "{name}={value}: counted, not silent: {lines:?}"
        );
        assert_eq!(counts(&lines[0]), (0, 1, 0), "{name}={value}");
    }
    // A null attribute is no attribute and no skip.
    let (got, lines) = window(vec![stored_item(RUN, json!({"runTags": {"NULL": true}}))]).await;
    assert_eq!(got[0].run_tags, None);
    assert!(lines.is_empty(), "{lines:?}");
}

#[tokio::test]
async fn a_damaged_removal_list_keeps_what_can_be_read_and_is_counted_apart() {
    // The person's removal wins on every read, so a removal list that is only partly unreadable must keep the removals it can read.
    let (got, lines) = window(vec![stored_item(
        RUN,
        json!({"runTagsOff": {"L": [{"S": "tech:python"}, {"N": "5"}, {"S": "topic:a"}]}}),
    )])
    .await;
    assert_eq!(
        got[0].run_tags_off,
        Some(vec!["tech:python".into(), "topic:a".into()])
    );
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(counts(&lines[0]), (0, 1, 1));
    let (got, lines) = window(vec![stored_item(
        RUN,
        json!({"runTagsOff": {"S": "tech:python"}}),
    )])
    .await;
    assert_eq!(got[0].run_tags_off, Some(vec!["tech:python".into()]));
    assert_eq!(counts(&lines[0]), (0, 1, 1));
    // Past repair: no list of text at all. The removals are lost and the log line says so.
    let (got, lines) = window(vec![stored_item(RUN, json!({"runTagsOff": {"M": {}}}))]).await;
    assert_eq!(got[0].run_tags_off, None);
    assert_eq!(counts(&lines[0]), (0, 1, 1));
}

#[tokio::test]
async fn an_undecodable_item_is_counted_not_silent() {
    // An item that is unreadable as a whole (a number where the run id belongs) is dropped, as before, and now counted. The other items of
    // the page still join, one of them with a tag attribute of a wrong type.
    let junk = json!({
        "PK": {"S": owner_pk(OWNER)}, "SK": {"S": "SRCH#canary-key#y"}, "runId": {"N": "5"},
        "promptHead": {"S": "canary-prompt-text"}, "runTags": {"L": [{"S": "topic:canary-tag"}]}
    });
    let good = stored_item(RUN, json!({}));
    let odd = stored_item("01K5ZQ4T8M3N6P7R9S0V1W2X3Z", json!({"runTags": {"N": "1"}}));
    let (got, lines) = window(vec![junk, good, odd]).await;
    assert_eq!(got.len(), 2, "the other items still join");
    assert_eq!(lines.len(), 1, "one line for the page: {lines:?}");
    assert_eq!(counts(&lines[0]), (1, 1, 0));
    // Counts only: no key, no prompt text, no tag, no run id.
    for k in ["canary", RUN, "SRCH", "PK", "topic:"] {
        assert!(
            !lines[0].contains(k),
            "{k} is in the log line: {}",
            lines[0]
        );
    }
    // A page of good items writes nothing.
    let (_, lines) = window(vec![stored_item(RUN, json!({"runTags": {"L": []}}))]).await;
    assert!(lines.is_empty(), "{lines:?}");
}

#[tokio::test]
async fn each_page_of_the_window_logs_its_own_count() {
    let junk = |n: u32| json!({"PK": {"S": owner_pk(OWNER)}, "SK": {"S": format!("SRCH#x#{n}")}, "runId": {"N": "5"}});
    let k = json!({"PK": {"S": owner_pk(OWNER)}, "SK": {"S": search_sk(CREATED, RUN)}});
    let r = replay(vec![
        (
            200,
            &json!({"Items": [junk(1), stored_item(RUN, json!({}))], "LastEvaluatedKey": k})
                .to_string(),
        ),
        (200, &json!({"Items": [junk(2), junk(3)]}).to_string()),
    ]);
    let mut lines: Vec<String> = Vec::new();
    let got = ddb(&r)
        .run_index_window(OWNER, CREATED, CREATED, &mut |l: &str| {
            lines.push(l.to_string())
        })
        .await
        .unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(
        lines.iter().map(|l| counts(l)).collect::<Vec<_>>(),
        [(1, 0, 0), (2, 0, 0)]
    );
}

#[tokio::test]
async fn the_trait_read_decodes_the_same_window() {
    // `Store::run_index_between` is the window read with the Lambda's log as its sink.
    let r = replay(vec![(
        200,
        &json!({"Items": [stored_item(RUN, json!({"runTags": {"L": [{"S": "topic:a"}]}}))]})
            .to_string(),
    )]);
    let got = ddb(&r)
        .run_index_between(OWNER, CREATED, CREATED)
        .await
        .unwrap();
    assert_eq!(got[0].run_tags, Some(vec!["topic:a".into()]));
}

#[test]
fn the_skip_line_is_counts_only() {
    assert_eq!(skip_line(&PageSkips::default()), None);
    let line = skip_line(&PageSkips {
        items: 2,
        tag_attributes: 3,
        off_attributes: 1,
    })
    .unwrap();
    let v: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(
        v,
        json!({"event": "run-index-skip", "count": 2, "tagAttributes": 3, "offAttributes": 1})
    );
    assert!(!line.contains('\n'), "one line");
    for pages in [
        PageSkips {
            items: 0,
            tag_attributes: 1,
            off_attributes: 0,
        },
        PageSkips {
            items: 1,
            tag_attributes: 0,
            off_attributes: 0,
        },
    ] {
        assert!(skip_line(&pages).is_some(), "{pages:?}");
    }
}

// ---- the tag edit --------------------------------------------------------------------------------------------
// `POST /v1/tags/edit` reads one search item and writes its two person-owned lists. The read is one consistent GetItem that projects
// seven attributes; the write is the `UpdateItem` the library builds, sent as it is.

fn tag_strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_string()).collect()
}

#[tokio::test]
async fn edit_update_text_is_the_runtag_spec() {
    let spec = edit_update(
        RUN,
        CREATED,
        &tag_strings(&["topic:a", "tech:b"]),
        &tag_strings(&["tech:c"]),
        Some(3),
    );
    assert_eq!(expr_rejects(&spec), None);
    let r = replay(vec![(200, "{}")]);
    ddb(&r)
        .put_run_tags(OWNER, &run_item(), &spec)
        .await
        .unwrap();
    let b = &bodies(&r)[0];
    assert_eq!(b["TableName"], "hamh-live-harness");
    assert_eq!(
        b["Key"],
        json!({"PK": {"S": owner_pk(OWNER)}, "SK": {"S": search_sk(CREATED, RUN)}}),
        "the partition is the caller's and the sort key is the one the start writes"
    );
    assert_eq!(b["UpdateExpression"], spec.expression.as_str());
    assert_eq!(
        b["ConditionExpression"],
        "attribute_not_exists(#runTagsRev) OR #runTagsRev = :r"
    );
    assert_eq!(
        b["ExpressionAttributeNames"],
        json!({
            "#runId": "runId", "#createdAt": "createdAt", "#runTagsUser": "runTagsUser",
            "#runTagsOff": "runTagsOff", "#runTagsRev": "runTagsRev"
        }),
        "names only the attributes of the edit"
    );
    assert_eq!(
        b["ExpressionAttributeValues"],
        json!({
            ":rid": {"S": RUN}, ":cat": {"S": CREATED},
            ":user": {"L": [{"S": "topic:a"}, {"S": "tech:b"}]}, ":off": {"L": [{"S": "tech:c"}]},
            ":zero": {"N": "0"}, ":one": {"N": "1"}, ":r": {"N": "3"}
        })
    );
    assert!(b.get("ReturnValues").is_none(), "{b}");
    let text = b.to_string();
    for needle in [
        "promptHead",
        "outcome",
        "lastActivity",
        "runName",
        "runTags\"",
        "runTagsAt",
        "runTagsInh",
    ] {
        assert!(!text.contains(needle), "{needle} is in the request: {text}");
    }
    // An empty list is removed, not set to an empty list; a run with no revision yet expects 0.
    let spec = edit_update(RUN, CREATED, &[], &tag_strings(&["tech:c"]), None);
    assert_eq!(expr_rejects(&spec), None);
    let r = replay(vec![(200, "{}")]);
    ddb(&r)
        .put_run_tags(OWNER, &run_item(), &spec)
        .await
        .unwrap();
    let b = &bodies(&r)[0];
    assert!(
        b["UpdateExpression"]
            .as_str()
            .unwrap()
            .contains("REMOVE #runTagsUser"),
        "{b}"
    );
    assert!(b["ExpressionAttributeValues"].get(":user").is_none());
    assert_eq!(b["ExpressionAttributeValues"][":r"], json!({"N": "0"}));
}

#[tokio::test]
async fn a_lost_race_is_a_conflict_and_other_failures_are_an_outage() {
    let spec = edit_update(RUN, CREATED, &tag_strings(&["topic:a"]), &[], Some(1));
    let r = replay(vec![(400, CCF)]);
    assert_eq!(
        ddb(&r).put_run_tags(OWNER, &run_item(), &spec).await,
        Err(StoreError::Conflict)
    );
    for (status, body) in [
        (
            500,
            r#"{"__type":"com.amazonaws.dynamodb.v20120810#InternalServerError","message":"x"}"#,
        ),
        (
            400,
            r#"{"__type":"com.amazonaws.dynamodb.v20120810#ValidationException","message":"x"}"#,
        ),
        (
            400,
            r#"{"__type":"com.amazonaws.dynamodb.v20120810#ProvisionedThroughputExceededException","message":"x"}"#,
        ),
    ] {
        let r = replay(vec![(status, body)]);
        assert!(
            matches!(
                ddb(&r).put_run_tags(OWNER, &run_item(), &spec).await,
                Err(StoreError::Unavailable(_))
            ),
            "{status} {body}"
        );
    }
}

/// The tag state of the run for a stored item whose tag attributes are `extra`.
async fn tag_state(extra: Value) -> (Option<TagRead>, Value) {
    let mut item = json!({
        "runId": {"S": RUN}, "createdAt": {"S": CREATED}, "repository": {"S": "cc:calc"}
    });
    for (k, v) in extra.as_object().cloned().unwrap_or_default() {
        item[k] = v;
    }
    let r = replay(vec![(200, &json!({ "Item": item }).to_string())]);
    let got = ddb(&r).get_run_tag_state(OWNER, &run_item()).await.unwrap();
    (got, bodies(&r)[0].clone())
}

#[tokio::test]
async fn the_tag_state_read_is_one_consistent_projected_get_item() {
    let (got, b) = tag_state(json!({
        "runTags": {"L": [{"S": "topic:a"}]}, "runTagsUser": {"L": [{"S": "project:x"}]},
        "runTagsOff": {"L": [{"S": "tech:b"}]}, "runTagsRev": {"N": "4"}
    }))
    .await;
    assert_eq!(b["TableName"], "hamh-live-harness");
    assert_eq!(
        b["Key"],
        json!({"PK": {"S": owner_pk(OWNER)}, "SK": {"S": search_sk(CREATED, RUN)}})
    );
    assert_eq!(
        b["ConsistentRead"], true,
        "a compare-and-set base is read consistently"
    );
    // The projection, with its aliases resolved: seven attributes, none of the prompt, the name, the outcome or the bookkeeping text.
    let names = b["ExpressionAttributeNames"].as_object().unwrap();
    let mut projected: Vec<&str> = b["ProjectionExpression"]
        .as_str()
        .unwrap()
        .split(',')
        .map(|alias| names[alias.trim()].as_str().unwrap())
        .collect();
    projected.sort_unstable();
    assert_eq!(
        projected,
        [
            "createdAt",
            "repository",
            "runId",
            "runTags",
            "runTagsOff",
            "runTagsRev",
            "runTagsUser"
        ]
    );
    assert_eq!(names.len(), 7, "no alias that the projection does not use");
    let got = got.unwrap();
    assert!(got.unreadable.is_empty(), "{got:?}");
    let it = &got.item;
    assert_eq!(
        (
            it.run_id.as_str(),
            it.created_at.as_str(),
            it.repository.as_str()
        ),
        (RUN, CREATED, "cc:calc")
    );
    assert_eq!(it.run_tags, Some(tag_strings(&["topic:a"])));
    assert_eq!(it.run_tags_user, Some(tag_strings(&["project:x"])));
    assert_eq!(it.run_tags_off, Some(tag_strings(&["tech:b"])));
    assert_eq!(it.run_tags_rev, Some(4));
    // No item is no state: the first edit creates it.
    let r = replay(vec![(200, "{}")]);
    assert_eq!(
        ddb(&r).get_run_tag_state(OWNER, &run_item()).await,
        Ok(None)
    );
    // A store failure is not "no item".
    let r = replay(vec![(
        500,
        r#"{"__type":"com.amazonaws.dynamodb.v20120810#InternalServerError","message":"x"}"#,
    )]);
    assert!(matches!(
        ddb(&r).get_run_tag_state(OWNER, &run_item()).await,
        Err(StoreError::Unavailable(_))
    ));
}

#[tokio::test]
async fn a_tag_attribute_an_edit_would_replace_and_cannot_read_is_reported() {
    // What the edit replaces or compares against: the typed list, the removal list and the revision. The same decoder as the list row's
    // reads them (a wrong type is no value), and the adapter says which attributes were there and could not be read as written.
    let cases = [
        ("runTagsUser", json!({"N": "5"}), "runTagsUser"),
        (
            "runTagsUser",
            json!({"L": [{"S": "topic:a"}, {"N": "1"}]}),
            "runTagsUser",
        ),
        ("runTagsUser", json!({"M": {}}), "runTagsUser"),
        (
            "runTagsOff",
            json!({"L": [{"S": "tech:a"}, {"N": "5"}]}),
            "runTagsOff",
        ),
        ("runTagsOff", json!({"S": "tech:a"}), "runTagsOff"),
        ("runTagsOff", json!({"M": {}}), "runTagsOff"),
        ("runTagsRev", json!({"S": "two"}), "runTagsRev"),
        ("runTagsRev", json!({"N": "99999999999"}), "runTagsRev"),
        ("runTagsRev", json!({"N": "-1"}), "runTagsRev"),
        ("runTagsRev", json!({"N": "1.5"}), "runTagsRev"),
    ];
    for (name, value, flagged) in cases {
        let (got, _) = tag_state(json!({ name: value.clone() })).await;
        let got = got.unwrap();
        assert_eq!(got.unreadable, [flagged], "{name}={value}");
        assert_eq!(
            got.item.repository, "cc:calc",
            "{name}={value}: the rest of the item reads"
        );
    }
    // The automatic list is not replaced by an edit, and a null is no attribute; a string set is a list of text.
    for (name, value) in [
        ("runTags", json!({"N": "1"})),
        ("runTags", json!({"L": [{"N": "1"}]})),
        ("runTagsUser", json!({"NULL": true})),
        ("runTagsOff", json!({"NULL": true})),
        ("runTagsRev", json!({"NULL": true})),
        ("runTagsUser", json!({"SS": ["topic:a"]})),
        ("runTagsOff", json!({"SS": ["topic:a"]})),
        ("runTagsUser", json!({"L": []})),
    ] {
        let (got, _) = tag_state(json!({ name: value.clone() })).await;
        assert_eq!(
            got.unwrap().unreadable,
            Vec::<String>::new(),
            "{name}={value}"
        );
    }
    // An item that cannot be read at all is reported as such, with the ids the edit knows.
    let r = replay(vec![(
        200,
        &json!({"Item": {"runId": {"S": RUN}, "createdAt": {"S": CREATED}, "repository": {"N": "5"}}}).to_string(),
    )]);
    let got = ddb(&r)
        .get_run_tag_state(OWNER, &run_item())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.unreadable, ["item"]);
    assert_eq!(
        (got.item.run_id.as_str(), got.item.created_at.as_str()),
        (RUN, CREATED)
    );
    // The partly readable removal list keeps what it can, as the list row does.
    let (got, _) =
        tag_state(json!({"runTagsOff": {"L": [{"S": "tech:a"}, {"N": "5"}, {"S": "topic:b"}]}}))
            .await;
    assert_eq!(
        got.unwrap().item.run_tags_off,
        Some(tag_strings(&["tech:a", "topic:b"]))
    );
}
