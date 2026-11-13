//! The DynamoDB calls behind the run tags against recorded HTTP exchanges (StaticReplayClient). The two writers send the specs of
//! `ham_core::runtag::dynamo` as they are; the vocabulary read is one narrow `Query` of the owner's partition whose projection names the
//! three tag lists only and whose items are decoded by a type of their own, never as a run-index item; the state reads are consistent and
//! project two or three attributes. A failed condition is `false`, any other failure is an error, and every read tolerates the wrong type.
#![allow(clippy::unwrap_used)]

use aws_smithy_http_client::test_util::{ReplayEvent, StaticReplayClient};
use aws_smithy_types::body::SdkBody;
use ham_core::run_index::{owner_pk, search_sk};
use ham_core::runtag::dynamo::{self, Answered, Av, UpdateSpec};
use ham_core::runtag::edit::TagState;
use ham_core::runtag::vocab::VocabItem;
use harness_protocol::store::RunItem;
use run_session::aws::DynamoStore;
use run_session::ports::{Store, TagMarks};
use serde_json::{Map, Value, json};

const RUN: &str = "01M4X3Q7R9S0V1W2X3Y4Z5A6B7";
const PARENT: &str = "01M4X3Q7R9S0V1W2X3Y4Z5A6B6";
const OWNER: &str = "arn:aws:sts::111122223333:assumed-role/Admin/tester";
const CREATED: &str = "2026-10-13T18:00:00Z";
const PARENT_CREATED: &str = "2026-10-13T17:00:00Z";
const ROUTE: &str = "us.anthropic.claude-haiku-5-5";
const CCF: &str = r#"{"__type":"com.amazonaws.dynamodb.v20120810#ConditionalCheckFailedException","message":"The conditional request failed"}"#;
const DDB_500: &str =
    r#"{"__type":"com.amazonaws.dynamodb.v20120810#InternalServerError","message":"boom"}"#;

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

fn ok(body: Value) -> (u16, String) {
    (200, body.to_string())
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
        "target": {"mode": "s3-only"}, "status": "active", "phase": "planning",
        "enginePolicyVersion": 0, "createdAt": CREATED, "version": 1, "continueFrom": PARENT
    }))
    .unwrap()
}

fn plain_run() -> RunItem {
    let mut r = run();
    r.continue_from = None;
    r
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_string()).collect()
}

fn key() -> Value {
    json!({"PK": {"S": owner_pk(OWNER)}, "SK": {"S": search_sk(CREATED, RUN)}})
}

/// A value of the specs as DynamoDB writes it on the wire.
fn av_json(v: &Av) -> Value {
    match v {
        Av::S(s) => json!({"S": s}),
        Av::N(n) => json!({"N": n.to_string()}),
        Av::L(l) => json!({"L": l.iter().map(|s| json!({"S": s})).collect::<Vec<_>>()}),
        Av::Ss(s) => json!({"SS": s}),
    }
}

fn values_json(spec: &UpdateSpec) -> Value {
    Value::Object(
        spec.values
            .iter()
            .map(|(k, v)| (k.clone(), av_json(v)))
            .collect::<Map<_, _>>(),
    )
}

/// The `UpdateItem` the wire carried, held to the spec word for word.
fn assert_is_spec(b: &Value, spec: &UpdateSpec) {
    assert_eq!(b["TableName"], "hamh-direct-harness");
    assert_eq!(
        b["Key"],
        key(),
        "the owner's partition and the item control-api writes at the start"
    );
    assert_eq!(b["UpdateExpression"], spec.expression.as_str());
    assert_eq!(b["ConditionExpression"], spec.condition.clone().unwrap());
    assert_eq!(b["ExpressionAttributeNames"], json!(spec.names));
    assert_eq!(b["ExpressionAttributeValues"], values_json(spec));
    // Only the tag attributes and the two key fields of a new item are named: never the head, the outcome, the time or the name.
    for name in b["ExpressionAttributeNames"].as_object().unwrap().values() {
        let name = name.as_str().unwrap();
        assert!(
            name.starts_with("runTags") || name == "runId" || name == "createdAt",
            "{name} is named: {b}"
        );
    }
}

#[tokio::test]
async fn writer_expressions_are_the_runtag_specs() {
    let r = replay(vec![
        ok(json!({})),
        (400, CCF.into()),
        (500, DDB_500.into()),
        ok(json!({})),
        (400, CCF.into()),
    ]);
    let s = ddb(&r);
    let tags = strings(&["topic:retry-logic", "tech:aws-s3"]);
    let at = "2026-10-13T18:00:09Z";
    let answered = Answered {
        route: ROUTE,
        version: "rt-1",
    };

    // The automatic list: stored, then the condition fails (the marker is there: kept), then a service fault.
    assert!(
        s.put_run_tags_auto(&plain_run(), &tags, at, Some(answered))
            .await
            .unwrap()
    );
    assert!(
        !s.put_run_tags_auto(&plain_run(), &tags, at, Some(answered))
            .await
            .unwrap(),
        "the condition failed: another invocation stored its list"
    );
    let err = s
        .put_run_tags_auto(&plain_run(), &tags, at, Some(answered))
        .await
        .unwrap_err();
    assert_eq!(err.error_type(), "RunSessionUnavailable");

    // The copy of a follow-up: stored, then lost to an edit or an earlier copy.
    let user = strings(&["project:billing-export"]);
    let off = strings(&["tech:csv"]);
    assert!(
        s.put_run_tags_inherit(&run(), PARENT, &user, &off)
            .await
            .unwrap()
    );
    assert!(
        !s.put_run_tags_inherit(&run(), PARENT, &user, &off)
            .await
            .unwrap()
    );
    // Nothing to copy: no call.
    assert!(
        !s.put_run_tags_inherit(&run(), PARENT, &[], &[])
            .await
            .unwrap()
    );

    let all = bodies(&r);
    assert_eq!(
        all.len(),
        5,
        "five calls were made, the empty copy made none"
    );
    let auto = dynamo::auto_update(RUN, CREATED, &tags, at, Some(answered));
    assert_eq!(dynamo::expr_rejects(&auto), None);
    for sent in &all[0..3] {
        assert_is_spec(sent, &auto);
    }
    assert!(
        auto.values
            .values()
            .any(|v| matches!(v, Av::L(l) if *l == tags)),
        "the list is a list of strings, not a set"
    );
    let inherit = dynamo::inherit_update(RUN, CREATED, PARENT, &user, &off).unwrap();
    assert_eq!(dynamo::expr_rejects(&inherit), None);
    for sent in &all[3..5] {
        assert_is_spec(sent, &inherit);
    }
    // The revision is incremented in the store, not computed from a read.
    assert!(
        all[0]["UpdateExpression"]
            .as_str()
            .unwrap()
            .contains("if_not_exists(#runTagsRev, :zero) + :one"),
        "{}",
        all[0]
    );
}

#[tokio::test]
async fn an_automatic_write_without_a_model_answer_names_neither_the_route_nor_the_version() {
    let r = replay(vec![ok(json!({}))]);
    let s = ddb(&r);
    let tags = strings(&["topic:backfill"]);
    s.put_run_tags_auto(&plain_run(), &tags, "2026-10-13T18:00:09Z", None)
        .await
        .unwrap();
    let b = &bodies(&r)[0];
    let spec = dynamo::auto_update(RUN, CREATED, &tags, "2026-10-13T18:00:09Z", None);
    assert_is_spec(b, &spec);
    assert!(
        !b["UpdateExpression"]
            .as_str()
            .unwrap()
            .contains("runTagsModel")
    );
    assert!(
        !b["UpdateExpression"]
            .as_str()
            .unwrap()
            .contains("runTagsVer")
    );
    // An empty list leaves the list attribute out and still sets the marker.
    let r = replay(vec![ok(json!({}))]);
    ddb(&r)
        .put_run_tags_auto(&plain_run(), &[], "2026-10-13T18:00:09Z", None)
        .await
        .unwrap();
    let b = &bodies(&r)[0];
    assert!(
        !b["UpdateExpression"]
            .as_str()
            .unwrap()
            .contains("#runTags =")
    );
    assert!(
        b["UpdateExpression"]
            .as_str()
            .unwrap()
            .contains("#runTagsAt = :at")
    );
}

#[tokio::test]
async fn the_vocabulary_is_one_narrow_query_of_the_owners_partition() {
    let r = replay(vec![ok(
        json!({"Items": [], "Count": 0, "ScannedCount": 0}),
    )]);
    let read = ddb(&r).tag_vocabulary(&run(), 200).await.unwrap();
    assert!(read.items.is_empty() && read.skipped == 0);
    let all = bodies(&r);
    assert_eq!(all.len(), 1, "one call, one page");
    let b = &all[0];
    let spec = dynamo::vocabulary_query(&owner_pk(OWNER));
    assert_eq!(b["TableName"], "hamh-direct-harness");
    assert_eq!(b["KeyConditionExpression"], spec.key_condition.as_str());
    assert_eq!(b["ExpressionAttributeNames"], json!(spec.names));
    assert_eq!(
        b["ExpressionAttributeValues"],
        json!({":pk": {"S": owner_pk(OWNER)}, ":sk": {"S": "SRCH#"}}),
        "the partition comes from the stored run, begins_with SRCH#"
    );
    assert_eq!(
        b["ProjectionExpression"],
        "#runTags, #runTagsUser, #runTagsOff"
    );
    assert_eq!(b["Select"], "SPECIFIC_ATTRIBUTES");
    assert_eq!(b["ConsistentRead"], false);
    assert_eq!(b["ScanIndexForward"], false, "newest first");
    assert_eq!(b["Limit"], 200);
    assert!(
        b.get("FilterExpression").is_none(),
        "no filter expression: {b}"
    );
    assert!(b.get("IndexName").is_none(), "{b}");
    assert_eq!(dynamo::expr_rejects(&spec), None);
    // The text of the request holds no other attribute of the search item.
    for other in [
        "promptHead",
        "outcome",
        "lastActivity",
        "runName",
        "repository",
        "title",
        "ownerPrincipalArn",
    ] {
        assert!(!b.to_string().contains(other), "{other} in {b}");
    }

    // A larger limit than the window is cut to the window; a smaller one is kept.
    let r = replay(vec![ok(json!({"Items": []})), ok(json!({"Items": []}))]);
    let s = ddb(&r);
    s.tag_vocabulary(&run(), 5000).await.unwrap();
    s.tag_vocabulary(&run(), 50).await.unwrap();
    let all = bodies(&r);
    assert_eq!(all[0]["Limit"], 200);
    assert_eq!(all[1]["Limit"], 50);

    // A service fault is an error (the branch goes on without a vocabulary).
    let r = replay(vec![(500, DDB_500.into())]);
    let err = ddb(&r).tag_vocabulary(&run(), 200).await.unwrap_err();
    assert_eq!(err.error_type(), "RunSessionUnavailable");
}

#[tokio::test]
async fn vocabulary_decoder_reads_only_the_projected_attributes() {
    let items = json!([
        // Only the tag attributes: no PK, no SK, no runId, no createdAt. A run-index item would not decode.
        {"runTags": {"L": [{"S": "topic:a"}, {"S": "tech:b"}]}, "runTagsUser": {"L": [{"S": "topic:c"}]}},
        // The keys and a removal list.
        {"PK": {"S": owner_pk(OWNER)}, "SK": {"S": search_sk(CREATED, RUN)}, "runTagsOff": {"L": [{"S": "tech:b"}]}},
        // A run with no tag: still a run of the window.
        {},
        // Wrong types: skipped and counted.
        {"runTags": {"N": "5"}},
        {"runTagsUser": {"M": {}}},
        {"runTags": {"L": [{"N": "1"}]}}
    ]);
    let r = replay(vec![ok(
        json!({"Items": items, "Count": 6, "ScannedCount": 6}),
    )]);
    let read = ddb(&r).tag_vocabulary(&run(), 200).await.unwrap();
    assert_eq!(
        read.items,
        vec![
            VocabItem {
                auto: strings(&["topic:a", "tech:b"]),
                user: strings(&["topic:c"]),
                off: vec![]
            },
            VocabItem {
                auto: vec![],
                user: vec![],
                off: strings(&["tech:b"])
            },
            VocabItem::default(),
        ]
    );
    assert_eq!(read.skipped, 3);
}

#[tokio::test]
async fn the_tag_marks_are_one_consistent_read_of_two_attributes() {
    let r = replay(vec![
        ok(
            json!({"Item": {"runTagsAt": {"S": "2026-10-13T18:00:05Z"}, "runTagsOff": {"L": [{"S": "tech:csv"}, {"S": "topic:old"}]}}}),
        ),
        ok(json!({})),
        ok(json!({"Item": {}})),
        ok(json!({"Item": {"runTagsAt": {"N": "5"}, "runTagsOff": {"S": "tech:csv"}}})),
        ok(json!({"Item": {"runTagsOff": {"L": [{"S": "a-b"}, {"N": "3"}]}}})),
        ok(json!({"Item": {"runTagsOff": {"N": "5"}}})),
        (500, DDB_500.into()),
    ]);
    let s = ddb(&r);
    assert_eq!(
        s.tag_state(&run()).await.unwrap(),
        Some(TagMarks {
            at: Some("2026-10-13T18:00:05Z".into()),
            off: strings(&["tech:csv", "topic:old"]),
        })
    );
    assert_eq!(s.tag_state(&run()).await.unwrap(), None, "no item");
    assert_eq!(
        s.tag_state(&run()).await.unwrap(),
        Some(TagMarks::default()),
        "an item with neither attribute"
    );
    assert_eq!(
        s.tag_state(&run()).await.unwrap(),
        Some(TagMarks {
            at: None,
            off: strings(&["tech:csv"])
        }),
        "a marker of another type is no marker; a lone text is a list of one, because the person's removal wins and must not be forgotten"
    );
    assert_eq!(
        s.tag_state(&run()).await.unwrap().unwrap().off,
        strings(&["a-b"]),
        "the entries that can be read are kept"
    );
    assert!(s.tag_state(&run()).await.unwrap().unwrap().off.is_empty());
    assert_eq!(
        s.tag_state(&run()).await.unwrap_err().error_type(),
        "RunSessionUnavailable"
    );
    let first = &bodies(&r)[0];
    assert_eq!(first["TableName"], "hamh-direct-harness");
    assert_eq!(first["Key"], key());
    assert_eq!(
        first["ConsistentRead"], true,
        "a retry must see the marker the first invocation wrote"
    );
    assert_eq!(first["ProjectionExpression"], "#runTagsAt, #runTagsOff");
    assert_eq!(
        first["ExpressionAttributeNames"],
        json!({"#runTagsAt": "runTagsAt", "#runTagsOff": "runTagsOff"})
    );
}

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

fn parent_run_reply(owner: &str) -> Value {
    let plain = json!({
        "runId": PARENT, "ownerPrincipalArn": owner, "promptSha256": "ab".repeat(32),
        "target": {"mode": "s3-only"}, "status": "complete", "phase": "green",
        "enginePolicyVersion": 0, "createdAt": PARENT_CREATED, "version": 4,
        "PK": format!("RUN#{PARENT}"), "SK": "META"
    });
    json!({"Item": plain.as_object().unwrap().iter().map(|(k, v)| (k.clone(), av(v))).collect::<Map<_, _>>()})
}

#[tokio::test]
async fn the_parents_tags_are_read_from_the_childs_partition_after_the_parents_run() {
    let r = replay(vec![
        ok(parent_run_reply(OWNER)),
        ok(json!({"Item": {
            "runTags": {"L": [{"S": "topic:backfill"}, {"S": "tech:dynamodb"}]},
            "runTagsUser": {"L": [{"S": "project:billing-export"}]},
            "runTagsOff": {"L": [{"S": "tech:csv"}]}
        }})),
    ]);
    let state = ddb(&r).parent_tags(&run()).await.unwrap();
    assert_eq!(
        state,
        Some(TagState {
            auto: strings(&["topic:backfill", "tech:dynamodb"]),
            user: strings(&["project:billing-export"]),
            off: strings(&["tech:csv"]),
        })
    );
    let all = bodies(&r);
    assert_eq!(all.len(), 2);
    assert_eq!(
        all[0]["Key"],
        json!({"PK": {"S": format!("RUN#{PARENT}")}, "SK": {"S": "META"}}),
        "the parent's Run item, for its createdAt"
    );
    assert_eq!(all[0]["ConsistentRead"], true);
    assert_eq!(
        all[1]["Key"],
        json!({"PK": {"S": owner_pk(OWNER)}, "SK": {"S": search_sk(PARENT_CREATED, PARENT)}}),
        "the parent's search item, in the child's own owner partition"
    );
    assert_eq!(all[1]["ConsistentRead"], true);
    assert_eq!(
        all[1]["ProjectionExpression"],
        "#runTags, #runTagsUser, #runTagsOff"
    );
}

#[tokio::test]
async fn a_parent_of_another_owner_or_a_missing_one_has_no_tags_to_give() {
    // Another principal's run: its search item is never read.
    let r = replay(vec![ok(parent_run_reply(
        "arn:aws:sts::111122223333:assumed-role/Admin/someone-else",
    ))]);
    assert_eq!(ddb(&r).parent_tags(&run()).await.unwrap(), None);
    assert_eq!(bodies(&r).len(), 1, "the second read is not made");

    // No such run.
    let r = replay(vec![ok(json!({}))]);
    assert_eq!(ddb(&r).parent_tags(&run()).await.unwrap(), None);
    assert_eq!(bodies(&r).len(), 1);

    // The run exists and its search item does not.
    let r = replay(vec![ok(parent_run_reply(OWNER)), ok(json!({}))]);
    assert_eq!(ddb(&r).parent_tags(&run()).await.unwrap(), None);

    // A run that continues nothing makes no call.
    let r = replay(vec![]);
    assert_eq!(ddb(&r).parent_tags(&plain_run()).await.unwrap(), None);
    assert!(bodies(&r).is_empty());

    // A fault of either read is an error.
    let r = replay(vec![(500, DDB_500.into())]);
    assert_eq!(
        ddb(&r).parent_tags(&run()).await.unwrap_err().error_type(),
        "RunSessionUnavailable"
    );
    let r = replay(vec![ok(parent_run_reply(OWNER)), (500, DDB_500.into())]);
    assert_eq!(
        ddb(&r).parent_tags(&run()).await.unwrap_err().error_type(),
        "RunSessionUnavailable"
    );
}
