//! The DynamoDB, S3 and Lambda adapters against recorded HTTP exchanges (aws-smithy StaticReplayClient): the exact requests they send and
//! how they read the answers. No network.

#![allow(clippy::unwrap_used)]

use aws_smithy_http_client::test_util::{ReplayEvent, StaticReplayClient};
use aws_smithy_types::body::SdkBody;
use harness_protocol::research::{ResearchErrorCode, ResearchLimits};
use research_api::aws::{DynamoTable, LambdaInvoker, S3Blobs, reserve_plan};
use research_api::ports::{Blobs, Delta, Invoker, JobEvent, PutOutcome, Table};
use serde_json::{Value, json};

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

fn bodies(client: &StaticReplayClient) -> Vec<Value> {
    client
        .actual_requests()
        .map(|r| {
            serde_json::from_slice(r.body().bytes().unwrap_or_default()).unwrap_or(Value::Null)
        })
        .collect()
}

fn ddb(client: &StaticReplayClient) -> DynamoTable {
    let conf = aws_sdk_dynamodb::Config::builder()
        .behavior_version_latest()
        .region(aws_sdk_dynamodb::config::Region::new("us-east-1"))
        .credentials_provider(aws_sdk_dynamodb::config::Credentials::for_tests())
        .http_client(client.clone())
        .build();
    DynamoTable {
        client: aws_sdk_dynamodb::Client::from_conf(conf),
        table: "hamh-direct-research".into(),
    }
}

const RUN: &str = "01M4XAB0CDEFGHJKMNPQRSTVWX";
const CONDITIONAL: &str = r#"{"__type": "com.amazonaws.dynamodb.v20120810#ConditionalCheckFailedException", "message": "The conditional request failed"}"#;

#[test]
fn the_reserve_expressions_name_only_the_parts_that_are_added() {
    let limits = ResearchLimits::default();
    let plan = reserve_plan(
        &Delta {
            searches: 1,
            outbound_bytes: 25,
            ..Delta::default()
        },
        &limits,
    )
    .unwrap();
    assert_eq!(
        plan.update,
        "ADD searches :add_searches, outboundBytes :add_outboundBytes SET #ttl = if_not_exists(#ttl, :ttl)"
    );
    assert_eq!(
        plan.condition,
        "(attribute_not_exists(searches) OR searches <= :max_searches) AND (attribute_not_exists(outboundBytes) OR outboundBytes <= :max_outboundBytes)"
    );
    assert!(plan.values.contains(&(":max_searches".into(), 199)));
    assert!(
        plan.values
            .contains(&(":max_outboundBytes".into(), 65_536 - 25))
    );
    // A part larger than its cap can never fit, and is refused before a request is made.
    let e = reserve_plan(
        &Delta {
            outbound_bytes: 65_537,
            ..Delta::default()
        },
        &limits,
    )
    .unwrap_err();
    assert_eq!(e.code, ResearchErrorCode::LimitBytes);
    assert!(reserve_plan(&Delta::default(), &limits).is_err());
}

#[tokio::test]
async fn reserve_sends_one_conditional_update_and_reads_the_usage_after() {
    let client = replay(vec![(
        200,
        r#"{"Attributes": {"searches": {"N": "3"}, "outboundBytes": {"N": "75"}, "fetchedUrls": {"N": "1"}}}"#,
    )]);
    let usage = ddb(&client)
        .reserve(
            RUN,
            &Delta {
                searches: 1,
                outbound_bytes: 25,
                ..Delta::default()
            },
            &ResearchLimits::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        (usage.searches, usage.outbound_bytes, usage.fetched_urls),
        (3, 75, 1)
    );
    let b = &bodies(&client)[0];
    assert_eq!(b["TableName"], "hamh-direct-research");
    assert_eq!(
        b["Key"],
        json!({"PK": {"S": format!("run#{RUN}")}, "SK": {"S": "ctr"}})
    );
    assert_eq!(b["ReturnValues"], "ALL_NEW");
    assert!(
        b["UpdateExpression"]
            .as_str()
            .unwrap()
            .starts_with("ADD searches")
    );
    assert_eq!(
        b["ExpressionAttributeValues"][":max_searches"],
        json!({"N": "199"})
    );
    assert_eq!(b["ExpressionAttributeNames"], json!({"#ttl": "ttl"}));
}

#[tokio::test]
async fn a_cap_that_holds_is_found_by_reading_the_counters() {
    let client = replay(vec![
        (400, CONDITIONAL),
        (
            200,
            r#"{"Item": {"PK": {"S": "x"}, "SK": {"S": "ctr"}, "searches": {"N": "200"}, "outboundBytes": {"N": "10"}}}"#,
        ),
    ]);
    let e = ddb(&client)
        .reserve(
            RUN,
            &Delta {
                searches: 1,
                outbound_bytes: 5,
                ..Delta::default()
            },
            &ResearchLimits::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(e.code, ResearchErrorCode::LimitCalls);
    assert!(e.message.contains("search count"), "{}", e.message);
    // The second request is the consistent read of the counters.
    assert_eq!(bodies(&client)[1]["ConsistentRead"], true);

    let client = replay(vec![
        (400, CONDITIONAL),
        (
            200,
            r#"{"Item": {"searches": {"N": "1"}, "outboundBytes": {"N": "65535"}}}"#,
        ),
    ]);
    let e = ddb(&client)
        .reserve(
            RUN,
            &Delta {
                searches: 1,
                outbound_bytes: 5,
                ..Delta::default()
            },
            &ResearchLimits::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(e.code, ResearchErrorCode::LimitBytes);
}

#[tokio::test]
async fn a_rate_window_is_a_conditional_count_and_a_full_window_is_limit_rate() {
    let client = replay(vec![(200, "{}"), (400, CONDITIONAL)]);
    let t = ddb(&client);
    t.rate_tick(RUN, 29_869_380, 30).await.unwrap();
    let e = t.rate_tick(RUN, 29_869_380, 30).await.unwrap_err();
    assert_eq!(e.code, ResearchErrorCode::LimitRate);
    let b = &bodies(&client)[0];
    assert_eq!(b["Key"]["SK"], json!({"S": "rate#29869380"}));
    assert_eq!(
        b["ConditionExpression"],
        "attribute_not_exists(calls) OR calls < :cap"
    );
    assert_eq!(b["ExpressionAttributeValues"][":cap"], json!({"N": "30"}));
    // The stage-wide window has its own partition.
    let client = replay(vec![(200, "{}")]);
    ddb(&client).stage_tick(7, 80).await.unwrap();
    assert_eq!(
        bodies(&client)[0]["Key"],
        json!({"PK": {"S": "stage"}, "SK": {"S": "lim#7"}})
    );
}

#[tokio::test]
async fn a_job_moves_only_from_the_states_it_may_leave() {
    use harness_protocol::research::JobState;
    let client = replay(vec![(200, "{}"), (400, CONDITIONAL)]);
    let t = ddb(&client);
    assert!(
        t.job_move(
            RUN,
            "j-1",
            &[JobState::Queued],
            JobState::Running,
            1_792_162_800,
            None,
            None
        )
        .await
        .unwrap()
    );
    assert!(
        !t.job_move(
            RUN,
            "j-1",
            &[JobState::Queued],
            JobState::Running,
            1_792_162_800,
            None,
            None
        )
        .await
        .unwrap()
    );
    let b = &bodies(&client)[0];
    assert_eq!(b["ConditionExpression"], "#state IN (:from0)");
    assert_eq!(
        b["ExpressionAttributeValues"][":from0"],
        json!({"S": "queued"})
    );
    assert_eq!(
        b["ExpressionAttributeValues"][":to"],
        json!({"S": "running"})
    );
    // `state`, `item` and `error` are DynamoDB reserved words: they only appear through aliases.
    assert_eq!(
        b["UpdateExpression"],
        "SET #state = :to, updatedS = :now REMOVE #item, #error"
    );
    assert_eq!(
        b["ExpressionAttributeNames"],
        json!({"#state": "state", "#item": "item", "#error": "error"})
    );
}

#[tokio::test]
async fn a_job_is_created_once() {
    use harness_protocol::research::JobState;
    use research_api::ports::JobRecord;
    let job = JobRecord {
        run: RUN.into(),
        agent: "direct".into(),
        job_id: "j-1".into(),
        req_hash: "00".repeat(32),
        state: JobState::Queued,
        created_s: 1_792_162_800,
        updated_s: 1_792_162_800,
        url: "https://github.com/a/b".into(),
        git_ref: None,
        paths: vec![],
        item: None,
        error: None,
    };
    let client = replay(vec![(200, "{}"), (400, CONDITIONAL)]);
    let t = ddb(&client);
    assert!(t.job_create(&job).await.unwrap());
    assert!(!t.job_create(&job).await.unwrap());
    let b = &bodies(&client)[0];
    assert_eq!(b["ConditionExpression"], "attribute_not_exists(PK)");
    assert_eq!(b["Item"]["SK"], json!({"S": "job#j-1"}));
    assert_eq!(b["Item"]["state"], json!({"S": "queued"}));
}

fn s3(client: &StaticReplayClient) -> S3Blobs {
    let conf = aws_sdk_s3::Config::builder()
        .behavior_version_latest()
        .region(aws_sdk_s3::config::Region::new("us-east-1"))
        .credentials_provider(aws_sdk_s3::config::Credentials::for_tests())
        .http_client(client.clone())
        .build();
    S3Blobs {
        client: aws_sdk_s3::Client::from_conf(conf),
        bucket: "hamh-direct-harness-111122223333".into(),
    }
}

#[tokio::test]
async fn an_object_is_written_create_only_and_an_existing_key_is_not_replaced() {
    let client = replay(vec![
        (200, ""),
        (
            412,
            r#"<Error><Code>PreconditionFailed</Code><Message>At least one of the pre-conditions you specified did not hold</Message></Error>"#,
        ),
    ]);
    let b = s3(&client);
    assert_eq!(
        b.put_new(
            "runs/r/agents/direct/research/audit/x.json",
            b"{}".to_vec(),
            "application/json"
        )
        .await
        .unwrap(),
        PutOutcome::Created
    );
    assert_eq!(
        b.put_new(
            "runs/r/agents/direct/research/audit/x.json",
            b"{}".to_vec(),
            "application/json"
        )
        .await
        .unwrap(),
        PutOutcome::Exists
    );
    for req in client.actual_requests() {
        assert_eq!(req.headers().get("if-none-match").unwrap(), "*");
    }
}

#[tokio::test]
async fn a_missing_object_reads_as_none() {
    let client = replay(vec![(
        404,
        r#"<Error><Code>NoSuchKey</Code><Message>missing</Message></Error>"#,
    )]);
    assert_eq!(s3(&client).get("runs/r/x").await.unwrap(), None);
}

#[tokio::test]
async fn a_job_start_is_an_asynchronous_invoke_and_anything_but_202_fails() {
    let conf = |client: &StaticReplayClient| {
        aws_sdk_lambda::Config::builder()
            .behavior_version_latest()
            .region(aws_sdk_lambda::config::Region::new("us-east-1"))
            .credentials_provider(aws_sdk_lambda::config::Credentials::for_tests())
            .http_client(client.clone())
            .build()
    };
    let event = JobEvent {
        run: RUN.into(),
        agent: "direct".into(),
        job_id: "j-1".into(),
    };
    let client = replay(vec![(202, "")]);
    let inv = LambdaInvoker {
        client: aws_sdk_lambda::Client::from_conf(conf(&client)),
        function: "hamh-direct-research-job".into(),
    };
    inv.start_job(&event).await.unwrap();
    let req = client.actual_requests().next().unwrap();
    assert_eq!(req.headers().get("x-amz-invocation-type").unwrap(), "Event");
    assert!(req.uri().contains("hamh-direct-research-job"));
    assert_eq!(
        serde_json::from_slice::<Value>(req.body().bytes().unwrap()).unwrap(),
        json!({"run": RUN, "agent": "direct", "jobId": "j-1"})
    );

    let client = replay(vec![(500, r#"{"Message": "boom"}"#)]);
    let inv = LambdaInvoker {
        client: aws_sdk_lambda::Client::from_conf(conf(&client)),
        function: "f".into(),
    };
    assert!(inv.start_job(&event).await.is_err());

    // A 200 is the answer to a synchronous call, not the 202 of an accepted asynchronous start.
    let client = replay(vec![(200, "")]);
    let inv = LambdaInvoker {
        client: aws_sdk_lambda::Client::from_conf(conf(&client)),
        function: "f".into(),
    };
    assert!(inv.start_job(&event).await.is_err());
}

/// Names the adapter uses in an expression with no alias; each must be a name DynamoDB does not reserve. A new bare name fails this test
/// until someone checks it against the reserved-word list (https://docs.aws.amazon.com/amazondynamodb/latest/developerguide/ReservedWords.html).
const BARE_NAMES_CHECKED: [&str; 10] = [
    "PK",
    "searches",
    "fetchedUrls",
    "repos",
    "outboundBytes",
    "contextBytes",
    "itemSeq",
    "searchSeq",
    "calls",
    "updatedS",
];
/// Words DynamoDB reserves that this code would plausibly reach for.
const RESERVED_WE_USE_BY_ALIAS: [&str; 6] = ["state", "item", "error", "ttl", "name", "status"];

fn bare_names(expr: &str) -> Vec<String> {
    let keywords = [
        "ADD",
        "SET",
        "REMOVE",
        "AND",
        "OR",
        "IN",
        "attribute_not_exists",
        "attribute_exists",
        "if_not_exists",
    ];
    expr.split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '#' | ':')))
        .filter(|t| {
            !t.is_empty() && !t.starts_with('#') && !t.starts_with(':') && !keywords.contains(t)
        })
        .filter(|t| !t.chars().all(|c| c.is_ascii_digit()))
        .map(str::to_string)
        .collect()
}

#[tokio::test]
async fn no_expression_the_adapter_sends_names_an_attribute_that_dynamodb_reserves() {
    use harness_protocol::research::{ItemRef, JobState, ResearchError};
    let client = replay(
        (0..8)
            .map(|_| (200, "{\"Attributes\": {\"itemSeq\": {\"N\": \"1\"}}}"))
            .collect(),
    );
    let t = ddb(&client);
    let limits = ResearchLimits::default();
    let delta = Delta {
        searches: 1,
        fetched_urls: 1,
        repos: 1,
        outbound_bytes: 1,
        context_bytes: 1,
    };
    t.reserve(RUN, &delta, &limits).await.unwrap();
    t.rate_tick(RUN, 1, 30).await.unwrap();
    t.stage_tick(1, 80).await.unwrap();
    t.next_id(RUN, research_api::ports::IdKind::Item)
        .await
        .unwrap();
    let item = ItemRef {
        id: "i-0001".into(),
        kind: harness_protocol::research::ItemKind::Repo,
        origin: "https://github.com/a/b".into(),
        manifest_key: "k".into(),
        manifest_sha256: "00".repeat(32),
        body_key: "k".into(),
        body_sha256: "00".repeat(32),
        bytes: 1,
        licence: None,
        truncated: false,
        files: None,
        dropped: None,
    };
    let err = ResearchError::new(research_api::ports::Fail::internal("x").code, "x");
    t.job_move(
        RUN,
        "j-1",
        &[JobState::Queued],
        JobState::Succeeded,
        1,
        Some(&item),
        None,
    )
    .await
    .unwrap();
    t.job_move(
        RUN,
        "j-1",
        &[JobState::Running],
        JobState::Failed,
        1,
        None,
        Some(&err),
    )
    .await
    .unwrap();
    let mut checked = 0;
    for b in bodies(&client) {
        for key in ["UpdateExpression", "ConditionExpression"] {
            if let Some(expr) = b[key].as_str() {
                for name in bare_names(expr) {
                    assert!(
                        BARE_NAMES_CHECKED.contains(&name.as_str()),
                        "{key} {expr:?} uses the bare name {name}"
                    );
                    assert!(
                        !RESERVED_WE_USE_BY_ALIAS.contains(&name.as_str()),
                        "{name} is reserved: alias it"
                    );
                    checked += 1;
                }
            }
        }
    }
    assert!(checked > 10, "the test looked at {checked} names");
}

#[tokio::test]
async fn seen_addresses_are_written_once_each_in_one_batch() {
    let client = replay(vec![(200, "{}")]);
    let hashes: Vec<String> = ["a", "b", "a", "c", "b"]
        .iter()
        .map(|h| h.repeat(64))
        .collect();
    ddb(&client)
        .seen_put(RUN, &hashes, 1_792_200_000)
        .await
        .unwrap();
    let b = &bodies(&client)[0];
    let writes = b["RequestItems"]["hamh-direct-research"]
        .as_array()
        .unwrap();
    assert_eq!(
        writes.len(),
        3,
        "BatchWriteItem refuses two writes to one key"
    );
}

#[tokio::test]
async fn a_write_in_flight_is_tried_again_and_an_existing_object_is_not_taken_for_one() {
    let conflict = r#"<Error><Code>ConditionalRequestConflict</Code><Message>A conflicting operation is in progress</Message></Error>"#;
    let client = replay(vec![(409, conflict), (200, "")]);
    assert_eq!(
        s3(&client)
            .put_new(
                "runs/r/agents/direct/research/x.json",
                b"{}".to_vec(),
                "application/json"
            )
            .await
            .unwrap(),
        PutOutcome::Created
    );
    assert_eq!(client.actual_requests().count(), 2);
    let client = replay(vec![
        (409, conflict),
        (409, conflict),
        (409, conflict),
        (409, conflict),
    ]);
    let e = s3(&client)
        .put_new(
            "runs/r/agents/direct/research/x.json",
            b"{}".to_vec(),
            "application/json",
        )
        .await
        .unwrap_err();
    assert_eq!(e.code, ResearchErrorCode::Internal);
    assert_eq!(client.actual_requests().count(), 4);
}

#[tokio::test]
async fn keys_a_busy_table_left_unprocessed_are_asked_again_and_every_answer_is_kept() {
    let a = "a".repeat(64);
    let b = "b".repeat(64);
    let first = format!(
        r#"{{"Responses": {{"hamh-direct-research": [{{"SK": {{"S": "seen#{a}"}}}}]}}, "UnprocessedKeys": {{"hamh-direct-research": {{"Keys": [{{"PK": {{"S": "run#{RUN}"}}, "SK": {{"S": "seen#{b}"}}}}], "ConsistentRead": true}}}}}}"#
    );
    let second =
        format!(r#"{{"Responses": {{"hamh-direct-research": [{{"SK": {{"S": "seen#{b}"}}}}]}}}}"#);
    let client = replay(vec![(200, first.as_str()), (200, second.as_str())]);
    let found = ddb(&client)
        .seen_get(RUN, &[a.clone(), b.clone(), a.clone()])
        .await
        .unwrap();
    assert_eq!(found, vec![a, b]);
    let sent = bodies(&client);
    assert_eq!(sent.len(), 2);
    assert_eq!(
        sent[0]["RequestItems"]["hamh-direct-research"]["Keys"]
            .as_array()
            .unwrap()
            .len(),
        2,
        "duplicates are asked once"
    );
    assert_eq!(
        sent[1]["RequestItems"]["hamh-direct-research"]["Keys"]
            .as_array()
            .unwrap()
            .len(),
        1,
        "only the unprocessed key is asked again"
    );
}
