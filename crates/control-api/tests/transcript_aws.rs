//! The S3 and DynamoDB adapters of the transcript and search routes against recorded HTTP exchanges (aws-smithy StaticReplayClient, as in
//! `tests/aws_adapters.rs`): the exact requests they send and how they read the answers. No network.
#![allow(clippy::unwrap_used)]

use aws_smithy_http_client::test_util::{ReplayEvent, StaticReplayClient};
use aws_smithy_types::body::SdkBody;
use control_api::aws::{DynamoStore, S3Blobs};
use control_api::handler::{Blobs, ObjectError, Store, StoreError};
use control_api::transcript::{SummaryFact, TranscriptFacts};
use ham_core::runsearch::{Coverage, owner_pk, sk_floor};
use ham_core::transcript::{CompletenessEntry, GapKind, TranscriptSummary};
use serde_json::{Value, json};

const RUN: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Y";
const OWNER: &str = "arn:aws:sts::800000000022:assumed-role/Admin/testuser";
const BUCKET: &str = "hamh-live-harness-800000000022";
const PREFIX: &str = "runs/01K5ZQ4T8M3N6P7R9S0V1W2X3Y/agents/direct/turns/1/transcript/";

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

fn json_replies(bodies: Vec<&str>) -> StaticReplayClient {
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
                .region($sdk::config::Region::new("us-west-2"))
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
        credentials: None,
    }
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

fn xml(status: u16, body: &str) -> Reply {
    (
        status,
        vec![("content-type", "application/xml")],
        body.as_bytes().to_vec(),
    )
}

fn query_param(uri: &str, name: &str) -> Option<String> {
    uri.split_once('?')?.1.split('&').find_map(|kv| {
        kv.strip_prefix(&format!("{name}="))
            .map(|v| v.replace("%2F", "/").replace("%3A", ":"))
    })
}

// ---- S3 --------------------------------------------------------------------------------------------------------------

const LISTING: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Name>b</Name><Prefix>p/</Prefix><KeyCount>2</KeyCount><MaxKeys>2</MaxKeys><IsTruncated>true</IsTruncated>
<Contents><Key>runs/01K5ZQ4T8M3N6P7R9S0V1W2X3Y/agents/direct/turns/1/transcript/000000000001.ndjson.gz</Key><LastModified>2026-10-16T10:00:00.000Z</LastModified><ETag>&quot;aa11&quot;</ETag><Size>1234</Size><StorageClass>STANDARD</StorageClass></Contents>
<Contents><Key>runs/01K5ZQ4T8M3N6P7R9S0V1W2X3Y/agents/direct/turns/1/transcript/junk</Key><LastModified>2026-10-16T10:00:01.000Z</LastModified><ETag>&quot;bb22&quot;</ETag><Size>5</Size><StorageClass>STANDARD</StorageClass></Contents>
</ListBucketResult>"#;

#[tokio::test]
async fn a_listing_is_a_v2_list_after_a_key_and_reads_keys_sizes_etags_and_truncation() {
    let r = replay(vec![xml(200, LISTING)]);
    let after = format!("{PREFIX}000000000000.ndjson.gz");
    let page = s3(&r).list_objects(PREFIX, Some(&after), 2).await.unwrap();
    assert!(page.truncated);
    assert_eq!(page.objects.len(), 2);
    assert_eq!(
        page.objects[0].key,
        format!("{PREFIX}000000000001.ndjson.gz")
    );
    assert_eq!(
        (page.objects[0].size, page.objects[0].etag.as_deref()),
        (1234, Some("\"aa11\""))
    );
    assert_eq!(page.objects[1].size, 5);
    let q = r.actual_requests().next().unwrap();
    assert_eq!(q.method(), "GET");
    let uri = q.uri().to_string();
    assert!(uri.contains(BUCKET), "{uri}");
    assert_eq!(query_param(&uri, "list-type").as_deref(), Some("2"));
    assert_eq!(query_param(&uri, "prefix").as_deref(), Some(PREFIX));
    assert_eq!(query_param(&uri, "start-after"), Some(after));
    assert_eq!(query_param(&uri, "max-keys").as_deref(), Some("2"));
    assert_eq!(query_param(&uri, "delimiter"), None, "keys are listed flat");
}

#[tokio::test]
async fn a_listing_without_a_start_key_sends_none_and_the_key_count_is_clamped_to_what_s3_accepts()
{
    let empty = r#"<?xml version="1.0" encoding="UTF-8"?><ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Name>b</Name><Prefix>p/</Prefix><KeyCount>0</KeyCount><MaxKeys>1000</MaxKeys><IsTruncated>false</IsTruncated></ListBucketResult>"#;
    let r = replay(vec![xml(200, empty), xml(200, empty), xml(200, empty)]);
    let b = s3(&r);
    for n in [0usize, 5000, 7] {
        let page = b.list_objects(PREFIX, None, n).await.unwrap();
        assert!(page.objects.is_empty() && !page.truncated);
    }
    let sent: Vec<(Option<String>, Option<String>)> = r
        .actual_requests()
        .map(|q| {
            (
                query_param(q.uri(), "max-keys"),
                query_param(q.uri(), "start-after"),
            )
        })
        .collect();
    assert_eq!(
        sent,
        vec![
            (Some("1".into()), None),
            (Some("1000".into()), None),
            (Some("7".into()), None)
        ]
    );
}

#[tokio::test]
async fn a_listing_that_fails_is_unavailable_and_carries_no_body() {
    let r = replay(vec![xml(
        403,
        r#"<Error><Code>AccessDenied</Code><Message>nope</Message></Error>"#,
    )]);
    let e = s3(&r).list_objects(PREFIX, None, 10).await.unwrap_err();
    assert!(matches!(e, StoreError::Unavailable(_)), "{e:?}");
}

fn object(status: u16, headers: Vec<(&'static str, &'static str)>, body: &[u8]) -> Reply {
    (status, headers, body.to_vec())
}

#[tokio::test]
async fn an_object_is_read_whole_and_only_while_its_etag_is_the_one_listed() {
    let key = format!("{PREFIX}000000000001.ndjson.gz");
    let r = replay(vec![
        object(
            200,
            vec![("content-length", "5"), ("etag", "\"aa11\"")],
            b"hello",
        ),
        object(200, vec![("content-length", "5")], b"world"),
    ]);
    let b = s3(&r);
    assert_eq!(
        b.get_object(&key, 1024, Some("\"aa11\"")).await.unwrap(),
        Some(b"hello".to_vec())
    );
    assert_eq!(
        b.get_object(&key, 1024, None).await.unwrap(),
        Some(b"world".to_vec())
    );
    let reqs: Vec<_> = r.actual_requests().collect();
    assert_eq!(reqs[0].method(), "GET");
    assert!(reqs[0].uri().contains(&key), "{}", reqs[0].uri());
    assert_eq!(reqs[0].headers().get("if-match"), Some("\"aa11\""));
    assert_eq!(reqs[1].headers().get("if-match"), None);
}

#[tokio::test]
async fn a_missing_key_a_replaced_object_and_a_refused_read_are_told_apart() {
    let key = format!("{PREFIX}000000000001.ndjson.gz");
    let r = replay(vec![
        xml(
            404,
            r#"<Error><Code>NoSuchKey</Code><Message>The specified key does not exist.</Message></Error>"#,
        ),
        xml(
            412,
            r#"<Error><Code>PreconditionFailed</Code><Message>At least one of the pre-conditions you specified did not hold</Message></Error>"#,
        ),
        xml(
            403,
            r#"<Error><Code>AccessDenied</Code><Message>Access Denied</Message></Error>"#,
        ),
    ]);
    let b = s3(&r);
    assert_eq!(b.get_object(&key, 1024, None).await.unwrap(), None);
    assert_eq!(
        b.get_object(&key, 1024, Some("\"x\"")).await,
        Err(ObjectError::Changed)
    );
    assert!(matches!(
        b.get_object(&key, 1024, None).await,
        Err(ObjectError::Store(StoreError::Unavailable(_)))
    ));
}

#[tokio::test]
async fn an_object_over_the_limit_is_refused_by_its_length_and_again_while_it_streams() {
    let key = format!("{PREFIX}000000000001.ndjson.gz");
    let r = replay(vec![
        object(200, vec![("content-length", "2000")], &[7u8; 2000]),
        object(200, vec![], &[7u8; 2000]), // no length header: only the stream can tell
        object(200, vec![("content-length", "10")], &[7u8; 10]),
    ]);
    let b = s3(&r);
    assert_eq!(
        b.get_object(&key, 1000, None).await,
        Err(ObjectError::TooLarge)
    );
    assert_eq!(
        b.get_object(&key, 1000, None).await,
        Err(ObjectError::TooLarge)
    );
    assert_eq!(
        b.get_object(&key, 10, None).await.unwrap().unwrap().len(),
        10,
        "exactly the limit is accepted"
    );
}

// ---- DynamoDB --------------------------------------------------------------------------------------------------------

fn agent_item(features: &str) -> String {
    format!(
        r#"{{"Item": {{"PK": {{"S": "RUN#{RUN}"}}, "SK": {{"S": "AGENT#direct"}}, "agentId": {{"S": "direct"}}, "status": {{"S": "running"}}, "version": {{"N": "3"}}{features}}}}}"#
    )
}

fn turn_item(transcript: &str) -> String {
    format!(
        r#"{{"Item": {{"PK": {{"S": "RUN#{RUN}"}}, "SK": {{"S": "TURN#direct#000001"}}, "n": {{"N": "1"}}{transcript}}}}}"#
    )
}

const FEATURES_V1: &str =
    r#", "features": {"L": [{"S": "something.else"}, {"S": "transcript.v1"}]}"#;

#[tokio::test]
async fn the_facts_are_two_consistent_gets_of_the_agent_and_turn_items_read_as_raw_attributes() {
    let summary = r#", "transcript": {"M": {"lastTranscriptSeq": {"N": "7"}, "entries": {"L": [{"M": {"kind": {"S": "excluded"}, "fromSeq": {"N": "0"}, "toSeq": {"N": "7"}}}, {"M": {"kind": {"S": "shed"}, "count": {"N": "3"}}}]}}}"#;
    let (a, t) = (agent_item(FEATURES_V1), turn_item(summary));
    let r = json_replies(vec![&a, &t]);
    let facts = ddb(&r).transcript_facts(RUN).await.unwrap();
    let want = TranscriptSummary {
        last_transcript_seq: Some(7),
        entries: vec![
            CompletenessEntry {
                kind: GapKind::Excluded,
                from_seq: Some(0),
                to_seq: Some(7),
                count: None,
            },
            CompletenessEntry {
                kind: GapKind::Shed,
                from_seq: None,
                to_seq: None,
                count: Some(3),
            },
        ],
    };
    assert_eq!(
        facts,
        TranscriptFacts {
            supported: true,
            purged: false,
            summary: SummaryFact::Valid(want)
        }
    );
    let b = bodies(&r);
    assert_eq!(b.len(), 2);
    assert_eq!(
        b[0]["Key"],
        json!({"PK": {"S": format!("RUN#{RUN}")}, "SK": {"S": "AGENT#direct"}})
    );
    assert_eq!(
        b[1]["Key"],
        json!({"PK": {"S": format!("RUN#{RUN}")}, "SK": {"S": "TURN#direct#000001"}})
    );
    assert!(
        b.iter()
            .all(|x| x["ConsistentRead"] == true && x["TableName"] == "hamh-live-harness")
    );
}

#[tokio::test]
async fn an_agent_without_the_flag_is_unsupported_and_the_turn_is_not_even_read() {
    for features in [
        "",
        r#", "features": {"L": []}"#,
        r#", "features": {"L": [{"S": "transcript.v2"}]}"#,
        r#", "features": {"S": "transcript.v1"}"#,
        r#", "features": {"NULL": true}"#,
    ] {
        let a = agent_item(features);
        let r = json_replies(vec![&a]);
        let facts = ddb(&r).transcript_facts(RUN).await.unwrap();
        assert_eq!(facts, TranscriptFacts::default(), "{features}");
        assert_eq!(
            r.actual_requests().count(),
            1,
            "{features}: only the Agent item is read"
        );
    }
    // No Agent item at all.
    let r = json_replies(vec!["{}"]);
    assert_eq!(
        ddb(&r).transcript_facts(RUN).await.unwrap(),
        TranscriptFacts::default()
    );
}

#[tokio::test]
async fn a_summary_that_is_absent_or_malformed_never_reads_as_complete() {
    let a = agent_item(FEATURES_V1);
    let absent = turn_item("");
    let null = turn_item(r#", "transcript": {"NULL": true}"#);
    let reversed = turn_item(
        r#", "transcript": {"M": {"entries": {"L": [{"M": {"kind": {"S": "shed"}, "fromSeq": {"N": "5"}, "toSeq": {"N": "4"}}}]}}}"#,
    );
    let unknown = turn_item(
        r#", "transcript": {"M": {"entries": {"L": [{"M": {"kind": {"S": "bogus"}}}]}}}"#,
    );
    let readers_word = turn_item(
        r#", "transcript": {"M": {"entries": {"L": [{"M": {"kind": {"S": "unreadable"}}}]}}}"#,
    );
    let wrong_type = turn_item(r#", "transcript": {"S": "all good"}"#);
    let many: String = (0..257)
        .map(|_| r#"{"M": {"kind": {"S": "excluded"}}}"#)
        .collect::<Vec<_>>()
        .join(",");
    let too_many = turn_item(&format!(
        r#", "transcript": {{"M": {{"entries": {{"L": [{many}]}}}}}}"#
    ));
    let no_turn = "{}".to_string();
    for (turn, want) in [
        (&absent, SummaryFact::Absent),
        (&null, SummaryFact::Absent),
        (&no_turn, SummaryFact::Absent),
        (&reversed, SummaryFact::Invalid),
        (&unknown, SummaryFact::Invalid),
        (&readers_word, SummaryFact::Invalid),
        (&wrong_type, SummaryFact::Invalid),
        (&too_many, SummaryFact::Invalid),
    ] {
        let r = json_replies(vec![&a, turn]);
        let facts = ddb(&r).transcript_facts(RUN).await.unwrap();
        assert!(facts.supported);
        assert_eq!(facts.summary, want, "{turn}");
    }
}

#[tokio::test]
async fn a_failing_read_of_the_facts_is_unavailable_not_unsupported() {
    let r = replay(vec![(
        500,
        vec![("content-type", "application/x-amz-json-1.0")],
        br#"{"__type":"com.amazonaws.dynamodb.v20120810#InternalServerError","message":"x"}"#
            .to_vec(),
    )]);
    assert!(matches!(
        ddb(&r).transcript_facts(RUN).await,
        Err(StoreError::Unavailable(_))
    ));
}

const RUN2: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X4Z";

/// A run index item: the small one the run list reads. `coverage` is only set to make an item that cannot be decoded.
fn index_item_json(run: &str, created: &str, coverage: Option<&str>) -> String {
    let extra = coverage.map_or(String::new(), |c| {
        format!(r#", "coverage": {{"S": "{c}"}}"#)
    });
    format!(
        r#"{{"PK": {{"S": "{pk}"}}, "SK": {{"S": "SRCH#{created}#{run}"}}, "runId": {{"S": "{run}"}}, "createdAt": {{"S": "{created}"}}, "promptHead": {{"S": "p"}}, "promptTruncated": {{"BOOL": false}}, "model": {{"S": "m"}}, "repository": {{"S": "r"}}, "lastActivity": {{"S": "{created}"}}{extra}}}"#,
        pk = owner_pk(OWNER)
    )
}

/// A chat item: the body of a run, kept apart so the run list never reads it.
fn chat_item_json(run: &str, created: &str, coverage: &str) -> String {
    format!(
        r#"{{"PK": {{"S": "{pk}"}}, "SK": {{"S": "CHAT#{created}#{run}"}}, "runId": {{"S": "{run}"}}, "createdAt": {{"S": "{created}"}}, "body": {{"S": "a chat about parsers\n"}}, "segs": {{"S": "[[0,3,1]]"}}, "coverage": {{"S": "{coverage}"}}, "bodyThrough": {{"N": "3"}}, "indexedAt": {{"S": "2026-10-16T11:00:00Z"}}}}"#,
        pk = owner_pk(OWNER)
    )
}

#[tokio::test]
async fn a_search_read_is_the_run_index_page_then_the_chat_bodies_of_its_window() {
    let (c1, c2) = ("2026-10-16T10:00:00Z", "2026-10-15T10:00:00Z");
    let sk1 = format!("SRCH#{c1}#{RUN}");
    let sk2 = format!("SRCH#{c2}#{RUN2}");
    // The run index page: two runs and a third item that cannot be decoded (a coverage word that is none); one more page of them follows.
    let base = format!(
        r#"{{"Items": [{}, {}, {}], "Count": 3, "LastEvaluatedKey": {{"PK": {{"S": "{}"}}, "SK": {{"S": "{sk2}"}}}}}}"#,
        index_item_json(RUN, c1, None),
        index_item_json(RUN2, c2, None),
        index_item_json(
            "01K5ZQ4T8M3N6P7R9S0V1W2X5A",
            "2026-10-14T10:00:00Z",
            Some("bogus")
        ),
        owner_pk(OWNER)
    );
    // The chat bodies of the window come in two pages: the body of the first run, then the second's, and one of a run that is not on the page.
    let lek = format!(
        r#""LastEvaluatedKey": {{"PK": {{"S": "{}"}}, "SK": {{"S": "CHAT#{c1}#{RUN}"}}}}"#,
        owner_pk(OWNER)
    );
    let page1 = format!(
        r#"{{"Items": [{}], "Count": 1, {lek}}}"#,
        chat_item_json(RUN, c1, "full")
    );
    let page2 = format!(
        r#"{{"Items": [{}, {}], "Count": 2}}"#,
        chat_item_json("01K5ZQ4T8M3N6P7R9S0V1W2X6B", "2026-10-15T20:00:00Z", "full"),
        chat_item_json(RUN2, c2, "partial")
    );
    let r = json_replies(vec![&base, &page1, &page2]);
    let store = ddb(&r);
    let floor = sk_floor("2026-09-01");
    let out = store
        .search_items(&owner_pk(OWNER), &floor, Some(&sk1), 25)
        .await
        .unwrap();
    assert_eq!(
        out.items.len(),
        2,
        "the item that cannot be decoded is left out"
    );
    let first = &out.items[0];
    assert_eq!(
        (first.run_id.as_str(), first.sk.as_str(), first.coverage),
        (RUN, sk1.as_str(), Some(Coverage::Full))
    );
    assert_eq!(first.body.as_deref(), Some("a chat about parsers\n"));
    assert_eq!(
        (
            first.body_through,
            first.prompt_head.as_deref(),
            first.model.as_deref(),
            first.repository.as_deref()
        ),
        (Some(3), Some("p"), Some("m"), Some("r"))
    );
    let second = &out.items[1];
    assert_eq!(
        (second.run_id.as_str(), second.sk.as_str(), second.coverage),
        (RUN2, sk2.as_str(), Some(Coverage::Partial))
    );
    assert_eq!(second.body.as_deref(), Some("a chat about parsers\n"));
    assert_eq!(out.next_sk.as_deref(), Some(sk2.as_str()));
    // An item that cannot be decoded still counts as examined, and its key is where a search that goes on goes on after.
    assert_eq!(
        (out.examined, out.last_sk.as_deref()),
        (
            3,
            Some("SRCH#2026-10-14T10:00:00Z#01K5ZQ4T8M3N6P7R9S0V1W2X5A")
        )
    );
    let b = bodies(&r);
    assert_eq!(
        b.len(),
        3,
        "one Query for the run index page and one per page of bodies"
    );
    // 1: the run index page, newest first, after the key, in the run index's own range.
    assert_eq!(b[0]["TableName"], "hamh-live-harness");
    assert_eq!(
        b[0]["KeyConditionExpression"],
        "PK = :pk AND SK BETWEEN :lo AND :hi"
    );
    assert_eq!(
        b[0]["ExpressionAttributeValues"][":pk"],
        json!({"S": owner_pk(OWNER)})
    );
    assert_eq!(
        b[0]["ExpressionAttributeValues"][":lo"],
        json!({"S": floor})
    );
    assert_eq!(
        b[0]["ExpressionAttributeValues"][":hi"],
        json!({"S": "SRCH#~"})
    );
    assert_eq!(b[0]["ScanIndexForward"], false);
    assert_eq!(b[0]["Limit"], 25);
    assert_eq!(
        b[0]["ExclusiveStartKey"],
        json!({"PK": {"S": owner_pk(OWNER)}, "SK": {"S": sk1}})
    );
    // 2 and 3: the bodies of the window the decoded items span (oldest to newest creation time), page by page, in the chat family.
    for body in &b[1..] {
        assert_eq!(
            body["KeyConditionExpression"],
            "PK = :pk AND SK BETWEEN :lo AND :hi"
        );
        assert_eq!(
            body["ExpressionAttributeValues"][":pk"],
            json!({"S": owner_pk(OWNER)})
        );
        assert_eq!(
            body["ExpressionAttributeValues"][":lo"],
            json!({"S": format!("CHAT#{c2}")})
        );
        assert_eq!(
            body["ExpressionAttributeValues"][":hi"],
            json!({"S": format!("CHAT#{c1}#~")})
        );
        assert!(
            body.get("Limit").is_none()
                && body.get("FilterExpression").is_none()
                && body.get("IndexName").is_none()
        );
        assert!(
            body.get("ConsistentRead").is_none() || body["ConsistentRead"] == false,
            "an index read is eventually consistent"
        );
    }
    assert!(b[1].get("ExclusiveStartKey").is_none());
    assert_eq!(
        b[2]["ExclusiveStartKey"],
        json!({"PK": {"S": owner_pk(OWNER)}, "SK": {"S": format!("CHAT#{c1}#{RUN}")}})
    );
}

#[tokio::test]
async fn a_run_without_a_chat_item_has_no_body_and_a_page_with_nothing_readable_reads_no_bodies() {
    let c = "2026-10-16T10:00:00Z";
    let base = format!(
        r#"{{"Items": [{}], "Count": 1}}"#,
        index_item_json(RUN, c, None)
    );
    let none = r#"{"Items": [], "Count": 0}"#;
    let r = json_replies(vec![&base, none, none, r#"{"Items": [], "Count": 0}"#]);
    let store = ddb(&r);
    let out = store
        .search_items(&owner_pk(OWNER), &sk_floor("2026-09-01"), None, 25)
        .await
        .unwrap();
    assert_eq!(
        (
            out.items.len(),
            out.items[0].body.clone(),
            out.items[0].coverage,
            out.next_sk.clone()
        ),
        (1, None, None, None)
    );
    assert_eq!(
        out.items[0].prompt_head.as_deref(),
        Some("p"),
        "the run is still searched on what the run index holds"
    );
    assert_eq!(bodies(&r).len(), 2);
    // Nothing decodable on the page: no window, so no body Query.
    let bad = format!(
        r#"{{"Items": [{}], "Count": 1}}"#,
        index_item_json(RUN, c, Some("bogus"))
    );
    let r = json_replies(vec![&bad]);
    let out = ddb(&r)
        .search_items(&owner_pk(OWNER), &sk_floor("2026-09-01"), None, 25)
        .await
        .unwrap();
    assert_eq!((out.items.len(), out.examined), (0, 1));
    assert_eq!(bodies(&r).len(), 1);
    // An empty page: one Query.
    let r = json_replies(vec![none]);
    let out = ddb(&r)
        .search_items(&owner_pk(OWNER), &sk_floor("2026-09-01"), None, 25)
        .await
        .unwrap();
    assert_eq!(
        (out.items.len(), out.examined, out.last_sk, out.next_sk),
        (0, 0, None, None)
    );
    assert_eq!(bodies(&r).len(), 1);
}

#[tokio::test]
async fn a_page_of_bodies_that_never_ends_is_a_failure_not_an_endless_read() {
    let c = "2026-10-16T10:00:00Z";
    let base = format!(
        r#"{{"Items": [{}], "Count": 1}}"#,
        index_item_json(RUN, c, None)
    );
    let lek = format!(
        r#""LastEvaluatedKey": {{"PK": {{"S": "{}"}}, "SK": {{"S": "CHAT#{c}#{RUN}"}}}}"#,
        owner_pk(OWNER)
    );
    let endless = format!(r#"{{"Items": [], "Count": 0, {lek}}}"#);
    let mut replies: Vec<&str> = vec![&base];
    replies.extend(std::iter::repeat_n(
        endless.as_str(),
        control_api::aws::SEARCH_BODY_MAX_PAGES + 3,
    ));
    let r = json_replies(replies);
    let err = ddb(&r)
        .search_items(&owner_pk(OWNER), &sk_floor("2026-09-01"), None, 25)
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Unavailable(_)), "{err:?}");
    assert_eq!(
        bodies(&r).len(),
        1 + control_api::aws::SEARCH_BODY_MAX_PAGES,
        "the read stops at its page cap"
    );
}

#[tokio::test]
async fn the_query_limit_is_clamped_to_what_dynamodb_accepts() {
    let none = r#"{"Items": [], "Count": 0}"#;
    let r = json_replies(vec![none, none]);
    let store = ddb(&r);
    for n in [0usize, 100_000] {
        store
            .search_items(&owner_pk(OWNER), "SRCH#2026", None, n)
            .await
            .unwrap();
    }
    assert_eq!(
        bodies(&r)
            .iter()
            .map(|b| b["Limit"].as_i64().unwrap())
            .collect::<Vec<_>>(),
        vec![1, 1000]
    );
}
