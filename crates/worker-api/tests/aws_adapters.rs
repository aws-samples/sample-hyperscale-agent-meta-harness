//! DynamoDB and STS adapters against recorded HTTP exchanges (aws-smithy StaticReplayClient):
//! the exact requests they send and how they read the responses. No network.

#![allow(clippy::unwrap_used)] // test assertions unwrap by design

use aws_smithy_http_client::test_util::{ReplayEvent, StaticReplayClient};
use aws_smithy_types::body::SdkBody;
use harness_protocol::api::internal::PendingControl;
use harness_protocol::common::Engine;
use harness_protocol::direct::{InstructAction, InstructOrigin};
use harness_protocol::envelope::{Authority, Envelope, EventType, Payload};
use harness_protocol::goal::GoalStatus;
use worker_api::aws::{DynamoStore, SfnCallbacks, StsMinter};
use worker_api::creds::MintPlan;
use worker_api::handler::{CallbackError, Callbacks, Minter, Store, StoreError, TokenContext};

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
                        .body(SdkBody::from(body.to_string()))
                        .unwrap(),
                )
            })
            .collect(),
    )
}

fn ddb(client: &StaticReplayClient) -> DynamoStore {
    let conf = aws_sdk_dynamodb::Config::builder()
        .behavior_version_latest()
        .region(aws_sdk_dynamodb::config::Region::new("us-west-2"))
        .credentials_provider(aws_sdk_dynamodb::config::Credentials::for_tests())
        .http_client(client.clone())
        .build();
    DynamoStore {
        client: aws_sdk_dynamodb::Client::from_conf(conf),
        table: "hamh-live-harness".into(),
    }
}

fn bodies(client: &StaticReplayClient) -> Vec<serde_json::Value> {
    client
        .actual_requests()
        .map(|r| serde_json::from_slice(r.body().bytes().unwrap_or_default()).unwrap())
        .collect()
}

const AGENT_ITEM: &str = r#"{"Item": {
  "PK": {"S": "RUN#R1"}, "SK": {"S": "AGENT#a1"}, "runId": {"S": "R1"}, "agentId": {"S": "a1"}, "taskId": {"S": "t1"},
  "attempt": {"N": "1"}, "runtimeArn": {"S": "arn"}, "runtimeSessionId": {"S": "R1-a1-session-000000000000000000000001"},
  "generation": {"N": "1"}, "engine": {"S": "cline"}, "engineVersion": {"S": "3.0.65"}, "transport": {"S": "acp"},
  "modelId": {"S": "m"}, "pool": {"S": "p"}, "status": {"S": "running"}, "version": {"N": "4"},
  "creds": {"M": {"nonceSha256": {"S": "abc"}, "revoked": {"BOOL": false}, "tenant": {"S": "t"}, "mode": {"S": "repo"},
                  "sessionPolicy": {"S": "{}"}}}}}"#;

#[tokio::test]
async fn get_agent_reads_a_consistent_item_by_run_and_agent_key() {
    let client = replay(vec![(200, AGENT_ITEM)]);
    let a = ddb(&client).get_agent("R1", "a1").await.unwrap().unwrap();
    assert_eq!(a.engine, Engine::CLINE);
    assert_eq!(a.creds.unwrap().nonce_sha256, "abc");
    let b = &bodies(&client)[0];
    assert_eq!(b["TableName"], "hamh-live-harness");
    assert_eq!(b["ConsistentRead"], true);
    assert_eq!(
        b["Key"],
        serde_json::json!({"PK": {"S": "RUN#R1"}, "SK": {"S": "AGENT#a1"}})
    );
}

#[tokio::test]
async fn a_missing_agent_is_none_and_a_service_error_is_unavailable() {
    let client = replay(vec![(200, "{}")]);
    assert_eq!(ddb(&client).get_agent("R1", "a1").await.unwrap(), None);
    let client = replay(vec![(
        400,
        r#"{"__type": "com.amazonaws.dynamodb.v20120810#ResourceNotFoundException", "message": "no table"}"#,
    )]);
    assert!(matches!(
        ddb(&client).get_agent("R1", "a1").await,
        Err(StoreError::Unavailable(_))
    ));
}

#[tokio::test]
async fn goal_status_reads_the_root_goal() {
    let client = replay(vec![(200, r#"{"Item": {"status": {"S": "active"}}}"#)]);
    assert_eq!(
        ddb(&client).get_goal_status("R1").await.unwrap(),
        Some(GoalStatus::Active)
    );
    assert_eq!(
        bodies(&client)[0]["Key"]["SK"],
        serde_json::json!({"S": "GOAL#root"})
    );
}

#[tokio::test]
async fn rotate_nonce_is_conditional_on_the_old_hash_and_unrevoked() {
    let client = replay(vec![(200, "{}")]);
    ddb(&client)
        .rotate_nonce("R1", "a1", "old", "new")
        .await
        .unwrap();
    let b = &bodies(&client)[0];
    assert_eq!(b["ConditionExpression"], "#c.#n = :old AND #c.#r = :false");
    assert_eq!(
        b["ExpressionAttributeValues"][":old"],
        serde_json::json!({"S": "old"})
    );
    assert_eq!(
        b["ExpressionAttributeValues"][":new"],
        serde_json::json!({"S": "new"})
    );
    let conflict = replay(vec![(
        400,
        r#"{"__type": "com.amazonaws.dynamodb.v20120810#ConditionalCheckFailedException", "message": "x"}"#,
    )]);
    assert_eq!(
        ddb(&conflict).rotate_nonce("R1", "a1", "old", "new").await,
        Err(StoreError::Conflict)
    );
}

fn envelope(i: u32) -> Envelope {
    Envelope {
        schema_version: 1,
        event_id: format!("01M3B8X7YK{:016}", i).replace(|c: char| !c.is_ascii_alphanumeric(), "0"),
        run_id: "R1".into(),
        task_id: None,
        agent_id: Some("a1".into()),
        seq: u64::from(i),
        ts: "2026-09-25T04:00:00Z".into(),
        event_type: EventType::TurnProgress,
        authority: Authority::Agent,
        payload: Payload::Inline {
            value: serde_json::json!({"i": i}),
        },
    }
}

/// The ids of the events a replay client recorded, one per request, in the order the requests were sent: every request is a
/// `BatchWriteItem` of exactly one put. (worker-api's role holds `dynamodb:BatchWriteItem` and not `dynamodb:PutItem`; one put in a
/// `BatchWriteItem` is committed on its own, like a `PutItem`.)
fn single_put_ids(client: &StaticReplayClient) -> Vec<String> {
    bodies(client)
        .iter()
        .map(|b| {
            let writes = b["RequestItems"]["hamh-live-harness"]
                .as_array()
                .unwrap_or_else(|| panic!("not a BatchWriteItem request: {b}"));
            assert_eq!(writes.len(), 1, "one event per request: {b}");
            writes[0]["PutRequest"]["Item"]["SK"]["S"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect()
}

#[tokio::test]
async fn a_backlog_is_batched_by_25_with_its_keys_and_unprocessed_items_retried() {
    // More events than one request may carry one at a time (a backlog after an outage): the BatchWriteItem path, as it always was.
    let events: Vec<Envelope> = (0..130).map(envelope).collect();
    let unprocessed = r#"{"UnprocessedItems": {"hamh-live-harness": [{"PutRequest": {"Item": {"PK": {"S": "EVT#R1#01"}, "SK": {"S": "x"}}}}]}}"#;
    let client = replay(vec![
        (200, unprocessed),
        (200, "{}"),
        (200, "{}"),
        (200, "{}"),
        (200, "{}"),
        (200, "{}"),
        (200, "{}"),
    ]);
    ddb(&client).put_events("R1", "a1", &events).await.unwrap();
    let b = bodies(&client);
    assert_eq!(b.len(), 7);
    assert_eq!(
        b[0]["RequestItems"]["hamh-live-harness"]
            .as_array()
            .unwrap()
            .len(),
        25
    );
    assert_eq!(
        b[1]["RequestItems"]["hamh-live-harness"]
            .as_array()
            .unwrap()
            .len(),
        1,
        "the retry carries only the unprocessed item"
    );
    assert_eq!(
        b[6]["RequestItems"]["hamh-live-harness"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
    let first = &b[0]["RequestItems"]["hamh-live-harness"][0]["PutRequest"]["Item"];
    assert!(first["PK"]["S"].as_str().unwrap().starts_with("EVT#R1#"));
    assert_eq!(first["SK"]["S"], serde_json::json!(envelope(0).event_id));
}

#[tokio::test]
async fn a_backlog_goes_out_in_chunks_that_are_sorted_by_id() {
    // Posted in a shuffled order, the chunks still hold the ids in order: a chunk is committed whole before the next one starts.
    let mut events: Vec<Envelope> = (0..130).map(envelope).collect();
    events.reverse();
    let client = replay(vec![(200, "{}"); 6]);
    ddb(&client).put_events("R1", "a1", &events).await.unwrap();
    let sent: Vec<String> = bodies(&client)
        .iter()
        .flat_map(|b| {
            b["RequestItems"]["hamh-live-harness"]
                .as_array()
                .unwrap()
                .iter()
                .map(|w| {
                    w["PutRequest"]["Item"]["SK"]["S"]
                        .as_str()
                        .unwrap()
                        .to_string()
                })
                .collect::<Vec<_>>()
        })
        .collect();
    let mut sorted = sent.clone();
    sorted.sort();
    assert_eq!(sent, sorted, "ascending across all chunks");
    assert_eq!(sent.len(), 130);
}

// ---- EVT1: the events of a batch are committed one at a time, in id order -----------------------------------------------------

#[tokio::test]
async fn a_batch_is_written_one_event_per_request_in_ascending_id_order() {
    // A BatchWriteItem of several puts performs them in parallel, so a reader can see a later id before an earlier one of the same
    // batch and then never return to it. 371 of 2,875 agent events on the testacct stage were committed after a larger id. One event per
    // request, each awaited, in id order makes the events a reader can see always a prefix of the batch.
    let mut events: Vec<Envelope> = (0..10).map(envelope).collect();
    events.swap(1, 7);
    events.swap(3, 9);
    let client = replay(vec![(200, "{}"); 10]);
    ddb(&client).put_events("R1", "a1", &events).await.unwrap();
    let ids = single_put_ids(&client);
    let expected: Vec<String> = (0..10).map(|i| envelope(i).event_id).collect();
    assert_eq!(ids, expected);
    for b in bodies(&client) {
        let item = &b["RequestItems"]["hamh-live-harness"][0]["PutRequest"]["Item"];
        assert!(item["PK"]["S"].as_str().unwrap().starts_with("EVT#R1#"));
    }
}

#[tokio::test]
async fn an_unprocessed_event_is_retried_before_the_next_one_is_sent() {
    // The order holds when the service throttles one event: the retry of event 1 comes before event 2 is sent.
    let unprocessed = r#"{"UnprocessedItems": {"hamh-live-harness": [{"PutRequest": {"Item": {"PK": {"S": "EVT#R1#01"}, "SK": {"S": "x"}}}}]}}"#;
    let client = replay(vec![
        (200, "{}"),
        (200, unprocessed),
        (200, "{}"),
        (200, "{}"),
    ]);
    let events: Vec<Envelope> = (0..3).map(envelope).collect();
    ddb(&client).put_events("R1", "a1", &events).await.unwrap();
    let b = bodies(&client);
    assert_eq!(
        b.len(),
        4,
        "event 0, event 1, the retry of event 1, event 2"
    );
    let sk = |i: usize| {
        b[i]["RequestItems"]["hamh-live-harness"][0]["PutRequest"]["Item"]["SK"]["S"].clone()
    };
    assert_eq!(sk(0), serde_json::json!(envelope(0).event_id));
    assert_eq!(sk(1), serde_json::json!(envelope(1).event_id));
    assert_eq!(
        sk(2),
        serde_json::json!("x"),
        "the retry carries the unprocessed item"
    );
    assert_eq!(sk(3), serde_json::json!(envelope(2).event_id));
}

#[tokio::test]
async fn the_largest_batch_that_is_written_one_event_at_a_time_is_100_events() {
    let events: Vec<Envelope> = (0..100).map(envelope).collect();
    let client = replay(vec![(200, "{}"); 100]);
    ddb(&client).put_events("R1", "a1", &events).await.unwrap();
    assert_eq!(single_put_ids(&client).len(), 100);
}

#[tokio::test]
async fn a_request_that_fails_stops_the_batch_so_what_was_stored_is_a_prefix() {
    let missing = r#"{"__type": "com.amazonaws.dynamodb.v20120810#ResourceNotFoundException", "message": "x"}"#;
    let client = replay(vec![(200, "{}"), (200, "{}"), (400, missing), (200, "{}")]);
    let events: Vec<Envelope> = (0..6).rev().map(envelope).collect();
    let r = ddb(&client).put_events("R1", "a1", &events).await;
    assert!(matches!(r, Err(StoreError::Unavailable(_))), "{r:?}");
    let ids = single_put_ids(&client);
    assert_eq!(
        ids,
        (0..3).map(|i| envelope(i).event_id).collect::<Vec<_>>(),
        "ids 0 and 1 were stored, id 2 failed, nothing after it was sent"
    );
}

#[tokio::test]
async fn an_empty_batch_sends_nothing() {
    let client = replay(vec![]);
    ddb(&client).put_events("R1", "a1", &[]).await.unwrap();
    assert!(bodies(&client).is_empty());
}

#[tokio::test]
async fn turn_usage_is_set_on_the_existing_turn_item_and_a_missing_one_is_left_alone() {
    let ccf = r#"{"__type":"com.amazonaws.dynamodb.v20120810#ConditionalCheckFailedException","message":"The conditional request failed"}"#;
    let client = replay(vec![(200, "{}"), (400, ccf), (500, "{}")]);
    let store = ddb(&client);
    let usage = harness_protocol::common::Usage {
        input_tokens: 12,
        output_tokens: 800,
        cache_read_input_tokens: 40000,
        cache_write_input_tokens: 10000,
    };
    store
        .record_turn_usage("R1", "a1", 3, &usage)
        .await
        .unwrap();
    store
        .record_turn_usage("R1", "a1", 4, &usage)
        .await
        .unwrap();
    assert!(matches!(
        store.record_turn_usage("R1", "a1", 5, &usage).await,
        Err(StoreError::Unavailable(_))
    ));
    let b = bodies(&client);
    assert_eq!(
        b[0]["Key"]["SK"],
        serde_json::json!({"S": "TURN#a1#000003"})
    );
    assert_eq!(b[0]["Key"]["PK"], serde_json::json!({"S": "RUN#R1"}));
    assert_eq!(b[0]["UpdateExpression"], "SET #u = :u");
    assert_eq!(
        b[0]["ExpressionAttributeNames"],
        serde_json::json!({"#u": "usage"})
    );
    assert_eq!(b[0]["ConditionExpression"], "attribute_exists(PK)");
    assert_eq!(
        b[0]["ExpressionAttributeValues"][":u"],
        serde_json::json!({"M": {"inputTokens": {"N": "12"}, "outputTokens": {"N": "800"},
            "cacheReadInputTokens": {"N": "40000"}, "cacheWriteInputTokens": {"N": "10000"}}})
    );
}

#[tokio::test]
async fn the_transcript_summary_is_set_on_the_existing_turn_item_and_a_missing_one_is_left_alone() {
    use harness_protocol::transcript::{CompletenessEntry, CompletenessKind, TranscriptSummary};
    let ccf = r#"{"__type":"com.amazonaws.dynamodb.v20120810#ConditionalCheckFailedException","message":"The conditional request failed"}"#;
    let client = replay(vec![(200, "{}"), (400, ccf), (500, "{}")]);
    let store = ddb(&client);
    let summary = TranscriptSummary {
        last_transcript_seq: Some(7),
        entries: vec![
            CompletenessEntry {
                kind: CompletenessKind::Failed,
                from_seq: None,
                to_seq: None,
                count: Some(3),
            },
            CompletenessEntry {
                kind: CompletenessKind::Conflict,
                from_seq: Some(2),
                to_seq: Some(2),
                count: Some(1),
            },
        ],
    };
    store
        .record_turn_transcript("R1", "a1", 3, &summary)
        .await
        .unwrap();
    store
        .record_turn_transcript("R1", "a1", 4, &summary)
        .await
        .unwrap();
    assert!(matches!(
        store.record_turn_transcript("R1", "a1", 5, &summary).await,
        Err(StoreError::Unavailable(_))
    ));
    let b = bodies(&client);
    assert_eq!(
        b[0]["Key"]["SK"],
        serde_json::json!({"S": "TURN#a1#000003"})
    );
    assert_eq!(b[0]["Key"]["PK"], serde_json::json!({"S": "RUN#R1"}));
    assert_eq!(b[0]["UpdateExpression"], "SET #t = :t");
    assert_eq!(
        b[0]["ExpressionAttributeNames"],
        serde_json::json!({"#t": "transcript"})
    );
    assert_eq!(b[0]["ConditionExpression"], "attribute_exists(PK)");
    assert_eq!(
        b[0]["ExpressionAttributeValues"][":t"],
        serde_json::json!({"M": {
            "lastTranscriptSeq": {"N": "7"},
            "entries": {"L": [
                {"M": {"kind": {"S": "failed"}, "count": {"N": "3"}}},
                {"M": {"kind": {"S": "conflict"}, "fromSeq": {"N": "2"}, "toSeq": {"N": "2"}, "count": {"N": "1"}}}
            ]}
        }})
    );
}

#[tokio::test]
async fn heartbeat_and_turn_complete_update_the_existing_agent_item() {
    let client = replay(vec![(200, "{}"), (200, "{}")]);
    let store = ddb(&client);
    store.record_heartbeat("R1", "a1", 3).await.unwrap();
    store.record_turn_complete("R1", "a1", 3).await.unwrap();
    let b = bodies(&client);
    assert_eq!(b[0]["UpdateExpression"], "SET heartbeatAt = :now");
    assert_eq!(b[0]["ConditionExpression"], "attribute_exists(PK)");
    assert!(
        b[0]["ExpressionAttributeValues"][":now"]["S"]
            .as_str()
            .unwrap()
            .ends_with('Z')
    );
    assert_eq!(
        b[1]["ExpressionAttributeValues"][":t"],
        serde_json::json!({"N": "3"})
    );
}

/// The key ID the STS replay mints: the ASIA prefix and a made-up rest, joined here so no committed line holds a key-ID-shaped literal.
fn minted_key_id() -> String {
    ["ASIA", "MINTED0000000001"].concat()
}

fn assume_role_xml() -> String {
    format!(
        r#"<AssumeRoleResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/">
  <AssumeRoleResult>
    <Credentials>
      <AccessKeyId>{key_id}</AccessKeyId>
      <SecretAccessKey>minted-secret</SecretAccessKey>
      <SessionToken>minted-token</SessionToken>
      <Expiration>2026-09-25T06:00:00Z</Expiration>
    </Credentials>
    <AssumedRoleUser><AssumedRoleId>AROA:R1.a1</AssumedRoleId><Arn>arn:aws:sts::800000000022:assumed-role/hamh-live-harness-worker-session/R1.a1</Arn></AssumedRoleUser>
    <SourceIdentity>a1</SourceIdentity>
  </AssumeRoleResult>
  <ResponseMetadata><RequestId>req-1</RequestId></ResponseMetadata>
</AssumeRoleResponse>"#,
        key_id = minted_key_id()
    )
}

#[tokio::test]
async fn minting_assumes_the_worker_role_with_the_plan() {
    let client = replay(vec![(200, &assume_role_xml())]);
    let conf = aws_sdk_sts::Config::builder()
        .behavior_version_latest()
        .region(aws_sdk_sts::config::Region::new("us-west-2"))
        .credentials_provider(aws_sdk_sts::config::Credentials::for_tests())
        .http_client(client.clone())
        .build();
    let minter = StsMinter {
        client: aws_sdk_sts::Client::from_conf(conf),
        role_arn: "arn:aws:iam::800000000022:role/hamh-live-harness-worker-session".into(),
    };
    let plan = MintPlan {
        role_session_name: "R1.a1".into(),
        source_identity: "a1".into(),
        tags: vec![("run".into(), "R1".into()), ("agent".into(), "a1".into())],
        session_policy: "{\"Version\":\"2012-10-17\"}".into(),
        duration_seconds: 3600,
    };
    let creds = minter.mint(&plan).await.unwrap();
    assert_eq!(creds.version, 1);
    assert_eq!(creds.access_key_id, minted_key_id());
    assert_eq!(creds.expiration, "2026-09-25T06:00:00Z");
    let req = client.actual_requests().next().unwrap();
    let form = String::from_utf8(req.body().bytes().unwrap().to_vec()).unwrap();
    for needle in [
        "Action=AssumeRole",
        "RoleSessionName=R1.a1",
        "SourceIdentity=a1",
        "DurationSeconds=3600",
        "Tags.member.1.Key=run",
        "Tags.member.1.Value=R1",
        "Tags.member.2.Key=agent",
        "Policy=",
    ] {
        assert!(form.contains(needle), "{needle} missing from {form}");
    }
}

fn sts_minter(client: &StaticReplayClient) -> StsMinter {
    let conf = aws_sdk_sts::Config::builder()
        .behavior_version_latest()
        .region(aws_sdk_sts::config::Region::new("us-west-2"))
        .credentials_provider(aws_sdk_sts::config::Credentials::for_tests())
        .http_client(client.clone())
        .build();
    StsMinter {
        client: aws_sdk_sts::Client::from_conf(conf),
        role_arn: "arn:aws:iam::800000000022:role/hamh-live-harness-worker-session".into(),
    }
}

fn plan(source_identity: &str) -> MintPlan {
    MintPlan {
        role_session_name: "R1.a1".into(),
        source_identity: source_identity.into(),
        tags: vec![("run".into(), "R1".into()), ("agent".into(), "a1".into())],
        session_policy: "{\"Version\":\"2012-10-17\"}".into(),
        duration_seconds: 3600,
    }
}

#[tokio::test]
async fn an_empty_source_identity_leaves_the_callers_in_place() {
    // STS keeps a source identity fixed across role chaining: an operator session that already
    // carries one (Testacct) cannot set another, so the operator path sends none.
    let client = replay(vec![(200, &assume_role_xml())]);
    sts_minter(&client).mint(&plan("")).await.unwrap();
    let req = client.actual_requests().next().unwrap();
    let form = String::from_utf8(req.body().bytes().unwrap().to_vec()).unwrap();
    assert!(!form.contains("SourceIdentity"), "{form}");
    assert!(form.contains("RoleSessionName=R1.a1"), "{form}");
}

const ACCESS_DENIED_XML: &str = r#"<ErrorResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/">
  <Error><Type>Sender</Type><Code>AccessDenied</Code><Message>User: arn:aws:sts::800000000022:assumed-role/Admin/op is not authorized to perform: sts:SetSourceIdentity on resource: arn:aws:iam::800000000022:role/hamh-live-harness-worker-session</Message></Error>
  <RequestId>req-2</RequestId>
</ErrorResponse>"#;

#[tokio::test]
async fn a_refused_mint_names_the_sts_error() {
    // The first live k-suite run printed only "mint failed: service error".
    let client = replay(vec![(403, ACCESS_DENIED_XML)]);
    let err = sts_minter(&client).mint(&plan("a1")).await.unwrap_err();
    assert!(err.0.contains("AccessDenied"), "{err:?}");
    assert!(err.0.contains("sts:SetSourceIdentity"), "{err:?}");
}

const CONDITIONAL_FAILED: &str = r#"{"__type": "com.amazonaws.dynamodb.v20120810#ConditionalCheckFailedException", "message": "x"}"#;

const CONTROLS_PAGE: &str = r#"{"Count": 2, "Items": [
  {"PK": {"S": "RUN#R1"}, "SK": {"S": "CTRL#a1#01J8ZQ4Y6X3M2K7N9P5R8T1V0W"}, "runId": {"S": "R1"}, "agentId": {"S": "a1"},
   "controlId": {"S": "01J8ZQ4Y6X3M2K7N9P5R8T1V0W"}, "createdAt": {"S": "2026-09-25T12:00:00Z"},
   "control": {"M": {"kind": {"S": "steer"}, "text": {"S": "use pytest"}, "now": {"BOOL": true}}}},
  {"PK": {"S": "RUN#R1"}, "SK": {"S": "CTRL#a1#01J8ZQ4Y6X3M2K7N9P5R8T1V0X"}, "runId": {"S": "R1"}, "agentId": {"S": "a1"},
   "controlId": {"S": "01J8ZQ4Y6X3M2K7N9P5R8T1V0X"}, "createdAt": {"S": "2026-09-25T12:00:01Z"},
   "control": {"M": {"kind": {"S": "cancel"}}}}]}"#;

#[tokio::test]
async fn take_controls_reads_undelivered_controls_and_marks_each_delivered_once() {
    // The second control was taken by a concurrent call: its conditional update fails.
    let client = replay(vec![
        (200, CONTROLS_PAGE),
        (200, "{}"),
        (400, CONDITIONAL_FAILED),
    ]);
    let got = ddb(&client).take_controls("R1", "a1").await.unwrap();
    assert_eq!(
        got,
        vec![PendingControl::Steer {
            text: "use pytest".into(),
            now: true
        }]
    );
    let b = bodies(&client);
    assert_eq!(b.len(), 3);
    assert_eq!(b[0]["TableName"], "hamh-live-harness");
    assert_eq!(b[0]["ConsistentRead"], true);
    assert_eq!(
        b[0]["KeyConditionExpression"],
        "PK = :pk AND begins_with(SK, :sk)"
    );
    assert_eq!(
        b[0]["FilterExpression"],
        "attribute_not_exists(deliveredAt)"
    );
    assert_eq!(
        b[0]["ExpressionAttributeValues"],
        serde_json::json!({":pk": {"S": "RUN#R1"}, ":sk": {"S": "CTRL#a1#"}})
    );
    assert_eq!(
        b[1]["Key"],
        serde_json::json!({"PK": {"S": "RUN#R1"}, "SK": {"S": "CTRL#a1#01J8ZQ4Y6X3M2K7N9P5R8T1V0W"}})
    );
    assert_eq!(b[1]["UpdateExpression"], "SET deliveredAt = :at");
    assert_eq!(
        b[1]["ConditionExpression"],
        "attribute_exists(PK) AND attribute_not_exists(deliveredAt)"
    );
    let at = b[1]["ExpressionAttributeValues"][":at"]["S"]
        .as_str()
        .unwrap();
    assert!(at.len() == 20 && at.ends_with('Z'), "{at}");
    assert_eq!(
        b[2]["Key"]["SK"],
        serde_json::json!({"S": "CTRL#a1#01J8ZQ4Y6X3M2K7N9P5R8T1V0X"})
    );
}

#[tokio::test]
async fn take_controls_follows_query_pages_and_reports_a_service_error() {
    let page1 = r#"{"Count": 0, "Items": [], "LastEvaluatedKey": {"PK": {"S": "RUN#R1"}, "SK": {"S": "CTRL#a1#01J8ZQ4Y6X3M2K7N9P5R8T1V0A"}}}"#;
    let page2 = r#"{"Count": 1, "Items": [
      {"PK": {"S": "RUN#R1"}, "SK": {"S": "CTRL#a1#01J8ZQ4Y6X3M2K7N9P5R8T1V0B"}, "runId": {"S": "R1"}, "agentId": {"S": "a1"},
       "controlId": {"S": "01J8ZQ4Y6X3M2K7N9P5R8T1V0B"}, "createdAt": {"S": "2026-09-25T12:00:00Z"},
       "control": {"M": {"kind": {"S": "cancel"}}}}]}"#;
    let client = replay(vec![(200, page1), (200, page2), (200, "{}")]);
    let got = ddb(&client).take_controls("R1", "a1").await.unwrap();
    assert_eq!(got, vec![PendingControl::Cancel]);
    let b = bodies(&client);
    assert_eq!(
        b[1]["ExclusiveStartKey"],
        serde_json::json!({"PK": {"S": "RUN#R1"}, "SK": {"S": "CTRL#a1#01J8ZQ4Y6X3M2K7N9P5R8T1V0A"}})
    );
    let client = replay(vec![(
        400,
        r#"{"__type": "com.amazonaws.dynamodb.v20120810#ResourceNotFoundException", "message": "no table"}"#,
    )]);
    assert!(matches!(
        ddb(&client).take_controls("R1", "a1").await,
        Err(StoreError::Unavailable(_))
    ));
}

// ---- the `instruct` control: the answer a user gives a run that asked for input ------------------------

const REQUEST_ID: &str = "01M5A2R7K3C9XJ4D8E6FQ0WT1N";

/// One stored control as control-api writes it: the control id is the request id, so one question has one answer.
fn instruct_item(sk_id: &str, control: &str) -> String {
    format!(
        r#"{{"PK": {{"S": "RUN#R1"}}, "SK": {{"S": "CTRL#a1#{sk_id}"}}, "runId": {{"S": "R1"}}, "agentId": {{"S": "a1"}},
   "controlId": {{"S": "{sk_id}"}}, "createdAt": {{"S": "2026-10-15T13:42:38Z"}}, "control": {{"M": {control}}}}}"#
    )
}

fn page(items: &[String]) -> String {
    format!(
        r#"{{"Count": {}, "Items": [{}]}}"#,
        items.len(),
        items.join(",")
    )
}

#[tokio::test]
async fn an_instruct_control_is_returned_once_and_marked_delivered() {
    let item = instruct_item(
        REQUEST_ID,
        r#"{"kind": {"S": "instruct"}, "requestId": {"S": "01M5A2R7K3C9XJ4D8E6FQ0WT1N"}, "action": {"S": "new_session_with_prompt"},
            "text": {"S": "Poll the log file instead of waiting on the command."}, "origin": {"S": "user"}}"#,
    );
    let client = replay(vec![(200, &page(std::slice::from_ref(&item))), (200, "{}")]);
    let got = ddb(&client).take_controls("R1", "a1").await.unwrap();
    assert_eq!(
        got.len(),
        1,
        "an instruct item must not be skipped: {got:?}"
    );
    assert_eq!(
        got[0],
        PendingControl::Instruct {
            request_id: REQUEST_ID.into(),
            action: InstructAction::NewSessionWithPrompt,
            text: Some("Poll the log file instead of waiting on the command.".into()),
            model: None,
            origin: InstructOrigin::User,
        }
    );
    assert_eq!(
        serde_json::to_value(&got[0]).unwrap(),
        serde_json::json!({
            "kind": "instruct", "requestId": REQUEST_ID, "action": "new_session_with_prompt",
            "text": "Poll the log file instead of waiting on the command.", "origin": "user"
        })
    );
    let b = bodies(&client);
    assert_eq!(b.len(), 2);
    assert_eq!(b[1]["UpdateExpression"], "SET deliveredAt = :at");
    assert_eq!(
        b[1]["Key"]["SK"],
        serde_json::json!({"S": format!("CTRL#a1#{REQUEST_ID}")})
    );

    // The next poll finds the same item already taken by a concurrent call: nothing comes back a second time.
    let client = replay(vec![(200, &page(&[item])), (400, CONDITIONAL_FAILED)]);
    assert!(
        ddb(&client)
            .take_controls("R1", "a1")
            .await
            .unwrap()
            .is_empty()
    );
}

/// A JSON value as DynamoDB's wire JSON writes it (`S`, `N`, `BOOL`, `M`, `L`, `NULL`).
fn dynamo(v: &serde_json::Value) -> serde_json::Value {
    use serde_json::{Value, json};
    match v {
        Value::String(s) => json!({"S": s}),
        Value::Number(n) => json!({"N": n.to_string()}),
        Value::Bool(b) => json!({"BOOL": b}),
        Value::Null => json!({"NULL": true}),
        Value::Array(a) => json!({"L": a.iter().map(dynamo).collect::<Vec<_>>()}),
        Value::Object(o) => {
            json!({"M": o.iter().map(|(k, v)| (k.clone(), dynamo(v))).collect::<serde_json::Map<_, _>>()})
        }
    }
}

/// `fixtures/direct-input/control-instruct.json`: the `ControlItem` control-api stores for an answer, the bytes it and agentd are tested against.
const GOLDEN_CONTROL_ITEM: &str =
    include_str!("../../../fixtures/direct-input/control-instruct.json");

#[tokio::test]
async fn the_golden_control_item_of_an_answer_is_taken_by_worker_api_as_the_golden_control() {
    let golden: serde_json::Value = serde_json::from_str(GOLDEN_CONTROL_ITEM).unwrap();
    let control_id = golden["controlId"].as_str().unwrap();
    let mut item = golden.as_object().unwrap().clone();
    item.insert(
        "PK".into(),
        serde_json::json!(format!("RUN#{}", golden["runId"].as_str().unwrap())),
    );
    item.insert(
        "SK".into(),
        serde_json::json!(format!(
            "CTRL#{}#{control_id}",
            golden["agentId"].as_str().unwrap()
        )),
    );
    // An item is the map of attributes itself, not an `M` value.
    let attributes = dynamo(&serde_json::Value::Object(item))["M"].clone();
    let page = serde_json::json!({"Count": 1, "Items": [attributes]}).to_string();
    let client = replay(vec![(200, &page), (200, "{}")]);
    let got = ddb(&client)
        .take_controls(
            golden["runId"].as_str().unwrap(),
            golden["agentId"].as_str().unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(serde_json::to_value(&got[0]).unwrap(), golden["control"]);
}

#[tokio::test]
async fn all_four_actions_come_back_as_stored_in_the_order_stored() {
    let items = [
        instruct_item(
            "01M5A2R7K3C9XJ4D8E6FQ0WT10",
            r#"{"kind": {"S": "instruct"}, "requestId": {"S": "01M5A2R7K3C9XJ4D8E6FQ0WT10"}, "action": {"S": "continue_with_prompt"},
                "text": {"S": "go on"}, "origin": {"S": "user"}}"#,
        ),
        instruct_item(
            "01M5A2R7K3C9XJ4D8E6FQ0WT11",
            r#"{"kind": {"S": "instruct"}, "requestId": {"S": "01M5A2R7K3C9XJ4D8E6FQ0WT11"}, "action": {"S": "new_session_with_prompt"}}"#,
        ),
        instruct_item(
            "01M5A2R7K3C9XJ4D8E6FQ0WT12",
            r#"{"kind": {"S": "instruct"}, "requestId": {"S": "01M5A2R7K3C9XJ4D8E6FQ0WT12"}, "action": {"S": "try_model"},
                "model": {"S": "us.anthropic.claude-opus-5-5"}, "origin": {"S": "user"}}"#,
        ),
        instruct_item(
            "01M5A2R7K3C9XJ4D8E6FQ0WT13",
            r#"{"kind": {"S": "instruct"}, "requestId": {"S": "01M5A2R7K3C9XJ4D8E6FQ0WT13"}, "action": {"S": "finish"}, "origin": {"S": "user"}}"#,
        ),
    ];
    let client = replay(vec![
        (200, &page(&items)),
        (200, "{}"),
        (200, "{}"),
        (200, "{}"),
        (200, "{}"),
    ]);
    let got = ddb(&client).take_controls("R1", "a1").await.unwrap();
    let json: Vec<serde_json::Value> = got
        .iter()
        .map(|c| serde_json::to_value(c).unwrap())
        .collect();
    assert_eq!(
        json,
        vec![
            serde_json::json!({"kind": "instruct", "requestId": "01M5A2R7K3C9XJ4D8E6FQ0WT10", "action": "continue_with_prompt", "text": "go on", "origin": "user"}),
            // A member control-api leaves out stays out; `origin` is the user's when the item names none.
            serde_json::json!({"kind": "instruct", "requestId": "01M5A2R7K3C9XJ4D8E6FQ0WT11", "action": "new_session_with_prompt", "origin": "user"}),
            serde_json::json!({"kind": "instruct", "requestId": "01M5A2R7K3C9XJ4D8E6FQ0WT12", "action": "try_model", "model": "us.anthropic.claude-opus-5-5", "origin": "user"}),
            serde_json::json!({"kind": "instruct", "requestId": "01M5A2R7K3C9XJ4D8E6FQ0WT13", "action": "finish", "origin": "user"}),
        ]
    );
    assert_eq!(
        bodies(&client).len(),
        5,
        "one query and four delivered marks"
    );
}

#[tokio::test]
async fn an_instruct_with_an_unknown_action_is_skipped_and_never_hides_the_cancel_after_it() {
    let future = instruct_item(
        "01M5A2R7K3C9XJ4D8E6FQ0WT20",
        r#"{"kind": {"S": "instruct"}, "requestId": {"S": "01M5A2R7K3C9XJ4D8E6FQ0WT20"}, "action": {"S": "dance"}}"#,
    );
    let cancel = instruct_item("01M5A2R7K3C9XJ4D8E6FQ0WT21", r#"{"kind": {"S": "cancel"}}"#);
    let client = replay(vec![(200, &page(&[future, cancel])), (200, "{}")]);
    let got = ddb(&client).take_controls("R1", "a1").await.unwrap();
    assert_eq!(got, vec![PendingControl::Cancel]);
    // Only the cancel was marked delivered; the item this build cannot read stays undelivered for a build that can.
    let b = bodies(&client);
    assert_eq!(b.len(), 2);
    assert_eq!(
        b[1]["Key"]["SK"],
        serde_json::json!({"S": "CTRL#a1#01M5A2R7K3C9XJ4D8E6FQ0WT21"})
    );
}

const TURN_ITEM: &str = r#"{"Item": {
  "PK": {"S": "RUN#R1"}, "SK": {"S": "TURN#a1#000002"}, "runId": {"S": "R1"}, "agentId": {"S": "a1"}, "n": {"N": "2"},
  "taskTokenCiphertext": {"S": "Y3Q="}, "startedAt": {"S": "2026-09-25T20:00:00Z"}, "rulesDigest": {"S": ""},
  "version": {"N": "1"}}}"#;

#[tokio::test]
async fn the_open_turn_is_read_by_its_key_and_ended_once() {
    let client = replay(vec![
        (200, TURN_ITEM),
        (200, "{}"),
        (400, CONDITIONAL_FAILED),
    ]);
    let s = ddb(&client);
    let t = s.get_turn("R1", "a1", 2).await.unwrap().unwrap();
    assert_eq!(t.task_token_ciphertext, "Y3Q=");
    s.mark_turn_ended("R1", "a1", 2).await.unwrap();
    // Already ended: the conditional write fails and that is not an error.
    s.mark_turn_ended("R1", "a1", 2).await.unwrap();
    let b = bodies(&client);
    assert_eq!(b[0]["ConsistentRead"], true);
    assert_eq!(
        b[0]["Key"],
        serde_json::json!({"PK": {"S": "RUN#R1"}, "SK": {"S": "TURN#a1#000002"}})
    );
    assert_eq!(
        b[1]["ConditionExpression"],
        "attribute_exists(PK) AND attribute_not_exists(endedAt)"
    );
    assert!(
        b[1]["UpdateExpression"]
            .as_str()
            .unwrap()
            .contains("endedAt")
    );
}

fn callbacks(kms: &StaticReplayClient, sfn: &StaticReplayClient) -> SfnCallbacks {
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
    SfnCallbacks {
        kms: client!(aws_sdk_kms, kms),
        sfn: client!(aws_sdk_sfn, sfn),
    }
}

fn ctx() -> TokenContext {
    TokenContext {
        run_id: "R1".into(),
        agent_id: "a1".into(),
        turn: 2,
    }
}

#[tokio::test]
async fn a_turn_is_reported_with_the_token_decrypted_under_the_callers_context() {
    // Plaintext "tok-1", base64 dG9rLTE=.
    let kms = replay(vec![(200, r#"{"Plaintext": "dG9rLTE=", "KeyId": "k"}"#)]);
    let sfn = replay(vec![(200, "{}")]);
    let c = callbacks(&kms, &sfn);
    c.turn_done("Y2lwaGVy", &ctx(), r#"{"runId":"R1"}"#)
        .await
        .unwrap();
    let k = &bodies(&kms)[0];
    assert_eq!(k["CiphertextBlob"], "Y2lwaGVy");
    assert_eq!(
        k["EncryptionContext"],
        serde_json::json!({"run": "R1", "agent": "a1", "turn": "2"})
    );
    let s = &bodies(&sfn)[0];
    assert_eq!(s["taskToken"], "tok-1");
    assert_eq!(s["output"], r#"{"runId":"R1"}"#);
}

#[tokio::test]
async fn heartbeats_decrypt_the_same_way_and_closed_tasks_are_named() {
    let kms = replay(vec![
        (200, r#"{"Plaintext": "dG9rLTE=", "KeyId": "k"}"#),
        (200, r#"{"Plaintext": "dG9rLTE=", "KeyId": "k"}"#),
        (
            400,
            r#"{"__type": "InvalidCiphertextException", "message": "context mismatch"}"#,
        ),
    ]);
    let sfn = replay(vec![
        (200, "{}"),
        (
            400,
            r#"{"__type": "TaskTimedOut", "message": "Task Timed Out"}"#,
        ),
    ]);
    let c = callbacks(&kms, &sfn);
    c.turn_alive("Y2lwaGVy", &ctx()).await.unwrap();
    assert_eq!(bodies(&sfn)[0]["taskToken"], "tok-1");
    let e = c.turn_alive("Y2lwaGVy", &ctx()).await.unwrap_err();
    assert!(
        matches!(e, CallbackError::Closed(ref m) if m.contains("TaskTimedOut")),
        "{e:?}"
    );
    // A ciphertext bound to another run, agent or turn fails to decrypt: nothing can be sent.
    let e = c.turn_done("Y2lwaGVy", &ctx(), "{}").await.unwrap_err();
    assert!(
        matches!(e, CallbackError::Closed(ref m) if m.contains("InvalidCiphertext")),
        "{e:?}"
    );
    assert_eq!(sfn.actual_requests().count(), 2);
}
