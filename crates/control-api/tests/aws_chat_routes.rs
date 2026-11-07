//! The DynamoDB and S3 calls behind the chat-client routes,
//! against recorded HTTP exchanges (StaticReplayClient): the exact requests they send and how they read the answers. No network.
#![allow(clippy::unwrap_used)]

use aws_smithy_http_client::test_util::{ReplayEvent, StaticReplayClient};
use aws_smithy_types::body::SdkBody;
use control_api::aws::{DynamoStore, S3Blobs};
use control_api::handler::{Blobs, Store, StoreError};
use ham_core::run_index::{RunIndexItem, owner_pk, search_sk};
use harness_protocol::envelope::Authority;
use harness_protocol::store::event_shard;
use serde_json::{Value, json};
use std::time::Duration;

const RUN: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Y";
const OWNER: &str = "arn:aws:sts::700000000011:assumed-role/Admin/testuser";
const THROTTLED: &str =
    r#"{"__type":"com.amazonaws.dynamodb.v20120810#ThrottlingException","message":"slow down"}"#;

fn event_id(n: u64) -> String {
    format!("01M5A2R7K3{n:016}")
}

/// One recorded answer: status, body, headers.
type Answer = (u16, String, Vec<(&'static str, String)>);

fn replay(responses: Vec<Answer>) -> StaticReplayClient {
    StaticReplayClient::new(
        responses
            .into_iter()
            .map(|(status, body, headers)| {
                let mut b = http::Response::builder().status(status);
                for (k, v) in headers {
                    b = b.header(k, v);
                }
                ReplayEvent::new(
                    http::Request::builder()
                        .uri("https://example.invalid/")
                        .body(SdkBody::empty())
                        .unwrap(),
                    b.body(SdkBody::from(body)).unwrap(),
                )
            })
            .collect(),
    )
}

fn json_ok(body: Value) -> Answer {
    (
        200,
        body.to_string(),
        vec![("content-type", "application/x-amz-json-1.0".into())],
    )
}

fn json_err(status: u16, body: &str) -> Answer {
    (
        status,
        body.to_string(),
        vec![("content-type", "application/x-amz-json-1.0".into())],
    )
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
        table: "hamh-direct-harness".into(),
    }
}

fn s3(r: &StaticReplayClient) -> S3Blobs {
    S3Blobs {
        client: aws_sdk_s3::Client::from_conf(
            aws_sdk_s3::Config::builder()
                .behavior_version_latest()
                .region(aws_sdk_s3::config::Region::new("us-east-1"))
                .credentials_provider(aws_sdk_s3::config::Credentials::for_tests())
                .retry_config(aws_sdk_s3::config::retry::RetryConfig::disabled())
                .http_client(r.clone())
                .force_path_style(true)
                .build(),
        ),
        bucket: "hamh-direct-harness-700000000011".into(),
        credentials: None,
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

fn event_item(id: &str) -> Value {
    json!({
        "PK": {"S": format!("EVT#{RUN}#{:02}", shard())}, "SK": {"S": id},
        "envelope": {"M": {
            "schemaVersion": {"N": "1"}, "eventId": {"S": id}, "runId": {"S": RUN}, "agentId": {"S": "direct"},
            "seq": {"N": "7"}, "ts": {"S": "2026-10-15T13:41:07Z"}, "type": {"S": "turn.progress"}, "authority": {"S": "agent"},
            "payload": {"M": {"kind": {"S": "inline"}, "value": {"M": {"kind": {"S": "message"}, "text": {"S": "x"}}}}}
        }}
    })
}

fn lek(id: &str) -> Value {
    json!({"PK": {"S": format!("EVT#{RUN}#{:02}", shard())}, "SK": {"S": id}})
}

// ---- events from the end ----------------------------------------------------------------------------------------------

#[tokio::test]
async fn events_before_is_a_reverse_consistent_query_below_the_cursor_returned_oldest_first() {
    let r = replay(vec![json_ok(json!({"Items": [
        event_item(&event_id(9)), event_item(&event_id(8)), event_item(&event_id(7))
    ]}))]);
    let got = ddb(&r)
        .events_before(RUN, shard(), Some(&event_id(10)), 3)
        .await
        .unwrap();
    let ids: Vec<String> = got.iter().map(|e| e.event_id.clone()).collect();
    assert_eq!(ids, vec![event_id(7), event_id(8), event_id(9)]);
    let b = bodies(&r);
    assert_eq!(b.len(), 1);
    assert_eq!(b[0]["TableName"], "hamh-direct-harness");
    assert_eq!(b[0]["KeyConditionExpression"], "PK = :pk AND SK < :before");
    assert_eq!(
        b[0]["ExpressionAttributeValues"][":before"],
        json!({"S": event_id(10)})
    );
    assert_eq!(
        b[0]["ExpressionAttributeValues"][":pk"],
        json!({"S": format!("EVT#{RUN}#{:02}", shard())})
    );
    assert_eq!(b[0]["ScanIndexForward"], false);
    assert_eq!(b[0]["ConsistentRead"], true);
    assert_eq!(b[0]["Limit"], 3);
}

#[tokio::test]
async fn without_a_cursor_the_query_has_no_sort_key_condition() {
    let r = replay(vec![json_ok(
        json!({"Items": [event_item(&event_id(2)), event_item(&event_id(1))]}),
    )]);
    let got = ddb(&r).events_before(RUN, shard(), None, 5).await.unwrap();
    assert_eq!(got.len(), 2);
    let b = bodies(&r);
    assert_eq!(b[0]["KeyConditionExpression"], "PK = :pk");
    assert!(b[0]["ExpressionAttributeValues"].get(":before").is_none());
}

#[tokio::test]
async fn a_one_megabyte_page_does_not_end_the_read_early() {
    // DynamoDB stops a Query at 1 MB and hands back a LastEvaluatedKey; the read goes on for what is still missing.
    let r = replay(vec![
        json_ok(
            json!({"Items": [event_item(&event_id(9)), event_item(&event_id(8))], "LastEvaluatedKey": lek(&event_id(8))}),
        ),
        json_ok(
            json!({"Items": [event_item(&event_id(7)), event_item(&event_id(6))], "LastEvaluatedKey": lek(&event_id(6))}),
        ),
    ]);
    let got = ddb(&r).events_before(RUN, shard(), None, 4).await.unwrap();
    let ids: Vec<String> = got.iter().map(|e| e.event_id.clone()).collect();
    assert_eq!(
        ids,
        vec![event_id(6), event_id(7), event_id(8), event_id(9)]
    );
    let b = bodies(&r);
    assert_eq!(b.len(), 2, "the second call carries the rest of the work");
    assert_eq!(b[0]["Limit"], 4);
    assert_eq!(b[1]["Limit"], 2);
    assert_eq!(b[1]["ExclusiveStartKey"], lek(&event_id(8)));
}

#[tokio::test]
async fn a_malformed_item_is_skipped_and_the_read_goes_on_for_the_missing_one() {
    let bad = json!({"PK": {"S": "x"}, "SK": {"S": event_id(5)}, "envelope": {"S": "garbage"}});
    let r = replay(vec![
        json_ok(
            json!({"Items": [bad, event_item(&event_id(4))], "LastEvaluatedKey": lek(&event_id(4))}),
        ),
        json_ok(json!({"Items": [event_item(&event_id(3))]})),
    ]);
    let got = ddb(&r).events_before(RUN, shard(), None, 2).await.unwrap();
    let ids: Vec<String> = got.iter().map(|e| e.event_id.clone()).collect();
    assert_eq!(ids, vec![event_id(3), event_id(4)]);
}

#[tokio::test]
async fn a_failed_read_is_unavailable() {
    let r = replay(vec![json_err(400, THROTTLED)]);
    assert!(matches!(
        ddb(&r).events_before(RUN, shard(), None, 5).await,
        Err(StoreError::Unavailable(_))
    ));
}

// ---- the run index ----------------------------------------------------------------------------------------------------

fn index_item(n: u64) -> RunIndexItem {
    RunIndexItem {
        run_id: format!("01K5{n:022}"),
        created_at: format!("2026-10-15T13:00:{n:02}Z"),
        prompt_head: "Add percent_change".into(),
        prompt_truncated: false,
        model: Some("global.anthropic.claude-sonnet-5-5".into()),
        repository: "s3-only".into(),
        outcome: None,
        last_activity: format!("2026-10-15T13:00:{n:02}Z"),
        run_name: None,
        run_name_source: None,
        run_name_model: None,
        ..Default::default()
    }
}

fn stored(owner: &str, i: &RunIndexItem) -> Value {
    json!({
        "PK": {"S": owner_pk(owner)}, "SK": {"S": search_sk(&i.created_at, &i.run_id)},
        "runId": {"S": i.run_id}, "createdAt": {"S": i.created_at}, "promptHead": {"S": i.prompt_head},
        "promptTruncated": {"BOOL": false}, "repository": {"S": i.repository}, "lastActivity": {"S": i.last_activity},
        "model": {"S": i.model.clone().unwrap()}
    })
}

#[tokio::test]
async fn the_page_window_is_one_between_query_on_the_owner_partition() {
    let (a, b) = (index_item(5), index_item(9));
    let r = replay(vec![json_ok(
        json!({"Items": [stored(OWNER, &a), stored(OWNER, &b)]}),
    )]);
    let got = ddb(&r)
        .run_index_between(OWNER, "2026-10-15T13:00:05Z", "2026-10-15T13:00:09Z")
        .await
        .unwrap();
    assert_eq!(got, vec![a, b]);
    let q = &bodies(&r)[0];
    assert_eq!(
        q["KeyConditionExpression"],
        "PK = :pk AND SK BETWEEN :lo AND :hi"
    );
    assert_eq!(
        q["ExpressionAttributeValues"][":pk"],
        json!({"S": owner_pk(OWNER)})
    );
    assert_eq!(
        q["ExpressionAttributeValues"][":lo"],
        json!({"S": "SRCH#2026-10-15T13:00:05Z"})
    );
    assert_eq!(
        q["ExpressionAttributeValues"][":hi"],
        json!({"S": "SRCH#2026-10-15T13:00:09Z#~"})
    );
    assert_eq!(q["ConsistentRead"], true);
    assert!(q.get("IndexName").is_none(), "the base table, never GSI1");
}

#[tokio::test]
async fn the_window_read_follows_the_last_evaluated_key() {
    let (a, b) = (index_item(1), index_item(2));
    let k = json!({"PK": {"S": owner_pk(OWNER)}, "SK": {"S": search_sk(&a.created_at, &a.run_id)}});
    let r = replay(vec![
        json_ok(json!({"Items": [stored(OWNER, &a)], "LastEvaluatedKey": k})),
        json_ok(json!({"Items": [stored(OWNER, &b)]})),
    ]);
    let got = ddb(&r)
        .run_index_between(OWNER, "2026-10-15T13:00:01Z", "2026-10-15T13:00:02Z")
        .await
        .unwrap();
    assert_eq!(got.len(), 2);
    assert_eq!(bodies(&r)[1]["ExclusiveStartKey"], k);
}

#[tokio::test]
async fn an_item_that_does_not_parse_is_skipped_and_a_failed_read_is_unavailable() {
    let junk = json!({"PK": {"S": owner_pk(OWNER)}, "SK": {"S": "SRCH#x#y"}, "runId": {"N": "5"}});
    let a = index_item(1);
    let r = replay(vec![
        json_ok(json!({"Items": [junk, stored(OWNER, &a)]})),
        json_err(400, THROTTLED),
    ]);
    let s = ddb(&r);
    let got = s
        .run_index_between(OWNER, "2026-10-15T13:00:01Z", "2026-10-15T13:00:01Z")
        .await
        .unwrap();
    assert_eq!(got, vec![a]);
    assert!(matches!(
        s.run_index_between(OWNER, "2026-10-15T13:00:01Z", "2026-10-15T13:00:01Z")
            .await,
        Err(StoreError::Unavailable(_))
    ));
}

#[tokio::test]
async fn the_index_write_keeps_every_field_that_is_there_and_never_carries_the_run_list_key() {
    // ALL_OLD returns the old item: none (created), a partial one (completed), a whole one (nothing to do).
    let partial = json!({"Attributes": {"runId": {"S": "x"}, "createdAt": {"S": "2026-10-15T13:00:03Z"}, "lastActivity": {"S": "2026-10-15T14:00:00Z"}}});
    let whole =
        json!({"Attributes": {"runId": {"S": "x"}, "promptHead": {"S": "Add percent_change"}}});
    let r = replay(vec![
        json_ok(json!({})),
        json_ok(partial),
        json_ok(whole),
        json_err(
            500,
            r#"{"__type":"com.amazonaws.dynamodb.v20120810#InternalServerError","message":"boom"}"#,
        ),
    ]);
    let s = ddb(&r);
    let item = index_item(3);
    assert!(s.put_run_index(OWNER, &item).await.unwrap(), "made");
    assert!(s.put_run_index(OWNER, &item).await.unwrap(), "completed");
    assert!(
        !s.put_run_index(OWNER, &item).await.unwrap(),
        "already whole"
    );
    assert!(matches!(
        s.put_run_index(OWNER, &item).await,
        Err(StoreError::Unavailable(_))
    ));
    let b = &bodies(&r)[0];
    assert_eq!(
        b["Key"],
        json!({"PK": {"S": owner_pk(OWNER)}, "SK": {"S": search_sk(&item.created_at, &item.run_id)}})
    );
    assert_eq!(b["ReturnValues"], "ALL_OLD");
    // Every start field is set only where it is not set yet, so a late retry or the backfill never overwrites what the end of a run wrote
    // (its outcome and its time are not in this expression at all).
    let expr = b["UpdateExpression"].as_str().unwrap();
    assert!(expr.starts_with("SET "), "{expr}");
    for f in [
        "runId",
        "createdAt",
        "promptHead",
        "promptTruncated",
        "repository",
        "lastActivity",
        "model",
    ] {
        assert!(
            expr.contains(&format!("{f} = if_not_exists({f},")),
            "{f}: {expr}"
        );
    }
    assert!(!expr.contains("outcome"), "{expr}");
    assert!(
        b.get("ConditionExpression").is_none(),
        "an upsert: the item may not exist"
    );
    // GSI1 is sparse on ownerPrincipalArn: an item that had the attribute would enter the run list.
    assert!(!b.to_string().contains("ownerPrincipalArn"), "{b}");
    // The values are the item's.
    assert_eq!(
        b["ExpressionAttributeValues"][":p"],
        json!({"S": "Add percent_change"})
    );
    assert_eq!(
        b["ExpressionAttributeValues"][":c"],
        json!({"S": item.created_at})
    );
}

#[tokio::test]
async fn an_item_without_a_model_sets_no_model() {
    let r = replay(vec![json_ok(json!({}))]);
    let mut item = index_item(4);
    item.model = None;
    ddb(&r).put_run_index(OWNER, &item).await.unwrap();
    let b = &bodies(&r)[0];
    assert!(!b["UpdateExpression"].as_str().unwrap().contains("model"));
}

// ---- the answers --------------------------------------------------------------------------------------------------------

fn control(id: &str) -> Value {
    json!({
        "PK": {"S": format!("RUN#{RUN}")}, "SK": {"S": format!("CTRL#direct#{id}")},
        "runId": {"S": RUN}, "agentId": {"S": "direct"}, "controlId": {"S": id},
        "control": {"M": {"kind": {"S": "instruct"}, "requestId": {"S": id}, "action": {"S": "continue_with_prompt"},
                          "text": {"S": "poll the log"}, "origin": {"S": "user"}}},
        "createdAt": {"S": "2026-10-15T13:42:38Z"}
    })
}

#[tokio::test]
async fn the_controls_of_a_window_are_one_consistent_between_query_that_says_when_there_are_more() {
    let r = replay(vec![json_ok(
        json!({"Items": [control(&event_id(1)), control(&event_id(2)), control(&event_id(3))]}),
    )]);
    let (got, more) = ddb(&r)
        .list_controls(RUN, "direct", &event_id(1), &event_id(9), 2)
        .await
        .unwrap();
    assert_eq!(got.len(), 2);
    assert!(more, "a third item was there");
    let q = &bodies(&r)[0];
    assert_eq!(
        q["KeyConditionExpression"],
        "PK = :pk AND SK BETWEEN :lo AND :hi"
    );
    assert_eq!(
        q["ExpressionAttributeValues"][":pk"],
        json!({"S": format!("RUN#{RUN}")})
    );
    assert_eq!(
        q["ExpressionAttributeValues"][":lo"],
        json!({"S": format!("CTRL#direct#{}", event_id(1))})
    );
    assert_eq!(
        q["ExpressionAttributeValues"][":hi"],
        json!({"S": format!("CTRL#direct#{}", event_id(9))})
    );
    assert_eq!(q["ConsistentRead"], true);
    assert_eq!(q["Limit"], 3, "one more than asked, to know");
}

#[tokio::test]
async fn the_controls_read_follows_the_last_evaluated_key_and_stops_at_the_limit() {
    let r = replay(vec![
        json_ok(
            json!({"Items": [control(&event_id(1))], "LastEvaluatedKey": {"PK": {"S": "p"}, "SK": {"S": "s"}}}),
        ),
        json_ok(json!({"Items": [control(&event_id(2))]})),
    ]);
    let (got, more) = ddb(&r)
        .list_controls(RUN, "direct", &event_id(1), &event_id(2), 5)
        .await
        .unwrap();
    assert_eq!((got.len(), more), (2, false));
    let b = bodies(&r);
    assert_eq!(
        b[1]["ExclusiveStartKey"],
        json!({"PK": {"S": "p"}, "SK": {"S": "s"}})
    );
    assert_eq!(b[1]["Limit"], 5, "6 wanted, 1 had");
}

// ---- reads that cannot finish are failures, not short answers ----------------------------------------------------------

#[tokio::test]
async fn a_read_that_runs_out_of_pages_is_unavailable_not_a_short_answer() {
    let page = |id: u64| json_ok(json!({"Items": [], "LastEvaluatedKey": lek(&event_id(id))}));
    // The window of the run index: more pages than one call reads.
    let r = replay((0..25).map(page).collect());
    assert!(matches!(
        ddb(&r)
            .run_index_between(OWNER, "2026-10-15T13:00:01Z", "2026-10-15T13:00:02Z")
            .await,
        Err(StoreError::Unavailable(m)) if m.contains("pages")
    ));
    assert!(bodies(&r).len() <= 21, "{}", bodies(&r).len());
    // Events whose items do not decode never fill the limit: the read stops instead of walking the shard.
    let r = replay((0..25).map(page).collect());
    assert!(matches!(
        ddb(&r).events_before(RUN, shard(), None, 5).await,
        Err(StoreError::Unavailable(m)) if m.contains("pages")
    ));
    let r = replay((0..25).map(page).collect());
    assert!(matches!(
        ddb(&r).list_controls(RUN, "direct", &event_id(1), &event_id(2), 5).await,
        Err(StoreError::Unavailable(m)) if m.contains("pages")
    ));
}

// ---- the snapshot -----------------------------------------------------------------------------------------------------

fn object(status: u16, body: &[u8], headers: Vec<(&'static str, String)>) -> Answer {
    (status, String::from_utf8_lossy(body).into_owned(), headers)
}

#[tokio::test]
async fn a_prefix_is_a_ranged_get_and_the_size_comes_from_content_range() {
    let r = replay(vec![object(
        206,
        b"# v2 git bundle\n",
        vec![
            ("content-range", "bytes 0-15/123456".into()),
            ("content-length", "16".into()),
            ("etag", "\"d41d8cd98f00b204e9800998ecf8427e\"".into()),
            (
                "x-amz-version-id",
                "3sL4kqtJlcpXroDTDmJ.rmSpXd3dIbrHY".into(),
            ),
            ("last-modified", "Thu, 15 Oct 2026 13:10:05 GMT".into()),
        ],
    )]);
    let got = s3(&r)
        .read_prefix("runs/R/agents/direct/turns/1/snapshot.bundle", 8192)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.bytes, b"# v2 git bundle\n");
    assert_eq!(got.size, 123_456);
    assert_eq!(
        got.etag.as_deref(),
        Some("\"d41d8cd98f00b204e9800998ecf8427e\"")
    );
    assert_eq!(
        got.version_id.as_deref(),
        Some("3sL4kqtJlcpXroDTDmJ.rmSpXd3dIbrHY")
    );
    assert_eq!(got.last_modified.as_deref(), Some("2026-10-15T13:10:05Z"));
    let q = r.actual_requests().next().unwrap();
    assert_eq!(q.headers().get("range"), Some("bytes=0-8191"));
    assert!(q.uri().contains("snapshot.bundle"), "{}", q.uri());
}

#[tokio::test]
async fn a_store_that_ignores_the_range_never_fills_the_lambda_with_the_object() {
    // 200 with the whole object: only the first `max_bytes` are kept, and the size is the object's.
    let body = vec![b'x'; 5_000];
    let r = replay(vec![object(
        200,
        &body,
        vec![("content-length", "5000".into())],
    )]);
    let got = s3(&r)
        .read_prefix("runs/R/agents/direct/turns/1/snapshot.bundle", 16)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.bytes.len(), 16);
    assert_eq!(got.size, 5_000);
}

#[tokio::test]
async fn a_presigned_url_for_a_version_names_the_version() {
    let creds = aws_sdk_s3::config::Credentials::new(
        "AKIDTESTONLY",
        "test-secret-not-a-key",
        None,
        None,
        "test",
    );
    let b = S3Blobs {
        client: aws_sdk_s3::Client::from_conf(
            aws_sdk_s3::Config::builder()
                .behavior_version_latest()
                .region(aws_sdk_s3::config::Region::new("us-east-1"))
                .credentials_provider(creds.clone())
                .build(),
        ),
        bucket: "hamh-direct-harness-700000000011".into(),
        credentials: Some(aws_sdk_s3::config::SharedCredentialsProvider::new(creds)),
    };
    let key = "runs/R/agents/direct/turns/1/snapshot.bundle";
    let pinned = b
        .presign_get_version(
            key,
            Some("3sL4kqtJlcpXroDTDmJ.rmSpXd3dIbrHY"),
            Duration::from_secs(900),
        )
        .await
        .unwrap();
    assert!(
        pinned
            .url
            .contains("versionId=3sL4kqtJlcpXroDTDmJ.rmSpXd3dIbrHY"),
        "{}",
        pinned.url
    );
    let plain = b
        .presign_get_version(key, None, Duration::from_secs(900))
        .await
        .unwrap();
    assert!(!plain.url.contains("versionId"), "{}", plain.url);
}

#[tokio::test]
async fn a_whole_small_object_answered_with_200_is_its_own_size() {
    let r = replay(vec![object(
        200,
        b"{\"a\":1}",
        vec![("content-length", "7".into())],
    )]);
    let got = s3(&r)
        .read_prefix("runs/R/agents/direct/turns/1/snapshot.json", 65_536)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.bytes, b"{\"a\":1}");
    assert_eq!(got.size, 7);
}

#[tokio::test]
async fn a_missing_key_is_none_an_empty_object_is_empty_and_a_failure_is_unavailable() {
    let missing = "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>NoSuchKey</Code><Message>The specified key does not exist.</Message></Error>";
    let empty = "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>InvalidRange</Code><Message>The requested range is not satisfiable</Message></Error>";
    let r = replay(vec![
        (
            404,
            missing.into(),
            vec![("content-type", "application/xml".into())],
        ),
        (
            416,
            empty.into(),
            vec![("content-type", "application/xml".into())],
        ),
        (
            503,
            "<Error><Code>SlowDown</Code></Error>".into(),
            vec![("content-type", "application/xml".into())],
        ),
    ]);
    let b = s3(&r);
    assert!(b.read_prefix("k1", 100).await.unwrap().is_none());
    let e = b.read_prefix("k2", 100).await.unwrap().unwrap();
    assert_eq!((e.bytes.len(), e.size), (0, 0));
    assert!(matches!(
        b.read_prefix("k3", 100).await,
        Err(StoreError::Unavailable(_))
    ));
}

// ---- `exists`: a listing whose prefix is the key ----------------------------------------------------------------------

fn xml_ok(body: &str) -> Answer {
    (
        200,
        body.to_string(),
        vec![("content-type", "application/xml".into())],
    )
}

fn listing(keys: &[&str]) -> String {
    let contents: String = keys
        .iter()
        .map(|k| {
            format!(
                "<Contents><Key>{k}</Key><LastModified>2026-10-15T13:00:00.000Z</LastModified><ETag>&quot;e&quot;</ETag><Size>7</Size><StorageClass>STANDARD</StorageClass></Contents>"
            )
        })
        .collect();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Name>b</Name><Prefix>p</Prefix><KeyCount>{}</KeyCount><MaxKeys>1</MaxKeys><IsTruncated>false</IsTruncated>{contents}</ListBucketResult>",
        keys.len()
    )
}

/// A role that may list only the prefix of one key gets a 404-or-403 on a GET of an absent key that tells nothing; the listing it is granted
/// is one call with `prefix` = the key, so the policy condition `s3:prefix` is satisfied, and only a key equal to it counts.
#[tokio::test]
async fn exists_is_one_listing_by_the_key_and_only_an_equal_key_counts() {
    let key = "runs/R/deliverable/verified.bundle";
    let r = replay(vec![
        xml_ok(&listing(&[key])),
        xml_ok(&listing(&[])),
        // A longer key sorts after the key and is not the key.
        xml_ok(&listing(&["runs/R/deliverable/verified.bundle.old"])),
        (
            403,
            "<?xml version=\"1.0\"?><Error><Code>AccessDenied</Code><Message>Access Denied</Message></Error>".into(),
            vec![("content-type", "application/xml".into())],
        ),
    ]);
    let b = s3(&r);
    assert!(b.exists(key).await.unwrap());
    assert!(!b.exists(key).await.unwrap());
    assert!(!b.exists(key).await.unwrap());
    assert!(matches!(
        b.exists(key).await,
        Err(StoreError::Unavailable(_))
    ));
    let requests: Vec<_> = r.actual_requests().collect();
    assert_eq!(requests.len(), 4, "one call each");
    let q = requests[0];
    assert_eq!(q.method(), "GET");
    assert!(q.uri().contains("list-type=2"), "{}", q.uri());
    assert!(
        q.uri()
            .contains("prefix=runs%2FR%2Fdeliverable%2Fverified.bundle")
            || q.uri()
                .contains("prefix=runs/R/deliverable/verified.bundle"),
        "{}",
        q.uri()
    );
    assert!(q.uri().contains("max-keys=1"), "{}", q.uri());
    assert!(
        q.headers().get("range").is_none(),
        "a listing, not a ranged GET"
    );
}
