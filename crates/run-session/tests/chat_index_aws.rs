//! The adapters the chat-content index uses against recorded HTTP exchanges (aws-smithy `StaticReplayClient`; no network):
//! the S3 list and bounded get of the transcript prefix, the raw reads of the Agent item and the user's answers, and the one
//! UpdateItem that writes the search body. The request each sends is read back off the wire.
#![allow(clippy::unwrap_used)]

use aws_smithy_http_client::test_util::{ReplayEvent, StaticReplayClient};
use aws_smithy_types::body::SdkBody;
use ham_core::runsearch::{Coverage, SearchBody, Seg, chat_sk, owner_pk, segs_to_string};
use run_session::aws::{DynamoStore, S3Blobs};
use run_session::error::RunSessionError;
use run_session::ports::{Blobs, SearchWrite, Store};
use serde_json::{Value, json};

const RUN: &str = "01M4X3Q7R9S0V1W2X3Y4Z5A6B7";
const TABLE: &str = "hamh-direct-harness";
const BUCKET: &str = "hamh-direct-harness-700000000011";
const OWNER: &str = "arn:aws:sts::700000000011:assumed-role/Admin/testuser";
const PREFIX: &str = "runs/01M4X3Q7R9S0V1W2X3Y4Z5A6B7/agents/direct/turns/1/transcript/";

type Reply = (u16, Vec<(&'static str, &'static str)>, Vec<u8>);

fn replay(replies: Vec<Reply>) -> StaticReplayClient {
    StaticReplayClient::new(
        replies
            .into_iter()
            .map(|(status, headers, body)| {
                let mut r = http::Response::builder().status(status);
                for (k, v) in headers {
                    r = r.header(k, v);
                }
                ReplayEvent::new(
                    http::Request::builder()
                        .uri("https://example.invalid/")
                        .body(SdkBody::empty())
                        .unwrap(),
                    r.body(SdkBody::from(body)).unwrap(),
                )
            })
            .collect(),
    )
}

fn json10(bodies: Vec<&str>) -> StaticReplayClient {
    replay(
        bodies
            .into_iter()
            .map(|b| {
                (
                    200,
                    vec![("content-type", "application/x-amz-json-1.0")],
                    b.as_bytes().to_vec(),
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
                .region($sdk::config::Region::new("us-east-1"))
                .credentials_provider($sdk::config::Credentials::for_tests())
                .retry_config($sdk::config::retry::RetryConfig::disabled())
                .http_client($replay.clone())
                .build(),
        )
    };
}

fn s3(r: &StaticReplayClient) -> S3Blobs {
    S3Blobs {
        client: client!(aws_sdk_s3, r),
        bucket: BUCKET.into(),
    }
}

fn ddb(r: &StaticReplayClient) -> DynamoStore {
    DynamoStore {
        client: client!(aws_sdk_dynamodb, r),
        table: TABLE.into(),
        research_table: None,
    }
}

fn bodies(r: &StaticReplayClient) -> Vec<Value> {
    r.actual_requests()
        .map(|q| serde_json::from_slice(q.body().bytes().unwrap_or_default()).unwrap())
        .collect()
}

fn query_param(uri: &str, name: &str) -> Option<String> {
    uri.split_once('?')?.1.split('&').find_map(|kv| {
        kv.strip_prefix(&format!("{name}="))
            .map(|v| v.replace("%2F", "/"))
    })
}

const LISTING: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Name>b</Name><Prefix>p</Prefix><KeyCount>2</KeyCount><MaxKeys>1000</MaxKeys><IsTruncated>true</IsTruncated>
<Contents><Key>runs/01M4X3Q7R9S0V1W2X3Y4Z5A6B7/agents/direct/turns/1/transcript/000000000000.ndjson.gz</Key><LastModified>2026-10-16T10:00:00.000Z</LastModified><ETag>&quot;aa&quot;</ETag><Size>321</Size></Contents>
<Contents><Key>runs/01M4X3Q7R9S0V1W2X3Y4Z5A6B7/agents/direct/turns/1/transcript/junk</Key><LastModified>2026-10-16T10:00:01.000Z</LastModified><ETag>&quot;bb&quot;</ETag><Size>7</Size></Contents>
</ListBucketResult>"#;

#[tokio::test]
async fn a_listing_is_a_v2_list_after_a_key_with_the_sizes_the_indexer_checks_first() {
    let empty = r#"<?xml version="1.0" encoding="UTF-8"?><ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Name>b</Name><IsTruncated>false</IsTruncated></ListBucketResult>"#;
    let r = replay(vec![
        (
            200,
            vec![("content-type", "application/xml")],
            LISTING.as_bytes().to_vec(),
        ),
        (
            200,
            vec![("content-type", "application/xml")],
            empty.as_bytes().to_vec(),
        ),
        (
            200,
            vec![("content-type", "application/xml")],
            empty.as_bytes().to_vec(),
        ),
    ]);
    let b = s3(&r);
    let after = format!("{PREFIX}000000000000.ndjson.gz");
    let page = b.list(PREFIX, Some(&after), 1000).await.unwrap();
    assert!(page.truncated);
    assert_eq!(page.objects.len(), 2);
    assert_eq!(
        (page.objects[0].key.as_str(), page.objects[0].size),
        ((after.as_str()), 321)
    );
    assert_eq!(page.objects[1].size, 7);
    assert!(b.list(PREFIX, None, 0).await.unwrap().objects.is_empty());
    assert!(
        b.list(PREFIX, None, 50_000)
            .await
            .unwrap()
            .objects
            .is_empty()
    );
    let sent: Vec<(String, Option<String>, Option<String>)> = r
        .actual_requests()
        .map(|q| {
            (
                query_param(q.uri(), "prefix").unwrap(),
                query_param(q.uri(), "start-after"),
                query_param(q.uri(), "max-keys"),
            )
        })
        .collect();
    assert_eq!(
        sent[0],
        (PREFIX.to_string(), Some(after), Some("1000".into()))
    );
    assert_eq!(
        (sent[1].1.clone(), sent[1].2.clone()),
        (None, Some("1".into())),
        "a count of 0 is sent as 1"
    );
    assert_eq!(
        sent[2].2.as_deref(),
        Some("1000"),
        "and a count over 1,000 as 1,000"
    );
    assert_eq!(
        query_param(r.actual_requests().next().unwrap().uri(), "list-type").as_deref(),
        Some("2")
    );
}

#[tokio::test]
async fn a_bounded_get_returns_the_object_and_nothing_when_it_is_missing_or_over_the_bound() {
    let key = format!("{PREFIX}000000000000.ndjson.gz");
    let nosuch = r#"<Error><Code>NoSuchKey</Code><Message>The specified key does not exist.</Message></Error>"#;
    let r = replay(vec![
        (200, vec![("content-length", "5")], b"hello".to_vec()),
        (
            404,
            vec![("content-type", "application/xml")],
            nosuch.as_bytes().to_vec(),
        ),
        (200, vec![("content-length", "2000")], vec![7u8; 2000]),
        (200, vec![], vec![7u8; 2000]),
        (200, vec![("content-length", "10")], vec![7u8; 10]),
        (
            403,
            vec![("content-type", "application/xml")],
            br#"<Error><Code>AccessDenied</Code><Message>no</Message></Error>"#.to_vec(),
        ),
    ]);
    let b = s3(&r);
    assert_eq!(
        b.get_bounded(&key, 1024).await.unwrap(),
        Some(b"hello".to_vec())
    );
    assert_eq!(b.get_bounded(&key, 1024).await.unwrap(), None, "missing");
    assert_eq!(
        b.get_bounded(&key, 1000).await.unwrap(),
        None,
        "over the bound by its length"
    );
    assert_eq!(
        b.get_bounded(&key, 1000).await.unwrap(),
        None,
        "over the bound while it streams"
    );
    assert_eq!(
        b.get_bounded(&key, 10).await.unwrap().unwrap().len(),
        10,
        "exactly the bound"
    );
    assert!(matches!(
        b.get_bounded(&key, 10).await,
        Err(RunSessionError::Unavailable(_))
    ));
    assert!(r.actual_requests().next().unwrap().uri().contains(&key));
}

fn agent_item(features: &str) -> String {
    format!(
        r#"{{"Item": {{"PK": {{"S": "RUN#{RUN}"}}, "SK": {{"S": "AGENT#direct"}}, "agentId": {{"S": "direct"}}{features}}}}}"#
    )
}

#[tokio::test]
async fn the_agent_items_features_decide_whether_the_run_stored_a_transcript() {
    for (features, want) in [
        (
            r#", "features": {"L": [{"S": "x"}, {"S": "transcript.v1"}]}"#,
            true,
        ),
        (r#", "features": {"L": []}"#, false),
        (r#", "features": {"L": [{"S": "transcript.v2"}]}"#, false),
        (r#", "features": {"S": "transcript.v1"}"#, false),
        ("", false),
    ] {
        let a = agent_item(features);
        let r = json10(vec![&a]);
        assert_eq!(
            ddb(&r).transcript_supported(RUN).await.unwrap(),
            want,
            "{features}"
        );
        let b = bodies(&r);
        assert_eq!(
            b[0]["Key"],
            json!({"PK": {"S": format!("RUN#{RUN}")}, "SK": {"S": "AGENT#direct"}})
        );
        assert_eq!(b[0]["ConsistentRead"], true);
    }
    let r = json10(vec!["{}"]);
    assert!(
        !ddb(&r).transcript_supported(RUN).await.unwrap(),
        "no Agent item"
    );
}

fn control(id: &str, control: &str) -> String {
    format!(
        r#"{{"PK": {{"S": "RUN#{RUN}"}}, "SK": {{"S": "CTRL#direct#{id}"}}, "runId": {{"S": "{RUN}"}}, "agentId": {{"S": "direct"}}, "controlId": {{"S": "{id}"}}, "createdAt": {{"S": "2026-10-16T10:00:00Z"}}, "control": {{"M": {control}}}}}"#
    )
}

#[tokio::test]
async fn the_users_answers_are_the_instruct_controls_with_text_in_order_and_nothing_else() {
    let instruct = |action: &str, text: Option<&str>, origin: &str| {
        let t = text.map_or(String::new(), |t| format!(r#", "text": {{"S": "{t}"}}"#));
        format!(
            r#"{{"kind": {{"S": "instruct"}}, "requestId": {{"S": "r1"}}, "action": {{"S": "{action}"}}, "origin": {{"S": "{origin}"}}{t}}}"#
        )
    };
    let items = [
        control(
            "01M4X3Q7R9S0V1W2X3Y4Z5A6B1",
            &instruct("continue_with_prompt", Some("use pytest"), "user"),
        ),
        control(
            "01M4X3Q7R9S0V1W2X3Y4Z5A6B2",
            &instruct("continue_with_prompt", Some("analyzer words"), "analyzer"),
        ),
        control(
            "01M4X3Q7R9S0V1W2X3Y4Z5A6B3",
            &instruct("finish", None, "user"),
        ),
        control(
            "01M4X3Q7R9S0V1W2X3Y4Z5A6B4",
            r#"{"kind": {"S": "steer"}, "text": {"S": "steering"}, "now": {"BOOL": false}}"#,
        ),
        control("01M4X3Q7R9S0V1W2X3Y4Z5A6B5", r#"{"kind": {"S": "cancel"}}"#),
        control(
            "01M4X3Q7R9S0V1W2X3Y4Z5A6B6",
            r#"{"kind": {"S": "somethingNew"}}"#,
        ),
        control(
            "01M4X3Q7R9S0V1W2X3Y4Z5A6B7",
            &instruct(
                "new_session_with_prompt",
                Some("start over with Rust"),
                "user",
            ),
        ),
    ];
    let page = format!(r#"{{"Items": [{}], "Count": 7}}"#, items.join(","));
    let r = json10(vec![&page]);
    let got = ddb(&r).user_answers(RUN).await.unwrap();
    assert_eq!(
        got,
        vec!["use pytest".to_string(), "start over with Rust".to_string()]
    );
    let b = &bodies(&r)[0];
    assert_eq!(
        b["KeyConditionExpression"],
        "PK = :pk AND begins_with(SK, :sk)"
    );
    assert_eq!(
        b["ExpressionAttributeValues"][":pk"],
        json!({"S": format!("RUN#{RUN}")})
    );
    assert_eq!(
        b["ExpressionAttributeValues"][":sk"],
        json!({"S": "CTRL#direct#"})
    );
    assert_eq!(
        (b["ConsistentRead"].clone(), b["Limit"].clone()),
        (json!(true), json!(50))
    );
}

fn write<'a>(pk: &'a str, sk: &'a str, body: &'a SearchBody) -> SearchWrite<'a> {
    SearchWrite {
        pk,
        sk,
        run_id: RUN,
        created_at: "2026-10-16T10:00:00Z",
        body,
        indexed_at: "2026-10-16T11:00:00Z",
    }
}

#[tokio::test]
async fn the_search_body_is_one_update_that_sets_only_its_own_attributes() {
    let body = SearchBody {
        body: "objective\nI will start\n".into(),
        segs: vec![
            Seg {
                start: 0,
                seq: None,
                rec: 0,
            },
            Seg {
                start: 10,
                seq: Some(0),
                rec: 1,
            },
        ],
        through: Some(2),
        coverage: Coverage::Full,
    };
    let (pk, sk) = (owner_pk(OWNER), chat_sk("2026-10-16T10:00:00Z", RUN));
    let r = json10(vec!["{}", "{}"]);
    let store = ddb(&r);
    store
        .put_search_body(&write(&pk, &sk, &body))
        .await
        .unwrap();
    let b = &bodies(&r)[0];
    assert_eq!(b["TableName"], TABLE);
    assert_eq!(b["Key"], json!({"PK": {"S": pk}, "SK": {"S": sk}}));
    assert_eq!(
        b["UpdateExpression"],
        "SET #r = :r, #c = :c, #b = :b, #s = :s, #v = :v, #i = :i, #t = :t"
    );
    assert_eq!(
        b["ExpressionAttributeNames"],
        json!({"#r": "runId", "#c": "createdAt", "#b": "body", "#s": "segs", "#v": "coverage", "#i": "indexedAt", "#t": "bodyThrough"})
    );
    let v = &b["ExpressionAttributeValues"];
    assert_eq!(v[":r"], json!({"S": RUN}));
    assert_eq!(v[":c"], json!({"S": "2026-10-16T10:00:00Z"}));
    assert_eq!(v[":b"], json!({"S": "objective\nI will start\n"}));
    assert_eq!(v[":s"], json!({"S": segs_to_string(&body.segs)}));
    assert_eq!(v[":v"], json!({"S": "full"}));
    assert_eq!(v[":i"], json!({"S": "2026-10-16T11:00:00Z"}));
    assert_eq!(v[":t"], json!({"N": "2"}));
    // It is an update (the run index's own attributes survive), unconditional (a repeat writes the same values) and it never carries
    // the attribute the run-list index is sparse on, nor a key attribute as a value.
    let text = b.to_string();
    for forbidden in [
        "ownerPrincipalArn",
        "ConditionExpression",
        "promptHead",
        "model",
        "repository",
    ] {
        assert!(!text.contains(forbidden), "{forbidden} in {text}");
    }
    // A body with no chunk behind it removes the old high-water mark instead of setting one.
    let none = SearchBody {
        through: None,
        ..body.clone()
    };
    store
        .put_search_body(&write(&pk, &sk, &none))
        .await
        .unwrap();
    let b = &bodies(&r)[1];
    assert_eq!(
        b["UpdateExpression"],
        "SET #r = :r, #c = :c, #b = :b, #s = :s, #v = :v, #i = :i REMOVE #t"
    );
    assert!(b["ExpressionAttributeValues"].get(":t").is_none());
}

#[tokio::test]
async fn a_failing_write_or_read_is_unavailable() {
    let err =
        r#"{"__type":"com.amazonaws.dynamodb.v20120810#InternalServerError","message":"boom"}"#;
    let r = replay(vec![
        (
            500,
            vec![("content-type", "application/x-amz-json-1.0")],
            err.as_bytes().to_vec()
        );
        3
    ]);
    let store = ddb(&r);
    let body = SearchBody {
        body: String::new(),
        segs: vec![],
        through: None,
        coverage: Coverage::Full,
    };
    assert!(matches!(
        store
            .put_search_body(&write("OWNER#x", "CHAT#y", &body))
            .await,
        Err(RunSessionError::Unavailable(_))
    ));
    assert!(matches!(
        store.user_answers(RUN).await,
        Err(RunSessionError::Unavailable(_))
    ));
    assert!(matches!(
        store.transcript_supported(RUN).await,
        Err(RunSessionError::Unavailable(_))
    ));
}

#[tokio::test]
async fn a_partial_body_never_replaces_a_full_one() {
    let ccf = r#"{"__type":"com.amazonaws.dynamodb.v20120810#ConditionalCheckFailedException","message":"The conditional request failed"}"#;
    let r = replay(vec![
        (
            200,
            vec![("content-type", "application/x-amz-json-1.0")],
            b"{}".to_vec(),
        ),
        (
            400,
            vec![("content-type", "application/x-amz-json-1.0")],
            ccf.as_bytes().to_vec(),
        ),
    ]);
    let store = ddb(&r);
    let body = SearchBody {
        body: "x\n".into(),
        segs: vec![],
        through: Some(1),
        coverage: Coverage::Partial,
    };
    let (pk, sk) = (owner_pk(OWNER), chat_sk("2026-10-16T10:00:00Z", RUN));
    store
        .put_search_body(&write(&pk, &sk, &body))
        .await
        .unwrap();
    // The item already holds a full body: the condition fails, which is not an error.
    store
        .put_search_body(&write(&pk, &sk, &body))
        .await
        .unwrap();
    let b = bodies(&r);
    for sent in &b {
        assert_eq!(
            sent["ConditionExpression"],
            "attribute_not_exists(#v) OR #v = :p"
        );
        assert_eq!(
            sent["ExpressionAttributeValues"][":p"],
            json!({"S": "partial"})
        );
    }
    // A full body has no condition (checked in the update test above).
}
